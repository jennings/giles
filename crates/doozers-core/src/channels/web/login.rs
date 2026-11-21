//! The web login (PLAN section 3.4): the authentication middleware, the login page, starting a
//! session, logout, and deleting expired sessions and invites. The invite and recovery links that
//! sign a browser in are in [`invite`](super::invite), and password sign-in in
//! [`password_pages`](super::password_pages).
//!
//! - A request through the trusted-proxy listener (marked [`TrustedProxy`]) that carries the
//!   proxy's identity header is the active user who has that `web` identity. Nothing else ever
//!   reads that header.
//! - Otherwise the session cookie, if it names a live `web_sessions` row of an active user who
//!   still has their `web` identity, is the user. A stale cookie is deleted in the response.
//! - Otherwise the request has no [`WebUser`] and the pages answer 401 with a pointer to the login
//!   page, `GET /login`, which lists the ways to sign in.

use std::time::Duration;

use askama::Template;
use axum::Extension;
use axum::Router;
use axum::extract::{Form, Request, State};
use axum::http::header::{HOST, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::SignInMethod;
use doozers_types::{NativeId, UserId};
use serde::Deserialize;
use tracing::{debug, warn};

use super::auth::{
    Login, SESSION_COOKIE, TrustedProxy, WEB_SESSION_TTL, WebUser, check_csrf,
    clear_session_cookie, cookie, cookie_csrf, nonce_cookie, nonce_for_form, proxy_csrf,
    session_cookie,
};
use super::pages::{WebApp, error_page, internal_error, no_store};
use super::signin::LoginOption;
use super::{COMPONENT, WebChannel, random_hex};
use crate::http;

/// How often expired web sessions are deleted.
pub const SESSION_EXPIRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The login routes: `GET /login` and `POST /logout`.
pub(super) fn routes() -> Router<WebApp> {
    Router::new()
        .route("/login", get(login_page))
        .route("/logout", post(log_out))
}

#[derive(Template)]
#[template(path = "web/login.html")]
struct LoginPage<'a> {
    options: Vec<LoginOption>,
    password: PasswordForm<'a>,
    /// Whether the password form sits behind "Use a password instead".
    collapse_password: bool,
}

/// What the password form on the login page shows.
struct PasswordForm<'a> {
    /// The sign-in nonce the form carries ([`nonce_for_form`]).
    nonce: &'a str,
    /// The username to put back in the form.
    username: &'a str,
    /// Whether to warn that the page isn't encrypted ([`WebApp::plain_http`]).
    plain_http: bool,
    /// Why the last attempt didn't sign in, if there was one.
    error: Option<&'a str>,
}

/// `GET /login`: the ways to sign in. Someone who is signed in goes to the chat list.
async fn login_page(
    State(app): State<WebApp>,
    user: Option<Extension<WebUser>>,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
) -> Response {
    if user.is_some() {
        return Redirect::to("/").into_response();
    }
    render_login(&app, &headers, trusted.is_some(), None, "")
}

/// The login page: the options, and the password form with `username` in it and `error` above it,
/// if any. It carries the sign-in nonce, which the response also sets as a cookie.
pub(super) fn render_login(
    app: &WebApp,
    headers: &HeaderMap,
    trusted: bool,
    error: Option<&str>,
    username: &str,
) -> Response {
    let nonce = nonce_for_form(headers);
    let mut response = http::render(&LoginPage {
        options: app.login_options(),
        password: PasswordForm {
            nonce: &nonce,
            username,
            plain_http: app.plain_http(headers, trusted),
            error,
        },
        collapse_password: app.password_collapsed(),
    });
    let https = is_https(&app.inner.channel, headers, trusted);
    response
        .headers_mut()
        .append(SET_COOKIE, nonce_cookie(&nonce, https));
    no_store(response)
}

/// Whether the response should mark cookies `Secure`: the public URL is `https`, or the trusted
/// proxy says the browser used HTTPS (`X-Forwarded-Proto`, believed only from the proxy).
pub(super) fn is_https(channel: &WebChannel, headers: &HeaderMap, trusted: bool) -> bool {
    channel.config().https()
        || (trusted
            && headers
                .get("x-forwarded-proto")
                .is_some_and(|proto| proto.as_bytes().eq_ignore_ascii_case(b"https")))
}

