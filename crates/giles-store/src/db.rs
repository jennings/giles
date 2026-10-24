//! The [`Store`] handle: one writer connection, a few read-only connections, and the blocking
//! pool that runs them.
//!
//! SQLite allows one writer at a time. Funnelling every write through a single connection behind
//! an async mutex means our own writers queue in Tokio instead of colliding inside SQLite, so they
//! never see `SQLITE_BUSY`. In WAL mode (the default) readers don't block the writer or each
//! other, so reads go to a small pool of read-only connections and run in parallel with writes.
//! `busy_timeout` stays as a backstop for other processes touching the file.
//!
//! In the rollback-journal modes ([`JournalMode`]) a commit needs every reader to be out, and
//! readers wait while it commits, so `busy_timeout` is what makes them wait instead of fail with
//! `SQLITE_BUSY`. That holds because reads are short and readers never try to become writers
//! (the read connections are read-only, so there is no `SQLITE_BUSY_SNAPSHOT` and no
//! reader-to-writer lock upgrade that SQLite resolves by failing at once). A read that runs
//! longer than `busy_timeout` can fail the commit waiting for it; `VACUUM INTO` is the one long
//! read, so it runs on the writer connection there ([`Store::vacuum_into`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::JournalMode;
use crate::migrate::{self, BeforeMigrate, Migration, MigrationPlan};
use crate::{BoxError, Error, Result};

/// How long a connection waits on a lock held by another process before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Read connections opened by default.
const DEFAULT_READERS: usize = 4;

/// Options for [`Store::open_with`].
pub struct OpenOptions {
    readers: usize,
    /// `None`: leave the database in the mode it is in.
    journal_mode: Option<JournalMode>,
    before_migrate: Option<BeforeMigrate>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            readers: DEFAULT_READERS,
            journal_mode: Some(JournalMode::default()),
            before_migrate: None,
        }
    }
}

impl std::fmt::Debug for OpenOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenOptions")
            .field("readers", &self.readers)
            .field("journal_mode", &self.journal_mode)
            .field("before_migrate", &self.before_migrate.is_some())
            .finish()
    }
}

impl OpenOptions {
    /// Default options: 4 read connections, WAL, no pre-migration hook.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of read-only connections (at least 1).
    pub fn readers(mut self, readers: usize) -> Self {
        self.readers = readers.max(1);
        self
    }

    /// The journal mode to run the database in ([`JournalMode::Wal`] unless set). Opening
    /// switches an existing database to it, whichever mode it was last in, with no manual step.
    /// If SQLite can't (WAL on a file system without shared memory, or leaving WAL while another
    /// process has the database open) the open fails with [`Error::JournalMode`].
    pub fn journal_mode(mut self, mode: JournalMode) -> Self {
        self.journal_mode = Some(mode);
        self
    }

    /// Opens the database in whatever mode it is in, without changing it: WAL if the file is
    /// recorded as WAL, otherwise `delete`. For tools that run beside the configuration, like
    /// `giles instance rename`, and must not undo the daemon's choice.
    pub fn keep_journal_mode(mut self) -> Self {
        self.journal_mode = None;
        self
    }

    /// Runs `hook` when an existing database (schema version 1 or later) is about to be
    /// migrated, before any migration starts. An error from the hook aborts the open with
    /// [`Error::BeforeMigrate`] and leaves the database untouched. This is where the core
    /// snapshots the database before an upgrade (PLAN 11.4).
    pub fn before_migrate(
        mut self,
        hook: impl FnOnce(&MigrationPlan<'_>) -> Result<(), BoxError> + Send + 'static,
    ) -> Self {
        self.before_migrate = Some(Box::new(hook));
        self
    }
}

/// An open database. Cheap to clone; all clones share the connections.
///
/// Every method runs its SQL on Tokio's blocking pool. A write runs in one `IMMEDIATE`
/// transaction; a read runs in one read transaction, so it sees a consistent snapshot. Once a
/// call has started on the blocking pool it finishes even if its future is dropped.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

// Fields drop in declaration order: the readers close before the writer, so the last connection
// to close is the writer, which in WAL mode checkpoints the WAL and deletes it. A read-only
// connection can't.
struct Inner {
    path: PathBuf,
    mode: JournalMode,
    readers: Mutex<Vec<Connection>>,
    reader_slots: Arc<Semaphore>,
    writer: Arc<AsyncMutex<Connection>>,
}

impl Drop for Inner {
    /// `truncate` and `persist` leave the (empty or invalidated) journal file behind after every
    /// commit. Neither is recorded in the database file, so switching the writer to `delete`
    /// before it closes removes the file without changing anything the next open sees. A writer
    /// still busy with a write that outlives the store's last handle is skipped.
    fn drop(&mut self) {
        if matches!(self.mode, JournalMode::Truncate | JournalMode::Persist)
            && let Ok(writer) = self.writer.try_lock()
            && let Err(err) = writer.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))
        {
            tracing::warn!(path = %self.path.display(), %err, "cannot remove the journal file on close");
        }
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens (creating if needed) the database at `path` with default options and migrates it
    /// to [`SCHEMA_VERSION`](crate::SCHEMA_VERSION). The parent directory must exist.
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self> {
        Self::open_with(path, OpenOptions::default()).await
    }

