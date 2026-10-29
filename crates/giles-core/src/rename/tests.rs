//! The per-kind instance renames (`giles channel rename` and the others), against temporary data
//! roots and databases.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use giles_store::{InboxState, InstanceRows, NewInboxEvent, NewOutbox, SessionKey, SessionKind};
use giles_store::{Store, UsageRecord};
use giles_types::{AgentId, ConversationId, EventId, NativeId, UserId};
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::config::{Config, LoadOptions};

const GILES_TOML: &str = r##"# My Giles
[owner]
identities = ["irc:libera:account:me"]

[provider.anthropic]

[provider.ollama]
component = "openai"

# The Libera network.
[channel.libera]
component = "irc"
unsandboxed = true
config = { server = "irc.libera.chat" }

[channel.web]

[toolset.github]
command = "/bin/github"

[[routes]]
channel = "libera"
agent = "jeeves"

[[routes]]
channel = "web"
agent = "jeeves"
"##;

const JEEVES: &str = r##"models = ["ollama/qwen3:8b", "anthropic/claude"]
tools = ["github__*", "memory*"]
ask = ["github__create_issue"]
"##;

const BUNTER: &str = "models = [\"anthropic/claude\"]\n";

fn env(_: &str) -> Option<OsString> {
    None
}

/// A data root with a database and the lock a rename needs.
struct Home {
    dir: TempDir,
    root: DataRoot,
    store: Store,
    lock: InstanceLock,
}

impl Home {
    async fn new(giles_toml: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        fs::create_dir_all(root.run_dir()).unwrap();
        fs::create_dir_all(root.data_dir()).unwrap();
        let lock = InstanceLock::acquire(&root).unwrap();
        let store = Store::open(root.database_file()).await.unwrap();
        let home = Self {
            dir,
            root,
            store,
            lock,
        };
        home.write("giles.toml", giles_toml);
        home
    }

    /// A home with everything a rename touches, under the names `libera`, `ollama`, `github`.
    async fn populated() -> Self {
        let home = Self::new(GILES_TOML).await;
        home.write("agents/jeeves/agent.toml", JEEVES);
        home.write("agents/bunter/agent.toml", BUNTER);
        home.write("data/channel/libera/state.json", "{\"cursor\": 7}");
        home.write("data/toolset/github/token-cache", "x");
        home.write("agents/jeeves/groups/libera/%23giles/MEMORY.md", "quiet");
        home.write(
            "agents/bunter/groups/web/chat-1/MEMORY.md",
            "other instance",
        );
        home.write("cache/describe/channel/libera.json", "{}");
        home.write("cache/toolsets/github.json", "{}");
        home.write("run/pids/channel/libera.pid", "123\n");
        home.seed("libera", "ollama").await;
        home
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.path(relative)).unwrap()
    }

    /// Rows of every table that names an instance: channel rows under `channel`, usage under
    /// `provider`, and a second channel `web` that must never move.
    async fn seed(&self, channel: &str, provider: &str) {
        let owner = UserId::new("owner").unwrap();
        for (name, conversation) in [(channel, "#giles"), (channel, "nick"), ("web", "chat-1")] {
            let key = SessionKey {
                agent: AgentId::new("jeeves").unwrap(),
                channel: name.into(),
                conversation: ConversationId::new(conversation).unwrap(),
            };
            let at = jiff::Timestamp::from_second(1_800_000_000).unwrap();
            self.store
                .open_session(key, SessionKind::Group, Some(owner.clone()), at)
                .await
                .unwrap();
        }
        for (instance, event) in [(channel, "e1"), (channel, "e2"), ("web", "e1")] {
            self.store
                .inbox_insert(NewInboxEvent {
                    instance: instance.into(),
                    event_id: EventId::new(event).unwrap(),
                    session: None,
                    user: None,
                    payload: json!({}),
                    state: InboxState::Done,
                    received_at: jiff::Timestamp::from_second(1_800_000_001).unwrap(),
                })
                .await
                .unwrap();
        }
        self.store
            .outbox_enqueue(NewOutbox {
                instance: channel.into(),
                recipient: NativeId::new("irc:libera:account:me").unwrap(),
                kind: "notice".into(),
                user: None,
                payload: json!({"markdown": "hi"}),
                created_at: jiff::Timestamp::from_second(1_800_000_002).unwrap(),
                expires_at: jiff::Timestamp::from_second(1_800_090_000).unwrap(),
            })
            .await
            .unwrap();
        for name in [provider, "anthropic"] {
            self.store
                .record_usage(UsageRecord {
                    at: jiff::Timestamp::from_second(1_800_000_003).unwrap(),
                    provider: name.into(),
                    model: "m".into(),
                    agent: AgentId::new("jeeves").unwrap(),
                    user: None,
                    session: None,
                    usage: Default::default(),
                    fallback: false,
                })
                .await
                .unwrap();
        }
    }

    async fn rename(&self, kind: Kind, old: &str, new: &str) -> Result<RenameReport, RenameError> {
        rename_instance(&self.root, &self.store, &self.lock, kind, old, new).await
    }

    fn load(&self) -> Config {
        Config::load_with(
            &self.root,
            &LoadOptions {
                env: &env,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    async fn rows(&self) -> BTreeMap<String, InstanceRows> {
        all_instance_rows(&self.store).await.unwrap()
    }

    /// Everything a failed rename must leave alone: the file tree with contents, and the rows.
    async fn snapshot(&self) -> (BTreeMap<PathBuf, String>, BTreeMap<String, InstanceRows>) {
        fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let relative = path.strip_prefix(root).unwrap().to_owned();
                // The database and the lock file change on their own.
                let name = relative
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if name.starts_with("giles.db") || name == "giles.lock" {
                    continue;
                }
                if path.is_dir() {
                    out.insert(relative, "<dir>".into());
                    walk(&path, root, out);
                } else {
                    out.insert(relative, fs::read_to_string(&path).unwrap());
                }
            }
        }
        let mut files = BTreeMap::new();
        walk(self.dir.path(), self.dir.path(), &mut files);
        (files, self.rows().await)
    }
}

use std::fs;
use std::path::{Path, PathBuf};

/// Every name the database's rows use, whatever the kind, with how many rows use it.
async fn all_instance_rows(
    store: &Store,
) -> Result<BTreeMap<String, InstanceRows>, giles_store::Error> {
    let mut names: BTreeMap<String, InstanceRows> = BTreeMap::new();
    for kind in Kind::ALL {
        for (name, rows) in store.instance_rows(kind).await? {
            let entry = names.entry(name).or_default();
            entry.sessions += rows.sessions;
            entry.inbox += rows.inbox;
            entry.outbox += rows.outbox;
            entry.usage += rows.usage;
        }
    }
    Ok(names)
}

