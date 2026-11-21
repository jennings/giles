//! Router-level tests of the admin pages over a fake admin service.

use std::sync::Mutex;

use doozers_proto::admin::v1 as pb;
use doozers_proto::admin::v1::admin_service_server::AdminService as AdminRpc;
use doozers_proto::channel::v1::channel_status::State;
use doozers_proto::channel::v1::needs_auth::Challenge;
use doozers_proto::channel::v1::{ChannelStatus as PbStatus, Connected, NeedsAuth};
use doozers_store::{NewIdentity, NewUser, Role as UserRole, UserState, Verification};
use tonic::{Request, Response as RpcResponse, Status};

use super::*;
use crate::users::{UserError, Users};

/// The admin service's answers, from a state the test sets. `RestartPlugin` records the kind and
/// instance as `<kind>/<instance>` (`NOT_FOUND` for one `Status` doesn't list, and
/// `INVALID_ARGUMENT` for no kind).
#[derive(Default)]
pub(super) struct FakeAdmin {
    pub(super) status: Mutex<pb::StatusResponse>,
    pub(super) restarted: Mutex<Vec<String>>,
}

impl FakeAdmin {
    fn set_channel_status(&self, instance: &str, state: State) {
        let mut status = self.status.lock().unwrap();
        let channel = status
            .channels
            .iter_mut()
            .find(|c| c.instance == instance)
            .unwrap();
        channel.status = Some(PbStatus { state: Some(state) });
    }

    pub(super) fn restarted(&self) -> Vec<String> {
        self.restarted.lock().unwrap().clone()
    }
}

#[tonic::async_trait]
impl AdminRpc for FakeAdmin {
    async fn status(
        &self,
        _: Request<pb::StatusRequest>,
    ) -> Result<RpcResponse<pb::StatusResponse>, Status> {
        Ok(RpcResponse::new(self.status.lock().unwrap().clone()))
    }

    async fn list_plugins(
        &self,
        _: Request<pb::ListPluginsRequest>,
    ) -> Result<RpcResponse<pb::ListPluginsResponse>, Status> {
        let plugins = self.status.lock().unwrap().plugins.clone();
        Ok(RpcResponse::new(pb::ListPluginsResponse { plugins }))
    }

    async fn restart_plugin(
        &self,
        request: Request<pb::RestartPluginRequest>,
    ) -> Result<RpcResponse<pb::RestartPluginResponse>, Status> {
        let request = request.into_inner();
        let kind = match request.kind() {
            pb::InstanceKind::Unspecified => {
                return Err(Status::invalid_argument("kind: name the kind"));
            }
            pb::InstanceKind::Provider => "provider",
            pb::InstanceKind::Channel => "channel",
            pb::InstanceKind::Toolset => "toolset",
            pb::InstanceKind::Environment => "environment",
        };
        let wanted = request.kind();
        let instance = request.instance;
        let listed = self
            .status
            .lock()
            .unwrap()
            .plugins
            .iter()
            .any(|p| p.kind() == wanted && p.instance == instance);
        if !listed {
            return Err(Status::not_found(format!(
                "there is no plugin instance `{instance}`"
            )));
        }
        self.restarted
            .lock()
            .unwrap()
            .push(format!("{kind}/{instance}"));
        Ok(RpcResponse::new(pb::RestartPluginResponse {}))
    }

    async fn list_sessions(
        &self,
        _: Request<pb::ListSessionsRequest>,
    ) -> Result<RpcResponse<pb::ListSessionsResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }

    async fn reset_session(
        &self,
        _: Request<pb::ResetSessionRequest>,
    ) -> Result<RpcResponse<pb::ResetSessionResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }

    async fn create_login_link(
        &self,
        _: Request<pb::CreateLoginLinkRequest>,
    ) -> Result<RpcResponse<pb::CreateLoginLinkResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }

    async fn usage_report(
        &self,
        _: Request<pb::UsageReportRequest>,
    ) -> Result<RpcResponse<pb::UsageReportResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }

    async fn backup(
        &self,
        _: Request<pb::BackupRequest>,
    ) -> Result<RpcResponse<pb::BackupResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }

    async fn list_toolsets(
        &self,
        _: Request<pb::ListToolsetsRequest>,
    ) -> Result<RpcResponse<pb::ListToolsetsResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }

    async fn get_toolset(
        &self,
        _: Request<pb::GetToolsetRequest>,
    ) -> Result<RpcResponse<pb::GetToolsetResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }

    async fn set_toolset(
        &self,
        _: Request<pb::SetToolsetRequest>,
    ) -> Result<RpcResponse<pb::SetToolsetResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }

    async fn remove_toolset(
        &self,
        _: Request<pb::RemoveToolsetRequest>,
    ) -> Result<RpcResponse<pb::RemoveToolsetResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }

    async fn rename_toolset(
        &self,
        _: Request<pb::RenameToolsetRequest>,
    ) -> Result<RpcResponse<pb::RenameToolsetResponse>, Status> {
        Err(Status::unimplemented("not in this fake"))
    }
}

