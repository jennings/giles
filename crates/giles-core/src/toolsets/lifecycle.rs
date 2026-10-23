//! One toolset's lifecycle (PLAN sections 7.5, 7.6, 7.9): a supervisor task that starts the
//! server, restarts it with backoff, stops it when idle, and keeps the tools it offers current;
//! and [`ManagedToolset`], the [`Toolset`] the registry holds for the instance's whole life.
//!
//! Calls go through [`ManagedToolset::call`], which asks the supervisor for the connection, so a
//! stopped stdio server relaunches on the next call without the registry or the dispatcher
//! knowing. The tools offered are the supervisor's: the cached list until the server answers,
//! then each fresh list.

use std::process::ExitStatus;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use giles_types::{
    CallContext, Error, Result, Skill, SkillSummary, ToolCall, ToolCallHandle, ToolSpec, Toolset,
};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::cache::{CacheKey, ToolCache};
use crate::config::{InstanceName, ToolsetInstance, ToolsetTransport};

/// The first restart delay after a crash or a failed start (PLAN section 7.5).
pub const RESTART_BACKOFF_MIN: Duration = Duration::from_secs(1);

/// The longest restart delay: the backoff doubles up to this.
pub const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// How long a connection must last for the backoff to start over at [`RESTART_BACKOFF_MIN`].
pub const HEALTHY_RESET: Duration = Duration::from_secs(10 * 60);

/// Calls waiting for the supervisor before callers wait to send.
const REQUEST_BUFFER: usize = 16;

/// One connection to a toolset's server.
pub(crate) struct Connection {
    /// The connection.
    pub toolset: Arc<dyn Toolset>,
    /// The cache key's source: `sha256:<hex>` of the program that runs, or the URL.
    pub source: String,
    /// The version the server reported.
    pub server_version: Option<String>,
    /// Resolves when the connection ends, with a stdio server's exit status if it exited.
    pub closed: BoxFuture<'static, Option<ExitStatus>>,
}

/// Makes connections; the real one launches servers ([`super::ToolsetLauncher`]), tests use
/// fakes.
#[async_trait]
pub(crate) trait Connector: Send + Sync + 'static {
    /// The cache key's source of `instance` without starting it, for the cache lookup at boot.
    async fn source(&self, instance: &ToolsetInstance) -> Result<String>;

    /// Connects to `instance`, ready for calls.
    async fn connect(&self, instance: &ToolsetInstance) -> Result<Connection>;
}

/// A connection lent to one call, and the call's place in the in-flight count.
struct Lease {
    toolset: Arc<dyn Toolset>,
    _call: CallGuard,
}

type Reply = oneshot::Sender<Result<Lease>>;

/// Calls in flight and when the last one ended, for `idle_stop`.
struct Activity {
    state: Mutex<(usize, Instant)>,
    changed: Notify,
}

impl Activity {
    fn new() -> Self {
        Self {
            state: Mutex::new((0, Instant::now())),
            changed: Notify::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (usize, Instant)> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A call starts.
    fn begin(self: &Arc<Self>) -> CallGuard {
        self.lock().0 += 1;
        self.changed.notify_one();
        CallGuard {
            activity: self.clone(),
        }
    }

    /// The idle clock starts over (a new connection).
    fn reset(&self) {
        self.lock().1 = Instant::now();
    }

    /// Since when no call has been in flight; `None` while one is.
    fn idle_since(&self) -> Option<Instant> {
        let (calls, since) = *self.lock();
        (calls == 0).then_some(since)
    }
}

/// Ends a call's place in the in-flight count when dropped.
struct CallGuard {
    activity: Arc<Activity>,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        let mut state = self.activity.lock();
        state.0 -= 1;
        state.1 = Instant::now();
        drop(state);
        self.activity.changed.notify_one();
    }
}

/// The [`Toolset`] registered for a managed instance. Its tools are what the supervisor offers;
/// a call gets the supervisor's connection, starting a stopped stdio server first.
pub(crate) struct ManagedToolset {
    instance: InstanceName,
    tools: watch::Receiver<Vec<ToolSpec>>,
    requests: mpsc::Sender<Reply>,
    /// The instance's `skills` directories.
    skills: Vec<std::path::PathBuf>,
}

impl ManagedToolset {
    fn stopped(&self) -> Error {
        Error::unavailable(format!("toolset {}: Giles is stopping", self.instance))
    }

