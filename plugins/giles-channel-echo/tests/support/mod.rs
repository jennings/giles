//! A data root with the echo plugin installed, for tests that launch it through the core's plugin
//! host, and a core (runtime, sessions, plugin host) around it.

#![allow(dead_code)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use giles_core::commands::NoPendingApprovals;
use giles_core::config::{Builtins, ChannelInstance, Config, LoadOptions};
use giles_core::paths::DataRoot;
use giles_core::plugin_host::{ChannelWiring, Launcher, PluginHost, STOP_GRACE};
use giles_core::runtime::Runtime;
use giles_core::sessions::{Sessions, Turn, TurnOutcome, TurnRunner};
use giles_plugin_sdk::async_trait;
use giles_plugin_sdk::giles_types::InboundMessage;
use tokio_util::sync::CancellationToken;

/// The echo plugin binary Cargo built for these tests.
pub const ECHO: &str = env!("CARGO_BIN_EXE_giles-channel-echo");

/// Upper bound for anything that must not hang.
pub const WAIT: Duration = Duration::from_secs(10);

/// A temporary data root whose `giles.toml` has the given channel tables.
pub struct Home {
    _dir: tempfile::TempDir,
    /// The data root.
    pub root: DataRoot,
    env: Vec<(String, String)>,
}

impl Home {
    /// A data root with `channels` in `giles.toml` and `env` as the environment config references
    /// (`{ env = "NAME" }`) resolve against.
    pub fn new(channels: &str, env: &[(&str, &str)]) -> Self {
        Self::create(
            &format!("[owner]\nidentities = [\"web:owner\"]\n\n{channels}"),
            env,
        )
    }

    /// A data root that routes every message from the owner, `echo:owner`, to agent `jeeves`,
    /// with `channels` in `giles.toml`: what [`Home::start_core`] needs.
    pub fn serving(channels: &str) -> Self {
        let home = Self::create(
            &format!(
                "[owner]\nidentities = [\"echo:owner\"]\n\n\
                 [provider.anthropic]\nconfig = {{ api_key = \"sk-test-key\" }}\n\n\
                 [[routes]]\nagent = \"jeeves\"\n\n{channels}"
            ),
            &[],
        );
        let agent = home.root.agents_dir().join("jeeves");
        fs::create_dir_all(&agent).unwrap();
        fs::write(
            agent.join("agent.toml"),
            "models = [\"anthropic/claude-test\"]\n",
        )
        .unwrap();
        home
    }

