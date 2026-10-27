//! Backups (PLAN section 11.4): a consistent snapshot of the data root.
//!
//! A snapshot is two files that share a name stem:
//!
//! - `<stem>.db`: the database, written with `VACUUM INTO`, which is consistent while the core
//!   runs, in any journal mode (copying the file isn't);
//! - `<stem>.tar`: a tar of the files people write and the blobs (see [`write_archive`]), with
//!   each agent's memory database (`agents/<agent>/memory.db`) as a `VACUUM INTO` snapshot taken
//!   the same way ([`MemorySnapshots`]).
//!
//! `giles backup <dir>` (the admin service's `Backup`) writes one to a directory the operator
//! names ([`create`]). The core also writes one to `backups/` before it applies schema migrations
//! to an existing database ([`open_options`]), so a bad upgrade can be undone by restoring it with
//! the old binary; only the newest [`KEEP_PRE_MIGRATION`] of those are kept. See
//! `Docs/technical/backups.md`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use giles_store::{BlobStore, MemoryStore, MigrationPlan, Store};
use giles_types::Timestamp;
use tempfile::TempDir;
use tracing::{info, warn};

use crate::paths::DataRoot;

#[cfg(test)]
mod tests;

/// How many pre-migration snapshots `backups/` keeps; older ones are deleted after a new one is
/// written. Snapshots made by `giles backup` are never deleted.
pub const KEEP_PRE_MIGRATION: usize = 5;

/// Mode of the files a backup creates: they hold every conversation.
const FILE_MODE: u32 = 0o600;

/// The directory the blob store keeps puts in progress in, relative to `data/blobs/`. Its files
/// are partial, and the store deletes them when it opens.
const BLOB_TMP: &str = "sha256/tmp";

/// The part of a pre-migration stem after the timestamp: `-pre-migration-v<schema version>`.
const PRE_MIGRATION_MARK: &str = "-pre-migration-v";

/// The name of an agent's memory database, in its directory.
const MEMORY_DB: &str = "memory.db";

/// The files SQLite keeps beside a memory database. The archive never carries them: a copy of a
/// live database's WAL or journal is not a consistent state, and the snapshot has neither.
const MEMORY_SIDECARS: [&str; 3] = ["memory.db-wal", "memory.db-shm", "memory.db-journal"];

/// Prefix of the private directory under `cache/` the memory snapshots are staged in.
const MEMORY_STAGE_PREFIX: &str = ".memory-backup-";

/// Why a backup failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackupError {
    /// A backup file already exists. Names carry the time to the second, so this only happens for
    /// two backups in one second.
    #[error("{} already exists; try again in a second", .0.display())]
    Exists(PathBuf),
    /// A file or directory couldn't be read or written.
    #[error("{}: {source}", .path.display())]
    Io {
        /// The file or directory.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: io::Error,
    },
    /// The database snapshot failed.
    #[error("cannot snapshot the database: {0}")]
    Database(#[from] giles_store::Error),
    /// An agent's memory database couldn't be opened or snapshotted.
    #[error("cannot snapshot the memory database {}: {source}", .path.display())]
    MemoryDatabase {
        /// The live `memory.db`.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: giles_store::Error,
    },
    /// A background task died.
    #[error("the backup task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> BackupError {
    let path = path.to_owned();
    move |source| BackupError::Io { path, source }
}

/// The two files of one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// `<stem>.db`, the database.
    pub database: PathBuf,
    /// `<stem>.tar`, the files.
    pub archive: PathBuf,
}

impl Snapshot {
    /// Creates both files, empty and private (mode 0600), in `dir` as `<stem>.db` and
    /// `<stem>.tar`. Creating them first reserves the names (an existing file is never
    /// overwritten) and sets their mode: SQLite writes into an empty destination file.
    ///
    /// # Errors
    ///
    /// [`BackupError::Exists`] if either exists; [`BackupError::Io`] otherwise.
    pub fn reserve(dir: &Path, stem: &str) -> Result<Self, BackupError> {
        let snapshot = Self {
            database: dir.join(format!("{stem}.db")),
            archive: dir.join(format!("{stem}.tar")),
        };
        create_private(&snapshot.database)?;
        if let Err(err) = create_private(&snapshot.archive) {
            let _ = fs::remove_file(&snapshot.database);
            return Err(err);
        }
        Ok(snapshot)
    }

