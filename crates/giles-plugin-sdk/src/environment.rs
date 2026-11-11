//! Serving an [`Environment`] as `giles.environment.v1.EnvironmentService` (PLAN section 8.8).

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream, StreamExt as _};
use giles_proto::MAX_MESSAGE_SIZE;
use giles_proto::environment::v1::environment_service_server::{
    self, EnvironmentService, EnvironmentServiceServer,
};
use giles_proto::environment::v1::{
    CreateWorkspaceRequest, CreateWorkspaceResponse, DeleteWorkspaceRequest,
    DeleteWorkspaceResponse, EnvironmentCapabilities, ExecRequest as PbExecRequest, ExecResponse,
    FindRequest as PbFindRequest, FindResponse, GrepRequest as PbGrepRequest, GrepResponse,
    InfoRequest, InfoResponse, ListWorkspacesRequest, ListWorkspacesResponse, ReadFileRequest,
    ReadFileResponse, StartWorkspaceRequest, StartWorkspaceResponse, StopWorkspaceRequest,
    StopWorkspaceResponse, WriteFileRequest, WriteFileResponse,
};
use giles_proto::plugin::v1::{DescribeResponse, PluginKind, describe_response};
use giles_types::{Environment, ExecEvent, Result, WorkspaceId};
use tonic::service::Routes;
use tonic::{Request, Response, Status};
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

use crate::base::{Grants, Kind};
use crate::blobs::Blobs;
use crate::configure::Configure;
use crate::convert::environment::{
    exec_event_to_proto, exec_request_from_proto, find_request_from_proto, find_result_to_proto,
    grep_request_from_proto, grep_result_to_proto, info_to_proto, options_from_proto,
    read_file_to_proto, start_outcome_to_proto, workspace_list_to_proto, workspace_to_proto,
};
use crate::convert::status_from_error;
use crate::remote::RemoteServer;
use crate::run;
use crate::server::{self, Context};

/// An environment plugin: what it reports in `Describe`, and how it builds its [`Environment`].
///
/// An environment plugin gives agents isolated workspaces: it creates and deletes them and serves
/// the primitives (run a command, read and write a file, find, grep) that Giles builds the model's
/// workspace tools on. Call [`run`](EnvironmentPlugin::run) from `main`. It parses the plugin's
/// command line: `socket` does everything a launched plugin must do and returns when the plugin
/// should exit, and `serve` runs the plugin for its operator as a remote plugin.
///
/// ```no_run
/// use std::process::ExitCode;
///
/// use futures::stream::BoxStream;
/// use giles_plugin_sdk::giles_types::{
///     Environment, EnvironmentInfo, Error, ExecEvent, ExecRequest, FindRequest, FindResult,
///     GrepRequest, GrepResult, ReadFile, Result, StartOutcome, Workspace, WorkspaceId,
///     WorkspaceList,
/// };
/// use giles_plugin_sdk::{Configure, EnvironmentPlugin, async_trait};
///
/// #[derive(serde::Deserialize)]
/// #[serde(deny_unknown_fields)]
/// struct Config {
///     engine: String,
/// }
///
/// struct Example {
///     engine: String,
/// }
///
/// fn unsupported<T>(what: &str) -> Result<T> {
///     Err(Error::InvalidRequest(format!("{what} isn't implemented")))
/// }
///
/// #[async_trait]
/// impl Environment for Example {
///     async fn info(&self) -> Result<EnvironmentInfo> {
///         Ok(EnvironmentInfo {
///             options_schema: serde_json::json!({ "type": "object", "properties": {} }),
///         })
///     }
///     async fn create_workspace(
///         &self,
///         _id: &WorkspaceId,
///         _options: serde_json::Value,
///     ) -> Result<Workspace> {
///         unsupported(&self.engine)
///     }
///     async fn delete_workspace(&self, _id: &WorkspaceId) -> Result<()> {
///         Ok(())
///     }
///     async fn start_workspace(&self, _id: &WorkspaceId) -> Result<StartOutcome> {
///         Ok(StartOutcome::Running)
///     }
///     async fn stop_workspace(&self, _id: &WorkspaceId) -> Result<()> {
///         Ok(())
///     }
///     async fn list_workspaces(&self) -> Result<WorkspaceList> {
///         Ok(WorkspaceList { workspaces: vec![], free_bytes: None })
///     }
///     async fn exec(
///         &self,
///         _id: &WorkspaceId,
///         _request: ExecRequest,
///     ) -> Result<BoxStream<'static, Result<ExecEvent>>> {
///         unsupported("exec")
///     }
///     async fn read_file(&self, _id: &WorkspaceId, _path: &str, _max: u64) -> Result<ReadFile> {
///         unsupported("read_file")
///     }
///     async fn write_file(&self, _id: &WorkspaceId, _path: &str, _content: Vec<u8>) -> Result<()> {
///         unsupported("write_file")
///     }
///     async fn find(&self, _id: &WorkspaceId, _request: FindRequest) -> Result<FindResult> {
///         unsupported("find")
///     }
///     async fn grep(&self, _id: &WorkspaceId, _request: GrepRequest) -> Result<GrepResult> {
///         unsupported("grep")
///     }
/// }
///
/// #[tokio::main]
/// async fn main() -> ExitCode {
///     EnvironmentPlugin::new("example", env!("CARGO_PKG_VERSION"))
///         .run(|ctx: Configure| async move {
///             let config: Config = ctx.parse_config()?;
///             Ok(Example { engine: config.engine })
///         })
///         .await
/// }
/// ```
#[derive(Debug, Clone)]
pub struct EnvironmentPlugin {
    name: String,
    version: String,
    requested_grants: Grants,
}

