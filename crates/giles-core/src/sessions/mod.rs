//! Sessions and turns: the inbound pipeline from channel events to turns (PLAN sections 4.5,
//! 6.2, 6.3, 8.5). Design: `Docs/technical/sessions.md`.
//!
//! [`Sessions`] consumes every attached channel's event stream, classifies each message with the
//! [`Router`], stores it in the `inbox` (deduplicated on instance and event id) and only then
//! acks it, and runs at most one turn per session through a [`TurnRunner`]. Messages that arrive
//! while a turn runs stay `queued` in the inbox and start the next turn. At startup it recovers
//! from a restart: empty tool-result slots get `interrupted`, rows of turns that were running
//! become `unknown`, and each requester is told.

mod driver;
mod inbound;
mod recovery;
mod turn;

#[cfg(test)]
mod tests;

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use giles_store::{SessionId, SessionKey, Store};
use giles_types::{AgentId, Channel, Timestamp, UserId};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::commands::{ApprovalReply, NoPendingApprovals};
use crate::config::{AgentConfig, BusyMode, Config, InstanceName};
use crate::registry::Registry;
use crate::router::{Routed, Router};

pub use inbound::ChannelStatusRecord;
pub use turn::{Reply, Steering, Turn, TurnMessage, TurnOutcome, TurnRunner};

/// How long inbox rows are kept as dedupe keys after they were received (PLAN section 8.5).
pub const DEDUPE_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How often dedupe keys older than [`DEDUPE_RETENTION`] are pruned. The first prune runs at
/// startup.
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The synthetic result startup recovery puts in every empty tool-result slot (PLAN section 6.3).
pub const INTERRUPTED_RESULT: &str = "interrupted";

/// What startup recovery tells the requester of each turn a restart cut off (PLAN section 6.3).
pub const INTERRUPTED_NOTICE: &str = "Interrupted by a restart; resend if needed.";

/// The current time. [`SystemClock`] in the daemon; tests drive their own.
pub trait Clock: Send + Sync {
    /// Now.
    fn now(&self) -> Timestamp;
}

/// The system's wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// Finds the channel registered for an instance. Implemented by [`Registry`]. Looked up at each
/// use, so a channel a plugin restart replaced is picked up.
pub trait ChannelLookup: Send + Sync {
    /// The channel registered for instance `instance`, if any.
    fn channel(&self, instance: &str) -> Option<Arc<dyn Channel>>;
}

impl ChannelLookup for Registry {
    fn channel(&self, instance: &str) -> Option<Arc<dyn Channel>> {
        Registry::channel(self, instance)
    }
}

/// The approvals code (TASKS.md 2.7), as the inbound pipeline sees it: whether a `yes`/`no` is an
/// approval reply, and where a reply goes. [`NoPendingApprovals`] until 2.7 lands.
#[async_trait]
pub trait Approvals: Send + Sync {
    /// Whether `to.user` has an approval pending in session `to.session`; with `code`, one with
    /// that short code. See [`PendingApprovals`](crate::commands::PendingApprovals).
    fn is_pending(&self, to: &Routed, code: Option<&str>) -> bool;

    /// Delivers a reply. Called only after the reply's event is recorded and acked, once per
    /// event (redeliveries are deduplicated).
    async fn answer(&self, to: Routed, reply: ApprovalReply);
}

#[async_trait]
impl Approvals for NoPendingApprovals {
    fn is_pending(&self, _: &Routed, _: Option<&str>) -> bool {
        false
    }

    async fn answer(&self, to: Routed, _: ApprovalReply) {
        // Unreachable through `classify`, which returns approval replies only while pending.
        tracing::warn!(
            agent = %to.session.agent,
            channel = to.session.channel,
            "approval reply with no approval pending; ignored"
        );
    }
}

/// The per-agent settings the session code uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentPolicy {
    /// What happens to the requester's new messages while a turn runs.
    pub busy_mode: BusyMode,
    /// End a session that has been quiet this long when the next message arrives.
    pub idle_reset: Option<Duration>,
}