    /// Deletes both files after a failed backup, so no partial snapshot is left to be mistaken
    /// for a good one.
    pub fn discard(&self) {
        for path in [&self.database, &self.archive] {
            if let Err(err) = fs::remove_file(path) {
                warn!(path = %path.display(), %err, "cannot remove a failed backup file");
            }
        }
    }

    /// Fills the archive with the files of `root` and the `memory` snapshots. See
    /// [`write_archive`].
    ///
    /// # Errors
    ///
    /// [`BackupError::Io`].
    pub fn write_archive(
        &self,
        root: &DataRoot,
        memory: &MemorySnapshots,
    ) -> Result<(), BackupError> {
        write_archive(
            root,
            &self.archive,
            &[&self.database, &self.archive],
            memory,
        )
    }
}

fn create_private(path: &Path) -> Result<(), BackupError> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            Err(BackupError::Exists(path.to_owned()))
        }
        Err(err) => Err(io_error(path)(err)),
    }
}

/// Snapshots of the agents' memory databases (`agents/<agent>/memory.db`), staged in a private
/// directory until [`write_archive`] puts them in the tar under the live databases' names.
///
/// Each database is opened by path, in the journal mode it is already in (so the daemon's choice
/// stands, and a running core or another process can have it open), and written with
/// `VACUUM INTO`, which is consistent while the database is written to. Copying the file isn't:
/// in WAL mode the main file is behind the WAL. The staged files are removed when this value is
/// dropped, whether or not the backup succeeded.
///
/// Only a regular `memory.db` in a real directory under `agents/` is snapshotted. Symbolic links
/// are skipped, and [`write_archive`] logs the warning for them.
#[derive(Debug, Default)]
pub struct MemorySnapshots {
    /// Keeps the staged files; removing it deletes them.
    _stage: Option<TempDir>,
    /// Each live database's path, and the staged snapshot to archive under that name.
    files: BTreeMap<PathBuf, PathBuf>,
}

impl MemorySnapshots {
    /// Snapshots every `agents/<agent>/memory.db` of `root`, in name order, one after the other.
    ///
    /// # Errors
    ///
    /// [`BackupError::MemoryDatabase`] for the first database that can't be opened or written,
    /// [`BackupError::Io`] for the staging directory or the agents listing. Nothing is left
    /// staged.
    pub async fn take(root: &DataRoot) -> Result<Self, BackupError> {
        let listing = root.clone();
        let (databases, stage) = tokio::task::spawn_blocking(move || {
            let databases = memory_databases(&listing)?;
            let stage = if databases.is_empty() {
                None
            } else {
                Some(stage_dir(&listing)?)
            };
            Ok::<_, BackupError>((databases, stage))
        })
        .await??;
        let Some(stage) = stage else {
            return Ok(Self::default());
        };
        let mut files = BTreeMap::new();
        for (i, live) in databases.into_iter().enumerate() {
            let staged = stage.path().join(format!("{i}.db"));
            create_private(&staged)?;
            let source = |source| BackupError::MemoryDatabase {
                path: live.clone(),
                source,
            };
            let options = giles_store::OpenOptions::new()
                .keep_journal_mode()
                .readers(1);
            let store = MemoryStore::open(&live, options).await.map_err(source)?;
            store.vacuum_into(staged.clone()).await.map_err(source)?;
            files.insert(live, staged);
        }
        Ok(Self {
            _stage: Some(stage),
            files,
        })
    }
}

