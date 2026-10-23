//! `inbox`: every acked inbound event, deduplicated, and the queue of turns (PLAN 6.3, 8.5).
//!
//! States: a message waiting for a turn is `queued`; the turn that takes it marks it `started`;
//! the turn's end marks it `done`. At startup, rows still `started` belonged to turns a restart
//! cut off; they become `unknown` (nothing is replayed). Events that never start a turn (status
//! events, dropped messages, consumed commands) can be inserted as `done` and serve only as
//! dedupe keys.

use giles_types::{EventId, Timestamp, UserId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use crate::sessions::SessionId;
use crate::time::to_millis;
use crate::{Error, Result, Store, get_time, parse_id, query_all, query_opt, row_id};

row_id!(
    /// A row in `inbox`.
    InboxId
);

/// Where an inbox row is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InboxState {
    /// Waiting for a turn.
    Queued,
    /// Taken by a running turn.
    Started,
    /// Handled. Kept only as a dedupe key until pruned.
    Done,
    /// Its turn was cut off by a restart. Kept only as a dedupe key until pruned.
    Unknown,
}

impl InboxState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Started => "started",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "started" => Ok(Self::Started),
            "done" => Ok(Self::Done),
            "unknown" => Ok(Self::Unknown),
            other => Err(Error::corrupt(format!("inbox state {other:?}"))),
        }
    }
}

/// An event to insert with [`Store::inbox_insert`].
#[derive(Debug, Clone, PartialEq)]
pub struct NewInboxEvent {
    /// The channel instance it came from.
    pub instance: String,
    /// Its id, unique within the instance; the dedupe key.
    pub event_id: EventId,
    /// The session it was routed to, if any.
    pub session: Option<SessionId>,
    /// The user who sent it, if known.
    pub user: Option<UserId>,
    /// The whole event as JSON.
    pub payload: Value,
    /// [`InboxState::Queued`] to wait for a turn, or [`InboxState::Done`] for a dedupe key only.
    pub state: InboxState,
    /// When the core received it.
    pub received_at: Timestamp,
}

/// A row in `inbox`.
#[derive(Debug, Clone, PartialEq)]
pub struct InboxEntry {
    /// The row.
    pub id: InboxId,
    /// The channel instance.
    pub instance: String,
    /// The event id.
    pub event_id: EventId,
    /// The session it was routed to. `None` if it had none, or its session was deleted.
    pub session: Option<SessionId>,
    /// The sender.
    pub user: Option<UserId>,
    /// The event as JSON.
    pub payload: Value,
    /// The state.
    pub state: InboxState,
    /// When it was received.
    pub received_at: Timestamp,
}

pub(crate) const SELECT: &str =
    "SELECT id, instance, event_id, session_id, user_id, payload, state, received_at FROM inbox";

pub(crate) fn from_row(row: &Row<'_>) -> Result<InboxEntry> {
    Ok(InboxEntry {
        id: row.get(0)?,
        instance: row.get(1)?,
        event_id: parse_id(row.get(2)?, EventId::new)?,
        session: row.get(3)?,
        user: row
            .get::<_, Option<String>>(4)?
            .map(|u| parse_id(u, UserId::new))
            .transpose()?,
        payload: serde_json::from_str(&row.get::<_, String>(5)?)?,
        state: InboxState::parse(&row.get::<_, String>(6)?)?,
        received_at: get_time(row, 7)?,
    })
}

/// Moves every row in `ids` from `from` to `to`, or none of them.
fn transition(conn: &Connection, ids: &[InboxId], from: InboxState, to: InboxState) -> Result<()> {
    let mut state = conn.prepare_cached("SELECT state FROM inbox WHERE id = ?1")?;
    let mut update = conn.prepare_cached("UPDATE inbox SET state = ?2 WHERE id = ?1")?;
    for &id in ids {
        let current = state
            .query_row([id], |row| row.get::<_, String>(0))
            .optional()?
            .ok_or_else(|| Error::not_found(format!("inbox row {id}")))?;
        let current = InboxState::parse(&current)?;
        if current != from {
            return Err(Error::conflict(format!(
                "inbox row {id} is {}, not {}",
                current.as_str(),
                from.as_str()
            )));
        }
        update.execute(params![id, to.as_str()])?;
    }
    Ok(())
}

