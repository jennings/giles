//! The `giles.admin.v1.AdminService` RPCs.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use giles_plugin_sdk::convert::{status_to_proto, timestamp_from_proto, timestamp_to_proto};
use giles_proto::admin::v1 as pb;
use giles_proto::admin::v1::admin_service_server::AdminService as AdminRpc;
use giles_proto::types::v1 as types_pb;
use giles_store::{BlobStore, ModelUsage, SessionId, SessionKind, SessionSummary, Store};
use giles_types::{AgentId, Timestamp, UserId};
use tonic::{Request, Response, Status};
use tracing::{error, info};

use super::plugins::{PluginControl, configured, landlock_abi, plugin_instances};
use super::toolsets::{
    ToolsetAdmin, manager_status, rename_status, renamed_to_proto, toolset_to_proto,
};
use crate::backup::{self, BackupError};
use crate::config::{ChannelRuntime, Config, InstanceName, Kind, LiveConfig, ToolsetSpec};
use crate::paths::DataRoot;
use crate::sandbox::HostReport;
use crate::sessions::{ChannelStatusRecord, Clock, Sessions};
use crate::toolsets::ToolsetManagerError;

/// The latest status each channel reported since startup. Implemented by [`Sessions`], which
/// records them.
pub trait ChannelStatuses: Send + Sync {
    /// The latest status of every channel that sent one, by instance.
    fn channel_statuses(&self) -> BTreeMap<InstanceName, ChannelStatusRecord>;
}

impl ChannelStatuses for Sessions {
    fn channel_statuses(&self) -> BTreeMap<InstanceName, ChannelStatusRecord> {
        Sessions::channel_statuses(self)
    }
}

/// Creates one-time web login links (PLAN section 3.4). The built-in web channel implements it;
/// without one, `CreateLoginLink` answers `UNAVAILABLE`.
pub trait LoginLinks: Send + Sync {
    /// A new one-time login link for `user`, valid from `now`.
    ///
    /// # Errors
    ///
    /// [`LoginLinkError`], with a message for the operator.
    fn create_login_link(&self, user: &UserId, now: Timestamp)
    -> Result<LoginLink, LoginLinkError>;
}

/// A one-time web login link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginLink {
    /// The URL to open.
    pub url: String,
    /// When it stops working.
    pub expires_at: Timestamp,
}

/// Why a login link couldn't be created. Each carries the message for the operator.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LoginLinkError {
    /// There is no such user (`NOT_FOUND`).
    #[error("{0}")]
    UnknownUser(String),
    /// The user exists but can't log in to the web chat, e.g. without a `web:` identity
    /// (`FAILED_PRECONDITION`).
    #[error("{0}")]
    Refused(String),
}

#[cfg(feature = "channel-web")]
impl LoginLinks for crate::channels::web::WebChannel {
    fn create_login_link(
        &self,
        user: &UserId,
        now: Timestamp,
    ) -> Result<LoginLink, LoginLinkError> {
        use crate::channels::web::{self, WebChannel};
        match WebChannel::create_login_link(self, user, now) {
            Ok(web::LoginLink { url, expires_at }) => Ok(LoginLink { url, expires_at }),
            Err(err @ web::LoginLinkError::UnknownUser(_)) => {
                Err(LoginLinkError::UnknownUser(err.to_string()))
            }
            Err(err @ web::LoginLinkError::NoWebIdentity(_)) => {
                Err(LoginLinkError::Refused(err.to_string()))
            }
        }
    }
}

