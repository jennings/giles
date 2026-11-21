//! Invite and recovery links (PLAN section 3.4), and the setup page a user lands on after
//! spending one. Design: `Docs/technical/web-channel.md`.
//!
//! - `GET /invite/<token>` shows a page with a button. Opening the link doesn't spend it, so a
//!   chat app or mail client that fetches links for a preview can't.
//! - `POST /invite` spends the link and signs the browser in, once. Like every sign-in POST it
//!   needs the nonce its form carried to match the browser's nonce cookie (login CSRF).
//! - `GET /setup` is where the user goes next, to add a way to sign in.

use askama::Template;
use axum::Extension;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use doozers_store::{InviteKind, SignInMethod};
use serde::Deserialize;
use tracing::{debug, info, warn};

use super::auth::{
    TrustedProxy, WebUser, check_nonce, clear_nonce_cookie, nonce_cookie, nonce_for_form,
};
use super::login::{is_https, start_session};
use super::pages::{WebApp, error_page, internal_error, no_store};
use super::signin::SetupOption;
use crate::http;
use crate::users::is_invite_token;

/// The invite routes: `GET /invite/<token>`, `POST /invite`, and `GET /setup`.
pub(super) fn routes() -> Router<WebApp> {
    Router::new()
        .route("/invite/{token}", get(invite_page))
        .route("/invite", post(spend_invite))
        .route("/setup", get(setup_page))
}

/// The answer for a link that doesn't (or no longer) work: the same whether it never existed,
/// was used, or expired.
fn dead_link() -> Response {
    no_store(error_page(
        StatusCode::GONE,
        "Link expired",
        "This link has expired or was already used. Ask the person who sent it for a new one. If \
         you run Doozers, `doozers admin login-link` makes one; each works once, for 72 hours.",
    ))
}

#[derive(Template)]
#[template(path = "web/invite.html")]
struct InvitePage<'a> {
    token: &'a str,
    nonce: &'a str,
    name: &'a str,
    recovery: bool,
    expires: String,
}

/// `GET /invite/<token>`: a page with a button that spends the link, and the nonce the button's
/// form carries (also set as a cookie).
async fn invite_page(
    State(app): State<WebApp>,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> Response {
    if !is_invite_token(&token) {
        return dead_link();
    }
    let now = app.inner.clock.now();
    let invite = match app.inner.store.user_invite(&token, now).await {
        Ok(Some(invite)) => invite,
        Ok(None) => return dead_link(),
        Err(err) => return internal_error("cannot look up an invite", &err),
    };
    let name = app
        .inner
        .channel
        .users()
        .get()
        .user(&invite.user)
        .map(|user| user.display_name.clone())
        .unwrap_or_default();
    let nonce = nonce_for_form(&headers);
    let mut response = http::render(&InvitePage {
        token: &token,
        nonce: &nonce,
        name: &name,
        recovery: invite.kind == InviteKind::Recovery,
        expires: invite.expires_at.strftime("%Y-%m-%d %H:%M UTC").to_string(),
    });
    let https = is_https(&app.inner.channel, &headers, trusted.is_some());
    response
        .headers_mut()
        .append(SET_COOKIE, nonce_cookie(&nonce, https));
    no_store(response)
}

#[derive(Debug, Deserialize)]
struct InviteForm {
    token: String,
    #[serde(default)]
    nonce: Option<String>,
}

/// `POST /invite`: spends the link and starts a 30-day session for its user, then 303 to
/// `/setup`. A browser that already had a cookie session is logged out of it. The link stays
/// unspent when the nonce doesn't match, and when its user can't sign in.
async fn spend_invite(
    State(app): State<WebApp>,
    user: Option<Extension<WebUser>>,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    Form(form): Form<InviteForm>,
) -> Response {
    if let Err(rejected) = check_nonce(&headers, form.nonce.as_deref()) {
        return rejected.into_response();
    }
    if !is_invite_token(&form.token) {
        debug!("a malformed invite link was used");
        return dead_link();
    }
    let channel = &app.inner.channel;
    let now = app.inner.clock.now();
    match app.inner.store.user_invite(&form.token, now).await {
        Ok(Some(invite)) if !channel.can_sign_in(&invite.user) => {
            warn!(user = %invite.user, "an invite link was used for a user who can't sign in");
            return no_store(error_page(
                StatusCode::FORBIDDEN,
                "Not allowed",
                "This account can't sign in to the web chat. Ask an admin.",
            ));
        }
        Ok(Some(_)) => {}
        Ok(None) => {
            debug!("a dead invite link was used");
            return dead_link();
        }
        Err(err) => return internal_error("cannot look up an invite", &err),
    }
    let invite = match app.inner.store.user_invite_redeem(&form.token, now).await {
        Ok(Some(invite)) => invite,
        Ok(None) => {
            debug!("an invite link was spent by another request first");
            return dead_link();
        }
        Err(err) => return internal_error("cannot spend an invite", &err),
    };
    let method = match invite.kind {
        InviteKind::Invite => SignInMethod::Invite,
        InviteKind::Recovery => SignInMethod::Recovery,
    };
    info!(user = %invite.user, method = method.as_str(), "signed in to the web chat with a link");
    let https = is_https(channel, &headers, trusted.is_some());
    let mut response = start_session(
        &app,
        invite.user,
        method,
        user.as_ref().map(|Extension(user)| user),
        https,
        "/setup",
    )
    .await;
    response
        .headers_mut()
        .append(SET_COOKIE, clear_nonce_cookie(https));
    no_store(response)
}

#[derive(Template)]
#[template(path = "web/setup.html")]
struct SetupPage {
    name: String,
    options: Vec<SetupOption>,
}

/// `GET /setup`: the sign-in methods the user can add, and, until there are any, what signing in
/// with a link means.
async fn setup_page(State(app): State<WebApp>, user: WebUser) -> Response {
    let name = app
        .inner
        .channel
        .users()
        .get()
        .user(&user.user)
        .map(|found| found.display_name.clone())
        .unwrap_or_default();
    let options = app.setup_options(&user);
    no_store(http::render(&SetupPage { name, options }))
}