    /// Like [`open`](Self::open), with options.
    pub async fn open_with(path: impl Into<PathBuf>, options: OpenOptions) -> Result<Self> {
        Self::open_migrating(path.into(), options, migrate::MIGRATIONS).await
    }

    pub(crate) async fn open_migrating(
        path: PathBuf,
        options: OpenOptions,
        migrations: &'static [Migration],
    ) -> Result<Self> {
        tokio::task::spawn_blocking(move || {
            let (mut writer, mode) = open_writer(&path, options.journal_mode)?;
            migrate::migrate(&mut writer, migrations, options.before_migrate)?;
            let readers = (0..options.readers)
                .map(|_| open_reader(&path, mode))
                .collect::<Result<Vec<_>>>()?;
            Ok(Self {
                inner: Arc::new(Inner {
                    path,
                    mode,
                    writer: Arc::new(AsyncMutex::new(writer)),
                    reader_slots: Arc::new(Semaphore::new(readers.len())),
                    readers: Mutex::new(readers),
                }),
            })
        })
        .await?
    }

    /// The database file.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Runs `f` in a write transaction on the writer connection and commits if it returns `Ok`.
    pub(crate) async fn write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let mut conn = self.inner.writer.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })
        .await?
    }

    /// Runs `f` in a read transaction on a pooled read-only connection.
    pub(crate) async fn read<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let permit = self
            .inner
            .reader_slots
            .clone()
            .acquire_owned()
            .await
            .expect("the reader semaphore is never closed");
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut pooled = Pooled::take(&inner);
            let tx = pooled.conn().transaction()?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })
        .await?
    }

    /// Writes a consistent snapshot of the database to `dest` with `VACUUM INTO`, while the
    /// store keeps serving (PLAN 11.4).
    ///
    /// In WAL mode it runs on a pooled read connection, outside a transaction (`VACUUM` can't run
    /// inside one), so it sees one consistent state and blocks neither writers nor other readers;
    /// copying the file instead can capture a broken state. In a rollback-journal mode a long
    /// read would stall a writer's commit until `busy_timeout` ran out and failed it, so the
    /// snapshot runs on the writer connection instead: writes queue behind it, reads go on.
    /// `dest` must not exist, or be an empty file, and must be valid UTF-8; the copy is a
    /// standalone database without a `-wal` or `-journal` file.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] if SQLite can't write `dest`, [`Error::InvalidRequest`] for a path
    /// that isn't UTF-8.
    pub async fn vacuum_into(&self, dest: PathBuf) -> Result<()> {
        if self.inner.mode != JournalMode::Wal {
            let conn = self.inner.writer.clone().lock_owned().await;
            return tokio::task::spawn_blocking(move || migrate::vacuum_into(&conn, &dest)).await?;
        }
        let permit = self
            .inner
            .reader_slots
            .clone()
            .acquire_owned()
            .await
            .expect("the reader semaphore is never closed");
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut pooled = Pooled::take(&inner);
            migrate::vacuum_into(pooled.conn(), &dest)
        })
        .await?
    }
}

/// A read connection checked out of the pool; it goes back on drop, even after a panic.
struct Pooled<'a> {
    inner: &'a Inner,
    conn: Option<Connection>,
}

impl<'a> Pooled<'a> {
    fn take(inner: &'a Inner) -> Self {
        let conn = inner
            .readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .expect("a semaphore permit guarantees a free reader");
        Self {
            inner,
            conn: Some(conn),
        }
    }

    fn conn(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("present until drop")
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.inner
                .readers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(conn);
        }
    }
}

/// Settings every connection gets once its journal mode is known.
fn configure(conn: &Connection, mode: JournalMode) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", true)?;
    // NORMAL is the documented safe pairing with WAL: a power loss can drop the last commits but
    // not corrupt the database. In a rollback-journal mode it isn't: a power loss at the wrong
    // time can corrupt the database, so those modes keep SQLite's default, FULL.
    let synchronous = if mode == JournalMode::Wal {
        "NORMAL"
    } else {
        "FULL"
    };
    conn.pragma_update(None, "synchronous", synchronous)?;
    Ok(())
}

