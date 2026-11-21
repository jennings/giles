//! The outbox: proactive sends (job replies, approval prompts of job turns, login codes,
//! notices), addressed to an identity or to a conversation (PLAN sections 6.11, 8.5). Design:
//! `Docs/technical/outbox.md`.
//!
//! [`Outbox::enqueue`] stores an [`OutboxItem`] in the `outbox` table and wakes the one sender
//! loop ([`Outbox::start`]), which sends each due row through its channel instance (with
//! [`SendTarget::Recipient`] to the instance owning an identity's namespace, or with
//! [`SendTarget::Conversation`] into a conversation), deletes it once the channel accepts it, and
//! retries it with backoff while the channel is down or answers
//! [`Unavailable`](doozers_types::Error::Unavailable). Every row expires: approval prompts and
//! login codes when their approval or code does, everything else [`DEFAULT_EXPIRY`] after it was
//! enqueued.

mod route;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use doozers_store::{NewOutbox, OutboxEntry, OutboxId, OutboxTarget, Store};
use doozers_types::{
    Channel, ConversationId, Error, MediaRef, NativeId, OutboundMessage, SendTarget, Timestamp,
    UserId,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn};

use crate::config::{Identity, InstanceName};
use crate::registry::Registry;
use crate::sessions::Clock;
use crate::users::User;

pub use route::{BACKOFF_INITIAL, BACKOFF_MAX, RETRY_AFTER_MAX};
use route::{Route, Slot, retry_delay, route};

/// How long a row without its own expiry is kept: 24 hours after it was enqueued (PLAN section
/// 6.11).
pub const DEFAULT_EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);

/// How long one send may take. A send that takes longer counts as a failed attempt and is
/// retried, so the message may arrive twice if the channel delivered it after all; the
/// instance's other due rows wait for the next pass rather than time out one after another.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(60);

/// Rows sent per pass of the sender loop; a full batch starts the next pass at once.
const BATCH: u32 = 32;

/// How long the sender waits after a database error before its next pass.
const STORE_RETRY: Duration = Duration::from_secs(5);

/// The longest the sender sleeps between passes, so a jump of the wall clock is caught up with.
const MAX_SLEEP: Duration = Duration::from_secs(5 * 60);

/// What a row is, for logs (the `kind` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OutboxKind {
    /// A job's reply, delivered to its user.
    JobReply,
    /// The approval prompt of a job turn with `ask_to = "deliver_to"` (PLAN section 6.4). Needs
    /// [`OutboxItem::expires_at`]: the approval's deadline. Pending approvals don't survive a
    /// restart, so [`Outbox::start`] drops every row of this kind.
    ApprovalPrompt,
    /// A web login code (PLAN section 3.4). Needs [`OutboxItem::expires_at`]: the code's expiry.
    LoginCode,
    /// Any other notice from the core.
    Notice,
}

impl OutboxKind {
    /// The name stored in the `kind` column and logged: `job_reply`, `approval_prompt`,
    /// `login_code`, or `notice`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::JobReply => "job_reply",
            Self::ApprovalPrompt => "approval_prompt",
            Self::LoginCode => "login_code",
            Self::Notice => "notice",
        }
    }

    /// Whether rows of this kind expire with something else and must say when.
    const fn needs_expiry(self) -> bool {
        matches!(self, Self::ApprovalPrompt | Self::LoginCode)
    }
}

impl fmt::Display for OutboxKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where an [`OutboxItem`] goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// An identity, `<namespace>:<native id>`, e.g. a user's default identity: the channel owning
    /// the namespace starts or continues a direct conversation.
    Identity(Identity),
    /// A conversation on a channel instance, replied into: a runtime job's reply to the
    /// conversation it was scheduled in.
    Conversation {
        /// The channel instance.
        instance: String,
        /// The conversation on it.
        conversation: ConversationId,
    },
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identity(identity) => write!(f, "{identity}"),
            Self::Conversation {
                instance,
                conversation,
            } => write!(f, "conversation {conversation} on `{instance}`"),
        }
    }
}

