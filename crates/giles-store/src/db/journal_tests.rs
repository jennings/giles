//! The journal modes: each one applied, switching between them, and the behavior that differs
//! between WAL and the rollback-journal modes.

use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::oneshot;

use super::*;

const ROLLBACK_MODES: [JournalMode; 3] = [
    JournalMode::Delete,
    JournalMode::Truncate,
    JournalMode::Persist,
];

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

async fn open(path: &Path, mode: JournalMode) -> Store {
    Store::open_with(path, OpenOptions::new().journal_mode(mode))
        .await
        .unwrap()
}

async fn insert(store: &Store, job: String) {
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO job_runs (agent, job, last_run_at) VALUES ('a', ?1, 0)",
                [job],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn count(store: &Store) -> i64 {
    store
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM job_runs", [], |r| r.get(0))?))
        .await
        .unwrap()
}

/// What the writer connection reports for `PRAGMA name`.
async fn writer_pragma(store: &Store, name: &'static str) -> String {
    store
        .write(move |conn| {
            Ok(conn.query_row(&format!("PRAGMA {name}"), [], |row| {
                row.get::<_, rusqlite::types::Value>(0)
            })?)
        })
        .await
        .map(|v| match v {
            rusqlite::types::Value::Text(text) => text,
            other => format!("{other:?}"),
        })
        .unwrap()
}

/// The journal mode a brand-new connection finds in the file: only WAL is recorded there.
fn recorded_mode(path: &Path) -> String {
    Connection::open(path)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap()
}

#[tokio::test]
async fn every_mode_is_applied_with_the_matching_synchronous_setting() {
    for mode in JournalMode::ALL {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir.path().join("giles.db"), mode).await;
        assert_eq!(writer_pragma(&store, "journal_mode").await, mode.as_str());
        // NORMAL (1) is only safe in WAL; the rollback modes keep FULL (2).
        let expected = if mode == JournalMode::Wal {
            "Integer(1)"
        } else {
            "Integer(2)"
        };
        assert_eq!(
            writer_pragma(&store, "synchronous").await,
            expected,
            "{mode}"
        );
        let readers = store.inner.readers.lock().unwrap();
        for reader in readers.iter() {
            let synchronous: i64 = reader
                .query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap();
            assert_eq!(format!("Integer({synchronous})"), expected, "{mode}");
        }
    }
}

#[tokio::test]
async fn the_default_is_wal() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("giles.db")).await.unwrap();
    assert_eq!(writer_pragma(&store, "journal_mode").await, "wal");
}

#[tokio::test]
async fn switching_an_existing_database_between_any_two_modes_keeps_its_data() {
    for from in JournalMode::ALL {
        for to in JournalMode::ALL {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("giles.db");
            let store = open(&path, from).await;
            for i in 0..3 {
                insert(&store, format!("config:{i}")).await;
            }
            drop(store);

            let store = open(&path, to).await;
            assert_eq!(writer_pragma(&store, "journal_mode").await, to.as_str());
            assert_eq!(count(&store).await, 3, "{from} -> {to}");
            insert(&store, "config:after".into()).await;
            // Only WAL is recorded in the file, so it's also what survives for the next open.
            let recorded = if to == JournalMode::Wal {
                "wal"
            } else {
                "delete"
            };
            assert_eq!(recorded_mode(&path), recorded, "{from} -> {to}");
            drop(store);
            assert_eq!(recorded_mode(&path), recorded, "{from} -> {to}, closed");

            let store = open(&path, to).await;
            assert_eq!(count(&store).await, 4, "{from} -> {to}");
        }
    }
}

