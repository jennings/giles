//! Channel event pumps: classify, store, ack, then act (PLAN sections 4.5, 8.5).

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use giles_store::{InboxState, NewInboxEvent, Session, SessionId, SessionKind};
use giles_types::{Channel, ChannelEvent, ChannelStatus, EventId, Timestamp};
use jiff::SignedDuration;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::{Inner, lock};
use crate::commands::CoreCommand;
use crate::config::InstanceName;
use crate::router::{ChannelSource, Inbound, Routed};

/// A channel's latest status event.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelStatusRecord {
    /// The status.
    pub status: ChannelStatus,
    /// When the core received it.
    pub at: Timestamp,
}

/// Delays between reopening a channel's event stream: 1 s, doubling to at most 60 s, back to 1 s
/// once a stream delivered an event.
#[derive(Debug)]
pub(super) struct Backoff {
    next: Duration,
}

impl Backoff {
    pub(super) const INITIAL: Duration = Duration::from_secs(1);
    pub(super) const MAX: Duration = Duration::from_secs(60);

    pub(super) fn new() -> Self {
        Self {
            next: Self::INITIAL,
        }
    }

    pub(super) fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (delay * 2).min(Self::MAX);
        delay
    }

    pub(super) fn reset(&mut self) {
        self.next = Self::INITIAL;
    }
}

