//! End-to-end tests of `giles channel rename`, `giles provider rename`, and `giles toolset rename`
//! with Giles stopped, running the real binary on a temporary data root with a populated database.
//! The toolset command with Giles running is tested with a real daemon in `serve.rs`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use giles_core::admin::InstanceLock;
use giles_core::config::Kind;
use giles_core::paths::DataRoot;
use giles_store::{JournalMode, OpenOptions, SessionKey, SessionKind, Store, UsageRecord};
use giles_types::{AgentId, ConversationId, UserId};

const GILES_TOML: &str = r#"# Giles
[owner]
identities = ["irc:libera:account:me"]

[provider.anthropic]

# The Libera network.
[channel.libera]
component = "irc"
unsandboxed = true

# A local server.
[toolset.github]
command = "/bin/github"

[[routes]]
channel = "libera"
agent = "jeeves"
"#;

const AGENT_TOML: &str = "models = [\"anthropic/claude\"]\n\
                          tools = [\"github__*\", \"notes__*\"]\n\
                          ask = [\"notes__delete\"]\n";

/// A stored toolset: a remote server, with no table in `giles.toml`.
const NOTES_SPEC: &str = r#"{ "url": "https://notes.example.com/mcp" }"#;

/// A data root with one agent, an IRC instance named `libera` with a session, a provider
/// `anthropic` with a usage row, a `github` toolset in `giles.toml`, and a stored toolset `notes`.
async fn populated(dir: &Path) -> DataRoot {
    let root = DataRoot::new(dir, Path::new("/tmp")).unwrap();
    fs::create_dir_all(root.run_dir()).unwrap();
    fs::create_dir_all(root.data_dir()).unwrap();
    fs::write(root.config_file(), GILES_TOML).unwrap();
    let agent = dir.join("agents/jeeves");
    fs::create_dir_all(&agent).unwrap();
    fs::write(agent.join("agent.toml"), AGENT_TOML).unwrap();
    let plugin_data = root.plugin_data_dir(Kind::Channel, "libera").unwrap();
    fs::create_dir_all(&plugin_data).unwrap();
    fs::write(plugin_data.join("state.json"), "{\"cursor\": 7}").unwrap();
    let toolset_data = root.plugin_data_dir(Kind::Toolset, "github").unwrap();
    fs::create_dir_all(&toolset_data).unwrap();
    fs::write(toolset_data.join("cache"), "x").unwrap();

    let store = Store::open(root.database_file()).await.unwrap();
    let key = SessionKey {
        agent: AgentId::new("jeeves").unwrap(),
        channel: "libera".into(),
        conversation: ConversationId::new("#giles").unwrap(),
    };
    store
        .open_session(
            key,
            SessionKind::Group,
            Some(UserId::new("owner").unwrap()),
            jiff::Timestamp::from_second(1_800_000_000).unwrap(),
        )
        .await
        .unwrap();
    record_usage(&store, "anthropic").await;
    store
        .toolset_insert("notes", NOTES_SPEC, jiff::Timestamp::now())
        .await
        .unwrap();
    root
}

/// One request of `provider` in the usage table.
async fn record_usage(store: &Store, provider: &str) {
    store
        .record_usage(UsageRecord {
            at: jiff::Timestamp::from_second(1_800_000_003).unwrap(),
            provider: provider.into(),
            model: "claude".into(),
            agent: AgentId::new("jeeves").unwrap(),
            user: None,
            session: None,
            usage: Default::default(),
            fallback: false,
        })
        .await
        .unwrap();
}