#[tokio::test]
async fn renaming_a_channel_moves_the_rows_the_directories_and_the_config_table() {
    let home = Home::populated().await;
    fs::set_permissions(home.path("giles.toml"), fs::Permissions::from_mode(0o600)).unwrap();
    let report = home
        .rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap();

    assert_eq!(report.kind, Kind::Channel);
    assert_eq!(
        (
            report.rows.sessions,
            report.rows.inbox,
            report.rows.outbox,
            report.rows.usage
        ),
        (2, 2, 1, 0)
    );
    assert!(report.config_table);
    assert_eq!(report.legacy_routes, 1);
    assert!(
        report.agent_files.is_empty(),
        "channels aren't named in agents"
    );

    // giles.toml: the table, its comment, and the route; nothing else.
    assert_eq!(
        home.read("giles.toml"),
        GILES_TOML
            .replace("[channel.libera]", "[channel.libera-chat]")
            .replace("channel = \"libera\"", "channel = \"libera-chat\"")
    );
    let mode = fs::metadata(home.path("giles.toml"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // Directories and disposable per-instance files moved; other instances' didn't.
    assert_eq!(
        home.read("data/channel/libera-chat/state.json"),
        "{\"cursor\": 7}"
    );
    assert!(!home.path("data/channel/libera").exists());
    assert_eq!(
        home.read("agents/jeeves/groups/libera-chat/%23giles/MEMORY.md"),
        "quiet"
    );
    assert!(!home.path("agents/jeeves/groups/libera").exists());
    assert_eq!(
        home.read("agents/bunter/groups/web/chat-1/MEMORY.md"),
        "other instance"
    );
    assert!(home.path("data/toolset/github/token-cache").is_file());
    assert!(
        home.path("cache/describe/channel/libera-chat.json")
            .is_file()
    );
    assert_eq!(home.read("run/pids/channel/libera-chat.pid"), "123\n");
    assert!(report.moved.len() >= 4, "{:?}", report.moved);

    // Rows: the renamed instance has all of them; `web` and the providers kept theirs.
    let rows = home.rows().await;
    assert!(!rows.contains_key("libera"));
    assert_eq!(rows["libera-chat"].sessions, 2);
    assert_eq!(rows["libera-chat"].inbox, 2);
    assert_eq!(rows["libera-chat"].outbox, 1);
    assert_eq!(rows["web"].sessions, 1);
    assert_eq!(rows["ollama"].usage, 1);

    // The result is a configuration that loads, with the `[[routes]]` it still has (which Giles
    // only imports or ignores) pointing at the new name.
    let config = home.load();
    assert!(config.channels.contains_key("libera-chat"));
    assert!(!config.channels.contains_key("libera"));
    assert_eq!(
        config.legacy_routes[0]["channel"].as_str(),
        Some("libera-chat")
    );
}

/// Stored routes (not the `[[routes]]` of giles.toml): two for `channel`, one for `web`, one for
/// any channel; in that order.
async fn seed_routes(home: &Home, channel: &str) {
    for (name, agent) in [
        (Some(channel), "jeeves"),
        (Some("web"), "bunter"),
        (None, "jeeves"),
        (Some(channel), "bunter"),
    ] {
        home.store
            .route_insert(
                usize::MAX,
                giles_store::NewRoute {
                    channel: name.map(str::to_owned),
                    kind: None,
                    conversation: None,
                    agent: AgentId::new(agent).unwrap(),
                },
            )
            .await
            .unwrap();
    }
}

async fn route_channels(home: &Home) -> Vec<Option<String>> {
    home.store
        .routes()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.route.channel)
        .collect()
}

#[tokio::test]
async fn renaming_a_channel_moves_its_stored_routes_in_place() {
    let home = Home::populated().await;
    seed_routes(&home, "libera").await;
    let report = home
        .rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap();
    assert_eq!(report.rows.routes, 2);
    // Same order, the other channel's route and the any-channel route untouched.
    assert_eq!(
        route_channels(&home).await,
        [
            Some("libera-chat".to_owned()),
            Some("web".to_owned()),
            None,
            Some("libera-chat".to_owned())
        ]
    );
    // The renamed configuration loads.
    home.load();
}

#[tokio::test]
async fn a_name_that_only_routes_use_can_be_renamed_once_its_new_name_is_configured() {
    // The operator renamed the table by hand and has no sessions: only the stored routes still
    // name `libera`.
    let home = Home::new(
        "[owner]\nidentities = [\"web:owner\"]\n[channel.web]\n\
         [channel.libera-chat]\ncomponent = \"irc\"\nunsandboxed = true\n",
    )
    .await;
    seed_routes(&home, "libera").await;
    let report = home
        .rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap();
    assert_eq!(report.rows.routes, 2);
    assert!(!report.config_table);
    assert_eq!(
        route_channels(&home).await[0].as_deref(),
        Some("libera-chat")
    );
}

#[tokio::test]
async fn a_failing_rename_leaves_the_stored_routes_where_they_were() {
    let home = Home::populated().await;
    seed_routes(&home, "libera").await;
    let before = route_channels(&home).await;
    // `giles.toml` is rewritten last, after the database rows (routes included) were updated in
    // the transaction; a read-only data root makes that write fail.
    fs::set_permissions(home.dir.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let writable_anyway = fs::File::create(home.path("probe")).is_ok();
    if writable_anyway {
        // Running as root: permissions don't stop us, so this failure can't be provoked.
        fs::set_permissions(home.dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let err = home
        .rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap_err();
    fs::set_permissions(home.dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(err, RenameError::Failed { .. }), "{err}");
    assert_eq!(route_channels(&home).await, before);
    assert_eq!(home.rows().await["libera"].sessions, 2);
}

#[tokio::test]
async fn renaming_a_provider_rewrites_the_models_that_use_it() {
    let home = Home::populated().await;
    let report = home
        .rename(Kind::Provider, "ollama", "local")
        .await
        .unwrap();

    assert_eq!(report.kind, Kind::Provider);
    assert_eq!(report.rows.usage, 1);
    assert_eq!(report.agent_files.len(), 1);
    assert_eq!(
        report.agent_files[0].0,
        home.path("agents/jeeves/agent.toml")
    );
    assert_eq!(report.agent_files[0].1, 1);
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        JEEVES.replace("ollama/", "local/")
    );
    assert_eq!(home.read("agents/bunter/agent.toml"), BUNTER);
    assert_eq!(
        home.read("giles.toml"),
        GILES_TOML.replace("[provider.ollama]", "[provider.local]")
    );
    let config = home.load();
    assert_eq!(
        config.agents[&AgentId::new("jeeves").unwrap()].models[0]
            .provider
            .as_str(),
        "local"
    );
    assert_eq!(home.rows().await["local"].usage, 1);
}

/// An agent removed in the web UI is moved to `trash/agents/<agent>-<time>/`, outside `agents/`:
/// a rename doesn't read it, rewrite it, or move the group directories in it, and doesn't mind
/// that the agent is gone.
#[tokio::test]
async fn an_agent_removed_in_the_web_ui_is_left_alone_by_a_rename() {
    let home = Home::populated().await;
    let trashed = home.path("trash/agents/jeeves-20261024T120000Z");
    fs::create_dir_all(trashed.parent().unwrap()).unwrap();
    fs::rename(home.path("agents/jeeves"), &trashed).unwrap();

    let report = home
        .rename(Kind::Provider, "ollama", "local")
        .await
        .unwrap();
    assert!(report.agent_files.is_empty(), "{:?}", report.agent_files);
    assert_eq!(
        fs::read_to_string(trashed.join("agent.toml")).unwrap(),
        JEEVES,
        "the trashed copy still says ollama"
    );
    assert_eq!(home.read("agents/bunter/agent.toml"), BUNTER);
    assert!(
        home.load()
            .agents
            .contains_key(&AgentId::new("bunter").unwrap())
    );

    // The channel's group memory in the trash stays where it is; the live agent's moves.
    home.rename(Kind::Channel, "libera", "oftc").await.unwrap();
    assert!(trashed.join("groups/libera/%23giles/MEMORY.md").is_file());
    assert!(!trashed.join("groups/oftc").exists());

    // Nothing in `agents/` at all is fine too.
    fs::remove_dir_all(home.path("agents")).unwrap();
    home.rename(Kind::Toolset, "github", "gh").await.unwrap();
    assert!(trashed.join("agent.toml").is_file());
}

#[tokio::test]
async fn renaming_a_toolset_rewrites_the_tool_globs_and_moves_its_directory() {
    let home = Home::populated().await;
    let report = home.rename(Kind::Toolset, "github", "gh").await.unwrap();

    assert_eq!(report.kind, Kind::Toolset);
    assert_eq!(report.agent_files[0].1, 2);
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        JEEVES.replace("github__", "gh__")
    );
    assert!(home.path("data/toolset/gh/token-cache").is_file());
    assert!(home.path("cache/toolsets/gh.json").is_file());
    let config = home.load();
    assert!(config.toolsets.contains_key("gh"));
    assert_eq!(
        config.agents[&AgentId::new("jeeves").unwrap()].ask,
        ["gh__create_issue"]
    );
}

#[tokio::test]
async fn a_table_renamed_by_hand_gets_its_data_moved() {
    // giles.toml already says `libera-chat`.
    let hand_edited = GILES_TOML
        .replace("libera", "libera-chat")
        .replace("irc.libera-chat.chat", "irc.libera.chat")
        .replace("irc:libera-chat:", "irc:libera:");
    let home = Home::new(&hand_edited).await;
    home.write("agents/jeeves/agent.toml", JEEVES);
    home.write("data/channel/libera/state.json", "{}");
    home.seed("libera", "ollama").await;

    let report = home
        .rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap();
    assert!(!report.config_table);
    assert_eq!(report.legacy_routes, 0);
    assert_eq!(home.read("giles.toml"), hand_edited);
    assert!(home.path("data/channel/libera-chat/state.json").is_file());
    assert_eq!(home.rows().await["libera-chat"].sessions, 2);
    home.load();
}

#[tokio::test]
async fn a_rename_that_stopped_part_way_can_be_run_again() {
    // The first attempt rewrote giles.toml and moved the plugin directory, then died before the
    // other directory and the database commit.
    let home = Home::populated().await;
    home.write(
        "giles.toml",
        &GILES_TOML
            .replace("[channel.libera]", "[channel.libera-chat]")
            .replace("channel = \"libera\"", "channel = \"libera-chat\""),
    );
    fs::rename(
        home.path("data/channel/libera"),
        home.path("data/channel/libera-chat"),
    )
    .unwrap();
    home.rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap();
    assert!(home.path("data/channel/libera-chat/state.json").is_file());
    assert!(home.path("agents/jeeves/groups/libera-chat").is_dir());
    assert_eq!(home.rows().await["libera-chat"].sessions, 2);
    home.load();
}

/// An interrupted rename is finished when `old` has since been configured as another kind: the
/// rows and directories of the channel are the channel's, whatever a provider `libera` is.
#[tokio::test]
async fn an_interrupted_rename_is_finished_when_the_old_name_is_now_another_kind() {
    let home = Home::populated().await;
    home.write(
        "giles.toml",
        &GILES_TOML
            .replace("[channel.libera]", "[channel.libera-chat]")
            .replace("channel = \"libera\"", "channel = \"libera-chat\"")
            .replace(
                "[provider.anthropic]",
                "[provider.anthropic]\n\n[provider.libera]",
            ),
    );
    fs::rename(
        home.path("data/channel/libera"),
        home.path("data/channel/libera-chat"),
    )
    .unwrap();

    home.rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap();

    assert!(home.path("data/channel/libera-chat/state.json").is_file());
    assert!(home.path("agents/jeeves/groups/libera-chat").is_dir());
    assert!(!home.path("agents/jeeves/groups/libera").exists());
    assert_eq!(home.rows().await["libera-chat"].sessions, 2);
    assert!(!home.rows().await.contains_key("libera"));
}

#[tokio::test]
async fn an_instance_without_data_is_just_renamed_in_the_config() {
    let home = Home::new(GILES_TOML).await;
    home.write("agents/jeeves/agent.toml", JEEVES);
    let report = home.rename(Kind::Channel, "web", "chat").await.unwrap();
    assert!(report.moved.is_empty());
    assert_eq!(report.rows.total(), 0);
    assert!(home.read("giles.toml").contains("[channel.chat]"));
    assert!(home.read("giles.toml").contains("channel = \"chat\""));
}

#[tokio::test]
async fn renaming_keeps_the_component_an_instance_was_named_after() {
    // `[channel.web]` and `[provider.anthropic]` name no `component`, so they run the components
    // called `web` and `anthropic` only because of their own names.
    let home = Home::new(GILES_TOML).await;
    home.write("agents/jeeves/agent.toml", JEEVES);
    home.rename(Kind::Channel, "web", "chat").await.unwrap();
    home.rename(Kind::Provider, "anthropic", "claude")
        .await
        .unwrap();

    let config = home.load();
    let chat = &config.channels["chat"];
    assert_eq!(chat.component, "web");
    assert!(
        matches!(chat.runtime, crate::config::ChannelRuntime::BuiltIn),
        "{:?}",
        chat.runtime
    );
    assert_eq!(config.providers["claude"].component, "anthropic");
    assert!(
        home.read("giles.toml")
            .contains("[channel.chat]\ncomponent = \"web\"\n")
    );
}

/// Runs a rename that must be refused and checks that it changed nothing.
async fn refused(home: &Home, kind: Kind, old: &str, new: &str) -> RenameError {
    let before = home.snapshot().await;
    let err = home.rename(kind, old, new).await.unwrap_err();
    assert_eq!(home.snapshot().await, before, "{err}");
    err
}

#[tokio::test]
async fn bad_requests_are_refused_without_changing_anything() {
    let home = Home::populated().await;

    for name in ["", "a b", "a/b", "..", "a_b", "é"] {
        let err = refused(&home, Kind::Channel, "libera", name).await;
        assert!(
            matches!(err, RenameError::InvalidName { .. }),
            "{name:?}: {err}"
        );
        let err = refused(&home, Kind::Channel, name, "fresh").await;
        assert!(
            matches!(err, RenameError::InvalidName { .. }),
            "{name:?}: {err}"
        );
    }

    let err = refused(&home, Kind::Channel, "libera", "libera").await;
    assert!(matches!(err, RenameError::SameName { .. }), "{err}");
    let err = refused(&home, Kind::Channel, "libera", "Libera").await;
    assert!(matches!(err, RenameError::SameName { .. }), "{err}");

    let err = refused(&home, Kind::Channel, "nothing", "fresh").await;
    assert!(matches!(err, RenameError::UnknownInstance { .. }), "{err}");
    assert!(
        err.to_string().contains("no channel named \"nothing\""),
        "{err}"
    );

    // Both configured: never a merge.
    let err = refused(&home, Kind::Channel, "libera", "web").await;
    assert!(matches!(err, RenameError::BothConfigured { .. }), "{err}");

    // A new name that is another configured instance of the kind in a different case.
    let err = refused(&home, Kind::Channel, "libera", "WEB").await;
    assert!(matches!(err, RenameError::CaseCollision { .. }), "{err}");
}

/// Each kind has its own names: a name that only another kind has, in `giles.toml` or stored in
/// the database, is not an instance of the command's kind.
#[tokio::test]
async fn a_name_only_another_kind_has_is_an_unknown_instance() {
    let home = Home::populated().await;
    add_stored(&home, "notes", NOTES).await;

    for (kind, old) in [
        (Kind::Provider, "libera"),
        (Kind::Toolset, "libera"),
        (Kind::Channel, "ollama"),
        (Kind::Toolset, "ollama"),
        (Kind::Channel, "github"),
        (Kind::Provider, "github"),
        (Kind::Channel, "notes"),
        (Kind::Provider, "NOTES"),
    ] {
        let err = refused(&home, kind, old, "fresh").await;
        assert!(
            matches!(&err, RenameError::UnknownInstance { kind: found, .. } if *found == kind),
            "{kind} {old}: {err}"
        );
    }
}

/// The error says what the name lacks for the command's kind. When another kind has the name in
/// `giles.toml`, it points at that kind's command.
#[tokio::test]
async fn an_unknown_instance_says_where_the_name_is_configured() {
    let home = Home::populated().await;

    let message = refused(&home, Kind::Toolset, "libera", "fresh")
        .await
        .to_string();
    assert!(
        message.starts_with(
            "no toolset named \"libera\": giles.toml has no `[toolset.libera]` table and there is \
             no toolset data under that name (no database rows, no directory under \
             data/toolset/)"
        ),
        "{message}"
    );
    assert!(
        message.ends_with("(giles.toml has `[channel.libera]`; use `giles channel rename`)"),
        "{message}"
    );

    let message = refused(&home, Kind::Channel, "nothing", "fresh")
        .await
        .to_string();
    assert_eq!(
        message,
        "no channel named \"nothing\": giles.toml has no `[channel.nothing]` table and there is no \
         channel data under that name (no database rows or routes, no directory under \
         data/channel/ or agents/*/groups/)"
    );

    // More than one other kind has the name.
    let home = Home::new(&format!("{GILES_TOML}\n[provider.libera]\n")).await;
    let message = refused(&home, Kind::Toolset, "libera", "fresh")
        .await
        .to_string();
    assert!(
        message.ends_with(
            "(giles.toml has `[provider.libera]` and `[channel.libera]`; use `giles provider \
             rename` or `giles channel rename`)"
        ),
        "{message}"
    );
}

/// A provider, a channel, and a toolset that share a name stay apart: renaming one of them moves
/// that kind's table, rows, directories, files, and references, and nothing of the other two.
#[tokio::test]
async fn renaming_one_kinds_instance_leaves_the_same_name_of_the_other_kinds_alone() {
    const SHARED: &str = r##"[owner]
identities = ["web:owner"]

[provider.aperture]
component = "openai"

[channel.aperture]
component = "irc"
unsandboxed = true

[toolset.aperture]
command = "/bin/aperture"

[[routes]]
channel = "aperture"
agent = "jeeves"
"##;
    const AGENT: &str = "models = [\"aperture/gpt\"]\n\
                         tools = [\"aperture__*\"]\n\
                         ask = [\"aperture__send\"]\n";
    async fn shared_home() -> Home {
        let home = Home::new(SHARED).await;
        home.write("agents/jeeves/agent.toml", AGENT);
        for kind in ["provider", "channel", "toolset"] {
            home.write(&format!("data/{kind}/aperture/state"), kind);
            home.write(&format!("run/pids/{kind}/aperture.pid"), "123\n");
        }
        home.write("cache/describe/channel/aperture.json", "{}");
        home.write("cache/describe/provider/aperture.json", "{}");
        home.write("cache/toolsets/aperture.json", "{}");
        home.write("agents/jeeves/groups/aperture/chat/MEMORY.md", "quiet");
        home.seed("aperture", "aperture").await;
        home
    }
    async fn channel_sessions(home: &Home, name: &str) -> u64 {
        home.store.instance_rows(Kind::Channel).await.unwrap()[name].sessions
    }

    // The provider.
    let home = shared_home().await;
    home.rename(Kind::Provider, "aperture", "gateway")
        .await
        .unwrap();
    let giles = home.read("giles.toml");
    assert!(giles.contains("[provider.gateway]"), "{giles}");
    assert!(giles.contains("[channel.aperture]"), "{giles}");
    assert!(giles.contains("[toolset.aperture]"), "{giles}");
    assert!(giles.contains("channel = \"aperture\""), "{giles}");
    // Only the model references are the provider's.
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        AGENT.replace("aperture/gpt", "gateway/gpt")
    );
    assert_eq!(home.read("data/provider/gateway/state"), "provider");
    assert!(!home.path("data/provider/aperture").exists());
    assert_eq!(home.read("data/channel/aperture/state"), "channel");
    assert_eq!(home.read("data/toolset/aperture/state"), "toolset");
    assert!(home.path("run/pids/provider/gateway.pid").is_file());
    assert!(home.path("run/pids/channel/aperture.pid").is_file());
    assert!(home.path("run/pids/toolset/aperture.pid").is_file());
    assert!(home.path("cache/describe/provider/gateway.json").is_file());
    assert!(home.path("cache/describe/channel/aperture.json").is_file());
    assert!(home.path("cache/toolsets/aperture.json").is_file());
    assert!(home.path("agents/jeeves/groups/aperture").is_dir());
    assert_eq!(channel_sessions(&home, "aperture").await, 2);
    let usage = home.store.instance_rows(Kind::Provider).await.unwrap();
    assert_eq!(usage["gateway"].usage, 1);
    assert!(!usage.contains_key("aperture"));
    let config = home.load();
    assert!(config.providers.contains_key("gateway"));
    assert!(config.channels.contains_key("aperture"));
    assert!(config.toolsets.contains_key("aperture"));

    // The channel.
    let home = shared_home().await;
    let report = home
        .rename(Kind::Channel, "aperture", "chat")
        .await
        .unwrap();
    assert_eq!(report.legacy_routes, 1);
    let giles = home.read("giles.toml");
    assert!(giles.contains("[provider.aperture]"), "{giles}");
    assert!(giles.contains("[channel.chat]"), "{giles}");
    assert!(giles.contains("[toolset.aperture]"), "{giles}");
    // Agents don't name a channel.
    assert_eq!(home.read("agents/jeeves/agent.toml"), AGENT);
    assert_eq!(home.read("data/channel/chat/state"), "channel");
    assert_eq!(home.read("data/provider/aperture/state"), "provider");
    assert_eq!(home.read("data/toolset/aperture/state"), "toolset");
    assert!(home.path("run/pids/channel/chat.pid").is_file());
    assert!(home.path("run/pids/provider/aperture.pid").is_file());
    assert!(home.path("run/pids/toolset/aperture.pid").is_file());
    assert!(home.path("cache/describe/channel/chat.json").is_file());
    assert!(home.path("cache/describe/provider/aperture.json").is_file());
    assert!(home.path("agents/jeeves/groups/chat").is_dir());
    assert!(!home.path("agents/jeeves/groups/aperture").exists());
    assert_eq!(channel_sessions(&home, "chat").await, 2);
    let usage = home.store.instance_rows(Kind::Provider).await.unwrap();
    assert_eq!(usage["aperture"].usage, 1);
    let config = home.load();
    assert!(config.providers.contains_key("aperture"));
    assert!(config.channels.contains_key("chat"));
    assert!(config.toolsets.contains_key("aperture"));

    // The toolset.
    let home = shared_home().await;
    home.rename(Kind::Toolset, "aperture", "tools")
        .await
        .unwrap();
    let giles = home.read("giles.toml");
    assert!(giles.contains("[provider.aperture]"), "{giles}");
    assert!(giles.contains("[channel.aperture]"), "{giles}");
    assert!(giles.contains("[toolset.tools]"), "{giles}");
    assert!(giles.contains("channel = \"aperture\""), "{giles}");
    // Only the tool globs are the toolset's.
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        AGENT.replace("aperture__", "tools__")
    );
    assert_eq!(home.read("data/toolset/tools/state"), "toolset");
    assert_eq!(home.read("data/provider/aperture/state"), "provider");
    assert_eq!(home.read("data/channel/aperture/state"), "channel");
    assert!(home.path("run/pids/toolset/tools.pid").is_file());
    assert!(home.path("run/pids/provider/aperture.pid").is_file());
    assert!(home.path("run/pids/channel/aperture.pid").is_file());
    assert!(home.path("cache/toolsets/tools.json").is_file());
    assert!(home.path("cache/describe/channel/aperture.json").is_file());
    assert!(home.path("agents/jeeves/groups/aperture").is_dir());
    assert_eq!(channel_sessions(&home, "aperture").await, 2);
    let usage = home.store.instance_rows(Kind::Provider).await.unwrap();
    assert_eq!(usage["aperture"].usage, 1);
    let config = home.load();
    assert!(config.providers.contains_key("aperture"));
    assert!(config.channels.contains_key("aperture"));
    assert!(config.toolsets.contains_key("tools"));
}

