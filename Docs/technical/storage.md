# Storage

The crate `giles-store` owns the SQLite database, `data/giles.db` (PLAN section 11.4). The core
opens it once with `Store::open(data_root.database_file())` and calls typed methods on the cloneable
`Store` handle; no other crate writes SQL.

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

## Schema (version 1)

`crates/giles-store/src/migrations/0001_initial.sql` is the authority; this is its outline.

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
