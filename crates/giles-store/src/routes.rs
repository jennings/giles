//! `routes`: the ordered routing rules (PLAN 6.1, TASKS 5.2), and the marker that says the
//! `[[routes]]` of `giles.toml` have been imported.
//!
//! The store keeps the rules as written and checks nothing about them but their shape: whether the
//! channel instance and the agent exist, and whether a rule can ever match, is the core's business
//! (`giles_core::routes`). Order is `position`, kept dense (0, 1, 2, ...) by every method here.
//! Ids are stable and never reused, so a page that names a route can't act on another one after
//! the list changed.

use giles_types::{AgentId, ConversationKind, Timestamp};
use rusqlite::{Connection, Row, params};

use crate::time::to_millis;
use crate::{Error, Result, Store, parse_id, query_all, row_id};

row_id!(
    /// A row in `routes`. Ids are never reused, even after a route is deleted.
    RouteId
);

/// A routing rule: it matches a conversation when every key it sets matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRoute {
    /// The channel instance, or any.
    pub channel: Option<String>,
    /// The conversation kind, or any.
    pub kind: Option<ConversationKind>,
    /// The conversation id, or any.
    pub conversation: Option<String>,
    /// The agent that handles matching conversations.
    pub agent: AgentId,
}

/// A stored routing rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRoute {
    /// The row.
    pub id: RouteId,
    /// The rule.
    pub route: NewRoute,
}

fn kind_text(kind: ConversationKind) -> Result<&'static str> {
    match kind {
        ConversationKind::Direct => Ok("direct"),
        ConversationKind::Group => Ok("group"),
        _ => Err(Error::invalid("a conversation kind routes can't store")),
    }
}

fn from_row(row: &Row<'_>) -> Result<StoredRoute> {
    let kind = match row.get::<_, Option<String>>(3)?.as_deref() {
        None => None,
        Some("direct") => Some(ConversationKind::Direct),
        Some("group") => Some(ConversationKind::Group),
        Some(other) => return Err(Error::corrupt(format!("route kind {other:?}"))),
    };
    Ok(StoredRoute {
        id: row.get(0)?,
        route: NewRoute {
            channel: row.get(2)?,
            kind,
            conversation: row.get(4)?,
            agent: parse_id(row.get(5)?, AgentId::new)?,
        },
    })
}

const SELECT: &str =
    "SELECT id, position, channel, kind, conversation, agent FROM routes ORDER BY position, id";

fn list(conn: &Connection) -> Result<Vec<StoredRoute>> {
    let mut stmt = conn.prepare_cached(SELECT)?;
    query_all(&mut stmt, [], from_row)
}

/// The ids in order.
fn ids(conn: &Connection) -> Result<Vec<RouteId>> {
    let mut stmt = conn.prepare_cached("SELECT id FROM routes ORDER BY position, id")?;
    query_all(&mut stmt, [], |row| Ok(row.get(0)?))
}

/// Writes `order` as the positions of the rows with those ids.
fn renumber(conn: &Connection, order: &[RouteId]) -> Result<()> {
    let mut stmt = conn.prepare_cached("UPDATE routes SET position = ?2 WHERE id = ?1")?;
    for (position, id) in order.iter().enumerate() {
        stmt.execute(params![id, position as i64])?;
    }
    Ok(())
}

fn insert(conn: &Connection, route: &NewRoute) -> Result<RouteId> {
    let kind = route.kind.map(kind_text).transpose()?;
    conn.prepare_cached(
        "INSERT INTO routes (position, channel, kind, conversation, agent)
         VALUES ((SELECT coalesce(max(position) + 1, 0) FROM routes), ?1, ?2, ?3, ?4)",
    )?
    .execute(params![
        route.channel,
        kind,
        route.conversation,
        route.agent.as_str()
    ])?;
    Ok(RouteId::new(conn.last_insert_rowid()))
}

impl Store {
    /// Every route, first match first.
    pub async fn routes(&self) -> Result<Vec<StoredRoute>> {
        self.read(list).await
    }

    /// Adds `route` at index `at` (0 is first; an index past the end appends) and returns its id.
    /// The routes from `at` on move back one place.
    pub async fn route_insert(&self, at: usize, route: NewRoute) -> Result<RouteId> {
        self.write(move |conn| {
            let id = insert(conn, &route)?;
            let mut order = ids(conn)?;
            order.pop();
            order.insert(at.min(order.len()), id);
            renumber(conn, &order)?;
            Ok(id)
        })
        .await
    }