/// What the admin service reads and controls.
pub struct AdminOptions {
    /// The running configuration: the one loaded at startup, plus the changes since. Read for
    /// each request, so toolsets added, edited, and removed at runtime show up.
    pub config: LiveConfig,
    /// The data root, for `Backup`.
    pub root: DataRoot,
    /// The blob store, whose sweep `Backup` holds off.
    pub blobs: BlobStore,
    /// The database.
    pub store: Store,
    /// The startup sandbox check (PLAN section 12.1): the host's report, or why it can't sandbox.
    pub sandbox: Result<HostReport, String>,
    /// When the core started.
    pub started_at: Timestamp,
    /// The channel statuses recorded since startup.
    pub channels: Arc<dyn ChannelStatuses>,
    /// Plugin supervision, once there is one.
    pub plugins: Option<Arc<dyn PluginControl>>,
    /// The web channel's login links, if it runs.
    pub login_links: Option<Arc<dyn LoginLinks>>,
    /// The toolset manager, which every toolset RPC calls. Without it they answer `UNAVAILABLE`.
    pub toolsets: Option<Arc<dyn ToolsetAdmin>>,
    /// The current time.
    pub clock: Arc<dyn Clock>,
}

impl fmt::Debug for AdminOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdminOptions")
            .field("store", &self.store)
            .field("sandbox", &self.sandbox)
            .field("started_at", &self.started_at)
            .field("plugins", &self.plugins.is_some())
            .field("login_links", &self.login_links.is_some())
            .field("toolsets", &self.toolsets.is_some())
            .finish_non_exhaustive()
    }
}

/// The admin service's RPCs, served by [`AdminListener`](super::AdminListener).
#[derive(Debug)]
pub struct AdminService {
    options: AdminOptions,
}

impl AdminService {
    /// The service over `options`.
    pub fn new(options: AdminOptions) -> Self {
        Self { options }
    }

    fn plugins(
        &self,
        statuses: &BTreeMap<InstanceName, ChannelStatusRecord>,
    ) -> Vec<pb::PluginInstance> {
        let runtimes = self
            .options
            .plugins
            .as_ref()
            .map(|control| control.plugins())
            .unwrap_or_default();
        plugin_instances(
            &self.options.config.get(),
            &self.options.sandbox,
            statuses,
            runtimes,
        )
    }

    fn toolsets(&self) -> Result<&dyn ToolsetAdmin, Status> {
        self.options.toolsets.as_deref().ok_or_else(|| {
            Status::unavailable("toolset management is not available, so toolsets can't be changed")
        })
    }
}

/// A store failure, logged with the RPC's name; the caller gets `INTERNAL`.
fn store_error(rpc: &'static str) -> impl FnOnce(giles_store::Error) -> Status {
    move |err| {
        error!(rpc, error = %err, "admin call failed");
        Status::internal(format!("database error: {err}"))
    }
}

#[tonic::async_trait]
impl AdminRpc for AdminService {
    async fn status(
        &self,
        _request: Request<pb::StatusRequest>,
    ) -> Result<Response<pb::StatusResponse>, Status> {
        let statuses = self.options.channels.channel_statuses();
        Ok(Response::new(pb::StatusResponse {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            started_at: Some(timestamp_to_proto(self.options.started_at)),
            // v1 has no configuration reload: an invalid configuration stops startup.
            config_error: None,
            plugins: self.plugins(&statuses),
            sandbox_host: Some(sandbox_host(&self.options.sandbox)),
            channels: channel_instances(&self.options.config.get(), &statuses),
        }))
    }

    async fn list_plugins(
        &self,
        _request: Request<pb::ListPluginsRequest>,
    ) -> Result<Response<pb::ListPluginsResponse>, Status> {
        let statuses = self.options.channels.channel_statuses();
        Ok(Response::new(pb::ListPluginsResponse {
            plugins: self.plugins(&statuses),
        }))
    }