/// Opens the writer and returns it with the journal mode it ended up in: `requested`, or, for
/// `None`, the one the file is already in.
///
/// The current mode is read first, so a file SQLite can't open at all (not a database, locked by
/// another process) fails as [`Error::Database`], and [`Error::JournalMode`] means only that
/// SQLite refused a switch it was asked to make.
fn open_writer(path: &Path, requested: Option<JournalMode>) -> Result<(Connection, JournalMode)> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    let current = current_journal_mode(&conn)?;
    let mode = match requested {
        Some(want) if want != current => set_journal_mode(&conn, want)?,
        _ => current,
    };
    configure(&conn, mode)?;
    Ok((conn, mode))
}

fn open_reader(path: &Path, mode: JournalMode) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    configure(&conn, mode)?;
    Ok(conn)
}

/// Switches the database to `want`. WAL is recorded in the file, so this changes the file when it
/// was in the other kind of mode (and needs no other connection open); the rollback-journal
/// modes are per connection. SQLite either fails or answers with the mode it kept, e.g. `delete`
/// when WAL can't run.
fn set_journal_mode(conn: &Connection, want: JournalMode) -> Result<JournalMode> {
    let refused = |reason: String| Error::JournalMode {
        requested: want,
        reason,
    };
    let got: String = conn
        .query_row(&format!("PRAGMA journal_mode = {want}"), [], |row| {
            row.get(0)
        })
        .map_err(|err| refused(err.to_string()))?;
    if got.eq_ignore_ascii_case(want.as_str()) {
        Ok(want)
    } else {
        Err(refused(format!("SQLite kept journal_mode {got}")))
    }
}

/// The mode a fresh connection finds: `wal` if the file is recorded as WAL, otherwise `delete`.
fn current_journal_mode(conn: &Connection) -> Result<JournalMode> {
    let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    mode.parse()
        .map_err(|err| Error::corrupt(format!("unexpected journal_mode {mode:?}: {err}")))
}

#[cfg(test)]
mod journal_tests;

#[cfg(test)]
mod tests {
    use super::*;

    async fn pragma(store: &Store, name: &'static str) -> String {
        store
            .read(move |conn| {
                Ok(conn.query_row(&format!("PRAGMA {name}"), [], |row| {
                    row.get::<_, rusqlite::types::Value>(0)
                })?)
            })
            .await
            .map(|v| format!("{v:?}"))
            .unwrap()
    }

