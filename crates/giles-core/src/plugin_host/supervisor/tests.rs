//! Supervision against fake launches, in paused Tokio time. The echo plugin's end-to-end
//! supervision tests are in `plugins/giles-channel-echo/tests/supervision.rs`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use giles_types::OutputMode;
use tokio::sync::mpsc;

use super::*;
use crate::paths::DataRoot;
use crate::plugin_host::describe_cache::RequestedGrants;

/// What the next launch does.
#[derive(Debug, Clone, Copy)]
enum Plan {
    /// It fails.
    Fail { retryable: bool },
    /// It comes up, and the process exits after the duration (never with `None`).
    Up(Option<Duration>),
}

/// A launch, as the test sees it.
#[derive(Debug)]
struct Launched {
    at: Instant,
    /// The pid, or `None` for a failed launch.
    pid: Option<u32>,
}

struct FakeLaunch {
    /// Plans for the next launches; once empty, launches come up and stay up.
    plans: Mutex<VecDeque<Plan>>,
    binary: Mutex<Option<[u8; 32]>>,
    launched: mpsc::UnboundedSender<Launched>,
    stopped: mpsc::UnboundedSender<u32>,
    healthy: Arc<AtomicBool>,
    health_checks: Arc<AtomicU32>,
    pids: AtomicU32,
}

#[async_trait]
impl Launch for FakeLaunch {
    async fn binary(&self) -> Option<(PathBuf, [u8; 32])> {
        let hash = (*lock(&self.binary))?;
        Some((PathBuf::from("/plugins/giles-channel-irc"), hash))
    }

    async fn launch(&self) -> Result<Box<dyn Running>, LaunchFailure> {
        let plan = lock(&self.plans).pop_front().unwrap_or(Plan::Up(None));
        match plan {
            Plan::Fail { retryable } => {
                let _ = self.launched.send(Launched {
                    at: Instant::now(),
                    pid: None,
                });
                Err(LaunchFailure {
                    message: "the plugin exited before the handshake".into(),
                    retryable,
                })
            }
            Plan::Up(exit_after) => {
                let pid = self.pids.fetch_add(1, Ordering::SeqCst) + 1;
                let _ = self.launched.send(Launched {
                    at: Instant::now(),
                    pid: Some(pid),
                });
                Ok(Box::new(FakeRun {
                    pid,
                    exit_at: exit_after.map(|after| Instant::now() + after),
                    healthy: Arc::clone(&self.healthy),
                    health_checks: Arc::clone(&self.health_checks),
                    stopped: self.stopped.clone(),
                }))
            }
        }
    }
}

struct FakeRun {
    pid: u32,
    exit_at: Option<Instant>,
    healthy: Arc<AtomicBool>,
    health_checks: Arc<AtomicU32>,
    stopped: mpsc::UnboundedSender<u32>,
}

fn info(version: &str) -> DescribeInfo {
    DescribeInfo {
        version: version.into(),
        output: OutputMode::Blocks,
        can_initiate: false,
        requested_grants: RequestedGrants::default(),
    }
}

#[async_trait]
impl Running for FakeRun {
    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn describe(&self) -> Option<DescribeInfo> {
        Some(info("1.0"))
    }

    async fn exited(&mut self) -> String {
        match self.exit_at {
            Some(at) => {
                tokio::time::sleep_until(at).await;
                "exited (exit status: 1)".into()
            }
            None => std::future::pending().await,
        }
    }

    async fn check_health(&self) -> Result<(), String> {
        self.health_checks.fetch_add(1, Ordering::SeqCst);
        if self.healthy.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("giles.channel.v1.ChannelService is NOT_SERVING".into())
        }
    }

    async fn stop(self: Box<Self>) {
        let _ = self.stopped.send(self.pid);
    }
}

struct Harness {
    supervision: Supervision,
    fake: Arc<FakeLaunch>,
    launched: mpsc::UnboundedReceiver<Launched>,
    stopped: mpsc::UnboundedReceiver<u32>,
    root: DataRoot,
    _dir: tempfile::TempDir,
}

/// Longer than any wait in these tests, in paused time.
const NEVER: Duration = Duration::from_secs(24 * 60 * 60);