/// The `memory.db` of each agent directory under `agents/`, in name order. Links and other
/// non-regular files are left for the archive walk to report; a directory whose name isn't valid
/// UTF-8 can't be an agent and is reported here, since the walk leaves its database out too.
fn memory_databases(root: &DataRoot) -> Result<Vec<PathBuf>, BackupError> {
    let agents = root.agents_dir();
    let entries = match fs::read_dir(&agents) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io_error(&agents)(err)),
    };
    let mut dirs = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()
        .map_err(io_error(&agents))?;
    dirs.sort();
    let mut databases = Vec::new();
    for dir in dirs {
        let is_dir = match fs::symlink_metadata(&dir) {
            Ok(meta) => meta.file_type().is_dir(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(io_error(&dir)(err)),
        };
        if !is_dir {
            continue;
        }
        let database = match dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| root.agent_memory_db(name))
        {
            Some(Ok(database)) => database,
            _ => {
                warn!(
                    path = %dir.display(),
                    "left an agent directory with an unusable name out of the memory backup",
                );
                continue;
            }
        };
        match fs::symlink_metadata(&database) {
            Ok(meta) if meta.file_type().is_file() => databases.push(database),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(&database)(err)),
        }
    }
    Ok(databases)
}

/// A private directory for the memory snapshots, under `cache/` where no archive walk goes (so
/// a staged file is never archived as itself, wherever the backup directory is).
fn stage_dir(root: &DataRoot) -> Result<TempDir, BackupError> {
    let cache = root.cache_dir();
    fs::create_dir_all(&cache).map_err(io_error(&cache))?;
    tempfile::Builder::new()
        .prefix(MEMORY_STAGE_PREFIX)
        .tempdir_in(&cache)
        .map_err(io_error(&cache))
}

/// The name stem of a snapshot made by `giles backup` at `now`: `giles-20261023T140312Z`. The
/// time is UTC, so names sort in the order they were made.
pub fn manual_stem(now: Timestamp) -> String {
    format!("giles-{}", stamp(now))
}

/// The name stem of a snapshot of a database at schema version `from`, made at `now` before
/// migrating it: `giles-20261023T140312Z-pre-migration-v1`.
pub fn pre_migration_stem(now: Timestamp, from: i64) -> String {
    format!("{}{PRE_MIGRATION_MARK}{from}", manual_stem(now))
}

