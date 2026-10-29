//! End-to-end tests of `giles instance rename`, running the real binary on
//! a temporary data root with a populated database.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use giles_core::admin::InstanceLock;
use giles_core::paths::DataRoot;
use giles_store::{JournalMode, OpenOptions, SessionKey, SessionKind, Store};
use giles_types::{AgentId, ConversationId, UserId};

const GILES_TOML: &str = r#"# Giles
[owner]
identities = ["irc:libera:account:me"]

# The Libera network.
[channel.libera]
component = "irc"
unsandboxed = true

[[routes]]
channel = "libera"
agent = "jeeves"
"#;

const AGENT_TOML: &str = "models = [\"anthropic/claude\"]\n";

/// A data root with one agent, one IRC instance named `libera`, and a session on it.
async fn populated(dir: &Path) -> DataRoot {
    let root = DataRoot::new(dir, Path::new("/tmp")).unwrap();
    fs::create_dir_all(root.run_dir()).unwrap();
    fs::create_dir_all(root.data_dir()).unwrap();
    fs::write(
        root.config_file(),
        GILES_TOML.replace("[owner]", "[provider.anthropic]\n[owner]"),
    )
    .unwrap();
    let agent = dir.join("agents/jeeves");
    fs::create_dir_all(&agent).unwrap();
    fs::write(agent.join("agent.toml"), AGENT_TOML).unwrap();
    let plugin_data = root.plugin_data_dir("libera").unwrap();
    fs::create_dir_all(&plugin_data).unwrap();
    fs::write(plugin_data.join("state.json"), "{\"cursor\": 7}").unwrap();

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
    root
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

#[tokio::test]
async fn rename_moves_the_data_and_prints_what_it_did() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;

    let output = giles(dir.path(), &["instance", "rename", "libera", "libera-chat"]);
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
        .plugin_data_dir("libera-chat")
        .unwrap()
        .join("state.json");
    assert_eq!(fs::read_to_string(moved).unwrap(), "{\"cursor\": 7}");
    assert!(!root.plugin_data_dir("libera").unwrap().exists());

    let store = Store::open(root.database_file()).await.unwrap();
    let rows = store.instance_rows().await.unwrap();
    assert_eq!(rows["libera-chat"].sessions, 1);
    assert!(!rows.contains_key("libera"));
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

        let output = giles(dir.path(), &["instance", "rename", "libera", "libera-chat"]);
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
            store.instance_rows().await.unwrap()["libera-chat"].sessions,
            1
        );
    }
}

#[tokio::test]
async fn a_failed_rename_exits_nonzero_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;

    let output = giles(dir.path(), &["instance", "rename", "nothing", "other"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no instance named \"nothing\""),
        "{}",
        stderr(&output)
    );

    let output = giles(dir.path(), &["instance", "rename", "libera", "bad name"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("invalid instance name"),
        "{}",
        stderr(&output)
    );

    assert_eq!(
        fs::read_to_string(root.config_file())
            .unwrap()
            .replace("[provider.anthropic]\n", ""),
        GILES_TOML
    );
    assert!(root.plugin_data_dir("libera").unwrap().is_dir());
}

#[tokio::test]
async fn a_running_core_blocks_the_rename() {
    let dir = tempfile::tempdir().unwrap();
    let root = populated(dir.path()).await;
    let _daemon = InstanceLock::acquire(&root).unwrap();

    let output = giles(dir.path(), &["instance", "rename", "libera", "libera-chat"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("already running"),
        "{}",
        stderr(&output)
    );
    assert!(
        fs::read_to_string(root.config_file())
            .unwrap()
            .contains("[channel.libera]")
    );
}
