# Backups

`giles backup` copies your Giles data root while the daemon keeps running.

**Don't copy `data/giles.db` or an agent's `memory.db` yourself.** Giles writes to them while it
runs, so a plain file copy can miss recent writes or catch a half-written state.

## Making a backup

The daemon must be running, and the directory must exist:

```sh
mkdir -p /mnt/backups/giles
giles backup /mnt/backups/giles
```

The command prints the two files it wrote:

```text
/mnt/backups/giles/giles-20261023T140312Z.db
/mnt/backups/giles/giles-20261023T140312Z.tar
```

The names carry the time in UTC, so backups sort in the order you made them.

Both files are readable only by you, but they aren't encrypted.
**Keep them somewhere as private as the data root**, or encrypt them yourself.

## What a backup holds

`giles-<time>.db` is a copy of the whole database, `data/giles.db`, including every conversation.

`giles-<time>.tar` holds the files:

- `giles.toml`
- everything under `agents/`, `users/`, and `skills/`, including each agent's memory database
- attachments and tool results, under `data/blobs/`

A backup leaves out:

- `secrets.toml`. Keep your own copy of it.
- The plugin binaries in `plugins/`. Install them again.
- The state plugins keep in `data/plugins/`, such as a WhatsApp session. A plugin that loses it asks
  you to link or log in again.
- [Agents you removed](web.md#removing-an-agent), which are kept in `trash/`. Copy that directory
  yourself if you want them kept elsewhere.
- Symbolic links. If `giles.toml` or an agent's `memory.db` is a link, as a dotfiles manager makes,
  neither the link nor its target is in the backup. `giles backup` logs a warning for each one. Back
  those files up yourself.

## Restoring

Restore into the data root of a stopped Giles, with the same or a newer version of the binary. The
`mkdir` is only needed on a new machine.

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

On a new machine, add `secrets.toml` (mode 0600) and the plugin binaries before step 6.

If the data root isn't `~/.giles`, use its path in steps 2 to 4, or set `GILES_HOME` for
`giles serve`.

Unpacking replaces files with the same name and leaves other files alone. To get back exactly what
the backup held, restore into an empty data root.

## Backups before an upgrade

A new version of Giles can change the database's schema. Before it changes an existing database,
`giles serve` writes a backup to `backups/` in the data root:

```text
~/.giles/backups/giles-20261023T140312Z-pre-migration-v1.db
~/.giles/backups/giles-20261023T140312Z-pre-migration-v1.tar
```

`v1` is the schema version the backup holds.

If Giles can't write this backup, for example because the disk is full, it doesn't start. The
database is left untouched.

Giles keeps the five newest pre-migration backups and deletes older ones. It never deletes backups
you make with `giles backup`, even in `backups/`.

To go back after a bad upgrade, stop Giles, install the older version, and [restore](#restoring) the
pre-migration backup. The older version refuses to start with the migrated database.