fn stamp(now: Timestamp) -> String {
    now.strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// Writes a snapshot of the data root to `dir` (which must exist): the database from the running
/// `store`, then the archive. The database goes first, and the blob sweep is held off from before
/// it until the archive is written, so every blob its rows name is in the archive (puts still run;
/// a blob stored after the snapshot is harmless extra).
///
/// Neither the database nor the archive is left behind if either step fails.
///
/// The work runs in its own task, so dropping this future (a client that hung up, an RPC that was
/// cancelled) doesn't stop it: the backup still finishes, with the sweep held off until it has,
/// or fails and removes its files. The caller just stops waiting for the answer. Otherwise the
/// blocking steps would carry on without the sweep hold and without the cleanup, and leave a
/// complete database next to an empty archive.
///
/// # Errors
///
/// [`BackupError`].
pub async fn create(
    root: &DataRoot,
    store: &Store,
    blobs: &BlobStore,
    dir: &Path,
    now: Timestamp,
) -> Result<Snapshot, BackupError> {
    create_detached(root, store, blobs, dir, now, async {}).await
}

/// [`create`], with `between` awaited after the database is written and before the archive is, so
/// a test can act in the window the sweep hold has to cover.
async fn create_detached(
    root: &DataRoot,
    store: &Store,
    blobs: &BlobStore,
    dir: &Path,
    now: Timestamp,
    between: impl Future<Output = ()> + Send + 'static,
) -> Result<Snapshot, BackupError> {
    let (root, store, blobs, dir) = (root.clone(), store.clone(), blobs.clone(), dir.to_owned());
    tokio::spawn(async move { create_with(&root, &store, &blobs, &dir, now, between).await })
        .await?
}

async fn create_with(
    root: &DataRoot,
    store: &Store,
    blobs: &BlobStore,
    dir: &Path,
    now: Timestamp,
    between: impl Future<Output = ()>,
) -> Result<Snapshot, BackupError> {
    let _hold = blobs.hold_off_sweep().await;
    let snapshot = Snapshot::reserve(dir, &manual_stem(now))?;
    let written = async {
        store.vacuum_into(snapshot.database.clone()).await?;
        let memory = MemorySnapshots::take(root).await?;
        between.await;
        let (root, snapshot) = (root.clone(), snapshot.clone());
        tokio::task::spawn_blocking(move || snapshot.write_archive(&root, &memory)).await?
    }
    .await;
    match written {
        Ok(()) => Ok(snapshot),
        Err(err) => {
            snapshot.discard();
            Err(err)
        }
    }
}

/// The store options that write a snapshot of an existing database to `backups/` before its
/// migrations run, and only then: not for a new database and not when the schema is current.
///
/// The hook runs before any migration, so the snapshot is the old schema; restoring it needs the
/// binary that wrote it. The archive carries the memory databases ([`MemorySnapshots`]), which
/// that binary may not have written: opening one for its snapshot migrates it to the memory
/// schema this binary has. If the snapshot can't be written the open fails and the database is not
/// migrated: an upgrade that can't be undone doesn't start.
///
/// The hook runs on a thread of Tokio's blocking pool, as the store calls it from there, and
/// waits for the memory snapshots on the runtime that thread belongs to.
pub fn open_options(root: &DataRoot) -> giles_store::OpenOptions {
    let root = root.clone();
    giles_store::OpenOptions::new().before_migrate(move |plan| {
        let snapshot = write_before_migration(&root, Timestamp::now(), plan)?;
        info!(
            from = plan.from_version(),
            to = plan.to_version(),
            database = %snapshot.database.display(),
            archive = %snapshot.archive.display(),
            "backed up before migrating the database",
        );
        Ok(())
    })
}

/// Writes the pre-migration snapshot for `plan` to `backups/` and deletes all but the newest
/// [`KEEP_PRE_MIGRATION`] pre-migration snapshots (a failure to delete one is logged, not an
/// error).
///
/// Must be called from a thread of a Tokio runtime's blocking pool (as the store's
/// `before_migrate` hook is), not from async code: it blocks on the memory snapshots.
///
/// # Errors
///
/// [`BackupError`] if the snapshot couldn't be written; nothing is left behind.
///
/// # Panics
///
/// Outside a Tokio runtime, or inside an async task.
pub fn write_before_migration(
    root: &DataRoot,
    now: Timestamp,
    plan: &MigrationPlan<'_>,
) -> Result<Snapshot, BackupError> {
    let dir = root.backups_dir();
    let snapshot = Snapshot::reserve(&dir, &pre_migration_stem(now, plan.from_version()))?;
    let written = plan
        .vacuum_into(&snapshot.database)
        .map_err(BackupError::from)
        .and_then(|()| tokio::runtime::Handle::current().block_on(MemorySnapshots::take(root)))
        .and_then(|memory| snapshot.write_archive(root, &memory));
    if let Err(err) = written {
        snapshot.discard();
        return Err(err);
    }
    match prune_pre_migration(&dir, KEEP_PRE_MIGRATION) {
        Ok(removed) => {
            for path in removed {
                info!(path = %path.display(), "removed an old pre-migration backup");
            }
        }
        Err(err) => warn!(dir = %dir.display(), %err, "cannot prune old pre-migration backups"),
    }
    Ok(snapshot)
}

/// Deletes the files of all but the newest `keep` pre-migration snapshots in `dir`, returning the
/// files removed. Only names this module makes ([`pre_migration_stem`] plus `.db` or `.tar`) are
/// considered; anything else, including snapshots from `giles backup`, is left alone.
///
/// # Errors
///
/// An I/O error listing the directory or deleting a file.
pub fn prune_pre_migration(dir: &Path, keep: usize) -> io::Result<Vec<PathBuf>> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(stem) = name.to_str().and_then(pre_migration_stem_of) {
            files.push((stem.to_owned(), entry.path()));
        }
    }
    // Timestamps are fixed-width UTC, so name order is time order; newest first.
    let mut stems: Vec<&String> = files.iter().map(|(stem, _)| stem).collect();
    stems.sort_unstable();
    stems.dedup();
    stems.reverse();
    let doomed: Vec<&String> = stems.into_iter().skip(keep).collect();
    let mut removed = Vec::new();
    for (stem, path) in &files {
        if doomed.contains(&stem) {
            fs::remove_file(path)?;
            removed.push(path.clone());
        }
    }
    removed.sort();
    Ok(removed)
}

