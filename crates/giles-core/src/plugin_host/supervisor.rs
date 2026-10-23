//! Supervision (PLAN sections 7.5 and 7.6): one task per plugin channel that launches it (or
//! connects to it, for a remote plugin) and registers its channel, checks its health every
//! [`HEALTH_INTERVAL`], relaunches it with backoff when it crashes or fails a check, waits for a
//! new binary after a failure that won't pass, and stops it at shutdown. Its state is kept for
//! `giles status` ([`PluginStatus`]).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use giles_types::Timestamp;
use tokio::sync::{Notify, oneshot};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{Instrument as _, error, info, warn};

use super::describe_cache::{DescribeCache, DescribeInfo};
use crate::config::{ConfigHash, InstanceName};

/// How often a running plugin's health is checked (PLAN section 7.5).
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(30);

/// How long a health check may take.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

/// The first delay before relaunching a plugin; it doubles after each failure.
pub const FIRST_BACKOFF: Duration = Duration::from_secs(1);

/// The longest delay before relaunching a plugin.
pub const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// How long a plugin must run for its backoff to start again from [`FIRST_BACKOFF`].
pub const HEALTHY_RESET: Duration = Duration::from_secs(10 * 60);

/// How often the binary of a plugin that failed for good is hashed again, to relaunch it once
/// it changes.
pub const BINARY_RECHECK: Duration = Duration::from_secs(60);

/// Where a supervised plugin is in its lifecycle (PLAN section 7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginState {
    /// Being launched.
    Starting,
    /// Up, registered, and healthy at the last check.
    Running {
        /// Its process id; `None` for a remote plugin, which the core doesn't run.
        pid: Option<u32>,
        /// When it came up.
        since: Timestamp,
    },
    /// Down after a failure, to be relaunched at `until`.
    BackingOff {
        /// When the next launch starts.
        until: Timestamp,
    },
    /// Down after a failure relaunching won't fix (a sandbox failure, no shared protocol, a
    /// rejected config, ...): relaunched when its binary changes or on request.
    Failed,
    /// Stopped at shutdown.
    Stopped,
}

/// What supervision knows about one plugin instance, for `giles status` and the admin service.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginStatus {
    /// The instance.
    pub instance: InstanceName,
    /// Its component, `<name>` of `giles-channel-<name>`.
    pub component: String,
    /// The binary found for it at the last launch (symlinks resolved); `None` before the first
    /// lookup or when there is none.
    pub binary: Option<PathBuf>,
    /// Where it is.
    pub state: PluginState,
    /// Relaunches since the core started: after crashes, failed health checks, failed launches,
    /// and on request.
    pub restarts: u32,
    /// The last launch failure, crash, or failed health check, kept after a recovery.
    pub last_error: Option<String>,
    /// What it reported in `Describe`: from the running plugin, or from the cache (PLAN section
    /// 7.9) for the binary and config at hand while it isn't running.
    pub describe: Option<DescribeInfo>,
    /// Whether [`describe`](Self::describe) came from the cache.
    pub describe_cached: bool,
}

/// A launch that failed.
#[derive(Debug)]
pub(crate) struct LaunchFailure {
    /// Safe to log.
    pub message: String,
    /// Whether relaunching with backoff may help.
    pub retryable: bool,
}

/// One plugin instance's launches, as supervision sees them.
#[async_trait]
pub(crate) trait Launch: Send + Sync {
    /// The binary a launch would run now and its SHA-256, or `None` if there is none (or it
    /// can't be read).
    async fn binary(&self) -> Option<(PathBuf, [u8; 32])>;

    /// Launches the plugin and registers its channel.
    async fn launch(&self) -> Result<Box<dyn Running>, LaunchFailure>;
}

/// A launched plugin, as supervision sees it.
#[async_trait]
pub(crate) trait Running: Send {
    /// Its process id; `None` for a remote plugin.
    fn pid(&self) -> Option<u32>;

    /// What it reported in `Describe`.
    fn describe(&self) -> Option<DescribeInfo>;

    /// Resolves once the process has exited (and the rest of its process group is killed),
    /// saying how. Cancel-safe.
    async fn exited(&mut self) -> String;

    /// One `grpc.health.v1` check of its channel service; the error says why it failed.
    async fn check_health(&self) -> Result<(), String>;

    /// Stops it: `Shutdown`, then `SIGTERM`, then `SIGKILL` to its process group.
    async fn stop(self: Box<Self>);
}

/// Relaunch delays: [`FIRST_BACKOFF`], doubling up to [`MAX_BACKOFF`].
#[derive(Debug)]
struct Backoff {
    next: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self {
            next: FIRST_BACKOFF,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = (delay * 2).min(MAX_BACKOFF);
        delay
    }

    fn reset(&mut self) {
        self.next = FIRST_BACKOFF;
    }
}

