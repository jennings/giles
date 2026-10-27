# Backups

`giles_core::backup` writes the snapshots of PLAN section 11.4, for two callers: the admin
service's `Backup` RPC (`giles backup <dir>`) and the core's own startup, before schema migrations.
The database halves are `giles_store::Store::vacuum_into` ([Storage](storage.md)), for `giles.db`
and, through `MemoryStore`, for each agent's `memory.db` (see [Memory
Databases](#memory-databases)).

## Anatomy of a Snapshot

A snapshot is two files that share a stem, `Snapshot { database, archive }`:

- `<stem>.db`: `VACUUM INTO` of the live database, one consistent state and a standalone file with
  no WAL or journal. In WAL mode (the default) it's taken on a read connection and doesn't block
  the writer. In the rollback-journal modes (`[core] journal_mode`) a long read would make a
  writer's commit time out, so `Store::vacuum_into` takes it on the writer connection instead:
  writes queue for the length of the copy, reads go on ([Storage](storage.md#journal-modes)).
- `<stem>.tar`: a tar with paths relative to the data root, so it unpacks over one. It holds each
  agent's `memory.db` as a `VACUUM INTO` snapshot, at `agents/<agent>/memory.db`.

Copying `giles.db` (or a `memory.db`) is not an option while the core runs: in WAL mode the main
file alone is behind the WAL, and the copy can race a checkpoint; in the other modes it can catch a
commit half done. The snapshot doesn't carry the journal mode (only WAL is recorded in a file, and
`VACUUM INTO` writes a plain rollback-mode file), so a restore opens in any mode and the next start
switches it.

Both files are created first, empty, with `create_new` and mode 0600 (`Snapshot::reserve`). That
reserves the names (a snapshot never overwrites a file, and two backups in one second fail with
`Exists`), and gives the files their mode: SQLite accepts an empty existing file as the destination
of `VACUUM INTO`, and writes into it without changing its permissions. A failed snapshot deletes
both files (`Snapshot::discard`), so a partial one is never left to be mistaken for a good one.
Stems are `giles-<UTC time as YYYYMMDDTHHMMSSZ>`, so name order is time order.

### The Archive

`write_archive` walks a fixed list, in name order, so the archive's content depends only on the
files:

| Included                                   | Why                                                          |
| ------------------------------------------ | ------------------------------------------------------------ |
| `giles.toml`                               | Startup settings.                                            |
| `agents/`, `users/`, `skills/` (whole trees) | Config and Markdown people write. Taken whole, not filtered by extension, since a skill can carry scripts and the trees hold nothing else. The one exception is each agent's `memory.db` and the files SQLite keeps beside it: see below. |
| `agents/<agent>/memory.db` (a snapshot)    | The agent's memories, written with `VACUUM INTO` and archived in place of the live file. |
| `data/blobs/` (whole tree)                 | Attachments; the database holds their metadata.              |

Left out: `secrets.toml` (a backup is meant to leave the machine, and credentials are the operator's
to keep), `plugins/` (binaries), `data/plugins/` (plugin-owned state, the core doesn't read it),
`data/giles.db*` (the `.db` file), `backups/`, `trash/` (agents removed in the web UI:
the list of what is archived is explicit, so nothing outside it, the trash included, is picked up),
`run/`, `cache/` (rebuilt; the memory snapshots are staged there, see below), and
`data/blobs/sha256/tmp/` (the blob store's puts in progress, which it deletes
when it opens). A file that vanishes while the walk runs is skipped. Directories are recorded (with
their modes) so a restore into an empty root gets the same private layout.

Every entry, including `giles.toml` and the four tree roots, goes through `Walk::visit`, which
uses `symlink_metadata`:

- **Links** (a symlinked `giles.toml`, `USER.md`, or whole tree) are never followed and never
  written as link entries, which a restore would leave dangling. Each one skipped is logged as a
  `warn` with its path, because linked Markdown files are a supported setup and the gap shouldn't be
  silent. Other file types (sockets, FIFOs) are skipped quietly.
- **Regular files** are opened, and the size and identity come from the open file (`fstat`), not
  the path. The tar header records the size up front, so `ExactReader` copies exactly that many
  bytes: a file that grew meanwhile is cut at the recorded size, and one that shrank fails the
  backup ("got shorter while it was being copied; run the backup again"), since a short entry
  would shift every entry after it.

The snapshot's own two files are skipped by **device and inode**, recorded from the reserved files
before the walk, not by path. The backup directory can be spelled differently from the data root's
paths (a symlinked `~/.giles`, `giles backup .` from a `getcwd` that resolved links, a `..`), and a
path comparison then misses the archive being written, which would read itself and grow until the
disk was full (a test backs up into `link/agents` where `link` points at the data root). So a
backup directory inside an archived tree is fine. Earlier backups there are ordinary files and are
archived.

The database is written before the archive. The blob sweep could otherwise delete an old
blob that only an inbox or outbox row refers to between the two, leaving a row in the snapshot
without its file in the archive. So `create` holds `BlobStore::hold_off_sweep` from before the
`VACUUM INTO` until the archive is written. That is the shared side of the lock the sweep takes
exclusively (puts take the shared side too), so the sweep waits for the backup and a backup waits
for a running sweep, while puts carry on, except behind a sweep that is already queued. With it,
every blob the snapshot's rows name is in the archive; one stored in between is extra and
harmless. A test starts a sweep between the two steps and checks that it waits and that the
archive has the blob.

### Memory Databases

Each agent's memories live in `agents/<agent>/memory.db`, a SQLite file with its own schema
(`giles_store::MemoryStore`). It is under `agents/`, so the walk would otherwise archive it as a
plain file, and for a live WAL database that is the wrong thing to copy. So the walk never copies
`memory.db`, `memory.db-wal`, `memory.db-shm` or `memory.db-journal` in an agent's directory
(`Walk::is_memory_file`), and archives a snapshot in the live file's place.

`MemorySnapshots::take` makes the snapshots before the archive walk:

1. It lists the directories of `agents/` (not links) and keeps each one whose `memory.db` is a
   regular file. A directory name that isn't an agent name is logged and skipped; links are left
   to the walk, which logs each. `trash/` is not under `agents/`, so a removed agent's database
   is never listed. An agent without a `memory.db` is just skipped.
2. It makes a private staging directory (mode 0700) under `cache/`, which no walk reaches, so a
   staged file is never archived as itself wherever the backup directory is. It's where each
   snapshot goes, as an empty 0600 file first (the same trick as the snapshot's own files, so the
   mode is private).
3. For each database, in name order, it opens `MemoryStore::open(path, OpenOptions::new()
   .keep_journal_mode().readers(1))` and calls `vacuum_into` into the staged file. The journal
   mode is whatever the file is in, so the daemon's choice (`[core] journal_mode`) is never
   undone by a backup. In WAL mode `VACUUM INTO` runs on a read connection and doesn't block the
   writer, so a daemon with the database open keeps writing; in a rollback mode the writes queue
   for the copy, as for `giles.db`. Another handle on the file (the daemon's) is fine, and closing
   this one doesn't delete the WAL while the other is open.

When the walk reaches the live `memory.db`, `Walk::file` appends the staged snapshot under the live
name (`agents/<agent>/memory.db`, mode 0600), so the tar stays in name order and a restore puts every
database back where it was. The staging directory is removed when `MemorySnapshots` is dropped,
after the archive is written or when anything fails.

Any failure to snapshot a memory database (it isn't a database, its schema is newer than the binary,
the disk is full) fails the backup with `BackupError::MemoryDatabase` naming the file, and the
`.db` and `.tar` are discarded like any partial snapshot. A memory database an agent got after
`take` listed the directories is not in the backup (the walk leaves any `memory.db` without a
snapshot out rather than copy it raw), and a symbolic link is skipped with the walk's warning like
any other link, so a linked `memory.db` has to be backed up by hand.

`MemoryStore::open` migrates the file to the binary's memory schema, so a database that is behind
is migrated by being snapshotted.

## The `Backup` RPC

`BackupRequest.dir` must be an absolute path to an existing directory (`giles backup` makes a
relative argument absolute against its own working directory, since the core resolves the path in
its own). The service answers:

| Status                | When                                                              |
| --------------------- | ----------------------------------------------------------------- |
| `INVALID_ARGUMENT`    | The path is empty, relative, or not a directory.                  |
| `NOT_FOUND`           | It doesn't exist.                                                 |
| `FAILED_PRECONDITION` | It can't be inspected (permissions).                              |
| `ALREADY_EXISTS`      | A snapshot with this second's name is already there.              |
| `INTERNAL`            | Anything else, with the cause in the message; it's logged too.    |

`create` runs the whole backup in its own Tokio task and the handler awaits its handle. When the
client hangs up (Ctrl-C on `giles backup`), hyper drops the handler's future, but not the task: the
backup finishes with the sweep hold kept for the whole walk, or fails and removes its files, the
same as if the client had waited. Without that, the blocking steps would outlive the future,
unguarded and uncleaned, and leave a complete `.db` next to an empty `.tar`. The client simply
doesn't see the paths; the files are there (and logged). A test drops the caller mid-backup and
checks the sweep stays held until the pair is complete.

`BackupResponse.paths` is the database file, then the archive. The name uses the service's clock,
and the work runs on the store's read pool and Tokio's blocking pool, so other RPCs and turns carry
on. Like every RPC it needs the admin token. The `giles.admin.v1` messages didn't change; `Backup`
was declared with the service.

## Before Migrations

`Runtime::start` opens the store with `backup::open_options(&root)`, which installs the store's
`before_migrate` hook ([Storage](storage.md#migrations)). The store calls it only for a database at
schema version 1 or later that is behind the binary, after opening and before the migration
transaction, so:

- a new database is not backed up (nothing to lose);
- an up-to-date database is not backed up (no migration is pending);
- a database newer than the binary is refused before the hook runs.

The hook writes a snapshot to `backups/` named `<stem>-pre-migration-v<from>` (the schema version it
holds), through `MigrationPlan::vacuum_into` on the connection the migration will use. A failure
returns an error, the store's open fails with `Error::BeforeMigrate`, and `giles serve` stops with
the cause: an upgrade that can't be undone doesn't run, and the database is untouched (a test
checks the schema version is unchanged).

The snapshot also holds the memory databases, taken the same way as for `giles backup`
(`MemorySnapshots::take`, staged under `cache/` and archived as `agents/<agent>/memory.db`). The
hook is synchronous and runs on a thread of Tokio's blocking pool, where `MemoryStore` (async)
is driven with `Handle::block_on`; the memory databases are separate files, so nothing waits on
`giles.db`'s migration connection. A memory database that can't be snapshotted fails the hook
like any other failure, so the upgrade doesn't start. Opening a memory database for the snapshot
migrates it to the binary's memory schema first (`MemoryStore::open` always does), so after a
release that changes the memory schema this hook's copy of a database that was behind is already
migrated; it is the `giles.db` snapshot that is the old schema.

After a successful snapshot, `prune_pre_migration` keeps the newest `KEEP_PRE_MIGRATION` (5)
pre-migration snapshots and deletes the files of older stems. Only names the module makes
(`giles-<16-character time>-pre-migration-v<digits>` with `.db` or `.tar`) are candidates, so
snapshots from `giles backup`, even in `backups/`, and anything else the operator put there are
never deleted. Failing to prune is logged, not an error. Five is a handful of upgrades back; the
snapshots hold whole databases, so an unbounded count would eat the disk.

## Tests

`backup/tests.rs` runs against real databases and data roots in temporary directories:

- the snapshot, copied into place, opens as a store with the same rows (including writes still in
  the WAL) and doesn't have later writes;
- the archive's exact entry list for a data root holding every kind of file, its contents and
  modes, that it unpacks over a fresh root, that links aren't followed, and that it doesn't contain
  itself;
- file modes, name collisions, and that a failed reservation leaves no file;
- links skipped with one warning each, a backup directory reached through a symlink to the data
  root, exact entry sizes, and the sweep held off during a backup;
- the pre-migration cases above, against a database rewound to schema version 1: none for a new or
  current database, one for an old one (with the old schema and the right archive), none migrated
  when the backup fails, and pruning to the newest five without touching other files;
- memory databases: two agents' databases (with user, shared, and group memories, open with their
  writes in the WAL) in a backup, with exactly one `memory.db` entry each and no `-wal`/`-shm`
  files or trash, restored by unpacking the archive over a fresh root with every memory intact
  (and not the one written after the backup); the same in each rollback journal mode; a database
  that isn't a SQLite file failing the backup with nothing left in the backup directory or
  `cache/`; the pre-migration archive carrying them; and a symlinked `memory.db` skipped with one
  warning.

`admin/tests.rs` covers the RPC (the token is required like every RPC, and the argument errors),
and `crates/giles/tests/serve.rs` runs `giles backup` against a real daemon.