const XSS: &str = "<script>alert(1)</script>";

fn at(seconds: i64) -> Option<prost_types::Timestamp> {
    Some(prost_types::Timestamp { seconds, nanos: 0 })
}

/// A daemon with the web channel, an IRC plugin channel in error (with markup in its messages),
/// a WhatsApp plugin channel, and an MCP toolset supervision doesn't report.
fn state() -> pb::StatusResponse {
    let channel =
        |instance: &str, component: &str, runtime: pb::ChannelRuntime, state| pb::ChannelInstance {
            instance: instance.into(),
            component: component.into(),
            runtime: runtime.into(),
            status: Some(PbStatus { state: Some(state) }),
            status_at: at(1_792_540_800),
        };
    pb::StatusResponse {
        version: "1.2.3".into(),
        started_at: at(1_792_540_000),
        config_error: None,
        sandbox_host: Some(pb::SandboxHost {
            available: true,
            description: "Seatbelt (sandbox-exec)".into(),
            landlock_abi: None,
        }),
        channels: vec![
            channel(
                "libera",
                "irc",
                pb::ChannelRuntime::Plugin,
                State::Error(format!("nick in use {XSS}")),
            ),
            channel(
                "web",
                "web",
                pb::ChannelRuntime::BuiltIn,
                State::Connected(Connected {}),
            ),
            channel(
                "whatsapp",
                "whatsapp",
                pb::ChannelRuntime::Plugin,
                State::Connected(Connected {}),
            ),
        ],
        plugins: vec![
            pb::PluginInstance {
                instance: "libera".into(),
                kind: pb::InstanceKind::Channel.into(),
                component: "irc".into(),
                location: "/opt/doozers/plugins/doozers-channel-irc".into(),
                state: pb::PluginState::Backoff.into(),
                retry_at: at(1_792_540_860),
                restarts: 3,
                last_error: Some(format!("connection refused {XSS}")),
                version: Some(format!("0.1.0 {XSS}")),
                channel_status: Some(PbStatus {
                    state: Some(State::Error(format!("nick in use {XSS}"))),
                }),
                sandbox: Some(pb::SandboxStatus {
                    enabled: true,
                    landlock_abi: None,
                    missing: vec![],
                }),
                ..pb::PluginInstance::default()
            },
            pb::PluginInstance {
                instance: "files".into(),
                kind: pb::InstanceKind::Toolset.into(),
                location: "/usr/local/bin/mcp-files".into(),
                state: pb::PluginState::Running.into(),
                started_at: at(1_792_540_900),
                restarts: 1,
                version: Some("2.1.0".into()),
                tool_count: Some(4),
                sandbox: Some(pb::SandboxStatus {
                    enabled: true,
                    landlock_abi: None,
                    missing: vec![],
                }),
                ..pb::PluginInstance::default()
            },
            pb::PluginInstance {
                instance: "broken".into(),
                kind: pb::InstanceKind::Toolset.into(),
                location: "https://mcp.example.com/broken".into(),
                state: pb::PluginState::Failed.into(),
                last_error: Some(format!("HTTP 401 {XSS}")),
                tool_count: Some(0),
                ..pb::PluginInstance::default()
            },
            pb::PluginInstance {
                instance: "search".into(),
                kind: pb::InstanceKind::Toolset.into(),
                location: "https://mcp.example.com/".into(),
                ..pb::PluginInstance::default()
            },
        ],
    }
}