/// The supervised instances and their tasks.
#[derive(Debug)]
pub(crate) struct Supervision {
    cache: DescribeCache,
    entries: Mutex<BTreeMap<InstanceName, Entry>>,
    stop: CancellationToken,
    tasks: TaskTracker,
}

#[derive(Debug)]
struct Entry {
    status: Arc<Mutex<PluginStatus>>,
    restart: Arc<Notify>,
}

impl Supervision {
    pub(crate) fn new(cache: DescribeCache) -> Self {
        Self {
            cache,
            entries: Mutex::default(),
            stop: CancellationToken::new(),
            tasks: TaskTracker::new(),
        }
    }

    /// Starts supervising `instance` (of `component`, config hash `config_hash`), unless it is
    /// supervised already or supervision has stopped. The receiver completes after the first
    /// launch attempt, whatever its outcome (or when supervision stops first).
    pub(crate) fn supervise(
        &self,
        instance: InstanceName,
        component: String,
        config_hash: ConfigHash,
        launch: Arc<dyn Launch>,
    ) -> oneshot::Receiver<()> {
        let (first, attempted) = oneshot::channel();
        let mut entries = lock(&self.entries);
        if entries.contains_key(&instance) || self.stop.is_cancelled() {
            return attempted;
        }
        let status = Arc::new(Mutex::new(PluginStatus {
            instance: instance.clone(),
            component,
            binary: None,
            state: PluginState::Starting,
            restarts: 0,
            last_error: None,
            describe: None,
            describe_cached: false,
        }));
        let restart = Arc::new(Notify::new());
        entries.insert(
            instance.clone(),
            Entry {
                status: Arc::clone(&status),
                restart: Arc::clone(&restart),
            },
        );
        // Launches log under their own `plugin{instance=...}` span inside this one.
        let span = tracing::info_span!("supervisor", instance = %instance);
        let supervisor = Supervisor {
            instance,
            config_hash,
            launch,
            cache: self.cache.clone(),
            status,
            restart,
            stop: self.stop.child_token(),
        };
        self.tasks.spawn(supervisor.run(first).instrument(span));
        attempted
    }

    pub(crate) fn statuses(&self) -> Vec<PluginStatus> {
        lock(&self.entries)
            .values()
            .map(|entry| lock(&entry.status).clone())
            .collect()
    }

    pub(crate) fn status(&self, instance: &str) -> Option<PluginStatus> {
        let entries = lock(&self.entries);
        let entry = entries
            .iter()
            .find(|(name, _)| name.as_str() == instance)?
            .1;
        Some(lock(&entry.status).clone())
    }

    /// Asks the supervisor of `instance` to relaunch it now: stopping it if it runs, skipping
    /// the backoff or the wait for a new binary otherwise. A request during a launch takes
    /// effect once that launch is up. `false` if `instance` isn't supervised.
    pub(crate) fn restart(&self, instance: &str) -> bool {
        let entries = lock(&self.entries);
        match entries.iter().find(|(name, _)| name.as_str() == instance) {
            Some((_, entry)) => {
                entry.restart.notify_one();
                true
            }
            None => false,
        }
    }

