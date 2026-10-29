//! Instance names in the database (PLAN 7.2): which names the rows use, and moving them all to a
//! new name for `giles instance rename`.
//!
//! The kinds of instance have separate namespaces, so every operation here takes the [`Kind`] and
//! touches only the places that kind is named in. A provider and a channel that share a name
//! don't share rows.
//!
//! - A channel is named in `sessions.channel`, `inbox.instance`, `outbox.instance`, and
//!   `routes.channel`.
//! - A provider is named in `usage.provider` and in the `provider` field of each assistant
//!   message's `meta` JSON.
//! - A toolset is named in none of them. A stored toolset is a row of its own
//!   ([`Store::toolset_rename`]), and the agents' files that mention it aren't the database's.
//!
//! No other column or JSON payload holds an instance name (Docs/technical/instance-rename.md has
//! the audit).
//!
//! Routes name a channel instance but aren't data: [`Store::instance_rows`] doesn't count them, so
//! a route to a removed instance (kept on purpose) doesn't count as data to merge in a rename. A
//! rename still moves them ([`RenamedRows::routes`], [`Store::instance_routes`]).

use std::collections::BTreeMap;

use giles_types::Kind;
use rusqlite::{Connection, params};

use crate::{BoxError, Error, Result, Store, query_all};

/// Rows that use an instance name, for [`Store::instance_rows`]. Only the columns of the kind
/// asked for are filled in; the others are zero. Messages are not counted: every assistant message
/// with a provider has a `usage` row, so they add no information.
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

impl From<InstanceRows> for RenamedRows {
    fn from(rows: InstanceRows) -> Self {
        Self {
            sessions: rows.sessions,
            inbox: rows.inbox,
            outbox: rows.outbox,
            usage: rows.usage,
            ..Self::default()
        }
    }
}

/// A column that holds an instance name, and the count it feeds.
struct NameColumn {
    table: &'static str,
    column: &'static str,
    field: fn(&mut InstanceRows) -> &mut u64,
}

const CHANNEL_COLUMNS: [NameColumn; 3] = [
    NameColumn {
        table: "sessions",
        column: "channel",
        field: |rows| &mut rows.sessions,
    },
    NameColumn {
        table: "inbox",
        column: "instance",
        field: |rows| &mut rows.inbox,
    },
    NameColumn {
        table: "outbox",
        column: "instance",
        field: |rows| &mut rows.outbox,
    },
];

const PROVIDER_COLUMNS: [NameColumn; 1] = [NameColumn {
    table: "usage",
    column: "provider",
    field: |rows| &mut rows.usage,
}];

/// The columns that count as data for `kind`: where a rename of it looks for rows to refuse a
/// merge, and what [`Store::instance_rows`] lists. Routes and messages aren't among them.
fn name_columns(kind: Kind) -> &'static [NameColumn] {
    match kind {
        Kind::Channel => &CHANNEL_COLUMNS,
        Kind::Provider => &PROVIDER_COLUMNS,
        Kind::Toolset => &[],
    }
}

fn count(conn: &Connection, sql: &str, name: &str) -> Result<u64> {
    let n: i64 = conn.query_row(sql, [name], |row| row.get(0))?;
    u64::try_from(n).map_err(|_| Error::corrupt("a negative row count"))
}

/// The number of rows that use `name` as an instance of `kind`, by table. Provider messages are
/// counted by their `meta`.
fn rows_using(conn: &Connection, kind: Kind, name: &str) -> Result<RenamedRows> {
    let mut counts = InstanceRows::default();
    for NameColumn {
        table,
        column,
        field,
    } in name_columns(kind)
    {
        *field(&mut counts) = count(
            conn,
            &format!("SELECT count(*) FROM {table} WHERE {column} = ?1"),
            name,
        )?;
    }
    let mut rows = RenamedRows::from(counts);
    if kind == Kind::Provider {
        rows.messages = count(
            conn,
            "SELECT count(*) FROM messages WHERE meta ->> '$.provider' = ?1",
            name,
        )?;
    }
    Ok(rows)
}

