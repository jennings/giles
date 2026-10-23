//! The per-session driver: runs the session's queued inbox rows as turns, one at a time.

use std::sync::{Arc, Mutex};

use giles_store::{InboxEntry, InboxId, Session, SessionId};
use giles_types::UserId;
use tokio::sync::Notify;
use tracing::{error, info, warn};

use super::turn::{Reply, Steering, Turn, TurnMessage, TurnOutcome};
use super::{Inner, LiveTurn, lock};

/// The next turn's requester and messages: the sender of the oldest queued row and all of that
/// sender's queued rows, in order (PLAN section 6.3: consecutive messages from one person form
/// one turn; others' messages never join it and wait for their own turn).
pub(super) fn select_batch(queued: Vec<InboxEntry>) -> (Option<UserId>, Vec<InboxEntry>) {
    let Some(first) = queued.first() else {
        return (None, Vec::new());
    };
    let requester = first.user.clone();
    let batch = queued
        .into_iter()
        .filter(|entry| entry.user == requester)
        .collect();
    (requester, batch)
}

/// Runs turns for `session` until it has no queued rows, then retires. Spawned by
/// [`Inner::wake`], which registered the session as live.
pub(super) async fn drive(inner: Arc<Inner>, session: SessionId) {
    loop {
        if let Some(live) = lock(&inner.live).get_mut(&session) {
            live.dirty = false;
        }
        if inner.shutdown.is_cancelled() {
            lock(&inner.live).remove(&session);
            return;
        }
        let queued = match inner.store.inbox_queued(session).await {
            Ok(queued) => queued,
            Err(error) => {
                error!(%session, %error, "cannot read the session's queued messages");
                lock(&inner.live).remove(&session);
                return;
            }
        };
        if queued.is_empty() {
            if retire(&inner, session) {
                return;
            }
            continue;
        }
        let loaded = match inner.store.session(session).await {
            Ok(Some(loaded)) => loaded,
            Ok(None) => {
                error!(%session, "queued messages for a session that doesn't exist");
                lock(&inner.live).remove(&session);
                return;
            }
            Err(error) => {
                error!(%session, %error, "cannot load the session");
                lock(&inner.live).remove(&session);
                return;
            }
        };
        if let Some(live) = lock(&inner.live).get_mut(&session) {
            live.key = loaded.key();
        }
        let (requester, batch) = select_batch(queued);
        if !run_turn(&inner, loaded, requester, batch).await {
            lock(&inner.live).remove(&session);
            return;
        }
    }
}

/// Removes the session from the live map unless a wake arrived since the driver last read the
/// queue. Returns whether it retired.
fn retire(inner: &Inner, session: SessionId) -> bool {
    let mut live = lock(&inner.live);
    if live.get(&session).is_some_and(|l| l.dirty) {
        return false;
    }
    live.remove(&session);
    true
}

