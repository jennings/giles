//! The toolset manager with the fake connector of `tests.rs`: stored toolsets reach the registry
//! and the host, specs are refused with their key and nothing stored (a stored toolset is remote,
//! with a bearer token from `secrets.toml`), secrets are read again, startup reports toolsets
//! that fail, and a rename changes the row, the agents' globs, and the running toolset together,
//! or none of them.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use giles_store::{JournalMode, Store};
use giles_types::AgentId;

use super::cache::ToolCache;
use super::host::ToolsetHost;
use super::manager::{
    AgentGlobChange, GlobChange, ToolsetManager, ToolsetManagerError, ToolsetManagerOptions,
    ToolsetSource, ToolsetView,
};
use super::tests::{FakeConnector, Script, settle};
use crate::admin::{PluginControl, PluginRunState, PluginRuntime};
use crate::agents::remove::FakeActivity;
use crate::agents::{AgentEdit, AgentManager, AgentManagerOptions};
use crate::config::{
    Config, InstanceName, KeyPath, Kind, LoadOptions, ToolsetSpec, ToolsetTransport,
};
use crate::memory_dbs::MemoryDbs;
use crate::paths::DataRoot;
use crate::registry::Registry;
use crate::routes::Routes;
use crate::scheduler::JobChanges;

const FILES: &str = r#"{ "url": "https://files.example.com/mcp" }"#;

fn no_env(_: &str) -> Option<OsString> {
    None
}

fn load_options() -> LoadOptions<'static> {
    LoadOptions {
        env: &no_env,
        giles_exe: None,
        ..LoadOptions::default()
    }
}

fn spec(json: &str) -> ToolsetSpec {
    ToolsetSpec::from_json(json).unwrap_or_else(|e| panic!("{e}"))
}