impl Harness {
    fn new(plans: &[Plan]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("home"), dir.path()).unwrap();
        Self::with_root(plans, root, dir)
    }

    fn with_root(plans: &[Plan], root: DataRoot, dir: tempfile::TempDir) -> Self {
        let (launched_tx, launched) = mpsc::unbounded_channel();
        let (stopped_tx, stopped) = mpsc::unbounded_channel();
        let fake = Arc::new(FakeLaunch {
            plans: Mutex::new(plans.iter().copied().collect()),
            binary: Mutex::new(Some([1; 32])),
            launched: launched_tx,
            stopped: stopped_tx,
            healthy: Arc::new(AtomicBool::new(true)),
            health_checks: Arc::new(AtomicU32::new(0)),
            pids: AtomicU32::new(0),
        });
        Self {
            supervision: Supervision::new(DescribeCache::new(&root)),
            fake,
            launched,
            stopped,
            root,
            _dir: dir,
        }
    }

    fn start(&self) -> oneshot::Receiver<()> {
        self.supervision.supervise(
            InstanceName::new("irc").unwrap(),
            "irc".into(),
            config_hash(),
            Arc::clone(&self.fake) as Arc<dyn Launch>,
        )
    }

    async fn next_launch(&mut self) -> Launched {
        tokio::time::timeout(NEVER, self.launched.recv())
            .await
            .expect("a launch")
            .unwrap()
    }

    async fn next_stop(&mut self) -> u32 {
        tokio::time::timeout(NEVER, self.stopped.recv())
            .await
            .expect("a stop")
            .unwrap()
    }

    fn status(&self) -> PluginStatus {
        self.supervision.status("irc").unwrap()
    }

    /// Lets the supervisor act on what just happened.
    async fn settle(&self) {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }
}

fn config_hash() -> ConfigHash {
    ConfigHash::of(&toml::Table::new())
}

fn secs(gaps: &[Duration]) -> Vec<u64> {
    gaps.iter().map(Duration::as_secs).collect()
}

