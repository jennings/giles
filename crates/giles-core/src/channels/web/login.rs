//! The web login (PLAN section 3.4): the authentication middleware, the one-time login link pages,
//! logout, and deleting expired sessions.
//!
//! - A request through the trusted-proxy listener (marked [`TrustedProxy`]) that carries the
//!   proxy's identity header is the user that header names, if it's one of the owner's `web`
//!   identities. Nothing else ever reads that header.
//! - Otherwise the session cookie, if it names a live `web_sessions` row, is the user. A stale
//!   cookie is deleted in the response.
//! - Otherwise the request has no [`WebUser`] and the pages answer with the login page.

use std::time::Duration;

use askama::Template;
use axum::Extension;
use axum::Router;
use axum::extract::{Form, Path, Request, State};
use axum::http::header::{CACHE_CONTROL, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use giles_types::{NativeId, UserId};
use serde::Deserialize;
use tracing::{debug, info, warn};

use super::auth::{
    Login, SESSION_COOKIE, TrustedProxy, WEB_SESSION_TTL, WebUser, check_csrf,
    clear_session_cookie, cookie, cookie_csrf, proxy_csrf, session_cookie,
};
use super::pages::{WebApp, error_page, internal_error};
use super::{COMPONENT, WebChannel, links, random_hex};
use crate::http;
use crate::identity;

/// How often expired web sessions are deleted.
pub const SESSION_EXPIRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The login routes: `GET /login/<token>` (the link), `POST /login` (using it), and
/// `POST /logout`.
pub(super) fn routes() -> Router<WebApp> {
    Router::new()
        .route("/login/{token}", get(login_page))
        .route("/login", post(log_in))
        .route("/logout", post(log_out))
}

/// Whether the response should mark cookies `Secure`: the public URL is `https`, or the trusted
/// proxy says the browser used HTTPS (`X-Forwarded-Proto`, believed only from the proxy).
fn is_https(channel: &WebChannel, headers: &HeaderMap, trusted: bool) -> bool {
    channel.config().https()
        || (trusted
            && headers
                .get("x-forwarded-proto")
                .is_some_and(|proto| proto.as_bytes().eq_ignore_ascii_case(b"https")))
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
        let Some(sender) = sender.filter(|id| channel.owner.contains(COMPONENT, id)) else {
            warn!(
                identity = ?value,
                "the trusted proxy named someone who isn't one of the owner's web identities",
            );
            return error_page(
                StatusCode::FORBIDDEN,
                "Not allowed",
                "The proxy in front of Giles logged you in as someone Giles doesn't know. The \
                 operator can add your identity to `[owner] identities` as `web:<id>`; the log \
                 shows the id.",
            );
        };
        request.extensions_mut().insert(WebUser {
            user: identity::owner(),
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
    /// The user of a live cookie session for `user`, unless `web:<user>` is no longer one of the
    /// owner's identities (then the session no longer logs anyone in).
    fn cookie_user(&self, user: &UserId, token: &str) -> Option<WebUser> {
        if *user != identity::owner() || !self.is_web_identity(user) {
            return None;
        }
        Some(WebUser {
            user: user.clone(),
            sender: NativeId::new(user.as_str()).ok()?,
            csrf: cookie_csrf(token),
            login: Login::Cookie(token.to_owned()),
        })
    }
}

/// `Cache-Control: no-store`: the login page holds a token, and the login answer sets a cookie.
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The answer for a link that doesn't (or no longer) work: the same whether it never existed,
/// was used, or expired.
fn dead_link() -> Response {
    no_store(error_page(
        StatusCode::GONE,
        "Login link expired",
        "This login link has expired or was already used. Ask the operator for a new one \
         (`giles admin login-link --user owner`); each works once, for 10 minutes.",
    ))
}

#[derive(Template)]
#[template(path = "web/login.html")]
struct LoginPage<'a> {
    token: &'a str,
    expires: String,
}

/// `GET /login/<token>`: a page with a button that uses the link. Opening the link doesn't use it
/// up, so a chat app or mail client that fetches links for a preview can't spend it.
async fn login_page(State(app): State<WebApp>, Path(token): Path<String>) -> Response {
    let now = app.inner.clock.now();
    if !links::is_token(&token) {
        return dead_link();
    }
    let Some(expires_at) = app.inner.channel.links.expires_at(&token, now) else {
        return dead_link();
    };
    no_store(http::render(&LoginPage {
        token: &token,
        expires: expires_at.strftime("%Y-%m-%d %H:%M UTC").to_string(),
    }))
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    token: String,
}

/// `POST /login`: uses up the link and starts a 30-day session (a new random cookie token; the
/// store keeps its SHA-256). A session cookie the browser already had is logged out.
async fn log_in(
    State(app): State<WebApp>,
    user: Option<Extension<WebUser>>,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let now = app.inner.clock.now();
    let channel = &app.inner.channel;
    let Some(user_id) = links::is_token(&form.token)
        .then(|| channel.links.redeem(&form.token, now))
        .flatten()
    else {
        debug!("a dead login link was used");
        return dead_link();
    };
    let token = random_hex(32);
    let expires_at = now
        .checked_add(WEB_SESSION_TTL)
        .expect("30 days from now is a valid time");
    if let Err(err) = app
        .inner
        .store
        .web_session_create(&token, user_id.clone(), now, expires_at)
        .await
    {
        return internal_error("cannot store a web session", &err);
    }
    if let Some(Extension(WebUser {
        login: Login::Cookie(old),
        ..
    })) = user
        && let Err(err) = app.inner.store.web_session_delete(&old).await
    {
        warn!(error = %err, "cannot delete the replaced web session");
    }
    info!(user = %user_id, "logged in to the web chat with a login link");
    let https = is_https(channel, &headers, trusted.is_some());
    let mut response = Redirect::to("/").into_response();
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
    /// Deletes expired web sessions now and every hour until shutdown. Expired sessions never log
    /// anyone in; this only keeps the table small.
    pub async fn expire_sessions(self) {
        let mut ticks = tokio::time::interval(SESSION_EXPIRY_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = self.inner.shutdown.cancelled() => return,
                _ = ticks.tick() => {
                    match self.inner.store.web_sessions_expire(self.inner.clock.now()).await {
                        Ok(0) => {}
                        Ok(deleted) => debug!(deleted, "deleted expired web sessions"),
                        Err(error) => warn!(%error, "cannot delete expired web sessions"),
                    }
                }
            }
        }
    }
}
