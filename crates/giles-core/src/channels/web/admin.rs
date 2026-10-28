//! The web admin pages (PLAN sections 8.7 and 10): the daemon's status with every channel (and the
//! QR code, pair code, or URL of one that needs linking), and the plugins with a restart button.
//!
//! The pages are clients of the admin service, like `giles status`: they call the same
//! [`AdminService`](crate::admin::AdminService) in-process through its RPC trait, so they show
//! exactly what the CLI shows. Only the owner may see them (v1 has no other users; the `admin`
//! role comes with multi-user support), and the restart button is a CSRF-checked form post.
//! Everything a plugin reports (errors, versions, codes, URLs) is escaped by the templates; a QR
//! payload is only ever encoded into the code ([`qr`](super::qr)).

use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{from_fn_with_state, map_response};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use giles_plugin_sdk::convert::timestamp_from_proto;
use giles_proto::admin::v1 as pb;
use giles_proto::admin::v1::admin_service_server::AdminService as AdminRpc;
use giles_proto::channel::v1::channel_status::State as ChannelState;
use giles_proto::channel::v1::needs_auth::Challenge;
use giles_proto::channel::v1::{ChannelStatus, NeedsAuth};
use giles_proto::plugin::v1::SandboxGrants;
use serde::Deserialize;
use tonic::{Code, Request};
use tracing::error;

use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, error_page, no_store};
use super::qr;
use crate::http::{self, RouterBuilder};
use crate::identity;

/// How often the status page reloads itself while a channel needs linking, in seconds, so it
/// shows the newest QR code (WhatsApp replaces its code about every 20 seconds).
pub const ADMIN_REFRESH_SECS: u32 = 5;

/// The admin pages' state: the admin service.
#[derive(Clone)]
struct AdminPages {
    admin: Arc<dyn AdminRpc>,
}

impl WebApp {
    /// Adds the admin pages over `admin` (in `giles serve`, the [`AdminService`] the admin socket
    /// serves) to `builder`, behind the same authentication middleware as the chat pages. Every
    /// answer on these routes (pages, errors, redirects, the login page) is `no-store`: they show
    /// live state.
    ///
    /// [`AdminService`]: crate::admin::AdminService
    pub fn install_admin(&self, builder: RouterBuilder, admin: Arc<dyn AdminRpc>) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder.merge(
            Router::new()
                .route("/admin", get(status_page))
                .route("/admin/plugins", get(plugins_page))
                .route("/admin/plugins/{instance}/restart", post(restart))
                .with_state(AdminPages { admin })
                .route_layer(auth)
                .route_layer(map_response(async |response: Response| no_store(response))),
        )
    }
}

/// Whether `user` may see the admin pages (the agent pages too) and use their buttons: the owner
/// only.
pub(super) fn may_administer(user: &WebUser) -> bool {
    user.user == identity::owner()
}

pub(super) fn forbidden() -> Response {
    error_page(
        StatusCode::FORBIDDEN,
        "Not allowed",
        "Only the owner can see the admin pages.",
    )
}

/// The answer to a failed admin call: the service's message for what the operator can act on, a
/// logged 500 for anything else.
fn call_failed(rpc: &str, title: &str, status: &tonic::Status) -> Response {
    let code = match status.code() {
        Code::NotFound => StatusCode::NOT_FOUND,
        Code::FailedPrecondition => StatusCode::CONFLICT,
        Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        Code::InvalidArgument => StatusCode::BAD_REQUEST,
        _ => {
            error!(rpc, error = %status, "admin call from the web admin pages failed");
            return error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Something went wrong",
                "Giles couldn't do that. The error is in its log.",
            );
        }
    };
    error_page(code, title, status.message())
}

/// A time on the pages: shown in UTC, with the RFC 3339 form for `<time datetime>`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Time {
    text: String,
    iso: String,
}

/// `at` for the pages; `None` if unset or out of range.
fn time(at: Option<prost_types::Timestamp>) -> Option<Time> {
    let at = timestamp_from_proto(at?).ok()?;
    Some(Time {
        text: at.strftime("%Y-%m-%d %H:%M:%S UTC").to_string(),
        iso: at.to_string(),
    })
}

/// How to link a channel that reported `NeedsAuth`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Linking {
    /// The QR code as an `<svg>` element.
    qr: Option<String>,
    /// Set if the QR payload can't be drawn.
    qr_error: Option<String>,
    /// A code to enter on the device.
    pair_code: Option<String>,
    /// A URL to open, as reported.
    url: Option<String>,
    /// The URL again if it's `http` or `https`, so the page can link it.
    url_href: Option<String>,
    /// When the challenge stops working.
    expires: Option<Time>,
}

/// A channel's last status: a short text, a CSS class, and the linking challenge if any.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusView {
    text: String,
    class: &'static str,
    linking: Option<Linking>,
}

