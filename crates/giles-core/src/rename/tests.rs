//! `giles instance rename` and the startup check, against temporary data roots and databases.

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
url = "https://example.com/mcp"

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
        home.write("data/plugins/libera/state.json", "{\"cursor\": 7}");
        home.write("data/plugins/github/token-cache", "x");
        home.write("agents/jeeves/groups/libera/%23giles/MEMORY.md", "quiet");
        home.write(
            "agents/bunter/groups/web/chat-1/MEMORY.md",
            "other instance",
        );
        home.write("cache/describe/libera.json", "{}");
        home.write("cache/toolsets/github.json", "{}");
        home.write("run/pids/libera.pid", "123\n");
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

    async fn rename(&self, old: &str, new: &str) -> Result<RenameReport, RenameError> {
        rename_instance(&self.root, &self.store, &self.lock, old, new).await
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
        self.store.instance_rows().await.unwrap()
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

#[tokio::test]
async fn renaming_a_channel_moves_the_rows_the_directories_and_the_config_table() {
    let home = Home::populated().await;
    fs::set_permissions(home.path("giles.toml"), fs::Permissions::from_mode(0o600)).unwrap();
    let report = home.rename("libera", "libera-chat").await.unwrap();

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
        home.read("data/plugins/libera-chat/state.json"),
        "{\"cursor\": 7}"
    );
    assert!(!home.path("data/plugins/libera").exists());
    assert_eq!(
        home.read("agents/jeeves/groups/libera-chat/%23giles/MEMORY.md"),
        "quiet"
    );
    assert!(!home.path("agents/jeeves/groups/libera").exists());
    assert_eq!(
        home.read("agents/bunter/groups/web/chat-1/MEMORY.md"),
        "other instance"
    );
    assert!(home.path("data/plugins/github/token-cache").is_file());
    assert!(home.path("cache/describe/libera-chat.json").is_file());
    assert_eq!(home.read("run/pids/libera-chat.pid"), "123\n");
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
    let report = home.rename("libera", "libera-chat").await.unwrap();
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
    // Routes aren't data to the startup check, and the renamed configuration is fine.
    home.load();
    check_unused_instance_data(&home.root, &home.load(), &home.store)
        .await
        .unwrap();
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
    let report = home.rename("libera", "libera-chat").await.unwrap();
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
    let err = home.rename("libera", "libera-chat").await.unwrap_err();
    fs::set_permissions(home.dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(err, RenameError::Failed { .. }), "{err}");
    assert_eq!(route_channels(&home).await, before);
    assert_eq!(home.rows().await["libera"].sessions, 2);
}

#[tokio::test]
async fn routes_to_a_removed_instance_do_not_make_a_new_instance_look_like_a_rename() {
    let home = Home::new(
        "[owner]\nidentities = [\"web:owner\"]\n[channel.web]\n\
         [channel.oftc]\ncomponent = \"irc\"\nunsandboxed = true\n",
    )
    .await;
    // A route to `libera`, which was removed, and nothing else with that name.
    seed_routes(&home, "libera").await;
    check_unused_instance_data(&home.root, &home.load(), &home.store)
        .await
        .unwrap();
}

#[tokio::test]
async fn renaming_a_provider_rewrites_the_models_that_use_it() {
    let home = Home::populated().await;
    let report = home.rename("ollama", "local").await.unwrap();

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

    let report = home.rename("ollama", "local").await.unwrap();
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
    home.rename("libera", "oftc").await.unwrap();
    assert!(trashed.join("groups/libera/%23giles/MEMORY.md").is_file());
    assert!(!trashed.join("groups/oftc").exists());

    // Nothing in `agents/` at all is fine too.
    fs::remove_dir_all(home.path("agents")).unwrap();
    home.rename("github", "gh").await.unwrap();
    assert!(trashed.join("agent.toml").is_file());
}

