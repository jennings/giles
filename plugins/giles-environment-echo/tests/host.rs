//! The environment echo plugin launched by the plugin host (PLAN sections 7.4, 7.5, and 8.8): it
//! is registered once `Info` has answered, serves workspaces through the registry's client,
//! cancels a command when the client drops its stream, is withdrawn from the registry while it is
//! down, and is relaunched after a restart or a crash.

mod support;

use std::fs;
use std::time::Duration;

use futures::StreamExt as _;
use giles_core::admin::PluginControl as _;
use giles_core::config::Kind;
use giles_core::plugin_host::PluginState;
use giles_plugin_sdk::giles_types::{
    Error, ExecEvent, ExecRequest, FindRequest, GrepRequest, StartOutcome, WorkspaceId,
    WorkspaceState,
};

use self::support::{Core, Home, wait_until};

const BOX: &str = "[environment.box]\ncomponent = \"echo\"\nunsandboxed = true\n";

fn workspace(id: &str) -> WorkspaceId {
    WorkspaceId::new(id).unwrap()
}

fn exec(command: &str) -> ExecRequest {
    ExecRequest {
        command: command.to_owned(),
        timeout: Duration::from_secs(30),
        output_limit: 1 << 20,
    }
}

/// A core whose only environment is `box`, run from `tables`.
async fn core_with(tables: &str) -> (Home, Core) {
    let home = Home::new(tables);
    home.install_echo_as("echo");
    let core = home.start_core().await;
    (home, core)
}

#[tokio::test]
async fn it_registers_once_info_has_answered() {
    let tables = format!(
        "{BOX}description = \"A scratch box\"\nstop_after = \"10m\"\n\
         config = {{ label = \"scratch\" }}\n"
    );
    let (home, core) = core_with(&tables).await;
    let registry = core.runtime.registry.clone();

    let entry = registry.environment("box").unwrap();
    assert!(entry.reachable());
    assert_eq!(entry.description, "A scratch box");
    assert_eq!(entry.stop_after, Duration::from_secs(600));
    assert_eq!(entry.delete_after, Duration::from_secs(30 * 24 * 60 * 60));
    assert_eq!(entry.connections, 1);
    // The config reached the plugin's Configure, and the schema is what Info answered after it.
    let schema = entry.options_schema.unwrap();
    assert_eq!(schema["description"], "Options of the scratch environment.");
    assert!(registry.pending().is_empty());
    assert_eq!(registry.environments().len(), 1);

    let status = core.host.environment_status("box").unwrap();
    assert_eq!(status.kind, Kind::Environment);
    let PluginState::Running { pid, .. } = status.state else {
        panic!("{status:?}");
    };
    assert!(pid.is_some());
    assert_eq!(
        status.binary,
        Some(home.root.plugins_dir().join("giles-environment-echo"))
            .map(|link| fs::canonicalize(link).unwrap())
    );
    // A channel of that name isn't it.
    assert!(core.host.status("box").is_none());
    assert_eq!(core.host.statuses().len(), 1);
    assert!(
        home.root
            .cache_dir()
            .join("describe/environment/box.json")
            .is_file()
    );
    assert!(home.data_dir("box").join("tmp").is_dir());

    core.stop().await;
    // Stopping the host takes the plugin out of the registry.
    assert!(!registry.environment("box").unwrap().reachable());
    assert_eq!(registry.pending().len(), 1);
}

