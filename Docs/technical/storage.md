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

`Store::vacuum_into(dest)` writes a consistent snapshot while the store keeps serving: `VACUUM INTO`
on a pooled read connection, outside a transaction (`VACUUM` can't run inside one). It sees one
consistent state and blocks neither the writer nor other readers. The destination must not exist or
be an empty file (so a caller can create it with the mode it wants first), and the copy is a
standalone database with no `-wal` file. A plain file copy of a WAL database can miss the WAL or
capture a torn state, which is why this is the only supported way to copy a live database
([Backups](backups.md)).

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
   copy). The core uses it to write the pre-upgrade snapshot to `backups/` (PLAN 11.4;
   [Backups](backups.md#before-migrations)). An
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
| `list_sessions`       | Sessions with their message counts (`SessionSummary`), newest activity (last message, else start) first; optionally one agent's, optionally with ended ones (admin `ListSessions`). |
| `latest_direct_session` | The direct session of a user with an agent on one of the given channel instances with the newest activity, open or ended: where a job finds the user's DM (PLAN 6.11). |
| `delete_session`      | Deletes a session with its messages and summaries; inbox and usage rows keep theirs with the session cleared (an isolated job run that replied `NO_REPLY`). |

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
| `last_seq`             | The seq of the last message, 0 if none.                              |
| `truncate_messages`    | Deletes the messages after a seq and the summaries covering any of them (a `dm` job run that replied `NO_REPLY`). |
| `insert_summary`       | Stores a compaction summary of `from_seq..=through_seq`; rejects empty ranges, ranges past the last message, and a cut between a `ToolUse` and its slots. |

The store doesn't decide when to compact or what a synthetic result says; the agent loop does
(tasks 2.3 to 2.10).

## Conversations

A conversation is every keyed session with one channel instance and conversation id, whatever the
agent, ended sessions included. The web channel reads its chat list and chat history this way.

| Method                  | Contract                                                            |
| ----------------------- | ------------------------------------------------------------------- |
| `channel_conversations` | A user's conversations on one instance, most recently active first (last message, else last session start), with when the first session started and the text of the first user message if it starts with a text block (filled slots skipped). |
| `conversation_transcript` | In one read transaction: the last N transcript rows of a conversation, oldest first, each with its session (sessions in id order, then by `seq`; summarized messages included, as a transcript to show, not a prompt; empty slots as `Entry::EmptySlot`), and the `waiting` inbox rows routed to its sessions that are `queued` or `started`, oldest first. A started row's message may be in the transcript already (its turn stores it). |

Neither has an index of its own: both scan `sessions` (small: one row per conversation and reset).

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
backoff schedule belongs to the sender ([Outbox](outbox.md)); the store records attempts, the last
error, and the next attempt time. Only one sender loop may run, since rows aren't leased.

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
`usage_by_agent(since)` sums requests, fallbacks, and tokens per agent; `usage_by_model(since,
until)` per agent, provider instance, and model (`since` inclusive, `until` exclusive; the admin
`UsageReport`). The prompt cache hit rate, `giles_types::Usage::cache_hit_rate`, is cache-read
tokens over all input tokens (uncached input + cache reads + cache writes), since `input_tokens`
excludes cached tokens. Token counts above `i64::MAX` are rejected, not truncated.

## Instance Names

Instance names (PLAN 7.2) are stored in `sessions.channel`, `inbox.instance`, `outbox.instance`,
`usage.provider`, and in the `provider` field of an assistant message's `meta` JSON. Nothing else
holds one: `inbox.payload` and `outbox.payload` carry conversations and text but no instance name,
`jobs.spec` carries a prompt and a schedule, and identities use the namespace the channel
reports, not the instance name.

`instance_rows` returns, per name, how many rows use it in each of those four columns. It's what
the startup check for an unused old instance name reads (`Docs/technical/instance-rename.md`).

`rename_instance(old, new, hook)` moves all five places to the new name in one `IMMEDIATE`
transaction, runs `hook` inside it after the updates and before the commit, and returns the number
of rows moved. It refuses with `Conflict` if any row already uses `new`, so two instances' data are
never merged. A failing hook rolls everything back and comes back as `Error::Hook`; the caller does
its filesystem work in the hook, so that work and the rows succeed or fail together. The caller
still has to undo that work if the commit itself fails. `messages.meta` is updated without a
trigger on the column, so the search index is untouched.

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
  after the file is in place. Metadata is a table, not sidecar files, so the sweep can
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

### Sweep

`BlobStore::sweep(now, grace)` removes what nothing uses (PLAN 11.5); the daemon runs it
periodically (see [Daemon](daemon.md)) with `SWEEP_GRACE`, 48 hours, and returns a `SweepReport`
(`blobs`, `orphan_files`, `bytes`). It's in `blobs/sweep.rs`.

- **What's garbage.** A `blobs` row whose `stored_at` is older than the grace period and whose id
  no reference mentions, and a file without a row (a crash, or a put cancelled between its rename
  and its row) whose mtime is older than the grace period and that no reference mentions. A row
  whose file is missing is removed the same way. Every put renames a fresh file into place, so a
  file's mtime is the time of its latest put, and `stored_at` moves forward on every put: content
  that was just sent again is young.
- **How references are found.** The sweep scans the text of every column that can hold a
  `MediaRef`, listed in `REFERENCING_COLUMNS`: `messages.content` and `messages.meta` (blocks,
  including tool-result content and `{"$media": id}` tool inputs), `summaries.content`,
  `inbox.payload`, `outbox.payload`, and `jobs.spec`. It doesn't parse their JSON: it looks for
  every 64-character run of lowercase hex (the shape of a `MediaId`) in the text, including windows
  inside longer runs. Any shape that carries an id, now or in a future version of the types, is
  found; the price is that a text that merely contains a blob's id also keeps it, which is the
  safe direction. A new table or column that stores a `MediaRef` must be added to the list.
- **Failing safe.** The decision and the row deletions happen in one write transaction on the
  writer connection, so no message or row referencing a blob can appear between the scan and the
  delete. Any error (listing the directories before the transaction, under the sweep lock, or a
  scan query inside it) means nothing is removed: the transaction aborts before any file is
  touched, and the sweep returns the error.
  Files are removed after the commit; a crash in between leaves rowless files, which the next sweep
  removes, and a file that can't be removed is logged and retried next time. A rowless file that is
  referenced is kept.
- **Lock.** It holds `BlobStore::lock_for_sweep()` (an exclusive `tokio::sync::RwLock` guard)
  throughout. Each put holds the shared side from just before its rename until its row is written.
  Without it, a sweep could delete a file whose row a put is about to write, or delete an old row
  and file just as a put of the same content commits, leaving a row without a file. Puts keep
  streaming into temp files while the sweep runs; only their commit waits. A backup takes the
  shared side too (`BlobStore::hold_off_sweep()`) from before it snapshots the database until its
  archive is written, so the sweep can't delete a blob the snapshot still refers to
  ([Backups](backups.md#the-archive)); that doesn't hold puts up.
- **What it leaves alone.** `sha256/tmp/` (puts in progress; a failed or cancelled put deletes its
  temp file, and opening the store clears a crash's leftovers) and anything under a shard
  directory that isn't a regular file named by a valid id in its own shard.
- **Cost.** The scan reads every message once. A still-referenced attachment older than 48 h
  stays a candidate on every sweep, so any install with older attachments does the full
  reference scan, holding the writer, every six hours; other writes wait while it runs. For the
  text volumes of a personal assistant that is a fraction of a second every few hours.

Blobs that only a plugin holds (its own copies, `GILES_PLUGIN_DATA`) aren't the core's concern.
