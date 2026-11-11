//! A data root with the environment echo plugin installed, and a core (runtime, sessions, plugin
//! host) around it, for tests that launch or connect to the plugin through the plugin host.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use giles_core::commands::NoPendingApprovals;
use giles_core::config::{Builtins, Config, LoadOptions};
use giles_core::paths::DataRoot;
use giles_core::plugin_host::{ChannelWiring, Launcher, PluginHost};
use giles_core::runtime::Runtime;
use giles_core::sessions::{Sessions, Turn, TurnOutcome, TurnRunner};
use giles_plugin_sdk::async_trait;
use tokio_util::sync::CancellationToken;

/// The environment echo binary Cargo built for these tests.
pub const ECHO: &str = env!("CARGO_BIN_EXE_giles-environment-echo");

/// Upper bound for anything that must not hang.
pub const WAIT: Duration = Duration::from_secs(10);

/// A temporary data root whose `giles.toml` has the given tables.
pub struct Home {
    _dir: tempfile::TempDir,
    /// The data root.
    pub root: DataRoot,
}

impl Home {
    /// A data root with `tables` (instance tables) in `giles.toml`.
    pub fn new(tables: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, so the data directory isn't below a symbolic link the sandbox would see
        // differently (macOS `/var` is a link to `/private/var`).
        let base = dir.path().canonicalize().unwrap();
        let root = DataRoot::new(base.join("home"), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        fs::write(
            root.config_file(),
            format!("[owner]\nidentities = [\"web:owner\"]\n\n{tables}"),
        )
        .unwrap();
        Self { _dir: dir, root }
    }

    /// Links the echo binary into `<root>/plugins/` as `giles-environment-<component>`.
    pub fn install_echo_as(&self, component: &str) -> PathBuf {
        let link = self
            .root
            .plugins_dir()
            .join(format!("giles-environment-{component}"));
        std::os::unix::fs::symlink(ECHO, &link).unwrap();
        link
    }

    /// Appends `tables` to `giles.toml`, e.g. once a remote plugin's address is known.
    pub fn add_tables(&self, tables: &str) {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(self.root.config_file())
            .unwrap();
        writeln!(file, "\n{tables}").unwrap();
    }

    /// The loaded configuration, or why it doesn't load. `ECHO_TOKEN` is `s3cret` for `{ env }`
    /// references.
    pub fn try_config(&self) -> Result<Config, giles_core::config::ConfigError> {
        Config::load_with(
            &self.root,
            &LoadOptions {
                builtins: Builtins::V1,
                env: &|name| (name == "ECHO_TOKEN").then(|| "s3cret".into()),
                giles_exe: None,
            },
        )
    }

    /// The loaded configuration.
    pub fn config(&self) -> Config {
        self.try_config().unwrap()
    }

    /// A launcher for this root that finds plugins only in `<root>/plugins/`, running them
    /// without a trampoline.
    pub fn launcher(&self) -> Launcher {
        self.launcher_with(PathBuf::from("/nonexistent/giles"))
    }

    /// [`Home::launcher`] with `trampoline` as the sandbox trampoline.
    pub fn launcher_with(&self, trampoline: PathBuf) -> Launcher {
        Launcher::new(self.root.clone(), trampoline, Ok(())).env(|_| None)
    }

    /// The data directory of environment `instance`.
    pub fn data_dir(&self, instance: &str) -> PathBuf {
        self.root
            .plugin_data_dir(giles_core::config::Kind::Environment, instance)
            .unwrap()
    }

    /// Starts the core's runtime and sessions and a plugin host that supervises every plugin
    /// in the configuration, returning after each one's first launch attempt.
    pub async fn start_core(&self) -> Core {
        let runtime = Runtime::start(self.root.clone(), Arc::new(self.config()))
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let sessions = runtime
            .start_sessions(
                Arc::new(NoTurns),
                Arc::new(NoPendingApprovals),
                shutdown.clone(),
            )
            .await
            .unwrap();
        let wiring = ChannelWiring {
            registry: runtime.registry.clone(),
            sessions: sessions.clone(),
            blobs: runtime.blobs.clone(),
        };
        let host = Arc::new(PluginHost::new(self.launcher(), wiring));
        tokio::time::timeout(WAIT, host.start(&self.config()))
            .await
            .unwrap();
        Core {
            runtime,
            sessions,
            host,
            shutdown,
        }
    }
}

/// A running core around a [`Home`].
pub struct Core {
    /// The runtime: registry, store, blobs.
    pub runtime: Runtime,
    /// The session manager.
    pub sessions: Sessions,
    /// The plugin host.
    pub host: Arc<PluginHost>,
    shutdown: CancellationToken,
}

impl Core {
    /// Stops the sessions, then the plugins, as `giles serve` does.
    pub async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(WAIT, self.sessions.closed())
            .await
            .unwrap();
        tokio::time::timeout(WAIT, self.host.stop_all())
            .await
            .unwrap();
    }
}

/// A turn runner for a core that is sent no messages.
struct NoTurns;

#[async_trait]
impl TurnRunner for NoTurns {
    async fn run_turn(&self, _turn: Turn) -> TurnOutcome {
        TurnOutcome::Completed
    }
}

/// Polls `check` until it returns something, for at most [`WAIT`].
pub async fn wait_until<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