    #[tokio::test]
    async fn connections_use_wal_foreign_keys_and_normal_sync() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        assert_eq!(pragma(&store, "journal_mode").await, r#"Text("wal")"#);
        assert_eq!(pragma(&store, "foreign_keys").await, "Integer(1)");
        // NORMAL is 1.
        assert_eq!(pragma(&store, "synchronous").await, "Integer(1)");
        assert_eq!(pragma(&store, "busy_timeout").await, "Integer(5000)");
        let writer_fk = store
            .write(|conn| Ok(conn.query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))?))
            .await
            .unwrap();
        assert_eq!(writer_fk, 1);
    }

    #[tokio::test]
    async fn the_bundled_sqlite_has_fts5_and_json() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let (fts5, json) = store
            .read(|conn| {
                let fts5 =
                    conn.query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
                        r.get::<_, i64>(0)
                    })?;
                let json =
                    conn.query_row(r#"SELECT '{"a":1}' ->> '$.a'"#, [], |r| r.get::<_, i64>(0))?;
                Ok((fts5, json))
            })
            .await
            .unwrap();
        assert_eq!((fts5, json), (1, 1));
    }

    #[tokio::test]
    async fn readers_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let err = store
            .read(|conn| {
                conn.execute("DELETE FROM sessions", [])?;
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Database(_)), "{err}");
    }

    #[tokio::test]
    async fn a_clean_close_checkpoints_and_removes_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giles.db");
        let wal = dir.path().join("giles.db-wal");
        let store = Store::open(&path).await.unwrap();
        for i in 0..50 {
            store
                .write(move |conn| {
                    conn.execute(
                        "INSERT INTO job_runs (agent, job, last_run_at) VALUES ('a', ?1, 0)",
                        [format!("config:{i}")],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        // Use the readers too, so each has had the WAL open.
        for _ in 0..8 {
            store
                .read(|conn| {
                    Ok(conn
                        .query_row("SELECT count(*) FROM job_runs", [], |r| r.get::<_, i64>(0))?)
                })
                .await
                .unwrap();
        }
        assert!(wal.exists());
        drop(store);
        assert!(!wal.exists(), "the writer closed last and removed the WAL");
    }

    #[tokio::test]
    async fn vacuum_into_snapshots_a_live_wal_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giles.db");
        let store = Store::open(&path).await.unwrap();
        for i in 0..20 {
            store
                .write(move |conn| {
                    conn.execute(
                        "INSERT INTO job_runs (agent, job, last_run_at) VALUES ('a', ?1, ?2)",
                        rusqlite::params![format!("config:{i}"), i],
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        // The writes are still in the WAL, which a plain copy of the main file would miss.
        assert!(dir.path().join("giles.db-wal").exists());

        let dest = dir.path().join("snapshot.db");
        store.vacuum_into(dest.clone()).await.unwrap();
        // The store keeps working, and the copy is a standalone database.
        store
            .write(|conn| {
                conn.execute("DELETE FROM job_runs", [])?;
                Ok(())
            })
            .await
            .unwrap();
        let copy = Connection::open(&dest).unwrap();
        let rows: i64 = copy
            .query_row("SELECT count(*) FROM job_runs", [], |r| r.get(0))
            .unwrap();
        let version: i64 = copy
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!((rows, version), (20, crate::SCHEMA_VERSION));
        assert!(!dir.path().join("snapshot.db-wal").exists());

        // An existing non-empty destination is refused, not overwritten.
        let err = store.vacuum_into(dest).await.unwrap_err();
        assert!(matches!(err, Error::Database(_)), "{err}");
    }

    #[tokio::test]
    async fn reopening_is_idempotent_and_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giles.db");
        let store = Store::open(&path).await.unwrap();
        store
            .write(|conn| {
                conn.execute(
                    "INSERT INTO job_runs (agent, job, last_run_at) VALUES ('a', 'config:x', 1)",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        drop(store);

        let store = Store::open_with(
            &path,
            OpenOptions::new().before_migrate(|_| panic!("already current")),
        )
        .await
        .unwrap();
        let count = store
            .read(|conn| {
                Ok(conn.query_row("SELECT count(*) FROM job_runs", [], |r| r.get::<_, i64>(0))?)
            })
            .await
            .unwrap();
        assert_eq!(count, 1_i64);
    }

    #[tokio::test]
    async fn the_hook_can_snapshot_before_an_upgrade() {
        static V1: &[Migration] = &[Migration {
            name: "v1",
            sql: "CREATE TABLE t (x INTEGER) STRICT; INSERT INTO t VALUES (7);",
        }];
        static V2: &[Migration] = &[
            V1[0],
            Migration {
                name: "v2",
                sql: "DROP TABLE t;",
            },
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giles.db");
        drop(
            Store::open_migrating(path.clone(), OpenOptions::new(), V1)
                .await
                .unwrap(),
        );

        let backup = dir.path().join("before.db");
        let dest = backup.clone();
        let options = OpenOptions::new().before_migrate(move |plan| {
            assert_eq!((plan.from_version(), plan.to_version()), (1, 2));
            plan.vacuum_into(&dest)?;
            Ok(())
        });
        Store::open_migrating(path, options, V2).await.unwrap();

        let copy = Connection::open(&backup).unwrap();
        let x: i64 = copy.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(x, 7);
    }

    #[tokio::test]
    async fn a_version_1_database_upgrades_to_the_current_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giles.db");
        let v1 = Store::open_migrating(path.clone(), OpenOptions::new(), &migrate::MIGRATIONS[..1])
            .await
            .unwrap();
        v1.write(|conn| {
            conn.execute(
                "INSERT INTO job_runs (agent, job, last_run_at) VALUES ('a', 'config:x', 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        drop(v1);

        let seen = Arc::new(Mutex::new(None));
        let record = seen.clone();
        let options = OpenOptions::new().before_migrate(move |plan| {
            *record.lock().unwrap() = Some((plan.from_version(), plan.to_version()));
            Ok(())
        });
        let store = Store::open_with(&path, options).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), Some((1, crate::SCHEMA_VERSION)));
        let (runs, blobs) = store
            .read(|conn| {
                let runs = conn.query_row("SELECT count(*) FROM job_runs", [], |r| r.get(0))?;
                let blobs = conn.query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))?;
                Ok((runs, blobs))
            })
            .await
            .unwrap();
        assert_eq!((runs, blobs), (1_i64, 0_i64));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reads_and_writes_never_surface_busy() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let mut tasks = tokio::task::JoinSet::new();
        for task in 0..16 {
            let store = store.clone();
            tasks.spawn(async move {
                for i in 0..25 {
                    store
                        .write(move |conn| {
                            conn.execute(
                                "INSERT INTO job_runs (agent, job, last_run_at) VALUES (?1, ?2, 0)",
                                (format!("agent-{task}"), format!("config:{i}")),
                            )?;
                            Ok(())
                        })
                        .await?;
                    store
                        .read(|conn| {
                            Ok(conn.query_row("SELECT count(*) FROM job_runs", [], |r| {
                                r.get::<_, i64>(0)
                            })?)
                        })
                        .await?;
                }
                Ok::<_, Error>(())
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        let total = store
            .read(|conn| {
                Ok(conn.query_row("SELECT count(*) FROM job_runs", [], |r| r.get::<_, i64>(0))?)
            })
            .await
            .unwrap();
        assert_eq!(total, 16 * 25_i64);
    }
}
