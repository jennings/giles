//! The `RemoteEnvironment` adapter (PLAN section 8.8): a plugin's `EnvironmentService` as a
//! [`giles_types::Environment`].
//!
//! Requests and responses are converted with the SDK's conversions
//! (`giles_plugin_sdk::convert::environment`). A failed call becomes the component error its
//! status code names, with its message scrubbed of the instance's secrets: the message may be
//! shown to the model. Dropping the stream [`exec`](Environment::exec) returns drops the gRPC call,
//! which cancels it: the plugin sees the cancellation and kills the command.

use std::fmt;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::convert::environment::{
    exec_event_from_proto, exec_request_to_proto, find_request_to_proto, find_result_from_proto,
    grep_request_to_proto, grep_result_from_proto, info_from_proto, read_file_from_proto,
    start_outcome_from_proto, workspace_from_proto, workspace_list_from_proto,
};
use giles_proto::environment::v1 as pb;
use giles_types::{
    Environment, EnvironmentInfo, Error, ExecEvent, ExecRequest, FindRequest, FindResult,
    GrepRequest, GrepResult, ReadFile, Result, StartOutcome, Workspace, WorkspaceId, WorkspaceList,
};
use tonic::{Code, Status, Streaming};
use tracing::Instrument as _;

use super::{LaunchedPlugin, PluginConnection, RemotePlugin, Secrets};

/// A launched or remote plugin's environment service as an [`Environment`]. One per launch: a
/// restarted plugin gets a new one.
pub struct RemoteEnvironment {
    connection: PluginConnection,
    secrets: Secrets,
    span: tracing::Span,
}

impl fmt::Debug for RemoteEnvironment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteEnvironment").finish_non_exhaustive()
    }
}

impl RemoteEnvironment {
    /// The environment of `plugin`.
    pub fn new(plugin: &LaunchedPlugin) -> Self {
        Self {
            connection: plugin.connection.clone(),
            secrets: Secrets::new(plugin.secrets.clone()),
            span: plugin.span.clone(),
        }
    }

    /// [`RemoteEnvironment::new`] for a remote plugin.
    pub fn from_remote(plugin: &RemotePlugin) -> Self {
        Self {
            connection: plugin.connection.clone(),
            secrets: plugin.secrets.clone(),
            span: plugin.span.clone(),
        }
    }

    /// A failed call as a component error. `UNAVAILABLE`, and `CANCELLED` and `DEADLINE_EXCEEDED`,
    /// which a call gets when the plugin dies or drains for a restart mid-call, are
    /// [`Error::Unavailable`]; `INVALID_ARGUMENT` is [`Error::InvalidRequest`] and `NOT_FOUND` is
    /// [`Error::NotFound`]; anything else is [`Error::Other`]. The message is scrubbed.
    fn error(&self, call: &str, status: &Status) -> Error {
        rpc_error(&self.secrets, call, status)
    }
}

fn rpc_error(secrets: &Secrets, call: &str, status: &Status) -> Error {
    let message = secrets.scrub(status.message());
    match status.code() {
        Code::Unavailable => Error::unavailable(format!("{call}: {message}")),
        code @ (Code::Cancelled | Code::DeadlineExceeded) => {
            Error::unavailable(format!("{call} failed: {code:?}: {message}"))
        }
        Code::InvalidArgument => Error::InvalidRequest(format!("{call}: {message}")),
        Code::NotFound => Error::NotFound(format!("{call}: {message}")),
        code => Error::other(format!("{call} failed: {code:?}: {message}")),
    }
}