impl EnvironmentPlugin {
    /// An environment plugin named `name`, the `<name>` in its binary's name
    /// `giles-environment-<name>`, at `version` (usually `env!("CARGO_PKG_VERSION")`). It requests
    /// no grants until told otherwise.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            requested_grants: Grants::default(),
        }
    }

    /// Sandbox grants to suggest to the operator, e.g. the container engine's socket in
    /// [`Grants::unix`]. Advisory only.
    pub fn requested_grants(mut self, grants: Grants) -> Self {
        self.requested_grants = grants;
        self
    }

    /// Runs the plugin until it should exit, and returns its exit status: return it from `main`.
    ///
    /// The SDK performs the launch handshake (PLAN section 7.4), serves `PluginService`,
    /// `grpc.health.v1.Health`, and `EnvironmentService` on fd 3, and calls `configure` when the
    /// core sends the instance config. Errors from `configure` go back to the core:
    /// [`Error::InvalidRequest`](giles_types::Error::InvalidRequest) for a bad config, naming the
    /// key. After a successful `Configure`, `configure` isn't called again: a repeated
    /// `Configure` with the same instance name and config gets the same answer, and one with a
    /// different config fails with `FAILED_PRECONDITION`.
    ///
    /// It returns when the core asks the plugin to shut down or goes away (stdin closes), after
    /// giving open calls a few seconds to finish. Run by hand, it explains that it's a Giles plugin
    /// and returns a failure status. Startup failures are printed to stderr.
    ///
    /// Call it first thing in `main`, before anything opens or closes file descriptors. It uses the
    /// ambient tokio runtime, which must enable I/O and timers (`#[tokio::main]` does).
    pub async fn serve<F, Fut, E>(self, configure: F) -> ExitCode
    where
        F: Fn(Configure) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<E>> + Send + 'static,
        E: Environment + 'static,
    {
        let display_name = crate::cli::binary_name(giles_types::Kind::Environment, &self.name);
        let configure = configure_fn(configure);
        server::run(
            &display_name,
            Box::new(move |context| Ok(self.services(configure, None, context))),
        )
        .await
    }

    /// Runs the plugin as a remote plugin (PLAN section 7.6) on `server`'s listener until its
    /// shutdown signal (`SIGINT` or `SIGTERM` by default), then gives open calls a few seconds to
    /// finish and returns.
    ///
    /// The services and `configure` work as with [`serve`](Self::serve), with the state in the
    /// server's data directory, except for the lifecycle: one core at a time connects, and it may
    /// disconnect and connect again (a restart, a network failure). Each connection calls
    /// `Describe` and `Configure` again; the environment built by the first `Configure` stays.
    /// `Shutdown` is only the core disconnecting, and the plugin keeps serving. To change the
    /// instance's settings, restart the plugin.
    ///
    /// An environment plugin's settings don't come from the core in this mode. `config` is the
    /// plugin's own settings, normally the `[config]` table of its `--config` file (`{}` for
    /// none), and it is what [`Configure::config`] returns. The core sends no config for a remote
    /// environment, and a `Configure` that carries one is refused with `INVALID_ARGUMENT`.
    ///
    /// Unlike [`serve`](Self::serve), it doesn't need to run first in `main`, and several can
    /// run in one process (each with its own data directory).
    ///
    /// # Errors
    ///
    /// The data directory or the SDK's state in it can't be opened, or the gRPC server failed.
    pub async fn serve_remote<F, Fut, E>(
        self,
        server: RemoteServer,
        config: serde_json::Value,
        configure: F,
    ) -> io::Result<()>
    where
        F: Fn(Configure) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<E>> + Send + 'static,
        E: Environment + 'static,
    {
        let configure = configure_fn(configure);
        server::run_remote(
            server,
            Box::new(move |context| Ok(self.services(configure, Some(config), context))),
        )
        .await
    }

    /// Runs the plugin as its command line says, and returns its exit status: return it from
    /// `main`. This is what a shipped plugin's `main` calls instead of [`serve`](Self::serve) or
    /// [`serve_remote`](Self::serve_remote).
    ///
    /// - `socket` is how Giles launches the plugin: [`serve`](Self::serve). Run by hand, it
    ///   explains that it's a Giles plugin.
    /// - `serve [options]` is a plugin its operator runs, for a core whose instance has an
    ///   `address`: [`serve_remote`](Self::serve_remote) on a TCP listener, with TLS if the
    ///   options give a certificate, and checking a static bearer token. The options are
    ///   `--listen`, `--data`, `--tls-cert`, `--tls-key`, `--token-file`, and `--no-auth`, or the
    ///   same as keys of a TOML file named by `--config` (a flag wins). The token is the
    ///   `GILES_ENVIRONMENT_<NAME>_TOKEN` environment variable (`<NAME>` is the plugin's name in
    ///   capitals, with `_` for anything but letters and digits) or `--token-file`, a file only
    ///   its owner can access; without either, `serve` refuses to start unless `--no-auth` says
    ///   to accept any caller. It prints `listening on <address>` to stdout once it listens.
    /// - Anything else prints the usage, to stdout for `--help` and to stderr for a mistake (and
    ///   exits with status 2).
    ///
    /// The plugin's own settings reach `configure` in both modes, but from different places. A
    /// launched plugin gets the instance's `config` from the core's `giles.toml`. A plugin run
    /// with `serve` gets the `[config]` table of its `--config` file, or `{}` without one: the
    /// instance in `giles.toml` has an `address` and no `config`.
    ///
    /// As with [`serve`](Self::serve), call it first thing in `main`, with tokio's runtime
    /// enabling I/O and timers.
    pub async fn run<F, Fut, E>(self, configure: F) -> ExitCode
    where
        F: Fn(Configure) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<E>> + Send + 'static,
        E: Environment + 'static,
    {
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        self.run_with(&args, &|name| std::env::var_os(name), configure)
            .await
    }

    async fn run_with<F, Fut, E>(
        self,
        args: &[OsString],
        env: &dyn Fn(&str) -> Option<OsString>,
        configure: F,
    ) -> ExitCode
    where
        F: Fn(Configure) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<E>> + Send + 'static,
        E: Environment + 'static,
    {
        let name = self.name.clone();
        run::run(
            giles_types::Kind::Environment,
            &name,
            args,
            env,
            (self, configure),
            |(plugin, configure)| plugin.serve(configure),
            |(plugin, configure), server, config| {
                let config =
                    config.unwrap_or_else(|| serde_json::Value::Object(Default::default()));
                plugin.serve_remote(server, config, configure)
            },
        )
        .await
    }

    /// The services for a plugin whose settings come from `own_config` (a remote plugin) or from
    /// the core (`None`).
    fn services(
        self,
        configure: Box<ConfigureFn>,
        own_config: Option<serde_json::Value>,
        context: &Context,
    ) -> (Arc<dyn Kind>, Routes) {
        let shared = Arc::new(Shared {
            meta: self,
            configure,
            own_config,
            data_dir: context.data_dir.clone(),
            blobs: context.blobs.clone(),
            health: context.health.clone(),
            configure_lock: tokio::sync::Mutex::new(()),
            configured: OnceLock::new(),
        });
        let service = EnvironmentServiceServer::new(EnvironmentServer(Arc::clone(&shared)))
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);
        (Arc::new(EnvironmentKind(shared)), Routes::new(service))
    }
}

