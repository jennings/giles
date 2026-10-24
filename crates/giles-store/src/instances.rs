//! Instance names in the database (PLAN 7.2): which names the rows use, and moving them all to a
//! new name for `giles instance rename`.
//!
//! An instance name appears in `sessions.channel`, `inbox.instance`, `outbox.instance`,
//! `usage.provider`, `routes.channel`, and in the `provider` field of each assistant message's
//! `meta` JSON. No other column or JSON payload holds one (Docs/technical/instance-rename.md has
//! the audit).
//!
//! Routes name a channel instance but aren't data: [`Store::instance_rows`] doesn't count them, so
//! the startup check for an unused old name ignores them (a route to a removed instance is kept on
//! purpose, and must not make adding an instance look like a rename). A rename still moves them
//! ([`RenamedRows::routes`], [`Store::instance_routes`]).

use std::collections::BTreeMap;

use rusqlite::{Connection, params};

use crate::{BoxError, Error, Result, Store, query_all};

/// Rows that use an instance name, for [`Store::instance_rows`]. Messages are not counted: every
/// assistant message with a provider has a `usage` row, so they add no information.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InstanceRows {
    /// Sessions of the channel instance (`sessions.channel`).
    pub sessions: u64,
    /// Inbound events of the channel instance (`inbox.instance`).
    pub inbox: u64,
    /// Proactive sends through the channel instance (`outbox.instance`).
    pub outbox: u64,
    /// Provider requests to the provider instance (`usage.provider`).
    pub usage: u64,
}

impl InstanceRows {
    /// Whether any row uses the name as a channel instance.
    pub fn is_channel(&self) -> bool {
        self.sessions + self.inbox + self.outbox > 0
    }

    /// Whether any row uses the name as a provider instance.
    pub fn is_provider(&self) -> bool {
        self.usage > 0
    }
}

/// What [`Store::rename_instance`] moved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenamedRows {
    /// `sessions` rows.
    pub sessions: u64,
    /// `inbox` rows.
    pub inbox: u64,
    /// `outbox` rows.
    pub outbox: u64,
    /// `usage` rows.
    pub usage: u64,
    /// Assistant messages whose `meta` named the provider instance.
    pub messages: u64,
    /// `routes` rows whose channel was the instance. Never counted in the conflict check: a route
    /// that already names the new name is a name, not data to merge.
    pub routes: u64,
}

impl RenamedRows {
    /// The total number of rows moved.
    pub fn total(&self) -> u64 {
        self.sessions + self.inbox + self.outbox + self.usage + self.messages + self.routes
    }
}

/// How a row's count is read: the table, the column holding the name.
const NAME_COLUMNS: [(&str, &str); 4] = [
    ("sessions", "channel"),
    ("inbox", "instance"),
    ("outbox", "instance"),
    ("usage", "provider"),
];

fn count(conn: &Connection, sql: &str, name: &str) -> Result<u64> {
    let n: i64 = conn.query_row(sql, [name], |row| row.get(0))?;
    u64::try_from(n).map_err(|_| Error::corrupt("a negative row count"))
}

/// The number of rows that use `name`, by table. Messages are counted by their `meta`.
fn rows_using(conn: &Connection, name: &str) -> Result<RenamedRows> {
    let [sessions, inbox, outbox, usage] = NAME_COLUMNS.map(|(table, column)| {
        count(
            conn,
            &format!("SELECT count(*) FROM {table} WHERE {column} = ?1"),
            name,
        )
    });
    Ok(RenamedRows {
        sessions: sessions?,
        inbox: inbox?,
        outbox: outbox?,
        usage: usage?,
        messages: count(
            conn,
            "SELECT count(*) FROM messages WHERE meta ->> '$.provider' = ?1",
            name,
        )?,
        routes: 0,
    })
}

