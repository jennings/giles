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
    Migration {
        name: "0003_routes",
        sql: include_str!("migrations/0003_routes.sql"),
    },
    Migration {
        name: "0004_toolsets",
        sql: include_str!("migrations/0004_toolsets.sql"),
    },
    Migration {
        name: "0005_workspaces",
        sql: include_str!("migrations/0005_workspaces.sql"),
    },
    Migration {
        name: "0006_workspace_operator_reason",
        sql: include_str!("migrations/0006_workspace_operator_reason.sql"),
    },
    Migration {
        name: "0007_secrets",
        sql: include_str!("migrations/0007_secrets.sql"),
    },
    Migration {
        name: "0008_outbox_conversations",
        sql: include_str!("migrations/0008_outbox_conversations.sql"),
    },
    Migration {
        name: "0009_archived_conversations",
        sql: include_str!("migrations/0009_archived_conversations.sql"),
    },
    Migration {
        name: "0010_workspace_chat_deleted_reason",
        sql: include_str!("migrations/0010_workspace_chat_deleted_reason.sql"),
    },
    Migration {
        name: "0011_workspace_removed_outside_doozers",
        sql: include_str!("migrations/0011_workspace_removed_outside_doozers.sql"),
    },
    Migration {
        name: "0012_ssh_keys",
        sql: include_str!("migrations/0012_ssh_keys.sql"),
    },
    Migration {
        name: "0013_workspace_key_user",
        sql: include_str!("migrations/0013_workspace_key_user.sql"),
    },
    Migration {
        name: "0014_ssh_known_hosts",
        sql: include_str!("migrations/0014_ssh_known_hosts.sql"),
    },
    Migration {
        name: "0015_users",
        sql: include_str!("migrations/0015_users.sql"),
    },
    Migration {
        name: "0016_workspace_user",
        sql: include_str!("migrations/0016_workspace_user.sql"),
    },
    Migration {
        name: "0017_inbox_user",
        sql: include_str!("migrations/0017_inbox_user.sql"),
    },
    Migration {
        name: "0018_user_invites",
        sql: include_str!("migrations/0018_user_invites.sql"),
    },
    Migration {
        name: "0019_user_passwords",
        sql: include_str!("migrations/0019_user_passwords.sql"),
    },
    Migration {
        name: "0020_user_passkeys",
        sql: include_str!("migrations/0020_user_passkeys.sql"),
    },
    Migration {
        name: "0021_oidc",
        sql: include_str!("migrations/0021_oidc.sql"),
    },
    Migration {
        name: "0022_user_link_codes",
        sql: include_str!("migrations/0022_user_link_codes.sql"),
    },
];

/// The schema version this binary writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

/// Every migration of the per-agent memory database (`memory.db`), oldest first. It has its own
/// `user_version`, independent of [`MIGRATIONS`].
pub(crate) const MEMORY_MIGRATIONS: &[Migration] = &[Migration {
    name: "memory_0001_initial",
    sql: include_str!("memory_migrations/0001_initial.sql"),
}];