/// The stem of `name` if it is a pre-migration snapshot file name:
/// `giles-<YYYYMMDDTHHMMSSZ>-pre-migration-v<digits>.db` or `.tar`.
fn pre_migration_stem_of(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".db")
        .or_else(|| name.strip_suffix(".tar"))?;
    let rest = stem.strip_prefix("giles-")?;
    let (time, version) = rest.split_once(PRE_MIGRATION_MARK)?;
    let time_ok = time.len() == 16
        && time.bytes().enumerate().all(|(i, b)| match i {
            8 => b == b'T',
            15 => b == b'Z',
            _ => b.is_ascii_digit(),
        });
    let version_ok = !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit());
    (time_ok && version_ok).then_some(stem)
}

/// Writes the tar `archive` (an existing, empty file) of what a restore needs besides the
/// database, with paths relative to the data root so it unpacks over one:
///
/// - `giles.toml`;
/// - everything under `agents/`, `users/`, and `skills/`: the TOML and Markdown files people and
///   the memory tools write, and whatever else is in those trees (a skill's scripts), except the
///   memory databases (below);
/// - everything under `data/blobs/`, except the blob store's `sha256/tmp/` (puts in progress).
///
/// An agent's memory database is archived only as its `memory` snapshot, under the live
/// database's name (`agents/<agent>/memory.db`), in the place the walk reaches it. The live
/// `memory.db`, `memory.db-wal`, `memory.db-shm`, and `memory.db-journal` are never copied: a
/// copy of a live database can miss its WAL. So a `memory.db` with no snapshot in `memory` (one
/// created after [`MemorySnapshots::take`] listed the agents) is left out.
///
/// Left out: `secrets.toml` (credentials stay where the operator keeps them, so a backup copied
/// elsewhere never carries them), the database (the `.db` file), `plugins/` (binaries, installed
/// again), `data/plugins/` (state that belongs to plugins, which the core doesn't read),
/// `backups/`, `trash/` (agents removed in the web UI: the operator put them there to be rid of
/// them, and keeps them on this machine only), `run/`, and `cache/` (rebuilt; the memory
/// snapshots are staged there). Symbolic links
/// (with a warning for each, since memory files can be links) and other non-regular files are
/// skipped, never followed. Files that vanish while the
/// archive is written (a blob swept, a file edited by replacement) are skipped. A file that
/// changes size while it is copied is copied up to the size its header recorded, or fails the
/// backup if it shrank, so one entry can't corrupt the ones after it.
///
/// The `skip` files (the snapshot's own) are never added, even when the backup directory is inside
/// an archived tree. They are recognized by device and inode, not by path, because the backup
/// directory can be spelled differently from the data root (a symlinked root, a `..`), and an
/// archive that read itself would grow until the disk was full.
///
/// # Errors
///
/// [`BackupError::Io`].
pub fn write_archive(
    root: &DataRoot,
    archive: &Path,
    skip: &[&Path],
    memory: &MemorySnapshots,
) -> Result<(), BackupError> {
    let skip_ids = skip
        .iter()
        .map(|path| {
            fs::metadata(path)
                .map(|meta| FileId::of(&meta))
                .map_err(io_error(path))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let file = OpenOptions::new()
        .write(true)
        .open(archive)
        .map_err(io_error(archive))?;
    let mut builder = tar::Builder::new(BufWriter::new(file));
    builder.follow_symlinks(false);
    let walk = Walk {
        root: root.root(),
        skip: skip_ids,
        skip_dir: root.blobs_dir().join(BLOB_TMP),
        agents: root.agents_dir(),
        memory: &memory.files,
    };
    let result = (|| {
        for path in [
            root.config_file(),
            root.agents_dir(),
            root.users_dir(),
            root.skills_dir(),
            root.blobs_dir(),
        ] {
            walk.visit(&mut builder, &path)?;
        }
        Ok(())
    })();
    result.and_then(|()| finish(builder, archive))
}

fn finish(builder: tar::Builder<BufWriter<File>>, archive: &Path) -> Result<(), BackupError> {
    let writer = builder.into_inner().map_err(io_error(archive))?;
    let file = writer
        .into_inner()
        .map_err(|err| io_error(archive)(err.into_error()))?;
    file.sync_all().map_err(io_error(archive))
}

/// A file's identity, whatever path reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
}

type Builder = tar::Builder<BufWriter<File>>;

/// What `write_archive` walks and what it leaves out.
struct Walk<'a> {
    root: &'a Path,
    /// The snapshot's own files.
    skip: Vec<FileId>,
    skip_dir: PathBuf,
    /// `agents/`, whose sub-directories hold the memory databases.
    agents: PathBuf,
    /// The live memory databases and the snapshots archived in their place.
    memory: &'a BTreeMap<PathBuf, PathBuf>,
}