type ConfigureFn =
    dyn Fn(Configure) -> BoxFuture<'static, Result<Arc<dyn Environment>>> + Send + Sync;

/// Boxes an author's `configure`.
fn configure_fn<F, Fut, E>(configure: F) -> Box<ConfigureFn>
where
    F: Fn(Configure) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<E>> + Send + 'static,
    E: Environment + 'static,
{
    Box::new(move |context| {
        let built = configure(context);
        Box::pin(async move { Ok(Arc::new(built.await?) as Arc<dyn Environment>) })
    })
}

/// The result of the successful `Configure`.
struct Configured {
    instance: String,
    config: serde_json::Value,
    environment: Arc<dyn Environment>,
}

/// State shared by the `Describe`/`Configure` half and `EnvironmentService`.
struct Shared {
    meta: EnvironmentPlugin,
    configure: Box<ConfigureFn>,
    /// The plugin's own settings, for a remote plugin; the core sends none.
    own_config: Option<serde_json::Value>,
    data_dir: PathBuf,
    blobs: Blobs,
    health: HealthReporter,
    /// Serializes `Configure` calls.
    configure_lock: tokio::sync::Mutex<()>,
    configured: OnceLock<Configured>,
}

impl Shared {
    fn environment(&self) -> Result<Arc<dyn Environment>, Status> {
        self.configured
            .get()
            .map(|configured| Arc::clone(&configured.environment))
            .ok_or_else(|| Status::failed_precondition("the plugin isn't configured yet"))
    }
}