fn status_view(status: Option<&ChannelStatus>) -> StatusView {
    let view = |text: &str, class| StatusView {
        text: text.to_owned(),
        class,
        linking: None,
    };
    match status.and_then(|s| s.state.as_ref()) {
        None => view("no status yet", "unknown"),
        Some(ChannelState::Connected(_)) => view("connected", "ok"),
        Some(ChannelState::Error(message)) => view(&format!("error: {message}"), "bad"),
        Some(ChannelState::NeedsAuth(NeedsAuth {
            challenge,
            expires_at,
        })) => {
            let mut linking = Linking {
                qr: None,
                qr_error: None,
                pair_code: None,
                url: None,
                url_href: None,
                expires: time(*expires_at),
            };
            let text = match challenge {
                Some(Challenge::QrCode(payload)) => {
                    match qr::svg(payload) {
                        Ok(svg) => linking.qr = Some(svg),
                        Err(err) => {
                            linking.qr_error = Some(format!(
                                "The QR code can't be shown ({err}); `giles status` prints its \
                                 payload."
                            ));
                        }
                    }
                    "needs linking"
                }
                Some(Challenge::PairCode(code)) => {
                    linking.pair_code = Some(code.clone());
                    "needs linking"
                }
                Some(Challenge::Url(url)) => {
                    linking.url_href = url::Url::parse(url)
                        .ok()
                        .filter(|u| matches!(u.scheme(), "http" | "https"))
                        .map(|_| url.clone());
                    linking.url = Some(url.clone());
                    "needs login"
                }
                None => "needs login",
            };
            StatusView {
                text: text.to_owned(),
                class: "warn",
                linking: Some(linking),
            }
        }
    }
}

/// A configured channel on the status page.
#[derive(Debug)]
struct ChannelRow {
    instance: String,
    component: String,
    runtime: &'static str,
    status: StatusView,
    since: Option<Time>,
}

fn channel_row(channel: &pb::ChannelInstance) -> ChannelRow {
    let runtime = match channel.runtime() {
        pb::ChannelRuntime::BuiltIn => "built in",
        pb::ChannelRuntime::Plugin => "plugin",
        pb::ChannelRuntime::Remote => "remote",
        pb::ChannelRuntime::Unspecified => "unknown",
    };
    ChannelRow {
        instance: channel.instance.clone(),
        component: channel.component.clone(),
        runtime,
        status: status_view(channel.status.as_ref()),
        since: time(channel.status_at),
    }
}

/// A plugin instance, as the plugins page (and the summary on the status page) shows it.
#[derive(Debug)]
struct PluginRow {
    instance: String,
    kind: &'static str,
    component: String,
    /// One word for the summary table.
    state_word: &'static str,
    /// The state with its details (pid, since, retry time, restarts).
    state: String,
    class: &'static str,
    error: Option<String>,
    /// A channel plugin's last status.
    channel: Option<String>,
    location: String,
    version: Option<String>,
    /// How many tools a toolset offers to new sessions.
    tools: Option<u32>,
    sandbox: String,
    lacking: Vec<String>,
    requests: Option<String>,
    /// Whether plugin supervision knows the instance, so `RestartPlugin` can restart it.
    restartable: bool,
}

fn plugin_row(plugin: &pb::PluginInstance) -> PluginRow {
    let kind = match plugin.kind() {
        pb::InstanceKind::Provider => "provider",
        pb::InstanceKind::Channel => "channel",
        pb::InstanceKind::Toolset => "toolset",
        pb::InstanceKind::Unspecified => "unknown kind",
    };
    let (state_word, class, mut state) = match plugin.state() {
        pb::PluginState::Unspecified => (
            "unknown",
            "unknown",
            "unknown (plugin supervision doesn't report it)".to_owned(),
        ),
        pb::PluginState::Starting => ("starting", "warn", "starting".to_owned()),
        pb::PluginState::Running => {
            let mut s = "running".to_owned();
            if let Some(pid) = plugin.pid {
                s.push_str(&format!(", pid {pid}"));
            }
            if let Some(since) = time(plugin.started_at) {
                s.push_str(&format!(", since {}", since.text));
            }
            ("running", "ok", s)
        }
        pb::PluginState::Idle => ("idle", "ok", "idle (starts on the next call)".to_owned()),
        pb::PluginState::Backoff => (
            "down",
            "bad",
            match time(plugin.retry_at) {
                Some(at) => format!("down, retrying at {}", at.text),
                None => "down, retrying".to_owned(),
            },
        ),
        pb::PluginState::Failed => (
            "failed",
            "bad",
            if plugin.kind() == pb::InstanceKind::Toolset {
                "failed (not retried until it's restarted)"
            } else {
                "failed (not retried until its binary changes or it's restarted)"
            }
            .to_owned(),
        ),
        pb::PluginState::Stopped => ("stopped", "unknown", "stopped".to_owned()),
    };
    if plugin.restarts > 0 {
        state.push_str(&format!("; {} restarts", plugin.restarts));
    }
    let channel = (plugin.kind() == pb::InstanceKind::Channel)
        .then(|| status_view(plugin.channel_status.as_ref()).text);
    let mut lacking = Vec::new();
    let sandbox = match &plugin.sandbox {
        None => "none (remote)".to_owned(),
        Some(sandbox) if !sandbox.enabled => "off (unsandboxed)".to_owned(),
        Some(sandbox) => {
            lacking.clone_from(&sandbox.missing);
            match sandbox.landlock_abi {
                Some(abi) => format!("on, Landlock ABI {abi}"),
                None => "on".to_owned(),
            }
        }
    };
    PluginRow {
        instance: plugin.instance.clone(),
        kind,
        component: plugin.component.clone(),
        state_word,
        state,
        class,
        error: plugin.last_error.clone(),
        channel,
        location: plugin.location.clone(),
        version: plugin.version.clone(),
        tools: plugin.tool_count,
        sandbox,
        lacking,
        requests: plugin.requested_grants.as_ref().map(requested_grants),
        restartable: plugin.state() != pb::PluginState::Unspecified,
    }
}