impl AgentPolicy {
    /// The policy `agent.toml` sets.
    pub fn of(agent: &AgentConfig) -> Self {
        Self {
            busy_mode: agent.busy_mode,
            idle_reset: agent.idle_reset,
        }
    }

    /// Every configured agent's policy.
    pub fn from_config(config: &Config) -> HashMap<AgentId, Self> {
        config
            .agents
            .iter()
            .map(|(id, agent)| (id.clone(), Self::of(agent)))
            .collect()
    }
}

/// What [`Sessions::start`] needs.
pub struct SessionsOptions {
    /// The database.
    pub store: Store,
    /// Classifies inbound messages.
    pub router: Router,
    /// Per-agent settings. An agent missing here (a session left over from an agent that was
    /// removed from the configuration) gets the defaults.
    pub agents: HashMap<AgentId, AgentPolicy>,
    /// Where replies and restart notices are sent.
    pub channels: Arc<dyn ChannelLookup>,
    /// Runs turns: the agent loop (TASKS.md 2.4).
    pub runner: Arc<dyn TurnRunner>,
    /// Pending approvals and where replies go.
    pub approvals: Arc<dyn Approvals>,
    /// The time source.
    pub clock: Arc<dyn Clock>,
}

impl fmt::Debug for SessionsOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionsOptions")
            .field("store", &self.store)
            .field("router", &self.router)
            .field("agents", &self.agents)
            .finish_non_exhaustive()
    }
}

/// The session manager: channel event pumps, the per-session turn queue, `/new` and `/stop`,
/// startup recovery, dedupe-key pruning, and the latest status of each channel. Cheap to clone.
#[derive(Clone)]
pub struct Sessions {
    inner: Arc<Inner>,
}

impl fmt::Debug for Sessions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sessions")
            .field("live", &lock(&self.inner.live).len())
            .field(
                "channels",
                &lock(&self.inner.pumps).keys().collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl Sessions {
    /// Runs startup recovery (PLAN section 6.3), resumes every session with queued inbox rows,
    /// and starts pruning dedupe keys. Attach channels afterwards with
    /// [`attach_channel`](Self::attach_channel). Everything stops when `shutdown` is cancelled:
    /// running turns are cancelled and their inbox rows stay `started`, so the next start
    /// reports them as interrupted.
    ///
    /// # Errors
    ///
    /// A database error during recovery.
    pub async fn start(
        options: SessionsOptions,
        shutdown: CancellationToken,
    ) -> Result<Self, giles_store::Error> {
        let inner = Arc::new(Inner {
            store: options.store,
            router: options.router,
            agents: options.agents,
            channels: options.channels,
            runner: options.runner,
            approvals: options.approvals,
            clock: options.clock,
            shutdown,
            live: Mutex::default(),
            notices: Mutex::default(),
            pumps: Mutex::default(),
            statuses: Mutex::default(),
            tasks: TaskTracker::new(),
        });
        recovery::recover(&inner).await?;
        recovery::resume(&inner).await?;
        inner.tasks.spawn(recovery::prune_loop(inner.clone()));
        Ok(Self { inner })
    }

    /// Starts consuming `channel`'s events as instance `instance` of component `component`
    /// (the namespace check in [`Router::classify`] needs it), and sends the restart notices
    /// recovery couldn't deliver to that instance. Attaching an instance again (a restarted
    /// plugin) stops the earlier stream after its current event. The stream is reopened with
    /// backoff whenever it fails or ends; resending unacked events is the channel's job (PLAN
    /// section 8.5).
    pub fn attach_channel(
        &self,
        instance: InstanceName,
        component: String,
        channel: Arc<dyn Channel>,
    ) {
        let stop = self.inner.shutdown.child_token();
        if let Some(old) = lock(&self.inner.pumps).insert(instance.clone(), stop.clone()) {
            old.cancel();
        }
        recovery::send_pending_notices(&self.inner, instance.as_str(), channel.clone());
        self.inner.tasks.spawn(inbound::pump(
            self.inner.clone(),
            instance,
            component,
            channel,
            stop,
        ));
    }

    /// The latest status event of channel instance `instance`, if it sent one since startup.
    pub fn channel_status(&self, instance: &str) -> Option<ChannelStatusRecord> {
        lock(&self.inner.statuses).get(instance).cloned()
    }

    /// The latest status event of every channel that sent one since startup.
    pub fn channel_statuses(&self) -> BTreeMap<InstanceName, ChannelStatusRecord> {
        lock(&self.inner.statuses).clone()
    }

    /// `/stop`: cancels the turn running for `key`, in its open session or one `/new` or an idle
    /// reset ended while the turn ran. Returns whether a turn was running.
    pub fn stop(&self, key: &SessionKey) -> bool {
        self.inner.stop(key)
    }

    /// Whether a turn is running in `session`.
    pub fn is_running(&self, session: SessionId) -> bool {
        lock(&self.inner.live)
            .get(&session)
            .is_some_and(|live| live.turn.is_some())
    }

    /// Waits until every task of the session manager has stopped: channel pumps, drivers, turns,
    /// and the prune task. Call it after cancelling the shutdown token; it waits as long as a
    /// runner ignores [`Turn::cancel`].
    pub async fn closed(&self) {
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
    }
}

/// Locks a std mutex, ignoring poisoning: the guarded maps stay consistent across a panic
/// because every update is a single insert or remove.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Inner {
    store: Store,
    router: Router,
    agents: HashMap<AgentId, AgentPolicy>,
    channels: Arc<dyn ChannelLookup>,
    runner: Arc<dyn TurnRunner>,
    approvals: Arc<dyn Approvals>,
    clock: Arc<dyn Clock>,
    shutdown: CancellationToken,
    /// Sessions with a driver task: one is running a turn or about to look for queued rows.
    live: Mutex<HashMap<SessionId, Live>>,
    /// Restart notices not delivered yet, by channel instance: sent when it's attached.
    notices: Mutex<HashMap<String, Vec<recovery::Notice>>>,
    /// The stop token of each attached channel's pump.
    pumps: Mutex<HashMap<InstanceName, CancellationToken>>,
    statuses: Mutex<BTreeMap<InstanceName, ChannelStatusRecord>>,
    /// Every task spawned, so shutdown can wait for them.
    tasks: TaskTracker,
}

