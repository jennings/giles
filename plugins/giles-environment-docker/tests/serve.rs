//! The plugin binary served as a remote plugin (`serve --config`), driven over gRPC the way the
//! core drives it: its settings come from the `[config]` table of its own file, `Configure` builds
//! the environment, and workspaces run on a real Docker API socket (skipped when there is none).

mod support;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};

use giles_proto::environment::v1::environment_service_client::EnvironmentServiceClient;
use giles_proto::environment::v1::{
    CreateWorkspaceRequest, DeleteWorkspaceRequest, ExecRequest, InfoRequest,
    ListWorkspacesRequest, StartOutcome, StartWorkspaceRequest, exec_response,
};
use giles_proto::plugin::v1::plugin_service_client::PluginServiceClient;
use giles_proto::plugin::v1::{ConfigureRequest, DescribeRequest, PluginKind};
use tonic::Code;
use tonic::transport::Endpoint;

const PLUGIN: &str = env!("CARGO_BIN_EXE_giles-environment-docker");

/// A running `giles-environment-docker serve`.
struct Served {
    child: Child,
    addr: SocketAddr,
    _dir: tempfile::TempDir,
}

impl Served {
    /// Starts the plugin with `config` as the `[config]` table of its file.
    fn start(config: &toml::Table) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plugin.toml");
        let mut document = toml::Table::new();
        document.insert("config".into(), config.clone().into());
        std::fs::write(&file, toml::to_string(&document).unwrap()).unwrap();
        let mut child = Command::new(PLUGIN)
            .arg("serve")
            .arg("--listen")
            .arg("127.0.0.1:0")
            .arg("--data")
            .arg(dir.path().join("data"))
            .arg("--no-auth")
            .arg("--config")
            .arg(&file)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let first = lines.next().unwrap().unwrap();
        let addr = first
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("unexpected first line {first:?}"))
            .parse()
            .unwrap();
        // Keep reading so the plugin never blocks on a full pipe.
        std::thread::spawn(move || for _ in lines {});
        Self {
            child,
            addr,
            _dir: dir,
        }
    }

    async fn channel(&self) -> tonic::transport::Channel {
        Endpoint::from_shared(format!("http://{}", self.addr))
            .unwrap()
            .connect()
            .await
            .unwrap()
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn configure() -> ConfigureRequest {
    ConfigureRequest {
        instance: "sandbox".into(),
        config_json: "{}".into(),
    }
}

#[tokio::test]
async fn settings_the_plugin_cant_parse_fail_configure_with_the_key() {
    let mut config = toml::Table::new();
    config.insert("memroy".into(), "2g".into());
    let plugin = Served::start(&config);
    let mut base = PluginServiceClient::new(plugin.channel().await);
    let described = base
        .describe(DescribeRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(described.kind(), PluginKind::Environment);
    assert_eq!(described.name, "docker");
    let status = base.configure(configure()).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("memroy"), "{}", status.message());
}

#[tokio::test]
async fn a_served_plugin_runs_workspaces_on_a_real_engine() {
    let Some(harness) = support::harness() else {
        return;
    };
    let config = harness.config();
    let mut table = toml::Table::new();
    table.insert(
        "socket".into(),
        config.socket.unwrap().to_string_lossy().into_owned().into(),
    );
    table.insert(
        "helper_dir".into(),
        config
            .helper_dir
            .unwrap()
            .to_string_lossy()
            .into_owned()
            .into(),
    );
    let plugin = Served::start(&table);
    let channel = plugin.channel().await;
    let mut base = PluginServiceClient::new(channel.clone());
    let mut env = EnvironmentServiceClient::new(channel);
    base.configure(configure()).await.unwrap();

    let info = env.info(InfoRequest {}).await.unwrap().into_inner();
    assert!(info.options_schema_json.contains("alpine:*"));

    let id = format!("e2e-{}", std::process::id());
    let refused = env
        .create_workspace(CreateWorkspaceRequest {
            workspace_id: id.clone(),
            options_json: r#"{"image": "busybox"}"#.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);

    let created = env
        .create_workspace(CreateWorkspaceRequest {
            workspace_id: id.clone(),
            options_json: format!(r#"{{"image": "{}"}}"#, support::IMAGE),
        })
        .await
        .unwrap()
        .into_inner()
        .workspace
        .unwrap();
    assert_eq!(created.working_directory, "/workspace");

    let started = env
        .start_workspace(StartWorkspaceRequest {
            workspace_id: id.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(started.outcome(), StartOutcome::Running);

    let mut events = env
        .exec(ExecRequest {
            workspace_id: id.clone(),
            command: "echo hi; exit 4".into(),
            timeout: Some(prost_types::Duration {
                seconds: 30,
                nanos: 0,
            }),
            output_limit: 1000,
        })
        .await
        .unwrap()
        .into_inner();
    let mut stdout = Vec::new();
    let mut exit = None;
    while let Some(event) = events.message().await.unwrap() {
        match event.event.unwrap() {
            exec_response::Event::Stdout(bytes) => stdout.extend(bytes),
            exec_response::Event::Exit(e) => exit = Some(e),
            exec_response::Event::Stderr(_) => {}
        }
    }
    assert_eq!(stdout, b"hi\n");
    assert_eq!(exit.unwrap().code, 4);

    let listed = env
        .list_workspaces(ListWorkspacesRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.workspaces.len(), 1);
    assert_eq!(listed.workspaces[0].workspace_id, id);

    env.delete_workspace(DeleteWorkspaceRequest {
        workspace_id: id.clone(),
    })
    .await
    .unwrap();
    assert!(
        env.list_workspaces(ListWorkspacesRequest {})
            .await
            .unwrap()
            .into_inner()
            .workspaces
            .is_empty()
    );
}