impl Store {
    /// Every name the rows use, with how many rows use it, sorted by name. Reads one snapshot.
    pub async fn instance_rows(&self) -> Result<BTreeMap<String, InstanceRows>> {
        self.read(|conn| {
            let mut names: BTreeMap<String, InstanceRows> = BTreeMap::new();
            for (i, (table, column)) in NAME_COLUMNS.into_iter().enumerate() {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {column}, count(*) FROM {table} WHERE {column} IS NOT NULL
                     GROUP BY {column}"
                ))?;
                let rows = query_all(&mut stmt, [], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                for (name, n) in rows {
                    let n = u64::try_from(n).map_err(|_| Error::corrupt("a negative count"))?;
                    let entry = names.entry(name).or_default();
                    match i {
                        0 => entry.sessions = n,
                        1 => entry.inbox = n,
                        2 => entry.outbox = n,
                        _ => entry.usage = n,
                    }
                }
            }
            Ok(names)
        })
        .await
    }

    /// How many routes name each channel instance, by name, sorted. Routes with no channel (any)
    /// aren't counted. This is separate from [`instance_rows`](Self::instance_rows) on purpose.
    pub async fn instance_routes(&self) -> Result<BTreeMap<String, u64>> {
        self.read(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT channel, count(*) FROM routes WHERE channel IS NOT NULL GROUP BY channel",
            )?;
            query_all(&mut stmt, [], |row| {
                let n: i64 = row.get(1)?;
                Ok((
                    row.get::<_, String>(0)?,
                    u64::try_from(n).map_err(|_| Error::corrupt("a negative count"))?,
                ))
            })
            .map(|rows| rows.into_iter().collect())
        })
        .await
    }

    /// Renames the instance `old` to `new` everywhere the database names one, in one
    /// transaction, and runs `hook` inside it before committing. If `hook` fails, or the commit
    /// does, no row changes.
    ///
    /// `hook` is where the caller does the work that must succeed together with the rows (the
    /// files and directories); an `Err` from it comes back as [`Error::Hook`] with the original
    /// error inside. The caller undoes its own work on any error: the hook may already have run
    /// when the commit fails.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if the names are equal or empty; [`Error::Conflict`] if rows
    /// already use `new`: renaming never merges two instances' data.
    pub async fn rename_instance(
        &self,
        old: &str,
        new: &str,
        hook: impl FnOnce(&RenamedRows) -> std::result::Result<(), BoxError> + Send + 'static,
    ) -> Result<RenamedRows> {
        if old.is_empty() || new.is_empty() || old == new {
            return Err(Error::invalid(
                "an instance rename needs two different, non-empty names",
            ));
        }
        let (old, new) = (old.to_owned(), new.to_owned());
        self.write(move |conn| {
            let existing = rows_using(conn, &new)?;
            if existing.total() > 0 {
                return Err(Error::conflict(format!(
                    "{new:?} already has data in the database ({} sessions, {} inbox rows, {} \
                     outbox rows, {} usage rows, {} messages); renaming doesn't merge instances",
                    existing.sessions,
                    existing.inbox,
                    existing.outbox,
                    existing.usage,
                    existing.messages,
                )));
            }
            let mut moved = RenamedRows::default();
            let update = |table: &str, column: &str| -> Result<u64> {
                let n = conn.execute(
                    &format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1"),
                    params![old, new],
                )?;
                Ok(n as u64)
            };
            moved.sessions = update("sessions", "channel")?;
            moved.inbox = update("inbox", "instance")?;
            moved.outbox = update("outbox", "instance")?;
            moved.usage = update("usage", "provider")?;
            moved.routes = update("routes", "channel")?;
            // `messages` has no trigger on `meta`, so the search index is untouched.
            moved.messages = conn.execute(
                "UPDATE messages SET meta = json_set(meta, '$.provider', ?2)
                 WHERE meta ->> '$.provider' = ?1",
                params![old, new],
            )? as u64;
            hook(&moved).map_err(Error::Hook)?;
            Ok(moved)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use giles_types::{AgentId, ConversationId, EventId, NativeId, UserId};
    use serde_json::json;

    use super::*;
    use crate::sessions::tests::{at, store};
    use crate::{InboxState, NewInboxEvent, NewOutbox, SessionKey, SessionKind, UsageRecord};

    async fn session(store: &Store, channel: &str, conversation: &str) -> crate::SessionId {
        let key = SessionKey {
            agent: AgentId::new("jeeves").unwrap(),
            channel: channel.into(),
            conversation: ConversationId::new(conversation).unwrap(),
        };
        let owner = UserId::new("owner").unwrap();
        store
            .open_session(key, SessionKind::Direct, Some(owner), at(0))
            .await
            .unwrap()
            .0
            .id
    }

    async fn inbox(store: &Store, instance: &str, event: &str) {
        store
            .inbox_insert(NewInboxEvent {
                instance: instance.into(),
                event_id: EventId::new(event).unwrap(),
                session: None,
                user: None,
                payload: json!({}),
                state: InboxState::Done,
                received_at: at(1),
            })
            .await
            .unwrap()
            .unwrap();
    }

    async fn outbox(store: &Store, instance: &str) {
        store
            .outbox_enqueue(NewOutbox {
                instance: instance.into(),
                recipient: NativeId::new("irc:libera:account:me").unwrap(),
                kind: "notice".into(),
                user: None,
                payload: json!({"markdown": "hi"}),
                created_at: at(1),
                expires_at: at(100),
            })
            .await
            .unwrap();
    }

    async fn usage(store: &Store, provider: &str) {
        store
            .record_usage(UsageRecord {
                at: at(2),
                provider: provider.into(),
                model: "m".into(),
                agent: AgentId::new("jeeves").unwrap(),
                user: None,
                session: None,
                usage: Default::default(),
                fallback: false,
            })
            .await
            .unwrap();
    }

    /// An assistant message in `session` that names `provider` in its `meta`, and a user message
    /// that has no provider.
    async fn messages(store: &Store, session: crate::SessionId, provider: &str) {
        let provider = provider.to_owned();
        store
            .write(move |conn| {
                for (seq, role, meta) in [
                    (1, "user", json!({"at": "2026-01-01T00:00:00Z"})),
                    (
                        2,
                        "assistant",
                        json!({"at": "2026-01-01T00:00:01Z", "provider": provider, "model": "m"}),
                    ),
                ] {
                    conn.execute(
                        "INSERT INTO messages (session_id, seq, role, content, meta, created_at)
                         VALUES (?1, ?2, ?3, '[]', ?4, 0)",
                        params![session, seq, role, meta.to_string()],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn provider_of_messages(store: &Store) -> Vec<Option<String>> {
        store
            .read(|conn| {
                let mut stmt =
                    conn.prepare("SELECT meta ->> '$.provider' FROM messages ORDER BY id")?;
                query_all(&mut stmt, [], |row| Ok(row.get(0)?))
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn instance_rows_counts_every_table_by_name() {
        let (_dir, store) = store().await;
        assert!(store.instance_rows().await.unwrap().is_empty());
        session(&store, "libera", "c1").await;
        session(&store, "libera", "c2").await;
        inbox(&store, "libera", "e1").await;
        outbox(&store, "oftc").await;
        usage(&store, "anthropic").await;
        usage(&store, "anthropic").await;

        let rows = store.instance_rows().await.unwrap();
        let names: Vec<_> = rows.keys().map(String::as_str).collect();
        assert_eq!(names, ["anthropic", "libera", "oftc"]);
        assert_eq!(
            rows["libera"],
            InstanceRows {
                sessions: 2,
                inbox: 1,
                ..Default::default()
            }
        );
        assert!(rows["libera"].is_channel() && !rows["libera"].is_provider());
        assert!(rows["oftc"].is_channel());
        assert!(rows["anthropic"].is_provider() && !rows["anthropic"].is_channel());
    }

    #[tokio::test]
    async fn rename_moves_every_row_and_leaves_other_instances_alone() {
        let (_dir, store) = store().await;
        let libera = session(&store, "libera", "c1").await;
        session(&store, "oftc", "c1").await;
        inbox(&store, "libera", "e1").await;
        inbox(&store, "oftc", "e1").await;
        outbox(&store, "libera").await;
        usage(&store, "libera").await;
        usage(&store, "other").await;
        messages(&store, libera, "libera").await;

        let moved = store
            .rename_instance("libera", "libera-chat", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            moved,
            RenamedRows {
                sessions: 1,
                inbox: 1,
                outbox: 1,
                usage: 1,
                messages: 1,
                routes: 0,
            }
        );

        let rows = store.instance_rows().await.unwrap();
        assert!(!rows.contains_key("libera"));
        assert_eq!(rows["libera-chat"].sessions, 1);
        assert_eq!(rows["libera-chat"].usage, 1);
        assert_eq!(rows["oftc"].sessions, 1);
        assert_eq!(rows["oftc"].inbox, 1);
        assert_eq!(rows["other"].usage, 1);
        assert_eq!(
            provider_of_messages(&store).await,
            [None, Some("libera-chat".to_owned())]
        );
        // The renamed session is still the open session of its key, so a new message on the
        // renamed instance continues the old conversation.
        let key = SessionKey {
            agent: AgentId::new("jeeves").unwrap(),
            channel: "libera-chat".into(),
            conversation: ConversationId::new("c1").unwrap(),
        };
        let found = store.find_open_session(key).await.unwrap().unwrap();
        assert_eq!(found.id, libera);
    }

    #[tokio::test]
    async fn rename_refuses_to_merge_into_a_name_with_data() {
        for existing in ["inbox", "outbox", "usage", "messages"] {
            let (_dir, store) = store_with(existing).await;
            let err = store
                .rename_instance("libera", "oftc", |_| Ok(()))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{existing}: {err:?}");
            assert!(err.to_string().contains("doesn't merge"), "{err}");
            assert!(store.instance_rows().await.unwrap().contains_key("libera"));
        }
    }

    /// A database where `libera` has a session and `oftc` has data in `table` only.
    async fn store_with(table: &str) -> (tempfile::TempDir, Store) {
        let (dir, store) = store().await;
        let s = session(&store, "libera", "c1").await;
        match table {
            "inbox" => inbox(&store, "oftc", "e1").await,
            "outbox" => outbox(&store, "oftc").await,
            "usage" => usage(&store, "oftc").await,
            _ => messages(&store, s, "oftc").await,
        }
        (dir, store)
    }

    #[tokio::test]
    async fn rename_rejects_degenerate_names() {
        let (_dir, store) = store().await;
        for (old, new) in [("a", "a"), ("", "b"), ("a", "")] {
            let err = store
                .rename_instance(old, new, |_| Ok(()))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
        }
    }

    #[tokio::test]
    async fn a_failing_hook_rolls_every_row_back() {
        let (_dir, store) = store().await;
        let s = session(&store, "libera", "c1").await;
        inbox(&store, "libera", "e1").await;
        usage(&store, "libera").await;
        messages(&store, s, "libera").await;
        let before = store.instance_rows().await.unwrap();

        let err = store
            .rename_instance("libera", "oftc", |moved| {
                assert_eq!(moved.total(), 4, "the hook runs after the updates");
                Err("disk on fire".into())
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Hook(e) if e.to_string() == "disk on fire"),
            "{err:?}"
        );

        assert_eq!(store.instance_rows().await.unwrap(), before);
        assert_eq!(
            provider_of_messages(&store).await,
            [None, Some("libera".to_owned())]
        );
    }

    async fn route(store: &Store, channel: Option<&str>, agent: &str) {
        store
            .route_insert(
                usize::MAX,
                crate::NewRoute {
                    channel: channel.map(str::to_owned),
                    kind: None,
                    conversation: None,
                    agent: AgentId::new(agent).unwrap(),
                },
            )
            .await
            .unwrap();
    }

    async fn route_channels(store: &Store) -> Vec<Option<String>> {
        store
            .routes()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.route.channel)
            .collect()
    }

    #[tokio::test]
    async fn routes_are_counted_apart_from_the_rows() {
        let (_dir, store) = store().await;
        route(&store, Some("libera"), "a").await;
        route(&store, Some("libera"), "b").await;
        route(&store, Some("oftc"), "a").await;
        route(&store, None, "c").await;
        assert_eq!(
            store.instance_routes().await.unwrap(),
            BTreeMap::from([("libera".to_owned(), 2), ("oftc".to_owned(), 1)])
        );
        // Not data: they don't make a name look used to the startup check.
        assert!(store.instance_rows().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_moves_the_routes_with_the_rows() {
        let (_dir, store) = store().await;
        session(&store, "libera", "c1").await;
        route(&store, Some("libera"), "a").await;
        route(&store, Some("oftc"), "b").await;
        route(&store, None, "c").await;
        route(&store, Some("libera"), "d").await;

        let moved = store
            .rename_instance("libera", "libera-chat", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!((moved.sessions, moved.routes), (1, 2));
        // Order and the other routes are untouched.
        assert_eq!(
            route_channels(&store).await,
            [
                Some("libera-chat".to_owned()),
                Some("oftc".to_owned()),
                None,
                Some("libera-chat".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn a_route_to_a_name_that_only_has_routes_can_be_renamed() {
        let (_dir, store) = store().await;
        route(&store, Some("libera"), "a").await;
        let moved = store
            .rename_instance("libera", "oftc", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(moved.routes, 1);
        assert_eq!(route_channels(&store).await, [Some("oftc".to_owned())]);
    }

    #[tokio::test]
    async fn routes_to_the_new_name_are_not_a_merge_conflict() {
        let (_dir, store) = store().await;
        session(&store, "libera", "c1").await;
        route(&store, Some("libera"), "a").await;
        route(&store, Some("oftc"), "b").await;
        store
            .rename_instance("libera", "oftc", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            route_channels(&store).await,
            [Some("oftc".to_owned()), Some("oftc".to_owned())]
        );
    }

    #[tokio::test]
    async fn a_failing_hook_leaves_the_routes_alone() {
        let (_dir, store) = store().await;
        session(&store, "libera", "c1").await;
        route(&store, Some("libera"), "a").await;
        let before = store.routes().await.unwrap();
        let err = store
            .rename_instance("libera", "oftc", |moved| {
                assert_eq!(moved.routes, 1, "the hook runs after the updates");
                Err("disk on fire".into())
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Hook(_)), "{err:?}");
        assert_eq!(store.routes().await.unwrap(), before);
    }
}
