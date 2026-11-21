//! The pages of password sign-in (PLAN section 3.4): logging in with a username and password, and
//! setting or changing the password on the setup page. The rules and the hashing are in
//! [`password`](super::password), the limits on guessing in [`limits`](super::limits). Design:
//! `Docs/technical/web-channel.md`.
//!
//! - `POST /login/password` signs in. The login page ([`login`](super::login)) holds its form.
//! - `GET` and `POST /setup/password` set the password. Only a browser that signed in with an
//!   invite or recovery link may: those are how a user proves who they are when they have no
//!   password to prove it with, and a 30-day session that came from anything else isn't enough to
//!   take over the account's way in.

use askama::Template;
use axum::Extension;
use axum::Router;
use axum::extract::{ConnectInfo, Form, Query, State};
use axum::http::header::{RETRY_AFTER, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::SignInMethod;
use doozers_types::UserId;
use serde::Deserialize;
use tracing::{debug, info, warn};

use super::auth::{TrustedProxy, WebUser, check_csrf, check_nonce, clear_nonce_cookie};
use super::limits::{Limited, client_key, username_key};
use super::login::{is_https, render_login, start_session};
use super::pages::{WebApp, error_page, internal_error, no_store};
use super::password::{self, MAX_CHARS, MIN_CHARS};
use super::signin::{SetupOption, needs_a_link};
use crate::http::{self, PeerAddr};

/// The password routes: `POST /login/password`, `GET /setup/password`, and `POST /setup/password`.
pub(super) fn routes() -> Router<WebApp> {
    Router::new()
        .route("/login/password", post(log_in))
        .route("/setup/password", get(setup_page).post(set_password))
}

/// The id the hash is read for when no user matches: no user has it (ids don't contain NUL), and
/// whatever the read finds is dropped.
const NO_USER: &str = "\0no user";

/// What the login form says about every sign-in that fails, however it failed: there is no such
/// user, they have no password, they can't sign in, or the password is wrong.
const WRONG: &str = "Wrong username or password.";

#[derive(Debug, Deserialize)]
struct LoginForm {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    nonce: Option<String>,
}

/// `POST /login/password`: checks the nonce, then the limits, then the password, and signs the
/// browser in as the user with a new session (the old cookie session, if the browser had one, is
/// deleted: a session id that existed before the login is never the one after it).
///
/// A failed attempt is a 401 with the login page and [`WRONG`], whatever went wrong. Over a limit it
/// is a 429 with how long to wait, and nothing was hashed.
async fn log_in(
    State(app): State<WebApp>,
    user: Option<Extension<WebUser>>,
    trusted: Option<Extension<TrustedProxy>>,
    peer: Option<Extension<ConnectInfo<PeerAddr>>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    if let Err(rejected) = check_nonce(&headers, form.nonce.as_deref()) {
        return rejected.into_response();
    }
    let trusted = trusted.is_some();
    let channel = &app.inner.channel;
    let client = client_key(trusted, &headers, peer.as_ref().map(|Extension(c)| &c.0));
    let attempt = match app.inner.logins.begin(
        &username_key(&form.username),
        &client,
        app.inner.clock.now(),
    ) {
        Ok(attempt) => attempt,
        Err(Limited { retry_after, first }) => {
            // One warning when a limit is reached; the rest of a flood is only debug noise.
            if first {
                warn!(%client, "refusing password attempts: too many failed ones");
            } else {
                debug!(%client, "refused a password attempt: too many failed ones");
            }
            return refused(
                &app,
                &headers,
                trusted,
                StatusCode::TOO_MANY_REQUESTS,
                retry_after.as_secs().max(1),
                &format!(
                    "Too many attempts. Try again in {}.",
                    wait_text(retry_after.as_secs())
                ),
                &form.username,
            )
            .await;
        }
    };

    // The user is looked up and the password is checked whether or not the user is there, so the
    // answer and the cost don't say. Someone who can't sign in is no one for this purpose.
    let username = form.username.trim();
    let found = {
        let directory = channel.users().get();
        directory
            .by_username(username)
            .filter(|found| channel.can_sign_in(&found.id))
            .map(|found| found.id.clone())
    };
    // The hash is read for every attempt, a user or not, so the time taken doesn't say which.
    let lookup = found
        .clone()
        .unwrap_or_else(|| UserId::new(NO_USER).expect("not empty"));
    let stored = match app.inner.store.user_password(&lookup).await {
        Ok(stored) => stored.filter(|_| found.is_some()),
        Err(err) => {
            app.inner.logins.cancel(attempt);
            return internal_error("cannot read a password hash", &err);
        }
    };
    let matched = match app
        .inner
        .passwords
        .verify(
            &form.password,
            stored.as_ref().map(|stored| stored.hash.as_str()),
        )
        .await
    {
        Ok(matched) => matched,
        Err(password::Error::Busy) => {
            app.inner.logins.cancel(attempt);
            warn!("refused a password attempt: too many passwords are being checked");
            return refused(
                &app,
                &headers,
                trusted,
                StatusCode::SERVICE_UNAVAILABLE,
                5,
                "Doozers is busy checking other passwords. Try again in a moment.",
                &form.username,
            )
            .await;
        }
        Err(err) => {
            app.inner.logins.cancel(attempt);
            return internal_error("cannot check a password", &err);
        }
    };
    let Some(id) = found.filter(|_| matched) else {
        debug!(%client, "a password sign-in failed");
        return refused(
            &app,
            &headers,
            trusted,
            StatusCode::UNAUTHORIZED,
            0,
            WRONG,
            &form.username,
        )
        .await;
    };

    app.inner.logins.succeeded(attempt);
    info!(user = %id, method = SignInMethod::Password.as_str(), "signed in to the web chat");
    let https = is_https(channel, &headers, trusted);
    let mut response = start_session(
        &app,
        id,
        SignInMethod::Password,
        user.as_ref().map(|Extension(user)| user),
        https,
        "/",
    )
    .await;
    response
        .headers_mut()
        .append(SET_COOKIE, clear_nonce_cookie(https));
    no_store(response)
}

/// The login page again with `error` and the status that says why, and `Retry-After` (seconds)
/// unless it is 0. The username stays in the form; the password never does.
async fn refused(
    app: &WebApp,
    headers: &HeaderMap,
    trusted: bool,
    status: StatusCode,
    retry_after: u64,
    error: &str,
    username: &str,
) -> Response {
    let mut response = render_login(app, headers, trusted, Some(error), username).await;
    // A page that couldn't be rendered says so as it is.
    if !response.status().is_success() {
        return response;
    }
    *response.status_mut() = status;
    if retry_after > 0 {
        response
            .headers_mut()
            .insert(RETRY_AFTER, HeaderValue::from(retry_after));
    }
    response
}

/// How long to wait, for a person: whole minutes, rounded up, or seconds under one.
fn wait_text(seconds: u64) -> String {
    let plural = |n: u64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    if seconds < 60 {
        plural(seconds.max(1), "second")
    } else {
        plural(seconds.div_ceil(60), "minute")
    }
}

impl WebApp {
    /// The setup page's entry for the password, if `user` may set one: what it offers depends on
    /// whether they have one.
    pub(super) async fn password_setup_option(
        &self,
        user: &WebUser,
    ) -> doozers_store::Result<Option<SetupOption>> {
        if !self.may_add_sign_in_method(user).await? {
            return Ok(None);
        }
        let has = self.inner.store.user_password(&user.user).await?.is_some();
        Ok(Some(SetupOption {
            label: if has {
                "Change your password"
            } else {
                "Password"
            }
            .to_owned(),
            description: if has {
                "You can already log in with your username and a password.".to_owned()
            } else {
                format!(
                    "Log in with your username and a password of at least {MIN_CHARS} characters."
                )
            },
            href: "/setup/password".to_owned(),
        }))
    }
}

#[derive(Template)]
#[template(path = "web/setup_password.html")]
struct SetupPasswordPage<'a> {
    username: &'a str,
    csrf: &'a str,
    /// When they last set it, if they have a password.
    changed: Option<String>,
    saved: bool,
    plain_http: bool,
    error: Option<&'a str>,
    min: usize,
    max: usize,
}