/// Data a removed instance left behind is moved by the command of its kind, whatever another
/// kind has made of the name since: only that kind's table, rows and directories are looked at.
#[tokio::test]
async fn left_behind_data_follows_the_kind_of_the_command_not_the_name() {
    let by_hand = "[owner]\nidentities = [\"web:owner\"]\n[provider.libera]\n\
                   [channel.libera-chat]\ncomponent = \"irc\"\nunsandboxed = true\n";
    let home = Home::new(by_hand).await;
    home.write("data/channel/libera/state.json", "{\"cursor\": 7}");
    home.seed("libera", "anthropic").await;

    let report = home
        .rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap();

    assert_eq!(report.kind, Kind::Channel);
    assert!(!report.config_table);
    assert_eq!(report.rows.sessions, 2);
    assert_eq!(home.rows().await["libera-chat"].sessions, 2);
    assert!(home.path("data/channel/libera-chat/state.json").is_file());
    assert_eq!(home.read("giles.toml"), by_hand);
}

/// Left-behind data can't move to a name that only another kind's table has: the new name has to
/// be configured as the kind.
#[tokio::test]
async fn left_behind_data_cannot_move_to_a_name_only_another_kind_has() {
    let home = Home::new(
        "[owner]\nidentities = [\"web:owner\"]\n[provider.libera-chat]\ncomponent = \"openai\"\n",
    )
    .await;
    home.write("data/channel/libera/state.json", "{}");

    let err = refused(&home, Kind::Channel, "libera", "libera-chat").await;
    assert!(
        matches!(
            &err,
            RenameError::NotConfigured {
                kind: Kind::Channel,
                ..
            }
        ),
        "{err}"
    );
}