/// A session with a driver task.
#[derive(Default)]
struct Live {
    /// Set by [`Inner::wake`]; the driver clears it before reading the queue and exits only if
    /// it's still clear when the queue is empty, so no wake is lost.
    dirty: bool,
    /// The session's key (unset for job sessions, and until the driver has loaded the session).
    key: Option<SessionKey>,
    /// The running turn.
    turn: Option<LiveTurn>,
}

struct LiveTurn {
    cancel: CancellationToken,
    requester: Option<UserId>,
    /// Woken when the requester sends a message during the turn.
    steer: Arc<tokio::sync::Notify>,
}

impl Inner {
    fn policy(&self, agent: &AgentId) -> AgentPolicy {
        self.agents.get(agent).copied().unwrap_or_default()
    }

    /// A row was queued for `session` (by `from`, if known): start a driver unless one is live,
    /// and tell a running turn of its requester's new message.
    fn wake(self: &Arc<Self>, session: SessionId, from: Option<&UserId>) {
        if self.shutdown.is_cancelled() {
            return;
        }
        match lock(&self.live).entry(session) {
            Entry::Occupied(mut entry) => {
                let live = entry.get_mut();
                live.dirty = true;
                if let Some(turn) = &live.turn
                    && from.is_some()
                    && turn.requester.as_ref() == from
                {
                    turn.steer.notify_one();
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(Live {
                    dirty: true,
                    ..Live::default()
                });
                self.tasks.spawn(driver::drive(self.clone(), session));
            }
        }
    }

    /// Whether `session` has a driver (a running turn or queued rows about to start one).
    fn is_live(&self, session: SessionId) -> bool {
        lock(&self.live).contains_key(&session)
    }

    fn stop(&self, key: &SessionKey) -> bool {
        let mut stopped = false;
        for live in lock(&self.live).values() {
            if live.key.as_ref() == Some(key)
                && let Some(turn) = &live.turn
            {
                turn.cancel.cancel();
                stopped = true;
            }
        }
        stopped
    }
}
