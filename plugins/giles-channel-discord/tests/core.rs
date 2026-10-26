//! The plugin as a channel of a running core (plugin host, `RemoteChannel`, router, sessions,
//! outbox), with the fake Discord client behind it. What the core does with the plugin's ids is
//! what these tests defend: the plugin sends a *native* id (`<user id>`), the core adds the
//! identity namespace the plugin reported (`discord:<user id>`) to match it against `[owner]
//! identities`, and strips it again from the identity of a proactive send. A unit test of the
//! plugin alone can't see either half (issue #22: the plugin put the namespace in the sender, the
//! core added it a second time, and every DM was dropped).

mod support;

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use giles_channel_discord::client::GatewayEvent;
use giles_channel_discord::fake::{direct_message, dm_channel_of};
use giles_core::commands::NoPendingApprovals;
use giles_core::config::{Builtins, Config, LoadOptions};
use giles_core::outbox::{OutboxItem, OutboxKind};
use giles_core::paths::DataRoot;
use giles_core::plugin_host::{ChannelWiring, Launcher, PluginHost};
use giles_core::runtime::Runtime;
use giles_core::sessions::{Turn, TurnOutcome, TurnRunner};
use giles_plugin_sdk::async_trait;
use giles_plugin_sdk::giles_types::InboundMessage;
use support::{Served, TOKEN, WAIT};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const USER: u64 = 82_198_898_841_029_460;
/// The DM channel of `USER`, as the conversation of their messages.
const CHANNEL: u64 = dm_channel_of(USER);
/// Another Discord user, who isn't the owner.
const STRANGER: u64 = 308_776_734_881_218_560;

/// A message id created now, so the plugin's age limit has nothing to say about it.
fn id_now(n: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    giles_channel_discord::snowflake::at_unix_ms(u64::try_from(now).unwrap()) + n
}

/// Polls `check` until it returns something, for at most [`WAIT`]; `what` is what it waits for.
async fn wait_until<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Records each turn's messages and replies `echo: <text>` to each.
#[derive(Default)]
struct Turns(Mutex<Vec<InboundMessage>>);

#[async_trait]
impl TurnRunner for Turns {
    async fn run_turn(&self, turn: Turn) -> TurnOutcome {
        for message in turn.messages {
            if let Some(reply) = &turn.reply
                && let Err(error) = reply.send(format!("echo: {}", message.message.text)).await
            {
                return TurnOutcome::Failed(error.to_string());
            }
            self.0.lock().unwrap().push(message.message);
        }
        TurnOutcome::Completed
    }
}

/// A core whose `[owner]` is `discord:<USER>` and whose one channel is the plugin in `served`.
struct Core {
    runtime: Runtime,
    config: Arc<Config>,
    turns: Arc<Turns>,
    host: Arc<PluginHost>,
    shutdown: CancellationToken,
    _dir: tempfile::TempDir,
}

impl Core {
    async fn start(served: &Served) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Resolved, so the data directory isn't below a symbolic link (macOS `/var`).
        let base = dir.path().canonicalize().unwrap();
        let root = DataRoot::new(base.join("home"), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        fs::write(
            root.config_file(),
            format!(
                "[owner]\nidentities = [\"discord:{USER}\"]\n\n\
                 [provider.anthropic]\nconfig = {{ api_key = \"sk-test-key\" }}\n\n\
                 [[routes]]\nagent = \"jeeves\"\n\n\
                 [channel.discord]\naddress = \"http://{}\"\n\n\
                 [channel.discord.config]\ntoken = {{ env = \"DISCORD_TOKEN\" }}\n",
                served.addr
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
        let env = |name: &str| (name == "DISCORD_TOKEN").then(|| OsString::from(TOKEN));
        let config = Arc::new(
            Config::load_with(
                &root,
                &LoadOptions {
                    builtins: Builtins::V1,
                    env: &env,
                    giles_exe: None,
                },
            )
            .unwrap(),
        );

        let runtime = Runtime::start(root.clone(), Arc::clone(&config))
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        runtime.start_outbox(shutdown.clone()).await.unwrap();
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
            sessions,
            blobs: runtime.blobs.clone(),
        };
        let launcher =
            Launcher::new(root, PathBuf::from("/nonexistent/giles"), Ok(())).env(|_| None);
        let host = Arc::new(PluginHost::new(launcher, wiring));
        tokio::time::timeout(WAIT, host.start(&config))
            .await
            .unwrap();
        wait_until("the channel to register", || {
            runtime.registry.channel("discord")
        })
        .await;
        Self {
            runtime,
            config,
            turns,
            host,
            shutdown,
            _dir: dir,
        }
    }

    /// The messages that went through turns so far.
    fn delivered(&self) -> Vec<InboundMessage> {
        self.turns.0.lock().unwrap().clone()
    }

    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(WAIT, self.host.stop_all())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn the_owners_dm_passes_the_core_as_discord_and_the_user_id() {
    let served = Served::start().await;
    let core = Core::start(&served).await;
    // The namespace the core got from `Configure` is the one `[owner] identities` is written in.
    let channel = core.runtime.registry.channel("discord").unwrap();
    assert_eq!(channel.info().identity_namespace, "discord");

    served.client.push(GatewayEvent::Ready { user_id: 7 });
    // The stranger's message comes first and the owner's second, so once the owner's has been
    // through a turn, the stranger's has been decided on.
    served.client.push(GatewayEvent::Message(direct_message(
        id_now(1),
        dm_channel_of(STRANGER),
        STRANGER,
        "let me in",
    )));
    served.client.push(GatewayEvent::Message(direct_message(
        id_now(2),
        CHANNEL,
        USER,
        "hello",
    )));

    wait_until(
        "the owner's message to reach a turn (dropped by the router?)",
        || (!core.delivered().is_empty()).then_some(()),
    )
    .await;
    let delivered = core.delivered();
    assert_eq!(
        delivered.len(),
        1,
        "only the owner's message reaches a turn: {delivered:?}"
    );
    assert_eq!(delivered[0].text, "hello");
    // The plugin's sender is the native id; the core's namespace makes it `discord:<USER>`.
    assert_eq!(delivered[0].sender.as_str(), USER.to_string());
    let owner = giles_core::identity::OwnerIdentities::new(&core.config.owner);
    assert!(owner.is_owner("discord", &delivered[0]));
    assert!(owner.contains("discord", &delivered[0].sender));

    // And the reply went back to the conversation.
    let reply = wait_until("a message to Discord", || {
        served.client.messages().into_iter().next()
    })
    .await;
    assert_eq!(reply, (CHANNEL, "echo: hello".to_owned()));
    core.stop().await;
}

#[tokio::test]
async fn a_proactive_send_reaches_the_plugin_without_the_namespace() {
    let served = Served::start().await;
    let core = Core::start(&served).await;

    // `[owner] default` is `discord:<USER>`; the outbox hands `Send` the part after the
    // namespace, and the plugin opens the DM with that user.
    core.runtime
        .outbox
        .enqueue(OutboxItem::to_owner(
            &core.config.owner,
            OutboxKind::Notice,
            "reminder",
        ))
        .await
        .unwrap();

    let sent = wait_until("a message to Discord", || {
        served.client.messages().into_iter().next()
    })
    .await;
    assert_eq!(sent, (dm_channel_of(USER), "reminder".to_owned()));
    assert_eq!(served.client.dms(), [USER]);
    core.stop().await;
}
