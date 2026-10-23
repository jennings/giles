//! The plugin binary launched by the core's plugin host (unsandboxed) against the fake server:
//! a query from the owner's account becomes a turn, the turn's reply goes back as a `PRIVMSG`, the
//! status reaches the core, and a bad config fails `Configure` naming the key.

mod support;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use giles_core::commands::NoPendingApprovals;
use giles_core::config::{Builtins, Config, LoadOptions};
use giles_core::paths::DataRoot;
use giles_core::plugin_host::{ChannelWiring, LaunchError, Launcher, PluginHost};
use giles_core::runtime::Runtime;
use giles_core::sessions::{Turn, TurnOutcome, TurnRunner};
use giles_plugin_sdk::async_trait;
use giles_plugin_sdk::giles_types::{ChannelStatus, InboundMessage};
use tokio_util::sync::CancellationToken;

use self::support::{FakeServer, MODERN_CAPS, MODERN_REQ, WAIT};

/// The plugin binary Cargo built for these tests.
const PLUGIN: &str = env!("CARGO_BIN_EXE_giles-channel-irc");

/// A data root with the plugin installed and `giles.toml` routing the owner, account `stephen`
/// on network `testnet`, to agent `jeeves`, with channel `testnet` configured by `config`.
struct Home {
    _dir: tempfile::TempDir,
    root: DataRoot,
}

impl Home {
    fn new(config: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, as the data directory must not be below a symbolic link.
        let base = dir.path().canonicalize().unwrap();
        let root = DataRoot::new(base.join("home"), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        fs::write(
            root.config_file(),
            format!(
                "[owner]\nidentities = [\"irc:testnet:account:stephen\"]\n\n\
                 [provider.anthropic]\nconfig = {{ api_key = \"sk-test-key\" }}\n\n\
                 [[routes]]\nagent = \"jeeves\"\n\n\
                 [channel.testnet]\ncomponent = \"irc\"\nunsandboxed = true\nconfig = {config}\n"
            ),
        )
        .unwrap();
        let agent = root.agents_dir().join("jeeves");
        fs::create_dir_all(&agent).unwrap();
        fs::write(
            agent.join("agent.toml"),
            "models = [\"anthropic/claude-test\"]\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(PLUGIN, root.plugins_dir().join("giles-channel-irc")).unwrap();
        Self { _dir: dir, root }
    }

    fn config(&self) -> Config {
        let env = |_: &str| -> Option<OsString> { None };
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

    fn launcher(&self) -> Launcher {
        // Unsandboxed instances bypass the trampoline.
        Launcher::new(
            self.root.clone(),
            PathBuf::from("/nonexistent/giles"),
            Ok(()),
        )
        .env(|_| None)
    }
}

/// Records each turn's messages and replies `echo: <text>` to each.
#[derive(Default)]
struct Turns {
    messages: Mutex<Vec<InboundMessage>>,
}

#[async_trait]
impl TurnRunner for Turns {
    async fn run_turn(&self, turn: Turn) -> TurnOutcome {
        for message in turn.messages {
            if let Some(reply) = &turn.reply
                && let Err(error) = reply.send(format!("echo: {}", message.message.text)).await
            {
                return TurnOutcome::Failed(error.to_string());
            }
            self.messages.lock().unwrap().push(message.message);
        }
        TurnOutcome::Completed
    }
}

/// Polls `check` until it returns something, for at most [`WAIT`].
async fn wait_until<T>(mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn owner_queries_become_turns_and_replies_go_back() {
    let server = FakeServer::plain().await;
    let home = Home::new(&format!(
        "{{ server = \"127.0.0.1\", port = {}, tls = false, nick = \"giles\", \
         network = \"testnet\", send_interval_ms = 0 }}",
        server.port()
    ));
    let config = Arc::new(home.config());
    let runtime = Runtime::start(home.root.clone(), Arc::clone(&config))
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
    let host = Arc::new(PluginHost::new(home.launcher(), wiring));
    tokio::time::timeout(WAIT, host.start(&config))
        .await
        .unwrap();
    assert!(runtime.registry.channel("testnet").is_some());

    let mut peer = server.accept().await;
    peer.register(MODERN_CAPS, Some(MODERN_REQ)).await;
    let status = wait_until(|| sessions.channel_status("testnet")).await;
    assert_eq!(status.status, ChannelStatus::Connected);

    // A channel message (group: dropped by the core in v1), someone without an account
    // (unauthenticated: dropped), then the owner.
    peer.send("@account=stephen :stephen!~sj@host PRIVMSG #giles :hi all")
        .await;
    peer.send(":mallory!m@host PRIVMSG giles :I am stephen")
        .await;
    peer.send("@account=stephen;msgid=m1 :Stephen!~sj@host PRIVMSG giles :hello")
        .await;
    peer.expect("PRIVMSG stephen :echo: hello").await;
    let messages = wait_until(|| {
        let messages = turns.messages.lock().unwrap().clone();
        (!messages.is_empty()).then_some(messages)
    })
    .await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "hello");
    assert_eq!(messages[0].sender.as_str(), "account:stephen");

    shutdown.cancel();
    tokio::time::timeout(WAIT, sessions.closed()).await.unwrap();
    tokio::time::timeout(WAIT, host.stop_all()).await.unwrap();
}

#[tokio::test]
async fn a_bad_config_fails_configure_naming_the_key() {
    let home = Home::new(
        "{ server = \"127.0.0.1\", tls = false, nick = \"two words\", network = \"testnet\" }",
    );
    let error = home
        .launcher()
        .launch_channel(&home.config().channels["testnet"])
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            LaunchError::Rpc { call: "Configure", code: tonic::Code::InvalidArgument, message }
                if message.contains("invalid config: nick: not a valid IRC nick")
        ),
        "{error}"
    );
    assert!(!error.is_retryable());
}