/// Consumes `channel`'s events until `stop` is cancelled, reopening the stream with backoff when
/// it fails or ends. Cancellation takes effect between events, never inside one, so an event is
/// never left stored but unacted.
pub(super) async fn pump(
    inner: Arc<Inner>,
    instance: InstanceName,
    component: String,
    channel: Arc<dyn Channel>,
    stop: CancellationToken,
) {
    let mut backoff = Backoff::new();
    loop {
        let opened = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            opened = channel.events() => opened,
        };
        match opened {
            Ok(mut events) => {
                debug!(%instance, "channel event stream opened");
                let source = ChannelSource {
                    instance: &instance,
                    component: &component,
                    identity_namespace: &channel.info().identity_namespace,
                };
                loop {
                    let next = tokio::select! {
                        biased;
                        () = stop.cancelled() => return,
                        next = events.next() => next,
                    };
                    match next {
                        Some(Ok(event)) => {
                            if let Err(error) = handle(&inner, &source, &*channel, event).await {
                                error!(
                                    %instance,
                                    %error,
                                    "cannot store a channel event; it stays unacked"
                                );
                                break;
                            }
                            backoff.reset();
                        }
                        Some(Err(error)) => {
                            warn!(%instance, %error, "channel event stream failed");
                            break;
                        }
                        None => {
                            info!(%instance, "channel event stream ended");
                            break;
                        }
                    }
                }
            }
            Err(error) => warn!(%instance, %error, "cannot open the channel's event stream"),
        }
        let delay = backoff.next_delay();
        debug!(%instance, ?delay, "reopening the channel's event stream after a delay");
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

/// Handles one event. Every path that returns `Ok` has acked the event; an `Err` (the database
/// failed before the event was stored) leaves it unacked, so the channel resends it when the
/// stream is reopened.
async fn handle(
    inner: &Arc<Inner>,
    source: &ChannelSource<'_>,
    channel: &dyn Channel,
    event: ChannelEvent,
) -> Result<(), giles_store::Error> {
    let now = inner.clock.now();
    let ChannelEvent::Message { id, message } = &event else {
        if let ChannelEvent::Status { status, .. } = &event {
            debug!(instance = %source.instance, ?status, "channel status");
            lock(&inner.statuses).insert(
                source.instance.clone(),
                ChannelStatusRecord {
                    status: status.clone(),
                    at: now,
                },
            );
        } else {
            warn!(instance = %source.instance, event = %event.id(), "unknown channel event; ignored");
        }
        ack(source, channel, event.id()).await;
        return Ok(());
    };
    let pending = |to: &Routed, code: Option<&str>| inner.approvals.is_pending(to, code);
    match inner.router.classify(source, id, message, &pending) {
        Inbound::Drop(_) => ack(source, channel, id).await,
        Inbound::Message(to) => {
            let session = session_for(inner, &to, now).await?;
            let row = inner
                .store
                .inbox_insert(NewInboxEvent {
                    instance: source.instance.to_string(),
                    event_id: id.clone(),
                    session: Some(session),
                    user: Some(to.user.clone()),
                    payload: serde_json::to_value(&event)?,
                    state: InboxState::Queued,
                    received_at: now,
                })
                .await?;
            ack(source, channel, id).await;
            match row {
                Some(row) => {
                    debug!(instance = %source.instance, event = %id, %row, %session, "message queued");
                    inner.wake(session, Some(&to.user));
                }
                None => debug!(instance = %source.instance, event = %id, "duplicate event"),
            }
        }
        Inbound::Command { to, command } => {
            // Commands never wait in the inbox, but their event ids are dedupe keys like any
            // other: recorded before acting, so a redelivered `/new` or `yes` acts once.
            let row = inner
                .store
                .inbox_insert(NewInboxEvent {
                    instance: source.instance.to_string(),
                    event_id: id.clone(),
                    session: None,
                    user: Some(to.user.clone()),
                    payload: serde_json::to_value(&event)?,
                    state: InboxState::Done,
                    received_at: now,
                })
                .await?;
            ack(source, channel, id).await;
            if row.is_none() {
                debug!(instance = %source.instance, event = %id, "duplicate command");
                return Ok(());
            }
            command_act(inner, to, command, now).await;
        }
    }
    Ok(())
}

async fn ack(source: &ChannelSource<'_>, channel: &dyn Channel, event: &EventId) {
    // A lost ack only means a redelivery, which the inbox deduplicates.
    if let Err(error) = channel.ack(event).await {
        warn!(instance = %source.instance, %event, %error, "cannot ack a channel event");
    }
}

/// The open session for a routed message, after ending it first if the agent's `idle_reset` has
/// passed since its last message and it has no running or queued turn.
async fn session_for(
    inner: &Inner,
    to: &Routed,
    now: Timestamp,
) -> Result<SessionId, giles_store::Error> {
    if let Some(idle) = inner.policy(&to.session.agent).idle_reset
        && let Some(open) = inner.store.find_open_session(to.session.clone()).await?
        && !inner.is_live(open.id)
        && idle_expired(&open, idle, now)
        && inner.store.end_session(open.id, now).await?
    {
        info!(session = %open.id, agent = %open.agent, "session ended by idle_reset");
    }
    let (session, created) = inner
        .store
        .open_session(
            to.session.clone(),
            SessionKind::Direct,
            Some(to.user.clone()),
            now,
        )
        .await?;
    if created {
        info!(session = %session.id, agent = %session.agent, channel = to.session.channel, "session started");
    }
    Ok(session.id)
}

/// Whether `session` has been quiet for at least `idle` at `now`: since its last message, or
/// since it started if it has none.
pub(super) fn idle_expired(session: &Session, idle: Duration, now: Timestamp) -> bool {
    let last = session.last_message_at.unwrap_or(session.started_at);
    SignedDuration::try_from(idle).is_ok_and(|idle| now.duration_since(last) >= idle)
}

/// Acts on a command whose event is recorded and acked. Failures are logged: the event can't be
/// redelivered any more.
async fn command_act(inner: &Inner, to: Routed, command: CoreCommand, now: Timestamp) {
    let agent = &to.session.agent;
    let channel = &to.session.channel;
    match command {
        CoreCommand::New => match inner.store.find_open_session(to.session.clone()).await {
            Ok(Some(open)) => match inner.store.end_session(open.id, now).await {
                Ok(_) => info!(session = %open.id, %agent, channel, "session ended by /new"),
                Err(error) => error!(session = %open.id, %error, "cannot end the session"),
            },
            Ok(None) => debug!(%agent, channel, "/new with no open session"),
            Err(error) => error!(%agent, channel, %error, "cannot find the session for /new"),
        },
        CoreCommand::Stop => {
            if inner.stop(&to.session) {
                info!(%agent, channel, "turn stopped by /stop");
            } else {
                info!(%agent, channel, "/stop with no turn running; nothing to do");
            }
        }
        CoreCommand::Approval(reply) => inner.approvals.answer(to, reply).await,
    }
}

#[cfg(test)]
mod tests {
    use giles_types::{AgentId, ConversationId};

    use super::*;

    #[test]
    fn backoff_doubles_to_a_cap_and_resets() {
        let mut backoff = Backoff::new();
        let delays: Vec<u64> = (0..8).map(|_| backoff.next_delay().as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Backoff::INITIAL);
    }

    fn session(started: i64, last: Option<i64>) -> Session {
        Session {
            id: SessionId::new(1),
            agent: AgentId::new("jeeves").unwrap(),
            kind: SessionKind::Direct,
            channel: Some("fake".to_owned()),
            conversation: Some(ConversationId::new("owner").unwrap()),
            user: None,
            started_at: Timestamp::from_second(started).unwrap(),
            ended_at: None,
            last_message_at: last.map(|s| Timestamp::from_second(s).unwrap()),
        }
    }

    #[test]
    fn idle_counts_from_the_last_message_or_the_start() {
        let idle = Duration::from_secs(100);
        let at = |s| Timestamp::from_second(s).unwrap();
        // Exactly `idle` after the last message resets; a moment earlier doesn't.
        assert!(idle_expired(&session(0, Some(50)), idle, at(150)));
        assert!(!idle_expired(&session(0, Some(50)), idle, at(149)));
        // Without messages, the start counts.
        assert!(idle_expired(&session(0, None), idle, at(100)));
        assert!(!idle_expired(&session(0, None), idle, at(99)));
        // A clock that went backwards never resets.
        assert!(!idle_expired(&session(0, Some(50)), idle, at(10)));
    }
}