/// Whether `host`, a `Host` header or the host of a URL, with or without a port, is this machine:
/// `localhost` or a name under it, or a loopback IP address. A browser treats such a page as a
/// secure context, whatever the scheme.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let name = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl WebApp {
    /// Whether a page for this request should warn that it isn't encrypted: it is plain HTTP
    /// ([`is_https`] says no) and its host isn't this machine ([`is_loopback_host`]). The host is
    /// the request's `Host` header, or, when there is none, the address Doozers' links point to.
    pub(super) fn plain_http(&self, headers: &HeaderMap, trusted: bool) -> bool {
        let channel = &self.inner.channel;
        if is_https(channel, headers, trusted) {
            return false;
        }
        let host = match headers.get(HOST).and_then(|host| host.to_str().ok()) {
            Some(host) => host.to_owned(),
            None => {
                let base = channel.link_base();
                let authority = base
                    .split_once("://")
                    .map_or(base.as_str(), |(_, rest)| rest);
                authority.split('/').next().unwrap_or_default().to_owned()
            }
        };
        !is_loopback_host(&host)
    }
}

/// Who a request is, if anyone ([`WebUser`]); see the module docs.
pub(super) async fn authenticate(
    State(app): State<WebApp>,
    mut request: Request,
    next: Next,
) -> Response {
    let channel = &app.inner.channel;
    let trusted = request.extensions().get::<TrustedProxy>().is_some();
    let proxy_identity = channel
        .config()
        .proxy
        .as_ref()
        .filter(|_| trusted)
        .and_then(|proxy| request.headers().get(&proxy.identity_header));
    if let Some(value) = proxy_identity {
        let sender = value
            .to_str()
            .ok()
            .and_then(|value| NativeId::new(value.trim()).ok());
        let directory = channel.users().get();
        let known = sender
            .as_ref()
            .and_then(|id| directory.resolve(COMPONENT, id));
        let (Some(sender), Some(known)) = (sender, known) else {
            warn!(
                identity = ?value,
                "the trusted proxy named someone who isn't the web identity of any user",
            );
            return error_page(
                StatusCode::FORBIDDEN,
                "Not allowed",
                "The proxy in front of Doozers logged you in as someone Doozers doesn't know. An \
                 admin can set your identity as `web:<id>`; the log shows the id.",
            );
        };
        if !known.is_active() {
            warn!(user = %known.id, "the trusted proxy named a disabled user");
            return error_page(
                StatusCode::FORBIDDEN,
                "Not allowed",
                "Your account is disabled. Ask an admin.",
            );
        }
        request.extensions_mut().insert(WebUser {
            user: known.id.clone(),
            role: known.role,
            csrf: proxy_csrf(&channel.proxy_key, &sender),
            sender,
            login: Login::Proxy,
        });
        return next.run(request).await;
    }

    let mut stale = false;
    if let Some(token) = cookie(request.headers(), SESSION_COOKIE) {
        let now = app.inner.clock.now();
        match app.inner.store.web_session(token, now).await {
            Ok(Some(session)) => match channel.cookie_user(&session.user, token) {
                Some(user) => {
                    request.extensions_mut().insert(user);
                }
                None => stale = true,
            },
            Ok(None) => stale = true,
            Err(err) => return internal_error("cannot look up a web session", &err),
        }
    }
    let https = stale && is_https(channel, request.headers(), trusted);
    let mut response = next.run(request).await;
    if stale && !response.headers().contains_key(SET_COOKIE) {
        response
            .headers_mut()
            .append(SET_COOKIE, clear_session_cookie(https));
    }
    response
}

impl WebChannel {
    /// The user of a live cookie session for `user`, unless the user is gone or disabled, or
    /// `web:<user>` is no longer one of their identities (then the session no longer logs anyone
    /// in).
    fn cookie_user(&self, user: &UserId, token: &str) -> Option<WebUser> {
        let directory = self.users().get();
        let known = directory.user(user).filter(|known| known.is_active())?;
        if !self.is_web_identity(user) {
            return None;
        }
        Some(WebUser {
            user: user.clone(),
            role: known.role,
            sender: NativeId::new(user.as_str()).ok()?,
            csrf: cookie_csrf(token),
            login: Login::Cookie(token.to_owned()),
        })
    }
}