/// The `Describe` and `Configure` half.
struct EnvironmentKind(Arc<Shared>);

#[async_trait]
impl Kind for EnvironmentKind {
    fn service_name(&self) -> &'static str {
        environment_service_server::SERVICE_NAME
    }

    fn describe(&self) -> DescribeResponse {
        let meta = &self.0.meta;
        DescribeResponse {
            kind: PluginKind::Environment.into(),
            name: meta.name.clone(),
            version: meta.version.clone(),
            requested_grants: Some(meta.requested_grants.clone().into()),
            capabilities: Some(describe_response::Capabilities::Environment(
                EnvironmentCapabilities {},
            )),
        }
    }

    async fn configure(
        &self,
        instance: String,
        config: serde_json::Value,
    ) -> Result<String, Status> {
        let shared = &self.0;
        // A remote environment's settings live in its own file, so what the core sends must be
        // empty: a `config` set next to `address` is a mistake worth naming, not ignoring.
        let config = match &shared.own_config {
            Some(own) => {
                let empty = match &config {
                    serde_json::Value::Null => true,
                    serde_json::Value::Object(map) => map.is_empty(),
                    _ => false,
                };
                if !empty {
                    return Err(Status::invalid_argument(
                        "this environment plugin runs remotely, and a remote environment's \
                         settings live in its own config file ([config] in the file given to \
                         `serve --config`); remove `config` from this instance in giles.toml",
                    ));
                }
                own.clone()
            }
            None => config,
        };
        let _serialized = shared.configure_lock.lock().await;
        if let Some(configured) = shared.configured.get() {
            // A remote plugin's core reconnecting configures it again.
            return if configured.instance == instance && configured.config == config {
                Ok(String::new())
            } else {
                Err(Status::failed_precondition(
                    "the plugin is already configured with another instance name or config; \
                     restart it to change them",
                ))
            };
        }
        tracing::info!(%instance, "configuring");
        let context = Configure::new(
            instance.clone(),
            config.clone(),
            shared.data_dir.clone(),
            shared.blobs.clone(),
        );
        let environment = (shared.configure)(context)
            .await
            .map_err(status_from_error)?;
        let _ = shared.configured.set(Configured {
            instance,
            config,
            environment,
        });
        shared
            .health
            .set_service_status(
                environment_service_server::SERVICE_NAME,
                ServingStatus::Serving,
            )
            .await;
        Ok(String::new())
    }

    fn core_detached(&self) {}
}

/// `EnvironmentService`.
struct EnvironmentServer(Arc<Shared>);

/// A workspace id from the core.
fn workspace_id(id: String) -> Result<WorkspaceId, Status> {
    WorkspaceId::new(id).map_err(|e| status_from_error(e.into()))
}