#[tokio::test]
async fn the_client_serves_workspaces() {
    let (_home, core) = core_with(BOX).await;
    let client = core
        .runtime
        .registry
        .environment("box")
        .unwrap()
        .client
        .unwrap();
    let id = workspace("w1");

    let created = client
        .create_workspace(&id, serde_json::json!({ "note": "hi" }))
        .await
        .unwrap();
    assert_eq!(created.id, id);
    assert_eq!(created.working_directory, "/work");
    assert_eq!(created.state, WorkspaceState::Running);
    // An id in use and a missing workspace come back as the errors their status codes name.
    let error = client
        .create_workspace(&id, serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error}");
    let error = client.start_workspace(&workspace("w2")).await.unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error}");

    assert_eq!(
        client.start_workspace(&id).await.unwrap(),
        StartOutcome::Running
    );
    client.stop_workspace(&id).await.unwrap();
    let listed = client.list_workspaces().await.unwrap();
    assert_eq!(listed.workspaces.len(), 1);
    assert_eq!(listed.workspaces[0].state, WorkspaceState::Stopped);
    assert_eq!(listed.free_bytes, Some(1 << 30));
    assert!(matches!(
        client.start_workspace(&id).await.unwrap(),
        StartOutcome::Started { .. }
    ));

    let mut events = client.exec(&id, exec("echo hello")).await.unwrap();
    let mut seen = Vec::new();
    while let Some(event) = events.next().await {
        seen.push(event.unwrap());
    }
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], ExecEvent::Stdout(b"hello\n".to_vec()));
    assert!(matches!(&seen[1], ExecEvent::Exit(exit) if exit.code == 0));
    let mut events = client.exec(&id, exec("fail")).await.unwrap();
    assert_eq!(
        events.next().await.unwrap().unwrap(),
        ExecEvent::Stderr(b"failed\n".to_vec())
    );
    assert!(matches!(
        events.next().await.unwrap().unwrap(),
        ExecEvent::Exit(exit) if exit.code == 3
    ));
    assert!(events.next().await.is_none());
    let error = client.exec(&id, exec("rm -rf /")).await.err().unwrap();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error}");

    client
        .write_file(&id, "src/a.txt", b"one\ntwo needle\n".to_vec())
        .await
        .unwrap();
    let file = client.read_file(&id, "src/a.txt", 3).await.unwrap();
    assert_eq!((file.content.as_slice(), file.size), (&b"one"[..], 15));
    let found = client
        .find(
            &id,
            FindRequest {
                pattern: "*a.txt".to_owned(),
                path: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(found.paths, ["src/a.txt"]);
    let grepped = client
        .grep(
            &id,
            GrepRequest {
                pattern: "needle".to_owned(),
                path: None,
                glob: None,
                case_insensitive: false,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(grepped.matches.len(), 1);
    assert_eq!(grepped.matches[0].line, 2);

    client.delete_workspace(&id).await.unwrap();
    assert!(
        client
            .list_workspaces()
            .await
            .unwrap()
            .workspaces
            .is_empty()
    );
    core.stop().await;
}

#[tokio::test]
async fn dropping_the_exec_stream_cancels_the_command() {
    let (home, core) = core_with(BOX).await;
    let client = core
        .runtime
        .registry
        .environment("box")
        .unwrap()
        .client
        .unwrap();
    let id = workspace("w1");
    client
        .create_workspace(&id, serde_json::json!({}))
        .await
        .unwrap();

    let mut events = client.exec(&id, exec("hang")).await.unwrap();
    assert_eq!(
        events.next().await.unwrap().unwrap(),
        ExecEvent::Stdout(b"started\n".to_vec())
    );
    let marker = home.data_dir("box").join("exec-cancelled");
    assert!(!marker.exists());
    drop(events);
    wait_until(|| marker.exists().then_some(())).await;
    core.stop().await;
}

#[tokio::test]
async fn a_restart_connects_again_and_the_plugin_forgets_its_workspaces() {
    let (_home, core) = core_with(BOX).await;
    let registry = &core.runtime.registry;
    let changes = registry.environment_changes();
    let old = registry.environment("box").unwrap().client.unwrap();
    old.create_workspace(&workspace("w1"), serde_json::json!({}))
        .await
        .unwrap();
    let first_pid = match core.host.environment_status("box").unwrap().state {
        PluginState::Running { pid, .. } => pid,
        state => panic!("{state:?}"),
    };

    // A channel `box` isn't restarted by asking for the environment, and the reverse.
    assert!(!core.host.restart(Kind::Channel, "box"));
    assert!(core.host.restart(Kind::Environment, "box"));
    let entry = wait_until(|| {
        let entry = registry.environment("box").unwrap();
        (entry.reachable() && entry.connections == 2).then_some(entry)
    })
    .await;
    assert!(changes.has_changed().unwrap());
    let new = entry.client.unwrap();
    assert!(new.list_workspaces().await.unwrap().workspaces.is_empty());
    let status = core.host.environment_status("box").unwrap();
    assert_eq!(status.restarts, 1);
    let PluginState::Running { pid, .. } = status.state else {
        panic!("{status:?}");
    };
    assert_ne!(pid, first_pid);
    // The old connection died with the old process.
    assert!(old.list_workspaces().await.is_err());
    core.stop().await;
}

#[tokio::test]
async fn a_crashed_plugin_is_unreachable_until_it_is_relaunched() {
    let (_home, core) = core_with(BOX).await;
    let registry = &core.runtime.registry;
    let PluginState::Running { pid: Some(pid), .. } =
        core.host.environment_status("box").unwrap().state
    else {
        panic!("not running");
    };
    let mut changes = registry.environment_changes();

    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    changes.changed().await.unwrap();
    let down = registry.environment("box").unwrap();
    assert!(!down.reachable());
    assert!(down.client.is_none());
    // What it reported stays, and the instance is pending again until the relaunch.
    assert!(down.options_schema.is_some());
    let last_error = core
        .host
        .environment_status("box")
        .unwrap()
        .last_error
        .unwrap();
    assert!(last_error.contains("exited"), "{last_error}");

    let up = wait_until(|| {
        let entry = registry.environment("box").unwrap();
        (entry.reachable() && entry.connections == 2).then_some(entry)
    })
    .await;
    assert!(up.client.is_some());
    core.stop().await;
}
