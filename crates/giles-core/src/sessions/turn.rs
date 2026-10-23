//! The turn seam: what the session code hands the agent loop, and what it gets back.

use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use giles_store::{InboxEntry, InboxId, Session, SessionId, Store};
use giles_types::{
    Channel, ChannelEvent, ConversationId, Error, EventId, InboundMessage, MessageId,
    OutboundMessage, SendTarget, Timestamp, UserId,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::{ChannelLookup, lock};
use crate::config::BusyMode;

/// Runs turns. The agent loop (TASKS.md 2.4) implements it.
#[async_trait]
pub trait TurnRunner: Send + Sync {
    /// Runs one turn to its end. The session code marks the turn's inbox rows `started` before
    /// calling this and `done` when it returns, whatever the outcome. It must return promptly
    /// once [`Turn::cancel`] is cancelled (`/stop`, shutdown).
    async fn run_turn(&self, turn: Turn) -> TurnOutcome;
}

/// How a turn ended. Logged; the inbox rows are `done` either way.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnOutcome {
    /// The model finished (or hit its iteration cap and said so).
    Completed,
    /// [`Turn::cancel`] fired: `/stop` or shutdown.
    Cancelled,
    /// The turn failed, e.g. the provider was unavailable. The text is logged.
    Failed(String),
}

/// One turn: a batch of queued messages from one requester in one session (PLAN section 6.3).
#[derive(Debug)]
pub struct Turn {
    /// The session, as loaded when the turn started.
    pub session: Session,
    /// The sender of the turn's messages, who decides which tools are allowed and answers
    /// approvals.
    pub requester: Option<UserId>,
    /// The messages that started the turn, oldest first. Never empty.
    pub messages: Vec<TurnMessage>,
    /// The agent's `busy_mode`: whether the agent loop takes [`steering`](Self::steering)
    /// messages at tool-batch boundaries.
    pub busy_mode: BusyMode,
    /// Cancelled by `/stop` and at shutdown.
    pub cancel: CancellationToken,
    /// The requester's new messages during the turn.
    pub steering: Steering,
    /// Where the turn's replies go: its session's conversation. `None` for a session with no
    /// conversation (an isolated job run).
    pub reply: Option<Reply>,
}

/// An inbound message of a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnMessage {
    /// Its inbox row.
    pub inbox: InboxId,
    /// The channel's event id.
    pub event: EventId,
    /// When the core received it.
    pub received_at: Timestamp,
    /// The message.
    pub message: InboundMessage,
}

impl TurnMessage {
    /// The message stored in an inbox row; `None` (logged) if the payload isn't a message
    /// event.
    pub(super) fn from_entry(entry: &InboxEntry) -> Option<Self> {
        match serde_json::from_value::<ChannelEvent>(entry.payload.clone()) {
            Ok(ChannelEvent::Message { id, message }) => Some(Self {
                inbox: entry.id,
                event: id,
                received_at: entry.received_at,
                message,
            }),
            Ok(_) => {
                tracing::error!(row = %entry.id, "queued inbox row is not a message; skipped");
                None
            }
            Err(error) => {
                tracing::error!(row = %entry.id, %error, "unreadable inbox row; skipped");
                None
            }
        }
    }
}

/// The requester's messages that arrive while a turn runs (PLAN sections 6.3, 6.4).
///
/// In both busy modes, [`changed`](Self::changed) wakes when the requester sends a message and
/// [`take`](Self::take) moves the requester's queued messages into the turn: their inbox rows
/// become `started` and are finished with the turn. The agent loop decides when to take, from
/// [`Turn::busy_mode`]: at tool-batch boundaries with `steer`, and in either mode while an
/// approval is pending (a message denies it and the model sees it). Messages the turn doesn't
/// take stay `queued` and start the next turn. Valid only while [`TurnRunner::run_turn`] runs;
/// afterwards `take` returns nothing.
pub struct Steering {
    shared: Option<Arc<SteerShared>>,
}

pub(super) struct SteerShared {
    store: Store,
    session: SessionId,
    requester: Option<UserId>,
    notify: Arc<Notify>,
    /// Every row the turn owns, finished when it ends.
    rows: Arc<Mutex<Vec<InboxId>>>,
    /// Held across a whole `take`; the driver takes it and sets the flag before finishing the
    /// turn's rows, so no row is started after the turn ended.
    closed: tokio::sync::Mutex<bool>,
}

impl fmt::Debug for Steering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Steering")
            .field("detached", &self.shared.is_none())
            .finish_non_exhaustive()
    }
}