fn giles(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_giles"))
        .args(args)
        .arg("--home")
        .arg(home)
        .env_remove("GILES_HOME")
        .env_remove("RUST_LOG")
        .env("GILES_LOG", "warn")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read(root: &DataRoot, relative: &str) -> String {
    fs::read_to_string(root.root().join(relative)).unwrap()
}

#[tokio::test]
async fn channel_rename_moves_the_data_and_prints_what_it_did() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;

    let output = giles(dir.path(), &["channel", "rename", "libera", "libera-chat"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains("Renamed channel instance \"libera\" to \"libera-chat\"."),
        "{out}"
    );
    assert!(out.contains("1 sessions"), "{out}");
    assert!(
        out.contains("[channel.libera] -> [channel.libera-chat]"),
        "{out}"
    );

    let config = fs::read_to_string(root.config_file()).unwrap();
    assert!(
        config.contains("# The Libera network.\n[channel.libera-chat]"),
        "{config}"
    );
    assert!(config.contains("channel = \"libera-chat\""), "{config}");
    let moved = root
        .plugin_data_dir(Kind::Channel, "libera-chat")
        .unwrap()
        .join("state.json");
    assert_eq!(fs::read_to_string(moved).unwrap(), "{\"cursor\": 7}");
    assert!(
        !root
            .plugin_data_dir(Kind::Channel, "libera")
            .unwrap()
            .exists()
    );

    let store = Store::open(root.database_file()).await.unwrap();
    let rows = store.instance_rows(Kind::Channel).await.unwrap();
    assert_eq!(rows["libera-chat"].sessions, 1);
    assert!(!rows.contains_key("libera"));
}

#[tokio::test]
async fn provider_rename_moves_the_usage_and_rewrites_the_agents_models() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;

    let output = giles(dir.path(), &["provider", "rename", "anthropic", "claude"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains("Renamed provider instance \"anthropic\" to \"claude\"."),
        "{out}"
    );
    assert!(out.contains("1 usage rows"), "{out}");
    assert!(
        out.contains("[provider.anthropic] -> [provider.claude]"),
        "{out}"
    );
    assert!(
        out.contains("agents/jeeves/agent.toml: 1 reference(s) updated"),
        "{out}"
    );

    let config = read(&root, "giles.toml");
    assert!(config.contains("[provider.claude]"), "{config}");
    assert!(!config.contains("[provider.anthropic]"), "{config}");
    assert!(
        read(&root, "agents/jeeves/agent.toml").contains("models = [\"claude/claude\"]"),
        "{}",
        read(&root, "agents/jeeves/agent.toml")
    );
    let store = Store::open(root.database_file()).await.unwrap();
    let providers = store.instance_rows(Kind::Provider).await.unwrap();
    assert_eq!(providers["claude"].usage, 1);
    assert!(!providers.contains_key("anthropic"));
    // The channel and the toolsets are not touched.
    let channels = store.instance_rows(Kind::Channel).await.unwrap();
    assert_eq!(channels["libera"].sessions, 1);
    assert!(config.contains("[channel.libera]"), "{config}");
    assert!(config.contains("[toolset.github]"), "{config}");
}

#[tokio::test]
async fn toolset_rename_with_giles_stopped_renames_a_table_of_giles_toml() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;

    let output = giles(dir.path(), &["toolset", "rename", "github", "gh"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains("Renamed toolset instance \"github\" to \"gh\"."),
        "{out}"
    );
    assert!(out.contains("[toolset.github] -> [toolset.gh]"), "{out}");

    let config = read(&root, "giles.toml");
    assert!(
        config.contains("# A local server.\n[toolset.gh]\ncommand = \"/bin/github\""),
        "{config}"
    );
    let agent = read(&root, "agents/jeeves/agent.toml");
    assert!(agent.contains("\"gh__*\""), "{agent}");
    assert!(agent.contains("\"notes__*\""), "{agent}");
    assert!(
        root.plugin_data_dir(Kind::Toolset, "gh")
            .unwrap()
            .join("cache")
            .is_file()
    );
    assert!(
        !root
            .plugin_data_dir(Kind::Toolset, "github")
            .unwrap()
            .exists()
    );
}

#[tokio::test]
async fn toolset_rename_with_giles_stopped_renames_a_stored_toolset() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;

    let output = giles(dir.path(), &["toolset", "rename", "notes", "memos"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains("Renamed toolset instance \"notes\" to \"memos\"."),
        "{out}"
    );
    assert!(
        out.contains("database: the stored toolset \"notes\" is now \"memos\""),
        "{out}"
    );
    assert!(!out.contains("giles.toml"), "{out}");

    // giles.toml has no table for it, so it is left as it was.
    assert_eq!(read(&root, "giles.toml"), GILES_TOML);
    let agent = read(&root, "agents/jeeves/agent.toml");
    assert!(agent.contains("\"memos__*\""), "{agent}");
    assert!(agent.contains("ask = [\"memos__delete\"]"), "{agent}");
    assert!(agent.contains("\"github__*\""), "{agent}");
    let store = Store::open(root.database_file()).await.unwrap();
    let names: Vec<String> = store
        .toolsets()
        .await
        .unwrap()
        .into_iter()
        .map(|toolset| toolset.name)
        .collect();
    assert_eq!(names, ["memos"]);
}

/// Each command moves what its kind keys and leaves what another kind of the same name keeps:
/// the name is what the command's kind makes of it.
#[tokio::test]
async fn a_rename_leaves_what_another_kind_keeps_under_the_same_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;
    // What removed instances of the other kinds named `libera` left behind.
    for kind in [Kind::Provider, Kind::Toolset] {
        let left = root.plugin_data_dir(kind, "libera").unwrap();
        fs::create_dir_all(&left).unwrap();
        fs::write(left.join("left"), "behind").unwrap();
    }
    let store = Store::open(root.database_file()).await.unwrap();
    record_usage(&store, "libera").await;
    drop(store);

    let output = giles(dir.path(), &["channel", "rename", "libera", "libera-chat"]);
    assert!(output.status.success(), "{}", stderr(&output));

    for kind in [Kind::Provider, Kind::Toolset] {
        let left = root.plugin_data_dir(kind, "libera").unwrap();
        assert_eq!(fs::read_to_string(left.join("left")).unwrap(), "behind");
        assert!(!root.plugin_data_dir(kind, "libera-chat").unwrap().exists());
    }
    assert!(
        root.plugin_data_dir(Kind::Channel, "libera-chat")
            .unwrap()
            .join("state.json")
            .is_file()
    );
    let store = Store::open(root.database_file()).await.unwrap();
    let providers = store.instance_rows(Kind::Provider).await.unwrap();
    assert_eq!(providers["libera"].usage, 1);
    assert!(!providers.contains_key("libera-chat"));
    let channels = store.instance_rows(Kind::Channel).await.unwrap();
    assert_eq!(channels["libera-chat"].sessions, 1);
}