#[derive(Debug, Deserialize)]
struct SetupQuery {
    #[serde(default)]
    saved: Option<String>,
}

/// The page, with `error` and `status` if the password was refused.
async fn render_setup(
    app: &WebApp,
    user: &WebUser,
    headers: &HeaderMap,
    trusted: bool,
    saved: bool,
    error: Option<&str>,
    status: StatusCode,
) -> Response {
    let stored = match app.inner.store.user_password(&user.user).await {
        Ok(stored) => stored,
        Err(err) => return internal_error("cannot read a password hash", &err),
    };
    let username = app
        .inner
        .channel
        .users()
        .get()
        .user(&user.user)
        .map(|found| found.username.clone())
        .unwrap_or_default();
    let mut response = http::render(&SetupPasswordPage {
        username: &username,
        csrf: &user.csrf,
        changed: stored.map(|stored| stored.changed_at.strftime("%Y-%m-%d %H:%M UTC").to_string()),
        saved,
        plain_http: app.plain_http(headers, trusted),
        error,
        min: MIN_CHARS,
        max: MAX_CHARS,
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    no_store(response)
}

/// `GET /setup/password`: the form that sets or changes the password.
async fn setup_page(
    State(app): State<WebApp>,
    user: WebUser,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    Query(query): Query<SetupQuery>,
) -> Response {
    match app.may_add_sign_in_method(&user).await {
        Ok(true) => {}
        Ok(false) => return needs_a_link(),
        Err(err) => return internal_error("cannot look up a web session", &err),
    }
    render_setup(
        &app,
        &user,
        &headers,
        trusted.is_some(),
        query.saved.is_some(),
        None,
        StatusCode::OK,
    )
    .await
}

#[derive(Debug, Deserialize)]
struct PasswordForm {
    #[serde(default)]
    csrf: Option<String>,
    #[serde(default)]
    password: String,
    #[serde(default)]
    confirm: String,
}

/// `POST /setup/password`: sets the password (CSRF-checked), then 303 to the page, which says it
/// is saved. A password that breaks a rule, or that the second box doesn't repeat, is the form again
/// with why (400), and changes nothing.
async fn set_password(
    State(app): State<WebApp>,
    user: WebUser,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    match app.may_add_sign_in_method(&user).await {
        Ok(true) => {}
        Ok(false) => return needs_a_link(),
        Err(err) => return internal_error("cannot look up a web session", &err),
    }
    let trusted = trusted.is_some();
    let refuse = |message: String| {
        let (app, user, headers) = (&app, &user, &headers);
        async move {
            render_setup(
                app,
                user,
                headers,
                trusted,
                false,
                Some(&message),
                StatusCode::BAD_REQUEST,
            )
            .await
        }
    };
    if form.password != form.confirm {
        return refuse("The two passwords differ. Type the same one in both boxes.".to_owned())
            .await;
    }
    let username = app
        .inner
        .channel
        .users()
        .get()
        .user(&user.user)
        .map(|found| found.username.clone())
        .unwrap_or_default();
    let password = match password::check_new(&form.password, &username) {
        Ok(password) => password,
        Err(problem) => return refuse(problem.to_string()).await,
    };
    let hash = match app.inner.passwords.hash(password).await {
        Ok(hash) => hash,
        Err(password::Error::Busy) => {
            return no_store(error_page(
                StatusCode::SERVICE_UNAVAILABLE,
                "Busy",
                "Doozers is busy checking other passwords. Go back and try again in a moment.",
            ));
        }
        Err(err) => return internal_error("cannot hash a password", &err),
    };
    let now = app.inner.clock.now();
    if let Err(err) = app
        .inner
        .store
        .user_password_set(&user.user, &hash, now)
        .await
    {
        return internal_error("cannot store a password", &err);
    }
    info!(user = %user.user, "a password was set");
    no_store(Redirect::to("/setup/password?saved=1").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wait_is_said_in_whole_units_and_never_plural_for_one() {
        for (seconds, said) in [
            (0, "1 second"),
            (1, "1 second"),
            (2, "2 seconds"),
            (59, "59 seconds"),
            (60, "1 minute"),
            (61, "2 minutes"),
            (120, "2 minutes"),
            (900, "15 minutes"),
        ] {
            assert_eq!(wait_text(seconds), said, "{seconds}");
        }
    }
}
