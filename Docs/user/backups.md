# Backups

Everything Giles knows is in its data root: your configuration, the agents' memory and skills, the
conversation database, and attachments. `giles backup` writes a consistent copy of all of it while
the daemon keeps running.

Don't copy `data/giles.db` yourself, or an agent's `memory.db`. These databases are written to while
Giles runs (in WAL mode by default, see [`journal_mode`](configuration.md#journal_mode)), so a plain
file copy can miss recent writes or capture a half-written state. `giles backup` asks the running
daemon for a snapshot it can trust.

## Making a Backup

```sh
mkdir -p /mnt/backups/giles
giles backup /mnt/backups/giles
```

The directory must exist. The command prints the two files it wrote, one per line:

```text
/mnt/backups/giles/giles-20261023T140312Z.db
/mnt/backups/giles/giles-20261023T140312Z.tar
```

The name carries the time in UTC, so backups sort in the order you made them, and Giles never
overwrites an existing file. Both files are readable only by you (mode 0600), because they hold your
conversations. Like the other [command line](cli.md) commands, `giles backup` needs the daemon to be
running.

- `giles-<time>.db` is the database, with every conversation, the scheduled jobs, the outbox, and
  usage.
- `giles-<time>.tar` is a tar of what the database doesn't hold:
  - `giles.toml`;
  - everything under `agents/`, `users/`, and `skills/`: the `agent.toml` and `user.toml` files,
    `SOUL.md`, `USER.md`, skills, and whatever else you keep in those directories;
  - each agent's memory database, `agents/<agent>/memory.db`, as a consistent snapshot taken the
    same way as `giles-<time>.db` (what the agent remembers, including the memories it keeps for
    group conversations). Only the snapshot is in the tar, never the live file or its `-wal` and
    `-shm` files. If a memory database can't be snapshotted, `giles backup` fails and writes
    nothing;
  - everything under `data/blobs/`: attachments and tool results.

The archive leaves out:

- `secrets.toml`. A backup is meant to be copied elsewhere, and your credentials stay where you keep
  them. Keep your own copy of that file.
- `plugins/`, the plugin binaries. Install them again.
- `data/plugins/`, the state plugins keep for themselves, such as a WhatsApp session. A plugin that
  loses it asks you to link or log in again.
- `trash/`, where agents you remove in the web UI are kept
  ([Removing an agent](web.md#removing-an-agent)). It stays on that machine; copy it yourself if you
  want it kept elsewhere. A backup holds the agents you have, not the ones you removed.
- `backups/`, `run/`, and `cache/`. Giles recreates them.
- Symbolic links, which are never followed. If `giles.toml` or an agent's `memory.db` is a link (a
  dotfiles manager does that), the link and its target are both missing from the backup and
  `giles backup` logs a warning for each; back those files up yourself.

Backups are not encrypted or compressed. Put them somewhere that is as private as the data root, or
encrypt them yourself.

## Restoring

Restore into the data root of a stopped Giles, with the same or a newer version of the binary. The
first command is only needed on a new machine; it creates the data root and its `data` directory
readable only by you, as Giles does:

```sh
# 1. Stop `giles serve`.
# 2. Make sure the directories exist, private to you (nothing changes if they do).
mkdir -p -m 700 ~/.giles ~/.giles/data
cd ~/.giles
# 3. Put the database back. The -wal, -shm and -journal files belong to the old database; remove
#    them.
rm -f data/giles.db data/giles.db-wal data/giles.db-shm data/giles.db-journal
cp /mnt/backups/giles/giles-20261023T140312Z.db data/giles.db
# 4. The same goes for the agents' memory databases: remove the leftovers of the old ones, so the
#    snapshots in the tar don't end up next to a stale WAL.
rm -f agents/*/memory.db-wal agents/*/memory.db-shm agents/*/memory.db-journal
# 5. Unpack the files over the data root. This includes every agent's memory database, which goes
#    back to `agents/<agent>/memory.db`.
tar -xf /mnt/backups/giles/giles-20261023T140312Z.tar
# 6. Start Giles again.
giles serve
```

On a new machine, add `secrets.toml` (mode 0600) and the plugin binaries before step 6. If the data
root isn't `~/.giles`, use its path in steps 2 to 4, or set `GILES_HOME` for `giles serve`.

The snapshot doesn't depend on the journal mode it was taken in. Giles switches the restored
databases (`giles.db` and each `memory.db`) to the `journal_mode` in `giles.toml` when it starts.

Unpacking replaces the files of the same name and leaves the others where they are. To get back
exactly what the backup held, restore into an empty data root.

## Backups Before an Upgrade

A new version of Giles can change the database's schema. Before it changes an existing database,
`giles serve` writes the same two files to `backups/` in the data root:

```text
~/.giles/backups/giles-20261023T140312Z-pre-migration-v1.db
~/.giles/backups/giles-20261023T140312Z-pre-migration-v1.tar
```

`v1` is the schema version the database had, which is the one the snapshot holds. It is written only
when there is something to migrate: not on first start, and not when the database is already
current. If the backup can't be written (a full disk, say), Giles doesn't start and the database is
untouched. The tar holds the agents' memory databases too, as they are when Giles starts.

Giles keeps the five newest pre-migration backups and deletes older ones. Backups you make with
`giles backup` are never deleted, even in `backups/`.

To go back after a bad upgrade, stop Giles, install the older version, and [restore](#restoring) the
pre-migration backup. The older version can't open the migrated database, and it refuses to start
with a message saying so; that is why the snapshot is taken first.