/// What a fresh connection finds recorded in the database file: WAL, or else `delete`.
fn recorded_journal_mode(root: &DataRoot) -> String {
    rusqlite::Connection::open(root.database_file())
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap()
}

/// The rename runs without loading the configuration, so it can't know the `journal_mode` the
/// daemon uses; it must leave the database in the mode it is in, which is what a data root on a
/// file system without WAL needs.
#[tokio::test]
async fn rename_leaves_the_database_in_the_journal_mode_it_is_in() {
    for mode in [JournalMode::Wal, JournalMode::Delete] {
        let dir = tempfile::tempdir().unwrap();
        let root = populated(dir.path()).await;
        drop(
            Store::open_with(root.database_file(), OpenOptions::new().journal_mode(mode))
                .await
                .unwrap(),
        );
        let recorded = if mode == JournalMode::Wal {
            "wal"
        } else {
            "delete"
        };
        assert_eq!(recorded_journal_mode(&root), recorded);

        let output = giles(dir.path(), &["channel", "rename", "libera", "libera-chat"]);
        assert!(output.status.success(), "{mode}: {}", stderr(&output));
        assert_eq!(recorded_journal_mode(&root), recorded, "{mode}");
        for suffix in ["-wal", "-journal"] {
            let mut name = root.database_file().into_os_string();
            name.push(suffix);
            assert!(!Path::new(&name).exists(), "{mode}: {suffix} left behind");
        }
        let store = Store::open_with(root.database_file(), OpenOptions::new().journal_mode(mode))
            .await
            .unwrap();
        assert_eq!(
            store.instance_rows(Kind::Channel).await.unwrap()["libera-chat"].sessions,
            1
        );
    }
}