#[async_trait]
impl Environment for RemoteEnvironment {
    async fn info(&self) -> Result<EnvironmentInfo> {
        let response = self
            .connection
            .environment()
            .info(pb::InfoRequest {})
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("Info", &status))?;
        info_from_proto(response.into_inner())
    }

    async fn create_workspace(
        &self,
        id: &WorkspaceId,
        options: serde_json::Value,
    ) -> Result<Workspace> {
        let request = pb::CreateWorkspaceRequest {
            workspace_id: id.to_string(),
            options_json: options.to_string(),
        };
        let response = self
            .connection
            .environment()
            .create_workspace(request)
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("CreateWorkspace", &status))?;
        let workspace = response
            .into_inner()
            .workspace
            .ok_or_else(|| Error::other("the plugin's CreateWorkspace returned no workspace"))?;
        workspace_from_proto(workspace)
    }

    async fn delete_workspace(&self, id: &WorkspaceId) -> Result<()> {
        let request = pb::DeleteWorkspaceRequest {
            workspace_id: id.to_string(),
        };
        self.connection
            .environment()
            .delete_workspace(request)
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("DeleteWorkspace", &status))?;
        Ok(())
    }

    async fn start_workspace(&self, id: &WorkspaceId) -> Result<StartOutcome> {
        let request = pb::StartWorkspaceRequest {
            workspace_id: id.to_string(),
        };
        let response = self
            .connection
            .environment()
            .start_workspace(request)
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("StartWorkspace", &status))?;
        start_outcome_from_proto(response.into_inner())
    }

    async fn stop_workspace(&self, id: &WorkspaceId) -> Result<()> {
        let request = pb::StopWorkspaceRequest {
            workspace_id: id.to_string(),
        };
        self.connection
            .environment()
            .stop_workspace(request)
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("StopWorkspace", &status))?;
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<WorkspaceList> {
        let response = self
            .connection
            .environment()
            .list_workspaces(pb::ListWorkspacesRequest {})
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("ListWorkspaces", &status))?;
        workspace_list_from_proto(response.into_inner())
    }

    async fn exec(
        &self,
        id: &WorkspaceId,
        request: ExecRequest,
    ) -> Result<BoxStream<'static, Result<ExecEvent>>> {
        let response = self
            .connection
            .environment()
            .exec(exec_request_to_proto(id, request))
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("Exec", &status))?;
        let events = ExecEvents {
            inner: response.into_inner(),
            secrets: self.secrets.clone(),
            finished: false,
        };
        Ok(stream::unfold(events, |mut events| async move {
            let event = events.next().await?;
            Some((event, events))
        })
        .boxed())
    }

    async fn read_file(&self, id: &WorkspaceId, path: &str, max_bytes: u64) -> Result<ReadFile> {
        let request = pb::ReadFileRequest {
            workspace_id: id.to_string(),
            path: path.to_owned(),
            max_bytes,
        };
        let response = self
            .connection
            .environment()
            .read_file(request)
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("ReadFile", &status))?;
        Ok(read_file_from_proto(response.into_inner()))
    }

    async fn write_file(&self, id: &WorkspaceId, path: &str, content: Vec<u8>) -> Result<()> {
        let request = pb::WriteFileRequest {
            workspace_id: id.to_string(),
            path: path.to_owned(),
            content,
        };
        self.connection
            .environment()
            .write_file(request)
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("WriteFile", &status))?;
        Ok(())
    }

    async fn find(&self, id: &WorkspaceId, request: FindRequest) -> Result<FindResult> {
        let response = self
            .connection
            .environment()
            .find(find_request_to_proto(id, request))
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("Find", &status))?;
        Ok(find_result_from_proto(response.into_inner()))
    }

    async fn grep(&self, id: &WorkspaceId, request: GrepRequest) -> Result<GrepResult> {
        let response = self
            .connection
            .environment()
            .grep(grep_request_to_proto(id, request))
            .instrument(self.span.clone())
            .await
            .map_err(|status| self.error("Grep", &status))?;
        Ok(grep_result_from_proto(response.into_inner()))
    }
}

/// The events of one `Exec` call. Dropping it drops the response stream, which cancels the call.
struct ExecEvents {
    inner: Streaming<pb::ExecResponse>,
    secrets: Secrets,
    /// The exit, or a failure, has been yielded: nothing follows.
    finished: bool,
}

impl ExecEvents {
    /// The next event, or `None` after the exit (or a failure).
    async fn next(&mut self) -> Option<Result<ExecEvent>> {
        if self.finished {
            return None;
        }
        let event = match self.inner.message().await {
            Ok(Some(response)) => exec_event_from_proto(response),
            Ok(None) => Err(Error::unavailable(
                "Exec: the plugin ended the call before the command's exit",
            )),
            Err(status) => Err(rpc_error(&self.secrets, "Exec", &status)),
        };
        self.finished = !matches!(event, Ok(ExecEvent::Stdout(_) | ExecEvent::Stderr(_)));
        Some(event)
    }
}
