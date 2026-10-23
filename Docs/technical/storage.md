# Storage

The crate `giles-store` owns the SQLite database, `data/giles.db` (PLAN section 11.4), and the
blob store under `data/blobs/` (PLAN 11.5). The core opens the database once with
`Store::open(data_root.database_file())` and calls typed methods on the cloneable `Store` handle;
no other crate writes SQL.

## Connections and Concurrency

SQLite comes from `rusqlite` with the `bundled` feature, so every build has the same SQLite with
FTS5 and the JSON functions compiled in (a test checks both). Every connection sets
`foreign_keys = ON`, `synchronous = NORMAL`, and a 5 s `busy_timeout`. The database runs in WAL
mode; `NORMAL` is the documented safe pairing for WAL: a power loss can drop the last commits but
never corrupts the file.

SQLite allows one writer at a time, so the store has:

- **One writer connection** behind an async mutex. Every write method runs as one `BEGIN IMMEDIATE`
  transaction on it. Writers queue in Tokio for the mutex instead of colliding inside SQLite, so the
  core's own writes never see `SQLITE_BUSY`, and a read-modify-write (next sequence number, find or
  create a session) can't race another one.
- **A pool of read-only connections** (4 by default, `OpenOptions::readers`). In WAL mode readers
  block neither the writer nor each other, so reads run alongside writes. Each read method runs in
  one read transaction and sees a consistent snapshot. A read issued after a write has returned
  sees that write.

All SQL runs on Tokio's blocking pool (`spawn_blocking`). A call that has started there finishes
even if its future is dropped, so a cancelled caller never leaves half a transaction behind.
`busy_timeout` remains as a backstop for other processes opening the file. Tests run many tasks
writing and reading at once and assert that none fails.

When the last `Store` clone is dropped, the read connections close first and the writer last. The
last connection to close checkpoints the WAL into the database file and deletes it, and only a
read-write connection can do that, so a clean shutdown leaves no `-wal` file (a test checks this).

A dedicated writer was chosen over a pool of interchangeable connections because with SQLite a
second writer connection only adds `SQLITE_BUSY` retries, never throughput.

## Migrations

Migrations are SQL files in `crates/giles-store/src/migrations/`, embedded with `include_str!`. The
schema version is the length of the list and lives in SQLite's `user_version` (a fresh file is
version 0). Opening:

1. refuses a database whose version is newer than the binary's (`Error::SchemaTooNew`), so an
   older binary never writes to a schema it doesn't know;
2. if the database is at version 1 or later and behind, calls the `OpenOptions::before_migrate`
   hook with a `MigrationPlan` (from and to versions, and `vacuum_into(dest)` for a consistent
   copy). This is where task 4.1 writes the pre-upgrade snapshot to `backups/` (PLAN 11.4). An
   error from the hook aborts the open before anything changes. Fresh databases skip the hook;
3. applies every pending migration and the new `user_version` in one `IMMEDIATE` transaction, so a
   failing step leaves the database exactly as it was.

Reopening an up-to-date database runs nothing. A released migration file is never edited; schema
changes are new files.

## Conventions

- Times are Unix milliseconds in `INTEGER` columns. They sort and compare in SQL without parsing,
  and `giles_types::Timestamp` converts both ways; values come back truncated to the millisecond.
  Methods that depend on the current time take it as a `now` argument, so tests control the clock.
- Tables are `STRICT`. JSON columns are `TEXT` holding the `serde_json` form of the domain type.
- Row ids are opaque newtypes (`SessionId`, ...) over SQLite rowids.
- Agent, user, channel instance, and conversation ids are stored as their string forms.

## Schema (version 2)

The files in `crates/giles-store/src/migrations/` are the authority (`0001_initial.sql` has every
table but `blobs`, which `0002_blobs.sql` adds); this is their outline.