    /// The skills in the instance's `skills` directories, bare-named (a later directory
    /// overrides an earlier one), read on a blocking thread. Skipped skills are logged. The
    /// agent's skill index reads the same directories with the `<instance>__` prefix
    /// ([`Registry::skill_sources`](crate::registry::Registry::skill_sources)).
    async fn discover_skills(&self) -> Result<crate::skills::SkillSet> {
        let sources: Vec<_> = self
            .skills
            .iter()
            .map(crate::skills::SkillSource::local)
            .collect();
        let found = tokio::task::spawn_blocking(move || crate::skills::discover(&sources))
            .await
            .map_err(Error::other)?;
        for warning in &found.warnings {
            warn!(toolset = %self.instance, %warning, "skill skipped");
        }
        Ok(found.skills)
    }
}

#[async_trait]
impl Toolset for ManagedToolset {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(self.tools.borrow().clone())
    }

    /// Every list the supervisor offers from now on: a fresh list at each start and on each
    /// `notifications/tools/list_changed`, or none while a remote server is down.
    fn tool_changes(&self) -> Option<BoxStream<'static, Vec<ToolSpec>>> {
        let mut tools = self.tools.clone();
        tools.mark_unchanged();
        let changes = futures::stream::unfold(tools, |mut tools| async move {
            tools.changed().await.ok()?;
            let list = tools.borrow_and_update().clone();
            Some((list, tools))
        });
        Some(changes.boxed())
    }

    /// The skills of the instance's `skills` directories, with bare names; available whether or
    /// not the server is running.
    async fn list_skills(&self) -> Result<Vec<SkillSummary>> {
        Ok(self.discover_skills().await?.summaries())
    }

    /// One skill of the instance's `skills` directories by bare name, else
    /// [`Error::NotFound`].
    async fn get_skill(&self, name: &str) -> Result<Skill> {
        self.discover_skills()
            .await?
            .get(name)
            .cloned()
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "skill {name}: toolset {} has no such skill",
                    self.instance
                ))
            })
    }

    /// Gets the connection from the supervisor (relaunching an idle stdio server first) and
    /// calls it. The connection counts as busy, and isn't stopped for idleness, until the
    /// returned events are dropped. While the server is down the call fails as
    /// [`Error::Unavailable`] with the reason.
    async fn call(&self, call: ToolCall, ctx: CallContext) -> Result<ToolCallHandle> {
        let (reply, lease) = oneshot::channel();
        self.requests
            .send(reply)
            .await
            .map_err(|_| self.stopped())?;
        let lease = lease.await.map_err(|_| self.stopped())??;
        let handle = lease.toolset.call(call, ctx).await?;
        let events = handle.events.map(move |event| {
            let _held = &lease;
            event
        });
        Ok(ToolCallHandle {
            events: events.boxed(),
            answers: handle.answers,
        })
    }
}

/// A supervised instance: the [`ManagedToolset`] to register and the supervisor to spawn.
pub(crate) struct Managed {
    pub toolset: Arc<ManagedToolset>,
    pub supervisor: Supervisor,
}

impl Managed {
    /// Sets up `instance`, offering `cached` tools (from the cache) until the server answers.
    pub(crate) fn new(
        instance: ToolsetInstance,
        cached: Vec<ToolSpec>,
        connector: Arc<dyn Connector>,
        cache: ToolCache,
        shutdown: CancellationToken,
    ) -> Self {
        let (tools_tx, tools) = watch::channel(cached);
        let (requests_tx, requests) = mpsc::channel(REQUEST_BUFFER);
        let toolset = Arc::new(ManagedToolset {
            instance: instance.name.clone(),
            tools,
            requests: requests_tx,
            skills: instance.skills.clone(),
        });
        let (stdio, idle_stop) = match &instance.transport {
            ToolsetTransport::Stdio(server) => (true, server.idle_stop),
            ToolsetTransport::Http(_) => (false, None),
        };
        let supervisor = Supervisor {
            instance,
            stdio,
            idle_stop,
            connector,
            cache,
            tools: tools_tx,
            requests,
            activity: Arc::new(Activity::new()),
            shutdown,
            backoff: RESTART_BACKOFF_MIN,
        };
        Self {
            toolset,
            supervisor,
        }
    }
}