    /// Replaces the rule of route `id`, keeping its place. Returns `false` if there's no such
    /// route.
    pub async fn route_update(&self, id: RouteId, route: NewRoute) -> Result<bool> {
        let kind = route.kind.map(kind_text).transpose()?;
        self.write(move |conn| {
            Ok(conn
                .prepare_cached(
                    "UPDATE routes SET channel = ?2, kind = ?3, conversation = ?4, agent = ?5
                     WHERE id = ?1",
                )?
                .execute(params![
                    id,
                    route.channel,
                    kind,
                    route.conversation,
                    route.agent.as_str()
                ])?
                == 1)
        })
        .await
    }

    /// Deletes route `id`. Returns `false` if there's no such route.
    pub async fn route_remove(&self, id: RouteId) -> Result<bool> {
        self.write(move |conn| {
            let removed = conn
                .prepare_cached("DELETE FROM routes WHERE id = ?1")?
                .execute([id])?
                == 1;
            if removed {
                renumber(conn, &ids(conn)?)?;
            }
            Ok(removed)
        })
        .await
    }

    /// Moves route `id` to index `to` (0 is first; an index past the end means last). Returns
    /// `false` if there's no such route.
    pub async fn route_move(&self, id: RouteId, to: usize) -> Result<bool> {
        self.write(move |conn| {
            let mut order = ids(conn)?;
            let Some(from) = order.iter().position(|&r| r == id) else {
                return Ok(false);
            };
            order.remove(from);
            order.insert(to.min(order.len()), id);
            renumber(conn, &order)?;
            Ok(true)
        })
        .await
    }

    /// Whether the import of the `[[routes]]` of `giles.toml` has run ([`routes_import`](Self::routes_import)).
    pub async fn routes_imported(&self) -> Result<bool> {
        self.read(|conn| {
            let done: i64 =
                conn.query_row("SELECT count(*) FROM routes_imported", [], |r| r.get(0))?;
            Ok(done > 0)
        })
        .await
    }