    /// Stops every supervisor, each stopping its plugin, and waits for them.
    pub(crate) async fn stop_all(&self) {
        self.stop.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

/// How a run of a launched plugin ended.
enum Ended {
    /// Shutdown: the plugin is stopped.
    Stopped,
    /// A restart was requested: the plugin is stopped.
    Restart,
    /// It crashed or failed a health check (and is stopped), after running for `ran`.
    Failed { reason: String, ran: Duration },
}

/// The task supervising one instance.
struct Supervisor {
    instance: InstanceName,
    config_hash: ConfigHash,
    launch: Arc<dyn Launch>,
    cache: DescribeCache,
    status: Arc<Mutex<PluginStatus>>,
    restart: Arc<Notify>,
    stop: CancellationToken,
}

impl Supervisor {
    fn update(&self, change: impl FnOnce(&mut PluginStatus)) {
        change(&mut lock(&self.status));
    }

    /// Looks up the binary and, while the plugin isn't running, shows its cached `Describe`.
    async fn find_binary(&self) -> Option<[u8; 32]> {
        let found = self.launch.binary().await;
        let cached = found
            .as_ref()
            .and_then(|(_, hash)| self.cache.load(&self.instance, hash, &self.config_hash));
        self.update(|status| {
            status.binary = found.as_ref().map(|(path, _)| path.clone());
            status.describe_cached = cached.is_some();
            status.describe = cached;
        });
        found.map(|(_, hash)| hash)
    }

    async fn run(self, first: oneshot::Sender<()>) {
        let mut first = Some(first);
        let mut backoff = Backoff::new();
        let mut launches: u32 = 0;
        loop {
            let binary = self.find_binary().await;
            if launches > 0 {
                self.update(|status| status.restarts += 1);
            }
            launches += 1;
            self.update(|status| status.state = PluginState::Starting);
            let launched = tokio::select! {
                biased;
                () = self.stop.cancelled() => return self.stopped(),
                launched = self.launch.launch() => launched,
            };
            if let Some(first) = first.take() {
                let _ = first.send(());
            }
            let (failure, crashed) = match launched {
                Ok(plugin) => match self.watch(plugin, binary).await {
                    Ended::Stopped => return self.stopped(),
                    Ended::Restart => {
                        backoff.reset();
                        continue;
                    }
                    Ended::Failed { reason, ran } => {
                        if ran >= HEALTHY_RESET {
                            backoff.reset();
                        }
                        let failure = LaunchFailure {
                            message: reason,
                            retryable: true,
                        };
                        (failure, true)
                    }
                },
                Err(failure) => (failure, false),
            };
            self.update(|status| status.last_error = Some(failure.message.clone()));
            let waited = if failure.retryable {
                let delay = backoff.next_delay();
                if crashed {
                    warn!(error = %failure.message, retry_in = ?delay, "plugin down; relaunching after a delay");
                } else {
                    warn!(error = %failure.message, retry_in = ?delay, "cannot launch the plugin; retrying after a delay");
                }
                self.update(|status| {
                    status.state = PluginState::BackingOff {
                        until: later(delay),
                    }
                });
                self.wait(tokio::time::sleep(delay)).await
            } else {
                error!(
                    error = %failure.message,
                    "cannot launch the plugin; not retrying until its binary changes",
                );
                self.update(|status| status.state = PluginState::Failed);
                let changed = self.binary_change(binary);
                let waited = self.wait(changed).await;
                backoff.reset();
                waited
            };
            match waited {
                Wait::Stopped => return self.stopped(),
                Wait::Restart => backoff.reset(),
                Wait::Done => {}
            }
        }
    }

    /// Runs a launched plugin until shutdown, a restart request, a crash, or a failed health
    /// check.
    async fn watch(&self, mut plugin: Box<dyn Running>, binary: Option<[u8; 32]>) -> Ended {
        let pid = plugin.pid();
        let describe = plugin.describe();
        if let (Some(describe), Some(binary)) = (&describe, &binary)
            && let Err(error) =
                self.cache
                    .store(&self.instance, binary, &self.config_hash, describe)
        {
            warn!(%error, "cannot write the Describe cache");
        }
        self.update(|status| {
            status.state = PluginState::Running {
                pid,
                since: Timestamp::now(),
            };
            status.describe = describe;
            status.describe_cached = false;
        });
        info!(pid, "plugin running");
        let started = Instant::now();
        let mut health = tokio::time::interval_at(started + HEALTH_INTERVAL, HEALTH_INTERVAL);
        health.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = self.stop.cancelled() => {
                    plugin.stop().await;
                    return Ended::Stopped;
                }
                () = self.restart.notified() => {
                    info!("restarting the plugin on request");
                    plugin.stop().await;
                    return Ended::Restart;
                }
                how = plugin.exited() => {
                    return Ended::Failed {
                        reason: format!("the plugin {how}"),
                        ran: started.elapsed(),
                    };
                }
                _ = health.tick() => {
                    if let Err(reason) = plugin.check_health().await {
                        warn!(%reason, "health check failed; stopping the plugin");
                        plugin.stop().await;
                        return Ended::Failed {
                            reason: format!("health check failed: {reason}"),
                            ran: started.elapsed(),
                        };
                    }
                }
            }
        }
    }

    /// Resolves once the binary's hash differs from `failed` (the binary of the failed launch),
    /// checking every [`BINARY_RECHECK`].
    async fn binary_change(&self, failed: Option<[u8; 32]>) {
        loop {
            tokio::time::sleep(BINARY_RECHECK).await;
            let now = self.find_binary().await;
            if now != failed {
                info!("the plugin binary changed; launching it again");
                return;
            }
        }
    }

    /// Waits for `until`, a restart request, or shutdown.
    async fn wait(&self, until: impl Future<Output = ()>) -> Wait {
        tokio::select! {
            biased;
            () = self.stop.cancelled() => Wait::Stopped,
            () = self.restart.notified() => {
                info!("relaunching the plugin on request");
                Wait::Restart
            }
            () = until => Wait::Done,
        }
    }

    fn stopped(&self) {
        self.update(|status| status.state = PluginState::Stopped);
    }
}

/// How a wait between launches ended.
enum Wait {
    Stopped,
    Restart,
    Done,
}

/// The wall-clock time `delay` from now, for display.
fn later(delay: Duration) -> Timestamp {
    let delay = jiff::SignedDuration::try_from(delay).unwrap_or(jiff::SignedDuration::MAX);
    Timestamp::now()
        .checked_add(delay)
        .unwrap_or(Timestamp::MAX)
}

/// Locks a std mutex, ignoring poisoning: every update is a single assignment.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