/// The grants a plugin asked for in `Describe`.
fn requested_grants(grants: &SandboxGrants) -> String {
    let mut parts = Vec::new();
    if grants.network_all {
        parts.push("network: all".to_owned());
    } else if !grants.network_ports.is_empty() {
        parts.push(format!("network: {}", grants.network_ports.join(", ")));
    }
    for (name, paths) in [("read", &grants.read), ("write", &grants.write)] {
        if !paths.is_empty() {
            parts.push(format!("{name}: {}", paths.join(", ")));
        }
    }
    if parts.is_empty() {
        "nothing".to_owned()
    } else {
        parts.join("; ")
    }
}

#[derive(Template)]
#[template(path = "web/admin_status.html")]
struct StatusPage {
    version: String,
    started: Option<Time>,
    sandbox: String,
    sandbox_ok: bool,
    config_error: Option<String>,
    channels: Vec<ChannelRow>,
    plugins: Vec<PluginRow>,
    /// Some channel needs linking: reload every [`ADMIN_REFRESH_SECS`].
    refresh: bool,
    refresh_secs: u32,
}

impl StatusPage {
    fn new(status: &pb::StatusResponse) -> Self {
        let channels: Vec<ChannelRow> = status.channels.iter().map(channel_row).collect();
        let (sandbox_ok, sandbox) = match &status.sandbox_host {
            Some(host) if host.available => (true, host.description.clone()),
            Some(host) => (false, format!("unavailable: {}", host.description)),
            None => (false, "unknown".to_owned()),
        };
        Self {
            version: status.version.clone(),
            started: time(status.started_at),
            sandbox,
            sandbox_ok,
            config_error: status.config_error.clone(),
            refresh: channels.iter().any(|c| c.status.linking.is_some()),
            channels,
            plugins: status.plugins.iter().map(plugin_row).collect(),
            refresh_secs: ADMIN_REFRESH_SECS,
        }
    }
}

/// `GET /admin`: the daemon, every channel with its status (and how to link one that needs it),
/// and a summary of the plugins. Reloads itself while a channel needs linking.
async fn status_page(State(pages): State<AdminPages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    match pages.admin.status(Request::new(pb::StatusRequest {})).await {
        Ok(status) => http::render(&StatusPage::new(status.get_ref())),
        Err(status) => call_failed("Status", "Status unavailable", &status),
    }
}

#[derive(Template)]
#[template(path = "web/admin_plugins.html")]
struct PluginsPage {
    plugins: Vec<PluginRow>,
    csrf: String,
}

/// `GET /admin/plugins`: every plugin instance with what supervision knows, and a restart button
/// for each supervised one.
async fn plugins_page(State(pages): State<AdminPages>, user: WebUser) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    match pages
        .admin
        .list_plugins(Request::new(pb::ListPluginsRequest {}))
        .await
    {
        Ok(listed) => http::render(&PluginsPage {
            plugins: listed.get_ref().plugins.iter().map(plugin_row).collect(),
            csrf: user.csrf,
        }),
        Err(status) => call_failed("ListPlugins", "Plugins unavailable", &status),
    }
}

#[derive(Debug, Deserialize)]
struct RestartForm {
    csrf: Option<String>,
}

