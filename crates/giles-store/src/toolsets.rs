//! `toolsets`: the MCP toolsets the web UI manages.
//!
//! The store keeps each toolset's `spec` as the JSON text it was given and checks nothing about
//! it: parsing and validating a spec is the core's business (`giles_core::config`). Names are
//! unique without regard to ASCII case (the column's `NOCASE` collation), and every lookup by
//! name ignores case too; the name comes back as it was first written (or last renamed to).
//!
//! `updated_at` is the stale-update token: [`Store::toolset_update`] can name the value the caller
//! loaded and fails with [`Error::StaleToolset`] if it has moved. An update or rename stamps it
//! with the caller's `now`, but never earlier than a millisecond past the stored value, so two
//! writes in the same millisecond, or after the clock stepped back, still differ.

use giles_types::Timestamp;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::time::{from_millis, to_millis};
use crate::{Error, Result, Store, get_time, query_all};

/// A stored toolset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToolset {
    /// The toolset's name, as written.
    pub name: String,
    /// The spec, as JSON text.
    pub spec: String,
    /// When the toolset was added. A rename keeps it.
    pub created_at: Timestamp,
    /// When the toolset was last changed (updated or renamed).
    pub updated_at: Timestamp,
}

const COLUMNS: &str = "name, spec, created_at, updated_at";

fn from_row(row: &Row<'_>) -> Result<StoredToolset> {
    Ok(StoredToolset {
        name: row.get(0)?,
        spec: row.get(1)?,
        created_at: get_time(row, 2)?,
        updated_at: get_time(row, 3)?,
    })
}

/// The toolset called `name` (any case), if there is one.
fn find(conn: &Connection, name: &str) -> Result<Option<StoredToolset>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {COLUMNS} FROM toolsets WHERE name = ?1"))?;
    stmt.query_row([name], |row| Ok(from_row(row)))
        .optional()?
        .transpose()
}

/// The time to stamp a change with: `now`, but always after `previous`, so the stamp changes even
/// when two writes land in the same millisecond or the clock stepped back.
fn next_stamp(now: Timestamp, previous: Timestamp) -> Result<i64> {
    let millis = to_millis(now).max(to_millis(previous).saturating_add(1));
    from_millis(millis)?;
    Ok(millis)
}

fn require(conn: &Connection, name: &str) -> Result<StoredToolset> {
    find(conn, name)?.ok_or(Error::NoSuchToolset)
}