impl Walk<'_> {
    /// The name `path` has in the archive.
    fn name<'p>(&self, path: &'p Path) -> &'p Path {
        path.strip_prefix(self.root).unwrap_or(path)
    }

    /// Adds `path` if it is a directory (with everything below it) or a regular file. A link is
    /// never followed: it is skipped with a warning. A path that is missing is skipped.
    fn visit(&self, builder: &mut Builder, path: &Path) -> Result<(), BackupError> {
        let kind = match fs::symlink_metadata(path) {
            Ok(meta) => meta.file_type(),
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(io_error(path)(err)),
        };
        if kind.is_symlink() {
            warn!(
                path = %path.display(),
                "left a symbolic link out of the backup; what it points to is not archived",
            );
            Ok(())
        } else if kind.is_dir() {
            self.tree(builder, path)
        } else if kind.is_file() {
            self.file(builder, path)
        } else {
            Ok(())
        }
    }

    /// Adds one regular file, unless it is one of the snapshot's own. A memory database is added
    /// as its snapshot, and the files beside it are left out.
    fn file(&self, builder: &mut Builder, path: &Path) -> Result<(), BackupError> {
        if let Some(snapshot) = self.memory.get(path) {
            return self.append(builder, snapshot, path);
        }
        if self.is_memory_file(path) {
            return Ok(());
        }
        self.append(builder, path, path)
    }

    /// Whether `path` is a memory database or one of the files SQLite keeps beside it:
    /// `agents/<agent>/memory.db*`.
    fn is_memory_file(&self, path: &Path) -> bool {
        let in_agent_dir = path.parent().and_then(Path::parent) == Some(self.agents.as_path());
        in_agent_dir
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == MEMORY_DB || MEMORY_SIDECARS.contains(&name))
    }

    /// Adds the regular file `source` to the archive under the name of `path`.
    fn append(&self, builder: &mut Builder, source: &Path, path: &Path) -> Result<(), BackupError> {
        let file = match File::open(source) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(io_error(source)(err)),
        };
        // The opened file, not the path: it can't change identity or size under us.
        let meta = file.metadata().map_err(io_error(source))?;
        if !meta.is_file() || self.skip.contains(&FileId::of(&meta)) {
            return Ok(());
        }
        let mut header = tar::Header::new_gnu();
        header.set_metadata_in_mode(&meta, tar::HeaderMode::Complete);
        let reader = ExactReader::new(file, meta.len(), source);
        builder
            .append_data(&mut header, self.name(path), reader)
            .map_err(io_error(source))
    }

    /// Adds a directory and everything below it, in name order; a missing directory is skipped.
    fn tree(&self, builder: &mut Builder, dir: &Path) -> Result<(), BackupError> {
        if dir == self.skip_dir {
            return Ok(());
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(io_error(dir)(err)),
        };
        builder
            .append_dir(self.name(dir), dir)
            .map_err(io_error(dir))?;
        let mut children = entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()
            .map_err(io_error(dir))?;
        children.sort();
        for child in children {
            self.visit(builder, &child)?;
        }
        Ok(())
    }
}

/// Reads exactly `len` bytes of a file: no more, even if it grew, and an error if it ends early.
/// The tar header records the size up front, so an entry whose body disagrees would shift every
/// entry after it.
struct ExactReader<R> {
    inner: io::Take<R>,
    left: u64,
    path: PathBuf,
}

impl<R: io::Read> ExactReader<R> {
    fn new(inner: R, len: u64, path: &Path) -> Self {
        Self {
            inner: io::Read::take(inner, len),
            left: len,
            path: path.to_owned(),
        }
    }
}

impl<R: io::Read> io::Read for ExactReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n == 0 && self.left > 0 && !buf.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "{} got shorter while it was being copied; run the backup again",
                    self.path.display()
                ),
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}
