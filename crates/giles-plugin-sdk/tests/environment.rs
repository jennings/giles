//! An environment served as a remote plugin (`EnvironmentPlugin::serve_remote`) on `127.0.0.1`:
//! the plugin's own settings reach `configure`, the core's must be empty, and the environment
//! service answers behind the same bearer check as the base service.

use std::net::SocketAddr;
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Environment, EnvironmentInfo, Error, ExecEvent, ExecExit, ExecRequest, FindRequest, FindResult,
    GrepRequest, GrepResult, ReadFile, Result, StartOutcome, Workspace, WorkspaceId, WorkspaceList,
};
use giles_plugin_sdk::{Configure, EnvironmentPlugin, RemoteAuth, RemoteServer, async_trait};
use giles_proto::environment::v1::environment_service_client::EnvironmentServiceClient;
use giles_proto::environment::v1::{ExecRequest as PbExecRequest, InfoRequest, exec_response};
use giles_proto::plugin::v1::plugin_service_client::PluginServiceClient;
use giles_proto::plugin::v1::{ConfigureRequest, DescribeRequest, PluginKind};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use tonic::metadata::MetadataValue;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};

const WAIT: Duration = Duration::from_secs(10);

/// An environment that greets with the `greeting` of its settings.
struct Greeter {
    greeting: String,
}

fn unsupported<T>(what: &str) -> Result<T> {
    Err(Error::InvalidRequest(format!("{what} isn't supported")))
}

#[async_trait]
impl Environment for Greeter {
    async fn info(&self) -> Result<EnvironmentInfo> {
        Ok(EnvironmentInfo {
            options_schema: serde_json::json!({"description": self.greeting}),
        })
    }
    async fn create_workspace(&self, _: &WorkspaceId, _: serde_json::Value) -> Result<Workspace> {
        unsupported("create_workspace")
    }
    async fn delete_workspace(&self, _: &WorkspaceId) -> Result<()> {
        Ok(())
    }
    async fn start_workspace(&self, _: &WorkspaceId) -> Result<StartOutcome> {
        Ok(StartOutcome::Running)
    }
    async fn stop_workspace(&self, _: &WorkspaceId) -> Result<()> {
        Ok(())
    }
    async fn list_workspaces(&self) -> Result<WorkspaceList> {
        Ok(WorkspaceList {
            workspaces: vec![],
            free_bytes: None,
        })
    }
    async fn exec(
        &self,
        _: &WorkspaceId,
        _: ExecRequest,
    ) -> Result<BoxStream<'static, Result<ExecEvent>>> {
        Ok(stream::iter([
            Ok(ExecEvent::Stdout(self.greeting.clone().into_bytes())),
            Ok(ExecEvent::Exit(ExecExit {
                code: 0,
                timed_out: false,
                dropped_bytes: 0,
            })),
        ])
        .boxed())
    }
    async fn read_file(&self, _: &WorkspaceId, _: &str, _: u64) -> Result<ReadFile> {
        unsupported("read_file")
    }
    async fn write_file(&self, _: &WorkspaceId, _: &str, _: Vec<u8>) -> Result<()> {
        unsupported("write_file")
    }
    async fn find(&self, _: &WorkspaceId, _: FindRequest) -> Result<FindResult> {
        unsupported("find")
    }
    async fn grep(&self, _: &WorkspaceId, _: GrepRequest) -> Result<GrepResult> {
        unsupported("grep")
    }
}

/// A running remote environment plugin with `config` as its own settings.
struct Plugin {
    _data: tempfile::TempDir,
    addr: SocketAddr,
    stop: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
}

impl Plugin {
    async fn start(config: serde_json::Value) -> Self {
        let data = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stop = CancellationToken::new();
        let server = RemoteServer::new(
            listener,
            data.path().join("state"),
            RemoteAuth::Bearer("s3cret".into()),
        )
        .shutdown_on(stop.clone().cancelled_owned());
        let addr = server.local_addr().unwrap();
        let task = tokio::spawn(EnvironmentPlugin::new("greeter", "1.0.0").serve_remote(
            server,
            config,
            |ctx: Configure| async move {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Settings {
                    greeting: String,
                }
                let settings: Settings = ctx.parse_config()?;
                Ok(Greeter {
                    greeting: settings.greeting,
                })
            },
        ));
        Self {
            _data: data,
            addr,
            stop,
            task,
        }
    }

    async fn stop(self) {
        self.stop.cancel();
        tokio::time::timeout(WAIT, self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    async fn channel(&self) -> InterceptedService<Channel, Bearer> {
        let channel = Endpoint::from_shared(format!("http://{}", self.addr))
            .unwrap()
            .connect()
            .await
            .unwrap();
        InterceptedService::new(channel, Bearer)
    }
}

#[derive(Clone)]
struct Bearer;

impl Interceptor for Bearer {
    fn call(&mut self, mut request: tonic::Request<()>) -> tonic::Result<tonic::Request<()>> {
        let value: MetadataValue<_> = "Bearer s3cret".parse().unwrap();
        request.metadata_mut().insert("authorization", value);
        Ok(request)
    }
}

fn configure_request(config_json: &str) -> ConfigureRequest {
    ConfigureRequest {
        instance: "sandbox".into(),
        config_json: config_json.into(),
    }
}

#[tokio::test]
async fn a_remote_environment_serves_with_its_own_settings() {
    let plugin = Plugin::start(serde_json::json!({"greeting": "hello"})).await;
    let channel = plugin.channel().await;
    let mut base = PluginServiceClient::new(channel.clone());
    let mut env = EnvironmentServiceClient::new(channel);

    let described = base
        .describe(DescribeRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(described.kind(), PluginKind::Environment);
    assert_eq!(described.name, "greeter");

    // Not usable until the core has configured it.
    let early = env.info(InfoRequest {}).await.unwrap_err();
    assert_eq!(early.code(), Code::FailedPrecondition);

    // The core's config must be empty: the settings are the plugin's.
    let refused = base
        .configure(configure_request(r#"{"greeting": "from the core"}"#))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert!(
        refused.message().contains("config file"),
        "{}",
        refused.message()
    );
    base.configure(configure_request("{}")).await.unwrap();

    let info = env.info(InfoRequest {}).await.unwrap().into_inner();
    assert_eq!(info.options_schema_json, r#"{"description":"hello"}"#);

    let mut events = env
        .exec(PbExecRequest {
            workspace_id: "ws-1".into(),
            command: "greet".into(),
            timeout: Some(prost_types::Duration {
                seconds: 5,
                nanos: 0,
            }),
            output_limit: 100,
        })
        .await
        .unwrap()
        .into_inner();
    let first = events.message().await.unwrap().unwrap();
    assert_eq!(
        first.event,
        Some(exec_response::Event::Stdout(b"hello".to_vec()))
    );
    let last = events.message().await.unwrap().unwrap();
    assert!(matches!(last.event, Some(exec_response::Event::Exit(_))));
    assert!(events.message().await.unwrap().is_none());

    plugin.stop().await;
}

#[tokio::test]
async fn settings_the_plugin_cant_parse_fail_configure_with_the_key() {
    let plugin = Plugin::start(serde_json::json!({"greeting": "hello", "typo": 1})).await;
    let mut base = PluginServiceClient::new(plugin.channel().await);
    let status = base.configure(configure_request("{}")).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("typo"), "{}", status.message());
    plugin.stop().await;
}