/// A proactive send, for [`Outbox::enqueue`].
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxItem {
    /// Where it goes.
    pub to: Destination,
    /// What it is.
    pub kind: OutboxKind,
    /// The user it's for, if any.
    pub user: Option<UserId>,
    /// The text, as CommonMark.
    pub markdown: String,
    /// Attachments in the blob store.
    pub attachments: Vec<MediaRef>,
    /// When it stops being worth sending. `None`: [`DEFAULT_EXPIRY`] after it's enqueued. Required
    /// for [`OutboxKind::ApprovalPrompt`] and [`OutboxKind::LoginCode`].
    pub expires_at: Option<Timestamp>,
}

impl OutboxItem {
    /// `markdown` to `to`, for no particular user, with no attachments, expiring after
    /// [`DEFAULT_EXPIRY`].
    pub fn new(to: Destination, kind: OutboxKind, markdown: impl Into<String>) -> Self {
        Self {
            to,
            kind,
            user: None,
            markdown: markdown.into(),
            attachments: Vec::new(),
            expires_at: None,
        }
    }

    /// `markdown` to `user` on their default identity (PLAN section 1.1), where jobs and
    /// proactive messages go. `None` if the user has no default identity.
    pub fn to_user(user: &User, kind: OutboxKind, markdown: impl Into<String>) -> Option<Self> {
        let identity = user.default_identity.clone()?;
        Some(Self {
            user: Some(user.id.clone()),
            ..Self::new(Destination::Identity(identity), kind, markdown)
        })
    }
}

/// [`Outbox::enqueue`] refused an item.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OutboxError {
    /// No configured channel instance can own the identity: no registered channel's namespace
    /// matches it, and no instance waiting to start has the component it names.
    #[error("no configured channel can send to `{0}`")]
    Unowned(Identity),
    /// The identity belongs to a channel that can't start a conversation
    /// (`can_initiate = false`).
    #[error("channel `{instance}` can't start a conversation with `{identity}`")]
    CannotInitiate {
        /// The channel instance.
        instance: InstanceName,
        /// The identity.
        identity: Identity,
    },
    /// The conversation is on a channel instance that isn't configured.
    #[error("no channel instance `{0}` is configured")]
    UnknownInstance(String),
    /// An approval prompt or login code came without `expires_at`.
    #[error("a {0} needs an expiry")]
    MissingExpiry(OutboxKind),
    /// `expires_at` isn't in the future.
    #[error("the {kind} for {to} expired before it was enqueued")]
    Expired {
        /// What it is.
        kind: OutboxKind,
        /// Where it was going.
        to: Destination,
    },
    /// The row couldn't be stored.
    #[error("cannot store the outbox row: {0}")]
    Store(#[from] doozers_store::Error),
}

/// The `payload` column.
#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    markdown: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<MediaRef>,
}

/// The outbox. Cheap to clone; every clone shares one sender loop.
#[derive(Clone)]
pub struct Outbox {
    inner: Arc<Inner>,
}

impl fmt::Debug for Outbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outbox")
            .field("store", &self.inner.store)
            .finish_non_exhaustive()
    }
}

struct Inner {
    store: Store,
    registry: Arc<Registry>,
    clock: Arc<dyn Clock>,
    /// Wakes the sender loop: something was enqueued.
    wake: Notify,
    tasks: TaskTracker,
}

