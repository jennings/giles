//! The admin service over a real socket in a temporary data root, with fake subsystems.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use giles_plugin_sdk::convert::timestamp_to_proto;
use giles_proto::admin::v1 as pb;
use giles_proto::admin::v1::admin_service_client::AdminServiceClient;
use giles_proto::channel::v1 as channel_pb;
use giles_proto::plugin::v1::SandboxGrants;
use giles_store::{BlobStore, JobKey, SessionKey, SessionKind, Store, UsageRecord};
use giles_types::{AgentId, ChannelStatus, ConversationId, Timestamp, Usage, UserId};
use hyper_util::rt::TokioIo;
use tempfile::TempDir;
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::Code;
use tonic::transport::{Channel, Endpoint, Uri};

use super::*;
use crate::config::test_toolsets::add_toolsets;
use crate::config::{Config, InstanceName, KeyPath, Kind, LiveConfig, LoadOptions, ToolsetSpec};
use crate::paths::DataRoot;
use crate::sandbox::HostReport;
use crate::sessions::{ChannelStatusRecord, Clock};
use crate::toolsets::{
    AgentGlobChange, GlobChange, Removed, ToolsetManagerError, ToolsetRenamed, ToolsetSource,
    ToolsetView,
};

const CONFIG: &str = r#"
[owner]
identities = ["web:owner"]

[channel.web]

[channel.libera]
component = "irc"
sandbox = { network = ["tcp:6697"] }

[channel.mail]
component = "email"
address = "https://mail.example.com:7443"

[toolset.files]
command = "/bin/echo"
unsandboxed = true
"#;

/// The remote toolset of the running configuration, stored in the database in a real core.
const GITHUB: (&str, &str) = ("github", "url = \"https://api.example.com/mcp/\"\n");

fn at(secs: i64) -> Timestamp {
    Timestamp::from_second(1_800_000_000 + secs).unwrap()
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        at(1000)
    }
}

#[derive(Default)]
struct FakeStatuses(Mutex<BTreeMap<InstanceName, ChannelStatusRecord>>);

impl FakeStatuses {
    fn set(&self, instance: &str, status: ChannelStatus, at: Timestamp) {
        self.0.lock().unwrap().insert(
            InstanceName::new(instance).unwrap(),
            ChannelStatusRecord { status, at },
        );
    }
}

impl ChannelStatuses for FakeStatuses {
    fn channel_statuses(&self) -> BTreeMap<InstanceName, ChannelStatusRecord> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct FakeControl {
    plugins: Vec<PluginRuntime>,
    /// The names restarted, in order.
    restarted: Mutex<Vec<String>>,
}

impl PluginControl for FakeControl {
    fn plugins(&self) -> Vec<PluginRuntime> {
        self.plugins.clone()
    }

    fn restart(&self, kind: Kind, instance: &str) -> bool {
        let supervised = self
            .plugins
            .iter()
            .any(|p| p.kind == kind && p.instance.as_str() == instance);
        if supervised {
            self.restarted.lock().unwrap().push(instance.to_owned());
        }
        supervised
    }
}

struct FakeLinks;

impl LoginLinks for FakeLinks {
    fn create_login_link(
        &self,
        user: &UserId,
        now: Timestamp,
    ) -> Result<LoginLink, LoginLinkError> {
        match user.as_str() {
            "owner" => Ok(LoginLink {
                url: "http://127.0.0.1:8080/login/abc".into(),
                expires_at: now + jiff::SignedDuration::from_mins(15),
            }),
            "guest" => Err(LoginLinkError::Refused("no web identity".into())),
            other => Err(LoginLinkError::UnknownUser(format!("no user {other}"))),
        }
    }
}

/// A toolset manager over a map, with the answers of the real one that the RPCs depend on:
/// names match ignoring case, a `giles.toml` toolset can't be changed, an update is refused as
/// stale when it expects another `updated_at`, and a spec without `url` is refused.
#[derive(Default)]
struct FakeToolsets {
    state: Mutex<FakeToolsetState>,
}

#[derive(Default)]
struct FakeToolsetState {
    /// By name: the spec and when it changed, in seconds after [`at`]`(0)`.
    stored: BTreeMap<String, (ToolsetSpec, i64)>,
    /// The toolsets of `giles.toml`.
    file: Vec<String>,
    /// The seconds of the last change.
    clock: i64,
    /// Another call creates the name between `update` and `add`: `update` finds nothing and
    /// `add` finds the name taken.
    racing: bool,
    /// The database fails.
    broken: bool,
}

const FILES_SPEC: &str = r#"
url = "https://files.example.com/mcp"
deadline = "90s"
skills = ["/skills/files"]

[auth]
bearer = { secret = "files" }
"#;

fn spec(text: &str) -> ToolsetSpec {
    ToolsetSpec::from_toml(text).unwrap()
}

impl FakeToolsets {
    fn new(stored: &[(&str, &str)], file: &[&str]) -> Arc<Self> {
        let fake = Self::default();
        {
            let mut state = fake.state.lock().unwrap();
            for (name, text) in stored {
                state.clock += 1;
                let clock = state.clock;
                state.stored.insert((*name).to_owned(), (spec(text), clock));
            }
            state.file = file.iter().map(|name| (*name).to_owned()).collect();
        }
        Arc::new(fake)
    }