/// Where a supervised server is.
enum State {
    Running(Running),
    /// Stopped for idleness; the next call starts it.
    Idle,
    /// Not running after a failure; `retry_at` is the next start, if any.
    Down {
        retry_at: Option<Instant>,
        reason: String,
    },
    Stopped,
}

struct Running {
    toolset: Arc<dyn Toolset>,
    closed: BoxFuture<'static, Option<ExitStatus>>,
    changes: Option<BoxStream<'static, Vec<ToolSpec>>>,
    key: CacheKey,
    since: Instant,
}

/// The task that owns one instance's connection.
pub(crate) struct Supervisor {
    instance: ToolsetInstance,
    stdio: bool,
    idle_stop: Option<Duration>,
    connector: Arc<dyn Connector>,
    cache: ToolCache,
    tools: watch::Sender<Vec<ToolSpec>>,
    requests: mpsc::Receiver<Reply>,
    activity: Arc<Activity>,
    shutdown: CancellationToken,
    backoff: Duration,
}

impl Supervisor {
    /// Starts the server, signals `started` once that first attempt is over, and then keeps it
    /// going until shutdown. Runs under a `toolset{instance}` span.
    pub(crate) async fn run(mut self, started: oneshot::Sender<()>) {
        let shutdown = self.shutdown.clone();
        let mut state = tokio::select! {
            state = self.start() => state,
            () = shutdown.cancelled() => State::Stopped,
        };
        let _ = started.send(());
        loop {
            state = match state {
                State::Running(running) => self.running(running).await,
                State::Idle => self.idle().await,
                State::Down { retry_at, reason } => self.down(retry_at, &reason).await,
                State::Stopped => return,
            };
        }
    }

    fn name(&self) -> &InstanceName {
        &self.instance.name
    }

    /// Offers `tools` to new sessions, if they changed.
    fn offer(&self, tools: Vec<ToolSpec>) {
        self.tools.send_if_modified(|offered| {
            let changed = *offered != tools;
            *offered = tools;
            changed
        });
    }

    fn next_backoff(&mut self) -> Duration {
        let delay = self.backoff;
        self.backoff = (delay * 2).min(RESTART_BACKOFF_MAX);
        delay
    }

    /// Connects, lists the tools (offering and caching them), and returns the new state.
    async fn start(&mut self) -> State {
        let connection = match self.connector.connect(&self.instance).await {
            Ok(connection) => connection,
            Err(error) => return self.failed(&error),
        };
        // Subscribed before listing, so no change between the two is missed.
        let changes = connection.toolset.tool_changes();
        let key = CacheKey {
            source: connection.source,
            config_hash: self.instance.config_hash.to_string(),
            server_version: connection.server_version,
        };
        match connection.toolset.list_tools().await {
            Ok(tools) => {
                info!(
                    tools = tools.len(),
                    server_version = key.server_version.as_deref(),
                    "toolset started",
                );
                self.cache.store(self.name(), &key, &tools);
                self.offer(tools);
            }
            Err(error) => {
                warn!(%error, "toolset started, but listing its tools failed; offering the previous list");
            }
        }
        self.activity.reset();
        State::Running(Running {
            toolset: connection.toolset,
            closed: connection.closed,
            changes,
            key,
            since: Instant::now(),
        })
    }

    /// The state after a failed start: down, retrying with backoff unless retrying can't help.
    /// A remote server's tools are withdrawn (PLAN section 7.6); a stdio server's stay offered
    /// while it will be retried, as for a crash, and are withdrawn if it won't be.
    fn failed(&mut self, error: &Error) -> State {
        let reason = error.to_string();
        if error.is_retryable() {
            if !self.stdio {
                self.offer(Vec::new());
            }
            let delay = self.next_backoff();
            error!(%error, retry_in = ?delay, "cannot start the toolset; retrying");
            State::Down {
                retry_at: Some(Instant::now() + delay),
                reason,
            }
        } else {
            self.offer(Vec::new());
            error!(%error, "cannot start the toolset; not retrying until Giles restarts");
            State::Down {
                retry_at: None,
                reason,
            }
        }
    }