| Table          | Columns and rules                                                                    |
| -------------- | ------------------------------------------------------------------------------------ |
| `sessions`     | `agent`, `kind` (`direct`, `group`, `job`), `channel` and `conversation` (both null for `job`), `user_id`, `started_at`, `ended_at`. A partial unique index allows one open session per (agent, channel, conversation). |
| `messages`     | `session_id`, `seq` (unique per session), `role`, `content` (JSON `Vec<Block>`), `meta` (JSON `MessageMeta`), `tool_use_id`, `created_at`. `content` is null only for an empty tool-result slot. |
| `summaries`    | `session_id`, `from_seq`, `through_seq`, `content` (text), `created_at`.               |
| `search`       | FTS5 over the top-level `Text` blocks of `messages`, `rowid` = message id.            |
| `inbox`        | `instance`, `event_id` (unique together), `session_id`, `user_id`, `payload` (JSON), `state` (`queued`, `started`, `done`, `unknown`), `received_at`. |
| `outbox`       | `instance`, `recipient`, `kind`, `user_id`, `payload` (JSON), `attempts`, `last_error`, `created_at`, `next_attempt_at`, `expires_at`. |
| `jobs`         | Runtime jobs: `agent`, `creator`, `spec` (JSON), `created_at`; ids never reused.      |
| `job_runs`     | (`agent`, `job`) to `last_run_at`; `job` is `config:<id>` or `runtime:<jobs.id>`.      |
| `usage`        | `at`, `provider`, `model`, `agent`, `user_id`, `session_id`, four token counts, `fallback`. |
| `web_sessions` | `token_hash` (SHA-256 of the cookie token), `user_id`, `created_at`, `expires_at`.     |
| `blobs`        | `id` (hex SHA-256), `size`, `media_type`, `name`, `created_at`, `stored_at` (latest put). |

`identities` and `pairings` are deferred with multi-user support (PLAN 1.1, D49) and don't exist
yet.

Deleting a session cascades to its messages and summaries and clears `session_id` in `inbox` and
`usage`, which keeps `giles user forget` (PLAN 11.4) a matter of deleting rows.

## Sessions

A session is one conversation's history with one agent (PLAN 6.2), keyed by (agent, channel
instance, conversation id). Isolated job runs get keyless `job` sessions.

| Method                | Contract                                                              |
| --------------------- | --------------------------------------------------------------------- |
| `open_session`        | Returns the open session for a key, or creates it; the flag says which. Concurrent calls for one key create one session. |
| `create_job_session`  | A new keyless session for an isolated job run.                        |
| `find_open_session`   | The open session for a key, if any.                                   |
| `session`             | A session by id, open or ended.                                       |
| `end_session`         | Sets `ended_at` (`/new`, idle reset, end of a job run); `false` if already ended. The next `open_session` for the key starts a new session. |
| `open_sessions`       | Every open session, oldest first.                                     |

`Session::last_message_at` is the `created_at` of the session's last message, for `idle_reset`.

## Messages and Tool-Result Slots

Each message is one `messages` row: its `seq` (1, 2, ... per session), role, `content` as the JSON
array of `giles_types::Block`, `meta` as JSON `MessageMeta`, and `created_at = meta.at`. Message
ids in the API are `StoredMessageId`.

The transcript invariant (every `ToolUse` has a `ToolResult`, PLAN 6.3) is kept with **slots**: a
slot is a `User` message row reserved for one `ToolUse` result, with `tool_use_id` set and `content`
null until it's filled. `append_messages` stores an assistant message and one slot per `ToolUse`
block (in block order, numbered right after it) in the same transaction. Filling a slot writes a
`User` message with one `ToolResult` block, so a loaded transcript is exactly what a provider needs;
adapters merge consecutive `User` messages where an API requires alternating roles (PLAN 5).

| Method                 | Contract                                                             |
| ---------------------- | -------------------------------------------------------------------- |
| `append_messages`      | Appends messages in one transaction, numbering them after the last; returns each one's id, seq, and slots. `NotFound` for a missing session; ended sessions still accept messages. |
| `fill_slot`            | Fills an empty slot with `ToolResult { content, is_error }`. `NotFound` if the id isn't a slot, `Conflict` if it's filled. |
| `fill_empty_slots`     | Fills a session's empty slots with an error result (`cancelled`, `denied`, `iteration limit`, ...). |
| `fill_all_empty_slots` | Startup recovery: the same for every session (`interrupted`); returns the sessions affected, so the core can tell each requester. |
| `load_history`         | The newest summary (highest `through_seq`) and every message after it. Empty slots come back as `Entry::EmptySlot`. |
| `recent_messages`      | The last N messages in order (a job's `dm_excerpt`).                 |
| `insert_summary`       | Stores a compaction summary of `from_seq..=through_seq`; rejects empty ranges, ranges past the last message, and a cut between a `ToolUse` and its slots. |

The store doesn't decide when to compact or what a synthetic result says; the agent loop does
(tasks 2.3 to 2.10).

## Search

`search` is an FTS5 table over messages for `memory_search` (PLAN 6.7). It holds the top-level
`Text` blocks of each message, joined by newlines; tool inputs and results, reasoning, and media are
left out. Three triggers on `messages` maintain it on insert, on update of `content` (so a
tombstone from `giles user forget` replaces the old text), and on delete (including deletes that
cascade from a session). The extraction runs in SQL (`json_each` over `content`), so no code path
can forget to update the index. The tokenizer is `porter unicode61 remove_diacritics 2`: English
stemming, case and accent folding.

`Store::search(scope, query, limit)`:

- `SearchScope` always names the agent and can narrow by session kind, user, channel, and
  conversation. `SearchScope::direct(agent, user)` is a DM's scope (that user's direct sessions with
  the agent on every channel); `SearchScope::conversation` is a group's.