#[tokio::test]
async fn keeping_the_journal_mode_leaves_the_file_as_it_is() {
    for from in JournalMode::ALL {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giles.db");
        let store = open(&path, from).await;
        insert(&store, "config:x".into()).await;
        drop(store);

        let store = Store::open_with(&path, OpenOptions::new().keep_journal_mode())
            .await
            .unwrap();
        // The rollback modes aren't recorded in the file; they come back as the default one.
        let kept = if from == JournalMode::Wal {
            "wal"
        } else {
            "delete"
        };
        assert_eq!(writer_pragma(&store, "journal_mode").await, kept, "{from}");
        assert_eq!(store.inner.mode.as_str(), kept, "{from}");
        assert_eq!(count(&store).await, 1);
        insert(&store, "config:y".into()).await;
        assert_eq!(count(&store).await, 2);
        drop(store);
        assert_eq!(recorded_mode(&path), kept, "{from}");
    }
}

#[tokio::test]
async fn a_clean_close_leaves_no_journal_or_wal_file_in_any_mode() {
    for mode in JournalMode::ALL {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giles.db");
        let (wal, journal) = (sibling(&path, "-wal"), sibling(&path, "-journal"));
        let store = open(&path, mode).await;
        for i in 0..5 {
            insert(&store, format!("config:{i}")).await;
        }
        for _ in 0..8 {
            count(&store).await;
        }
        // While open, the file each mode leaves behind is the one the close has to clean up:
        // truncate and persist keep the journal between transactions, delete and WAL don't.
        assert_eq!(wal.exists(), mode == JournalMode::Wal, "{mode}");
        assert_eq!(
            journal.exists(),
            matches!(mode, JournalMode::Truncate | JournalMode::Persist),
            "{mode}"
        );
        drop(store);
        assert!(!wal.exists(), "{mode}: -wal left behind");
        assert!(!journal.exists(), "{mode}: -journal left behind");
    }
}