    async fn restart_plugin(
        &self,
        request: Request<pb::RestartPluginRequest>,
    ) -> Result<Response<pb::RestartPluginResponse>, Status> {
        let request = request.into_inner();
        let kind = match request.kind() {
            pb::InstanceKind::Provider => Kind::Provider,
            pb::InstanceKind::Channel => Kind::Channel,
            pb::InstanceKind::Toolset => Kind::Toolset,
            pb::InstanceKind::Unspecified => {
                return Err(Status::invalid_argument(
                    "kind: name the kind of the plugin instance (channel or toolset)",
                ));
            }
        };
        let instance = request.instance;
        // `ListPlugins` lists the configured instances, and a stored toolset that failed to start,
        // which the configuration doesn't have but supervision reports.
        let listed = configured(&self.options.config.get())
            .iter()
            .any(|c| c.kind == kind && c.instance.as_str() == instance)
            || self.options.plugins.as_ref().is_some_and(|control| {
                control
                    .plugins()
                    .iter()
                    .any(|runtime| runtime.kind == kind && runtime.instance.as_str() == instance)
            });
        if !listed {
            return Err(Status::not_found(format!(
                "there is no {kind} plugin instance `{instance}`"
            )));
        }
        let Some(control) = &self.options.plugins else {
            return Err(Status::unavailable(
                "plugin supervision is not available, so plugins can't be restarted",
            ));
        };
        if !control.restart(kind, &instance) {
            return Err(Status::failed_precondition(format!(
                "{kind} plugin instance `{instance}` isn't supervised"
            )));
        }
        info!(
            %kind,
            instance,
            "plugin restart requested through the admin service"
        );
        Ok(Response::new(pb::RestartPluginResponse {}))
    }

    async fn list_sessions(
        &self,
        request: Request<pb::ListSessionsRequest>,
    ) -> Result<Response<pb::ListSessionsResponse>, Status> {
        let request = request.into_inner();
        let agent = request
            .agent
            .map(AgentId::new)
            .transpose()
            .map_err(|err| Status::invalid_argument(format!("agent: {err}")))?;
        let sessions = self
            .options
            .store
            .list_sessions(agent, request.include_ended)
            .await
            .map_err(store_error("ListSessions"))?;
        Ok(Response::new(pb::ListSessionsResponse {
            sessions: sessions.into_iter().map(session_to_proto).collect(),
        }))
    }

    async fn reset_session(
        &self,
        request: Request<pb::ResetSessionRequest>,
    ) -> Result<Response<pb::ResetSessionResponse>, Status> {
        let text = request.into_inner().session_id;
        let id = text.trim().parse().map(SessionId::new).map_err(|_| {
            Status::invalid_argument(format!("session_id: {text:?} is not a session id"))
        })?;
        let store = &self.options.store;
        if store
            .end_session(id, self.options.clock.now())
            .await
            .map_err(store_error("ResetSession"))?
        {
            info!(session = %id, "session ended through the admin service");
            return Ok(Response::new(pb::ResetSessionResponse {}));
        }
        match store
            .session(id)
            .await
            .map_err(store_error("ResetSession"))?
        {
            None => Err(Status::not_found(format!("there is no session {id}"))),
            Some(_) => Err(Status::failed_precondition(format!(
                "session {id} has already ended"
            ))),
        }
    }

    async fn create_login_link(
        &self,
        request: Request<pb::CreateLoginLinkRequest>,
    ) -> Result<Response<pb::CreateLoginLinkResponse>, Status> {
        let user = UserId::new(request.into_inner().user)
            .map_err(|err| Status::invalid_argument(format!("user: {err}")))?;
        let Some(links) = &self.options.login_links else {
            return Err(Status::unavailable(
                "login links need the built-in web channel, which isn't running",
            ));
        };
        match links.create_login_link(&user, self.options.clock.now()) {
            Ok(link) => {
                info!(%user, "login link created through the admin service");
                Ok(Response::new(pb::CreateLoginLinkResponse {
                    url: link.url,
                    expires_at: Some(timestamp_to_proto(link.expires_at)),
                }))
            }
            Err(LoginLinkError::UnknownUser(message)) => Err(Status::not_found(message)),
            Err(LoginLinkError::Refused(message)) => Err(Status::failed_precondition(message)),
        }
    }