impl Store {
    /// Every toolset, ordered by lower-cased name, then by name.
    pub async fn toolsets(&self) -> Result<Vec<StoredToolset>> {
        self.read(|conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM toolsets ORDER BY lower(name), name"
            ))?;
            query_all(&mut stmt, [], from_row)
        })
        .await
    }

    /// Adds a toolset, with `created_at` and `updated_at` set to `now`, and returns it.
    ///
    /// # Errors
    ///
    /// [`Error::ToolsetExists`] if `name` is taken, ignoring ASCII case. Nothing changes.
    pub async fn toolset_insert(
        &self,
        name: &str,
        spec: &str,
        now: Timestamp,
    ) -> Result<StoredToolset> {
        let (name, spec) = (name.to_owned(), spec.to_owned());
        self.write(move |conn| {
            if find(conn, &name)?.is_some() {
                return Err(Error::ToolsetExists);
            }
            let now = to_millis(now);
            conn.prepare_cached(
                "INSERT INTO toolsets (name, spec, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?3)",
            )?
            .execute(params![name, spec, now])?;
            require(conn, &name)
        })
        .await
    }

    /// Replaces the spec of toolset `name` (any case), sets `updated_at` to `now` (or just past the stored value, whichever is later), and
    /// returns it. When `expected_updated_at` is set, it must equal the stored `updated_at`; the compare and
    /// the write are one transaction, so a concurrent change can't slip between.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchToolset`] if there is no such toolset (checked first);
    /// [`Error::StaleToolset`] if `expected_updated_at` is set and differs from the stored value.
    /// Nothing changes in either case.
    pub async fn toolset_update(
        &self,
        name: &str,
        spec: &str,
        expected_updated_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<StoredToolset> {
        let (name, spec) = (name.to_owned(), spec.to_owned());
        self.write(move |conn| {
            let current = require(conn, &name)?;
            if expected_updated_at
                .is_some_and(|expected| to_millis(expected) != to_millis(current.updated_at))
            {
                return Err(Error::StaleToolset);
            }
            let stamp = next_stamp(now, current.updated_at)?;
            conn.prepare_cached("UPDATE toolsets SET spec = ?2, updated_at = ?3 WHERE name = ?1")?
                .execute(params![name, spec, stamp])?;
            require(conn, &name)
        })
        .await
    }

    /// Deletes toolset `name` (any case). Returns `false` if there is none.
    pub async fn toolset_remove(&self, name: &str) -> Result<bool> {
        let name = name.to_owned();
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("DELETE FROM toolsets WHERE name = ?1")?
                .execute([name])?
                == 1)
        })
        .await
    }

    /// Renames toolset `old` (any case) to `new`, keeping its spec and `created_at`, sets
    /// `updated_at` to `now` (or just past the stored value, whichever is later), and returns it.
    /// Changing only the case of the name is allowed.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchToolset`] if `old` is absent (checked first); [`Error::ToolsetExists`] if
    /// `new` belongs to a different toolset. Nothing changes in either case.
    pub async fn toolset_rename(
        &self,
        old: &str,
        new: &str,
        now: Timestamp,
    ) -> Result<StoredToolset> {
        let (old, new) = (old.to_owned(), new.to_owned());
        self.write(move |conn| {
            let current = require(conn, &old)?;
            // The column's collation makes `<>` ignore case, so the row being renamed never
            // counts as a clash with its own new spelling.
            let taken: i64 = conn
                .prepare_cached("SELECT count(*) FROM toolsets WHERE name = ?1 AND name <> ?2")?
                .query_row([&new, &old], |row| row.get(0))?;
            if taken > 0 {
                return Err(Error::ToolsetExists);
            }
            let stamp = next_stamp(now, current.updated_at)?;
            conn.prepare_cached("UPDATE toolsets SET name = ?2, updated_at = ?3 WHERE name = ?1")?
                .execute(params![old, new, stamp])?;
            require(conn, &new)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::SCHEMA_VERSION;
    use crate::migrate::{MIGRATIONS, migrate};
    use crate::sessions::tests::{at, store};

    fn ms(millis: i64) -> Timestamp {
        Timestamp::from_millisecond(millis).unwrap()
    }

    async fn names(store: &Store) -> Vec<String> {
        store
            .toolsets()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect()
    }

    async fn spec_of(store: &Store, name: &str) -> String {
        store
            .toolsets()
            .await
            .unwrap()
            .into_iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .unwrap()
            .spec
    }

    fn table_exists(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'toolsets'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    }

    #[tokio::test]
    async fn a_fresh_database_has_the_table() {
        let (_dir, store) = store().await;
        let (exists, version) = store
            .read(|conn| {
                Ok((
                    table_exists(conn),
                    conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
                ))
            })
            .await
            .unwrap();
        assert!(exists);
        assert_eq!(version, SCHEMA_VERSION);
        assert!(store.toolsets().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_existing_database_gains_the_table_and_keeps_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("giles.db");
        let before = MIGRATIONS
            .iter()
            .position(|m| m.name == "0004_toolsets")
            .unwrap();
        {
            let mut conn = Connection::open(&path).unwrap();
            migrate(&mut conn, &MIGRATIONS[..before], None).unwrap();
            assert!(!table_exists(&conn));
            conn.execute(
                "INSERT INTO routes (position, agent) VALUES (0, 'jeeves')",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).await.unwrap();
        let (exists, version) = store
            .read(|conn| {
                Ok((
                    table_exists(conn),
                    conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
                ))
            })
            .await
            .unwrap();
        assert!(exists);
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(store.routes().await.unwrap().len(), 1);
        assert!(store.toolsets().await.unwrap().is_empty());
        store.toolset_insert("files", "{}", at(0)).await.unwrap();
        assert_eq!(names(&store).await, ["files"]);
    }

    #[tokio::test]
    async fn insert_stores_the_spec_as_written() {
        let (_dir, store) = store().await;
        let spec = r#"{ "transport": "stdio",  "command":"x" }"#;
        let inserted = store.toolset_insert("Files", spec, at(0)).await.unwrap();
        assert_eq!(inserted.name, "Files");
        assert_eq!(inserted.spec, spec);
        assert_eq!(inserted.created_at, inserted.updated_at);
        assert_eq!(store.toolsets().await.unwrap(), [inserted]);
    }

    #[tokio::test]
    async fn the_list_is_ordered_by_lowercased_name_then_name() {
        let (_dir, store) = store().await;
        for name in ["beta", "Zed", "alpha", "Beta2", "Alpha-b"] {
            store.toolset_insert(name, "{}", at(0)).await.unwrap();
        }
        assert_eq!(
            names(&store).await,
            ["alpha", "Alpha-b", "beta", "Beta2", "Zed"]
        );
    }

    #[tokio::test]
    async fn a_duplicate_name_in_any_case_is_refused() {
        let (_dir, store) = store().await;
        store
            .toolset_insert("Files", r#"{"a":1}"#, at(0))
            .await
            .unwrap();
        for name in ["Files", "files", "FILES"] {
            let err = store.toolset_insert(name, "{}", at(0)).await.unwrap_err();
            assert!(matches!(err, Error::ToolsetExists), "{name}: {err:?}");
        }
        assert_eq!(names(&store).await, ["Files"]);
        assert_eq!(spec_of(&store, "files").await, r#"{"a":1}"#);
    }

    #[tokio::test]
    async fn update_finds_the_toolset_in_any_case_and_bumps_updated_at() {
        let (_dir, store) = store().await;
        let first = store.toolset_insert("Files", "{}", at(0)).await.unwrap();
        let updated = store
            .toolset_update("fIlEs", r#"{"b":2}"#, None, at(0))
            .await
            .unwrap();
        assert_eq!(updated.name, "Files");
        assert_eq!(updated.spec, r#"{"b":2}"#);
        assert_eq!(updated.created_at, first.created_at);
        assert!(updated.updated_at > first.updated_at);
        assert_eq!(store.toolsets().await.unwrap(), [updated]);
    }

    #[tokio::test]
    async fn update_with_the_loaded_updated_at_succeeds_and_a_stale_one_changes_nothing() {
        let (_dir, store) = store().await;
        let first = store
            .toolset_insert("files", r#"{"v":1}"#, at(0))
            .await
            .unwrap();

        let second = store
            .toolset_update("files", r#"{"v":2}"#, Some(first.updated_at), at(0))
            .await
            .unwrap();
        assert!(second.updated_at > first.updated_at);

        // Back-to-back updates stay distinguishable, even inside one millisecond.
        let third = store
            .toolset_update("files", r#"{"v":3}"#, Some(second.updated_at), at(0))
            .await
            .unwrap();
        assert!(third.updated_at > second.updated_at);

        for stale in [first.updated_at, second.updated_at, at(0)] {
            let err = store
                .toolset_update("files", r#"{"v":4}"#, Some(stale), at(0))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::StaleToolset), "{err:?}");
        }
        assert_eq!(store.toolsets().await.unwrap(), [third]);
    }

    #[tokio::test]
    async fn the_stamp_is_now_but_always_past_the_stored_value() {
        let (_dir, store) = store().await;
        let first = store.toolset_insert("files", "{}", ms(5)).await.unwrap();
        assert_eq!((first.created_at, first.updated_at), (ms(5), ms(5)));

        // The same millisecond: one past it.
        let same = store
            .toolset_update("files", "{}", None, ms(5))
            .await
            .unwrap();
        assert_eq!(same.updated_at, ms(6));
        // The clock stepped back: still forward.
        let back = store
            .toolset_update("files", "{}", None, ms(3))
            .await
            .unwrap();
        assert_eq!(back.updated_at, ms(7));
        // A rename is stamped the same way.
        let renamed = store.toolset_rename("files", "docs", ms(7)).await.unwrap();
        assert_eq!(renamed.updated_at, ms(8));
        // A clock ahead of the stored value is used as is.
        let ahead = store
            .toolset_update("docs", "{}", None, ms(100))
            .await
            .unwrap();
        assert_eq!(ahead.updated_at, ms(100));
        assert_eq!(ahead.created_at, ms(5));
    }

    #[tokio::test]
    async fn update_without_an_expectation_always_applies() {
        let (_dir, store) = store().await;
        store
            .toolset_insert("files", r#"{"v":1}"#, at(0))
            .await
            .unwrap();
        store
            .toolset_update("files", r#"{"v":2}"#, None, at(0))
            .await
            .unwrap();
        store
            .toolset_update("files", r#"{"v":3}"#, None, at(0))
            .await
            .unwrap();
        assert_eq!(spec_of(&store, "files").await, r#"{"v":3}"#);
    }

    #[tokio::test]
    async fn remove_ignores_case_and_reports_whether_it_removed() {
        let (_dir, store) = store().await;
        store.toolset_insert("Files", "{}", at(0)).await.unwrap();
        store.toolset_insert("other", "{}", at(0)).await.unwrap();
        assert!(store.toolset_remove("fILES").await.unwrap());
        assert_eq!(names(&store).await, ["other"]);
        assert!(!store.toolset_remove("Files").await.unwrap());
        assert!(!store.toolset_remove("never").await.unwrap());
        assert_eq!(names(&store).await, ["other"]);

        // The name is free again.
        store.toolset_insert("files", "{}", at(0)).await.unwrap();
        assert_eq!(names(&store).await, ["files", "other"]);
    }

    #[tokio::test]
    async fn rename_moves_the_row_and_keeps_created_at() {
        let (_dir, store) = store().await;
        let first = store
            .toolset_insert("Files", r#"{"v":1}"#, at(0))
            .await
            .unwrap();
        let renamed = store.toolset_rename("files", "Docs", at(0)).await.unwrap();
        assert_eq!(renamed.name, "Docs");
        assert_eq!(renamed.spec, first.spec);
        assert_eq!(renamed.created_at, first.created_at);
        assert!(renamed.updated_at > first.updated_at);
        assert_eq!(store.toolsets().await.unwrap(), [renamed]);

        let err = store
            .toolset_update("Files", "{}", None, at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSuchToolset), "{err:?}");
    }

    #[tokio::test]
    async fn rename_onto_another_toolset_is_refused_in_any_case() {
        let (_dir, store) = store().await;
        store
            .toolset_insert("a", r#"{"n":"a"}"#, at(0))
            .await
            .unwrap();
        store
            .toolset_insert("B", r#"{"n":"b"}"#, at(0))
            .await
            .unwrap();
        let before = store.toolsets().await.unwrap();
        for new in ["B", "b"] {
            let err = store.toolset_rename("a", new, at(0)).await.unwrap_err();
            assert!(matches!(err, Error::ToolsetExists), "{new}: {err:?}");
        }
        assert_eq!(store.toolsets().await.unwrap(), before);
    }

    #[tokio::test]
    async fn a_case_only_rename_is_allowed() {
        let (_dir, store) = store().await;
        let first = store.toolset_insert("files", "{}", at(0)).await.unwrap();
        let renamed = store.toolset_rename("files", "Files", at(0)).await.unwrap();
        assert_eq!(renamed.name, "Files");
        assert_eq!(renamed.created_at, first.created_at);
        assert!(renamed.updated_at > first.updated_at);
        // Renaming to the identical name is the same row, not a clash.
        let same = store.toolset_rename("Files", "Files", at(0)).await.unwrap();
        assert_eq!(same.name, "Files");
        assert_eq!(names(&store).await, ["Files"]);
    }

    #[tokio::test]
    async fn rename_looks_the_old_name_up_in_any_case() {
        let (_dir, store) = store().await;
        store.toolset_insert("Files", "{}", at(0)).await.unwrap();
        let renamed = store.toolset_rename("FILES", "docs", at(0)).await.unwrap();
        assert_eq!(renamed.name, "docs");
        assert_eq!(names(&store).await, ["docs"]);
    }

    #[tokio::test]
    async fn a_missing_toolset_is_reported() {
        let (_dir, store) = store().await;
        store.toolset_insert("exists", "{}", at(0)).await.unwrap();
        let err = store
            .toolset_update("nope", "{}", None, at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSuchToolset), "{err:?}");
        // Absent wins over stale and over a taken new name.
        let err = store
            .toolset_update("nope", "{}", Some(at(0)), at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSuchToolset), "{err:?}");
        let err = store
            .toolset_rename("nope", "exists", at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSuchToolset), "{err:?}");
        let err = store
            .toolset_rename("nope", "fresh", at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSuchToolset), "{err:?}");
        assert!(!store.toolset_remove("nope").await.unwrap());
        assert_eq!(names(&store).await, ["exists"]);
    }

    #[tokio::test]
    async fn the_schema_refuses_an_empty_name_and_a_case_variant() {
        let (_dir, store) = store().await;
        store.toolset_insert("files", "{}", at(0)).await.unwrap();
        for sql in [
            "INSERT INTO toolsets (name, spec, created_at, updated_at) VALUES ('', '{}', 0, 0)",
            "INSERT INTO toolsets (name, spec, created_at, updated_at) VALUES ('FILES', '{}', 0, 0)",
        ] {
            let err = store
                .write(move |conn| Ok(conn.execute(sql, [])?))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Database(_)), "{sql}: {err}");
        }
    }
}
