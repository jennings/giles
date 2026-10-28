//! Router-level tests of the admin pages over a fake admin service.

use std::sync::Mutex;

use giles_proto::admin::v1 as pb;
use giles_proto::admin::v1::admin_service_server::AdminService as AdminRpc;
use giles_proto::channel::v1::channel_status::State;
use giles_proto::channel::v1::needs_auth::Challenge;
use giles_proto::channel::v1::{ChannelStatus as PbStatus, Connected, NeedsAuth};
use tonic::{Request, Response as RpcResponse, Status};

use super::*;

/// The admin service's answers, from a state the test sets. `RestartPlugin` records the instance
/// (`NOT_FOUND` for one `Status` doesn't list).
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
        let instance = request.into_inner().instance;
        let listed = self
            .status
            .lock()
            .unwrap()
            .plugins
            .iter()
            .any(|p| p.instance == instance);
        if !listed {
            return Err(Status::not_found(format!(
                "there is no plugin instance `{instance}`"
            )));
        }
        self.restarted.lock().unwrap().push(instance);
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
                location: "/opt/giles/plugins/giles-channel-irc".into(),
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
        sender: NativeId::new("guest").unwrap(),
        csrf: CSRF.into(),
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
async fn only_the_owner_gets_the_admin_pages() {
    let env = env().await;
    let admin = Arc::new(FakeAdmin {
        status: Mutex::new(state()),
        ..FakeAdmin::default()
    });
    let cases = [
        (Method::GET, "/admin"),
        (Method::GET, "/admin/plugins"),
        (Method::POST, "/admin/plugins/libera/restart"),
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
        "giles 1.2.3",
        "Seatbelt (sandbox-exec)",
        r#"<section class="admin-item" id="channel-libera">"#,
        "irc, plugin",
        "web, built in",
        "error: nick in use &#60;script&#62;alert(1)&#60;/script&#62;",
        "since <time datetime=\"2026-10-21T00:00:00Z\">2026-10-21 00:00:00 UTC</time>",
        r##"<a href="/admin/plugins#plugin-libera">libera</a>"##,
        r#"<td class="state-bad">down</td>"#,
        r#"<td class="state-unknown">unknown</td>"#,
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(!html.contains(XSS), "{html}");
    // Nothing needs linking, so the page doesn't reload itself.
    assert!(!html.contains("http-equiv=\"refresh\""), "{html}");
    assert!(!html.contains("<svg"), "{html}");
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
    assert!(section.contains("needs linking"), "{section}");
    assert!(section.contains(r#"<svg class="qr""#), "{section}");
    assert!(section.contains("2026-10-21 00:00:20 UTC"), "{section}");
    // The payload is drawn, never printed.
    assert!(!html.contains("QRPAYLOAD"), "{html}");
    assert_eq!(html.matches("<svg").count(), 1);

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
        let start = page.find("<svg").unwrap();
        page[start..page.find("</svg>").unwrap()].to_owned()
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
    assert!(!html.contains("<svg"), "{html}");
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
    let (libera, search) = html.split_once(r#"id="plugin-search""#).unwrap();
    for expected in [
        "down, retrying at 2026-10-21 00:01:00 UTC; 3 restarts",
        "connection refused &#60;script&#62;",
        "0.1.0 &#60;script&#62;",
        "/opt/giles/plugins/giles-channel-irc",
        "<dd>error: nick in use",
        r#"<form method="post" action="/admin/plugins/libera/restart">"#,
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
    let (_, rest) = html.split_once(r#"id="plugin-files""#).unwrap();
    let (files, rest) = rest.split_once(r#"id="plugin-broken""#).unwrap();
    let (broken, _) = rest.split_once(r#"id="plugin-search""#).unwrap();

    // A running toolset: state with its restarts, the binary found, the version, the tools it
    // offers, and no pid (stdio servers run under the sandbox trampoline).
    for expected in [
        "MCP server",
        "running, since 2026-10-21 00:01:40 UTC; 1 restarts",
        "/usr/local/bin/mcp-files",
        "<dd>2.1.0</dd>",
        "<dt>Tools</dt>",
        "<dd>4</dd>",
        r#"<form method="post" action="/admin/plugins/files/restart">"#,
        &format!(r#"<input type="hidden" name="csrf" value="{CSRF}">"#),
    ] {
        assert!(files.contains(expected), "{expected}\n{files}");
    }
    assert!(!files.contains("pid"), "{files}");

    // A failed one: why, that only a restart retries it, and the same restart form.
    for expected in [
        "failed (not retried until it",
        "HTTP 401 &#60;script&#62;",
        r#"<form method="post" action="/admin/plugins/broken/restart">"#,
    ] {
        assert!(broken.contains(expected), "{expected}\n{broken}");
    }
    assert!(!broken.contains("binary changes"), "{broken}");
    assert!(!html.contains(XSS), "{html}");

    // The status page's summary has the state word and the tool count.
    let (_, html) = env.admin_page(&admin, "/admin").await;
    for expected in [
        r##"<a href="/admin/plugins#plugin-files">files</a>"##,
        r#"<td class="state-ok">running</td><td>4</td>"#,
        r#"<td class="state-bad">failed</td><td>0</td>"#,
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
    let libera = "/admin/plugins/libera/restart";
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
    assert_eq!(admin.restarted(), ["libera", "libera"]);

    // The service's refusal is shown (escaped).
    let response = restart("/admin/plugins/%3Cb%3Enope/restart", &[], &form).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let html = body_text(response).await;
    assert!(
        html.contains("there is no plugin instance `&#60;b&#62;nope`"),
        "{html}"
    );
    assert_eq!(admin.restarted(), ["libera", "libera"]);

    // A toolset's Restart goes to the same RPC.
    let response = restart("/admin/plugins/files/restart", &[], &form).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/admin/plugins");
    assert_eq!(admin.restarted(), ["libera", "libera", "files"]);
}