fn write_secrets(root: &DataRoot, text: &str) {
    std::fs::write(root.secrets_file(), text).unwrap();
    std::fs::set_permissions(root.secrets_file(), std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A data root with a `giles.toml`, a database, a registry, and a host running the toolsets of
/// `giles.toml` against the fake connector, and a manager over them.
struct Fixture {
    _dir: tempfile::TempDir,
    root: DataRoot,
    store: Store,
    registry: Arc<Registry>,
    connector: Arc<FakeConnector>,
    host: Arc<ToolsetHost>,
    manager: Arc<ToolsetManager>,
    agent_lock: Arc<tokio::sync::Mutex<()>>,
}

impl Fixture {
    /// `extra` is appended to a minimal `giles.toml`. `stored` are the rows in the database when
    /// the manager starts; `scripts` are the toolsets the fake connector can connect to.
    async fn start(
        extra: &str,
        stored: &[(&str, &str)],
        scripts: Vec<(&'static str, Script)>,
    ) -> Self {
        Self::start_with_agents(extra, stored, &[], scripts).await
    }

    /// [`start`](Self::start) with the agents `(id, agent.toml)`, which are loaded with the
    /// configuration.
    async fn start_with_agents(
        extra: &str,
        stored: &[(&str, &str)],
        agents: &[(&str, &str)],
        scripts: Vec<(&'static str, Script)>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        std::fs::write(
            root.config_file(),
            format!("[owner]\nidentities = [\"web:owner\"]\n{extra}"),
        )
        .unwrap();
        for (id, toml) in agents {
            let agent = root.agents_dir().join(id);
            std::fs::create_dir_all(&agent).unwrap();
            std::fs::write(agent.join("agent.toml"), toml).unwrap();
        }
        let config = Config::load_with(&root, &load_options()).unwrap_or_else(|e| panic!("{e}"));
        let store = Store::open(root.database_file()).await.unwrap();
        for (name, json) in stored {
            store
                .toolset_insert(name, json, giles_types::Timestamp::now())
                .await
                .unwrap();
        }
        let registry = Arc::new(Registry::new(root.clone(), Arc::new(config)));
        let connector = FakeConnector::with(scripts);
        let cache = ToolCache::new(root.cache_dir().join("toolsets"));
        let host = Arc::new(ToolsetHost::start_with(connector.clone(), &registry, cache).await);
        host.started().await;
        let agent_lock = Arc::new(tokio::sync::Mutex::new(()));
        let manager = ToolsetManager::start(ToolsetManagerOptions {
            store: store.clone(),
            root: root.clone(),
            config: registry.live_config().clone(),
            host: host.clone(),
            agent_lock: agent_lock.clone(),
        })
        .await
        .unwrap();
        Self {
            _dir: dir,
            root,
            store,
            registry,
            connector,
            host,
            manager,
            agent_lock,
        }
    }

    /// The tools new sessions get, once background updates have settled.
    async fn offered(&self) -> Vec<String> {
        settle().await;
        self.registry
            .tools()
            .snapshot()
            .tools()
            .iter()
            .map(|t| t.spec.name.clone())
            .collect()
    }

    /// The toolsets of the running configuration, by name.
    fn configured(&self) -> Vec<String> {
        self.registry
            .config()
            .toolsets
            .keys()
            .map(ToString::to_string)
            .collect()
    }

    /// What the host reports for the toolsets it supervises, by name.
    fn supervised(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .host
            .plugins()
            .into_iter()
            .map(|p| p.instance.to_string())
            .collect();
        names.sort();
        names
    }

    /// The rows in the database, by name.
    async fn stored(&self) -> Vec<String> {
        self.store
            .toolsets()
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.name)
            .collect()
    }

    /// The stored toolsets that failed, with their reasons.
    fn failures(&self) -> Vec<(String, String)> {
        let mut failures: Vec<(String, String)> = self
            .manager
            .plugins()
            .into_iter()
            .map(|p: PluginRuntime| {
                assert_eq!(p.state, PluginRunState::Failed);
                (p.instance.to_string(), p.last_error.unwrap())
            })
            .collect();
        failures.sort();
        failures
    }
}

fn invalid(error: ToolsetManagerError) -> (KeyPath, String) {
    match error {
        ToolsetManagerError::Invalid { key, reason } => (key, reason),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

fn stamps(view: &ToolsetView) -> (giles_types::Timestamp, giles_types::Timestamp) {
    match view.source {
        ToolsetSource::Stored {
            created_at,
            updated_at,
        } => (created_at, updated_at),
        ToolsetSource::File => panic!("{view:?} is not stored"),
    }
}

#[tokio::test]
async fn add_update_and_remove_reach_the_store_the_registry_and_the_host() {
    let f = Fixture::start("", &[], vec![("files", Script::new(&["read"]))]).await;
    assert!(f.configured().is_empty());

    let view = f.manager.add("files", &spec(FILES)).await.unwrap();
    assert_eq!(view.name, "files");
    assert_eq!(view.spec, Some(spec(FILES)));
    assert_eq!(view.failure, None);
    let (created, updated) = stamps(&view);
    assert_eq!(created, updated);
    assert_eq!(f.stored().await, ["files"]);
    let row = &f.store.toolsets().await.unwrap()[0];
    assert_eq!(ToolsetSpec::from_json(&row.spec).unwrap(), spec(FILES));
    assert_eq!(f.configured(), ["files"]);
    assert_eq!(f.supervised(), ["files"]);
    assert_eq!(f.offered().await, ["files__read"]);
    assert_eq!(f.connector.connects("files"), 1);

    // An edit restarts it with the new spec, and its time moves on.
    let edited = spec(r#"{ "url": "https://files.example.com/v2/mcp" }"#);
    let view = f
        .manager
        .update("files", &edited, Some(updated))
        .await
        .unwrap();
    assert_eq!(view.spec, Some(edited.clone()));
    let (_, edited_at) = stamps(&view);
    assert!(edited_at > updated);
    settle().await;
    assert_eq!(f.connector.connects("files"), 2);
    assert_eq!(f.connector.live("files"), 1, "the old connection is gone");
    let config = f.registry.config();
    let ToolsetTransport::Http(remote) = &config.toolsets["files"].transport else {
        panic!("not a remote toolset");
    };
    assert_eq!(remote.url.as_str(), "https://files.example.com/v2/mcp");
    assert_eq!(f.offered().await, ["files__read"]);

    // An update made from a stale read changes nothing.
    let err = f
        .manager
        .update("files", &spec(FILES), Some(updated))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolsetManagerError::Stale), "{err:?}");
    assert_eq!(f.manager.get("files").await.unwrap().spec, Some(edited));
    assert_eq!(f.connector.connects("files"), 2);
    // Without an expected time the update goes through.
    f.manager.update("Files", &spec(FILES), None).await.unwrap();

    // Removal stops it, withdraws its tools, and deletes the row.
    let removed = f.manager.remove("FILES").await.unwrap();
    assert_eq!(removed.name, "files");
    settle().await;
    assert!(f.stored().await.is_empty());
    assert!(f.configured().is_empty());
    assert!(f.supervised().is_empty());
    assert!(f.registry.toolset("files").is_none());
    assert!(f.offered().await.is_empty());
    assert_eq!(f.connector.live("files"), 0);
    for err in [
        f.manager.remove("files").await.unwrap_err(),
        f.manager
            .update("files", &spec(FILES), None)
            .await
            .unwrap_err(),
        f.manager.get("files").await.unwrap_err(),
    ] {
        assert!(matches!(err, ToolsetManagerError::NotFound), "{err:?}");
    }
}

#[tokio::test]
async fn an_invalid_spec_is_refused_with_its_key_and_nothing_is_stored() {
    let f = Fixture::start("", &[], vec![("files", Script::new(&["read"]))]).await;

    let bad_url = spec(r#"{ "url": "ftp://files.example.com/mcp" }"#);
    let (key, _) = invalid(f.manager.add("files", &bad_url).await.unwrap_err());
    assert_eq!(key, KeyPath::root().key("url"));

    let (key, _) = invalid(f.manager.add("bad name", &spec(FILES)).await.unwrap_err());
    assert!(key.is_root(), "{key}");

    // Nothing was stored, configured, or started.
    assert!(f.stored().await.is_empty());
    assert!(f.configured().is_empty());
    assert!(f.supervised().is_empty());
    assert_eq!(f.connector.connects("files"), 0);

    // An edit is refused the same way and leaves the running toolset alone.
    f.manager.add("files", &spec(FILES)).await.unwrap();
    let before = f.store.toolsets().await.unwrap();
    let (key, _) = invalid(f.manager.update("files", &bad_url, None).await.unwrap_err());
    assert_eq!(key, KeyPath::root().key("url"));
    assert_eq!(f.store.toolsets().await.unwrap(), before);
    settle().await;
    assert_eq!(f.connector.connects("files"), 1);
    assert_eq!(f.supervised(), ["files"]);
}

#[tokio::test]
async fn a_stored_toolset_is_a_remote_server_with_its_token_in_secrets_toml() {
    let f = Fixture::start("", &[], vec![("files", Script::new(&["read"]))]).await;
    let url = r#""url": "https://files.example.com/mcp""#;

    // A stdio server is configured in giles.toml.
    let (key, reason) = invalid(
        f.manager
            .add("files", &spec(r#"{ "command": "/nonexistent/files" }"#))
            .await
            .unwrap_err(),
    );
    assert_eq!(key, KeyPath::root().key("url"));
    assert!(reason.contains("giles.toml"), "{reason}");

    // So is anything only a stdio server has.
    for (stdio_key, value) in [
        ("command", r#""/nonexistent/files""#),
        ("args", r#"["--verbose"]"#),
        ("env", r#"{ "TOKEN": { "secret": "files-token" } }"#),
        ("idle_stop", r#""5m""#),
        ("sandbox", "{}"),
        ("unsandboxed", "true"),
    ] {
        let both = spec(&format!(r#"{{ {url}, "{stdio_key}": {value} }}"#));
        let (key, _) = invalid(f.manager.add("files", &both).await.unwrap_err());
        assert_eq!(key, KeyPath::root().key(stdio_key));
    }

    // A bearer token comes from `secrets.toml`: not the environment, and not the database.
    for bearer in [r#"{ "env": "FILES_TOKEN" }"#, r#""s3cr3t-value-42""#] {
        let json = format!(r#"{{ {url}, "auth": {{ "bearer": {bearer} }} }}"#);
        let (key, reason) = invalid(f.manager.add("files", &spec(&json)).await.unwrap_err());
        assert_eq!(key, KeyPath::root().key("auth").key("bearer"));
        assert!(reason.contains("secrets.toml"), "{reason}");
        assert!(!reason.contains("s3cr3t-value-42"), "{reason}");
    }

    assert!(f.stored().await.is_empty());
    assert!(f.configured().is_empty());
}

#[tokio::test]
async fn a_missing_secret_is_refused_and_then_accepted_once_secrets_toml_has_it() {
    let f = Fixture::start("", &[], vec![("files", Script::new(&["read"]))]).await;
    let bearer = |secret: &str| {
        spec(&format!(
            r#"{{ "url": "https://files.example.com/mcp",
                  "auth": {{ "bearer": {{ "secret": "{secret}" }} }} }}"#
        ))
    };
    let needs_secret = bearer("files-token");

    let (key, reason) = invalid(f.manager.add("files", &needs_secret).await.unwrap_err());
    assert_eq!(key, KeyPath::root().key("auth").key("bearer"));
    assert!(reason.contains("no secret \"files-token\""), "{reason}");
    assert!(f.stored().await.is_empty());

    // The same manager reads `secrets.toml` again: no restart.
    write_secrets(&f.root, "files-token = \"s3cr3t-value-42\"\n");
    f.manager.add("files", &needs_secret).await.unwrap();
    assert_eq!(f.configured(), ["files"]);
    assert_eq!(f.offered().await, ["files__read"]);
    // Only the reference is stored.
    let row = &f.store.toolsets().await.unwrap()[0];
    assert!(row.spec.contains("files-token"), "{}", row.spec);
    assert!(!row.spec.contains("s3cr3t-value-42"), "{}", row.spec);

    // An edit that needs a secret that isn't there yet is refused too, then accepted.
    let other = bearer("other-token");
    let (key, _) = invalid(f.manager.update("files", &other, None).await.unwrap_err());
    assert_eq!(key, KeyPath::root().key("auth").key("bearer"));
    write_secrets(
        &f.root,
        "files-token = \"s3cr3t-value-42\"\nother-token = \"another-value-7\"\n",
    );
    f.manager.update("files", &other, None).await.unwrap();
}

const NEEDS_SECRET: &str = r#"{ "url": "https://bad.example.com/mcp",
    "auth": { "bearer": { "secret": "bad-token" } } }"#;

#[tokio::test]
async fn startup_adds_what_validates_and_reports_the_rest_as_failed() {
    let giles_toml = "[toolset.files]\ncommand = \"/nonexistent/files\"\n";
    let f = Fixture::start(
        giles_toml,
        &[
            ("good", r#"{ "url": "https://good.example.com/mcp" }"#),
            ("bad", NEEDS_SECRET),
            ("Files", r#"{ "url": "https://other.example.com/mcp" }"#),
            ("stdio", r#"{ "command": "/nonexistent/stdio" }"#),
            ("broken", r#"{ "url": 5 }"#),
        ],
        vec![
            ("files", Script::new(&["read"])),
            ("good", Script::new(&["fetch"])),
            ("bad", Script::new(&["later"])),
        ],
    )
    .await;

    // The valid one runs beside the file's toolset; the file's instance keeps its name.
    assert_eq!(f.configured(), ["files", "good"]);
    assert_eq!(f.offered().await, ["files__read", "good__fetch"]);
    assert_eq!(f.connector.connects("files"), 1);
    assert_eq!(f.connector.connects("good"), 1);

    let failures = f.failures();
    let names: Vec<&str> = failures.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["Files", "bad", "broken", "stdio"]);
    let reason = |name: &str| {
        failures
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, reason)| reason.as_str())
            .unwrap()
    };
    assert_eq!(
        reason("Files"),
        "the name is taken by `[toolset.files]` in giles.toml"
    );
    assert!(
        reason("bad").starts_with("auth.bearer: no secret \"bad-token\""),
        "{}",
        reason("bad")
    );
    assert!(
        reason("stdio").starts_with("url: a stored toolset is a remote server"),
        "{}",
        reason("stdio")
    );
    assert!(
        reason("broken").starts_with("the stored spec can't be read: url: "),
        "{}",
        reason("broken")
    );
    // A failed toolset isn't started. A running one is the host's to restart, not the manager's.
    assert!(!f.manager.restart(Kind::Toolset, "good"));
    assert!(!f.manager.restart(Kind::Toolset, "nope"));
    assert_eq!(f.supervised(), ["files", "good"]);

    // The pages see every toolset: the failed ones with their reasons, the file's as read-only.
    let views = f.manager.list().await.unwrap();
    let listed: Vec<(&str, bool, bool)> = views
        .iter()
        .map(|v| {
            (
                v.name.as_str(),
                v.source == ToolsetSource::File,
                v.failure.is_some(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            ("bad", false, true),
            ("broken", false, true),
            ("Files", false, true),
            ("files", true, false),
            ("good", false, false),
            ("stdio", false, true),
        ]
    );
    assert_eq!(views[1].spec, None);
    assert_eq!(views[0].failure.as_deref(), Some(reason("bad")));

    // Removing the one that lost its name leaves the instance that kept it running.
    f.manager.remove("Files").await.unwrap();
    assert_eq!(f.configured(), ["files", "good"]);
    assert_eq!(f.supervised(), ["files", "good"]);
    assert_eq!(f.stored().await, ["bad", "broken", "good", "stdio"]);

    // Saving a failed toolset starts it, now that the secret is there.
    write_secrets(&f.root, "bad-token = \"bad-token-value\"\n");
    let view = f
        .manager
        .update("bad", &spec(NEEDS_SECRET), None)
        .await
        .unwrap();
    assert_eq!(view.failure, None);
    assert_eq!(f.configured(), ["bad", "files", "good"]);
    assert_eq!(
        f.offered().await,
        ["bad__later", "files__read", "good__fetch"]
    );
    assert!(f.failures().iter().all(|(name, _)| name != "bad"));

    // A failed toolset can be removed too.
    f.manager.remove("broken").await.unwrap();
    assert_eq!(f.stored().await, ["bad", "good", "stdio"]);
    assert!(f.failures().iter().all(|(name, _)| name != "broken"));
}

#[tokio::test]
async fn toolsets_of_giles_toml_are_read_only_and_keep_their_names() {
    let f = Fixture::start(
        "[toolset.docs]\ncommand = \"/nonexistent/docs\"\n",
        &[],
        vec![
            ("docs", Script::new(&["search"])),
            ("good", Script::new(&["fetch"])),
        ],
    )
    .await;

    let docs = f.manager.get("DOCS").await.unwrap();
    assert_eq!(
        (docs.name.as_str(), docs.source, docs.spec),
        ("docs", ToolsetSource::File, None)
    );
    for err in [
        f.manager
            .update("docs", &spec(FILES), None)
            .await
            .unwrap_err(),
        f.manager.remove("docs").await.unwrap_err(),
    ] {
        assert!(matches!(err, ToolsetManagerError::Unsupported), "{err:?}");
        assert!(err.is_invalid_input());
    }
    for name in ["docs", "Docs"] {
        let (key, reason) = invalid(f.manager.add(name, &spec(FILES)).await.unwrap_err());
        assert!(key.is_root());
        assert_eq!(
            reason,
            "the name is taken by `[toolset.docs]` in giles.toml"
        );
    }

    // The address of the form for a new toolset is no toolset's name, in any case.
    for name in ["new", "NEW", "New"] {
        let (key, reason) = invalid(f.manager.add(name, &spec(FILES)).await.unwrap_err());
        assert!(key.is_root(), "{name}");
        assert!(reason.contains("can't be a toolset's name"), "{reason}");
    }
    assert!(f.stored().await.is_empty());

    // Stored names are unique ignoring case.
    f.manager.add("good", &spec(FILES)).await.unwrap();
    let err = f.manager.add("GOOD", &spec(FILES)).await.unwrap_err();
    assert!(matches!(err, ToolsetManagerError::Exists), "{err:?}");
    assert_eq!(f.stored().await, ["good"]);
    assert_eq!(f.configured(), ["docs", "good"]);

    // The giles.toml toolset kept running through all of that.
    assert_eq!(f.connector.connects("docs"), 1);
    assert_eq!(f.supervised(), ["docs", "good"]);
    assert_eq!(f.registry.config().toolsets.len(), 2);
}

#[tokio::test]
async fn restarting_a_failed_stored_toolset_validates_it_again_and_starts_it() {
    let f = Fixture::start(
        "",
        &[("bad", NEEDS_SECRET)],
        vec![("bad", Script::new(&["later"]))],
    )
    .await;
    assert_eq!(f.failures().len(), 1);

    // Still no secret: it stays failed, with the reason as it is now.
    let view = f.manager.retry("bad").await.unwrap();
    let reason = view.failure.expect("still failed");
    assert!(
        reason.starts_with("auth.bearer: no secret \"bad-token\""),
        "{reason}"
    );
    assert_eq!(f.connector.connects("bad"), 0);
    assert!(f.configured().is_empty());

    // The secret is added: the restart is accepted, and the toolset starts. It runs in the
    // background, so wait for it.
    write_secrets(&f.root, "bad-token = \"bad-token-value\"\n");
    assert!(f.manager.restart(Kind::Toolset, "bad"));
    for _ in 0..500 {
        if f.failures().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(f.failures().is_empty(), "{:?}", f.failures());
    assert_eq!(f.configured(), ["bad"]);
    assert_eq!(f.offered().await, ["bad__later"]);
    assert_eq!(f.connector.connects("bad"), 1);

    // Running now, so a restart is the host's, and the manager leaves it alone.
    assert!(!f.manager.restart(Kind::Toolset, "bad"));
    let view = f.manager.retry("bad").await.unwrap();
    assert_eq!(view.failure, None);
    assert_eq!(f.connector.connects("bad"), 1);
    // Unknown names and the toolsets of `giles.toml` aren't the manager's.
    assert!(!f.manager.restart(Kind::Toolset, "ghost"));
    assert!(matches!(
        f.manager.retry("ghost").await.unwrap_err(),
        ToolsetManagerError::NotFound
    ));
}

#[tokio::test]
async fn a_toolset_the_host_cannot_start_is_taken_out_of_the_configuration_and_shown_failed() {
    // The host can only refuse a toolset that isn't in the live configuration, which the manager
    // never lets happen, so this is the rollback itself: what the manager does when it does.
    let f = Fixture::start("", &[], vec![("files", Script::new(&["read"]))]).await;
    f.manager.add("files", &spec(FILES)).await.unwrap();
    assert_eq!(f.configured(), ["files"]);

    f.manager.withdraw("files", "boom".to_owned());
    assert!(f.configured().is_empty());
    assert_eq!(f.failures(), [("files".to_owned(), "boom".to_owned())]);
    // One view, the stored toolset, failed. Not also a toolset of `giles.toml`.
    let views = f.manager.list().await.unwrap();
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(matches!(views[0].source, ToolsetSource::Stored { .. }));
    assert_eq!(views[0].failure.as_deref(), Some("boom"));

    // It can be saved again (not refused as a name of `giles.toml`), which starts it.
    let view = f.manager.update("files", &spec(FILES), None).await.unwrap();
    assert_eq!(view.failure, None);
    assert_eq!(f.configured(), ["files"]);
    assert!(f.failures().is_empty());
    assert_eq!(f.offered().await, ["files__read"]);

    // And a failed one can be removed.
    f.manager.withdraw("files", "boom".to_owned());
    f.manager.remove("files").await.unwrap();
    assert!(f.stored().await.is_empty());
    assert!(f.failures().is_empty());
}

#[tokio::test]
async fn the_name_taken_reason_wins_over_the_spec() {
    // A stdio spec, refused at `url` if the name were free.
    let stdio = r#"{ "command": "/nonexistent/web" }"#;
    let giles_toml = "[toolset.web]\ncommand = \"/nonexistent/web\"\n\
                      [toolset.chat]\ncommand = \"/nonexistent/chat\"\n";
    let scripts = vec![
        ("web", Script::new(&["read"])),
        ("chat", Script::new(&["read"])),
    ];
    let f = Fixture::start(giles_toml, &[("web", stdio)], scripts).await;
    let taken = "the name is taken by `[toolset.web]` in giles.toml";
    assert_eq!(f.failures(), [("web".to_owned(), taken.to_owned())]);

    // A name no stored toolset has gets the same reason (a stored name would be `Exists`).
    let (key, reason) = invalid(f.manager.add("Chat", &spec(stdio)).await.unwrap_err());
    assert!(key.is_root(), "{key}");
    assert_eq!(
        reason,
        "the name is taken by `[toolset.chat]` in giles.toml"
    );
    let err = f.manager.add("Web", &spec(stdio)).await.unwrap_err();
    assert!(matches!(err, ToolsetManagerError::Exists), "{err:?}");
}

#[tokio::test]
async fn saving_a_running_toolset_the_configuration_lost_stops_it_and_shows_it_failed() {
    let f = Fixture::start("", &[], vec![("files", Script::new(&["read"]))]).await;
    f.manager.add("files", &spec(FILES)).await.unwrap();
    assert_eq!(f.supervised(), ["files"]);

    // The configuration can't replace what it doesn't have. The host's copy is stopped, so it
    // doesn't run on a configuration the manager can't see.
    f.registry
        .live_config()
        .remove_toolset(&InstanceName::new("files").unwrap());
    let (key, reason) = invalid(
        f.manager
            .update("files", &spec(FILES), None)
            .await
            .unwrap_err(),
    );
    assert!(key.is_root(), "{key}");
    assert!(reason.contains("files"), "{reason}");

    assert!(f.supervised().is_empty());
    assert!(f.configured().is_empty());
    assert_eq!(f.failures(), [("files".to_owned(), reason)]);
    assert_eq!(f.connector.live("files"), 0);
    assert_eq!(f.manager.list().await.unwrap().len(), 1);
}

// Renaming.

const PROVIDER: &str = "[provider.anthropic]\n";

const JEEVES: &str = "models = [\"anthropic/claude\"]\n\
                      tools = [\"files__*\", \"memory*\"]\n\
                      ask = [\"files__delete\"]\n";

const BUNTER: &str = "models = [\"anthropic/claude\"]\ntools = [\"memory*\"]\n";

fn glob(key: &'static str, from: &str, to: &str) -> GlobChange {
    GlobChange {
        key,
        from: from.to_owned(),
        to: to.to_owned(),
    }
}

fn agent_toml(f: &Fixture, id: &str) -> String {
    std::fs::read_to_string(f.root.agents_dir().join(id).join("agent.toml")).unwrap()
}

/// The stored toolset `files`, running, and the agents `jeeves` (whose globs name it) and
/// `bunter` (whose don't). The fake connector also has a server for `notes`.
async fn files_and_agents() -> Fixture {
    Fixture::start_with_agents(
        PROVIDER,
        &[("files", FILES)],
        &[("jeeves", JEEVES), ("bunter", BUNTER)],
        vec![
            ("files", Script::new(&["read"])),
            ("notes", Script::new(&["read", "write"])),
        ],
    )
    .await
}

#[tokio::test]
async fn renaming_a_running_toolset_renames_its_tools_and_rewrites_the_agents_globs() {
    let f = files_and_agents().await;
    assert_eq!(f.offered().await, ["files__read"]);
    let before = f.manager.get("files").await.unwrap();
    let (created, updated) = stamps(&before);

    // The preview says what changes and changes nothing.
    let changes = vec![AgentGlobChange {
        agent: "jeeves".to_owned(),
        globs: vec![
            glob("tools", "files__*", "notes__*"),
            glob("ask", "files__delete", "notes__delete"),
        ],
    }];
    let preview = f.manager.rename_preview("files", "notes").await.unwrap();
    assert_eq!(preview.agents, changes);
    assert_eq!(f.stored().await, ["files"]);
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);
    assert_eq!(f.supervised(), ["files"]);

    // Any case finds the toolset.
    let renamed = f.manager.rename("FILES", "notes").await.unwrap();
    assert_eq!(renamed.agents, changes);
    // `from` is the name as stored, not as asked for.
    assert_eq!(renamed.from, "files");
    assert_eq!(renamed.view.name, "notes");
    assert_eq!(renamed.view.failure, None);
    assert_eq!(renamed.view.spec, Some(spec(FILES)));
    let (renamed_created, renamed_updated) = stamps(&renamed.view);
    assert_eq!(renamed_created, created);
    assert!(renamed_updated > updated);

    // The row, the live configuration, the host, and the registry have the new name only, and
    // new sessions get the tools under the new prefix.
    assert_eq!(f.stored().await, ["notes"]);
    assert_eq!(f.configured(), ["notes"]);
    assert_eq!(f.supervised(), ["notes"]);
    assert_eq!(f.offered().await, ["notes__read", "notes__write"]);
    assert!(f.registry.toolset("files").is_none());
    assert!(f.registry.toolset("notes").is_some());
    assert_eq!(f.connector.live("files"), 0);
    assert_eq!(f.connector.connects("notes"), 1);
    assert!(f.manager.get("files").await.is_err());

    // The agents' files and the live configuration say `notes__`; others are untouched.
    assert_eq!(
        agent_toml(&f, "jeeves"),
        JEEVES.replace("files__", "notes__")
    );
    assert_eq!(agent_toml(&f, "bunter"), BUNTER);
    let config = f.registry.config();
    let jeeves = &config.agents[&AgentId::new("jeeves").unwrap()];
    assert_eq!(jeeves.tools, ["notes__*", "memory*"]);
    assert_eq!(jeeves.ask, ["notes__delete"]);
    assert_eq!(
        config.agents[&AgentId::new("bunter").unwrap()].tools,
        ["memory*"]
    );

    // And back again.
    let back = f.manager.rename("notes", "files").await.unwrap();
    assert_eq!(back.agents.len(), 1);
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);
    assert_eq!(f.offered().await, ["files__read"]);
}

#[tokio::test]
async fn a_toolset_no_agent_names_is_renamed_without_touching_the_agents() {
    let f = Fixture::start_with_agents(
        PROVIDER,
        &[("files", FILES)],
        &[("bunter", BUNTER)],
        vec![
            ("files", Script::new(&["read"])),
            ("notes", Script::new(&["read"])),
        ],
    )
    .await;
    let preview = f.manager.rename_preview("files", "notes").await.unwrap();
    assert!(preview.agents.is_empty());
    let renamed = f.manager.rename("files", "notes").await.unwrap();
    assert!(renamed.agents.is_empty());
    assert_eq!(agent_toml(&f, "bunter"), BUNTER);
    assert_eq!(f.offered().await, ["notes__read"]);
}

/// The rename rewrites agents' files, so it takes the lock that agent edits hold.
#[tokio::test]
async fn a_rename_waits_for_an_agent_change_and_holds_the_agents_lock() {
    let f = Fixture::start_with_agents(
        PROVIDER,
        &[("files", FILES)],
        &[("jeeves", JEEVES)],
        vec![
            ("files", Script::new(&["read"])),
            ("notes", Script::new(&["read"])),
        ],
    )
    .await;

    // An agent change is in progress: the rename doesn't start, and nothing has changed.
    let change = f.agent_lock.clone().lock_owned().await;
    let renaming = tokio::spawn({
        let manager = f.manager.clone();
        async move { manager.rename("files", "notes").await }
    });
    settle().await;
    assert!(!renaming.is_finished());
    assert_eq!(f.stored().await, ["files"]);
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);

    // Once it is done the rename runs, and the lock is free again afterwards.
    drop(change);
    renaming.await.unwrap().unwrap();
    assert_eq!(f.stored().await, ["notes"]);
    assert!(agent_toml(&f, "jeeves").contains("notes__"));
    drop(
        f.agent_lock
            .try_lock()
            .expect("the rename released the lock"),
    );
}

/// The other half of the shared lock: an agent edit waits for a toolset rename, and the other way
/// round, because both rewrite `agent.toml` files.
#[tokio::test]
async fn an_agent_edit_waits_for_the_lock_the_toolset_manager_shares() {
    let f = Fixture::start_with_agents(
        PROVIDER,
        &[("files", FILES)],
        &[("jeeves", JEEVES)],
        vec![("files", Script::new(&["read"]))],
    )
    .await;
    let agents = Arc::new(AgentManager::new(AgentManagerOptions {
        registry: f.registry.clone(),
        store: f.store.clone(),
        memory_dbs: MemoryDbs::new(f.root.clone(), JournalMode::Wal),
        routes: Arc::new(
            Routes::open(f.store.clone(), f.registry.live_config().clone())
                .await
                .unwrap(),
        ),
        jobs: JobChanges::default(),
        activity: Arc::new(FakeActivity::default()),
        lock: f.agent_lock.clone(),
    }));
    let form = agents.edit_form("jeeves").await.unwrap();
    let edit = AgentEdit {
        id: form.id,
        name: "Jeeves".to_owned(),
        models: form.models,
        tools: form.tools,
        ask: form.ask,
        timezone: form.timezone,
        soul: form.soul,
        version: form.version,
    };

    // A change is in progress: the edit doesn't start, and nothing has changed.
    let change = f.agent_lock.clone().lock_owned().await;
    let editing = tokio::spawn({
        let agents = agents.clone();
        async move { agents.edit(&edit).await }
    });
    settle().await;
    assert!(!editing.is_finished());
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);

    // Once it is done the edit runs, and the lock is free again afterwards.
    drop(change);
    editing.await.unwrap().unwrap();
    assert!(agent_toml(&f, "jeeves").contains("Jeeves"));
    drop(f.agent_lock.try_lock().expect("the edit released the lock"));
}

#[tokio::test]
async fn a_name_that_is_taken_or_reserved_is_refused() {
    let giles_toml = "[provider.anthropic]\n[channel.web]\n\
                      [toolset.docs]\ncommand = \"/nonexistent/docs\"\n";
    let f = Fixture::start_with_agents(
        giles_toml,
        &[("files", FILES), ("other", FILES)],
        &[("jeeves", JEEVES)],
        vec![
            ("files", Script::new(&["read"])),
            ("other", Script::new(&["read"])),
            ("docs", Script::new(&["search"])),
            ("leftover", Script::new(&["read"])),
        ],
    )
    .await;

    // Another stored toolset has the name, in any case.
    for new in ["other", "OTHER"] {
        for err in [
            f.manager.rename("files", new).await.unwrap_err(),
            f.manager.rename_preview("files", new).await.unwrap_err(),
        ] {
            assert!(matches!(err, ToolsetManagerError::Exists), "{new}: {err:?}");
            assert!(err.is_invalid_input());
        }
    }
    // Every other refusal is about the name as a whole.
    let cases = [
        (
            "docs",
            "the name is taken by `[toolset.docs]` in giles.toml",
        ),
        (
            "DOCS",
            "the name is taken by `[toolset.docs]` in giles.toml",
        ),
        ("new", "can't be a toolset's name"),
        ("bad name", "invalid instance name"),
        ("files", "already has that name"),
    ];
    for (new, fragment) in cases {
        let err = f.manager.rename("files", new).await.unwrap_err();
        assert!(err.is_invalid_input(), "{new}: {err:?}");
        let (key, reason) = invalid(err);
        assert!(key.is_root(), "{new}: {key}");
        assert!(reason.contains(fragment), "{new}: {reason}");
        // The preview refuses the same way.
        let (key, same) = invalid(f.manager.rename_preview("files", new).await.unwrap_err());
        assert!(key.is_root(), "{new}: {key}");
        assert_eq!(same, reason, "{new}");
    }

    // Toolsets that aren't stored ones, and ones that aren't there, can't be renamed.
    let err = f.manager.rename("docs", "notes").await.unwrap_err();
    assert!(matches!(err, ToolsetManagerError::Unsupported), "{err:?}");
    let err = f.manager.rename_preview("docs", "notes").await.unwrap_err();
    assert!(matches!(err, ToolsetManagerError::Unsupported), "{err:?}");
    let err = f.manager.rename("ghost", "notes").await.unwrap_err();
    assert!(matches!(err, ToolsetManagerError::NotFound), "{err:?}");
    assert!(!err.is_invalid_input());

    // Nothing changed, and the toolset was never stopped.
    assert_eq!(f.stored().await, ["files", "other"]);
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);
    assert_eq!(f.supervised(), ["docs", "files", "other"]);
    assert_eq!(f.connector.connects("files"), 1);

    // Data an earlier instance left under a name doesn't keep a remote toolset from taking it,
    // as it doesn't keep a new one from being added: the toolset has no data of its own to merge.
    let leftover = f.root.plugin_data_dir(Kind::Toolset, "leftover").unwrap();
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("state.db"), "kept").unwrap();
    f.manager.rename("files", "leftover").await.unwrap();
    assert_eq!(f.stored().await, ["leftover", "other"]);
    assert_eq!(
        std::fs::read_to_string(leftover.join("state.db")).unwrap(),
        "kept"
    );
}

#[tokio::test]
async fn a_toolset_may_have_the_name_of_a_provider_or_a_channel() {
    let mason = "models = [\"anthropic/claude\"]\ntools = [\"web__*\", \"memory*\"]\n";
    let f = Fixture::start_with_agents(
        "[provider.anthropic]\n[channel.web]\n",
        &[("files", FILES)],
        &[("jeeves", JEEVES), ("mason", mason)],
        vec![
            ("files", Script::new(&["read"])),
            ("web", Script::new(&["read"])),
            ("pages", Script::new(&["read"])),
            ("anthropic", Script::new(&["read"])),
        ],
    )
    .await;

    // Added under a channel's name, and renamed to a provider's.
    f.manager.add("web", &spec(FILES)).await.unwrap();
    let renamed = f.manager.rename("files", "anthropic").await.unwrap();
    assert_eq!(renamed.agents.len(), 1);
    assert_eq!(f.stored().await, ["anthropic", "web"]);
    assert_eq!(f.configured(), ["anthropic", "web"]);
    assert_eq!(f.offered().await, ["anthropic__read", "web__read"]);
    let config = f.registry.config();
    assert!(config.channels.contains_key("web"));
    assert!(config.providers.contains_key("anthropic"));
    assert_eq!(
        agent_toml(&f, "jeeves"),
        JEEVES.replace("files__", "anthropic__")
    );

    // The channel's name is the stored toolset's, so its agents' globs follow the rename, and the
    // channel stays.
    let renamed = f.manager.rename("web", "pages").await.unwrap();
    assert_eq!(renamed.agents.len(), 1);
    assert_eq!(renamed.agents[0].agent.as_str(), "mason");
    assert_eq!(f.stored().await, ["anthropic", "pages"]);
    assert_eq!(f.configured(), ["anthropic", "pages"]);
    assert_eq!(f.offered().await, ["anthropic__read", "pages__read"]);
    assert_eq!(agent_toml(&f, "mason"), mason.replace("web__", "pages__"));
    assert!(f.registry.config().channels.contains_key("web"));
}

#[tokio::test]
async fn globs_that_match_by_the_old_name_refuse_the_rename_and_change_nothing() {
    // `bunter` has globs that match `files__` tools without saying `files__`; `jeeves` has only
    // globs that can be rewritten.
    let ambiguous = "models = [\"anthropic/claude\"]\n\
                     tools = [\"file*\", \"files__read\"]\n\
                     ask = [\"*\", \"fil?s__delete\"]\n";
    let f = Fixture::start_with_agents(
        PROVIDER,
        &[("files", FILES)],
        &[("jeeves", JEEVES), ("bunter", ambiguous)],
        vec![
            ("files", Script::new(&["read"])),
            ("notes", Script::new(&["read"])),
        ],
    )
    .await;
    let before = f.store.toolsets().await.unwrap();

    for err in [
        f.manager.rename("files", "notes").await.unwrap_err(),
        f.manager
            .rename_preview("files", "notes")
            .await
            .unwrap_err(),
    ] {
        assert!(err.is_invalid_input());
        let ToolsetManagerError::Refused { reason } = err else {
            panic!("expected Refused, got {err:?}");
        };
        assert!(reason.contains("`bunter`"), "{reason}");
        assert!(reason.contains("tools entry \"file*\""), "{reason}");
        assert!(reason.contains("ask entry \"fil?s__delete\""), "{reason}");
        assert!(!reason.contains("jeeves"), "{reason}");
        assert!(!reason.contains("files__read"), "{reason}");
    }

    // Nothing changed: not the row, the files, or the running toolset.
    assert_eq!(f.store.toolsets().await.unwrap(), before);
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);
    assert_eq!(agent_toml(&f, "bunter"), ambiguous);
    assert_eq!(f.configured(), ["files"]);
    assert_eq!(f.supervised(), ["files"]);
    assert_eq!(f.connector.connects("files"), 1);
    assert_eq!(f.offered().await, ["files__read"]);

    // Once the owner says what they mean, the rename goes through.
    std::fs::write(
        f.root.agents_dir().join("bunter/agent.toml"),
        "models = [\"anthropic/claude\"]\ntools = [\"files__*\"]\n",
    )
    .unwrap();
    let renamed = f.manager.rename("files", "notes").await.unwrap();
    let agents: Vec<&str> = renamed.agents.iter().map(|a| a.agent.as_str()).collect();
    assert_eq!(agents, ["bunter", "jeeves"]);
}

#[tokio::test]
async fn an_agent_file_that_cannot_be_edited_refuses_the_rename() {
    let f = files_and_agents().await;
    std::fs::write(
        f.root.agents_dir().join("bunter/agent.toml"),
        "models = [\n",
    )
    .unwrap();
    let err = f.manager.rename("files", "notes").await.unwrap_err();
    let ToolsetManagerError::Refused { reason } = err else {
        panic!("expected Refused, got {err:?}");
    };
    assert!(reason.contains("`bunter`"), "{reason}");
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);
    assert_eq!(f.supervised(), ["files"]);
}

#[tokio::test]
async fn a_failed_step_is_undone_and_the_toolset_runs_again_under_its_old_name() {
    // Both agents name the toolset. `bunter` is rewritten first, then `jeeves` can't be: the
    // name of the temporary file the edit uses is taken by a directory.
    let both = "models = [\"anthropic/claude\"]\ntools = [\"files__read\"]\n";
    let f = Fixture::start_with_agents(
        PROVIDER,
        &[("files", FILES)],
        &[("bunter", both), ("jeeves", JEEVES)],
        vec![
            ("files", Script::new(&["read"])),
            ("notes", Script::new(&["read"])),
        ],
    )
    .await;
    let before = f.store.toolsets().await.unwrap();
    std::fs::create_dir(f.root.agents_dir().join("jeeves/.agent.toml.edit.tmp")).unwrap();
    assert_eq!(f.connector.connects("files"), 1);

    let err = f.manager.rename("files", "notes").await.unwrap_err();
    assert!(!err.is_invalid_input());
    let ToolsetManagerError::Failed {
        step, undo_errors, ..
    } = &err
    else {
        panic!("expected Failed, got {err:?}");
    };
    assert!(step.contains("jeeves"), "{step}");
    assert!(undo_errors.is_empty(), "{undo_errors:?}");
    assert!(err.to_string().ends_with("Nothing was changed."), "{err}");

    // The old state is back: the agents' files and their configuration, the row, and the running
    // toolset.
    assert_eq!(agent_toml(&f, "bunter"), both);
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);
    let config = f.registry.config();
    assert_eq!(
        config.agents[&AgentId::new("bunter").unwrap()].tools,
        ["files__read"]
    );
    assert_eq!(f.store.toolsets().await.unwrap(), before);
    assert_eq!(f.configured(), ["files"]);
    assert_eq!(f.supervised(), ["files"]);
    assert_eq!(f.offered().await, ["files__read"]);
    assert_eq!(f.connector.connects("files"), 2, "it was started again");
    assert_eq!(f.connector.live("files"), 1);
    assert_eq!(f.connector.connects("notes"), 0);
    assert_eq!(f.manager.get("files").await.unwrap().failure, None);
}

#[tokio::test]
async fn a_failed_toolset_can_be_renamed_and_is_started_when_it_validates() {
    let f = Fixture::start_with_agents(
        PROVIDER,
        &[("bad", NEEDS_SECRET)],
        &[(
            "jeeves",
            "models = [\"anthropic/claude\"]\ntools = [\"bad__*\"]\n",
        )],
        vec![("later", Script::new(&["go"]))],
    )
    .await;
    assert_eq!(f.failures().len(), 1);

    // It isn't running, so only the name is checked, and it is renamed. It still can't start.
    let renamed = f.manager.rename("bad", "later").await.unwrap();
    assert_eq!(renamed.view.name, "later");
    assert_eq!(renamed.agents.len(), 1);
    let reason = renamed.view.failure.expect("still failed");
    assert!(
        reason.starts_with("auth.bearer: no secret \"bad-token\""),
        "{reason}"
    );
    assert_eq!(f.stored().await, ["later"]);
    assert_eq!(f.failures(), [("later".to_owned(), reason)]);
    assert!(f.configured().is_empty());
    assert_eq!(
        agent_toml(&f, "jeeves"),
        "models = [\"anthropic/claude\"]\ntools = [\"later__*\"]\n"
    );
    assert!(
        f.manager
            .plugins()
            .iter()
            .all(|p| p.instance.as_str() != "bad")
    );

    // The secret arrives, and it starts under its new name.
    write_secrets(&f.root, "bad-token = \"bad-token-value\"\n");
    let view = f.manager.retry("later").await.unwrap();
    assert_eq!(view.failure, None);
    assert_eq!(f.configured(), ["later"]);
    assert_eq!(f.offered().await, ["later__go"]);

    // A failed toolset that validates now is started by the rename itself.
    let g = Fixture::start(
        "",
        &[("files", NEEDS_SECRET)],
        vec![("notes", Script::new(&["read"]))],
    )
    .await;
    assert_eq!(g.failures().len(), 1);
    write_secrets(&g.root, "bad-token = \"bad-token-value\"\n");
    let renamed = g.manager.rename("files", "notes").await.unwrap();
    assert_eq!(renamed.view.failure, None);
    assert_eq!(g.configured(), ["notes"]);
    assert_eq!(g.offered().await, ["notes__read"]);
    assert!(g.failures().is_empty());
}

#[tokio::test]
async fn a_stored_toolset_that_lost_its_name_to_giles_toml_leaves_the_instances_globs_alone() {
    // `Files` is stored, but `[toolset.files]` took the name at startup, so the agents' `files__`
    // globs are the instance's.
    let f = Fixture::start_with_agents(
        "[provider.anthropic]\n[toolset.files]\ncommand = \"/nonexistent/files\"\n",
        &[("Files", FILES)],
        &[("jeeves", JEEVES)],
        vec![
            ("files", Script::new(&["read"])),
            ("docs", Script::new(&["search"])),
        ],
    )
    .await;
    assert_eq!(f.failures().len(), 1);

    let renamed = f.manager.rename("Files", "docs").await.unwrap();
    assert!(renamed.agents.is_empty());
    assert_eq!(renamed.view.name, "docs");
    assert_eq!(renamed.view.failure, None);
    assert_eq!(f.stored().await, ["docs"]);
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);
    assert_eq!(f.configured(), ["docs", "files"]);
    assert_eq!(f.supervised(), ["docs", "files"]);
    assert_eq!(f.offered().await, ["docs__search", "files__read"]);
    assert!(f.failures().is_empty());
}

#[tokio::test]
async fn the_case_of_a_toolset_name_can_change() {
    let f = Fixture::start_with_agents(
        PROVIDER,
        &[("files", FILES)],
        &[("jeeves", JEEVES)],
        vec![
            ("files", Script::new(&["read"])),
            ("Files", Script::new(&["read"])),
        ],
    )
    .await;

    let err = f.manager.rename("files", "files").await.unwrap_err();
    assert!(err.is_invalid_input(), "{err:?}");
    let renamed = f.manager.rename("files", "Files").await.unwrap();
    assert_eq!(renamed.from, "files");
    assert_eq!(renamed.view.name, "Files");
    assert_eq!(renamed.agents.len(), 1);
    assert_eq!(f.stored().await, ["Files"]);
    assert_eq!(f.configured(), ["Files"]);
    assert_eq!(f.supervised(), ["Files"]);
    assert_eq!(f.offered().await, ["Files__read"]);
    assert_eq!(
        agent_toml(&f, "jeeves"),
        JEEVES.replace("files__", "Files__")
    );

    // And back.
    f.manager.rename("FILES", "files").await.unwrap();
    assert_eq!(f.stored().await, ["files"]);
    assert_eq!(agent_toml(&f, "jeeves"), JEEVES);
}