    fn create(giles_toml: &str, env: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, so the data directory isn't below a symbolic link the sandbox would see
        // differently (macOS `/var` is a link to `/private/var`).
        let base = dir.path().canonicalize().unwrap();
        let root = DataRoot::new(base.join("home"), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        fs::write(root.config_file(), giles_toml).unwrap();
        Self {
            _dir: dir,
            root,
            env: env
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    /// Links the echo binary into `<root>/plugins/` as `giles-channel-<component>`.
    pub fn install_echo_as(&self, component: &str) -> PathBuf {
        let link = self
            .root
            .plugins_dir()
            .join(format!("giles-channel-{component}"));
        std::os::unix::fs::symlink(ECHO, &link).unwrap();
        link
    }

    /// The loaded configuration.
    pub fn config(&self) -> Config {
        let env = |name: &str| {
            self.env
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| OsString::from(v))
        };
        Config::load_with(
            &self.root,
            &LoadOptions {
                builtins: Builtins::V1,
                env: &env,
                giles_exe: None,
            },
        )
        .unwrap()
    }

    /// The configured channel `name`.
    pub fn channel(&self, name: &str) -> ChannelInstance {
        self.config().channels[name].clone()
    }

    /// A launcher for this root that finds plugins only in `<root>/plugins/`, with `trampoline`.
    pub fn launcher(&self, trampoline: PathBuf) -> Launcher {
        Launcher::new(self.root.clone(), trampoline, Ok(())).env(|_| None)
    }

    /// The data directory of instance `instance`.
    pub fn data_dir(&self, instance: &str) -> PathBuf {
        self.root.plugin_data_dir(instance).unwrap()
    }

    /// Makes the echo plugin of `instance` emit `injection` (an `inject/*.json` document), as
    /// file `<name>.json`. The `inject/` directory exists once the plugin is configured.
    pub fn inject(&self, instance: &str, name: &str, injection: &serde_json::Value) {
        let data = self.data_dir(instance);
        let staged = data.join(format!("{name}.staged"));
        fs::write(&staged, serde_json::to_vec(injection).unwrap()).unwrap();
        fs::rename(&staged, data.join("inject").join(format!("{name}.json"))).unwrap();
    }

    /// The JSON lines of `file` in `instance`'s data directory (none if it doesn't exist).
    pub fn lines(&self, instance: &str, file: &str) -> Vec<serde_json::Value> {
        match fs::read_to_string(self.data_dir(instance).join(file)) {
            Ok(text) => text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The files in `giles-sdk/<dir>` of `instance`'s data directory.
    pub fn sdk_files(&self, instance: &str, dir: &str) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(self.data_dir(instance).join("giles-sdk").join(dir))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
        names.sort();
        names
    }

    /// Starts the core's runtime and sessions (turns recorded by [`Turns`], which replies to
    /// each), and a plugin host that launches every plugin channel without a trampoline.
    pub async fn start_core(&self) -> Core {
        let runtime = Runtime::start(self.root.clone(), Arc::new(self.config()))
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let turns = Arc::new(Turns::default());
        let sessions = runtime
            .start_sessions(
                turns.clone(),
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
        let host = PluginHost::new(self.launcher(PathBuf::from("/nonexistent/giles")), wiring);
        Core {
            runtime,
            sessions,
            turns,
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
    /// The turns run so far.
    pub turns: Arc<Turns>,
    /// The plugin host.
    pub host: PluginHost,
    shutdown: CancellationToken,
}

impl Core {
    /// Stops the sessions, then the plugins, as `giles serve` does.
    pub async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(WAIT, self.sessions.closed())
            .await
            .unwrap();
        tokio::time::timeout(WAIT, self.host.stop_all(STOP_GRACE))
            .await
            .unwrap();
    }
}

/// A turn runner that records each turn's messages and replies `echo: <text>` to each.
#[derive(Default)]
pub struct Turns {
    turns: Mutex<Vec<Vec<InboundMessage>>>,
}

#[async_trait]
impl TurnRunner for Turns {
    async fn run_turn(&self, turn: Turn) -> TurnOutcome {
        let messages: Vec<InboundMessage> = turn.messages.into_iter().map(|m| m.message).collect();
        if let Some(reply) = &turn.reply {
            for message in &messages {
                if let Err(error) = reply.send(format!("echo: {}", message.text)).await {
                    return TurnOutcome::Failed(error.to_string());
                }
            }
        }
        self.turns.lock().unwrap().push(messages);
        TurnOutcome::Completed
    }
}

impl Turns {
    /// Waits until `count` messages have been through turns, and returns them all.
    pub async fn wait_for(&self, count: usize) -> Vec<InboundMessage> {
        wait_until(|| {
            let messages: Vec<InboundMessage> = self
                .turns
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .cloned()
                .collect();
            (messages.len() >= count).then_some(messages)
        })
        .await
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

/// An injection: a direct message `text` from the owner, event id `id`, in conversation `c1`,
/// with text attachments.
pub fn owner_message(id: &str, text: &str, attachments: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "event": {
            "type": "message",
            "id": id,
            "message": {
                "conversation": { "id": "c1", "kind": "direct" },
                "sender": "owner",
                "sender_aliases": [],
                "sender_authenticated": true,
                "text": text,
                "attachments": [],
            },
        },
        "attachments": attachments
            .iter()
            .map(|text| serde_json::json!({
                "text": text,
                "media_type": "text/plain",
                "name": "note.txt",
            }))
            .collect::<Vec<_>>(),
    })
}
