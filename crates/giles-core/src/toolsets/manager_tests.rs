//! The toolset manager with the fake connector of `tests.rs`: stored toolsets reach the registry
//! and the host, specs are refused with their key and nothing stored (a stored toolset is remote,
//! with a bearer token from `secrets.toml`), secrets are read again, and startup reports toolsets
//! that fail.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use giles_store::Store;

use super::cache::ToolCache;
use super::host::ToolsetHost;
use super::manager::{
    ToolsetManager, ToolsetManagerError, ToolsetManagerOptions, ToolsetSource, ToolsetView,
};
use super::tests::{FakeConnector, Script, settle};
use crate::admin::{PluginControl, PluginRunState, PluginRuntime};
use crate::config::{Config, InstanceName, KeyPath, LoadOptions, ToolsetSpec, ToolsetTransport};
use crate::paths::DataRoot;
use crate::registry::Registry;

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
}

impl Fixture {
    /// `extra` is appended to a minimal `giles.toml`. `stored` are the rows in the database when
    /// the manager starts; `scripts` are the toolsets the fake connector can connect to.
    async fn start(
        extra: &str,
        stored: &[(&str, &str)],
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
        let manager = ToolsetManager::start(ToolsetManagerOptions {
            store: store.clone(),
            root: root.clone(),
            config: registry.live_config().clone(),
            host: host.clone(),
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
        ("component", r#""files""#),
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
    let giles_toml = "[toolset.files]\ncommand = \"/nonexistent/files\"\n[channel.web]\n";
    let f = Fixture::start(
        giles_toml,
        &[
            ("good", r#"{ "url": "https://good.example.com/mcp" }"#),
            ("bad", NEEDS_SECRET),
            ("Files", r#"{ "url": "https://other.example.com/mcp" }"#),
            ("web", r#"{ "url": "https://other.example.com/mcp" }"#),
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
    assert_eq!(names, ["Files", "bad", "broken", "stdio", "web"]);
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
    assert_eq!(
        reason("web"),
        "the name is taken by `[channel.web]` in giles.toml"
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
    assert!(!f.manager.restart("good"));
    assert!(!f.manager.restart("nope"));
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
            ("web", false, true),
        ]
    );
    assert_eq!(views[1].spec, None);
    assert_eq!(views[0].failure.as_deref(), Some(reason("bad")));

    // Removing the one that lost its name leaves the instance that kept it running.
    f.manager.remove("Files").await.unwrap();
    assert_eq!(f.configured(), ["files", "good"]);
    assert_eq!(f.supervised(), ["files", "good"]);
    assert_eq!(f.stored().await, ["bad", "broken", "good", "stdio", "web"]);

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
    assert_eq!(f.stored().await, ["bad", "good", "stdio", "web"]);
    assert!(f.failures().iter().all(|(name, _)| name != "broken"));
}

#[tokio::test]
async fn toolsets_of_giles_toml_are_read_only_and_keep_their_names() {
    let f = Fixture::start(
        "[core]\nretired_instances = [\"old\"]\n[toolset.docs]\ncommand = \"/nonexistent/docs\"\n",
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
    // A name `[core] retired_instances` lists is for instances that are not configured.
    let (key, reason) = invalid(f.manager.add("old", &spec(FILES)).await.unwrap_err());
    assert!(key.is_root());
    assert!(reason.contains("retired_instances"), "{reason}");

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
    assert!(f.manager.restart("bad"));
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
    assert!(!f.manager.restart("bad"));
    let view = f.manager.retry("bad").await.unwrap();
    assert_eq!(view.failure, None);
    assert_eq!(f.connector.connects("bad"), 1);
    // Unknown names and the toolsets of `giles.toml` aren't the manager's.
    assert!(!f.manager.restart("ghost"));
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
    let giles_toml = "[channel.web]\n[channel.chat]\ncomponent = \"irc\"\n";
    let f = Fixture::start(giles_toml, &[("web", stdio)], vec![]).await;
    let taken = "the name is taken by `[channel.web]` in giles.toml";
    assert_eq!(f.failures(), [("web".to_owned(), taken.to_owned())]);

    // A name no stored toolset has gets the same reason (a stored name would be `Exists`).
    let (key, reason) = invalid(f.manager.add("Chat", &spec(stdio)).await.unwrap_err());
    assert!(key.is_root(), "{key}");
    assert_eq!(
        reason,
        "the name is taken by `[channel.chat]` in giles.toml"
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
