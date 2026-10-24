//! The [`Store`] handle: one writer connection, a few read-only connections, and the blocking
//! pool that runs them.
//!
//! SQLite allows one writer at a time. Funnelling every write through a single connection behind
//! an async mutex means our own writers queue in Tokio instead of colliding inside SQLite, so they
//! never see `SQLITE_BUSY`. In WAL mode readers don't block the writer or each other, so reads go
//! to a small pool of read-only connections and run in parallel with writes. `busy_timeout` stays
//! as a backstop for other processes touching the file.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use crate::migrate::{self, BeforeMigrate, Migration, MigrationPlan};
use crate::{BoxError, Error, Result};

/// How long a connection waits on a lock held by another process before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Read connections opened by default.
const DEFAULT_READERS: usize = 4;

/// Options for [`Store::open_with`].
pub struct OpenOptions {
    readers: usize,
    before_migrate: Option<BeforeMigrate>,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            readers: DEFAULT_READERS,
            before_migrate: None,
        }
    }
}

impl std::fmt::Debug for OpenOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenOptions")
            .field("readers", &self.readers)
            .field("before_migrate", &self.before_migrate.is_some())
            .finish()
    }
}

impl OpenOptions {
    /// Default options: 4 read connections, no pre-migration hook.
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of read-only connections (at least 1).
    pub fn readers(mut self, readers: usize) -> Self {
        self.readers = readers.max(1);
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
// to close is the writer, which checkpoints the WAL and deletes it. A read-only connection can't.
struct Inner {
    path: PathBuf,
    readers: Mutex<Vec<Connection>>,
    reader_slots: Arc<Semaphore>,
    writer: Arc<AsyncMutex<Connection>>,
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
            let mut writer = open_writer(&path)?;
            migrate::migrate(&mut writer, migrations, options.before_migrate)?;
            let readers = (0..options.readers)
                .map(|_| open_reader(&path))
                .collect::<Result<Vec<_>>>()?;
            Ok(Self {
                inner: Arc::new(Inner {
                    path,
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
    /// It runs on a pooled read connection, outside a transaction (`VACUUM` can't run inside
    /// one), so it sees one consistent state and blocks neither writers nor other readers. In WAL
    /// mode that is the difference from copying the file, which can capture a broken state.
    /// `dest` must not exist, or be an empty file, and must be valid UTF-8; the copy is a
    /// standalone database without a `-wal` file.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] if SQLite can't write `dest`, [`Error::InvalidRequest`] for a path
    /// that isn't UTF-8.
    pub async fn vacuum_into(&self, dest: PathBuf) -> Result<()> {
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

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

fn open_writer(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::corrupt(format!(
            "{} refused WAL mode (journal_mode is {mode})",
            path.display()
        )));
    }
    Ok(conn)
}

fn open_reader(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    configure(&conn)?;
    Ok(conn)
}

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