    fn with<R>(&self, f: impl FnOnce(&mut FakeToolsetState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    /// The stored spec of `name`, and when it changed.
    fn stored(&self, name: &str) -> Option<(ToolsetSpec, i64)> {
        self.with(|state| state.stored.get(name).cloned())
    }
}

impl FakeToolsetState {
    fn check(&self) -> Result<(), ToolsetManagerError> {
        if self.broken {
            return Err(ToolsetManagerError::Store(giles_store::Error::Io(
                std::io::Error::other("disk on fire"),
            )));
        }
        Ok(())
    }

    fn stored_name(&self, name: &str) -> Option<String> {
        self.stored
            .keys()
            .find(|other| other.eq_ignore_ascii_case(name))
            .cloned()
    }

    fn is_file(&self, name: &str) -> bool {
        self.file
            .iter()
            .any(|other| other.eq_ignore_ascii_case(name))
    }

    fn view(&self, name: &str) -> ToolsetView {
        match self.stored.get(name) {
            Some((spec, changed)) => ToolsetView {
                name: name.to_owned(),
                source: ToolsetSource::Stored {
                    created_at: at(0),
                    updated_at: at(*changed),
                },
                spec: Some(spec.clone()),
                failure: (name == "broken").then(|| "no secret \"broken\"".to_owned()),
            },
            None => ToolsetView {
                name: name.to_owned(),
                source: ToolsetSource::File,
                spec: None,
                failure: None,
            },
        }
    }

    fn refuse_stdio(spec: &ToolsetSpec) -> Result<(), ToolsetManagerError> {
        if spec.url.is_none() {
            return Err(ToolsetManagerError::Invalid {
                key: KeyPath::root().key("url"),
                reason: "a stored toolset is a remote server: set `url`".to_owned(),
            });
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl ToolsetAdmin for FakeToolsets {
    async fn list(&self) -> Result<Vec<ToolsetView>, ToolsetManagerError> {
        self.with(|state| {
            state.check()?;
            let mut names: Vec<&String> = state.stored.keys().chain(&state.file).collect();
            names.sort_by_key(|name| name.to_ascii_lowercase());
            Ok(names.into_iter().map(|name| state.view(name)).collect())
        })
    }

    async fn get(&self, name: &str) -> Result<ToolsetView, ToolsetManagerError> {
        self.with(|state| {
            state.check()?;
            let own = state.stored_name(name).or_else(|| {
                state
                    .file
                    .iter()
                    .find(|other| other.eq_ignore_ascii_case(name))
                    .cloned()
            });
            own.map(|own| state.view(&own))
                .ok_or(ToolsetManagerError::NotFound)
        })
    }

    async fn add(
        &self,
        name: &str,
        spec: &ToolsetSpec,
    ) -> Result<ToolsetView, ToolsetManagerError> {
        self.with(|state| {
            state.check()?;
            if state.racing || state.stored_name(name).is_some() {
                return Err(ToolsetManagerError::Exists);
            }
            // A name that a provider or channel of giles.toml has.
            if name == "libera" {
                return Err(ToolsetManagerError::Invalid {
                    key: KeyPath::root(),
                    reason: "the name is taken by `[channel.libera]` in giles.toml".to_owned(),
                });
            }
            FakeToolsetState::refuse_stdio(spec)?;
            state.clock += 1;
            let clock = state.clock;
            state.stored.insert(name.to_owned(), (spec.clone(), clock));
            Ok(state.view(name))
        })
    }

    async fn update(
        &self,
        name: &str,
        spec: &ToolsetSpec,
        expected_updated_at: Option<Timestamp>,
    ) -> Result<ToolsetView, ToolsetManagerError> {
        self.with(|state| {
            state.check()?;
            let own = match state.stored_name(name) {
                Some(own) if !state.racing => own,
                _ if state.is_file(name) => return Err(ToolsetManagerError::Unsupported),
                _ => return Err(ToolsetManagerError::NotFound),
            };
            if expected_updated_at.is_some_and(|expected| expected != at(state.stored[&own].1)) {
                return Err(ToolsetManagerError::Stale);
            }
            FakeToolsetState::refuse_stdio(spec)?;
            state.clock += 1;
            let clock = state.clock;
            state.stored.insert(own.clone(), (spec.clone(), clock));
            Ok(state.view(&own))
        })
    }

    async fn remove(&self, name: &str) -> Result<Removed, ToolsetManagerError> {
        self.with(|state| {
            state.check()?;
            let Some(own) = state.stored_name(name) else {
                return Err(if state.is_file(name) {
                    ToolsetManagerError::Unsupported
                } else {
                    ToolsetManagerError::NotFound
                });
            };
            state.stored.remove(&own);
            Ok(Removed { name: own })
        })
    }

    /// The checks of the real one that the RPC maps, by name: `libera` is taken by a channel of
    /// giles.toml, `globby` is refused for an agent's glob, `stuck` fails a step that can't be
    /// undone. Renaming `files` rewrites two globs of the agent `main`.
    async fn rename(&self, old: &str, new: &str) -> Result<ToolsetRenamed, ToolsetManagerError> {
        self.with(|state| {
            state.check()?;
            let Some(own) = state.stored_name(old) else {
                return Err(if state.is_file(old) {
                    ToolsetManagerError::Unsupported
                } else {
                    ToolsetManagerError::NotFound
                });
            };
            let invalid = |reason: &str| ToolsetManagerError::Invalid {
                key: KeyPath::root(),
                reason: reason.to_owned(),
            };
            if own == new {
                return Err(invalid("the new name is the same as the old one"));
            }
            if InstanceName::new(new).is_err() {
                return Err(invalid("the name can only have letters, digits, and `-`"));
            }
            if new == "libera" {
                return Err(invalid(
                    "the name is taken by `[channel.libera]` in giles.toml",
                ));
            }
            if state.stored_name(new).is_some_and(|other| other != own) {
                return Err(ToolsetManagerError::Exists);
            }
            if new == "globby" {
                return Err(ToolsetManagerError::Refused {
                    reason: "agent `main` has the glob `fi*` in tools, which matches the tools of \
                             `files` without starting with `files__`"
                        .to_owned(),
                });
            }
            if new == "stuck" {
                return Err(ToolsetManagerError::Failed {
                    step: "rewriting /agents/main/agent.toml".to_owned(),
                    error: "permission denied".to_owned(),
                    undo_errors: vec!["put the toolset's row back: busy".to_owned()],
                });
            }
            let (spec, _) = state.stored.remove(&own).unwrap();
            state.clock += 1;
            let clock = state.clock;
            state.stored.insert(new.to_owned(), (spec, clock));
            let agents = if own == "files" {
                let glob = |key: &'static str, suffix: &str| GlobChange {
                    key,
                    from: format!("files__{suffix}"),
                    to: format!("{new}__{suffix}"),
                };
                vec![AgentGlobChange {
                    agent: "main".to_owned(),
                    globs: vec![glob("tools", "*"), glob("ask", "delete")],
                }]
            } else {
                Vec::new()
            };
            Ok(ToolsetRenamed {
                from: own,
                view: state.view(new),
                agents,
            })
        })
    }
}

/// A running admin service in a temporary data root.
struct Harness {
    _dir: TempDir,
    root: DataRoot,
    store: Store,
    statuses: Arc<FakeStatuses>,
    shutdown: CancellationToken,
    server: JoinHandle<Result<(), tonic::transport::Error>>,
    _lock: InstanceLock,
}

/// What to plug into the service.
#[derive(Default)]
struct Fakes {
    control: Option<Arc<dyn PluginControl>>,
    toolsets: Option<Arc<dyn ToolsetAdmin>>,
    links: bool,
    sandbox: Option<Result<HostReport, String>>,
}

impl Harness {
    async fn start(fakes: Fakes) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        std::fs::write(root.config_file(), CONFIG).unwrap();
        let mut config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        add_toolsets(&root, &mut config, [GITHUB]);
        let store = Store::open(root.database_file()).await.unwrap();
        let blobs = BlobStore::open(root.blobs_dir(), store.clone())
            .await
            .unwrap();
        let statuses = Arc::new(FakeStatuses::default());
        let lock = InstanceLock::acquire(&root).unwrap();
        let listener = AdminListener::bind(&root, &lock).unwrap();
        let service = Arc::new(AdminService::new(AdminOptions {
            config: LiveConfig::new(Arc::new(config)),
            root: root.clone(),
            blobs,
            store: store.clone(),
            sandbox: fakes
                .sandbox
                .unwrap_or(Ok(HostReport::Linux { landlock_abi: 5 })),
            started_at: at(0),
            channels: statuses.clone(),
            plugins: fakes.control,
            login_links: fakes
                .links
                .then(|| Arc::new(FakeLinks) as Arc<dyn LoginLinks>),
            toolsets: fakes.toolsets,
            clock: Arc::new(FixedClock),
        }));
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(listener.serve(service, shutdown.clone()));
        Self {
            _dir: dir,
            root,
            store,
            statuses,
            shutdown,
            server,
            _lock: lock,
        }
    }

    async fn client(&self) -> AdminClient {
        connect(&self.root).await.unwrap()
    }

    /// A connection without the token interceptor.
    async fn raw_channel(&self) -> Channel {
        let mut stream = Some(UnixStream::connect(self.root.admin_socket()).await.unwrap());
        Endpoint::from_static("http://admin")
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let stream = stream.take().unwrap();
                async move { Ok::<_, std::io::Error>(TokioIo::new(stream)) }
            }))
            .await
            .unwrap()
    }