/// Signs `user` in, having proved who they are with `method`: stores a 30-day session (a new random
/// cookie token; the store keeps its SHA-256 and `method`), logs `previous`, the browser's old
/// cookie session if it had one, out, and answers 303 to `to` with the new session cookie. Every
/// way of signing in ends here.
pub(super) async fn start_session(
    app: &WebApp,
    user: UserId,
    method: SignInMethod,
    previous: Option<&WebUser>,
    https: bool,
    to: &str,
) -> Response {
    let now = app.inner.clock.now();
    let token = random_hex(32);
    let expires_at = now
        .checked_add(WEB_SESSION_TTL)
        .expect("30 days from now is a valid time");
    if let Err(err) = app
        .inner
        .store
        .web_session_create(&token, user, method, now, expires_at)
        .await
    {
        return internal_error("cannot store a web session", &err);
    }
    if let Some(WebUser {
        login: Login::Cookie(old),
        ..
    }) = previous
        && let Err(err) = app.inner.store.web_session_delete(old).await
    {
        warn!(error = %err, "cannot delete the replaced web session");
    }
    let mut response = Redirect::to(to).into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, session_cookie(&token, https));
    no_store(response)
}

#[derive(Debug, Deserialize)]
struct LogoutForm {
    #[serde(default)]
    csrf: Option<String>,
}

/// `POST /logout`: ends a cookie login (deletes its session and the cookie). A proxy login has
/// nothing to end: the proxy names the user on every request.
async fn log_out(
    State(app): State<WebApp>,
    user: WebUser,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    Form(form): Form<LogoutForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let mut response = Redirect::to("/").into_response();
    if let Login::Cookie(token) = &user.login {
        if let Err(err) = app.inner.store.web_session_delete(token).await {
            return internal_error("cannot delete a web session", &err);
        }
        let https = is_https(&app.inner.channel, &headers, trusted.is_some());
        response
            .headers_mut()
            .insert(SET_COOKIE, clear_session_cookie(https));
    }
    response
}

impl WebApp {
    /// Deletes expired web sessions and invites now and every hour until shutdown. Expired ones
    /// never log anyone in; this only keeps the tables small.
    pub async fn expire_sessions(self) {
        let mut ticks = tokio::time::interval(SESSION_EXPIRY_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = self.inner.shutdown.cancelled() => return,
                _ = ticks.tick() => {
                    let now = self.inner.clock.now();
                    match self.inner.store.web_sessions_expire(now).await {
                        Ok(0) => {}
                        Ok(deleted) => debug!(deleted, "deleted expired web sessions"),
                        Err(error) => warn!(%error, "cannot delete expired web sessions"),
                    }
                    match self.inner.store.user_invites_expire(now).await {
                        Ok(0) => {}
                        Ok(deleted) => debug!(deleted, "deleted expired invite links"),
                        Err(error) => warn!(%error, "cannot delete expired invite links"),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_password_form_can_sit_behind_use_a_password_instead() {
        let page = |collapse_password, error| {
            LoginPage {
                options: Vec::new(),
                password: PasswordForm {
                    nonce: "n",
                    username: "",
                    plain_http: false,
                    error,
                },
                collapse_password,
            }
            .render()
            .unwrap()
        };
        let shown = page(false, None);
        assert!(!shown.contains("<details"), "{shown}");
        assert!(shown.contains("<h2>Log in with a password</h2>"), "{shown}");
        assert!(shown.contains(r#"action="/login/password""#), "{shown}");

        // Folded away, the form is the same, and a failed attempt opens it again.
        let folded = page(true, None);
        assert!(
            folded.contains(r#"<details class="password-signin">"#),
            "{folded}"
        );
        assert!(
            folded.contains("<summary>Use a password instead</summary>"),
            "{folded}"
        );
        assert!(folded.contains(r#"action="/login/password""#), "{folded}");
        assert!(!folded.contains("Log in with a password"), "{folded}");
        let failed = page(true, Some("Wrong username or password."));
        assert!(
            failed.contains(r#"<details class="password-signin" open>"#),
            "{failed}"
        );
    }

    #[test]
    fn a_host_is_this_machine_when_it_is_loopback() {
        for host in [
            "localhost",
            "LOCALHOST:8080",
            "localhost.:80",
            "app.localhost",
            "127.0.0.1",
            "127.0.0.1:8080",
            "127.9.9.9",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "",
            "doozers.lan",
            "doozers.lan:8080",
            "192.168.0.2",
            "10.0.0.1:80",
            "[2001:db8::1]:80",
            "notlocalhost",
            "localhost.example.com",
            "0.0.0.0",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }
}