impl Store {
    /// Stores an inbound event unless (`instance`, `event_id`) is already there. Returns the new
    /// row, or `None` for a duplicate (which changes nothing). Call it before acking the event
    /// (PLAN 8.5). `state` must be `Queued` or `Done`, and a `Queued` row needs a session (nothing
    /// would ever take it otherwise).
    pub async fn inbox_insert(&self, event: NewInboxEvent) -> Result<Option<InboxId>> {
        if !matches!(event.state, InboxState::Queued | InboxState::Done) {
            return Err(Error::invalid(format!(
                "new inbox rows can't be {}",
                event.state.as_str()
            )));
        }
        if event.state == InboxState::Queued && event.session.is_none() {
            return Err(Error::invalid("a queued inbox row needs a session"));
        }
        let payload = serde_json::to_string(&event.payload)?;
        self.write(move |conn| {
            let inserted = conn
                .prepare_cached(
                    "INSERT INTO inbox
                        (instance, event_id, session_id, user_id, payload, state, received_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (instance, event_id) DO NOTHING",
                )?
                .execute(params![
                    event.instance,
                    event.event_id.as_str(),
                    event.session,
                    event.user.as_ref().map(UserId::as_str),
                    payload,
                    event.state.as_str(),
                    to_millis(event.received_at),
                ])?;
            Ok((inserted == 1).then(|| InboxId(conn.last_insert_rowid())))
        })
        .await
    }

    /// One inbox row.
    pub async fn inbox_entry(&self, id: InboxId) -> Result<Option<InboxEntry>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!("{SELECT} WHERE id = ?1"))?;
            query_opt(&mut stmt, [id], from_row)
        })
        .await
    }

    /// The session's queued rows, oldest first.
    pub async fn inbox_queued(&self, session: SessionId) -> Result<Vec<InboxEntry>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "{SELECT} WHERE session_id = ?1 AND state = 'queued' ORDER BY id"
            ))?;
            query_all(&mut stmt, [session], from_row)
        })
        .await
    }

    /// Sessions with queued rows, in the order of their oldest queued row. At startup these are
    /// the turns still to run.
    pub async fn inbox_sessions_with_queued(&self) -> Result<Vec<SessionId>> {
        self.read(|conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT session_id FROM inbox
                 WHERE state = 'queued' AND session_id IS NOT NULL
                 GROUP BY session_id ORDER BY min(id)",
            )?;
            query_all(&mut stmt, [], |row| Ok(row.get(0)?))
        })
        .await
    }

    /// Marks rows `started`: a turn has taken them. All must be `queued`, else nothing changes
    /// and the result is [`Error::Conflict`] ([`Error::NotFound`] for a missing row).
    pub async fn inbox_start(&self, ids: Vec<InboxId>) -> Result<()> {
        self.write(move |conn| transition(conn, &ids, InboxState::Queued, InboxState::Started))
            .await
    }

    /// Marks rows `done`: their turn ended. All must be `started`, as for
    /// [`inbox_start`](Self::inbox_start).
    pub async fn inbox_finish(&self, ids: Vec<InboxId>) -> Result<()> {
        self.write(move |conn| transition(conn, &ids, InboxState::Started, InboxState::Done))
            .await
    }

    /// Startup recovery: marks every `started` row `unknown` and returns those rows, so the core
    /// can tell each requester "Interrupted by a restart; resend if needed" (PLAN 6.3).
    pub async fn inbox_interrupt(&self) -> Result<Vec<InboxEntry>> {
        self.write(|conn| {
            let mut stmt =
                conn.prepare_cached(&format!("{SELECT} WHERE state = 'started' ORDER BY id"))?;
            let mut rows = query_all(&mut stmt, [], from_row)?;
            conn.prepare_cached("UPDATE inbox SET state = 'unknown' WHERE state = 'started'")?
                .execute([])?;
            for row in &mut rows {
                row.state = InboxState::Unknown;
            }
            Ok(rows)
        })
        .await
    }

    /// Deletes `done` and `unknown` rows received before `before` (now minus 30 days: dedupe
    /// keys are kept that long, PLAN 8.5). `queued` and `started` rows are never pruned.
    /// Returns the number deleted.
    pub async fn inbox_prune(&self, before: Timestamp) -> Result<u64> {
        self.write(move |conn| {
            let deleted = conn
                .prepare_cached(
                    "DELETE FROM inbox
                     WHERE state IN ('done', 'unknown') AND received_at < ?1",
                )?
                .execute([to_millis(before)])?;
            Ok(deleted as u64)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::messages::tests::session;
    use crate::sessions::tests::{at, store};

    fn event(id: &str, session: Option<SessionId>, state: InboxState, secs: i64) -> NewInboxEvent {
        NewInboxEvent {
            instance: "whatsapp".into(),
            event_id: EventId::new(id).unwrap(),
            session,
            user: Some(UserId::new("owner").unwrap()),
            payload: json!({ "text": id }),
            state,
            received_at: at(secs),
        }
    }

    #[tokio::test]
    async fn inserts_dedupe_on_instance_and_event_id() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let id = store
            .inbox_insert(event("e1", Some(s), InboxState::Queued, 1))
            .await
            .unwrap()
            .unwrap();
        let mut again = event("e1", None, InboxState::Done, 2);
        again.payload = json!("different");
        assert_eq!(store.inbox_insert(again).await.unwrap(), None);
        let mut other_instance = event("e1", Some(s), InboxState::Queued, 3);
        other_instance.instance = "irc".into();
        assert!(store.inbox_insert(other_instance).await.unwrap().is_some());

        let row = store.inbox_entry(id).await.unwrap().unwrap();
        assert_eq!(
            row.payload,
            json!({ "text": "e1" }),
            "the duplicate changed nothing"
        );
        assert_eq!(
            (row.state, row.session, row.received_at),
            (InboxState::Queued, Some(s), at(1))
        );

        let err = store
            .inbox_insert(event("e2", Some(s), InboxState::Started, 1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        let err = store
            .inbox_insert(event("e3", None, InboxState::Queued, 1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_duplicates_insert_once() {
        let (_dir, store) = store().await;
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let store = store.clone();
            tasks.spawn(async move {
                store
                    .inbox_insert(event("same", None, InboxState::Done, 1))
                    .await
            });
        }
        let mut inserted = 0;
        while let Some(result) = tasks.join_next().await {
            inserted += usize::from(result.unwrap().unwrap().is_some());
        }
        assert_eq!(inserted, 1);
    }

    #[tokio::test]
    async fn rows_move_queued_started_done_all_or_nothing() {
        let (_dir, store) = store().await;
        let s1 = session(&store, "c1").await;
        let s2 = session(&store, "c2").await;
        let mut ids = Vec::new();
        for (i, s) in [s2, s1, s1, s2].into_iter().enumerate() {
            let event = event(&format!("e{i}"), Some(s), InboxState::Queued, 1);
            ids.push(store.inbox_insert(event).await.unwrap().unwrap());
        }
        store
            .inbox_insert(event("status", None, InboxState::Done, 1))
            .await
            .unwrap();
        assert_eq!(store.inbox_sessions_with_queued().await.unwrap(), [s2, s1]);
        let queued: Vec<_> = store
            .inbox_queued(s1)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(queued, [ids[1], ids[2]]);

        store.inbox_start(vec![ids[1], ids[2]]).await.unwrap();
        assert!(store.inbox_queued(s1).await.unwrap().is_empty());
        assert_eq!(store.inbox_sessions_with_queued().await.unwrap(), [s2]);

        // ids[1] is no longer queued, so the whole batch fails and ids[0] stays queued.
        let err = store.inbox_start(vec![ids[0], ids[1]]).await.unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        assert_eq!(
            store.inbox_entry(ids[0]).await.unwrap().unwrap().state,
            InboxState::Queued
        );
        let err = store
            .inbox_start(vec![InboxId::new(999)])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");

        let err = store.inbox_finish(vec![ids[0]]).await.unwrap_err();
        assert!(
            matches!(err, Error::Conflict(_)),
            "a queued row can't finish: {err}"
        );
        store.inbox_finish(vec![ids[1], ids[2]]).await.unwrap();
        assert_eq!(
            store.inbox_entry(ids[2]).await.unwrap().unwrap().state,
            InboxState::Done
        );
    }

    #[tokio::test]
    async fn interrupt_marks_started_rows_unknown_and_keeps_queued_ones() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let a = store
            .inbox_insert(event("a", Some(s), InboxState::Queued, 1))
            .await
            .unwrap()
            .unwrap();
        let b = store
            .inbox_insert(event("b", Some(s), InboxState::Queued, 2))
            .await
            .unwrap()
            .unwrap();
        store.inbox_start(vec![a]).await.unwrap();

        let interrupted = store.inbox_interrupt().await.unwrap();
        assert_eq!(interrupted.len(), 1);
        assert_eq!(
            (interrupted[0].id, interrupted[0].state),
            (a, InboxState::Unknown)
        );
        assert_eq!(
            store.inbox_entry(a).await.unwrap().unwrap().state,
            InboxState::Unknown
        );
        assert_eq!(
            store.inbox_entry(b).await.unwrap().unwrap().state,
            InboxState::Queued
        );
        assert!(store.inbox_interrupt().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prune_drops_old_terminal_rows_only() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let insert = |id: &'static str, state, secs| {
            let store = store.clone();
            async move {
                let session = (state == InboxState::Queued).then_some(s);
                let event = event(id, session, state, secs);
                store.inbox_insert(event).await.unwrap().unwrap()
            }
        };
        let old_done = insert("d", InboxState::Done, 10).await;
        let boundary = insert("b", InboxState::Done, 20).await;
        let old_unknown = insert("u", InboxState::Queued, 10).await;
        store.inbox_start(vec![old_unknown]).await.unwrap();
        store.inbox_interrupt().await.unwrap();
        let old_queued = insert("q", InboxState::Queued, 10).await;
        let old_started = insert("s", InboxState::Queued, 10).await;
        store.inbox_start(vec![old_started]).await.unwrap();

        assert_eq!(store.inbox_prune(at(20)).await.unwrap(), 2);
        for (id, kept) in [
            (old_done, false),
            (boundary, true),
            (old_unknown, false),
            (old_queued, true),
            (old_started, true),
        ] {
            assert_eq!(store.inbox_entry(id).await.unwrap().is_some(), kept, "{id}");
        }
        // The pruned key can be inserted again.
        assert!(
            store
                .inbox_insert(event("d", None, InboxState::Done, 30))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn deleting_a_session_keeps_its_inbox_rows() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let id = store
            .inbox_insert(event("a", Some(s), InboxState::Done, 1))
            .await
            .unwrap()
            .unwrap();
        store
            .write(move |conn| {
                conn.execute("DELETE FROM sessions WHERE id = ?1", [s])?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(store.inbox_entry(id).await.unwrap().unwrap().session, None);
    }
}