/// A toolset stored in the database is a toolset: its name can't be given to a `[toolset.*]` table
/// of `giles.toml`, ignoring case, which would take the name from it at the next start. A
/// channel or a provider has its own names, so it may take the name.
#[tokio::test]
async fn a_new_name_that_a_stored_toolset_has_is_refused_for_a_toolset_only() {
    let home = Home::populated().await;
    home.store
        .toolset_insert(
            "Notes",
            r#"{ "url": "https://notes.example.com/mcp" }"#,
            jiff::Timestamp::now(),
        )
        .await
        .unwrap();
    for new in ["notes", "Notes", "NOTES"] {
        let err = refused(&home, Kind::Toolset, "github", new).await;
        assert!(
            matches!(&err, RenameError::StoredToolset { existing, .. } if existing == "Notes"),
            "{new}: {err}"
        );
    }
    // The other kinds take the name, and the stored toolset stays as it is.
    home.rename(Kind::Channel, "libera", "notes").await.unwrap();
    home.rename(Kind::Provider, "ollama", "NOTES")
        .await
        .unwrap();
    assert_eq!(stored_names(&home).await, ["Notes"]);
    let config = home.load();
    assert!(config.channels.contains_key("notes"));
    assert!(config.providers.contains_key("NOTES"));
}

const NOTES: &str = r#"{ "url": "https://notes.example.com/mcp" }"#;

const NOTES_AGENT: &str = "models = [\"anthropic/claude\"]\n\
                           tools = [\"notes__*\", \"memory*\"]\n\
                           ask = [\"notes__delete\"]\n";

async fn add_stored(home: &Home, name: &str, spec: &str) {
    home.store
        .toolset_insert(name, spec, jiff::Timestamp::now())
        .await
        .unwrap();
}

async fn stored_names(home: &Home) -> Vec<String> {
    let rows = home.store.toolsets().await.unwrap();
    rows.into_iter().map(|row| row.name).collect()
}

/// A toolset stored in the database is renamed like a table of `giles.toml`: its row replaces
/// the table, `giles.toml` stays as it is, and the agents' globs follow the new name.
#[tokio::test]
async fn a_stored_toolset_is_renamed_with_its_row_and_agent_globs() {
    let home = Home::populated().await;
    add_stored(&home, "notes", NOTES).await;
    home.write("agents/jeeves/agent.toml", NOTES_AGENT);
    let before = home.store.toolsets().await.unwrap();

    let report = home.rename(Kind::Toolset, "notes", "memos").await.unwrap();

    assert_eq!(report.kind, Kind::Toolset);
    assert!(report.stored_toolset);
    assert!(!report.config_table);
    assert_eq!(report.legacy_routes, 0);
    assert_eq!(
        report.agent_files,
        [(home.path("agents/jeeves/agent.toml"), 2)]
    );
    assert!(report.moved.is_empty(), "{:?}", report.moved);
    // The row keeps its spec and its creation time.
    let rows = home.store.toolsets().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "memos");
    assert_eq!(rows[0].spec, before[0].spec);
    assert_eq!(rows[0].created_at, before[0].created_at);
    assert_eq!(home.read("giles.toml"), GILES_TOML);
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        NOTES_AGENT.replace("notes__", "memos__")
    );
}