fn guest() -> WebUser {
    WebUser {
        user: UserId::new("guest").unwrap(),
        role: doozers_store::Role::Member,
        sender: NativeId::new("guest").unwrap(),
        csrf: CSRF.into(),
        display_name: "Test user".into(),
        login: auth::Login::Test,
    }
}

impl Env {
    /// The whole application with the admin pages over `admin`, as `user`.
    fn admin_router(&self, admin: &Arc<FakeAdmin>, user: Option<WebUser>) -> axum::Router {
        let app = self
            .app
            .install_admin(self.app.install(RouterBuilder::new()), admin.clone())
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    async fn admin_page(&self, admin: &Arc<FakeAdmin>, path: &str) -> (StatusCode, String) {
        let response = call(
            self.admin_router(admin, Some(owner())),
            Method::GET,
            path,
            &[],
            None,
        )
        .await;
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        let status = response.status();
        (status, body_text(response).await)
    }
}

#[tokio::test]
async fn only_an_admin_gets_the_admin_pages() {
    let env = env().await;
    let admin = Arc::new(FakeAdmin {
        status: Mutex::new(state()),
        ..FakeAdmin::default()
    });
    let cases = [
        (Method::GET, "/admin"),
        (Method::GET, "/admin/plugins"),
        (Method::POST, "/admin/plugins/channel/libera/restart"),
    ];
    for (method, path) in cases {
        for (user, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(guest()), StatusCode::FORBIDDEN),
        ] {
            let response = call(
                env.admin_router(&admin, user),
                method.clone(),
                path,
                &[("x-csrf-token", CSRF)],
                Some(&format!("csrf={CSRF}")),
            )
            .await;
            assert_eq!(response.status(), expected, "{method} {path}");
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
            let html = body_text(response).await;
            assert!(!html.contains("libera"), "{html}");
        }
    }
    assert!(admin.restarted().is_empty());
}

/// Opens the stored users of `env`, which imports the owner of the test configuration (an admin)
/// the first time. The channel's user list follows them.
pub(super) async fn open_users(env: &Env) -> Users {
    Users::open(
        env.store.clone(),
        env.channel.users().clone(),
        &env.registry.config(),
        &env._dir.path().join("doozers.toml"),
    )
    .await
    .unwrap()
}

/// The stored users of `env` ([`open_users`]), and a second admin `root` and a member `mia`, each
/// with the `web:<id>` identity a login signs in as.
pub(super) async fn stored_users(env: &Env) -> Arc<Users> {
    let users = open_users(env).await;
    for (id, role) in [("root", UserRole::Admin), ("mia", UserRole::Member)] {
        let new = NewUser {
            id: UserId::new(id).unwrap(),
            username: id.to_owned(),
            display_name: id.to_uppercase(),
            role,
            timezone: None,
            default_identity: Some(format!("web:{id}")),
        };
        let web = NewIdentity {
            namespace: "web".to_owned(),
            native_id: id.to_owned(),
            verification: Verification::Admin,
        };
        users
            .create(new, vec![web], Timestamp::now())
            .await
            .unwrap();
    }
    Arc::new(users)
}

/// Logs `user` in with a recovery link on the main listener; the session token.
pub(super) async fn session_of(env: &Env, user: &str) -> String {
    let token = invite_token(env, &UserId::new(user).unwrap(), InviteKind::Recovery).await;
    let response = redeem(env.main_app(), &token, &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "{user}");
    cookie_token(set_cookie(&response))
}

impl Env {
    /// `method path` with the session `token`, a CSRF token for a post, and no injected user: the
    /// request is authenticated as a browser's is, so the user's role is read from the stored
    /// users.
    async fn as_session(
        &self,
        admin: &Arc<FakeAdmin>,
        token: &str,
        method: Method,
        path: &str,
    ) -> StatusCode {
        let csrf = auth::cookie_csrf(token);
        let cookie = session(token);
        let form = format!("csrf={csrf}");
        let response = call(
            self.admin_router(admin, None),
            method.clone(),
            path,
            &[("cookie", &cookie), ("x-csrf-token", &csrf)],
            (method == Method::POST).then_some(form.as_str()),
        )
        .await;
        response.status()
    }
}

