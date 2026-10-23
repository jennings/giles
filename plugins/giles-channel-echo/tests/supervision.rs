//! Supervision of the echo plugin in a running core (unsandboxed): a crash relaunches it and its
//! new channel takes over, a restart request does the same, a failure that won't pass waits, and
//! the `Describe` cache. Backoff timing, health checks, and binary changes are tested in paused
//! time against fakes in `crates/giles-core/src/plugin_host/supervisor/tests.rs`.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use std::sync::Arc;

use giles_core::admin::{
    self, AdminListener, AdminOptions, AdminService, InstanceLock, PluginControl,
};
use giles_core::plugin_host::{PluginState, PluginStatus};
use giles_core::sessions::SystemClock;
use giles_plugin_sdk::giles_types::{OutputMode, Timestamp};
use giles_proto::admin::v1 as pb;
use tokio_util::sync::CancellationToken;

use self::support::{Core, Home, owner_message, wait_until};

/// The pid of the running echo plugin.
fn running_pid(core: &Core) -> Option<u32> {
    match core.host.status("echo")?.state {
        PluginState::Running { pid, .. } => pid,
        _ => None,
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks whether the process exists.
    #[allow(unsafe_code)]
    let alive = unsafe { libc::kill(i32::try_from(pid).unwrap(), 0) } == 0;
    alive
}

/// Sends a message through the plugin and waits for the turn and its reply.
async fn round_trip(home: &Home, core: &Core, id: &str, count: usize) {
    home.inject("echo", id, &owner_message(id, id, &[]));
    let messages = core.turns.wait_for(count).await;
    assert_eq!(messages[count - 1].text, id);
    wait_until(|| {
        home.lines("echo", "sent.jsonl")
            .iter()
            .any(|line| line["message"]["markdown"] == format!("echo: {id}"))
            .then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_crashed_plugin_is_relaunched_and_its_new_channel_takes_over() {
    let home = Home::serving("[channel.echo]\nunsandboxed = true\n");
    home.install_echo_as("echo");
    let core = home.start_core().await;
    let first = running_pid(&core).unwrap();
    round_trip(&home, &core, "before", 1).await;

    // SAFETY: SIGKILL to the plugin this test launched.
    #[allow(unsafe_code)]
    let killed = unsafe { libc::kill(i32::try_from(first).unwrap(), libc::SIGKILL) };
    assert_eq!(killed, 0);
    let second = wait_until(|| running_pid(&core).filter(|&pid| pid != first)).await;
    assert!(!alive(first));
    let status = core.host.status("echo").unwrap();
    assert_eq!(status.restarts, 1);
    let error = status.last_error.unwrap();
    assert!(error.starts_with("the plugin exited"), "{error}");

    // The relaunched plugin's channel is registered and attached: events and replies flow.
    round_trip(&home, &core, "after", 2).await;
    assert!(alive(second));
    core.stop().await;
    assert!(!alive(second));
}

#[tokio::test]
async fn a_restart_request_relaunches_the_plugin() {
    let home = Home::serving("[channel.echo]\nunsandboxed = true\n");
    home.install_echo_as("echo");
    let core = home.start_core().await;
    let first = running_pid(&core).unwrap();
    assert!(core.host.restart("echo"));
    assert!(!core.host.restart("nope"));
    let second = wait_until(|| running_pid(&core).filter(|&pid| pid != first)).await;
    assert!(!alive(first));
    assert!(alive(second));
    assert_eq!(core.host.status("echo").unwrap().restarts, 1);
    round_trip(&home, &core, "after", 1).await;
    core.stop().await;
}

#[tokio::test]
async fn the_admin_service_shows_and_restarts_a_running_plugin() {
    let home = Home::serving("[channel.echo]\nunsandboxed = true\n");
    home.install_echo_as("echo");
    let core = home.start_core().await;
    let first = running_pid(&core).unwrap();

    // The admin service as `giles serve` wires it, on `run/admin.sock`.
    let lock = InstanceLock::acquire(&home.root).unwrap();
    let listener = AdminListener::bind(&home.root, &lock).unwrap();
    let admin = AdminService::new(AdminOptions {
        config: core.runtime.config.clone(),
        store: core.runtime.store.clone(),
        sandbox: Err("not checked in this test".into()),
        started_at: Timestamp::now(),
        channels: Arc::new(core.sessions.clone()),
        plugins: Some(core.host.clone() as Arc<dyn PluginControl>),
        login_links: None,
        clock: Arc::new(SystemClock),
    });
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(listener.serve(Arc::new(admin), shutdown.clone()));
    let mut client = admin::connect(&home.root).await.unwrap();

    // `giles status` lists every plugin with what supervision knows.
    let status = client
        .status(pb::StatusRequest {})
        .await
        .unwrap()
        .into_inner();
    let [echo] = &status.plugins[..] else {
        panic!("{:?}", status.plugins);
    };
    assert_eq!(echo.instance, "echo");
    assert_eq!(echo.state(), pb::PluginState::Running);
    assert_eq!(echo.pid, Some(first));
    assert_eq!(echo.restarts, 0);
    assert_eq!(echo.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert!(echo.requested_grants.is_some());
    let binary = std::path::Path::new(support::ECHO).canonicalize().unwrap();
    assert_eq!(echo.location, binary.display().to_string());

    // `RestartPlugin` relaunches it.
    client
        .restart_plugin(pb::RestartPluginRequest {
            instance: "echo".into(),
        })
        .await
        .unwrap();
    let second = wait_until(|| running_pid(&core).filter(|&pid| pid != first)).await;
    assert!(!alive(first));
    let plugins = client
        .list_plugins(pb::ListPluginsRequest {})
        .await
        .unwrap()
        .into_inner()
        .plugins;
    assert_eq!(plugins[0].pid, Some(second));
    assert_eq!(plugins[0].restarts, 1);
    let unknown = client
        .restart_plugin(pb::RestartPluginRequest {
            instance: "nope".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), tonic::Code::NotFound);

    shutdown.cancel();
    server.await.unwrap().unwrap();
    drop(lock);
    core.stop().await;
}

#[tokio::test]
async fn a_failure_that_wont_pass_waits_and_a_restart_retries() {
    let home = Home::serving("[channel.echo]\nunsandboxed = true\n");
    // No shared protocol (exit status 76): not retried until the binary changes.
    let script = home.root.plugins_dir().join("giles-channel-echo");
    fs::write(
        &script,
        "#!/bin/sh\necho 'no shared protocol' >&2\nexit 76\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let core = home.start_core().await;
    let status = core.host.status("echo").unwrap();
    assert_eq!(status.state, PluginState::Failed);
    let error = status.last_error.unwrap();
    assert!(error.contains("shares no protocol version"), "{error}");
    assert_eq!(status.binary, Some(script.canonicalize().unwrap()));
    assert!(core.runtime.registry.channel("echo").is_none());

    // The operator installs a working binary and asks for a restart (the admin service's
    // `RestartPlugin`); without the request, the next binary check would find it.
    fs::remove_file(&script).unwrap();
    home.install_echo_as("echo");
    assert!(core.host.restart("echo"));
    wait_until(|| running_pid(&core)).await;
    assert!(core.runtime.registry.channel("echo").is_some());
    round_trip(&home, &core, "works", 1).await;
    core.stop().await;
}

#[tokio::test]
async fn describe_answers_are_cached_for_status() {
    // The namespace comes from the environment, which the config hash doesn't cover.
    let mut home = Home::serving(
        "[channel.echo]\nunsandboxed = true\nconfig = { namespace = { env = \"ECHO_NS\" } }\n",
    );
    home.set_env("ECHO_NS", "echo");
    home.install_echo_as("echo");
    let core = home.start_core().await;
    let PluginStatus {
        describe,
        describe_cached,
        ..
    } = core.host.status("echo").unwrap();
    let describe = describe.unwrap();
    assert!(!describe_cached);
    assert_eq!(describe.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(describe.output, OutputMode::Blocks);
    assert!(describe.can_initiate);
    core.stop().await;
    let cached = home.root.cache_dir().join("describe/echo.json");
    let entry: serde_json::Value = serde_json::from_slice(&fs::read(cached).unwrap()).unwrap();
    assert_eq!(entry["describe"]["version"], env!("CARGO_PKG_VERSION"));

    // The same binary and config fail to start now (the plugin refuses a namespace that isn't
    // its own); status still shows what this binary reported.
    home.set_env("ECHO_NS", "other");
    let core = home.start_core().await;
    let status = core.host.status("echo").unwrap();
    assert_eq!(status.state, PluginState::Failed, "{status:?}");
    assert_eq!(status.describe, Some(describe));
    assert!(status.describe_cached);
    core.stop().await;
}