impl Steering {
    /// Steering for a turn in `session`, and the handle the driver closes when the turn ends.
    pub(super) fn new(
        store: Store,
        session: SessionId,
        requester: Option<UserId>,
        notify: Arc<Notify>,
        rows: Arc<Mutex<Vec<InboxId>>>,
    ) -> (Self, Arc<SteerShared>) {
        let shared = Arc::new(SteerShared {
            store,
            session,
            requester,
            notify,
            rows,
            closed: tokio::sync::Mutex::new(false),
        });
        (
            Self {
                shared: Some(shared.clone()),
            },
            shared,
        )
    }

    /// Steering with no session behind it, which never yields a message: for turns built
    /// outside the session manager, e.g. in tests.
    pub fn detached() -> Self {
        Self { shared: None }
    }

    /// Waits until the requester sends a new message. Never returns when detached. A wake-up
    /// only says [`take`](Self::take) may have something.
    pub async fn changed(&self) {
        match &self.shared {
            Some(shared) => shared.notify.notified().await,
            None => std::future::pending().await,
        }
    }

    /// The requester's queued messages in this session, oldest first, without taking them:
    /// their rows stay `queued`, so a turn that ends before it [`take`](Self::take)s them (a
    /// `/stop`) leaves them for the next one. Empty when detached.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn peek(&self) -> Result<Vec<TurnMessage>, giles_store::Error> {
        let Some(shared) = &self.shared else {
            return Ok(Vec::new());
        };
        Ok(shared
            .store
            .inbox_queued(shared.session)
            .await?
            .iter()
            .filter(|entry| entry.user == shared.requester)
            .filter_map(TurnMessage::from_entry)
            .collect())
    }

    /// Takes the requester's queued messages in this session, oldest first, and makes them part
    /// of the turn. Empty when detached, nothing is queued, or the turn has ended.
    ///
    /// # Errors
    ///
    /// A database error; nothing was taken.
    pub async fn take(&self) -> Result<Vec<TurnMessage>, giles_store::Error> {
        let Some(shared) = &self.shared else {
            return Ok(Vec::new());
        };
        let closed = shared.closed.lock().await;
        if *closed {
            return Ok(Vec::new());
        }
        let queued: Vec<InboxEntry> = shared
            .store
            .inbox_queued(shared.session)
            .await?
            .into_iter()
            .filter(|entry| entry.user == shared.requester)
            .collect();
        if queued.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<InboxId> = queued.iter().map(|entry| entry.id).collect();
        shared.store.inbox_start(ids.clone()).await?;
        lock(&shared.rows).extend(ids);
        drop(closed);
        Ok(queued.iter().filter_map(TurnMessage::from_entry).collect())
    }
}

impl SteerShared {
    /// Called by the driver when the turn has ended: later `take` calls return nothing.
    pub(super) async fn close(&self) {
        *self.closed.lock().await = true;
    }
}

/// Sends into a session's conversation: replies (TASKS.md 2.8) and notices.
#[derive(Clone)]
pub struct Reply {
    channels: Arc<dyn ChannelLookup>,
    instance: String,
    conversation: ConversationId,
}

impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("instance", &self.instance)
            .field("conversation", &self.conversation)
            .finish_non_exhaustive()
    }
}

impl Reply {
    /// The conversation of `session`, or `None` if it has none (a job session).
    pub(crate) fn for_session(session: &Session, channels: Arc<dyn ChannelLookup>) -> Option<Self> {
        Some(Self {
            channels,
            instance: session.channel.clone()?,
            conversation: session.conversation.clone()?,
        })
    }

    /// The channel instance.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The conversation on that channel.
    pub fn conversation(&self) -> &ConversationId {
        &self.conversation
    }

    /// The channel registered for the instance right now (its `info().output` decides deltas or
    /// blocks). `None` while the instance has no registered channel, e.g. a plugin that hasn't
    /// started yet.
    pub fn channel(&self) -> Option<Arc<dyn Channel>> {
        self.channels.channel(&self.instance)
    }

    /// Sends `markdown` into the conversation.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] if the instance has no registered channel; otherwise the
    /// channel's error.
    pub async fn send(&self, markdown: impl Into<String>) -> giles_types::Result<MessageId> {
        let channel = self.channel().ok_or_else(|| {
            Error::unavailable(format!("channel `{}` is not available yet", self.instance))
        })?;
        channel
            .send(OutboundMessage {
                target: SendTarget::Conversation(self.conversation.clone()),
                markdown: markdown.into(),
                attachments: Vec::new(),
                reply_to: None,
            })
            .await
    }
}