- The query is free text. Each word with a letter or digit becomes a quoted FTS5 string, and the
  words are OR-ed, so the model can't write a malformed query and BM25 ranks messages matching more
  and rarer words first.
- Each hit has the message's ids, seq, role, time, the session's channel and conversation, and a
  snippet with matches wrapped in `**`.

## Inbox

Every acked inbound event is an `inbox` row (PLAN 8.5): the channel instance, the event id, the
routed session and sender, the whole event as JSON, and a state. `inbox_insert` uses
`ON CONFLICT (instance, event_id) DO NOTHING`, so inserting and deduplicating are one statement in
one transaction, and the core acks only after it returns. Duplicates return `None` and change
nothing.

```text
insert(queued) -> queued -> started -> done
                            started -> unknown   (startup recovery)
insert(done)                                     (dedupe key only: status events, dropped messages)
```

| Method                       | Contract                                                       |
| ---------------------------- | -------------------------------------------------------------- |
| `inbox_insert`               | Insert-or-dedupe; `state` must be `Queued` (with a session) or `Done`. |
| `inbox_entry`                | One row by id.                                                 |
| `inbox_queued`               | A session's queued rows, oldest first: the next turn's messages. |
| `inbox_sessions_with_queued` | Sessions with queued rows, by oldest row: the turns to run after a restart. |
| `inbox_start`, `inbox_finish` | Queued to started, started to done; all rows in the batch or none (`Conflict`, `NotFound`). |
| `inbox_interrupt`            | Startup: every started row becomes `unknown` and is returned, so the requester can be told. |
| `inbox_prune(before)`        | Deletes `done` and `unknown` rows received strictly before `before` (now minus 30 days). Queued and started rows are never pruned. |

## Outbox

Proactive sends wait in `outbox` until the channel accepts them or they expire (PLAN 6.11). A row
is due while `next_attempt_at <= now < expires_at`, and expired once `expires_at <= now`. The
backoff schedule belongs to the sender (task 3.1); the store records attempts, the last error, and
the next attempt time. Only one sender loop may run, since rows aren't leased.

| Method               | Contract                                                               |
| -------------------- | ---------------------------------------------------------------------- |
| `outbox_enqueue`     | New row, due at `created_at`; `expires_at` must be later.              |
| `outbox_due`         | Due rows, earliest attempt first, up to a limit.                       |
| `outbox_next_wakeup` | The earliest `next_attempt_at` or `expires_at`, for the sender's sleep. |
| `outbox_retry`       | Records a failure: `attempts + 1`, `last_error`, next attempt time.    |
| `outbox_delete`      | Delivered, or its approval was resolved.                               |
| `outbox_expire`      | Deletes and returns expired rows, for logging instance, recipient, and kind. |
| `outbox_expire_kind` | Deletes and returns every row of a kind, due or not: at startup, the prompts of approvals that died with the old process. |

## Jobs

Config jobs live in `agent.toml`; runtime jobs (from the `schedule` tool) are `jobs` rows with the
agent, the creator, and the scheduler's JSON `spec`, which the store doesn't interpret. Their ids
use `AUTOINCREMENT`, so a deleted job's id is never reused. `job_runs` keeps the last run of every
job, keyed by agent and `JobKey`: `Config(id)` is stored as `config:<id>` and `Runtime(id)` as
`runtime:<row id>`, so config and runtime ids can't collide. A trigger deletes a runtime job's
`job_runs` row with the job.

| Method                               | Contract                                         |
| ------------------------------------ | ------------------------------------------------ |
| `job_create`                         | New runtime job.                                 |
| `job`, `jobs(agent?, creator?)`      | One job; jobs filtered by agent and creator, oldest first. |
| `job_update_spec`, `job_delete`      | `false` if the job doesn't exist.                |
| `job_last_run`, `job_set_last_run`   | Read and upsert the last run time.               |