#[tokio::test]
async fn a_stored_toolset_cannot_take_the_name_of_another_toolset() {
    let home = Home::populated().await;
    add_stored(&home, "notes", NOTES).await;
    add_stored(&home, "memos", NOTES).await;
    let before = home.store.toolsets().await.unwrap();

    // The `[toolset.*]` tables of `giles.toml` own their names, ignoring case. A channel or a
    // provider of the name is no toolset, so it doesn't count.
    let err = refused(&home, Kind::Toolset, "notes", "github").await;
    assert!(
        matches!(&err, RenameError::NewIsConfigured { kind: Kind::Toolset, old, .. }
            if old == "notes"),
        "{err}"
    );
    let err = refused(&home, Kind::Toolset, "notes", "GITHUB").await;
    assert!(matches!(err, RenameError::CaseCollision { .. }), "{err}");
    // So does another stored toolset, ignoring case.
    for new in ["memos", "MEMOS"] {
        let err = refused(&home, Kind::Toolset, "notes", new).await;
        assert!(
            matches!(&err, RenameError::StoredToolset { existing, .. } if existing == "memos"),
            "{new}: {err}"
        );
    }
    // And a name that has data: this command never gives an instance a name with data.
    home.write("data/toolset/leftover/x", "1");
    let err = refused(&home, Kind::Toolset, "notes", "leftover").await;
    assert!(matches!(err, RenameError::NewHasData { .. }), "{err}");
    assert_eq!(home.store.toolsets().await.unwrap(), before);
}

/// `giles.toml` ignores a `[toolset.<name>]` with `url`, and so does this command: the stored
/// toolset of that name is the one renamed, and the table's name is free.
#[tokio::test]
async fn an_ignored_remote_toolset_table_is_not_an_instance() {
    let with_ignored = format!(
        "{GILES_TOML}[toolset.notes]\nurl = \"https://notes.example.com/mcp\"\n\
         [toolset.memos]\nurl = \"https://memos.example.com/mcp\"\n"
    );
    let home = Home::new(&with_ignored).await;
    home.write("agents/jeeves/agent.toml", NOTES_AGENT);
    add_stored(&home, "notes", NOTES).await;

    // The stored toolset is renamed, to the name another ignored table has, and `giles.toml`
    // stays as it is.
    let report = home.rename(Kind::Toolset, "notes", "memos").await.unwrap();
    assert!(report.stored_toolset);
    assert!(!report.config_table);
    assert_eq!(stored_names(&home).await, ["memos"]);
    assert_eq!(home.read("giles.toml"), with_ignored);
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        NOTES_AGENT.replace("notes__", "memos__")
    );

    // Without a stored toolset, the name of an ignored table is no instance.
    let err = refused(&home, Kind::Toolset, "notes", "docs").await;
    assert!(matches!(err, RenameError::UnknownInstance { .. }), "{err}");

    // A table of `giles.toml` can't be renamed onto an ignored table: one would overwrite the
    // other.
    let err = refused(&home, Kind::Toolset, "github", "notes").await;
    assert!(
        matches!(&err, RenameError::Edit { reason, .. }
            if reason.contains("already has a [toolset.notes] table")),
        "{err}"
    );
    assert_eq!(home.read("giles.toml"), with_ignored);
}

/// The stored name is found in any case, and the case of a name can change.
#[tokio::test]
async fn a_stored_toolset_is_found_in_any_case_and_its_name_can_change_case() {
    let home = Home::populated().await;
    add_stored(&home, "Notes", NOTES).await;
    home.write(
        "agents/jeeves/agent.toml",
        &NOTES_AGENT.replace("notes__", "Notes__"),
    );

    // The name it has is no new name, in whatever case it is given.
    for old in ["Notes", "NOTES"] {
        let err = refused(&home, Kind::Toolset, old, "Notes").await;
        assert!(matches!(err, RenameError::SameName { .. }), "{old}: {err}");
    }

    let report = home.rename(Kind::Toolset, "NOTES", "notes").await.unwrap();
    assert!(report.stored_toolset);
    assert_eq!(stored_names(&home).await, ["notes"]);
    assert_eq!(home.read("agents/jeeves/agent.toml"), NOTES_AGENT);

    // Now it has the name, so the same request is the same name.
    let err = refused(&home, Kind::Toolset, "NOTES", "notes").await;
    assert!(matches!(err, RenameError::SameName { .. }), "{err}");
}

#[tokio::test]
async fn globs_that_match_a_stored_toolset_by_its_old_name_are_refused() {
    let home = Home::populated().await;
    add_stored(&home, "notes", NOTES).await;
    home.write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/claude\"]\ntools = [\"not*\"]\n",
    );
    let before = home.store.toolsets().await.unwrap();
    let err = refused(&home, Kind::Toolset, "notes", "memos").await;
    assert!(matches!(err, RenameError::Edit { .. }), "{err}");
    assert!(err.to_string().contains("tools entry \"not*\""), "{err}");
    assert_eq!(home.store.toolsets().await.unwrap(), before);
}

#[tokio::test]
async fn a_failing_stored_rename_puts_the_row_back_exactly() {
    let home = Home::populated().await;
    add_stored(&home, "notes", NOTES).await;
    home.write("agents/jeeves/agent.toml", NOTES_AGENT);
    home.write("agents/zed/agent.toml", NOTES_AGENT);
    let rows = home.store.toolsets().await.unwrap();
    let before = home.snapshot().await;

    // `jeeves` is rewritten, then `zed` can't be: its directory is read-only.
    let zed = home.path("agents/zed");
    fs::set_permissions(&zed, fs::Permissions::from_mode(0o500)).unwrap();
    let writable_anyway = fs::File::create(zed.join("probe")).is_ok();
    if writable_anyway {
        // Running as root: permissions don't stop us, so this failure can't be provoked.
        fs::set_permissions(&zed, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let err = home
        .rename(Kind::Toolset, "notes", "memos")
        .await
        .unwrap_err();
    fs::set_permissions(&zed, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(
        matches!(&err, RenameError::Failed { undo_errors, .. } if undo_errors.is_empty()),
        "{err}"
    );
    // The row is as it was, `updated_at` included, and so are the files.
    assert_eq!(home.store.toolsets().await.unwrap(), rows);
    assert_eq!(home.snapshot().await, before);
}

/// The row is renamed before the agents' files are rewritten. A run that stopped in between
/// leaves the row with the new name and the agents' globs with the old, and running the command
/// again rewrites the globs.
#[tokio::test]
async fn a_stored_rename_that_stopped_after_the_row_is_finished_by_running_it_again() {
    let home = Home::populated().await;
    add_stored(&home, "memos", NOTES).await;
    home.write("agents/jeeves/agent.toml", NOTES_AGENT);

    let report = home.rename(Kind::Toolset, "notes", "memos").await.unwrap();

    assert!(report.stored_toolset);
    assert!(report.moved.is_empty(), "{:?}", report.moved);
    assert_eq!(stored_names(&home).await, ["memos"]);
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        NOTES_AGENT.replace("notes__", "memos__")
    );

    // With nothing left under the old name there is nothing to finish: it is no instance.
    let err = refused(&home, Kind::Toolset, "notes", "memos").await;
    assert!(matches!(err, RenameError::UnknownInstance { .. }), "{err}");
}

/// A stored toolset may have the name of a channel: `giles.toml`'s `[channel.libera]` is no
/// toolset, so the toolset rename moves the stored row and the agents' globs and leaves the
/// channel alone.
#[tokio::test]
async fn a_stored_toolset_named_like_a_channel_is_renamed_without_touching_the_channel() {
    let home = Home::populated().await;
    add_stored(&home, "libera", NOTES).await;
    home.write(
        "agents/jeeves/agent.toml",
        &NOTES_AGENT.replace("notes__", "libera__"),
    );
    let rows = home.rows().await;
    assert_eq!(rows["libera"].sessions, 2);

    let report = home.rename(Kind::Toolset, "libera", "memos").await.unwrap();

    assert_eq!(report.kind, Kind::Toolset);
    assert!(report.stored_toolset);
    assert!(!report.config_table);
    assert_eq!(report.legacy_routes, 0);
    assert_eq!(
        report.agent_files,
        [(home.path("agents/jeeves/agent.toml"), 2)]
    );
    assert!(report.moved.is_empty(), "{:?}", report.moved);
    assert_eq!(stored_names(&home).await, ["memos"]);
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        NOTES_AGENT.replace("notes__", "memos__")
    );
    // The channel keeps its table, routes, rows, directories, and files.
    assert_eq!(home.read("giles.toml"), GILES_TOML);
    assert_eq!(home.rows().await, rows);
    assert!(home.path("data/channel/libera/state.json").is_file());
    assert!(
        home.path("agents/jeeves/groups/libera/%23giles/MEMORY.md")
            .is_file()
    );
    assert!(home.path("run/pids/channel/libera.pid").is_file());
}

/// The toolset rename resumes a stored rename that stopped after the row moved, even when a
/// channel has the old name.
#[tokio::test]
async fn a_stored_rename_is_finished_while_a_channel_has_the_old_name() {
    let home = Home::populated().await;
    add_stored(&home, "memos", NOTES).await;
    home.write(
        "agents/jeeves/agent.toml",
        &NOTES_AGENT.replace("notes__", "libera__"),
    );
    let rows = home.rows().await;

    let report = home.rename(Kind::Toolset, "libera", "memos").await.unwrap();

    assert!(report.stored_toolset);
    assert_eq!(stored_names(&home).await, ["memos"]);
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        NOTES_AGENT.replace("notes__", "memos__")
    );
    assert_eq!(home.read("giles.toml"), GILES_TOML);
    assert_eq!(home.rows().await, rows);
    assert!(home.path("data/channel/libera/state.json").is_file());
}

