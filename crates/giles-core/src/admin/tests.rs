//! The admin service over a real socket in a temporary data root, with fake subsystems.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
use crate::config::{Config, InstanceName, LoadOptions};
use crate::paths::DataRoot;
use crate::sandbox::HostReport;
use crate::sessions::{ChannelStatusRecord, Clock};

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

[toolset.github]
url = "https://api.example.com/mcp/"
"#;

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
    restarted: Mutex<Vec<String>>,
}

impl PluginControl for FakeControl {
    fn plugins(&self) -> Vec<PluginRuntime> {
        self.plugins.clone()
    }

    fn restart(&self, instance: &str) -> bool {
        let supervised = self.plugins.iter().any(|p| p.instance.as_str() == instance);
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
    links: bool,
    sandbox: Option<Result<HostReport, String>>,
}

impl Harness {
    async fn start(fakes: Fakes) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        std::fs::write(root.config_file(), CONFIG).unwrap();
        let config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let store = Store::open(root.database_file()).await.unwrap();
        let blobs = BlobStore::open(root.blobs_dir(), store.clone())
            .await
            .unwrap();
        let statuses = Arc::new(FakeStatuses::default());
        let lock = InstanceLock::acquire(&root).unwrap();
        let listener = AdminListener::bind(&root, &lock).unwrap();
        let service = Arc::new(AdminService::new(AdminOptions {
            config: Arc::new(config),
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

async fn restart(h: &Harness, instance: &str) -> Result<(), Code> {
    h.client()
        .await
        .restart_plugin(pb::RestartPluginRequest {
            instance: instance.into(),
        })
        .await
        .map(drop)
        .map_err(|s| s.code())
}

#[tokio::test]
async fn restart_plugin_needs_a_configured_supervised_plugin() {
    // Without supervision.
    let h = Harness::start(Fakes::default()).await;
    assert_eq!(restart(&h, "libera").await, Err(Code::Unavailable));
    assert_eq!(restart(&h, "web").await, Err(Code::NotFound), "built in");
    assert_eq!(restart(&h, "nope").await, Err(Code::NotFound));
    h.stop().await;

    let control = Arc::new(FakeControl {
        plugins: vec![PluginRuntime {
            instance: InstanceName::new("libera").unwrap(),
            binary: None,
            state: PluginRunState::Failed,
            restarts: 0,
            last_error: None,
            version: None,
            requested_grants: None,
            tool_count: None,
        }],
        ..FakeControl::default()
    });
    let h = Harness::start(Fakes {
        control: Some(control.clone()),
        ..Fakes::default()
    })
    .await;
    assert_eq!(restart(&h, "libera").await, Ok(()));
    assert_eq!(*control.restarted.lock().unwrap(), ["libera"]);
    assert_eq!(restart(&h, "files").await, Err(Code::FailedPrecondition));
    h.stop().await;
}

#[tokio::test]
async fn a_merged_control_lists_every_host_and_restarts_on_the_one_that_owns_the_instance() {
    let runtime = |instance: &str, state, tool_count| PluginRuntime {
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
        plugins: vec![runtime("libera", PluginRunState::Failed, None)],
        ..FakeControl::default()
    });
    let toolsets = Arc::new(FakeControl {
        plugins: vec![
            runtime("files", PluginRunState::Idle, Some(3)),
            runtime(
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
    assert_eq!(restart(&h, "files").await, Ok(()));
    assert_eq!(restart(&h, "libera").await, Ok(()));
    assert_eq!(restart(&h, "mail").await, Err(Code::FailedPrecondition));
    assert_eq!(*toolsets.restarted.lock().unwrap(), ["files"]);
    assert_eq!(*channels.restarted.lock().unwrap(), ["libera"]);
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