impl Outbox {
    /// An outbox over `store`'s `outbox` table, sending through the channels registered in
    /// `registry` (looked up at each attempt, so a restarted plugin's channel is picked up).
    /// Nothing is sent until [`start`](Self::start); rows enqueued before then wait.
    pub fn new(store: Store, registry: Arc<Registry>, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                registry,
                clock,
                wake: Notify::new(),
                tasks: TaskTracker::new(),
            }),
        }
    }

    /// Stores `item` for delivery and wakes the sender. Returns the row's id, for
    /// [`remove`](Self::remove).
    ///
    /// For an identity, the row records the channel instance it routes to now (logged when the
    /// row is sent, dropped, or expires); the sender routes again at each attempt. A
    /// conversation's row is sent through its instance.
    ///
    /// # Errors
    ///
    /// [`OutboxError`]: nothing can ever deliver it (no channel owns the identity, its channel
    /// can't initiate, or the conversation's instance isn't configured), its expiry is missing or
    /// past, or the database failed.
    pub async fn enqueue(&self, item: OutboxItem) -> Result<OutboxId, OutboxError> {
        let now = self.inner.clock.now();
        let expires_at = match item.expires_at {
            Some(at) => at,
            None if item.kind.needs_expiry() => return Err(OutboxError::MissingExpiry(item.kind)),
            None => now.checked_add(DEFAULT_EXPIRY).unwrap_or(Timestamp::MAX),
        };
        if expires_at <= now {
            return Err(OutboxError::Expired {
                kind: item.kind,
                to: item.to,
            });
        }
        let channels = self.inner.channels();
        let (instance, target) = match &item.to {
            Destination::Identity(identity) => {
                let instance = match route(identity.as_str(), &slots(&channels)) {
                    Route::Send { instance, .. } => instance.to_string(),
                    Route::Wait { instances } => join(&instances),
                    Route::CannotInitiate { instance } => {
                        return Err(OutboxError::CannotInitiate {
                            instance: instance.clone(),
                            identity: identity.clone(),
                        });
                    }
                    Route::Unowned => return Err(OutboxError::Unowned(identity.clone())),
                };
                let recipient =
                    NativeId::new(identity.as_str()).expect("an identity is a non-empty string");
                (instance, OutboxTarget::Recipient(recipient))
            }
            Destination::Conversation {
                instance,
                conversation,
            } => {
                if !channels.iter().any(|c| c.instance.as_str() == instance) {
                    return Err(OutboxError::UnknownInstance(instance.clone()));
                }
                (
                    instance.clone(),
                    OutboxTarget::Conversation(conversation.clone()),
                )
            }
        };
        let payload = serde_json::to_value(Payload {
            markdown: item.markdown,
            attachments: item.attachments,
        })
        .map_err(doozers_store::Error::from)?;
        let id = self
            .inner
            .store
            .outbox_enqueue(NewOutbox {
                instance: instance.clone(),
                target,
                kind: item.kind.as_str().to_owned(),
                user: item.user,
                payload,
                created_at: now,
                expires_at,
            })
            .await?;
        debug!(
            id = %id,
            %instance,
            to = %item.to,
            kind = %item.kind,
            "outbox row enqueued",
        );
        self.inner.wake.notify_one();
        Ok(id)
    }

    /// Deletes row `id` before it's sent: what it announced is over, e.g. its approval was
    /// answered or expired. Returns `false` if the row was already gone (sent, expired, or
    /// removed). A send already under way still completes.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn remove(&self, id: OutboxId) -> Result<bool, doozers_store::Error> {
        let removed = self.inner.store.outbox_delete(id).await?;
        if removed {
            debug!(id = %id, "outbox row removed before it was sent");
        }
        Ok(removed)
    }

    /// Drops every approval prompt left from the previous process (its approval died with it;
    /// PLAN section 6.4), logging each, then starts the sender loop, which runs until `shutdown`
    /// is cancelled. Call it once.
    ///
    /// # Errors
    ///
    /// A database error while dropping the approval prompts; the loop doesn't start.
    pub async fn start(&self, shutdown: CancellationToken) -> Result<(), doozers_store::Error> {
        let stale = self
            .inner
            .store
            .outbox_expire_kind(OutboxKind::ApprovalPrompt.as_str())
            .await?;
        for row in &stale {
            warn!(
                id = %row.id,
                instance = row.instance,
                target = %row.target,
                kind = row.kind,
                attempts = row.attempts,
                "approval prompt dropped undelivered: its approval ended with the previous process",
            );
        }
        self.inner.tasks.spawn(self.inner.clone().run(shutdown));
        Ok(())
    }

    /// Waits until the sender loop has stopped. Call it after cancelling the shutdown token.
    pub async fn closed(&self) {
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
    }
}

/// What the sender does with a row after an attempt.
enum Outcome {
    Delivered,
    Retry {
        error: String,
        retry_after: Option<Duration>,
    },
    Drop {
        error: String,
    },
}

/// What a send's error means for its row: only a definite rejection of this message drops it;
/// anything else (the channel is down or restarting, a transport failure) may pass, so the row
/// is retried until it expires.
fn outcome_of(error: Error) -> Outcome {
    match error {
        Error::InvalidRequest(_) | Error::InvalidId(_) | Error::NotFound(_) => Outcome::Drop {
            error: error.to_string(),
        },
        Error::Unavailable {
            message,
            retry_after,
        } => Outcome::Retry {
            error: message,
            retry_after,
        },
        error => Outcome::Retry {
            error: error.to_string(),
            retry_after: None,
        },
    }
}