#[tokio::test]
async fn a_failed_rename_exits_nonzero_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;

    for (args, reason) in [
        (
            ["channel", "rename", "nothing", "other"],
            "no channel named \"nothing\"",
        ),
        (
            ["provider", "rename", "nothing", "other"],
            "no provider named \"nothing\"",
        ),
        (
            ["toolset", "rename", "nothing", "other"],
            "no toolset named \"nothing\"",
        ),
        (
            ["channel", "rename", "libera", "bad name"],
            "invalid instance name",
        ),
        (
            ["provider", "rename", "anthropic", "bad name"],
            "invalid instance name",
        ),
        (
            ["toolset", "rename", "github", "bad name"],
            "invalid instance name",
        ),
    ] {
        let output = giles(dir.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        assert!(stdout(&output).is_empty(), "{args:?}: {}", stdout(&output));
        assert!(
            stderr(&output).contains(reason),
            "{args:?}: {}",
            stderr(&output)
        );
    }

    assert_eq!(read(&root, "giles.toml"), GILES_TOML);
    assert_eq!(read(&root, "agents/jeeves/agent.toml"), AGENT_TOML);
    assert!(
        root.plugin_data_dir(Kind::Channel, "libera")
            .unwrap()
            .is_dir()
    );
}

/// The command says which kind to rename, so a name that is another kind's is refused, with the
/// command that renames it, and nothing changes.
#[tokio::test]
async fn a_command_for_another_kind_of_instance_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;

    for (args, hint) in [
        (
            ["provider", "rename", "libera", "other"],
            "run `giles channel rename libera other`",
        ),
        (
            ["toolset", "rename", "libera", "other"],
            "run `giles channel rename libera other`",
        ),
        (
            ["channel", "rename", "anthropic", "other"],
            "run `giles provider rename anthropic other`",
        ),
        (
            ["toolset", "rename", "anthropic", "other"],
            "run `giles provider rename anthropic other`",
        ),
        (
            ["channel", "rename", "github", "other"],
            "run `giles toolset rename github other`",
        ),
        (
            ["provider", "rename", "github", "other"],
            "run `giles toolset rename github other`",
        ),
        // A stored toolset is a toolset too.
        (
            ["channel", "rename", "notes", "other"],
            "run `giles toolset rename notes other`",
        ),
        (
            ["provider", "rename", "notes", "other"],
            "run `giles toolset rename notes other`",
        ),
    ] {
        let output = giles(dir.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        assert!(
            stderr(&output).contains(hint),
            "{args:?}: {}",
            stderr(&output)
        );
    }

    assert_eq!(read(&root, "giles.toml"), GILES_TOML);
    assert_eq!(read(&root, "agents/jeeves/agent.toml"), AGENT_TOML);
    let store = Store::open(root.database_file()).await.unwrap();
    assert_eq!(store.toolsets().await.unwrap()[0].name, "notes");
}

#[tokio::test]
async fn a_running_core_blocks_the_channel_and_provider_renames() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;
    let _daemon = InstanceLock::acquire(&root).unwrap();

    for args in [
        ["channel", "rename", "libera", "libera-chat"],
        ["provider", "rename", "anthropic", "claude"],
    ] {
        let output = giles(dir.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        assert!(
            stderr(&output).contains("already running"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    let config = read(&root, "giles.toml");
    assert!(config.contains("[channel.libera]"), "{config}");
    assert!(config.contains("[provider.anthropic]"), "{config}");
}

#[test]
fn the_instance_command_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let output = giles(dir.path(), &["instance", "rename", "libera", "libera-chat"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("unrecognized subcommand 'instance'"),
        "{}",
        stderr(&output)
    );
}
