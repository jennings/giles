//! The web login (PLAN section 3.4): the authentication middleware, starting a session, logout, and
//! deleting expired sessions and invites. The invite and recovery links that sign a browser in are
//! in [`invite`](super::invite).
//!
//! - A request through the trusted-proxy listener (marked [`TrustedProxy`]) that carries the
//!   proxy's identity header is the active user who has that `web` identity. Nothing else ever
//!   reads that header.
//! - Otherwise the session cookie, if it names a live `web_sessions` row of an active user who
//!   still has their `web` identity, is the user. A stale cookie is deleted in the response.
//! - Otherwise the request has no [`WebUser`] and the pages answer with the login page.

use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::extract::{Form, Request, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use doozers_store::SignInMethod;
use doozers_types::{NativeId, UserId};
use serde::Deserialize;
use tracing::{debug, warn};

use super::auth::{
    Login, SESSION_COOKIE, TrustedProxy, WEB_SESSION_TTL, WebUser, check_csrf,
    clear_session_cookie, cookie, cookie_csrf, proxy_csrf, session_cookie,
};
use super::pages::{WebApp, error_page, internal_error, no_store};
use super::{COMPONENT, WebChannel, random_hex};

/// How often expired web sessions are deleted.
pub const SESSION_EXPIRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The login routes: `POST /logout`.
pub(super) fn routes() -> Router<WebApp> {
    Router::new().route("/logout", post(log_out))
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