#[async_trait]
impl EnvironmentService for EnvironmentServer {
    async fn info(&self, _request: Request<InfoRequest>) -> Result<Response<InfoResponse>, Status> {
        let info = self
            .0
            .environment()?
            .info()
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(info_to_proto(&info)))
    }

    async fn create_workspace(
        &self,
        request: Request<CreateWorkspaceRequest>,
    ) -> Result<Response<CreateWorkspaceResponse>, Status> {
        let environment = self.0.environment()?;
        let request = request.into_inner();
        let id = workspace_id(request.workspace_id)?;
        let options = options_from_proto(&request.options_json).map_err(status_from_error)?;
        let workspace = environment
            .create_workspace(&id, options)
            .await
            .map_err(status_from_error)?;
        if workspace.id != id {
            return Err(Status::internal(format!(
                "plugin bug: create_workspace({id}) returned workspace {}",
                workspace.id
            )));
        }
        Ok(Response::new(CreateWorkspaceResponse {
            workspace: Some(workspace_to_proto(workspace).map_err(status_from_error)?),
        }))
    }

    async fn delete_workspace(
        &self,
        request: Request<DeleteWorkspaceRequest>,
    ) -> Result<Response<DeleteWorkspaceResponse>, Status> {
        let environment = self.0.environment()?;
        let id = workspace_id(request.into_inner().workspace_id)?;
        environment
            .delete_workspace(&id)
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(DeleteWorkspaceResponse {}))
    }

    async fn start_workspace(
        &self,
        request: Request<StartWorkspaceRequest>,
    ) -> Result<Response<StartWorkspaceResponse>, Status> {
        let environment = self.0.environment()?;
        let id = workspace_id(request.into_inner().workspace_id)?;
        let outcome = environment
            .start_workspace(&id)
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(
            start_outcome_to_proto(outcome).map_err(status_from_error)?,
        ))
    }

    async fn stop_workspace(
        &self,
        request: Request<StopWorkspaceRequest>,
    ) -> Result<Response<StopWorkspaceResponse>, Status> {
        let environment = self.0.environment()?;
        let id = workspace_id(request.into_inner().workspace_id)?;
        environment
            .stop_workspace(&id)
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(StopWorkspaceResponse {}))
    }

    async fn list_workspaces(
        &self,
        _request: Request<ListWorkspacesRequest>,
    ) -> Result<Response<ListWorkspacesResponse>, Status> {
        let list = self
            .0
            .environment()?
            .list_workspaces()
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(
            workspace_list_to_proto(list).map_err(status_from_error)?,
        ))
    }

    type ExecStream = BoxStream<'static, Result<ExecResponse, Status>>;

    async fn exec(
        &self,
        request: Request<PbExecRequest>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        let environment = self.0.environment()?;
        let (id, request) =
            exec_request_from_proto(request.into_inner()).map_err(status_from_error)?;
        let events = environment
            .exec(&id, request)
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(exec_responses(events)))
    }

    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<ReadFileResponse>, Status> {
        let environment = self.0.environment()?;
        let request = request.into_inner();
        let id = workspace_id(request.workspace_id)?;
        let file = environment
            .read_file(&id, &request.path, request.max_bytes)
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(read_file_to_proto(file)))
    }

    async fn write_file(
        &self,
        request: Request<WriteFileRequest>,
    ) -> Result<Response<WriteFileResponse>, Status> {
        let environment = self.0.environment()?;
        let request = request.into_inner();
        let id = workspace_id(request.workspace_id)?;
        environment
            .write_file(&id, &request.path, request.content)
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(WriteFileResponse {}))
    }

    async fn find(
        &self,
        request: Request<PbFindRequest>,
    ) -> Result<Response<FindResponse>, Status> {
        let environment = self.0.environment()?;
        let (id, request) =
            find_request_from_proto(request.into_inner()).map_err(status_from_error)?;
        let result = environment
            .find(&id, request)
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(find_result_to_proto(result)))
    }

    async fn grep(
        &self,
        request: Request<PbGrepRequest>,
    ) -> Result<Response<GrepResponse>, Status> {
        let environment = self.0.environment()?;
        let (id, request) =
            grep_request_from_proto(request.into_inner()).map_err(status_from_error)?;
        let result = environment
            .grep(&id, request)
            .await
            .map_err(status_from_error)?;
        Ok(Response::new(grep_result_to_proto(result)))
    }
}

/// The response stream of an `Exec` call: the environment's events, ending with the exit.
///
/// The stream owns the environment's, so it is dropped together with it: when the core cancels the
/// call tonic drops the response stream, which drops `events`, and that is the environment's cue to
/// kill the command. It is dropped as soon as the exit event is out too, and after an error. An
/// environment whose stream ends without an exit event is a plugin bug the core must not mistake
/// for a command's end.
fn exec_responses(
    events: BoxStream<'static, Result<ExecEvent>>,
) -> BoxStream<'static, Result<ExecResponse, Status>> {
    stream::unfold(Some(events), |state| async move {
        let mut events = state?;
        let item = match events.next().await {
            Some(Ok(event)) => {
                let last = matches!(event, ExecEvent::Exit(_));
                let response = exec_event_to_proto(event).map_err(status_from_error);
                return Some((response, (!last).then_some(events)));
            }
            Some(Err(e)) => status_from_error(e),
            None => Status::internal("plugin bug: the exec stream ended without an exit event"),
        };
        Some((Err(item), None))
    })
    .boxed()
}

#[cfg(test)]
mod tests;