/// A name that only a removed agent's trashed directories have data for doesn't make a fresh
/// instance in `giles.toml` look like a rename that was never carried out; the same data in
/// `agents/` does.
#[tokio::test]
async fn the_startup_check_does_not_read_the_trash() {
    let home =
        Home::new("[owner]\nidentities = [\"web:owner\"]\n[provider.anthropic]\n[channel.web]\n")
            .await;
    home.write("agents/jeeves/agent.toml", BUNTER);
    home.write("agents/jeeves/groups/libera/%23giles/MEMORY.md", "quiet");
    let refused = check_unused_instance_data(&home.root, &home.load(), &home.store)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("libera"), "{refused}");

    let trash = home.path("trash/agents/jeeves-20261024T120000Z");
    fs::create_dir_all(trash.parent().unwrap()).unwrap();
    fs::rename(home.path("agents/jeeves"), &trash).unwrap();
    assert!(trash.join("groups/libera/%23giles/MEMORY.md").is_file());
    check_unused_instance_data(&home.root, &home.load(), &home.store)
        .await
        .unwrap();
}

#[tokio::test]
async fn renaming_a_toolset_rewrites_the_tool_globs_and_moves_its_directory() {
    let home = Home::populated().await;
    let report = home.rename("github", "gh").await.unwrap();

    assert_eq!(report.kind, Kind::Toolset);
    assert_eq!(report.agent_files[0].1, 2);
    assert_eq!(
        home.read("agents/jeeves/agent.toml"),
        JEEVES.replace("github__", "gh__")
    );
    assert!(home.path("data/plugins/gh/token-cache").is_file());
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
    // What the startup check asks for: giles.toml already says `libera-chat`.
    let hand_edited = GILES_TOML
        .replace("libera", "libera-chat")
        .replace("irc.libera-chat.chat", "irc.libera.chat")
        .replace("irc:libera-chat:", "irc:libera:");
    let home = Home::new(&hand_edited).await;
    home.write("agents/jeeves/agent.toml", JEEVES);
    home.write("data/plugins/libera/state.json", "{}");
    home.seed("libera", "ollama").await;

    let report = home.rename("libera", "libera-chat").await.unwrap();
    assert!(!report.config_table);
    assert_eq!(report.legacy_routes, 0);
    assert_eq!(home.read("giles.toml"), hand_edited);
    assert!(home.path("data/plugins/libera-chat/state.json").is_file());
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
        home.path("data/plugins/libera"),
        home.path("data/plugins/libera-chat"),
    )
    .unwrap();
    home.rename("libera", "libera-chat").await.unwrap();
    assert!(home.path("data/plugins/libera-chat/state.json").is_file());
    assert!(home.path("agents/jeeves/groups/libera-chat").is_dir());
    assert_eq!(home.rows().await["libera-chat"].sessions, 2);
    home.load();
}