/// A stored toolset may be renamed to a name a channel or a provider has: each kind has its own
/// names, so the toolset takes nothing from them.
#[tokio::test]
async fn a_stored_toolset_may_take_the_name_of_a_channel_or_a_provider() {
    for new in ["libera", "ollama"] {
        let home = Home::populated().await;
        add_stored(&home, "notes", NOTES).await;
        home.write("agents/jeeves/agent.toml", NOTES_AGENT);
        let rows = home.rows().await;

        let report = home.rename(Kind::Toolset, "notes", new).await.unwrap();

        assert!(report.stored_toolset, "{new}");
        assert_eq!(stored_names(&home).await, [new]);
        assert_eq!(
            home.read("agents/jeeves/agent.toml"),
            NOTES_AGENT.replace("notes__", &format!("{new}__")),
            "{new}"
        );
        assert_eq!(home.read("giles.toml"), GILES_TOML, "{new}");
        assert_eq!(home.rows().await, rows, "{new}");
        assert!(home.path("data/channel/libera/state.json").is_file());
    }
}

/// The rules for the name of a stored toolset hold for a rename in the database too.
#[tokio::test]
async fn a_stored_toolset_cannot_be_renamed_to_a_reserved_name() {
    let home = Home::populated().await;
    add_stored(&home, "notes", NOTES).await;

    for new in ["new", "New", "NEW"] {
        let err = refused(&home, Kind::Toolset, "notes", new).await;
        assert!(
            matches!(&err, RenameError::StoredToolsetName { .. }),
            "{new}: {err}"
        );
        assert!(err.to_string().contains("new toolset"), "{err}");
    }

    // A table of `giles.toml` can still be named `new`: only a stored toolset can't.
    home.rename(Kind::Channel, "libera", "new").await.unwrap();
}

#[tokio::test]
async fn a_new_name_that_already_has_data_is_refused() {
    let home = Home::populated().await;
    // Database rows under the new name, from an instance that was removed.
    home.seed("old-irc", "other-provider").await;
    let err = refused(&home, Kind::Channel, "libera", "old-irc").await;
    assert!(
        matches!(&err, RenameError::NewHasData { what, .. } if what.contains("sessions")),
        "{err}"
    );
    assert!(err.to_string().contains("never merges"), "{err}");

    // Only a directory under the new name.
    home.write("data/channel/leftover/x", "1");
    let err = refused(&home, Kind::Channel, "libera", "leftover").await;
    assert!(
        matches!(&err, RenameError::NewHasData { what, .. } if what.contains("leftover")),
        "{err}"
    );
    home.write("agents/jeeves/groups/leftover2/c/MEMORY.md", "1");
    let err = refused(&home, Kind::Channel, "libera", "leftover2").await;
    assert!(matches!(err, RenameError::NewHasData { .. }), "{err}");
}

#[tokio::test]
async fn a_directory_at_the_target_of_a_move_is_refused() {
    // `libera` is mid-way through a previous rename (rows still old), but this time the same
    // directory exists on both sides: moving would overwrite or merge.
    let home = Home::populated().await;
    home.write("data/channel/libera-chat/other.json", "{}");
    let err = refused(&home, Kind::Channel, "libera", "libera-chat").await;
    assert!(matches!(err, RenameError::NewHasData { .. }), "{err}");
}

#[tokio::test]
async fn directories_of_an_unrelated_instance_are_not_taken_over() {
    // `libera` has rows and is still configured, so it isn't mid-way through a rename: a
    // directory called `libera-chat` is somebody else's (a removed instance's group memory).
    let home = Home::populated().await;
    home.write("agents/bunter/groups/libera-chat/c/MEMORY.md", "not yours");
    let err = refused(&home, Kind::Channel, "libera", "libera-chat").await;
    assert!(
        matches!(&err, RenameError::NewHasData { what, .. } if what.contains("libera-chat")),
        "{err}"
    );
    assert!(
        home.path("agents/bunter/groups/libera-chat/c/MEMORY.md")
            .is_file()
    );
}

#[tokio::test]
async fn globs_that_depend_on_the_old_toolset_name_stop_the_rename() {
    let home = Home::populated().await;
    home.write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/claude\"]\ntools = [\"*\"]\nask = [\"github*\"]\n",
    );
    let err = refused(&home, Kind::Toolset, "github", "gh").await;
    assert!(
        matches!(&err, RenameError::Edit { path, .. } if path.ends_with("agents/jeeves/agent.toml")),
        "{err}"
    );
    assert!(err.to_string().contains("ask entry \"github*\""), "{err}");
}

#[test]
fn a_commit_failure_is_a_failed_step_unless_nothing_was_done() {
    let store_error = || giles_store::Error::NotFound("x".into());
    assert!(matches!(
        store_failure(store_error(), false, Vec::new()),
        RenameError::Store(_)
    ));
    // The commit failed after the files changed and were undone: the files were touched.
    let err = store_failure(store_error(), true, Vec::new());
    assert!(
        matches!(&err, RenameError::Failed { step, undo_errors, .. }
            if step.contains("committing") && undo_errors.is_empty()),
        "{err}"
    );
    assert!(err.to_string().contains("Nothing was changed"), "{err}");
    let err = store_failure(store_error(), true, vec!["restore a".into()]);
    assert!(err.to_string().contains("partly done"), "{err}");
}

#[tokio::test]
async fn unconfigured_data_needs_the_new_name_configured() {
    let home = Home::new("[owner]\nidentities = [\"web:owner\"]\n").await;
    home.write("data/channel/gone/x", "1");
    let err = refused(&home, Kind::Channel, "gone", "fresh").await;
    assert!(matches!(err, RenameError::NotConfigured { .. }), "{err}");
    assert!(err.to_string().contains("[channel.fresh]"), "{err}");
}

#[tokio::test]
async fn a_missing_or_broken_giles_toml_is_refused() {
    let home = Home::populated().await;
    home.write("giles.toml", "[channel.libera");
    let err = refused(&home, Kind::Channel, "libera", "libera-chat").await;
    assert!(matches!(err, RenameError::Edit { .. }), "{err}");
    assert!(err.to_string().contains("giles.toml"), "{err}");

    fs::remove_file(home.path("giles.toml")).unwrap();
    let err = refused(&home, Kind::Channel, "libera", "libera-chat").await;
    assert!(matches!(err, RenameError::Io { .. }), "{err}");

    // An agent.toml that isn't TOML blocks a provider rename before anything moves.
    let home = Home::populated().await;
    home.write("agents/bunter/agent.toml", "models = [");
    let err = refused(&home, Kind::Provider, "ollama", "local").await;
    assert!(
        matches!(&err, RenameError::Edit { path, .. } if path.ends_with("agents/bunter/agent.toml")),
        "{err}"
    );
}

#[tokio::test]
async fn a_quoted_table_key_and_its_sub_tables_are_renamed() {
    // The sub-table `libera.extra` must follow its parent.
    let home = Home::populated().await;
    home.write(
        "giles.toml",
        &GILES_TOML.replace(
            "[channel.libera]",
            "[channel.\"libera\"]\n[channel.libera.extra]",
        ),
    );
    // Quoted keys are fine and must still rename; the check is about the result, not the form.
    let report = home
        .rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap();
    assert!(report.config_table);
    assert!(
        home.read("giles.toml")
            .contains("[channel.libera-chat.extra]")
    );
}

