//! Schema migrations, embedded in the binary and tracked by SQLite's `user_version`.
//!
//! Migration `n` (1-based) is the SQL that takes the schema from version `n - 1` to `n`. A fresh
//! database is version 0. Opening applies every pending migration in one `IMMEDIATE` transaction
//! and sets `user_version` in the same transaction, so a failed upgrade leaves the database as it
//! was. Migrations are append-only: a released file is never edited.

use std::path::Path;

use rusqlite::{Connection, TransactionBehavior};

use crate::{BoxError, Error, Result};

/// One embedded migration.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Migration {
    /// File name without extension, for logs.
    pub(crate) name: &'static str,
    /// The SQL, run with `execute_batch`.
    pub(crate) sql: &'static str,
}

/// Every migration, oldest first. The schema version is the length of this list.
pub(crate) const MIGRATIONS: &[Migration] = &[
    Migration {
        name: "0001_initial",
        sql: include_str!("migrations/0001_initial.sql"),
    },
    Migration {
        name: "0002_blobs",
        sql: include_str!("migrations/0002_blobs.sql"),
    },
];

/// The schema version this binary writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// The hook [`crate::OpenOptions::before_migrate`] installs.
pub(crate) type BeforeMigrate =
    Box<dyn FnOnce(&MigrationPlan<'_>) -> Result<(), BoxError> + Send + 'static>;

/// An upgrade about to run on an existing database, passed to the
/// [`before_migrate`](crate::OpenOptions::before_migrate) hook.
///
/// The hook runs after the database is open and before any migration starts, outside any
/// transaction. It's the place to snapshot the database (PLAN 11.4: backups before migrations).
pub struct MigrationPlan<'a> {
    from: i64,
    to: i64,
    conn: &'a Connection,
}

impl MigrationPlan<'_> {
    /// The database's current schema version (at least 1: fresh databases have nothing to back
    /// up, so the hook isn't called for them).
    pub fn from_version(&self) -> i64 {
        self.from
    }

    /// The version the migrations will leave it at ([`SCHEMA_VERSION`]).
    pub fn to_version(&self) -> i64 {
        self.to
    }

    /// Writes a consistent copy of the database to `dest` with `VACUUM INTO`. `dest` must not
    /// exist and must be valid UTF-8.
    pub fn vacuum_into(&self, dest: &Path) -> Result<()> {
        vacuum_into(self.conn, dest)
    }
}

pub(crate) fn vacuum_into(conn: &Connection, dest: &Path) -> Result<()> {
    let dest = dest
        .to_str()
        .ok_or_else(|| Error::invalid(format!("path {} is not UTF-8", dest.display())))?;
    conn.execute("VACUUM INTO ?1", [dest])?;
    Ok(())
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Brings `conn` to the newest schema in `migrations`. Returns the version it started from.
pub(crate) fn migrate(
    conn: &mut Connection,
    migrations: &[Migration],
    before: Option<BeforeMigrate>,
) -> Result<i64> {
    let latest = i64::try_from(migrations.len()).expect("a handful of migrations");
    let found = user_version(conn)?;
    if found > latest {
        return Err(Error::SchemaTooNew {
            found,
            supported: latest,
        });
    }
    if found == latest {
        return Ok(found);
    }
    if found > 0
        && let Some(before) = before
    {
        let plan = MigrationPlan {
            from: found,
            to: latest,
            conn,
        };
        before(&plan).map_err(Error::BeforeMigrate)?;
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another process may have migrated between the check and the lock; start from what's there.
    let current = user_version(&tx)?;
    if current > latest {
        return Err(Error::SchemaTooNew {
            found: current,
            supported: latest,
        });
    }
    let start = usize::try_from(current).map_err(|_| Error::corrupt("negative user_version"))?;
    for migration in &migrations[start..] {
        tracing::info!(migration = migration.name, "applying database migration");
        tx.execute_batch(migration.sql)?;
    }
    tx.pragma_update(None, "user_version", latest)?;
    tx.commit()?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: Migration = Migration {
        name: "first",
        sql: "CREATE TABLE a (x INTEGER) STRICT;",
    };
    const SECOND: Migration = Migration {
        name: "second",
        sql: "CREATE TABLE b (y INTEGER) STRICT;",
    };
    const BROKEN: Migration = Migration {
        name: "broken",
        sql: "CREATE TABLE c (z INTEGER) STRICT; THIS IS NOT SQL;",
    };

    fn tables(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn upgrades_step_by_step_and_calls_the_hook_only_for_existing_databases() {
        let mut conn = Connection::open_in_memory().unwrap();
        let fresh_hook: BeforeMigrate = Box::new(|_| panic!("no backup of a fresh database"));
        assert_eq!(migrate(&mut conn, &[FIRST], Some(fresh_hook)).unwrap(), 0);
        assert_eq!(user_version(&conn).unwrap(), 1);

        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = seen.clone();
        let hook: BeforeMigrate = Box::new(move |plan| {
            // The hook sees the database before the upgrade.
            let has_b = plan
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name = 'b'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap();
            *record.lock().unwrap() = Some((plan.from_version(), plan.to_version(), has_b));
            Ok(())
        });
        assert_eq!(migrate(&mut conn, &[FIRST, SECOND], Some(hook)).unwrap(), 1);
        assert_eq!(*seen.lock().unwrap(), Some((1, 2, 0)));
        assert_eq!(tables(&conn), ["a", "b"]);
        assert_eq!(user_version(&conn).unwrap(), 2);

        // Up to date: nothing runs, the hook isn't called.
        let idle_hook: BeforeMigrate = Box::new(|_| panic!("nothing to migrate"));
        assert_eq!(
            migrate(&mut conn, &[FIRST, SECOND], Some(idle_hook)).unwrap(),
            2
        );
    }

    #[test]
    fn a_failed_migration_rolls_back_every_pending_step() {
        let mut conn = Connection::open_in_memory().unwrap();
        let err = migrate(&mut conn, &[FIRST, BROKEN], None).unwrap_err();
        assert!(matches!(err, Error::Database(_)), "{err}");
        assert_eq!(user_version(&conn).unwrap(), 0);
        assert!(tables(&conn).is_empty());
    }

    #[test]
    fn a_failing_hook_stops_the_upgrade() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &[FIRST], None).unwrap();
        let hook: BeforeMigrate = Box::new(|_| Err("disk full".into()));
        let err = migrate(&mut conn, &[FIRST, SECOND], Some(hook)).unwrap_err();
        assert!(matches!(err, Error::BeforeMigrate(_)), "{err}");
        assert_eq!(user_version(&conn).unwrap(), 1);
        assert_eq!(tables(&conn), ["a"]);
    }

    #[test]
    fn refuses_a_database_from_a_newer_binary() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &[FIRST, SECOND], None).unwrap();
        let err = migrate(&mut conn, &[FIRST], None).unwrap_err();
        assert!(
            matches!(
                err,
                Error::SchemaTooNew {
                    found: 2,
                    supported: 1
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn the_embedded_schema_applies_to_an_empty_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, MIGRATIONS, None).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        for table in [
            "sessions",
            "messages",
            "summaries",
            "search",
            "inbox",
            "outbox",
            "jobs",
            "job_runs",
            "usage",
            "web_sessions",
            "blobs",
        ] {
            assert!(tables(&conn).iter().any(|t| t == table), "missing {table}");
        }
    }
}