/// Starts `batch` as a turn, runs it, and finishes its rows. Returns `false` if the driver must
/// stop (a database error, or shutdown).
async fn run_turn(
    inner: &Arc<Inner>,
    session: Session,
    requester: Option<UserId>,
    batch: Vec<InboxEntry>,
) -> bool {
    let id = session.id;
    let ids: Vec<InboxId> = batch.iter().map(|entry| entry.id).collect();
    if let Err(error) = inner.store.inbox_start(ids.clone()).await {
        error!(session = %id, %error, "cannot start a turn");
        return false;
    }
    let messages: Vec<TurnMessage> = batch.iter().filter_map(TurnMessage::from_entry).collect();
    let rows = Arc::new(Mutex::new(ids));
    if messages.is_empty() {
        // Every row was unreadable (logged); nothing to run.
        return finish(inner, id, &rows).await;
    }

    let policy = inner.policy(&session.agent);
    let cancel = inner.shutdown.child_token();
    let notify = Arc::new(Notify::new());
    let (steering, steer) = Steering::new(
        inner.store.clone(),
        id,
        requester.clone(),
        notify.clone(),
        rows.clone(),
    );
    if let Some(live) = lock(&inner.live).get_mut(&id) {
        // A message that arrived after the queue was read has set `dirty` without waking the
        // turn; wake it now (a spurious wake costs one empty `take`).
        if live.dirty {
            notify.notify_one();
        }
        live.turn = Some(LiveTurn {
            cancel: cancel.clone(),
            requester: requester.clone(),
            steer: notify,
        });
    }
    let turn = Turn {
        reply: Reply::for_session(&session, inner.channels.clone()),
        session,
        requester,
        messages,
        busy_mode: policy.busy_mode,
        cancel,
        steering,
    };
    info!(session = %id, agent = %turn.session.agent, messages = turn.messages.len(), "turn started");

    let runner = inner.runner.clone();
    let outcome = inner
        .tasks
        .spawn(async move { runner.run_turn(turn).await })
        .await;
    if let Some(live) = lock(&inner.live).get_mut(&id) {
        live.turn = None;
    }
    steer.close().await;
    let stopping = inner.shutdown.is_cancelled();
    if stopping && !matches!(outcome, Ok(TurnOutcome::Completed)) {
        // Left `started`: the next start marks the rows `unknown` and tells the requester.
        warn!(session = %id, "turn cut off by shutdown");
        return false;
    }
    match outcome {
        Ok(TurnOutcome::Completed) => info!(session = %id, "turn completed"),
        Ok(TurnOutcome::Cancelled) => info!(session = %id, "turn cancelled"),
        Ok(TurnOutcome::Failed(reason)) => warn!(session = %id, %reason, "turn failed"),
        Err(error) => error!(session = %id, %error, "turn panicked"),
    }
    finish(inner, id, &rows).await && !stopping
}

async fn finish(inner: &Inner, session: SessionId, rows: &Mutex<Vec<InboxId>>) -> bool {
    let ids = std::mem::take(&mut *lock(rows));
    match inner.store.inbox_finish(ids).await {
        Ok(()) => true,
        Err(error) => {
            error!(%session, %error, "cannot finish the turn's inbox rows");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use giles_store::InboxState;
    use giles_types::{EventId, Timestamp};
    use serde_json::Value;

    use super::*;

    fn entry(id: i64, user: Option<&str>) -> InboxEntry {
        InboxEntry {
            id: InboxId::new(id),
            instance: "fake".to_owned(),
            event_id: EventId::new(format!("e{id}")).unwrap(),
            session: Some(SessionId::new(1)),
            user: user.map(|u| UserId::new(u).unwrap()),
            payload: Value::Null,
            state: InboxState::Queued,
            received_at: Timestamp::UNIX_EPOCH,
        }
    }

    fn ids(batch: &[InboxEntry]) -> Vec<i64> {
        batch.iter().map(|e| e.id.get()).collect()
    }

    #[test]
    fn a_batch_is_every_queued_row_of_the_oldest_rows_sender() {
        let (requester, batch) = select_batch(vec![
            entry(1, Some("ann")),
            entry(2, Some("bob")),
            entry(3, Some("ann")),
            entry(4, Some("bob")),
        ]);
        assert_eq!(requester, Some(UserId::new("ann").unwrap()));
        assert_eq!(ids(&batch), [1, 3]);

        let (requester, batch) = select_batch(vec![entry(2, Some("bob")), entry(4, Some("bob"))]);
        assert_eq!(requester, Some(UserId::new("bob").unwrap()));
        assert_eq!(ids(&batch), [2, 4]);
    }

    #[test]
    fn rows_without_a_user_batch_together() {
        let (requester, batch) =
            select_batch(vec![entry(1, None), entry(2, Some("ann")), entry(3, None)]);
        assert_eq!(requester, None);
        assert_eq!(ids(&batch), [1, 3]);
        assert_eq!(select_batch(Vec::new()), (None, Vec::new()));
    }
}