/// The schema version of the memory database this binary writes.
pub const MEMORY_SCHEMA_VERSION: i64 = MEMORY_MIGRATIONS.len() as i64;

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
            "routes",
            "routes_imported",
            "toolsets",
            "workspaces",
            "secrets",
            "secret_key",
            "archived_conversations",
            "user_invites",
            "user_passwords",
            "user_passkeys",
            "passkey_fake_key",
            "oidc_providers",
            "user_oidc_connections",
            "user_link_codes",
        ] {
            assert!(tables(&conn).iter().any(|t| t == table), "missing {table}");
        }
    }

    #[test]
    fn the_operator_reason_migration_keeps_workspaces_and_allows_the_reason() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &MIGRATIONS[..5], None).unwrap();
        conn.execute_batch(
            "INSERT INTO workspaces (id, environment, agent, channel, conversation, options,
                 description, state, created_at, last_used_at, deleted_at, delete_reason)
             VALUES ('ws-1', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'deleted', 1, 1, 2,
                 'model_delete'),
                    ('ws-2', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'active', 3, 3, NULL,
                 NULL);",
        )
        .unwrap();
        let rejected = conn.execute(
            "UPDATE workspaces SET delete_reason = 'operator' WHERE id = 'ws-1'",
            [],
        );
        assert!(rejected.is_err(), "0005 doesn't know the reason");

        assert_eq!(migrate(&mut conn, MIGRATIONS, None).unwrap(), 5);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM workspaces ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, ["ws-1", "ws-2"]);
        conn.execute(
            "UPDATE workspaces SET delete_reason = 'operator' WHERE id = 'ws-1'",
            [],
        )
        .unwrap();
        // The one-active-workspace-per-owner index came along.
        let second = conn.execute(
            "INSERT INTO workspaces (id, environment, agent, channel, conversation, options,
                 description, state, created_at, last_used_at)
             VALUES ('ws-3', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'active', 4, 4)",
            [],
        );
        assert!(second.is_err());
    }

    #[test]
    fn the_chat_deleted_reason_migration_keeps_workspaces_and_allows_the_reason() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &MIGRATIONS[..9], None).unwrap();
        conn.execute_batch(
            "INSERT INTO workspaces (id, environment, agent, channel, conversation, options,
                 description, state, created_at, last_used_at, deleted_at, delete_reason)
             VALUES ('ws-1', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'deleted', 1, 1, 2,
                 'operator'),
                    ('ws-2', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'active', 3, 3, NULL,
                 NULL);",
        )
        .unwrap();
        let set_reason = "UPDATE workspaces SET delete_reason = 'chat_deleted' WHERE id = 'ws-1'";
        assert!(
            conn.execute(set_reason, []).is_err(),
            "0006 doesn't know the reason"
        );

        assert_eq!(migrate(&mut conn, MIGRATIONS, None).unwrap(), 9);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT id, state FROM workspaces ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("ws-1".to_owned(), "deleted".to_owned()),
                ("ws-2".to_owned(), "active".to_owned())
            ]
        );
        conn.execute(set_reason, []).unwrap();
        let second = conn.execute(
            "INSERT INTO workspaces (id, environment, agent, channel, conversation, options,
                 description, state, created_at, last_used_at)
             VALUES ('ws-3', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'active', 4, 4)",
            [],
        );
        assert!(second.is_err(), "the one-active-workspace index came along");
    }

    #[test]
    fn the_doozers_reason_migration_renames_the_stored_reason() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &MIGRATIONS[..10], None).unwrap();
        conn.execute_batch(
            "INSERT INTO workspaces (id, environment, agent, channel, conversation, options,
                 description, state, created_at, last_used_at, deleted_at, delete_reason)
             VALUES ('ws-1', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'deleted', 1, 1, 2,
                 'removed_outside_giles'),
                    ('ws-2', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'deleted', 3, 3, 4,
                 'operator'),
                    ('ws-3', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'active', 5, 5, NULL,
                 NULL);",
        )
        .unwrap();

        assert_eq!(migrate(&mut conn, MIGRATIONS, None).unwrap(), 10);
        let rows: Vec<(String, Option<String>)> = conn
            .prepare("SELECT id, delete_reason FROM workspaces ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (
                    "ws-1".to_owned(),
                    Some("removed_outside_doozers".to_owned())
                ),
                ("ws-2".to_owned(), Some("operator".to_owned())),
                ("ws-3".to_owned(), None),
            ]
        );
        let old = "UPDATE workspaces SET delete_reason = 'removed_outside_giles' WHERE id = 'ws-1'";
        assert!(conn.execute(old, []).is_err(), "the old name is gone");
        let second = conn.execute(
            "INSERT INTO workspaces (id, environment, agent, channel, conversation, options,
                 description, state, created_at, last_used_at)
             VALUES ('ws-4', 'box', 'jeeves', 'web', 'c1', '{}', 'a box', 'active', 6, 6)",
            [],
        );
        assert!(second.is_err(), "the one-active-workspace index came along");
    }

    #[test]
    fn the_key_user_migration_backfills_conversations_from_their_direct_sessions() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &MIGRATIONS[..12], None).unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, agent, kind, channel, conversation, user_id, started_at,
                 ended_at)
             VALUES (1, 'jeeves', 'direct', 'web', 'c1', 'old', 1, 2),
                    (2, 'jeeves', 'direct', 'web', 'c1', 'owner', 3, NULL),
                    (3, 'jeeves', 'group', 'chat', 'g1', NULL, 1, NULL),
                    (4, 'jeeves', 'direct', 'web', 'c3', 'owner', 1, NULL),
                    (5, 'jeeves', 'job', NULL, NULL, 'owner', 1, NULL);
             INSERT INTO workspaces (id, environment, agent, channel, conversation, job_session,
                 options, description, state, created_at, last_used_at, deleted_at, delete_reason)
             VALUES ('ws-direct', 'box', 'jeeves', 'web', 'c1', NULL, '{}', 'a box', 'active', 1,
                 1, NULL, NULL),
                    ('ws-group', 'box', 'jeeves', 'chat', 'g1', NULL, '{}', 'a box', 'active', 1,
                 1, NULL, NULL),
                    ('ws-nosession', 'box', 'jeeves', 'web', 'c2', NULL, '{}', 'a box', 'active',
                 1, 1, NULL, NULL),
                    ('ws-old', 'box', 'jeeves', 'web', 'c3', NULL, '{}', 'a box', 'deleted', 1, 1,
                 2, 'operator'),
                    ('ws-job', 'box', NULL, NULL, NULL, 5, '{}', 'a box', 'active', 1, 1, NULL,
                 NULL);",
        )
        .unwrap();

        assert_eq!(migrate(&mut conn, MIGRATIONS, None).unwrap(), 12);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let rows: Vec<(String, Option<String>)> = conn
            .prepare("SELECT id, key_user FROM workspaces ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("ws-direct".to_owned(), Some("owner".to_owned())),
                ("ws-group".to_owned(), None),
                ("ws-job".to_owned(), None),
                ("ws-nosession".to_owned(), None),
                ("ws-old".to_owned(), Some("owner".to_owned())),
            ]
        );
    }

    #[test]
    fn the_workspace_user_migration_gives_conversation_workspaces_their_key_user_as_owner() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &MIGRATIONS[..15], None).unwrap();
        conn.execute_batch(
            "INSERT INTO workspaces (id, environment, agent, channel, conversation, job_session,
                 options, description, state, created_at, last_used_at, deleted_at, delete_reason,
                 key_user)
             VALUES ('ws-direct', 'box', 'jeeves', 'web', 'c1', NULL, '{}', 'a box', 'active', 1,
                 1, NULL, NULL, 'owner'),
                    ('ws-group', 'box', 'jeeves', 'chat', 'g1', NULL, '{}', 'a box', 'active', 1,
                 1, NULL, NULL, NULL),
                    ('ws-old', 'box', 'jeeves', 'web', 'c1', NULL, '{}', 'a box', 'deleted', 1, 1,
                 2, 'operator', 'owner'),
                    ('ws-job', 'box', NULL, NULL, NULL, 5, '{}', 'a box', 'active', 1, 1, NULL,
                 NULL, 'owner');",
        )
        .unwrap();

        assert_eq!(migrate(&mut conn, MIGRATIONS, None).unwrap(), 15);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let rows: Vec<(String, Option<String>)> = conn
            .prepare("SELECT id, user_id FROM workspaces ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("ws-direct".to_owned(), Some("owner".to_owned())),
                ("ws-group".to_owned(), None),
                ("ws-job".to_owned(), None),
                ("ws-old".to_owned(), Some("owner".to_owned())),
            ]
        );

        // The one-active-workspace index now allows one per user, and still one per group.
        let insert = |id: &str, channel: &str, user: Option<&str>| {
            conn.execute(
                "INSERT INTO workspaces (id, environment, agent, channel, conversation, options,
                     description, state, created_at, last_used_at, user_id)
                 VALUES (?1, 'box', 'jeeves', ?2, 'c1', '{}', 'a box', 'active', 6, 6, ?3)",
                rusqlite::params![id, channel, user],
            )
        };
        assert!(insert("ws-again", "web", Some("owner")).is_err());
        insert("ws-bob", "web", Some("bob")).unwrap();
        assert!(
            conn.execute(
                "INSERT INTO workspaces (id, environment, agent, channel, conversation, options,
                     description, state, created_at, last_used_at)
                 VALUES ('ws-group-twice', 'box', 'jeeves', 'chat', 'g1', '{}', 'a box', 'active',
                     6, 6)",
                [],
            )
            .is_err(),
            "a group conversation still has one"
        );
    }

    #[test]
    fn the_user_invites_migration_calls_existing_sessions_recovery_logins() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn, &MIGRATIONS[..17], None).unwrap();
        conn.execute(
            "INSERT INTO web_sessions (token_hash, user_id, created_at, expires_at)
             VALUES (zeroblob(32), 'owner', 1, 2)",
            [],
        )
        .unwrap();

        assert_eq!(migrate(&mut conn, MIGRATIONS, None).unwrap(), 17);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        let method: String = conn
            .query_row("SELECT method FROM web_sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(method, "recovery");
        let unknown = conn.execute(
            "INSERT INTO web_sessions (token_hash, user_id, method, created_at, expires_at)
             VALUES (zeroblob(31) || x'01', 'owner', 'telepathy', 1, 2)",
            [],
        );
        assert!(unknown.is_err(), "the method is one the store knows");
    }
}