#[tokio::test]
async fn a_failing_step_undoes_the_steps_before_it() {
    let home = Home::populated().await;
    // Rewriting `agents/jeeves/agent.toml` works; `agents/zed/agent.toml` is in a directory the
    // rename can't write to.
    home.write("agents/zed/agent.toml", "models = [\"ollama/x\"]\n");
    let zed = home.path("agents/zed");
    fs::set_permissions(&zed, fs::Permissions::from_mode(0o500)).unwrap();
    let writable_anyway = fs::File::create(zed.join("probe")).is_ok();
    if writable_anyway {
        // Running as root: permissions don't stop us, so this failure can't be provoked.
        fs::set_permissions(&zed, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }

    let before = home.snapshot().await;
    let err = home
        .rename(Kind::Provider, "ollama", "local")
        .await
        .unwrap_err();
    fs::set_permissions(&zed, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(&err, RenameError::Failed { undo_errors, .. } if undo_errors.is_empty()),
        "{err}"
    );
    assert!(err.to_string().contains("Nothing was changed"), "{err}");
    assert!(
        matches!(&err, RenameError::Failed { step, .. } if step.contains("zed")),
        "{err}"
    );
    assert_eq!(home.snapshot().await, before);
}

#[test]
fn the_journal_takes_back_files_and_moves_in_reverse() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.toml");
    fs::write(&file, "old text").unwrap();
    let from = dir.path().join("from");
    fs::create_dir(&from).unwrap();
    fs::write(from.join("x"), "x").unwrap();
    let to = dir.path().join("to");
    let missing = dir.path().join("missing");

    let plan = Plan {
        files: vec![StagedFile {
            path: file.clone(),
            original: "old text".into(),
            text: "new text".into(),
            changes: 1,
        }],
        moves: vec![
            Move {
                from: from.clone(),
                to: to.clone(),
            },
            // The second move can't happen: its source isn't there.
            Move {
                from: missing.clone(),
                to: dir.path().join("elsewhere"),
            },
        ],
        agent_files: Vec::new(),
        config_table: true,
        routes: 0,
    };
    let journal = Journal::default();
    let (step, _) = plan.apply(&journal).unwrap_err();
    assert!(step.contains("missing"), "{step}");
    assert_eq!(fs::read_to_string(&file).unwrap(), "new text");
    assert!(to.join("x").is_file());

    assert!(journal.undo().is_empty());
    assert_eq!(fs::read_to_string(&file).unwrap(), "old text");
    assert!(from.join("x").is_file());
    assert!(!to.exists());
    // Undoing twice has nothing left to do.
    assert!(journal.undo().is_empty());
}

#[test]
fn undo_reports_what_it_could_not_take_back() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::default();
    journal.push(Applied::Move {
        from: dir.path().join("a"),
        to: dir.path().join("b"),
    });
    let errors = journal.undo();
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("move"), "{errors:?}");
    let message = RenameError::Failed {
        step: "moving a".into(),
        error: "boom".into(),
        undo_errors: errors,
    }
    .to_string();
    assert!(message.contains("partly done"), "{message}");
}

/// The agents' memory databases (`agents/<agent>/memory.db`): the group owners' channel instance
/// is renamed with the rest.
mod memory {
    use giles_store::{Memory, MemoryId, MemoryOwner, MemoryViewer, NewMemory};

    use super::*;

    /// What `seed_memories` wrote.
    struct Seeded {
        /// jeeves: a private group memory of `libera` `#giles`, and a shared one that group wrote.
        jeeves_group: Memory,
        jeeves_shared_by_group: Memory,
        /// jeeves: a user's private memory, and a group memory of the instance `web`.
        jeeves_user: Memory,
        jeeves_web: Memory,
        /// bunter: a private group memory of `libera` `nick`, and a shared memory of the owner.
        bunter_group: Memory,
        bunter_shared: Memory,
    }

    fn group(instance: &str, conversation: &str, can_write_shared: bool) -> MemoryViewer {
        MemoryViewer::new(
            MemoryOwner::Group {
                instance: instance.to_owned(),
                conversation: ConversationId::new(conversation).unwrap(),
            },
            can_write_shared,
        )
    }

    fn user(name: &str) -> MemoryViewer {
        MemoryViewer::new(MemoryOwner::User(UserId::new(name).unwrap()), true)
    }

    async fn open_db(path: PathBuf) -> MemoryStore {
        MemoryStore::open(path, OpenOptions::new()).await.unwrap()
    }

    async fn remember(
        store: &MemoryStore,
        viewer: &MemoryViewer,
        shared: bool,
        content: &str,
    ) -> Memory {
        let new = NewMemory {
            content: content.to_owned(),
            core: false,
            shared,
            source_session: None,
        };
        let now = jiff::Timestamp::from_second(1_800_000_000).unwrap();
        store.add(viewer, new, 2200, now).await.unwrap()
    }

    /// Whether `viewer` can read memory `id` of the database at `path`.
    async fn sees(path: PathBuf, viewer: &MemoryViewer, id: MemoryId) -> bool {
        open_db(path).await.get(viewer, id).await.is_ok()
    }

    /// `jeeves` and `bunter` each get a memory database with group memories on `libera`, and
    /// memories a rename must leave alone.
    async fn seed_memories(home: &Home) -> Seeded {
        let jeeves = open_db(home.path("agents/jeeves/memory.db")).await;
        let bunter = open_db(home.path("agents/bunter/memory.db")).await;
        Seeded {
            jeeves_group: remember(&jeeves, &group("libera", "#giles", false), false, "quiet")
                .await,
            jeeves_shared_by_group: remember(
                &jeeves,
                &group("libera", "#giles", true),
                true,
                "said on irc",
            )
            .await,
            jeeves_user: remember(&jeeves, &user("owner"), false, "likes tea").await,
            jeeves_web: remember(&jeeves, &group("web", "chat-1", false), false, "on web").await,
            bunter_group: remember(&bunter, &group("libera", "nick", false), false, "dm-ish").await,
            bunter_shared: remember(&bunter, &user("owner"), true, "for everyone").await,
        }
    }

    fn db(home: &Home, agent: &str) -> PathBuf {
        home.path(&format!("agents/{agent}/memory.db"))
    }

    #[tokio::test]
    async fn group_memories_follow_the_instance_to_its_new_name() {
        let home = Home::populated().await;
        let s = seed_memories(&home).await;

        let report = home.rename(Kind::Channel, "libera", "oftc").await.unwrap();
        // Agents in name order; the count is the memories whose group owner changed.
        assert_eq!(
            report.memory_dbs,
            [(db(&home, "bunter"), 1), (db(&home, "jeeves"), 2)]
        );

        let jeeves = db(&home, "jeeves");
        let (new_giles, old_giles) = (
            group("oftc", "#giles", false),
            group("libera", "#giles", false),
        );
        for id in [s.jeeves_group.id, s.jeeves_shared_by_group.id] {
            assert!(sees(jeeves.clone(), &new_giles, id).await);
        }
        // The private one isn't visible to the old name any more; the shared one still is.
        assert!(!sees(jeeves.clone(), &old_giles, s.jeeves_group.id).await);
        assert!(sees(jeeves.clone(), &old_giles, s.jeeves_shared_by_group.id).await);
        let author = open_db(jeeves.clone())
            .await
            .get(&user("owner"), s.jeeves_shared_by_group.id)
            .await
            .unwrap()
            .owner;
        assert_eq!(
            author,
            MemoryOwner::Group {
                instance: "oftc".to_owned(),
                conversation: ConversationId::new("#giles").unwrap(),
            }
        );
        let bunter = db(&home, "bunter");
        assert!(
            sees(
                bunter.clone(),
                &group("oftc", "nick", false),
                s.bunter_group.id
            )
            .await
        );
        assert!(
            !sees(
                bunter.clone(),
                &group("libera", "nick", false),
                s.bunter_group.id
            )
            .await
        );

        // Memories of a user, of the instance `web`, and shared ones are untouched.
        assert_eq!(
            open_db(jeeves.clone())
                .await
                .get(&user("owner"), s.jeeves_user.id)
                .await
                .unwrap(),
            s.jeeves_user
        );
        assert_eq!(
            open_db(jeeves.clone())
                .await
                .get(&group("web", "chat-1", false), s.jeeves_web.id)
                .await
                .unwrap(),
            s.jeeves_web
        );
        assert_eq!(
            open_db(bunter)
                .await
                .get(&group("elsewhere", "x", false), s.bunter_shared.id)
                .await
                .unwrap(),
            s.bunter_shared
        );
        // The rows and the group directories still move as before.
        assert_eq!(home.rows().await["oftc"].sessions, 2);
        assert!(
            home.path("agents/jeeves/groups/oftc/%23giles/MEMORY.md")
                .is_file()
        );
    }