#[tokio::test]
async fn an_instance_without_data_is_just_renamed_in_the_config() {
    let home = Home::new(GILES_TOML).await;
    home.write("agents/jeeves/agent.toml", JEEVES);
    let report = home.rename("web", "chat").await.unwrap();
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
    home.rename("web", "chat").await.unwrap();
    home.rename("anthropic", "claude").await.unwrap();

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
async fn refused(home: &Home, old: &str, new: &str) -> RenameError {
    let before = home.snapshot().await;
    let err = home.rename(old, new).await.unwrap_err();
    assert_eq!(home.snapshot().await, before, "{err}");
    err
}

#[tokio::test]
async fn bad_requests_are_refused_without_changing_anything() {
    let home = Home::populated().await;

    for name in ["", "a b", "a/b", "..", "a_b", "é"] {
        let err = refused(&home, "libera", name).await;
        assert!(
            matches!(err, RenameError::InvalidName { .. }),
            "{name:?}: {err}"
        );
        let err = refused(&home, name, "fresh").await;
        assert!(
            matches!(err, RenameError::InvalidName { .. }),
            "{name:?}: {err}"
        );
    }

    let err = refused(&home, "libera", "libera").await;
    assert!(matches!(err, RenameError::SameName { .. }), "{err}");
    let err = refused(&home, "libera", "Libera").await;
    assert!(matches!(err, RenameError::SameName { .. }), "{err}");

    let err = refused(&home, "nothing", "fresh").await;
    assert!(matches!(err, RenameError::UnknownInstance { .. }), "{err}");
    assert!(
        err.to_string().contains("no instance named \"nothing\""),
        "{err}"
    );

    // Both configured: never a merge, even across kinds.
    let err = refused(&home, "libera", "web").await;
    assert!(matches!(err, RenameError::BothConfigured { .. }), "{err}");
    let err = refused(&home, "libera", "github").await;
    assert!(
        matches!(
            err,
            RenameError::BothConfigured {
                new_kind: Kind::Toolset,
                ..
            }
        ),
        "{err}"
    );

    // A new name that is another configured instance in a different case.
    let err = refused(&home, "libera", "WEB").await;
    assert!(matches!(err, RenameError::CaseCollision { .. }), "{err}");
}

#[tokio::test]
async fn a_new_name_that_already_has_data_is_refused() {
    let home = Home::populated().await;
    // Database rows under the new name, from an instance that was removed.
    home.seed("old-irc", "other-provider").await;
    let err = refused(&home, "libera", "old-irc").await;
    assert!(
        matches!(&err, RenameError::NewHasData { what, .. } if what.contains("sessions")),
        "{err}"
    );
    assert!(err.to_string().contains("never merges"), "{err}");

    // Only a directory under the new name.
    home.write("data/plugins/leftover/x", "1");
    let err = refused(&home, "libera", "leftover").await;
    assert!(
        matches!(&err, RenameError::NewHasData { what, .. } if what.contains("leftover")),
        "{err}"
    );
    home.write("agents/jeeves/groups/leftover2/c/MEMORY.md", "1");
    let err = refused(&home, "libera", "leftover2").await;
    assert!(matches!(err, RenameError::NewHasData { .. }), "{err}");
}

#[tokio::test]
async fn a_directory_at_the_target_of_a_move_is_refused() {
    // `libera` is mid-way through a previous rename (rows still old), but this time the same
    // directory exists on both sides: moving would overwrite or merge.
    let home = Home::populated().await;
    home.write("data/plugins/libera-chat/other.json", "{}");
    let err = refused(&home, "libera", "libera-chat").await;
    assert!(matches!(err, RenameError::NewHasData { .. }), "{err}");
}

#[tokio::test]
async fn directories_of_an_unrelated_instance_are_not_taken_over() {
    // `libera` has rows and is still configured, so it isn't mid-way through a rename: a
    // directory called `libera-chat` is somebody else's (a removed instance's group memory).
    let home = Home::populated().await;
    home.write("agents/bunter/groups/libera-chat/c/MEMORY.md", "not yours");
    let err = refused(&home, "libera", "libera-chat").await;
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
    let err = refused(&home, "github", "gh").await;
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
    home.write("data/plugins/gone/x", "1");
    let err = refused(&home, "gone", "fresh").await;
    assert!(matches!(err, RenameError::NotConfigured { .. }), "{err}");
    assert!(err.to_string().contains("[<kind>.fresh]"), "{err}");
}

#[tokio::test]
async fn a_missing_or_broken_giles_toml_is_refused() {
    let home = Home::populated().await;
    home.write("giles.toml", "[channel.libera");
    let err = refused(&home, "libera", "libera-chat").await;
    assert!(matches!(err, RenameError::Edit { .. }), "{err}");
    assert!(err.to_string().contains("giles.toml"), "{err}");

    fs::remove_file(home.path("giles.toml")).unwrap();
    let err = refused(&home, "libera", "libera-chat").await;
    assert!(matches!(err, RenameError::Io { .. }), "{err}");

    // An agent.toml that isn't TOML blocks a provider rename before anything moves.
    let home = Home::populated().await;
    home.write("agents/bunter/agent.toml", "models = [");
    let err = refused(&home, "ollama", "local").await;
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
    let report = home.rename("libera", "libera-chat").await.unwrap();
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
    let err = home.rename("ollama", "local").await.unwrap_err();
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

mod startup_check {
    use super::*;

    async fn check(home: &Home) -> Result<(), InstanceDataError> {
        check_unused_instance_data(&home.root, &home.load(), &home.store).await
    }

    #[tokio::test]
    async fn a_rename_done_in_the_config_alone_is_refused_until_the_command_runs() {
        let home = Home::populated().await;
        check(&home).await.unwrap();

        // The operator renames the table (and the route) by hand.
        home.write(
            "giles.toml",
            &GILES_TOML
                .replace("[channel.libera]", "[channel.libera-chat]")
                .replace("channel = \"libera\"", "channel = \"libera-chat\""),
        );
        let err = check(&home).await.unwrap_err();
        let message = err.to_string();
        assert!(matches!(err, InstanceDataError::Unused(_)), "{message}");
        assert!(
            message.contains("giles instance rename libera libera-chat"),
            "{message}"
        );
        assert!(message.contains("2 sessions"), "{message}");

        home.rename("libera", "libera-chat").await.unwrap();
        check(&home).await.unwrap();
    }

    #[tokio::test]
    async fn an_instance_removed_on_purpose_can_be_retired() {
        let home = Home::populated().await;
        // `libera` is removed (with its route) and an unrelated IRC network is added.
        let without = "[owner]\nidentities = [\"web:owner\"]\n[provider.anthropic]\n[channel.web]\n[channel.oftc]\ncomponent = \"irc\"\nunsandboxed = true\n";
        home.write("giles.toml", without);
        home.write("agents/jeeves/agent.toml", BUNTER);
        assert!(matches!(
            check(&home).await,
            Err(InstanceDataError::Unused(_))
        ));

        home.write(
            "giles.toml",
            &format!("[core]\nretired_instances = [\"libera\", \"github\"]\n{without}"),
        );
        check(&home).await.unwrap();
    }

    #[tokio::test]
    async fn adding_or_removing_alone_is_fine() {
        let home = Home::populated().await;
        // A new instance and nothing orphaned.
        home.write(
            "giles.toml",
            &format!("{GILES_TOML}[channel.oftc]\ncomponent = \"irc\"\nunsandboxed = true\n"),
        );
        check(&home).await.unwrap();
        // An instance removed and nothing added: `libera` is gone, the rest has data or is
        // referenced.
        home.write(
            "giles.toml",
            "[owner]\nidentities = [\"web:owner\"]\n[channel.web]\n[provider.anthropic]\n[provider.ollama]\ncomponent = \"openai\"\n[toolset.github]\nurl = \"https://example.com/mcp\"\n",
        );
        check(&home).await.unwrap();
    }

    #[tokio::test]
    async fn a_remote_toolset_is_never_taken_for_a_renamed_plugin() {
        // `github` is remote (`url`): it never has rows or a data directory, so it always looks
        // new. The directory of a stdio toolset that was removed must not make the check
        // refuse to start, and the check must not suggest moving that state onto `github`.
        let remote = "[owner]\nidentities = [\"web:owner\"]\n[toolset.github]\nurl = \"https://example.com/mcp\"\n";
        let home = Home::new(remote).await;
        home.write("data/plugins/files/state.json", "{}");
        check(&home).await.unwrap();

        // A launched toolset that has not started yet is a candidate, though.
        home.write(
            "giles.toml",
            "[owner]\nidentities = [\"web:owner\"]\n[toolset.docs]\n",
        );
        let err = check(&home).await.unwrap_err().to_string();
        assert!(err.contains("giles instance rename files docs"), "{err}");
    }

    #[tokio::test]
    async fn an_empty_installation_starts() {
        let home = Home::new(GILES_TOML).await;
        home.write("agents/jeeves/agent.toml", JEEVES);
        check(&home).await.unwrap();
    }

    #[tokio::test]
    async fn the_runtime_refuses_to_start_on_it() {
        use crate::runtime::{Runtime, StartError};

        let by_hand = "[owner]\nidentities = [\"web:owner\"]\n[channel.libera-chat]\n\
                       component = \"irc\"\nunsandboxed = true\n";
        let home = Home::new(by_hand).await;
        home.write("data/plugins/libera/state.json", "{}");
        home.seed("libera", "anthropic").await;
        let start = |home: &Home| Runtime::start(home.root.clone(), Arc::new(home.load()));
        let err = start(&home).await.unwrap_err();
        assert!(matches!(err, StartError::InstanceData(_)), "{err}");
        assert!(err.to_string().contains("giles instance rename"), "{err}");

        home.rename("libera", "libera-chat").await.unwrap();
        start(&home).await.unwrap();
    }
}