impl Store {
    /// Every name the rows use for an instance of `kind`, with how many rows use it, sorted by
    /// name. Reads one snapshot. A toolset has no rows, so the result is empty.
    pub async fn instance_rows(&self, kind: Kind) -> Result<BTreeMap<String, InstanceRows>> {
        self.read(move |conn| {
            let mut names: BTreeMap<String, InstanceRows> = BTreeMap::new();
            for NameColumn {
                table,
                column,
                field,
            } in name_columns(kind)
            {
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
                    *field(entry) = n;
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

    /// Renames the instance `old` of `kind` to `new` everywhere the database names an instance of
    /// that kind, in one transaction, and runs `hook` inside it before committing. Rows of another
    /// kind with the same name stay as they are. If `hook` fails, or the commit does, no row
    /// changes.
    ///
    /// A toolset has no rows of this kind, so renaming one changes none and only runs `hook`.
    ///
    /// `hook` is where the caller does the work that must succeed together with the rows (the
    /// files and directories); an `Err` from it comes back as [`Error::Hook`] with the original
    /// error inside. The caller undoes its own work on any error: the hook may already have run
    /// when the commit fails.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if the names are equal or empty; [`Error::Conflict`] if rows of
    /// `kind` already use `new`: renaming never merges two instances' data.
    pub async fn rename_instance(
        &self,
        kind: Kind,
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
            let existing = rows_using(conn, kind, &new)?;
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
            let update = |table: &str, column: &str| -> Result<u64> {
                let n = conn.execute(
                    &format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1"),
                    params![old, new],
                )?;
                Ok(n as u64)
            };
            let mut counts = InstanceRows::default();
            for NameColumn {
                table,
                column,
                field,
            } in name_columns(kind)
            {
                *field(&mut counts) = update(table, column)?;
            }
            let mut moved = RenamedRows::from(counts);
            match kind {
                Kind::Channel => moved.routes = update("routes", "channel")?,
                Kind::Provider => {
                    // `messages` has no trigger on `meta`, so the search index is untouched.
                    moved.messages = conn.execute(
                        "UPDATE messages SET meta = json_set(meta, '$.provider', ?2)
                         WHERE meta ->> '$.provider' = ?1",
                        params![old, new],
                    )? as u64;
                }
                Kind::Toolset => {}
            }
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
    async fn instance_rows_counts_each_kinds_tables_by_name() {
        let (_dir, store) = store().await;
        for kind in Kind::ALL {
            assert!(store.instance_rows(kind).await.unwrap().is_empty());
        }
        session(&store, "libera", "c1").await;
        session(&store, "libera", "c2").await;
        inbox(&store, "libera", "e1").await;
        outbox(&store, "oftc").await;
        usage(&store, "anthropic").await;
        usage(&store, "anthropic").await;

        let channels = store.instance_rows(Kind::Channel).await.unwrap();
        let names: Vec<_> = channels.keys().map(String::as_str).collect();
        assert_eq!(names, ["libera", "oftc"]);
        assert_eq!(
            channels["libera"],
            InstanceRows {
                sessions: 2,
                inbox: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            store.instance_rows(Kind::Provider).await.unwrap(),
            BTreeMap::from([(
                "anthropic".to_owned(),
                InstanceRows {
                    usage: 2,
                    ..Default::default()
                }
            )])
        );
        // A toolset is named in no table.
        assert!(store.instance_rows(Kind::Toolset).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn instance_rows_of_one_kind_ignore_the_same_name_of_another() {
        let (_dir, store) = store().await;
        session(&store, "aperture", "c1").await;
        usage(&store, "aperture").await;
        assert_eq!(
            store.instance_rows(Kind::Channel).await.unwrap()["aperture"],
            InstanceRows {
                sessions: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            store.instance_rows(Kind::Provider).await.unwrap()["aperture"],
            InstanceRows {
                usage: 1,
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn rename_moves_every_channel_row_and_leaves_other_instances_alone() {
        let (_dir, store) = store().await;
        let libera = session(&store, "libera", "c1").await;
        session(&store, "oftc", "c1").await;
        inbox(&store, "libera", "e1").await;
        inbox(&store, "oftc", "e1").await;
        outbox(&store, "libera").await;
        usage(&store, "other").await;

        let moved = store
            .rename_instance(Kind::Channel, "libera", "libera-chat", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            moved,
            RenamedRows {
                sessions: 1,
                inbox: 1,
                outbox: 1,
                ..Default::default()
            }
        );

        let rows = store.instance_rows(Kind::Channel).await.unwrap();
        assert!(!rows.contains_key("libera"));
        assert_eq!(rows["libera-chat"].sessions, 1);
        assert_eq!(rows["libera-chat"].inbox, 1);
        assert_eq!(rows["libera-chat"].outbox, 1);
        assert_eq!(rows["oftc"].sessions, 1);
        assert_eq!(rows["oftc"].inbox, 1);
        assert_eq!(
            store.instance_rows(Kind::Provider).await.unwrap()["other"].usage,
            1
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
    async fn rename_moves_every_provider_row_and_leaves_other_instances_alone() {
        let (_dir, store) = store().await;
        // The messages live in a session of a channel, which isn't renamed.
        let s = session(&store, "web", "c1").await;
        usage(&store, "ollama").await;
        usage(&store, "other").await;
        messages(&store, s, "ollama").await;

        let moved = store
            .rename_instance(Kind::Provider, "ollama", "local", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            moved,
            RenamedRows {
                usage: 1,
                messages: 1,
                ..Default::default()
            }
        );

        let rows = store.instance_rows(Kind::Provider).await.unwrap();
        let names: Vec<_> = rows.keys().map(String::as_str).collect();
        assert_eq!(names, ["local", "other"]);
        assert_eq!(
            provider_of_messages(&store).await,
            [None, Some("local".to_owned())]
        );
        assert_eq!(store.instance_rows(Kind::Channel).await.unwrap().len(), 1);
    }

    /// One row of every table that names an instance, all under `name`: a channel's session,
    /// inbox event, outbox row, and route, and a provider's usage row and assistant message.
    async fn seed_every_kind(store: &Store, name: &str) {
        let s = session(store, name, "c1").await;
        inbox(store, name, "e1").await;
        outbox(store, name).await;
        route(store, Some(name), "jeeves").await;
        usage(store, name).await;
        messages(store, s, name).await;
    }

    /// What the tables hold for `name`: the channel's rows and routes, the provider's rows, and
    /// the assistant messages naming it.
    async fn held_by(store: &Store, name: &str) -> (InstanceRows, u64, InstanceRows, u64) {
        let channels = store.instance_rows(Kind::Channel).await.unwrap();
        let providers = store.instance_rows(Kind::Provider).await.unwrap();
        let routes = store.instance_routes().await.unwrap();
        let messages = provider_of_messages(store)
            .await
            .into_iter()
            .filter(|p| p.as_deref() == Some(name))
            .count() as u64;
        (
            channels.get(name).copied().unwrap_or_default(),
            routes.get(name).copied().unwrap_or_default(),
            providers.get(name).copied().unwrap_or_default(),
            messages,
        )
    }

    #[tokio::test]
    async fn renaming_a_channel_leaves_a_provider_of_the_same_name_alone() {
        let (_dir, store) = store().await;
        seed_every_kind(&store, "aperture").await;

        let moved = store
            .rename_instance(Kind::Channel, "aperture", "lens", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            moved,
            RenamedRows {
                sessions: 1,
                inbox: 1,
                outbox: 1,
                routes: 1,
                ..Default::default()
            }
        );

        let (channel, routes, provider, messages) = held_by(&store, "aperture").await;
        assert_eq!(channel, InstanceRows::default());
        assert_eq!(routes, 0);
        // The provider keeps its usage and its messages.
        assert_eq!(provider.usage, 1);
        assert_eq!(messages, 1);
        let (channel, routes, provider, messages) = held_by(&store, "lens").await;
        assert_eq!(
            channel,
            InstanceRows {
                sessions: 1,
                inbox: 1,
                outbox: 1,
                ..Default::default()
            }
        );
        assert_eq!(routes, 1);
        assert_eq!(provider, InstanceRows::default());
        assert_eq!(messages, 0);
    }

    #[tokio::test]
    async fn renaming_a_provider_leaves_a_channel_of_the_same_name_alone() {
        let (_dir, store) = store().await;
        seed_every_kind(&store, "aperture").await;

        let moved = store
            .rename_instance(Kind::Provider, "aperture", "lens", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(
            moved,
            RenamedRows {
                usage: 1,
                messages: 1,
                ..Default::default()
            }
        );

        let (channel, routes, provider, messages) = held_by(&store, "aperture").await;
        // The channel keeps its sessions, inbox, outbox, and route.
        assert_eq!(
            channel,
            InstanceRows {
                sessions: 1,
                inbox: 1,
                outbox: 1,
                ..Default::default()
            }
        );
        assert_eq!(routes, 1);
        assert_eq!(provider, InstanceRows::default());
        assert_eq!(messages, 0);
        let (channel, routes, provider, messages) = held_by(&store, "lens").await;
        assert_eq!(channel, InstanceRows::default());
        assert_eq!(routes, 0);
        assert_eq!(provider.usage, 1);
        assert_eq!(messages, 1);
    }

    #[tokio::test]
    async fn renaming_a_toolset_changes_no_row_but_runs_the_hook() {
        let (_dir, store) = store().await;
        seed_every_kind(&store, "aperture").await;
        let before = held_by(&store, "aperture").await;

        let moved = store
            .rename_instance(Kind::Toolset, "aperture", "lens", |moved| {
                assert_eq!(moved.total(), 0);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(moved, RenamedRows::default());
        assert_eq!(held_by(&store, "aperture").await, before);
        assert_eq!(
            held_by(&store, "lens").await,
            (InstanceRows::default(), 0, InstanceRows::default(), 0)
        );

        let err = store
            .rename_instance(Kind::Toolset, "aperture", "lens", |_| {
                Err("disk on fire".into())
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Hook(_)), "{err:?}");
    }

    #[tokio::test]
    async fn another_kinds_data_under_the_new_name_is_not_a_merge() {
        let (_dir, store) = store().await;
        session(&store, "libera", "c1").await;
        usage(&store, "oftc").await;
        let moved = store
            .rename_instance(Kind::Channel, "libera", "oftc", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(moved.sessions, 1);
        // The provider `oftc` is where it was.
        assert_eq!(
            store.instance_rows(Kind::Provider).await.unwrap()["oftc"].usage,
            1
        );

        // And the other way round: a channel's rows under the new name don't block a provider.
        usage(&store, "ollama").await;
        let moved = store
            .rename_instance(Kind::Provider, "ollama", "libera", |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(moved.usage, 1);
        assert_eq!(
            store.instance_rows(Kind::Channel).await.unwrap()["oftc"].sessions,
            1
        );
    }

    #[tokio::test]
    async fn rename_refuses_to_merge_into_a_name_with_data_of_the_same_kind() {
        for (kind, existing) in [
            (Kind::Channel, "session"),
            (Kind::Channel, "inbox"),
            (Kind::Channel, "outbox"),
            (Kind::Provider, "usage"),
            (Kind::Provider, "messages"),
        ] {
            let (_dir, store) = store_with(kind, existing).await;
            let err = store
                .rename_instance(kind, "old", "oftc", |_| Ok(()))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{existing}: {err:?}");
            assert!(err.to_string().contains("doesn't merge"), "{err}");
            assert!(store.instance_rows(kind).await.unwrap().contains_key("old"));
        }
    }

    /// A database where the instance `old` of `kind` has data and `oftc` has data of the same
    /// kind in `table` only.
    async fn store_with(kind: Kind, table: &str) -> (tempfile::TempDir, Store) {
        let (dir, store) = store().await;
        let s = session(&store, "web", "c1").await;
        match kind {
            Kind::Channel => session(&store, "old", "c2").await,
            _ => {
                usage(&store, "old").await;
                s
            }
        };
        match table {
            "session" => {
                session(&store, "oftc", "c3").await;
            }
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
        for kind in Kind::ALL {
            for (old, new) in [("a", "a"), ("", "b"), ("a", "")] {
                let err = store
                    .rename_instance(kind, old, new, |_| Ok(()))
                    .await
                    .unwrap_err();
                assert!(matches!(err, Error::InvalidRequest(_)), "{err:?}");
            }
        }
    }

    #[tokio::test]
    async fn a_failing_hook_rolls_every_row_back() {
        let (_dir, store) = store().await;
        let s = session(&store, "libera", "c1").await;
        inbox(&store, "libera", "e1").await;
        usage(&store, "ollama").await;
        messages(&store, s, "ollama").await;
        let channels = store.instance_rows(Kind::Channel).await.unwrap();
        let providers = store.instance_rows(Kind::Provider).await.unwrap();

        for (kind, old) in [(Kind::Channel, "libera"), (Kind::Provider, "ollama")] {
            let err = store
                .rename_instance(kind, old, "other", |moved| {
                    assert_eq!(moved.total(), 2, "the hook runs after the updates");
                    Err("disk on fire".into())
                })
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Hook(e) if e.to_string() == "disk on fire"),
                "{err:?}"
            );
        }

        assert_eq!(store.instance_rows(Kind::Channel).await.unwrap(), channels);
        assert_eq!(
            store.instance_rows(Kind::Provider).await.unwrap(),
            providers
        );
        assert_eq!(
            provider_of_messages(&store).await,
            [None, Some("ollama".to_owned())]
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
        // Not data: they don't count as rows of the name.
        assert!(store.instance_rows(Kind::Channel).await.unwrap().is_empty());
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
            .rename_instance(Kind::Channel, "libera", "libera-chat", |_| Ok(()))
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
            .rename_instance(Kind::Channel, "libera", "oftc", |_| Ok(()))
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
            .rename_instance(Kind::Channel, "libera", "oftc", |_| Ok(()))
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
            .rename_instance(Kind::Channel, "libera", "oftc", |moved| {
                assert_eq!(moved.routes, 1, "the hook runs after the updates");
                Err("disk on fire".into())
            })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Hook(_)), "{err:?}");
        assert_eq!(store.routes().await.unwrap(), before);
    }
}