    /// When an idle stdio server stops: `idle_stop` after the last call ended (or the start), if
    /// no call is in flight.
    fn idle_deadline(&self) -> Option<Instant> {
        Some(self.activity.idle_since()? + self.idle_stop?)
    }

    fn lease(&self, toolset: &Arc<dyn Toolset>) -> Lease {
        Lease {
            toolset: toolset.clone(),
            _call: self.activity.begin(),
        }
    }

    async fn running(&mut self, mut running: Running) -> State {
        loop {
            let idle_at = self.idle_deadline();
            tokio::select! {
                () = self.shutdown.cancelled() => return State::Stopped,
                status = &mut running.closed => {
                    if running.since.elapsed() >= HEALTHY_RESET {
                        self.backoff = RESTART_BACKOFF_MIN;
                    }
                    let delay = self.next_backoff();
                    let reason = match status {
                        Some(status) => format!("its server exited ({status})"),
                        None => "the connection closed".to_owned(),
                    };
                    warn!(retry_in = ?delay, "toolset stopped: {reason}; restarting");
                    if !self.stdio {
                        self.offer(Vec::new());
                    }
                    return State::Down {
                        retry_at: Some(Instant::now() + delay),
                        reason,
                    };
                }
                tools = next_change(&mut running.changes) => {
                    info!(tools = tools.len(), "toolset's tools changed");
                    self.cache.store(self.name(), &running.key, &tools);
                    self.offer(tools);
                }
                Some(reply) = self.requests.recv() => {
                    let _ = reply.send(Ok(self.lease(&running.toolset)));
                }
                () = sleep_until(idle_at) => {
                    if self.idle_deadline().is_some_and(|at| at <= Instant::now()) {
                        info!(idle_stop = ?self.idle_stop, "stopping the idle toolset; its tools stay offered");
                        // It stopped without crashing: the next failure starts the backoff over.
                        self.backoff = RESTART_BACKOFF_MIN;
                        return State::Idle;
                    }
                }
                () = self.activity.changed.notified() => {}
            }
        }
    }

    async fn idle(&mut self) -> State {
        let shutdown = self.shutdown.clone();
        let reply = tokio::select! {
            () = shutdown.cancelled() => return State::Stopped,
            reply = self.requests.recv() => match reply {
                Some(reply) => reply,
                None => return State::Stopped,
            },
        };
        info!("relaunching the idle toolset for a call");
        let state = tokio::select! {
            state = self.start() => state,
            () = shutdown.cancelled() => State::Stopped,
        };
        let answer = match &state {
            State::Running(running) => Ok(self.lease(&running.toolset)),
            State::Down { reason, .. } => Err(self.not_running(reason, None)),
            _ => Err(self.not_running("Giles is stopping", None)),
        };
        let _ = reply.send(answer);
        state
    }

    async fn down(&mut self, retry_at: Option<Instant>, reason: &str) -> State {
        let shutdown = self.shutdown.clone();
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return State::Stopped,
                () = sleep_until(retry_at) => {
                    return tokio::select! {
                        state = self.start() => state,
                        () = shutdown.cancelled() => State::Stopped,
                    };
                }
                Some(reply) = self.requests.recv() => {
                    let retry_after = retry_at.map(|at| at.saturating_duration_since(Instant::now()));
                    let _ = reply.send(Err(self.not_running(reason, retry_after)));
                }
            }
        }
    }

    fn not_running(&self, reason: &str, retry_after: Option<Duration>) -> Error {
        Error::Unavailable {
            message: format!("toolset {} is not running: {reason}", self.name()),
            retry_after,
        }
    }
}

/// The next list from `changes`; pending forever once it ends (or if there is none).
async fn next_change(changes: &mut Option<BoxStream<'static, Vec<ToolSpec>>>) -> Vec<ToolSpec> {
    while let Some(stream) = changes {
        match stream.next().await {
            Some(tools) => return tools,
            None => *changes = None,
        }
    }
    std::future::pending().await
}

/// Sleeps until `at`; forever if `None`.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