#[test]
fn backoff_doubles_to_the_cap_and_resets() {
    let mut backoff = Backoff::new();
    let delays: Vec<Duration> = (0..11).map(|_| backoff.next_delay()).collect();
    assert_eq!(secs(&delays), [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
    backoff.reset();
    assert_eq!(backoff.next_delay(), FIRST_BACKOFF);
}

#[tokio::test(start_paused = true)]
async fn crashes_relaunch_with_backoff_up_to_five_minutes() {
    let mut h = Harness::new(&[Plan::Up(Some(Duration::ZERO)); 12]);
    h.start();
    let mut times = Vec::new();
    for _ in 0..12 {
        times.push(h.next_launch().await.at);
    }
    let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(secs(&gaps), [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
    h.settle().await;
    let status = h.status();
    assert_eq!(status.restarts, 11);
    assert!(
        matches!(status.state, PluginState::BackingOff { .. }),
        "{status:?}"
    );
    assert_eq!(
        status.last_error.as_deref(),
        Some("the plugin exited (exit status: 1)")
    );
}

#[tokio::test(start_paused = true)]
async fn launch_failures_back_off_too() {
    let mut h = Harness::new(&[
        Plan::Fail { retryable: true },
        Plan::Fail { retryable: true },
    ]);
    let attempted = h.start();
    let first = h.next_launch().await;
    attempted.await.unwrap();
    h.settle().await;
    let status = h.status();
    assert!(matches!(status.state, PluginState::BackingOff { .. }));
    assert_eq!(
        status.last_error.as_deref(),
        Some("the plugin exited before the handshake")
    );
    let second = h.next_launch().await;
    let third = h.next_launch().await;
    assert_eq!(secs(&[second.at - first.at, third.at - second.at]), [1, 2]);
    assert!(third.pid.is_some());
    h.settle().await;
    let status = h.status();
    assert!(
        matches!(status.state, PluginState::Running { pid: Some(1), .. }),
        "{status:?}"
    );
    assert_eq!(status.restarts, 2);
    // The error stays visible after the recovery.
    assert!(status.last_error.is_some());
}

#[tokio::test(start_paused = true)]
async fn ten_healthy_minutes_reset_the_backoff() {
    let crash = Plan::Up(Some(Duration::ZERO));
    let mut h = Harness::new(&[
        crash,
        crash,
        crash,
        Plan::Up(Some(HEALTHY_RESET)),
        crash,
        Plan::Up(Some(HEALTHY_RESET - Duration::from_secs(60))),
        crash,
    ]);
    h.start();
    let mut times = Vec::new();
    for _ in 0..8 {
        times.push(h.next_launch().await.at);
    }
    let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
    // 1, 2, 4; then 10 minutes up and a fresh 1 s; 2; then 9 minutes up, which isn't enough: 4;
    // then 8.
    assert_eq!(secs(&gaps), [1, 2, 4, 600 + 1, 2, 540 + 4, 8]);
    // Health was checked every 30 s while it ran.
    assert!(h.fake.health_checks.load(Ordering::SeqCst) >= 19 + 17);
}

#[tokio::test(start_paused = true)]
async fn a_failed_health_check_restarts_the_plugin() {
    let mut h = Harness::new(&[]);
    h.start();
    let first = h.next_launch().await;
    h.settle().await;
    assert!(matches!(
        h.status().state,
        PluginState::Running { pid: Some(1), .. }
    ));
    h.fake.healthy.store(false, Ordering::SeqCst);
    assert_eq!(h.next_stop().await, 1);
    assert_eq!(first.at.elapsed(), HEALTH_INTERVAL);
    h.fake.healthy.store(true, Ordering::SeqCst);
    h.settle().await;
    let status = h.status();
    assert!(matches!(status.state, PluginState::BackingOff { .. }));
    assert_eq!(
        status.last_error.as_deref(),
        Some("health check failed: giles.channel.v1.ChannelService is NOT_SERVING")
    );
    let second = h.next_launch().await;
    assert_eq!(second.at - first.at, HEALTH_INTERVAL + FIRST_BACKOFF);
    assert_eq!(second.pid, Some(2));
    h.settle().await;
    assert_eq!(h.status().restarts, 1);
}

#[tokio::test(start_paused = true)]
async fn a_failure_that_wont_pass_waits_for_a_new_binary() {
    let mut h = Harness::new(&[
        Plan::Fail { retryable: false },
        Plan::Fail { retryable: false },
    ]);
    h.start();
    h.next_launch().await;
    h.settle().await;
    let status = h.status();
    assert_eq!(status.state, PluginState::Failed);
    assert!(status.last_error.is_some());

    // Not relaunched however long it waits with the same binary.
    tokio::time::sleep(Duration::from_secs(60 * 60)).await;
    assert!(h.launched.try_recv().is_err());

    // A new binary is picked up at the next check.
    *lock(&h.fake.binary) = Some([2; 32]);
    let changed = Instant::now();
    let second = h.next_launch().await;
    assert!(
        second.at - changed <= BINARY_RECHECK,
        "{:?}",
        second.at - changed
    );
    h.settle().await;
    assert_eq!(h.status().state, PluginState::Failed);

    // Failed again with the new binary; a restart request doesn't wait for another.
    assert!(h.supervision.restart("irc"));
    let third = h.next_launch().await;
    assert_eq!(third.at, second.at);
    assert_eq!(third.pid, Some(1));
    h.settle().await;
    let status = h.status();
    assert!(matches!(status.state, PluginState::Running { .. }));
    assert_eq!(status.restarts, 2);
}

#[tokio::test(start_paused = true)]
async fn a_restart_request_relaunches_at_once() {
    let mut h = Harness::new(&[]);
    h.start();
    let first = h.next_launch().await;
    h.settle().await;
    assert!(h.supervision.restart("irc"));
    assert!(!h.supervision.restart("other"));
    assert_eq!(h.next_stop().await, 1);
    let second = h.next_launch().await;
    assert_eq!(second.at, first.at);
    assert_eq!(second.pid, Some(2));
    h.settle().await;
    assert_eq!(h.status().restarts, 1);
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_a_running_plugin_and_a_backoff_at_once() {
    let mut h = Harness::new(&[]);
    h.start();
    h.next_launch().await;
    h.settle().await;
    h.supervision.stop_all().await;
    assert_eq!(h.next_stop().await, 1);
    assert_eq!(h.status().state, PluginState::Stopped);

    let mut h = Harness::new(&[Plan::Fail { retryable: true }]);
    h.start();
    h.next_launch().await;
    h.settle().await;
    let before = Instant::now();
    h.supervision.stop_all().await;
    assert_eq!(Instant::now(), before);
    assert_eq!(h.status().state, PluginState::Stopped);
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(h.launched.try_recv().is_err());
    // Nothing new is supervised after a stop.
    let attempted = h.start();
    assert!(attempted.await.is_err());
}

#[tokio::test(start_paused = true)]
async fn the_describe_cache_shows_capabilities_while_the_plugin_is_down() {
    let mut h = Harness::new(&[]);
    h.start();
    h.next_launch().await;
    h.settle().await;
    let status = h.status();
    assert_eq!(status.describe, Some(info("1.0")));
    assert!(!status.describe_cached);
    assert_eq!(
        status.binary.as_deref(),
        Some(std::path::Path::new("/plugins/giles-channel-irc"))
    );
    h.supervision.stop_all().await;
    assert!(h.root.cache_dir().join("describe/irc.json").is_file());

    // The next start fails, but the cache for this binary and config fills in.
    let Harness { root, _dir, .. } = h;
    let mut h = Harness::with_root(&[Plan::Fail { retryable: false }], root, _dir);
    h.start();
    h.next_launch().await;
    h.settle().await;
    let status = h.status();
    assert_eq!(status.state, PluginState::Failed);
    assert_eq!(status.describe, Some(info("1.0")));
    assert!(status.describe_cached);

    // Another binary: nothing cached for it.
    *lock(&h.fake.binary) = Some([9; 32]);
    *lock(&h.fake.plans) = VecDeque::from([Plan::Fail { retryable: false }]);
    h.next_launch().await;
    h.settle().await;
    let status = h.status();
    assert_eq!(status.describe, None);
    assert!(!status.describe_cached);
}