/// `POST /admin/plugins/<instance>/restart` (CSRF-checked): `RestartPlugin`, then 303 back to the
/// plugins page.
async fn restart(
    State(pages): State<AdminPages>,
    user: WebUser,
    Path(instance): Path<String>,
    headers: HeaderMap,
    Form(form): Form<RestartForm>,
) -> Response {
    if !may_administer(&user) {
        return forbidden();
    }
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    match pages
        .admin
        .restart_plugin(Request::new(pb::RestartPluginRequest { instance }))
        .await
    {
        Ok(_) => Redirect::to("/admin/plugins").into_response(),
        Err(status) => call_failed("RestartPlugin", "Restart failed", &status),
    }
}

#[cfg(test)]
mod tests {
    use giles_proto::channel::v1::Connected;

    use super::*;

    fn needs_auth(challenge: Challenge) -> ChannelStatus {
        ChannelStatus {
            state: Some(ChannelState::NeedsAuth(NeedsAuth {
                challenge: Some(challenge),
                expires_at: Some(prost_types::Timestamp {
                    seconds: 1_792_540_800,
                    nanos: 0,
                }),
            })),
        }
    }

    #[test]
    fn statuses_in_words() {
        assert_eq!(status_view(None).text, "no status yet");
        let connected = ChannelStatus {
            state: Some(ChannelState::Connected(Connected {})),
        };
        assert_eq!(status_view(Some(&connected)).class, "ok");
        assert_eq!(status_view(Some(&connected)).linking, None);
        let error = ChannelStatus {
            state: Some(ChannelState::Error("bad password".into())),
        };
        assert_eq!(status_view(Some(&error)).text, "error: bad password");
    }

    #[test]
    fn a_qr_challenge_becomes_a_code_with_its_expiry() {
        let view = status_view(Some(&needs_auth(Challenge::QrCode("2@abc,def".into()))));
        assert_eq!(view.text, "needs linking");
        let linking = view.linking.unwrap();
        assert!(linking.qr.unwrap().starts_with("<svg"));
        assert_eq!(linking.expires.unwrap().text, "2026-10-21 00:00:00 UTC");

        let view = status_view(Some(&needs_auth(Challenge::QrCode("x".repeat(4000)))));
        let linking = view.linking.unwrap();
        assert_eq!(linking.qr, None);
        assert!(linking.qr_error.unwrap().contains("giles status"));
    }

    #[test]
    fn only_http_urls_are_linked() {
        for (url, linked) in [
            ("https://example.com/link?c=1", true),
            ("http://127.0.0.1:9/x", true),
            ("javascript:alert(1)", false),
            ("data:text/html,<b>x</b>", false),
            ("not a url", false),
        ] {
            let linking = status_view(Some(&needs_auth(Challenge::Url(url.into()))))
                .linking
                .unwrap();
            assert_eq!(linking.url.as_deref(), Some(url));
            assert_eq!(linking.url_href.is_some(), linked, "{url}");
        }
    }

    #[test]
    fn plugin_states_and_restartability() {
        let mut plugin = pb::PluginInstance {
            instance: "libera".into(),
            kind: pb::InstanceKind::Channel.into(),
            component: "irc".into(),
            state: pb::PluginState::Running.into(),
            pid: Some(42),
            restarts: 2,
            sandbox: Some(pb::SandboxStatus {
                enabled: true,
                landlock_abi: Some(6),
                missing: vec!["no TCP port restrictions".into()],
            }),
            requested_grants: Some(SandboxGrants {
                network_ports: vec!["tcp:6697".into()],
                ..SandboxGrants::default()
            }),
            ..pb::PluginInstance::default()
        };
        let row = plugin_row(&plugin);
        assert_eq!(row.state, "running, pid 42; 2 restarts");
        assert_eq!(row.channel.as_deref(), Some("no status yet"));
        assert_eq!(row.sandbox, "on, Landlock ABI 6");
        assert_eq!(row.lacking, ["no TCP port restrictions"]);
        assert_eq!(row.requests.as_deref(), Some("network: tcp:6697"));
        assert!(row.restartable);

        plugin.kind = pb::InstanceKind::Toolset.into();
        plugin.state = pb::PluginState::Unspecified.into();
        plugin.sandbox = None;
        let row = plugin_row(&plugin);
        assert_eq!(row.channel, None);
        assert_eq!(row.sandbox, "none (remote)");
        assert!(!row.restartable);

        // A supervised toolset: its state, the tools it offers, and a restart for a failed one.
        plugin.state = pb::PluginState::Failed.into();
        plugin.tool_count = Some(0);
        let row = plugin_row(&plugin);
        assert_eq!(
            row.state,
            "failed (not retried until it's restarted); 2 restarts"
        );
        assert_eq!(row.tools, Some(0));
        assert!(row.restartable);
        plugin.kind = pb::InstanceKind::Channel.into();
        assert_eq!(
            plugin_row(&plugin).state,
            "failed (not retried until its binary changes or it's restarted); 2 restarts"
        );
    }
}