#[tokio::test]
async fn the_admin_role_decides_who_gets_the_admin_pages() {
    let env = env().await;
    let users = stored_users(&env).await;
    let admin = Arc::new(FakeAdmin {
        status: Mutex::new(state()),
        ..FakeAdmin::default()
    });
    let owner_session = session_of(&env, "owner").await;
    let root = session_of(&env, "root").await;
    let mia = session_of(&env, "mia").await;
    let cases = [
        (Method::GET, "/admin"),
        (Method::GET, "/admin/plugins"),
        (Method::POST, "/admin/plugins/channel/libera/restart"),
    ];
    let try_all = async |token: &str| {
        let mut statuses = Vec::new();
        for (method, path) in &cases {
            statuses.push(env.as_session(&admin, token, method.clone(), path).await);
        }
        statuses
    };
    let allowed = [StatusCode::OK, StatusCode::OK, StatusCode::SEE_OTHER];
    let refused = [StatusCode::FORBIDDEN; 3];

    // The imported owner and a second admin can do the same, and a member can do none of it.
    assert_eq!(try_all(&owner_session).await, allowed);
    assert_eq!(try_all(&root).await, allowed);
    assert_eq!(admin.restarted().len(), 2);
    assert_eq!(try_all(&mia).await, refused);
    assert_eq!(admin.restarted().len(), 2, "the member restarted nothing");

    // A change of role applies to the next request on the same session.
    let now = Timestamp::now();
    users
        .set_role(&UserId::new("root").unwrap(), UserRole::Member, now)
        .await
        .unwrap();
    users
        .set_role(&UserId::new("mia").unwrap(), UserRole::Admin, now)
        .await
        .unwrap();
    assert_eq!(try_all(&root).await, refused);
    assert_eq!(try_all(&mia).await, allowed);

    // A disabled admin is signed out. The last active admin can't be demoted, so the pages stay
    // reachable.
    users
        .set_state(&UserId::new("mia").unwrap(), UserState::Disabled, now)
        .await
        .unwrap();
    assert_eq!(try_all(&mia).await, [StatusCode::UNAUTHORIZED; 3]);
    let err = users
        .set_role(&UserId::new("owner").unwrap(), UserRole::Member, now)
        .await
        .unwrap_err();
    assert!(matches!(err, UserError::LastAdmin), "{err}");
    assert_eq!(try_all(&owner_session).await, allowed);
}