    async fn usage_report(
        &self,
        request: Request<pb::UsageReportRequest>,
    ) -> Result<Response<pb::UsageReportResponse>, Status> {
        let request = request.into_inner();
        let time = |field: &str, value: Option<prost_types::Timestamp>| {
            value
                .map(timestamp_from_proto)
                .transpose()
                .map_err(|err| Status::invalid_argument(format!("{field}: {err}")))
        };
        let since = time("since", request.since)?;
        let until = time("until", request.until)?;
        let rows = self
            .options
            .store
            .usage_by_model(since, until)
            .await
            .map_err(store_error("UsageReport"))?;
        Ok(Response::new(pb::UsageReportResponse {
            rows: rows.into_iter().map(usage_row).collect(),
        }))
    }

    async fn backup(
        &self,
        request: Request<pb::BackupRequest>,
    ) -> Result<Response<pb::BackupResponse>, Status> {
        let dir = PathBuf::from(request.into_inner().dir);
        if !dir.is_absolute() {
            return Err(Status::invalid_argument(
                "dir must be an absolute path to an existing directory",
            ));
        }
        match tokio::fs::metadata(&dir).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(Status::invalid_argument(format!(
                    "{} is not a directory",
                    dir.display()
                )));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Status::not_found(format!(
                    "{} does not exist; create it first",
                    dir.display()
                )));
            }
            Err(err) => {
                return Err(Status::failed_precondition(format!(
                    "cannot read {}: {err}",
                    dir.display()
                )));
            }
        }
        let snapshot = backup::create(
            &self.options.root,
            &self.options.store,
            &self.options.blobs,
            &dir,
            self.options.clock.now(),
        )
        .await
        .map_err(|err| {
            error!(rpc = "Backup", error = %err, dir = %dir.display(), "admin call failed");
            match err {
                BackupError::Exists(_) => Status::already_exists(err.to_string()),
                err => Status::internal(format!("backup failed: {err}")),
            }
        })?;
        info!(
            database = %snapshot.database.display(),
            archive = %snapshot.archive.display(),
            "wrote a backup",
        );
        let path = |path: PathBuf| path.to_string_lossy().into_owned();
        Ok(Response::new(pb::BackupResponse {
            paths: vec![path(snapshot.database), path(snapshot.archive)],
        }))
    }

    async fn list_toolsets(
        &self,
        _request: Request<pb::ListToolsetsRequest>,
    ) -> Result<Response<pb::ListToolsetsResponse>, Status> {
        let toolsets = self.toolsets()?;
        let views = toolsets
            .list()
            .await
            .map_err(|err| manager_status("ListToolsets", "", err))?;
        Ok(Response::new(pb::ListToolsetsResponse {
            toolsets: views.into_iter().map(toolset_to_proto).collect(),
        }))
    }

    async fn get_toolset(
        &self,
        request: Request<pb::GetToolsetRequest>,
    ) -> Result<Response<pb::GetToolsetResponse>, Status> {
        let name = request.into_inner().name;
        let view = self
            .toolsets()?
            .get(&name)
            .await
            .map_err(|err| manager_status("GetToolset", &name, err))?;
        Ok(Response::new(pb::GetToolsetResponse {
            toolset: Some(toolset_to_proto(view)),
        }))
    }

    async fn set_toolset(
        &self,
        request: Request<pb::SetToolsetRequest>,
    ) -> Result<Response<pb::SetToolsetResponse>, Status> {
        let request = request.into_inner();
        let name = request.name;
        let expected = request
            .expected_updated_at
            .map(timestamp_from_proto)
            .transpose()
            .map_err(|err| Status::invalid_argument(format!("expected_updated_at: {err}")))?;
        let toolsets = self.toolsets()?;
        let spec = ToolsetSpec::from_toml(&request.spec_toml)
            .map_err(|err| Status::invalid_argument(err.to_string()))?;
        // A stored name is replaced (refused if it changed since `expected`). Any other name is
        // created, unless the caller expected to find the toolset.
        let view = match toolsets.update(&name, &spec, expected).await {
            Err(ToolsetManagerError::NotFound) if expected.is_none() => {
                toolsets.add(&name, &spec).await
            }
            result => result,
        }
        .map_err(|err| manager_status("SetToolset", &name, err))?;
        info!(toolset = %view.name, "toolset set through the admin service");
        Ok(Response::new(pb::SetToolsetResponse {
            toolset: Some(toolset_to_proto(view)),
        }))
    }

    async fn remove_toolset(
        &self,
        request: Request<pb::RemoveToolsetRequest>,
    ) -> Result<Response<pb::RemoveToolsetResponse>, Status> {
        let name = request.into_inner().name;
        let removed = self
            .toolsets()?
            .remove(&name)
            .await
            .map_err(|err| manager_status("RemoveToolset", &name, err))?;
        info!(toolset = %removed.name, "toolset removed through the admin service");
        Ok(Response::new(pb::RemoveToolsetResponse {
            name: removed.name,
        }))
    }

    async fn rename_toolset(
        &self,
        request: Request<pb::RenameToolsetRequest>,
    ) -> Result<Response<pb::RenameToolsetResponse>, Status> {
        let request = request.into_inner();
        let renamed = self
            .toolsets()?
            .rename(&request.old, &request.new)
            .await
            .map_err(|err| rename_status(&request.old, &request.new, err))?;
        info!(
            from = %request.old,
            to = %renamed.view.name,
            "toolset renamed through the admin service"
        );
        Ok(Response::new(renamed_to_proto(renamed)))
    }
}