/// WAL lets the writer commit while a read is running; a rollback journal makes the commit wait
/// for the read, and `busy_timeout` is what turns that wait from `SQLITE_BUSY` into a delay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_in_a_rollback_mode_does_a_commit_wait_for_a_running_read() {
    for mode in JournalMode::ALL {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir.path().join("giles.db"), mode).await;
        insert(&store, "config:first".into()).await;

        let read_finished = Arc::new(AtomicBool::new(false));
        let (started, started_rx) = oneshot::channel();
        let reader = tokio::spawn({
            let (store, read_finished) = (store.clone(), read_finished.clone());
            async move {
                store
                    .read(move |conn| {
                        let seen: i64 =
                            conn.query_row("SELECT count(*) FROM job_runs", [], |r| r.get(0))?;
                        started.send(()).unwrap();
                        std::thread::sleep(Duration::from_millis(300));
                        read_finished.store(true, Ordering::SeqCst);
                        Ok(seen)
                    })
                    .await
            }
        });
        started_rx.await.unwrap();
        insert(&store, "config:second".into()).await;
        let waited = read_finished.load(Ordering::SeqCst);
        assert_eq!(waited, mode != JournalMode::Wal, "{mode}");
        // The read keeps the snapshot it started with and the write is not lost.
        assert_eq!(reader.await.unwrap().unwrap(), 1, "{mode}");
        assert_eq!(count(&store).await, 2, "{mode}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_during_an_open_write_transaction_see_the_committed_state_in_every_mode() {
    for mode in JournalMode::ALL {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir.path().join("giles.db"), mode).await;
        insert(&store, "config:first".into()).await;

        let (started, started_rx) = oneshot::channel();
        let writer = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .write(move |conn| {
                        conn.execute(
                            "INSERT INTO job_runs (agent, job, last_run_at) VALUES ('a', 'x', 0)",
                            [],
                        )?;
                        started.send(()).unwrap();
                        std::thread::sleep(Duration::from_millis(300));
                        Ok(())
                    })
                    .await
            }
        });
        started_rx.await.unwrap();
        // The write transaction is open and holds its lock; the read must neither fail nor see
        // the uncommitted row.
        assert_eq!(count(&store).await, 1, "{mode}");
        writer.await.unwrap().unwrap();
        assert_eq!(count(&store).await, 2, "{mode}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_operations_never_surface_busy_in_a_rollback_mode() {
    for mode in ROLLBACK_MODES {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir.path().join("giles.db"), mode).await;
        let mut tasks = tokio::task::JoinSet::new();
        for task in 0..8 {
            let store = store.clone();
            tasks.spawn(async move {
                for i in 0..12 {
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
        // Snapshots while all of that goes on.
        let snapshots: Vec<PathBuf> = (0..3)
            .map(|i| dir.path().join(format!("snapshot-{i}.db")))
            .collect();
        for dest in snapshots.clone() {
            let store = store.clone();
            tasks.spawn(async move { store.vacuum_into(dest).await });
        }
        while let Some(result) = tasks.join_next().await {
            result
                .unwrap()
                .unwrap_or_else(|err| panic!("{mode}: {err}"));
        }
        assert_eq!(count(&store).await, 8 * 12, "{mode}");
        for snapshot in snapshots {
            let copy = Connection::open(&snapshot).unwrap();
            let check: String = copy
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .unwrap();
            let rows: i64 = copy
                .query_row("SELECT count(*) FROM job_runs", [], |r| r.get(0))
                .unwrap();
            assert_eq!(check, "ok", "{mode}");
            assert!((0..=8 * 12).contains(&rows), "{mode}: {rows} rows");
        }
    }
}

#[tokio::test]
async fn a_snapshot_of_a_rollback_journal_database_is_standalone_and_opens_in_any_mode() {
    for mode in ROLLBACK_MODES {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir.path().join("giles.db"), mode).await;
        for i in 0..20 {
            insert(&store, format!("config:{i}")).await;
        }
        let dest = dir.path().join("snapshot.db");
        store.vacuum_into(dest.clone()).await.unwrap();
        // The store keeps working and the snapshot doesn't change.
        store
            .write(|conn| {
                conn.execute("DELETE FROM job_runs", [])?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(count(&store).await, 0);
        assert!(!sibling(&dest, "-wal").exists());
        assert!(!sibling(&dest, "-journal").exists());

        // An existing non-empty destination is refused, not overwritten.
        let err = store.vacuum_into(dest.clone()).await.unwrap_err();
        assert!(matches!(err, Error::Database(_)), "{mode}: {err}");

        // A restore is a copy of the snapshot: it opens in the mode it was taken in and in WAL.
        for target in [mode, JournalMode::Wal] {
            let restored = dir.path().join(format!("restored-{target}.db"));
            std::fs::copy(&dest, &restored).unwrap();
            let store = open(&restored, target).await;
            assert_eq!(count(&store).await, 20, "{mode} -> {target}");
        }
    }
}

/// A database file that can't be written can't be switched, and the error says which mode.
#[tokio::test]
async fn a_mode_sqlite_refuses_fails_the_open_naming_the_mode() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("giles.db");
    drop(open(&path, JournalMode::Delete).await);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    // Root ignores the permission bits, so there's nothing to refuse.
    if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
        return;
    }
    let err = Store::open_with(&path, OpenOptions::new().journal_mode(JournalMode::Wal))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::JournalMode {
                requested: JournalMode::Wal,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("wal"), "{err}");
}

/// A file SQLite can't open at all is a plain database error: `Error::JournalMode` is for a switch
/// SQLite refused, and its advice would send the operator after the wrong problem.
#[tokio::test]
async fn a_file_that_is_not_a_database_fails_as_a_database_error_in_every_mode() {
    for mode in JournalMode::ALL {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giles.db");
        std::fs::write(&path, "this is not a database, just text".repeat(100)).unwrap();
        let err = Store::open_with(&path, OpenOptions::new().journal_mode(mode))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Database(_)), "{mode}: {err:?}");
        assert!(err.to_string().contains("not a database"), "{mode}: {err}");
        let err = Store::open_with(&path, OpenOptions::new().keep_journal_mode())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Database(_)), "keep: {err:?}");
    }
}