    /// Stops the server and releases the lock; the data root stays until the returned directory
    /// is dropped.
    async fn stop(self) -> (TempDir, DataRoot) {
        self.shutdown.cancel();
        self.server.await.unwrap().unwrap();
        (self._dir, self.root)
    }
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Calls every RPC on `client`, returning each one's status code.
async fn every_rpc<T>(client: &mut AdminServiceClient<T>) -> Vec<(&'static str, Code)>
where
    T: tonic::client::GrpcService<tonic::body::Body>,
    T::Error: Into<tonic::codegen::StdError>,
    T::ResponseBody: tonic::codegen::Body<Data = bytes::Bytes> + Send + 'static,
    <T::ResponseBody as tonic::codegen::Body>::Error: Into<tonic::codegen::StdError> + Send,
{
    fn code<R>(result: Result<R, tonic::Status>) -> Code {
        result.map_or_else(|status| status.code(), |_| Code::Ok)
    }
    vec![
        ("Status", code(client.status(pb::StatusRequest {}).await)),
        (
            "ListPlugins",
            code(client.list_plugins(pb::ListPluginsRequest {}).await),
        ),
        (
            "RestartPlugin",
            code(
                client
                    .restart_plugin(pb::RestartPluginRequest {
                        instance: "libera".into(),
                        kind: pb::InstanceKind::Channel.into(),
                    })
                    .await,
            ),
        ),
        (
            "ListSessions",
            code(
                client
                    .list_sessions(pb::ListSessionsRequest::default())
                    .await,
            ),
        ),
        (
            "ResetSession",
            code(
                client
                    .reset_session(pb::ResetSessionRequest {
                        session_id: "1".into(),
                    })
                    .await,
            ),
        ),
        (
            "CreateLoginLink",
            code(
                client
                    .create_login_link(pb::CreateLoginLinkRequest {
                        user: "owner".into(),
                    })
                    .await,
            ),
        ),
        (
            "UsageReport",
            code(client.usage_report(pb::UsageReportRequest::default()).await),
        ),
        (
            "Backup",
            code(
                client
                    // Rejected before it touches anything.
                    .backup(pb::BackupRequest {
                        dir: "relative".into(),
                    })
                    .await,
            ),
        ),
        (
            "ListToolsets",
            code(client.list_toolsets(pb::ListToolsetsRequest {}).await),
        ),
        (
            "GetToolset",
            code(
                client
                    .get_toolset(pb::GetToolsetRequest {
                        name: "files".into(),
                    })
                    .await,
            ),
        ),
        (
            "SetToolset",
            code(
                client
                    .set_toolset(pb::SetToolsetRequest {
                        name: "files".into(),
                        spec_toml: FILES_SPEC.into(),
                        expected_updated_at: None,
                    })
                    .await,
            ),
        ),
        (
            "RemoveToolset",
            code(
                client
                    .remove_toolset(pb::RemoveToolsetRequest {
                        name: "files".into(),
                    })
                    .await,
            ),
        ),
        (
            "RenameToolset",
            code(
                client
                    .rename_toolset(pb::RenameToolsetRequest {
                        old: "files".into(),
                        new: "docs".into(),
                    })
                    .await,
            ),
        ),
    ]
}

#[tokio::test]
async fn every_rpc_requires_the_token() {
    let h = Harness::start(Fakes::default()).await;

    let mut anonymous = AdminServiceClient::new(h.raw_channel().await);
    let mut wrong = AdminServiceClient::with_interceptor(
        h.raw_channel().await,
        BearerToken::new(&"0".repeat(64)).unwrap(),
    );
    for (rpc, code) in every_rpc(&mut anonymous)
        .await
        .into_iter()
        .chain(every_rpc(&mut wrong).await)
    {
        assert_eq!(code, Code::Unauthenticated, "{rpc}");
    }

    // With the token every RPC gets through to its handler.
    for (rpc, code) in every_rpc(&mut h.client().await).await {
        assert_ne!(code, Code::Unauthenticated, "{rpc}");
    }
    h.stop().await;
}

#[tokio::test]
async fn the_socket_and_token_are_private_fresh_and_removed_at_shutdown() {
    let h = Harness::start(Fakes::default()).await;
    let socket = h.root.admin_socket();
    let token_file = h.root.admin_token_file();
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(&token_file), 0o600);
    let first = std::fs::read_to_string(&token_file).unwrap();
    assert_eq!(first.len(), 64);

    let (_dir, root) = h.stop().await;
    assert!(!socket.exists() && !token_file.exists());
    assert!(matches!(
        connect(&root).await,
        Err(ConnectError::NotRunning { .. })
    ));

    // A socket left behind by a crash is replaced, and the token changes at every start.
    let lock = InstanceLock::acquire(&root).unwrap();
    std::os::unix::net::UnixListener::bind(&socket).unwrap();
    assert!(matches!(
        connect(&root).await,
        Err(ConnectError::NotRunning { .. })
    ));
    let listener = AdminListener::bind(&root, &lock).unwrap();
    assert_eq!(listener.socket(), socket);
    let second = std::fs::read_to_string(&token_file).unwrap();
    assert_ne!(first, second);
}

#[tokio::test]
async fn status_reports_the_sandbox_channels_and_plugins() {
    let control = FakeControl {
        plugins: vec![PluginRuntime {
            kind: Kind::Channel,
            instance: InstanceName::new("libera").unwrap(),
            binary: Some("/opt/giles/plugins/giles-channel-irc".into()),
            state: PluginRunState::Running {
                pid: Some(4242),
                since: at(3),
            },
            restarts: 2,
            last_error: Some("health check timed out".into()),
            version: Some("1.2.3".into()),
            requested_grants: Some(SandboxGrants {
                network_ports: vec!["tcp:6697".into()],
                ..SandboxGrants::default()
            }),
            tool_count: None,
        }],
        ..FakeControl::default()
    };
    let h = Harness::start(Fakes {
        control: Some(Arc::new(control)),
        ..Fakes::default()
    })
    .await;
    h.statuses.set("web", ChannelStatus::Connected, at(1));
    h.statuses.set(
        "libera",
        ChannelStatus::Error {
            message: "netsplit".into(),
        },
        at(2),
    );

    let status = h
        .client()
        .await
        .status(pb::StatusRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(status.started_at.unwrap().seconds, at(0).as_second());
    assert_eq!(status.config_error, None);
    let host = status.sandbox_host.unwrap();
    assert!(host.available);
    assert_eq!(host.landlock_abi, Some(5));
    assert_eq!(host.description, "Landlock ABI 5 and seccomp filters");

    // Every configured channel, built-in ones included, sorted by name.
    let channels: Vec<_> = status
        .channels
        .iter()
        .map(|c| (c.instance.as_str(), c.component.as_str(), c.runtime()))
        .collect();
    assert_eq!(
        channels,
        [
            ("libera", "irc", pb::ChannelRuntime::Plugin),
            ("mail", "email", pb::ChannelRuntime::Remote),
            ("web", "web", pb::ChannelRuntime::BuiltIn),
        ]
    );
    let web = &status.channels[2];
    assert!(matches!(
        web.status.as_ref().unwrap().state,
        Some(channel_pb::channel_status::State::Connected(_))
    ));
    assert_eq!(web.status_at.unwrap().seconds, at(1).as_second());
    assert!(status.channels[1].status.is_none(), "mail reported nothing");

    // Channels, then toolsets; built-in channels aren't plugins.
    let plugins: Vec<_> = status
        .plugins
        .iter()
        .map(|p| (p.instance.as_str(), p.kind(), p.component.as_str()))
        .collect();
    assert_eq!(
        plugins,
        [
            ("libera", pb::InstanceKind::Channel, "irc"),
            ("mail", pb::InstanceKind::Channel, "email"),
            ("files", pb::InstanceKind::Toolset, ""),
            ("github", pb::InstanceKind::Toolset, ""),
        ]
    );
    let libera = &status.plugins[0];
    assert_eq!(libera.state(), pb::PluginState::Running);
    assert_eq!(libera.location, "/opt/giles/plugins/giles-channel-irc");
    assert_eq!(libera.pid, Some(4242));
    assert_eq!(libera.started_at.unwrap().seconds, at(3).as_second());
    assert_eq!(libera.restarts, 2);
    assert_eq!(libera.version.as_deref(), Some("1.2.3"));
    assert_eq!(libera.last_error.as_deref(), Some("health check timed out"));
    assert_eq!(
        libera.requested_grants.as_ref().unwrap().network_ports,
        ["tcp:6697"]
    );
    assert!(matches!(
        &libera.channel_status.as_ref().unwrap().state,
        Some(channel_pb::channel_status::State::Error(m)) if m == "netsplit"
    ));
    // ABI 5 with a port list: only the ABI 6 scoping is missing.
    let sandbox = libera.sandbox.as_ref().unwrap();
    assert!(sandbox.enabled);
    assert_eq!(sandbox.landlock_abi, Some(5));
    assert_eq!(sandbox.missing.len(), 1, "{:?}", sandbox.missing);

    // Not supervised: configuration only.
    let mail = &status.plugins[1];
    assert_eq!(mail.state(), pb::PluginState::Unspecified);
    assert_eq!(mail.location, "https://mail.example.com:7443/");
    assert!(mail.sandbox.is_none(), "remote");
    let files = &status.plugins[2];
    assert_eq!(files.location, "/bin/echo");
    assert!(!files.sandbox.as_ref().unwrap().enabled, "unsandboxed");

    // ListPlugins lists the same.
    let list = h
        .client()
        .await
        .list_plugins(pb::ListPluginsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.plugins, status.plugins);
    h.stop().await;
}

#[tokio::test]
async fn a_host_without_a_sandbox_is_reported() {
    let h = Harness::start(Fakes {
        sandbox: Some(Err("Landlock is not available".into())),
        ..Fakes::default()
    })
    .await;
    let status = h
        .client()
        .await
        .status(pb::StatusRequest {})
        .await
        .unwrap()
        .into_inner();
    let host = status.sandbox_host.unwrap();
    assert!(!host.available);
    assert_eq!(host.description, "Landlock is not available");
    let libera = status.plugins[0].sandbox.as_ref().unwrap();
    assert!(libera.enabled);
    assert!(libera.missing[0].contains("Landlock is not available"));
    h.stop().await;
}

async fn restart(h: &Harness, kind: pb::InstanceKind, instance: &str) -> Result<(), Code> {
    restart_as(h, kind.into(), instance).await
}

/// A restart with a `kind` that may be a number the protocol doesn't name.
async fn restart_as(h: &Harness, kind: i32, instance: &str) -> Result<(), Code> {
    h.client()
        .await
        .restart_plugin(pb::RestartPluginRequest {
            instance: instance.into(),
            kind,
        })
        .await
        .map(drop)
        .map_err(|s| s.code())
}

/// A supervised instance of `kind` in a failed state.
fn failed(kind: Kind, instance: &str) -> PluginRuntime {
    PluginRuntime {
        kind,
        instance: InstanceName::new(instance).unwrap(),
        binary: None,
        state: PluginRunState::Failed,
        restarts: 0,
        last_error: None,
        version: None,
        requested_grants: None,
        tool_count: None,
    }
}

#[tokio::test]
async fn restart_plugin_needs_a_configured_supervised_plugin() {
    // Without supervision.
    let h = Harness::start(Fakes::default()).await;
    assert_eq!(
        restart(&h, pb::InstanceKind::Channel, "libera").await,
        Err(Code::Unavailable)
    );
    assert_eq!(
        restart(&h, pb::InstanceKind::Channel, "web").await,
        Err(Code::NotFound),
        "built in"
    );
    assert_eq!(
        restart(&h, pb::InstanceKind::Channel, "nope").await,
        Err(Code::NotFound)
    );
    h.stop().await;

    let control = Arc::new(FakeControl {
        plugins: vec![failed(Kind::Channel, "libera")],
        ..FakeControl::default()
    });
    let h = Harness::start(Fakes {
        control: Some(control.clone()),
        ..Fakes::default()
    })
    .await;
    assert_eq!(
        restart(&h, pb::InstanceKind::Channel, "libera").await,
        Ok(())
    );
    assert_eq!(*control.restarted.lock().unwrap(), ["libera"]);
    assert_eq!(
        restart(&h, pb::InstanceKind::Toolset, "files").await,
        Err(Code::FailedPrecondition)
    );
    h.stop().await;
}

#[tokio::test]
async fn restart_plugin_needs_the_kind_of_a_plugin_the_core_runs() {
    let control = Arc::new(FakeControl {
        plugins: vec![failed(Kind::Channel, "libera")],
        ..FakeControl::default()
    });
    let h = Harness::start(Fakes {
        control: Some(control.clone()),
        ..Fakes::default()
    })
    .await;

    // No kind, or one this version doesn't know, is rejected whatever the name.
    for kind in [pb::InstanceKind::Unspecified as i32, 99] {
        assert_eq!(
            restart_as(&h, kind, "libera").await,
            Err(Code::InvalidArgument)
        );
        assert_eq!(
            restart_as(&h, kind, "nope").await,
            Err(Code::InvalidArgument)
        );
    }
    // The instance has to be of the named kind: `libera` is a channel, and `files` a toolset.
    assert_eq!(
        restart(&h, pb::InstanceKind::Toolset, "libera").await,
        Err(Code::NotFound)
    );
    assert_eq!(
        restart(&h, pb::InstanceKind::Channel, "files").await,
        Err(Code::NotFound)
    );
    // Providers are built in, so they have no plugin to restart.
    assert_eq!(
        restart(&h, pb::InstanceKind::Provider, "libera").await,
        Err(Code::NotFound)
    );
    assert!(control.restarted.lock().unwrap().is_empty());
    h.stop().await;
}

#[tokio::test]
async fn a_merged_control_restarts_only_the_kind_asked_for() {
    // A channel and a toolset with the same name, each supervised by its own control.
    let channels = Arc::new(FakeControl {
        plugins: vec![failed(Kind::Channel, "shared")],
        ..FakeControl::default()
    });
    let toolsets = Arc::new(FakeControl {
        plugins: vec![failed(Kind::Toolset, "shared")],
        ..FakeControl::default()
    });
    let merged = MergedControl::new(vec![
        channels.clone() as Arc<dyn PluginControl>,
        toolsets.clone() as Arc<dyn PluginControl>,
    ]);

    assert!(merged.restart(Kind::Toolset, "shared"));
    assert!(channels.restarted.lock().unwrap().is_empty());
    assert_eq!(*toolsets.restarted.lock().unwrap(), ["shared"]);

    assert!(merged.restart(Kind::Channel, "shared"));
    assert_eq!(*channels.restarted.lock().unwrap(), ["shared"]);
    assert_eq!(*toolsets.restarted.lock().unwrap(), ["shared"]);

    assert!(!merged.restart(Kind::Provider, "shared"));
    assert!(!merged.restart(Kind::Toolset, "other"));
}

#[tokio::test]
async fn a_merged_control_lists_every_host_and_restarts_on_the_one_that_owns_the_instance() {
    let runtime = |kind, instance: &str, state, tool_count| PluginRuntime {
        kind,
        instance: InstanceName::new(instance).unwrap(),
        binary: None,
        state,
        restarts: 1,
        last_error: Some(format!("{instance} failed")),
        version: Some("2.0".into()),
        requested_grants: None,
        tool_count,
    };
    let channels = Arc::new(FakeControl {
        plugins: vec![runtime(
            Kind::Channel,
            "libera",
            PluginRunState::Failed,
            None,
        )],
        ..FakeControl::default()
    });
    let toolsets = Arc::new(FakeControl {
        plugins: vec![
            runtime(Kind::Toolset, "files", PluginRunState::Idle, Some(3)),
            runtime(
                Kind::Toolset,
                "github",
                PluginRunState::BackingOff { until: at(60) },
                Some(0),
            ),
        ],
        ..FakeControl::default()
    });
    let merged = Arc::new(MergedControl::new(vec![
        channels.clone() as Arc<dyn PluginControl>,
        toolsets.clone() as Arc<dyn PluginControl>,
    ]));
    let h = Harness::start(Fakes {
        control: Some(merged),
        ..Fakes::default()
    })
    .await;

    // `ListPlugins` shows the toolsets with their state, last error, restarts, version, and tool
    // count, beside the channel plugin.
    let listed = h
        .client()
        .await
        .list_plugins(pb::ListPluginsRequest::default())
        .await
        .unwrap()
        .into_inner()
        .plugins;
    let by_name = |name: &str| listed.iter().find(|p| p.instance == name).unwrap();
    let files = by_name("files");
    assert_eq!(files.kind(), pb::InstanceKind::Toolset);
    assert_eq!(files.state(), pb::PluginState::Idle);
    assert_eq!(files.tool_count, Some(3));
    assert_eq!(files.restarts, 1);
    assert_eq!(files.last_error.as_deref(), Some("files failed"));
    assert_eq!(files.version.as_deref(), Some("2.0"));
    let github = by_name("github");
    assert_eq!(github.state(), pb::PluginState::Backoff);
    assert_eq!(github.retry_at.unwrap().seconds, at(60).as_second());
    assert_eq!(github.tool_count, Some(0));
    assert_eq!(by_name("libera").state(), pb::PluginState::Failed);
    assert_eq!(by_name("libera").tool_count, None);

    // Each restart reaches only the host that supervises the instance.
    assert_eq!(
        restart(&h, pb::InstanceKind::Toolset, "files").await,
        Ok(())
    );
    assert_eq!(
        restart(&h, pb::InstanceKind::Channel, "libera").await,
        Ok(())
    );
    assert_eq!(
        restart(&h, pb::InstanceKind::Channel, "mail").await,
        Err(Code::FailedPrecondition)
    );
    assert_eq!(*toolsets.restarted.lock().unwrap(), ["files"]);
    assert_eq!(*channels.restarted.lock().unwrap(), ["libera"]);
    h.stop().await;
}

#[tokio::test]
async fn a_runtime_of_another_kind_with_a_configured_name_is_listed_apart_from_that_instance() {
    // The configuration has the channel `libera`; supervision reports a toolset by that name,
    // which is not the channel's runtime. The channel row gets no runtime, and the toolset is
    // listed after the configured instances as a row of its own.
    let control = Arc::new(FakeControl {
        plugins: vec![PluginRuntime {
            kind: Kind::Toolset,
            instance: InstanceName::new("libera").unwrap(),
            binary: None,
            state: PluginRunState::Failed,
            restarts: 2,
            last_error: Some("toolset failed".into()),
            version: Some("9.9".into()),
            requested_grants: None,
            tool_count: Some(5),
        }],
        ..FakeControl::default()
    });
    let h = Harness::start(Fakes {
        control: Some(control),
        ..Fakes::default()
    })
    .await;

    let listed = h
        .client()
        .await
        .list_plugins(pb::ListPluginsRequest::default())
        .await
        .unwrap()
        .into_inner()
        .plugins;
    let named: Vec<_> = listed.iter().filter(|p| p.instance == "libera").collect();
    assert_eq!(named.len(), 2, "{listed:?}");
    let channel = named[0];
    assert_eq!(channel.kind(), pb::InstanceKind::Channel);
    assert_eq!(channel.state(), pb::PluginState::Unspecified);
    assert_eq!(channel.last_error, None);
    assert_eq!(channel.version, None);
    assert_eq!(channel.tool_count, None);
    let toolset = named[1];
    assert_eq!(toolset.kind(), pb::InstanceKind::Toolset);
    assert_eq!(toolset.state(), pb::PluginState::Failed);
    assert_eq!(toolset.last_error.as_deref(), Some("toolset failed"));
    assert_eq!(toolset.version.as_deref(), Some("9.9"));
    assert_eq!(toolset.tool_count, Some(5));
    assert_eq!(toolset.restarts, 2);
    assert_eq!(
        listed.last().unwrap().kind(),
        pb::InstanceKind::Toolset,
        "the leftover row comes last"
    );
    h.stop().await;
}

#[tokio::test]
async fn a_failed_stored_toolset_is_listed_though_the_configuration_lacks_it() {
    // The manager reports a stored toolset that failed validation, which isn't in the live
    // configuration, so it isn't started. It is listed, and restarting it goes to the manager.
    let control = Arc::new(FakeControl {
        plugins: vec![PluginRuntime {
            kind: Kind::Toolset,
            instance: InstanceName::new("notes").unwrap(),
            binary: None,
            state: PluginRunState::Failed,
            restarts: 0,
            last_error: Some("auth.bearer: no secret \"notes-token\" in secrets.toml".into()),
            version: None,
            requested_grants: None,
            tool_count: Some(0),
        }],
        ..FakeControl::default()
    });
    let h = Harness::start(Fakes {
        control: Some(control.clone()),
        ..Fakes::default()
    })
    .await;
    let mut client = h.client().await;

    let listed = client
        .list_plugins(pb::ListPluginsRequest::default())
        .await
        .unwrap()
        .into_inner()
        .plugins;
    let notes = listed.iter().find(|p| p.instance == "notes").unwrap();
    assert_eq!(notes.kind(), pb::InstanceKind::Toolset);
    assert_eq!(notes.state(), pb::PluginState::Failed);
    assert_eq!(
        notes.last_error.as_deref(),
        Some("auth.bearer: no secret \"notes-token\" in secrets.toml")
    );
    assert_eq!(notes.tool_count, Some(0));
    // The configured instances are still listed, and `Status` shows it too.
    assert!(listed.len() > 1, "{listed:?}");
    let status = client
        .status(pb::StatusRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(status.plugins.iter().any(|p| p.instance == "notes"));

    assert_eq!(
        restart(&h, pb::InstanceKind::Toolset, "notes").await,
        Ok(())
    );
    assert_eq!(*control.restarted.lock().unwrap(), ["notes"]);
    assert_eq!(
        restart(&h, pb::InstanceKind::Toolset, "ghost").await,
        Err(Code::NotFound)
    );
    // A stored toolset is not a channel.
    assert_eq!(
        restart(&h, pb::InstanceKind::Channel, "notes").await,
        Err(Code::NotFound)
    );
    h.stop().await;
}

#[tokio::test]
async fn a_stored_toolset_that_lost_its_name_does_not_replace_the_instance_that_kept_it() {
    let runtime = |state, last_error: &str, tool_count| PluginRuntime {
        kind: Kind::Toolset,
        instance: InstanceName::new("files").unwrap(),
        binary: None,
        state,
        restarts: 0,
        last_error: Some(last_error.into()),
        version: None,
        requested_grants: None,
        tool_count,
    };
    // The toolset host supervises `[toolset.files]`; the manager reports the stored `files`.
    let host = Arc::new(FakeControl {
        plugins: vec![runtime(PluginRunState::Idle, "idle", Some(3))],
        ..FakeControl::default()
    });
    let manager = Arc::new(FakeControl {
        plugins: vec![runtime(
            PluginRunState::Failed,
            "the name is taken by `[toolset.files]` in giles.toml",
            Some(0),
        )],
        ..FakeControl::default()
    });
    let merged = Arc::new(MergedControl::new(vec![
        host.clone() as Arc<dyn PluginControl>,
        manager.clone() as Arc<dyn PluginControl>,
    ]));
    let h = Harness::start(Fakes {
        control: Some(merged),
        ..Fakes::default()
    })
    .await;

    let listed = h
        .client()
        .await
        .list_plugins(pb::ListPluginsRequest::default())
        .await
        .unwrap()
        .into_inner()
        .plugins;
    let files: Vec<_> = listed.iter().filter(|p| p.instance == "files").collect();
    assert_eq!(files.len(), 2, "{listed:?}");
    // The configured toolset (listed with the configured instances) has the host's report.
    assert_eq!(files[0].state(), pb::PluginState::Idle);
    assert_eq!(files[0].tool_count, Some(3));
    assert_eq!(files[0].location, "/bin/echo");
    // The stored one is the unconfigured row, failed, with the reason.
    assert_eq!(files[1].kind(), pb::InstanceKind::Toolset);
    assert_eq!(files[1].state(), pb::PluginState::Failed);
    assert_eq!(
        files[1].last_error.as_deref(),
        Some("the name is taken by `[toolset.files]` in giles.toml")
    );
    // A restart goes to the first control that has the name.
    assert_eq!(
        restart(&h, pb::InstanceKind::Toolset, "files").await,
        Ok(())
    );
    assert_eq!(*host.restarted.lock().unwrap(), ["files"]);
    assert!(manager.restarted.lock().unwrap().is_empty());
    h.stop().await;
}

#[tokio::test]
async fn sessions_are_listed_and_reset() {
    let h = Harness::start(Fakes::default()).await;
    let key = SessionKey {
        agent: AgentId::new("jeeves").unwrap(),
        channel: "web".into(),
        conversation: ConversationId::new("chat-1").unwrap(),
    };
    let (session, _) = h
        .store
        .open_session(key.clone(), SessionKind::Direct, None, at(5))
        .await
        .unwrap();
    let mut client = h.client().await;

    let list = client
        .list_sessions(pb::ListSessionsRequest::default())
        .await
        .unwrap()
        .into_inner()
        .sessions;
    assert_eq!(list.len(), 1);
    let s = &list[0];
    assert_eq!(s.id, session.id.to_string());
    assert_eq!(
        (s.agent.as_str(), s.instance.as_deref()),
        ("jeeves", Some("web"))
    );
    let conversation = s.conversation.as_ref().unwrap();
    assert_eq!(conversation.id, "chat-1");
    assert_eq!(
        conversation.kind(),
        giles_proto::types::v1::ConversationKind::Direct
    );
    assert_eq!(s.started_at.unwrap().seconds, at(5).as_second());
    assert!(s.ended_at.is_none());
    assert_eq!(s.message_count, 0);

    let reset = |id: &str| pb::ResetSessionRequest {
        session_id: id.into(),
    };
    client.reset_session(reset(&s.id)).await.unwrap();
    // Like /new: the session ended at the clock's time, and the key opens a new one.
    let ended = h.store.session(session.id).await.unwrap().unwrap();
    assert_eq!(ended.ended_at, Some(at(1000)));
    assert!(h.store.find_open_session(key).await.unwrap().is_none());
    let code =
        |r: Result<tonic::Response<pb::ResetSessionResponse>, tonic::Status>| r.unwrap_err().code();
    assert_eq!(
        code(client.reset_session(reset(&s.id)).await),
        Code::FailedPrecondition
    );
    assert_eq!(
        code(client.reset_session(reset("999")).await),
        Code::NotFound
    );
    assert_eq!(
        code(client.reset_session(reset("abc")).await),
        Code::InvalidArgument
    );

    // Ended sessions only on request; the agent filter applies.
    let list = |agent: Option<&str>, include_ended| pb::ListSessionsRequest {
        agent: agent.map(Into::into),
        include_ended,
    };
    let count = |r: tonic::Response<pb::ListSessionsResponse>| r.into_inner().sessions.len();
    assert_eq!(
        count(client.list_sessions(list(None, false)).await.unwrap()),
        0
    );
    assert_eq!(
        count(client.list_sessions(list(None, true)).await.unwrap()),
        1
    );
    assert_eq!(
        count(
            client
                .list_sessions(list(Some("bunter"), true))
                .await
                .unwrap()
        ),
        0
    );
    assert_eq!(
        client
            .list_sessions(list(Some(""), true))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    h.stop().await;
}

#[tokio::test]
async fn login_links_come_from_the_web_channel() {
    let request = |user: &str| pb::CreateLoginLinkRequest { user: user.into() };

    let h = Harness::start(Fakes::default()).await;
    let err = h
        .client()
        .await
        .create_login_link(request("owner"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unavailable);
    assert!(err.message().contains("web channel"), "{}", err.message());
    h.stop().await;

    let h = Harness::start(Fakes {
        links: true,
        ..Fakes::default()
    })
    .await;
    let mut client = h.client().await;
    let link = client
        .create_login_link(request("owner"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(link.url, "http://127.0.0.1:8080/login/abc");
    assert_eq!(link.expires_at.unwrap().seconds, at(1000).as_second() + 900);
    let code = |user| {
        let mut client = client.clone();
        async move {
            client
                .create_login_link(request(user))
                .await
                .unwrap_err()
                .code()
        }
    };
    assert_eq!(code("guest").await, Code::FailedPrecondition);
    assert_eq!(code("stranger").await, Code::NotFound);
    assert_eq!(code("").await, Code::InvalidArgument);
    h.stop().await;
}

#[tokio::test]
async fn usage_is_reported_per_agent_provider_and_model() {
    let h = Harness::start(Fakes::default()).await;
    for (secs, model, read) in [(10, "claude", 900), (20, "claude", 0), (30, "haiku", 5)] {
        h.store
            .record_usage(UsageRecord {
                at: at(secs),
                provider: "anthropic".into(),
                model: model.into(),
                agent: AgentId::new("jeeves").unwrap(),
                user: None,
                session: None,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cache_read_tokens: read,
                    cache_write_tokens: 0,
                },
                fallback: model == "haiku",
            })
            .await
            .unwrap();
    }
    let mut client = h.client().await;
    let rows = client
        .usage_report(pb::UsageReportRequest::default())
        .await
        .unwrap()
        .into_inner()
        .rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (
            rows[0].agent.as_str(),
            rows[0].provider.as_str(),
            rows[0].model.as_str()
        ),
        ("jeeves", "anthropic", "claude")
    );
    assert_eq!((rows[0].requests, rows[0].fallback_requests), (2, 0));
    let usage = rows[0].usage.unwrap();
    assert_eq!((usage.input_tokens, usage.cache_read_tokens), (200, 900));
    assert_eq!((rows[1].requests, rows[1].fallback_requests), (1, 1));

    let ts = |secs| Some(giles_plugin_sdk::convert::timestamp_to_proto(at(secs)));
    let rows = client
        .usage_report(pb::UsageReportRequest {
            since: ts(20),
            until: ts(30),
        })
        .await
        .unwrap()
        .into_inner()
        .rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].requests, 1);
    assert_eq!(rows[0].usage.unwrap().cache_read_tokens, 0);

    let bad = prost_types::Timestamp {
        seconds: 0,
        nanos: -1,
    };
    let err = client
        .usage_report(pb::UsageReportRequest {
            since: Some(bad),
            until: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    h.stop().await;
}

#[tokio::test]
async fn backup_writes_a_snapshot_to_the_directory() {
    let h = Harness::start(Fakes::default()).await;
    let agent = AgentId::new("main").unwrap();
    h.store
        .job_set_last_run(agent.clone(), JobKey::Config("tea".into()), at(5))
        .await
        .unwrap();
    let out = tempfile::tempdir().unwrap();

    let paths = h
        .client()
        .await
        .backup(pb::BackupRequest {
            dir: out.path().to_str().unwrap().into(),
        })
        .await
        .unwrap()
        .into_inner()
        .paths;
    // The service's clock names it: `FixedClock` is 1000 s after 2027-01-15 08:00 UTC.
    let database = out.path().join("giles-20270115T081640Z.db");
    let archive = out.path().join("giles-20270115T081640Z.tar");
    assert_eq!(
        paths,
        [database.to_str().unwrap(), archive.to_str().unwrap()]
    );

    let copy = Store::open(&database).await.unwrap();
    assert_eq!(
        copy.job_last_run(agent, JobKey::Config("tea".into()))
            .await
            .unwrap(),
        Some(at(5))
    );
    let mut names = Vec::new();
    for entry in tar::Archive::new(std::fs::File::open(&archive).unwrap())
        .entries()
        .unwrap()
    {
        names.push(
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
    }
    // The config and the (empty) trees of the layout.
    assert_eq!(
        names,
        [
            "giles.toml",
            "agents",
            "users",
            "skills",
            "data/blobs",
            "data/blobs/sha256"
        ]
    );

    // A second backup in the same second is refused, not overwritten.
    let err = h
        .client()
        .await
        .backup(pb::BackupRequest {
            dir: out.path().to_str().unwrap().into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists);
    h.stop().await;
}

#[tokio::test]
async fn backup_needs_an_absolute_path_to_an_existing_directory() {
    let h = Harness::start(Fakes::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    let mut client = h.client().await;
    for (what, path, code) in [
        ("relative", "backups".to_owned(), Code::InvalidArgument),
        ("empty", String::new(), Code::InvalidArgument),
        (
            "missing",
            dir.path().join("missing").to_str().unwrap().to_owned(),
            Code::NotFound,
        ),
        (
            "a file",
            file.to_str().unwrap().to_owned(),
            Code::InvalidArgument,
        ),
    ] {
        let err = client
            .backup(pb::BackupRequest { dir: path })
            .await
            .unwrap_err();
        assert_eq!(err.code(), code, "{what}: {}", err.message());
    }
    // Nothing was written anywhere.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    h.stop().await;
}

fn toolset_with(toolsets: &Arc<FakeToolsets>) -> Fakes {
    Fakes {
        toolsets: Some(toolsets.clone()),
        ..Fakes::default()
    }
}

fn set_request(name: &str, spec_toml: &str, expected: Option<i64>) -> pb::SetToolsetRequest {
    pb::SetToolsetRequest {
        name: name.into(),
        spec_toml: spec_toml.into(),
        expected_updated_at: expected.map(|secs| timestamp_to_proto(at(secs))),
    }
}

async fn set_toolset(
    client: &mut AdminClient,
    request: pb::SetToolsetRequest,
) -> Result<pb::Toolset, tonic::Status> {
    client
        .set_toolset(request)
        .await
        .map(|response| response.into_inner().toolset.unwrap())
}

#[tokio::test]
async fn toolsets_are_listed_and_shown_as_toml_with_references_as_written() {
    let toolsets = FakeToolsets::new(
        &[
            ("notes", "url = \"https://notes.example.com/mcp\""),
            ("files", FILES_SPEC),
        ],
        &["Github"],
    );
    toolsets.with(|state| {
        state.stored.insert(
            "broken".into(),
            (spec("url = \"https://gone.example.com/mcp\""), 7),
        );
    });
    let h = Harness::start(toolset_with(&toolsets)).await;
    let mut client = h.client().await;

    let listed = client
        .list_toolsets(pb::ListToolsetsRequest {})
        .await
        .unwrap()
        .into_inner()
        .toolsets;
    let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["broken", "files", "Github", "notes"]);

    let files = &listed[1];
    assert_eq!(files.source(), pb::ToolsetSource::Stored);
    assert_eq!(files.updated_at, Some(timestamp_to_proto(at(2))));
    assert_eq!(files.failure, None);
    // The references are as written, and the text reads back as the stored spec.
    let text = files.spec_toml.as_deref().unwrap();
    assert!(text.contains("secret = \"files\""), "{text}");
    assert_eq!(ToolsetSpec::from_toml(text).unwrap(), spec(FILES_SPEC));

    // A stored toolset that isn't running says why.
    assert_eq!(listed[0].failure.as_deref(), Some("no secret \"broken\""));

    // A toolset of giles.toml has a source and nothing else.
    let github = &listed[2];
    assert_eq!(github.source(), pb::ToolsetSource::GilesToml);
    assert_eq!((&github.spec_toml, &github.updated_at), (&None, &None));

    // `GetToolset` finds it ignoring case, under the name it has.
    let got = client
        .get_toolset(pb::GetToolsetRequest {
            name: "FILES".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .toolset
        .unwrap();
    assert_eq!(&got, files);
    let github = client
        .get_toolset(pb::GetToolsetRequest {
            name: "github".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .toolset
        .unwrap();
    assert_eq!(github.name, "Github");
    assert_eq!(github.source(), pb::ToolsetSource::GilesToml);
    let err = client
        .get_toolset(pb::GetToolsetRequest {
            name: "ghost".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert!(err.message().contains("`ghost`"), "{}", err.message());
    h.stop().await;
}

#[tokio::test]
async fn set_toolset_creates_and_replaces() {
    let toolsets = FakeToolsets::new(&[], &[]);
    let h = Harness::start(toolset_with(&toolsets)).await;
    let mut client = h.client().await;

    let created = set_toolset(&mut client, set_request("files", FILES_SPEC, None))
        .await
        .unwrap();
    assert_eq!(created.name, "files");
    assert_eq!(created.source(), pb::ToolsetSource::Stored);
    assert_eq!(created.updated_at, Some(timestamp_to_proto(at(1))));
    assert_eq!(toolsets.stored("files"), Some((spec(FILES_SPEC), 1)));

    // The name matches ignoring case, and keeps its spelling. No `expected_updated_at`: replaces
    // whatever is there.
    let other = "url = \"https://other.example.com/mcp\"";
    let replaced = set_toolset(&mut client, set_request("Files", other, None))
        .await
        .unwrap();
    assert_eq!(replaced.name, "files");
    assert_eq!(replaced.updated_at, Some(timestamp_to_proto(at(2))));
    assert_eq!(toolsets.stored("files"), Some((spec(other), 2)));
    assert_eq!(toolsets.stored("Files"), None);

    // With the `updated_at` of the last read, it replaces.
    let again = set_toolset(
        &mut client,
        set_request("files", "url = \"https://third.example.com/mcp\"", Some(2)),
    )
    .await
    .unwrap();
    assert_eq!(again.updated_at, Some(timestamp_to_proto(at(3))));
    h.stop().await;
}

#[tokio::test]
async fn set_toolset_refuses_a_change_made_since_it_was_read() {
    let toolsets = FakeToolsets::new(&[("files", FILES_SPEC)], &["github"]);
    let h = Harness::start(toolset_with(&toolsets)).await;
    let mut client = h.client().await;

    // Changed since the caller read it (at 0).
    let err = set_toolset(
        &mut client,
        set_request("files", "url = \"https://other.example.com/mcp\"", Some(0)),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("was changed"), "{}", err.message());
    assert_eq!(toolsets.stored("files"), Some((spec(FILES_SPEC), 1)));

    // Expecting a toolset that isn't there.
    let err = set_toolset(&mut client, set_request("ghost", FILES_SPEC, Some(1)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert_eq!(toolsets.stored("ghost"), None);

    // A toolset of giles.toml can't be changed, with or without the time.
    for expected in [None, Some(1)] {
        let err = set_toolset(&mut client, set_request("github", FILES_SPEC, expected))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::FailedPrecondition, "{expected:?}");
        assert!(err.message().contains("giles.toml"), "{}", err.message());
    }

    // An `expected_updated_at` that isn't a time.
    let mut request = set_request("files", FILES_SPEC, None);
    request.expected_updated_at = Some(prost_types::Timestamp {
        seconds: i64::MAX,
        nanos: 0,
    });
    let err = client.set_toolset(request).await.unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(
        err.message().starts_with("expected_updated_at: "),
        "{}",
        err.message()
    );
    assert_eq!(toolsets.stored("files"), Some((spec(FILES_SPEC), 1)));
    h.stop().await;
}

#[tokio::test]
async fn set_toolset_names_the_key_of_a_spec_that_is_refused() {
    let toolsets = FakeToolsets::new(&[("files", FILES_SPEC)], &[]);
    let h = Harness::start(toolset_with(&toolsets)).await;
    let mut client = h.client().await;

    for (what, name, spec_toml, expected) in [
        // The manager refuses it, at a key.
        (
            "a stdio server",
            "files",
            "command = \"/bin/files\"\n",
            "url: a stored toolset is a remote server",
        ),
        (
            "a stdio server, new",
            "new",
            "command = \"/bin/files\"\n",
            "url: a stored toolset is a remote server",
        ),
        // Or as a whole.
        (
            "a taken name",
            "libera",
            "url = \"https://libera.example.com/mcp\"\n",
            "the name is taken by `[channel.libera]` in giles.toml",
        ),
        // The text isn't a spec.
        (
            "a syntax error",
            "new",
            "url = \n",
            "invalid TOML at line 1, column ",
        ),
        (
            "a wrong type",
            "new",
            "url = \"https://x.example.com/mcp\"\nparallel = \"yes\"\n",
            "parallel: ",
        ),
        (
            "a wrong type in a list",
            "new",
            "skills = [\"/a\", 2]\n",
            "skills[1]: ",
        ),
        (
            "a misspelled key",
            "new",
            "ulr = \"https://x.example.com/mcp\"\n",
            "ulr: unknown key",
        ),
        (
            "a misspelled key in a strict table",
            "new",
            "[sandbox]\nredd = [\"/x\"]\n",
            "sandbox.redd: unknown field `redd`",
        ),
    ] {
        let err = set_toolset(&mut client, set_request(name, spec_toml, None))
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            Code::InvalidArgument,
            "{what}: {}",
            err.message()
        );
        assert!(
            err.message().starts_with(expected),
            "{what}: {}",
            err.message()
        );
    }
    // Nothing changed.
    assert_eq!(toolsets.stored("files"), Some((spec(FILES_SPEC), 1)));
    assert_eq!(toolsets.stored("new"), None);
    h.stop().await;
}

#[tokio::test]
async fn a_set_toolset_that_races_with_a_create_says_it_exists_and_a_database_failure_is_internal()
{
    let toolsets = FakeToolsets::new(&[], &[]);
    let h = Harness::start(toolset_with(&toolsets)).await;
    let mut client = h.client().await;

    toolsets.with(|state| state.racing = true);
    let err = set_toolset(&mut client, set_request("files", FILES_SPEC, None))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::AlreadyExists);
    assert!(err.message().contains("`files`"), "{}", err.message());
    toolsets.with(|state| state.racing = false);

    toolsets.with(|state| state.broken = true);
    for code in [
        set_toolset(&mut client, set_request("files", FILES_SPEC, None))
            .await
            .unwrap_err()
            .code(),
        client
            .list_toolsets(pb::ListToolsetsRequest {})
            .await
            .unwrap_err()
            .code(),
        client
            .get_toolset(pb::GetToolsetRequest {
                name: "files".into(),
            })
            .await
            .unwrap_err()
            .code(),
        client
            .remove_toolset(pb::RemoveToolsetRequest {
                name: "files".into(),
            })
            .await
            .unwrap_err()
            .code(),
    ] {
        assert_eq!(code, Code::Internal);
    }
    let err = client
        .rename_toolset(pb::RenameToolsetRequest {
            old: "files".into(),
            new: "docs".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Internal);
    toolsets.with(|state| state.broken = false);
    h.stop().await;
}

#[tokio::test]
async fn remove_toolset_removes_a_stored_toolset() {
    let toolsets = FakeToolsets::new(&[("files", FILES_SPEC)], &["github"]);
    let h = Harness::start(toolset_with(&toolsets)).await;
    let mut client = h.client().await;
    let remove = |name: &str| pb::RemoveToolsetRequest { name: name.into() };

    let removed = client
        .remove_toolset(remove("FILES"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(removed.name, "files");
    assert_eq!(toolsets.stored("files"), None);

    let err = client.remove_toolset(remove("files")).await.unwrap_err();
    assert_eq!(err.code(), Code::NotFound);
    assert!(err.message().contains("`files`"), "{}", err.message());
    let err = client.remove_toolset(remove("github")).await.unwrap_err();
    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("giles.toml"), "{}", err.message());
    h.stop().await;
}

fn rename_request(old: &str, new: &str) -> pb::RenameToolsetRequest {
    pb::RenameToolsetRequest {
        old: old.into(),
        new: new.into(),
    }
}

#[tokio::test]
async fn rename_toolset_renames_a_stored_toolset_and_lists_the_globs_it_rewrote() {
    let toolsets = FakeToolsets::new(&[("files", FILES_SPEC), ("notes", FILES_SPEC)], &[]);
    let h = Harness::start(toolset_with(&toolsets)).await;
    let mut client = h.client().await;

    // The old name matches ignoring case.
    let renamed = client
        .rename_toolset(rename_request("FILES", "docs"))
        .await
        .unwrap()
        .into_inner();
    // The name as stored, not as asked for.
    assert_eq!(renamed.old, "files");
    let toolset = renamed.toolset.unwrap();
    assert_eq!(toolset.name, "docs");
    assert_eq!(toolset.source(), pb::ToolsetSource::Stored);
    assert_eq!(
        toolset.updated_at,
        Some(timestamp_to_proto(at(toolsets.stored("docs").unwrap().1)))
    );
    assert_eq!(toolset.failure, None);
    assert_eq!(
        renamed.agents,
        [pb::AgentGlobChange {
            agent: "main".into(),
            globs: vec![
                pb::GlobChange {
                    key: "tools".into(),
                    from: "files__*".into(),
                    to: "docs__*".into(),
                },
                pb::GlobChange {
                    key: "ask".into(),
                    from: "files__delete".into(),
                    to: "docs__delete".into(),
                },
            ],
        }]
    );
    assert_eq!(toolsets.stored("files"), None);
    assert_eq!(toolsets.stored("docs").unwrap().0, spec(FILES_SPEC));

    // No agent names it: nothing is listed.
    let renamed = client
        .rename_toolset(rename_request("notes", "memos"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(renamed.old, "notes");
    assert_eq!(renamed.toolset.unwrap().name, "memos");
    assert!(renamed.agents.is_empty());

    // Another case of the same name is a rename too.
    let renamed = client
        .rename_toolset(rename_request("memos", "Memos"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(renamed.old, "memos");
    assert_eq!(renamed.toolset.unwrap().name, "Memos");
    h.stop().await;
}

#[tokio::test]
async fn rename_toolset_answers_each_refusal_with_its_status() {
    let toolsets = FakeToolsets::new(&[("files", FILES_SPEC), ("notes", FILES_SPEC)], &["github"]);
    let h = Harness::start(toolset_with(&toolsets)).await;
    let mut client = h.client().await;
    let mut rename = async |old: &str, new: &str| {
        client
            .rename_toolset(rename_request(old, new))
            .await
            .unwrap_err()
    };

    let err = rename("ghost", "docs").await;
    assert_eq!(err.code(), Code::NotFound);
    assert!(err.message().contains("`ghost`"), "{}", err.message());

    // The name that is taken is the new one.
    let err = rename("files", "NOTES").await;
    assert_eq!(err.code(), Code::AlreadyExists);
    assert!(err.message().contains("`NOTES`"), "{}", err.message());

    for (new, reason) in [
        ("files", "the same"),
        ("not a name", "letters, digits"),
        ("libera", "`[channel.libera]`"),
    ] {
        let err = rename("files", new).await;
        assert_eq!(err.code(), Code::InvalidArgument, "{new}");
        assert!(err.message().contains(reason), "{new}: {}", err.message());
    }

    // A glob that can't be rewritten is named.
    let err = rename("files", "globby").await;
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("`fi*`"), "{}", err.message());

    let err = rename("github", "code").await;
    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("giles.toml"), "{}", err.message());

    // A failed step says which, why, and what could not be undone.
    let err = rename("files", "stuck").await;
    assert_eq!(err.code(), Code::Internal);
    for part in [
        "rewriting /agents/main/agent.toml",
        "permission denied",
        "put the toolset's row back: busy",
    ] {
        assert!(err.message().contains(part), "{part}: {}", err.message());
    }
    assert_eq!(toolsets.stored("files").unwrap().0, spec(FILES_SPEC));
    h.stop().await;
}

#[tokio::test]
async fn toolset_rpcs_are_unavailable_without_a_toolset_manager() {
    let h = Harness::start(Fakes::default()).await;
    let mut client = h.client().await;
    let codes: Vec<(&str, Code)> = every_rpc(&mut client)
        .await
        .into_iter()
        .filter(|(rpc, _)| rpc.ends_with("Toolset") || *rpc == "ListToolsets")
        .collect();
    assert_eq!(
        codes,
        [
            ("ListToolsets", Code::Unavailable),
            ("GetToolset", Code::Unavailable),
            ("SetToolset", Code::Unavailable),
            ("RemoveToolset", Code::Unavailable),
            ("RenameToolset", Code::Unavailable),
        ]
    );
    h.stop().await;
}