    /// Imports `routes`, the `[[routes]]` of `giles.toml`, once: if the import has never run, it
    /// stores them in order, unless routes exist already (then they stay), and records that it
    /// ran. Returns how many routes it stored, or `None` if the import had run before, in which
    /// case nothing changes, whatever `routes` holds and even if every route has since been
    /// deleted. Nothing is stored if the import fails.
    pub async fn routes_import(
        &self,
        routes: Vec<NewRoute>,
        now: Timestamp,
    ) -> Result<Option<usize>> {
        self.write(move |conn| {
            let done: i64 =
                conn.query_row("SELECT count(*) FROM routes_imported", [], |r| r.get(0))?;
            if done > 0 {
                return Ok(None);
            }
            conn.execute(
                "INSERT INTO routes_imported (id, at) VALUES (1, ?1)",
                [to_millis(now)],
            )?;
            let existing: i64 = conn.query_row("SELECT count(*) FROM routes", [], |r| r.get(0))?;
            if existing > 0 {
                return Ok(Some(0));
            }
            for route in &routes {
                insert(conn, route)?;
            }
            Ok(Some(routes.len()))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::{at, store};

    fn rule(channel: Option<&str>, conversation: Option<&str>, agent: &str) -> NewRoute {
        NewRoute {
            channel: channel.map(str::to_owned),
            kind: None,
            conversation: conversation.map(str::to_owned),
            agent: AgentId::new(agent).unwrap(),
        }
    }

    async fn agents(store: &Store) -> Vec<String> {
        store
            .routes()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.route.agent.to_string())
            .collect()
    }

    async fn id_of(store: &Store, agent: &str) -> RouteId {
        let routes = store.routes().await.unwrap();
        routes
            .iter()
            .find(|r| r.route.agent.as_str() == agent)
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn routes_keep_their_order_and_every_field() {
        let (_dir, store) = store().await;
        assert!(store.routes().await.unwrap().is_empty());
        let full = NewRoute {
            channel: Some("irc-libera".into()),
            kind: Some(ConversationKind::Direct),
            conversation: Some("#giles".into()),
            agent: AgentId::new("butler").unwrap(),
        };
        let first = store.route_insert(usize::MAX, full.clone()).await.unwrap();
        let second = store
            .route_insert(usize::MAX, rule(None, None, "jeeves"))
            .await
            .unwrap();
        let routes = store.routes().await.unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!((routes[0].id, &routes[0].route), (first, &full));
        assert_eq!(routes[1].id, second);
        assert_eq!(routes[1].route, rule(None, None, "jeeves"));
    }

    #[tokio::test]
    async fn insert_places_the_route_at_an_index() {
        let (_dir, store) = store().await;
        for agent in ["a", "b"] {
            store
                .route_insert(usize::MAX, rule(None, None, agent))
                .await
                .unwrap();
        }
        store
            .route_insert(1, rule(None, None, "mid"))
            .await
            .unwrap();
        store
            .route_insert(0, rule(None, None, "top"))
            .await
            .unwrap();
        store
            .route_insert(99, rule(None, None, "end"))
            .await
            .unwrap();
        assert_eq!(agents(&store).await, ["top", "a", "mid", "b", "end"]);
    }

    #[tokio::test]
    async fn move_update_and_remove_keep_the_others_in_order() {
        let (_dir, store) = store().await;
        for agent in ["a", "b", "c", "d"] {
            store
                .route_insert(usize::MAX, rule(None, None, agent))
                .await
                .unwrap();
        }
        let (a, b, d) = (
            id_of(&store, "a").await,
            id_of(&store, "b").await,
            id_of(&store, "d").await,
        );
        assert!(store.route_move(d, 0).await.unwrap());
        assert_eq!(agents(&store).await, ["d", "a", "b", "c"]);
        assert!(store.route_move(d, 99).await.unwrap());
        assert_eq!(agents(&store).await, ["a", "b", "c", "d"]);

        assert!(
            store
                .route_update(b, rule(Some("web"), Some("x"), "bb"))
                .await
                .unwrap()
        );
        let routes = store.routes().await.unwrap();
        assert_eq!(routes[1].id, b);
        assert_eq!(routes[1].route, rule(Some("web"), Some("x"), "bb"));

        assert!(store.route_remove(a).await.unwrap());
        assert_eq!(agents(&store).await, ["bb", "c", "d"]);
        store
            .route_insert(usize::MAX, rule(None, None, "e"))
            .await
            .unwrap();
        assert_eq!(agents(&store).await, ["bb", "c", "d", "e"]);
    }

    #[tokio::test]
    async fn an_unknown_route_is_reported_not_created() {
        let (_dir, store) = store().await;
        let gone = RouteId::new(42);
        assert!(
            !store
                .route_update(gone, rule(None, None, "a"))
                .await
                .unwrap()
        );
        assert!(!store.route_remove(gone).await.unwrap());
        assert!(!store.route_move(gone, 0).await.unwrap());
        assert!(store.routes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ids_are_never_reused() {
        let (_dir, store) = store().await;
        let first = store.route_insert(0, rule(None, None, "a")).await.unwrap();
        store.route_remove(first).await.unwrap();
        let second = store.route_insert(0, rule(None, None, "b")).await.unwrap();
        assert_ne!(first, second);
        // The stale id can't touch the new route.
        assert!(!store.route_remove(first).await.unwrap());
        assert_eq!(agents(&store).await, ["b"]);
    }

    #[tokio::test]
    async fn the_import_runs_once() {
        let (_dir, store) = store().await;
        let imported = store
            .routes_import(
                vec![rule(Some("irc"), None, "a"), rule(None, None, "b")],
                at(0),
            )
            .await
            .unwrap();
        assert_eq!(imported, Some(2));
        assert!(store.routes_imported().await.unwrap());
        assert_eq!(agents(&store).await, ["a", "b"]);

        // Again, with other routes: nothing changes.
        let again = store
            .routes_import(vec![rule(None, None, "c")], at(1))
            .await
            .unwrap();
        assert_eq!(again, None);
        assert_eq!(agents(&store).await, ["a", "b"]);

        // Not even after every route is deleted.
        for route in store.routes().await.unwrap() {
            store.route_remove(route.id).await.unwrap();
        }
        let after = store
            .routes_import(vec![rule(None, None, "c")], at(2))
            .await
            .unwrap();
        assert_eq!(after, None);
        assert!(store.routes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_import_still_counts_as_the_import() {
        let (_dir, store) = store().await;
        assert_eq!(
            store.routes_import(Vec::new(), at(0)).await.unwrap(),
            Some(0)
        );
        assert_eq!(
            store
                .routes_import(vec![rule(None, None, "late")], at(1))
                .await
                .unwrap(),
            None
        );
        assert!(store.routes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_import_leaves_existing_routes_alone() {
        let (_dir, store) = store().await;
        store
            .route_insert(0, rule(None, None, "mine"))
            .await
            .unwrap();
        let imported = store
            .routes_import(vec![rule(None, None, "theirs")], at(0))
            .await
            .unwrap();
        assert_eq!(imported, Some(0));
        assert_eq!(agents(&store).await, ["mine"]);
    }

    #[tokio::test]
    async fn the_schema_refuses_malformed_rows() {
        let (_dir, store) = store().await;
        for sql in [
            "INSERT INTO routes (position, agent) VALUES (0, '')",
            "INSERT INTO routes (position, channel, agent) VALUES (0, '', 'a')",
            "INSERT INTO routes (position, conversation, agent) VALUES (0, '', 'a')",
            "INSERT INTO routes (position, kind, agent) VALUES (0, 'channel', 'a')",
            "INSERT INTO routes (position, agent) VALUES (-1, 'a')",
        ] {
            let err = store
                .write(move |conn| Ok(conn.execute(sql, [])?))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Database(_)), "{sql}: {err}");
        }
    }
}
