# Backups

`giles_core::backup` writes the snapshots of PLAN section 11.4, for two callers: the admin
service's `Backup` RPC (`giles backup <dir>`) and the core's own startup, before schema migrations.
The database half is `giles_store::Store::vacuum_into` ([Storage](storage.md)).

## Anatomy of a Snapshot

A snapshot is two files that share a stem, `Snapshot { database, archive }`:

- `<stem>.db`: `VACUUM INTO` of the live database. It's taken on a read connection, so it's one
  consistent state, doesn't block the writer, and is a standalone file with no WAL.
- `<stem>.tar`: a tar with paths relative to the data root, so it unpacks over one.

Copying `giles.db` is not an option while the core runs: in WAL mode the main file alone is behind
the WAL, and the copy can race a checkpoint.

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
| `agents/`, `users/`, `skills/` (whole trees) | Config and Markdown people and memory tools write. Taken whole, not filtered by extension, since a skill can carry scripts and the trees hold nothing else. |
| `data/blobs/` (whole tree)                 | Attachments; the database holds their metadata.              |

Left out: `secrets.toml` (a backup is meant to leave the machine, and credentials are the
operator's to keep), `plugins/` (binaries), `data/plugins/` (plugin-owned state, the core doesn't
read it), `data/giles.db*` (the `.db` file), `backups/`, `run/`, `cache/`, and
`data/blobs/sha256/tmp/` (the blob store's puts in progress, which it deletes when it opens).
A file that vanishes while the walk runs is skipped. Directories are recorded (with their modes) so
a restore into an empty root gets the same private layout.

Every entry, including `giles.toml` and the four tree roots, goes through `Walk::visit`, which
uses `symlink_metadata`:

- **Links** (a symlinked `giles.toml`, memory file, or whole tree) are never followed and never
  written as link entries, which a restore would leave dangling. Each one skipped is logged as a
  `warn` with its path, because linked memory files are a supported setup and the gap shouldn't be
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

The database is written before the archive. The blob sweep (task 4.2) could otherwise delete an old
blob that only an inbox or outbox row refers to between the two, leaving a row in the snapshot
without its file in the archive. So `create` holds `BlobStore::hold_off_sweep` from before the
`VACUUM INTO` until the archive is written. That is the shared side of the lock the sweep takes
exclusively (puts take the shared side too), so the sweep waits for the backup and a backup waits
for a running sweep, while puts carry on, except behind a sweep that is already queued. With it,
every blob the snapshot's rows name is in the archive; one stored in between is extra and
harmless. A test starts a sweep between the two steps and checks that it waits and that the
archive has the blob.

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
  when the backup fails, and pruning to the newest five without touching other files.

`admin/tests.rs` covers the RPC (the token is required like every RPC, and the argument errors),
and `crates/giles/tests/serve.rs` runs `giles backup` against a real daemon.