    #[tokio::test]
    async fn a_removed_agents_memory_database_in_the_trash_is_left_alone() {
        let home = Home::populated().await;
        let s = seed_memories(&home).await;
        let trashed = home.path("trash/agents/jeeves-20261024T120000Z");
        fs::create_dir_all(trashed.parent().unwrap()).unwrap();
        fs::rename(home.path("agents/jeeves"), &trashed).unwrap();

        let report = home.rename(Kind::Channel, "libera", "oftc").await.unwrap();
        assert_eq!(report.memory_dbs, [(db(&home, "bunter"), 1)]);
        // The trashed copy keeps the old name, so restoring the agent restores what it had.
        let trashed_db = trashed.join("memory.db");
        assert!(
            sees(
                trashed_db.clone(),
                &group("libera", "#giles", false),
                s.jeeves_group.id
            )
            .await
        );
        assert!(
            !sees(
                trashed_db,
                &group("oftc", "#giles", false),
                s.jeeves_group.id
            )
            .await
        );
    }

    #[tokio::test]
    async fn agents_without_a_memory_database_are_skipped_and_none_is_created() {
        let home = Home::populated().await;
        let report = home.rename(Kind::Channel, "libera", "oftc").await.unwrap();
        assert!(report.memory_dbs.is_empty(), "{:?}", report.memory_dbs);
        for agent in ["jeeves", "bunter"] {
            assert!(!db(&home, agent).exists(), "{agent}");
        }
    }

    #[tokio::test]
    async fn a_run_after_a_partial_rename_finishes_the_databases_left() {
        let home = Home::populated().await;
        let s = seed_memories(&home).await;
        // The first attempt rewrote giles.toml and renamed the owners in bunter's database, then
        // died before jeeves's and the commit.
        home.write(
            "giles.toml",
            &GILES_TOML
                .replace("[channel.libera]", "[channel.oftc]")
                .replace("channel = \"libera\"", "channel = \"oftc\""),
        );
        let done = open_db(db(&home, "bunter")).await;
        assert_eq!(done.rename_instance("libera", "oftc").await.unwrap(), 1);
        drop(done);

        let report = home.rename(Kind::Channel, "libera", "oftc").await.unwrap();
        // Only what was left is counted: bunter's database has nothing under `libera` any more.
        assert_eq!(report.memory_dbs, [(db(&home, "jeeves"), 2)]);
        assert!(
            sees(
                db(&home, "jeeves"),
                &group("oftc", "#giles", false),
                s.jeeves_group.id
            )
            .await
        );
        assert!(
            sees(
                db(&home, "bunter"),
                &group("oftc", "nick", false),
                s.bunter_group.id
            )
            .await
        );
        assert_eq!(home.rows().await["oftc"].sessions, 2);
    }

    #[tokio::test]
    async fn a_database_that_cannot_be_renamed_undoes_the_ones_before_it() {
        let home = Home::populated().await;
        let s = seed_memories(&home).await;
        // `zed` sorts after the other agents, so theirs are renamed first.
        home.write("agents/zed/memory.db", &"not a database. ".repeat(40));

        let err = home
            .rename(Kind::Channel, "libera", "oftc")
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RenameError::Failed { step, undo_errors, .. }
                if step.contains("zed") && undo_errors.is_empty()),
            "{err}"
        );
        assert!(err.to_string().contains("Nothing was changed"), "{err}");

        // The memories, the rows, the configuration and the directories are as they were.
        assert!(
            sees(
                db(&home, "jeeves"),
                &group("libera", "#giles", false),
                s.jeeves_group.id
            )
            .await
        );
        assert!(
            sees(
                db(&home, "bunter"),
                &group("libera", "nick", false),
                s.bunter_group.id
            )
            .await
        );
        assert!(
            !sees(
                db(&home, "jeeves"),
                &group("oftc", "#giles", false),
                s.jeeves_group.id
            )
            .await
        );
        assert_eq!(home.read("giles.toml"), GILES_TOML);
        assert_eq!(home.rows().await["libera"].sessions, 2);
        assert!(home.path("data/channel/libera/state.json").is_file());
        assert!(home.path("agents/jeeves/groups/libera").is_dir());
    }
}

/// Names that nothing configures keep their rows and directories without keeping Giles from
/// starting, whatever is configured next to them.
#[tokio::test]
async fn the_runtime_starts_when_an_unconfigured_name_still_has_data() {
    use crate::runtime::Runtime;

    let by_hand = "[owner]\nidentities = [\"web:owner\"]\n[channel.libera-chat]\n\
                   component = \"irc\"\nunsandboxed = true\n";
    let home = Home::new(by_hand).await;
    home.write("data/channel/libera/state.json", "{}");
    home.seed("libera", "anthropic").await;
    Runtime::start(home.root.clone(), Arc::new(home.load()))
        .await
        .unwrap();
    assert_eq!(home.rows().await["libera"].sessions, 2);
    assert!(home.path("data/channel/libera/state.json").is_file());
}

/// Per-instance state is keyed by kind: renaming a channel moves its own files and leaves a
/// toolset's or provider's files of the same name where they are.
#[tokio::test]
async fn a_rename_leaves_the_files_other_kinds_keep_under_the_same_name() {
    let home = Home::populated().await;
    home.write("data/toolset/libera/x", "1");
    home.write("data/provider/libera/y", "2");
    home.write("cache/describe/provider/libera.json", "{}");
    home.write("cache/toolsets/libera.json", "{}");
    home.write("run/pids/toolset/libera.pid", "9\n");

    home.rename(Kind::Channel, "libera", "libera-chat")
        .await
        .unwrap();

    assert!(home.path("data/channel/libera-chat/state.json").is_file());
    assert!(
        home.path("cache/describe/channel/libera-chat.json")
            .is_file()
    );
    assert!(home.path("run/pids/channel/libera-chat.pid").is_file());
    for kept in [
        "data/toolset/libera/x",
        "data/provider/libera/y",
        "cache/describe/provider/libera.json",
        "cache/toolsets/libera.json",
        "run/pids/toolset/libera.pid",
    ] {
        assert!(home.path(kept).is_file(), "{kept}");
    }
}

/// A group directory is a channel's, wherever it lives: a provider or toolset rename of the same
/// name leaves it, and the memories its group owns, alone.
#[tokio::test]
async fn a_provider_rename_leaves_the_groups_and_memories_of_a_channel_of_that_name() {
    use giles_store::{MemoryOwner, MemoryViewer};

    let home = Home::populated().await;
    home.write("agents/jeeves/groups/ollama/c/MEMORY.md", "left behind");
    let db = home.path("agents/jeeves/memory.db");
    let viewer = MemoryViewer::new(
        MemoryOwner::Group {
            instance: "ollama".to_owned(),
            conversation: ConversationId::new("c").unwrap(),
        },
        false,
    );
    let memories = giles_store::MemoryStore::open(db.clone(), giles_store::OpenOptions::new())
        .await
        .unwrap();
    let new = giles_store::NewMemory {
        content: "left behind".to_owned(),
        core: false,
        shared: false,
        source_session: None,
    };
    let now = jiff::Timestamp::from_second(1_800_000_000).unwrap();
    let memory = memories.add(&viewer, new, 2200, now).await.unwrap();
    drop(memories);

    let report = home
        .rename(Kind::Provider, "ollama", "local")
        .await
        .unwrap();

    assert_eq!(report.kind, Kind::Provider);
    assert!(report.memory_dbs.is_empty(), "{:?}", report.memory_dbs);
    assert!(
        home.path("agents/jeeves/groups/ollama/c/MEMORY.md")
            .is_file()
    );
    assert!(!home.path("agents/jeeves/groups/local").exists());
    let owner = giles_store::MemoryStore::open(db, giles_store::OpenOptions::new())
        .await
        .unwrap()
        .get(&viewer, memory.id)
        .await
        .unwrap()
        .owner;
    assert_eq!(
        owner,
        MemoryOwner::Group {
            instance: "ollama".to_owned(),
            conversation: ConversationId::new("c").unwrap(),
        }
    );
}

/// A group directory under the new name belongs to a channel of that name, not to the provider
/// or toolset being renamed to it.
#[tokio::test]
async fn a_group_directory_of_the_new_name_does_not_stop_a_provider_or_toolset_rename() {
    let home = Home::populated().await;
    home.write("agents/x/groups/local/c/MEMORY.md", "a channel's");

    let report = home
        .rename(Kind::Provider, "ollama", "local")
        .await
        .unwrap();
    assert_eq!(report.kind, Kind::Provider);
    let report = home
        .rename(Kind::Toolset, "github", "local-tools")
        .await
        .unwrap();
    assert_eq!(report.kind, Kind::Toolset);

    assert!(home.path("agents/x/groups/local/c/MEMORY.md").is_file());
    assert!(home.path("data/toolset/local-tools/token-cache").is_file());
}
