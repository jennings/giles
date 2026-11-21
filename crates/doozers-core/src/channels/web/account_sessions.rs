//! The web sessions part of the Account page: the user's own logins, how each signed in, and
//! revoking one. Design: `Docs/technical/web-channel.md#account-page`.
//!
//! A session is named by the opaque id the store gives it (`web_sessions.id`), never by its
//! cookie token or the hash the store keeps of it, and neither is ever on a page. Listing and
//! revoking are scoped to the user in the store, so another user's session id is no session: a
//! revoke of it is a 404, as is one that doesn't exist. Revoking the session the request came in on
//! ends the login, as logging out does. Revoking a session needs no confirmation: it removes
//! access and adds none.

use axum::Extension;
use axum::extract::{Form, Path, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use doozers_store::SignInMethod;
use serde::Deserialize;
use tracing::info;

use super::account::AccountPages;
use super::auth::{Login, TrustedProxy, WebUser, check_csrf, clear_session_cookie};
use super::login::is_https;
use super::pages::{WebApp, error_page, internal_error};
use super::toolsets::when;

/// A session on the page.
pub(super) struct SessionRow {
    pub(super) id: String,
    /// How it signed in, in words.
    pub(super) how: &'static str,
    pub(super) started: String,
    pub(super) last_seen: String,
    pub(super) expires: String,
    /// Whether it is the session the page was asked for in.
    pub(super) current: bool,
}

/// How a session signed in, as the page says it.
fn how(method: SignInMethod) -> &'static str {
    match method {
        SignInMethod::Invite => "Invite link",
        SignInMethod::Recovery => "Recovery link",
        SignInMethod::Password => "Password",
        SignInMethod::Passkey => "Passkey",
        SignInMethod::Oidc => "Identity provider",
    }
}

impl WebApp {
    /// The user's live sessions, newest first, with the one `user` is using marked.
    pub(super) async fn sessions_view(
        &self,
        user: &WebUser,
    ) -> doozers_store::Result<Vec<SessionRow>> {
        let now = self.inner.clock.now();
        let current = self.current_session_id(user).await?;
        Ok(self
            .inner
            .store
            .web_sessions_of(&user.user, now)
            .await?
            .into_iter()
            .map(|session| SessionRow {
                how: how(session.method),
                started: when(session.created_at),
                last_seen: session
                    .last_seen_at
                    .map_or_else(|| "not since it began".to_owned(), when),
                expires: when(session.expires_at),
                current: current.as_deref() == Some(session.id.as_str()),
                id: session.id,
            })
            .collect())
    }

    /// The id of the session `user`'s request came in on, if it was a cookie login.
    async fn current_session_id(&self, user: &WebUser) -> doozers_store::Result<Option<String>> {
        let Login::Cookie(token) = &user.login else {
            return Ok(None);
        };
        Ok(self
            .inner
            .store
            .web_session(token, self.inner.clock.now())
            .await?
            .map(|session| session.id))
    }
}

/// Whether `id` is shaped like a session id: 32 lowercase hex digits.
fn is_session_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn no_such_session() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "No such login",
        "You have no such login. It may have ended already.",
    )
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct RevokeForm {
    #[serde(default)]
    csrf: Option<String>,
}

/// `POST /account/sessions/<id>/revoke` (CSRF-checked): ends the user's session `id`, then 303 to
/// the account page. If it is the one the request came in on, it also deletes the cookie and goes to
/// `/`, as logging out does. Another user's id, and one that doesn't exist, are a 404.
pub(super) async fn revoke(
    State(pages): State<AccountPages>,
    user: WebUser,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Form(form): Form<RevokeForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    if !is_session_id(&id) {
        return no_such_session();
    }
    let app = &pages.app;
    let current = match app.current_session_id(&user).await {
        Ok(current) => current,
        Err(err) => return internal_error("cannot look up a web session", &err),
    };
    match app.inner.store.web_session_revoke(&user.user, &id).await {
        Ok(true) => {}
        Ok(false) => return no_such_session(),
        Err(err) => return internal_error("cannot revoke a web session", &err),
    }
    info!(user = %user.user, "revoked a web session");
    if current.as_deref() == Some(id.as_str()) {
        let https = is_https(&app.inner.channel, &headers, trusted.is_some());
        let mut response = Redirect::to("/").into_response();
        response
            .headers_mut()
            .insert(SET_COOKIE, clear_session_cookie(https));
        return response;
    }
    Redirect::to("/account?notice=revoked").into_response()
}
