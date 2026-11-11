//! The environment echo plugin as a remote plugin (PLAN section 7.6) of a running core: the binary
//! runs on its own (`serve --listen 127.0.0.1:...`) and `[environment.<name>]` with `address`
//! connects to it with a bearer token. The core never sends `config` to a remote environment: the
//! config loader refuses it, and the plugin takes its settings from its own file.

mod support;

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;

use giles_core::plugin_host::PluginState;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};

use self::support::{ECHO, Home, WAIT, wait_until};

/// The echo binary running as a remote plugin.
struct Remote {
    child: Child,
    addr: SocketAddr,
}

impl Remote {
    /// Starts it with data directory `data`, accepting `token` (any call without one is refused),
    /// and its own settings from `config`.
    async fn start(data: &Path, token: &str, config: &Path) -> Self {
        let mut child = Command::new(ECHO)
            .args(["serve", "--listen", "127.0.0.1:0", "--data"])
            .arg(data)
            .arg("--config")
            .arg(config)
            .env("GILES_ENVIRONMENT_ECHO_TOKEN", token)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let line = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .expect("the listening line");
        let addr = line.strip_prefix("listening on ").unwrap().parse().unwrap();
        Self { child, addr }
    }
}

#[tokio::test]
async fn an_environment_with_an_address_connects_and_serves() {
    let state = tempfile::tempdir().unwrap();
    // The plugin's own settings: the `[config]` table of its file.
    let file = state.path().join("echo.toml");
    std::fs::write(&file, "[config]\nlabel = \"remote\"\n").unwrap();
    let mut remote = Remote::start(&state.path().join("data"), "s3cret", &file).await;

    let home = Home::new("");
    home.add_tables(&format!(
        "[environment.far]\ncomponent = \"echo\"\naddress = \"http://{}\"\n\
         auth = {{ bearer = {{ env = \"ECHO_TOKEN\" }} }}\ndescription = \"far away\"\n",
        remote.addr
    ));
    let core = home.start_core().await;
    let registry = &core.runtime.registry;
    let entry = registry.environment("far").unwrap();
    assert!(
        entry.reachable(),
        "{:?}",
        core.host.environment_status("far")
    );
    assert_eq!(entry.description, "far away");
    // `Info` reflects the plugin's own file: the core sent no config.
    assert_eq!(
        entry.options_schema.unwrap()["description"],
        "Options of the remote environment."
    );
    let status = core.host.environment_status("far").unwrap();
    assert!(
        matches!(status.state, PluginState::Running { pid: None, .. }),
        "{status:?}"
    );
    assert!(status.binary.is_none());
    let client = entry.client.unwrap();
    assert!(
        client
            .list_workspaces()
            .await
            .unwrap()
            .workspaces
            .is_empty()
    );

    // The plugin going away withdraws the client; the core keeps retrying.
    remote.child.kill().await.unwrap();
    wait_until(|| (!registry.environment("far").unwrap().reachable()).then_some(())).await;
    assert!(client.list_workspaces().await.is_err());
    core.stop().await;
}

#[tokio::test]
async fn a_wrong_token_leaves_the_environment_unreachable_and_config_is_refused() {
    let state = tempfile::tempdir().unwrap();
    let file = state.path().join("echo.toml");
    std::fs::write(&file, "").unwrap();
    let remote = Remote::start(&state.path().join("data"), "another", &file).await;

    let home = Home::new("");
    home.add_tables(&format!(
        "[environment.far]\ncomponent = \"echo\"\naddress = \"http://{}\"\n\
         auth = {{ bearer = {{ env = \"ECHO_TOKEN\" }} }}\n",
        remote.addr
    ));
    let core = home.start_core().await;
    assert!(
        !core
            .runtime
            .registry
            .environment("far")
            .unwrap()
            .reachable()
    );
    let status = core.host.environment_status("far").unwrap();
    assert!(
        matches!(status.state, PluginState::BackingOff { .. }),
        "{status:?}"
    );
    core.stop().await;

    // `config` next to `address` can't reach a remote plugin's Configure: it doesn't load.
    let home = Home::new(
        "[environment.far]\ncomponent = \"echo\"\naddress = \"http://127.0.0.1:1\"\n\
         config = { label = \"x\" }\n",
    );
    let error = home.try_config().unwrap_err();
    assert_eq!(error.key.to_string(), "environment.far.config");
}