#[tokio::test]
async fn the_status_page_shows_the_admin_state_with_plugin_strings_escaped() {
    let env = env().await;
    let admin = Arc::new(FakeAdmin {
        status: Mutex::new(state()),
        ..FakeAdmin::default()
    });
    let (status, html) = env.admin_page(&admin, "/admin").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "doozers 1.2.3",
        "Seatbelt (sandbox-exec)",
        r#"<section class="card" id="channel-libera">"#,
        "irc, plugin",
        "web, built in",
        r#"<span class="pill pill-bad">Error</span>"#,
        r#"<p class="warn-text">nick in use &#60;script&#62;alert(1)&#60;/script&#62;</p>"#,
        "since <time datetime=\"2026-10-21T00:00:00Z\">2026-10-21 00:00:00 UTC</time>",
        r##"<a href="/admin/plugins#plugin-channel-libera">libera</a>"##,
        r#"<td data-label="State" class="m-aside"><span class="pill pill-bad">down</span></td>"#,
        r#"<td data-label="State" class="m-aside"><span class="pill pill-unknown">unknown</span></td>"#,
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(!html.contains(XSS), "{html}");
    // Nothing needs linking, so the page doesn't reload itself.
    assert!(!html.contains("http-equiv=\"refresh\""), "{html}");
    assert!(!html.contains(r#"<svg class="qr""#), "{html}");
}

#[tokio::test]
async fn a_channel_that_needs_linking_shows_its_qr_code_until_it_connects() {
    let env = env().await;
    let admin = Arc::new(FakeAdmin {
        status: Mutex::new(state()),
        ..FakeAdmin::default()
    });
    let payload = "2@QRPAYLOADref,noisekey,identitykey,advsecret";
    admin.set_channel_status(
        "whatsapp",
        State::NeedsAuth(NeedsAuth {
            challenge: Some(Challenge::QrCode(payload.into())),
            expires_at: at(1_792_540_820),
        }),
    );
    let (status, html) = env.admin_page(&admin, "/admin").await;
    assert_eq!(status, StatusCode::OK);
    let refresh = format!(r#"<meta http-equiv="refresh" content="{ADMIN_REFRESH_SECS}">"#);
    assert!(html.contains(&refresh), "{html}");
    let section = html.split(r#"id="channel-whatsapp""#).nth(1).unwrap();
    assert!(section.contains("Needs linking"), "{section}");
    assert!(section.contains(r#"<svg class="qr""#), "{section}");
    assert!(section.contains("2026-10-21 00:00:20 UTC"), "{section}");
    // The payload is drawn, never printed.
    assert!(!html.contains("QRPAYLOAD"), "{html}");
    assert_eq!(html.matches(r#"<svg class="qr""#).count(), 1);

    // The plugin replaced its code: the next load shows the new one.
    admin.set_channel_status(
        "whatsapp",
        State::NeedsAuth(NeedsAuth {
            challenge: Some(Challenge::QrCode("2@another,code".into())),
            expires_at: None,
        }),
    );
    let (_, next) = env.admin_page(&admin, "/admin").await;
    let svg = |page: &str| {
        let start = page.find(r#"<svg class="qr""#).unwrap();
        let end = start + page[start..].find("</svg>").unwrap();
        page[start..end].to_owned()
    };
    assert_ne!(svg(&html), svg(&next));

    // A pair code and a login URL are shown as text; only an http(s) URL is a link.
    admin.set_channel_status(
        "whatsapp",
        State::NeedsAuth(NeedsAuth {
            challenge: Some(Challenge::PairCode(format!("ABCD-1234{XSS}"))),
            expires_at: None,
        }),
    );
    let (_, html) = env.admin_page(&admin, "/admin").await;
    assert!(
        html.contains(r#"<p class="pair-code">ABCD-1234&#60;script&#62;"#),
        "{html}"
    );
    assert!(!html.contains(XSS), "{html}");
    admin.set_channel_status(
        "whatsapp",
        State::NeedsAuth(NeedsAuth {
            challenge: Some(Challenge::Url("javascript:alert(1)".into())),
            expires_at: None,
        }),
    );
    let (_, html) = env.admin_page(&admin, "/admin").await;
    assert!(html.contains("<code>javascript:alert(1)</code>"), "{html}");
    assert!(!html.contains("href=\"javascript:"), "{html}");

    // Once linked, no code and no reload.
    admin.set_channel_status("whatsapp", State::Connected(Connected {}));
    let (_, html) = env.admin_page(&admin, "/admin").await;
    assert!(!html.contains(r#"<svg class="qr""#), "{html}");
    assert!(!html.contains("http-equiv=\"refresh\""), "{html}");
}

#[tokio::test]
async fn the_plugins_page_shows_each_plugin_and_restart_buttons_for_supervised_ones() {
    let env = env().await;
    let admin = Arc::new(FakeAdmin {
        status: Mutex::new(state()),
        ..FakeAdmin::default()
    });
    let (status, html) = env.admin_page(&admin, "/admin/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let (libera, search) = html.split_once(r#"id="plugin-toolset-search""#).unwrap();
    for expected in [
        r#"id="plugin-channel-libera""#,
        "down, retrying at 2026-10-21 00:01:00 UTC; 3 restarts",
        "connection refused &#60;script&#62;",
        "0.1.0 &#60;script&#62;",
        "/opt/doozers/plugins/doozers-channel-irc",
        "<dd>error: nick in use",
        r#"<form method="post" action="/admin/plugins/channel/libera/restart">"#,
        &format!(r#"<input type="hidden" name="csrf" value="{CSRF}">"#),
    ] {
        assert!(libera.contains(expected), "{expected}\n{libera}");
    }
    assert!(search.contains("MCP server"), "{search}");
    assert!(search.contains("none (remote)"), "{search}");
    assert!(!search.contains("<form"), "{search}");
    assert!(!html.contains(XSS), "{html}");
}

#[tokio::test]
async fn the_plugins_page_shows_a_toolsets_state_and_a_restart_form() {
    let env = env().await;
    let admin = Arc::new(FakeAdmin {
        status: Mutex::new(state()),
        ..FakeAdmin::default()
    });
    let (status, html) = env.admin_page(&admin, "/admin/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let (_, rest) = html.split_once(r#"id="plugin-toolset-files""#).unwrap();
    let (files, rest) = rest.split_once(r#"id="plugin-toolset-broken""#).unwrap();
    let (broken, _) = rest.split_once(r#"id="plugin-toolset-search""#).unwrap();

    // A running toolset: state with its restarts, the binary found, the version, the tools it
    // offers, and no pid (stdio servers run under the sandbox trampoline).
    for expected in [
        "MCP server",
        "running, since 2026-10-21 00:01:40 UTC; 1 restarts",
        "/usr/local/bin/mcp-files",
        "<dd>2.1.0</dd>",
        "<dt>Tools</dt>",
        "<dd>4</dd>",
        r#"<form method="post" action="/admin/plugins/toolset/files/restart">"#,
        &format!(r#"<input type="hidden" name="csrf" value="{CSRF}">"#),
    ] {
        assert!(files.contains(expected), "{expected}\n{files}");
    }
    assert!(!files.contains("pid"), "{files}");

    // A failed one: why, that only a restart retries it, and the same restart form.
    for expected in [
        "failed (not retried until it",
        "HTTP 401 &#60;script&#62;",
        r#"<form method="post" action="/admin/plugins/toolset/broken/restart">"#,
    ] {
        assert!(broken.contains(expected), "{expected}\n{broken}");
    }
    assert!(!broken.contains("binary changes"), "{broken}");
    assert!(!html.contains(XSS), "{html}");

    // The status page's summary has the state word and the tool count.
    let (_, html) = env.admin_page(&admin, "/admin").await;
    for expected in [
        r##"<a href="/admin/plugins#plugin-toolset-files">files</a>"##,
        r#"<span class="pill pill-ok">running</span></td><td data-label="Tools" class="secondary m-meta m-label">4</td>"#,
        r#"<span class="pill pill-bad">failed</span></td><td data-label="Tools" class="secondary m-meta m-label">0</td>"#,
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
}

#[tokio::test]
async fn restarting_a_plugin_needs_the_csrf_token() {
    let env = env().await;
    let admin = Arc::new(FakeAdmin {
        status: Mutex::new(state()),
        ..FakeAdmin::default()
    });
    // Every answer, refusals and redirects included, is `no-store`.
    let restart = async |path: &str, headers: &[(&str, &str)], form: &str| {
        let response = call(
            env.admin_router(&admin, Some(owner())),
            Method::POST,
            path,
            headers,
            Some(form),
        )
        .await;
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        response
    };
    let libera = "/admin/plugins/channel/libera/restart";
    for (headers, form) in [
        (&[][..], ""),
        (&[][..], "csrf=wrong"),
        (&[("x-csrf-token", "wrong")][..], ""),
    ] {
        let response = restart(libera, headers, form).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{headers:?} {form}"
        );
    }
    assert!(admin.restarted().is_empty());

    let form = format!("csrf={CSRF}");
    let response = restart(libera, &[], &form).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/admin/plugins");
    let response = restart(libera, &[("x-csrf-token", CSRF)], "").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(admin.restarted(), ["channel/libera", "channel/libera"]);

    // The service's refusal is shown (escaped).
    let response = restart("/admin/plugins/channel/%3Cb%3Enope/restart", &[], &form).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let html = body_text(response).await;
    assert!(
        html.contains("there is no plugin instance `&#60;b&#62;nope`"),
        "{html}"
    );
    assert_eq!(admin.restarted(), ["channel/libera", "channel/libera"]);

    // A toolset's Restart goes to the same RPC, with its own kind.
    let response = restart("/admin/plugins/toolset/files/restart", &[], &form).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/admin/plugins");
    assert_eq!(
        admin.restarted(),
        ["channel/libera", "channel/libera", "toolset/files"]
    );

    // The kind is part of the route: `libera` is not a toolset, and there is no kind `plugin`.
    let response = restart("/admin/plugins/toolset/libera/restart", &[], &form).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = restart("/admin/plugins/plugin/libera/restart", &[], &form).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let html = body_text(response).await;
    assert!(
        html.contains("there is no kind of plugin called `plugin`"),
        "{html}"
    );
    assert_eq!(admin.restarted().len(), 3);
    // The old route, without a kind, is gone.
    let response = call(
        env.admin_router(&admin, Some(owner())),
        Method::POST,
        "/admin/plugins/libera/restart",
        &[],
        Some(&form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(admin.restarted().len(), 3);
}
