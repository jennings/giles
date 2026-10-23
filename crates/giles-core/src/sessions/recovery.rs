//! Startup recovery (PLAN section 6.3) and dedupe-key pruning (PLAN section 8.5).

use std::sync::Arc;
use std::time::Duration;

use giles_store::SessionId;
use giles_types::{Channel, ConversationId, OutboundMessage, SendTarget};
use tracing::{debug, info, warn};

use super::{
    DEDUPE_RETENTION, INTERRUPTED_NOTICE, INTERRUPTED_RESULT, Inner, PRUNE_INTERVAL, lock,
};

/// How long the core waits for one channel to accept a restart notice.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(10);

/// A restart notice for one session's conversation.
#[derive(Debug, Clone)]
pub(super) struct Notice {
    session: SessionId,
    conversation: ConversationId,
}

/// Fills every empty tool-result slot with `interrupted`, marks the inbox rows of turns that were
/// running `unknown`, and tells each of those turns' requesters, once per session, in the
/// session's conversation. Nothing is replayed.
pub(super) async fn recover(inner: &Inner) -> Result<(), giles_store::Error> {
    let now = inner.clock.now();
    let filled = inner
        .store
        .fill_all_empty_slots(INTERRUPTED_RESULT, now)
        .await?;
    if !filled.is_empty() {
        warn!(
            sessions = filled.len(),
            "filled empty tool-result slots with `interrupted` after a restart"
        );
    }
    let rows = inner.store.inbox_interrupt().await?;
    let mut sessions: Vec<SessionId> = Vec::new();
    for row in &rows {
        if let Some(session) = row.session
            && !sessions.contains(&session)
        {
            sessions.push(session);
        }
    }
    if !rows.is_empty() {
        warn!(
            rows = rows.len(),
            sessions = sessions.len(),
            "turns were cut off by a restart"
        );
    }
    for session in sessions {
        notify_interrupted(inner, session).await;
    }
    Ok(())
}

/// Tells the requester of `session`'s interrupted turn. Plugin channels aren't registered this
/// early, so a notice for an instance without a channel (or one whose send fails) is kept and
/// sent when the instance is attached ([`send_pending_notices`]).
async fn notify_interrupted(inner: &Inner, session: SessionId) {
    let loaded = match inner.store.session(session).await {
        Ok(Some(loaded)) => loaded,
        Ok(None) => {
            warn!(%session, "interrupted turn's session is gone; requester not told");
            return;
        }
        Err(error) => {
            warn!(%session, %error, "cannot load the interrupted turn's session; requester not told");
            return;
        }
    };
    let (Some(instance), Some(conversation)) = (loaded.channel, loaded.conversation) else {
        warn!(%session, "interrupted turn's session has no conversation; requester not told");
        return;
    };
    let notice = Notice {
        session,
        conversation,
    };
    match inner.channels.channel(&instance) {
        Some(channel) => {
            if !deliver(&*channel, &instance, &notice).await {
                keep(inner, instance, notice);
            }
        }
        None => {
            info!(%session, channel = instance, "restart notice waits for the channel");
            keep(inner, instance, notice);
        }
    }
}

fn keep(inner: &Inner, instance: String, notice: Notice) {
    lock(&inner.notices)
        .entry(instance)
        .or_default()
        .push(notice);
}

/// Sends one restart notice. Returns whether the channel accepted it.
async fn deliver(channel: &dyn Channel, instance: &str, notice: &Notice) -> bool {
    let session = notice.session;
    let send = channel.send(OutboundMessage {
        target: SendTarget::Conversation(notice.conversation.clone()),
        markdown: INTERRUPTED_NOTICE.to_owned(),
        attachments: Vec::new(),
        reply_to: None,
    });
    match tokio::time::timeout(NOTICE_TIMEOUT, send).await {
        Ok(Ok(_)) => {
            info!(%session, channel = instance, "told the requester about the restart");
            true
        }
        Ok(Err(error)) => {
            warn!(%session, channel = instance, %error, "cannot tell the requester about the restart");
            false
        }
        Err(_) => {
            warn!(%session, channel = instance, "timed out telling the requester about the restart");
            false
        }
    }
}

/// Sends, in the background, the restart notices kept for `instance` to its newly attached
/// `channel`. Each is sent once; one the channel refuses is kept for the next attach.
pub(super) fn send_pending_notices(inner: &Arc<Inner>, instance: &str, channel: Arc<dyn Channel>) {
    let Some(notices) = lock(&inner.notices).remove(instance) else {
        return;
    };
    let inner = inner.clone();
    let instance = instance.to_owned();
    tokio::spawn(async move {
        for notice in notices {
            if !deliver(&*channel, &instance, &notice).await {
                keep(&inner, instance.clone(), notice);
            }
        }
    });
}

/// Wakes every session with queued inbox rows, oldest first.
pub(super) async fn resume(inner: &Arc<Inner>) -> Result<(), giles_store::Error> {
    let sessions = inner.store.inbox_sessions_with_queued().await?;
    if !sessions.is_empty() {
        info!(sessions = sessions.len(), "resuming queued messages");
    }
    for session in sessions {
        inner.wake(session, None);
    }
    Ok(())
}

/// Prunes dedupe keys now and every [`PRUNE_INTERVAL`] until shutdown.
pub(super) async fn prune_loop(inner: Arc<Inner>) {
    let mut ticks = tokio::time::interval(PRUNE_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = inner.shutdown.cancelled() => return,
            _ = ticks.tick() => prune(&inner).await,
        }
    }
}

/// Deletes finished inbox rows received more than [`DEDUPE_RETENTION`] ago.
async fn prune(inner: &Inner) {
    let Some(before) = inner.clock.now().checked_sub(DEDUPE_RETENTION).ok() else {
        return;
    };
    match inner.store.inbox_prune(before).await {
        Ok(0) => {}
        Ok(deleted) => debug!(deleted, "pruned old inbox dedupe keys"),
        Err(error) => warn!(%error, "cannot prune inbox dedupe keys"),
    }
}