impl Inner {
    /// The sender loop: passes over the due rows, sleeping in between until the next row is due
    /// or expires, or something is enqueued. A pass is abandoned when `shutdown` is cancelled,
    /// even in the middle of a send: that row stays as it was, for the next start.
    async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        loop {
            let pause = tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                result = self.pass() => match result {
                    Ok(pause) => pause,
                    Err(error) => {
                        warn!(%error, "outbox pass failed; retrying");
                        STORE_RETRY
                    }
                },
            };
            if pause.is_zero() {
                continue;
            }
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                () = self.wake.notified() => {}
                () = tokio::time::sleep(pause) => {}
            }
        }
    }

    /// Drops expired rows, attempts due rows one by one (up to [`BATCH`]), and returns how long
    /// to sleep before the next pass: zero if the batch was full.
    ///
    /// Each row is fetched just before it's sent, with the time then, so a row removed
    /// ([`Outbox::remove`]) or expired while earlier ones were being sent is never sent.
    async fn pass(&self) -> Result<Duration, doozers_store::Error> {
        for row in self.store.outbox_expire(self.clock.now()).await? {
            warn!(
                id = %row.id,
                instance = row.instance,
                target = %row.target,
                kind = row.kind,
                attempts = row.attempts,
                last_error = row.last_error.as_deref(),
                "outbox row expired undelivered; dropped",
            );
        }
        // Instances whose send timed out in this pass: their other rows wait for the next pass
        // instead of each holding up the sender for another `SEND_TIMEOUT`.
        let mut wedged = HashSet::new();
        for _ in 0..BATCH {
            let Some(row) = self.store.outbox_due(self.clock.now(), 1).await?.pop() else {
                return Ok(match self.store.outbox_next_wakeup().await? {
                    Some(next) => until(self.clock.now(), next).min(MAX_SLEEP),
                    None => MAX_SLEEP,
                });
            };
            self.attempt(row, &mut wedged).await?;
        }
        Ok(Duration::ZERO)
    }

    /// Sends one row and records the outcome.
    async fn attempt(
        &self,
        row: OutboxEntry,
        wedged: &mut HashSet<String>,
    ) -> Result<(), doozers_store::Error> {
        let (instance, outcome) = self.send(&row, wedged).await;
        match outcome {
            Outcome::Delivered => {
                self.store.outbox_delete(row.id).await?;
                info!(
                    id = %row.id,
                    %instance,
                    target = %row.target,
                    kind = row.kind,
                    attempts = row.attempts + 1,
                    "outbox row delivered",
                );
            }
            Outcome::Drop { error } => {
                self.store.outbox_delete(row.id).await?;
                warn!(
                    id = %row.id,
                    %instance,
                    target = %row.target,
                    kind = row.kind,
                    attempts = row.attempts + 1,
                    %error,
                    "outbox row can't be delivered; dropped",
                );
            }
            Outcome::Retry { error, retry_after } => {
                let delay = retry_delay(row.attempts, retry_after);
                let next = self
                    .clock
                    .now()
                    .checked_add(delay)
                    .unwrap_or(row.expires_at);
                match self.store.outbox_retry(row.id, next, error.clone()).await {
                    Ok(attempts) => {
                        // The first failure is worth noting; the retries that follow aren't.
                        if attempts == 1 {
                            info!(id = %row.id, %instance, target = %row.target,
                                kind = row.kind, attempts, %error, ?delay,
                                "outbox send failed; will retry");
                        } else {
                            debug!(id = %row.id, %instance, target = %row.target,
                                kind = row.kind, attempts, %error, ?delay,
                                "outbox send failed; will retry");
                        }
                    }
                    // Removed during the send (its approval was resolved).
                    Err(doozers_store::Error::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    /// Routes and sends `row`, unless its instance is in `wedged`; adds the instance there when
    /// the send times out. Returns the instance it went to (or the stored one, if routing found
    /// none) and what to do with the row.
    async fn send(&self, row: &OutboxEntry, wedged: &mut HashSet<String>) -> (String, Outcome) {
        let payload: Payload = match serde_json::from_value(row.payload.clone()) {
            Ok(payload) => payload,
            Err(error) => {
                let error = format!("unreadable payload: {error}");
                return (row.instance.clone(), Outcome::Drop { error });
            }
        };
        let channels = self.channels();
        let (instance, target) = match &row.target {
            OutboxTarget::Recipient(identity) => {
                match route_identity(identity, &row.instance, &channels) {
                    Ok(routed) => routed,
                    Err(failed) => return failed,
                }
            }
            OutboxTarget::Conversation(conversation) => {
                if !channels.iter().any(|c| c.instance.as_str() == row.instance) {
                    let error = "the channel instance isn't configured".to_owned();
                    return (row.instance.clone(), Outcome::Drop { error });
                }
                (
                    row.instance.clone(),
                    SendTarget::Conversation(conversation.clone()),
                )
            }
        };
        if wedged.contains(&instance) {
            let error = format!("not tried: a send to channel `{instance}` just timed out");
            return (
                instance,
                Outcome::Retry {
                    error,
                    retry_after: None,
                },
            );
        }
        let Some(channel) = channels
            .iter()
            .find(|configured| configured.instance.as_str() == instance)
            .and_then(|configured| configured.channel.clone())
        else {
            // A conversation's instance whose plugin isn't up yet: wait for it. (`route` sends an
            // identity's row through registered channels only; this keeps it if that changes.)
            let error = format!("channel `{instance}` isn't running");
            return (
                instance,
                Outcome::Retry {
                    error,
                    retry_after: None,
                },
            );
        };
        let message = OutboundMessage {
            target,
            markdown: payload.markdown,
            attachments: payload.attachments,
            reply_to: None,
        };
        let outcome = match tokio::time::timeout(SEND_TIMEOUT, channel.send(message)).await {
            Ok(Ok(_)) => Outcome::Delivered,
            Ok(Err(error)) => outcome_of(error),
            Err(_) => {
                wedged.insert(instance.clone());
                Outcome::Retry {
                    error: format!("the send timed out after {SEND_TIMEOUT:?}"),
                    retry_after: None,
                }
            }
        };
        (instance, outcome)
    }

    /// Every configured channel instance, sorted by name, with its registered channel.
    fn channels(&self) -> Vec<Configured> {
        self.registry
            .config()
            .channels
            .values()
            .map(|instance| Configured {
                instance: instance.name.clone(),
                component: instance.component.clone(),
                channel: self.registry.channel(instance.name.as_str()),
            })
            .collect()
    }
}

/// A configured channel instance and its registered channel, if it's up.
struct Configured {
    instance: InstanceName,
    component: String,
    channel: Option<Arc<dyn Channel>>,
}

/// Where an identity's row goes now: its instance and target. When it can't be sent now, the
/// instance to log (`stored`, the row's, if routing found none) and what to do with the row.
fn route_identity(
    identity: &NativeId,
    stored: &str,
    channels: &[Configured],
) -> Result<(String, SendTarget), (String, Outcome)> {
    match route(identity.as_str(), &slots(channels)) {
        Route::Send {
            instance,
            recipient,
        } => Ok((
            instance.to_string(),
            SendTarget::Recipient(
                NativeId::new(recipient).expect("route never returns an empty native id"),
            ),
        )),
        Route::Wait { instances } => {
            let error = format!("channel `{}` isn't running", join(&instances));
            Err((
                join(&instances),
                Outcome::Retry {
                    error,
                    retry_after: None,
                },
            ))
        }
        Route::CannotInitiate { instance } => {
            let error = "the channel can't start conversations".to_owned();
            Err((instance.to_string(), Outcome::Drop { error }))
        }
        Route::Unowned => {
            let error = "no configured channel owns the identity".to_owned();
            Err((stored.to_owned(), Outcome::Drop { error }))
        }
    }
}

/// The routing view of [`Inner::channels`].
fn slots(channels: &[Configured]) -> Vec<Slot<'_>> {
    channels
        .iter()
        .map(|configured| Slot {
            instance: &configured.instance,
            component: &configured.component,
            info: configured.channel.as_deref().map(Channel::info),
        })
        .collect()
}

/// Instance names joined with `,`: the instances a row may go to, when several could.
fn join(instances: &[&InstanceName]) -> String {
    instances
        .iter()
        .map(|name| name.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// The time from `now` to `then`, rounded up to the millisecond (the store's precision), so the
/// sender never wakes just before a row is due; zero if `then` has passed.
fn until(now: Timestamp, then: Timestamp) -> Duration {
    let nanos = then.as_nanosecond() - now.as_nanosecond();
    u64::try_from(nanos)
        .map(|nanos| Duration::from_millis(nanos.div_ceil(1_000_000)))
        .unwrap_or(Duration::ZERO)
}