## Usage

`record_usage` stores one row per provider request (PLAN 11.7): time, provider instance, model,
agent, user, session, the four token counts of `giles_types::Usage`, and the fallback flag.
`usage_by_agent(since)` sums requests, fallbacks, and tokens per agent. `AgentUsage::cache_hit_rate`
is cache-read tokens over all input tokens (uncached input + cache reads + cache writes), since
`input_tokens` excludes cached tokens. Token counts above `i64::MAX` are rejected, not truncated.

## Web Sessions

`web_sessions` backs the login cookie (PLAN 3.4). The store hashes the cookie token with SHA-256
and keeps only the hash, so a database copy can't be replayed as a login. The token is random with
at least 128 bits of entropy, so a plain hash suffices. `web_session_create` (`Conflict` on a token
collision), `web_session(token, now)` (only unexpired sessions), `web_session_delete` (logout), and
`web_sessions_expire(now)`. The 30-day lifetime is the caller's `expires_at`.

## Blob Store

`BlobStore` keeps attachments and tool media as content-addressed files (PLAN 11.5). The core opens
it once with `BlobStore::open(data_root.blobs_dir(), store)`; it takes the directory from the caller
and its metadata lives in the database.

```text
data/blobs/sha256/
  tmp/                    puts in progress; emptied when the store opens
  2c/
    2cf24dba...938b9824   one file per blob, named by its full hex SHA-256
```

- **Sharding.** Files sit in 256 directories named by the first two hex digits of the digest, so no
  directory grows past a few thousand entries for any realistic store, and listing for the sweep
  stays cheap, without the extra lookups of a deeper tree. Directories are created with mode 0700
  and files with 0600.
- **Put.** `put(reader, limit, meta, now)` streams an `AsyncRead` into a temp file in
  `sha256/tmp/` (same filesystem, so the final rename is atomic) while hashing it. Past `limit`
  bytes it stops with `Error::TooLarge`. Callers pass the smaller of the instance's `max_blob` and a
  transfer header's `size` (PLAN 8.3). The temp file is fsynced and renamed to its final name, and
  the shard directory is fsynced (and `sha256/` too when the shard directory is new), so a stored
  blob survives a power loss. The rename always replaces an existing copy of the same content: that
  costs a directory fsync per put, but a copy damaged in place (even one with the right size) is
  repaired by storing the content again, and readers with the old file open keep reading it. The
  temp file is deleted when a put fails or its future is dropped, and leftovers from a crash are
  deleted at open.
- **Metadata.** A `blobs` row (id, size, media type, name, `created_at`, `stored_at`) is upserted
  after the file is in place. Metadata is a table, not sidecar files, so the sweep (task 4.2) can
  find unreferenced, old blobs with SQL, and a row and its file can't disagree about anything but
  existence. Media type and name are those of the first put; the returned `MediaRef`
  (`id = MediaId::from_digest`) carries the caller's own, since it describes that attachment.
  `stored_at` moves forward on every put, so content that's being sent again isn't swept as old.
- **Get.** `get(id)` returns the stored `MediaRef` and a `BlobReader`, or `None` for an unknown id.
  A missing file or one of the wrong size is `Error::BlobDamaged`; storing the same content again
  repairs it. The reader hashes as it goes, and the read that reaches the end fails with
  `InvalidData` if the content doesn't match its id, so a consumer that reads to the end never
  accepts damaged bytes. `stat(id)` returns the metadata alone.

Concurrent puts of the same content need no lock among themselves: each writes its own temp file, a
rename over an existing file is atomic, and the row upsert is idempotent. Hashing runs on the async
task between reads, one 64 KiB chunk at a time, which takes well under a millisecond per chunk, so
a put never holds a runtime thread for long.

The sweep (task 4.2) must follow two rules:

- **Lock.** It holds `BlobStore::lock_for_sweep()` (an exclusive `tokio::sync::RwLock` guard)
  while it decides what to delete and deletes it. Each put holds the shared side from just before
  its rename until its row is written. Without it, a sweep could delete a file whose row a put is
  about to write, or delete an old row and file just as a put of the same content commits, leaving
  a row without a file. Puts keep streaming into temp files while the sweep runs; only their commit
  waits.
- **Grace period for rowless files.** A file without a row is left by a crash, or by a put
  cancelled between its rename and its row. It's garbage only once its mtime is older than 48 hours
  (the same grace as unreferenced blobs, PLAN 11.5). Every put renames a fresh file into place, so
  the mtime is the time of the latest put.