/// The host's sandbox, for `Status`.
fn sandbox_host(host: &Result<HostReport, String>) -> pb::SandboxHost {
    match host {
        Ok(report) => pb::SandboxHost {
            available: true,
            description: report.to_string(),
            landlock_abi: landlock_abi(report),
        },
        Err(error) => pb::SandboxHost {
            available: false,
            description: error.clone(),
            landlock_abi: None,
        },
    }
}

/// Every configured channel with its latest status, sorted by name.
fn channel_instances(
    config: &Config,
    statuses: &BTreeMap<InstanceName, ChannelStatusRecord>,
) -> Vec<pb::ChannelInstance> {
    config
        .channels
        .values()
        .map(|channel| {
            let runtime = match channel.runtime {
                ChannelRuntime::BuiltIn => pb::ChannelRuntime::BuiltIn,
                ChannelRuntime::Plugin(_) => pb::ChannelRuntime::Plugin,
                ChannelRuntime::Remote(_) => pb::ChannelRuntime::Remote,
            };
            let record = statuses.get(&channel.name);
            pb::ChannelInstance {
                instance: channel.name.to_string(),
                component: channel.component.clone(),
                runtime: runtime.into(),
                status: record.and_then(|r| status_to_proto(r.status.clone()).ok()),
                status_at: record.map(|r| timestamp_to_proto(r.at)),
            }
        })
        .collect()
}

fn session_to_proto(summary: SessionSummary) -> pb::Session {
    let s = summary.session;
    let kind = match s.kind {
        SessionKind::Direct => types_pb::ConversationKind::Direct,
        SessionKind::Group => types_pb::ConversationKind::Group,
        _ => types_pb::ConversationKind::Unspecified,
    };
    pb::Session {
        id: s.id.to_string(),
        agent: s.agent.to_string(),
        instance: s.channel,
        conversation: s.conversation.map(|id| types_pb::ConversationRef {
            id: id.to_string(),
            kind: kind.into(),
            title: None,
        }),
        started_at: Some(timestamp_to_proto(s.started_at)),
        ended_at: s.ended_at.map(timestamp_to_proto),
        last_message_at: s.last_message_at.map(timestamp_to_proto),
        message_count: summary.messages,
    }
}

fn usage_row(row: ModelUsage) -> pb::UsageRow {
    let t = row.tokens;
    pb::UsageRow {
        agent: row.agent.to_string(),
        provider: row.provider,
        model: row.model,
        requests: row.requests,
        fallback_requests: row.fallbacks,
        usage: Some(types_pb::Usage {
            input_tokens: t.input_tokens,
            output_tokens: t.output_tokens,
            cache_read_tokens: t.cache_read_tokens,
            cache_write_tokens: t.cache_write_tokens,
        }),
    }
}
