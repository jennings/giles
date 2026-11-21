//! Confirming it's you: the step-up before a session may add, change, or remove a sign-in method.
//! Design: `Docs/technical/web-channel.md#step-up-re-verification`.
//!
//! A session that didn't come from an invite or recovery link may change the user's sign-in
//! methods only for [`STEP_UP_TTL`](super::signin::STEP_UP_TTL) after the user proved who they are
//! again with a method of their own ([`WebApp::sign_in_change`]). This is where they do:
//!
//! - `GET /account/reverify` offers the user's own methods: a password form, a passkey button, and
//!   a link for each connected identity provider that can sign in.
//! - `POST /account/reverify/password` checks the password.
//! - `POST /account/reverify/passkey/start` and `/finish` are a passkey ceremony, called by
//!   `passkey.js` with JSON.
//! - The identity provider's way is `GET /account/reverify/oidc/<id>` and the callback, in
//!   [`oidc_pages`](super::oidc_pages).
//!
//! Each success stores the time on the session's own row ([`WebApp::reverified`]): nothing the
//! client sends says it re-verified, and the time applies to that session only, so a session that
//! was stolen has none of it. A proxy login has no session, so it can't re-verify and can't change
//! sign-in methods.
//!
//! The password check counts against the same limits as logging in with that username and from that
//! client address ([`LoginLimits`](super::limits::LoginLimits)): holding a stolen cookie must not
//! be a way to guess a password without them. A passkey must be one of the user's own, and a
//! ceremony is bound to the user and the CSRF token of the login that started it.
//!
//! All of them go on to a page the user came from, one of a short list ([`back_to`]).

use askama::Template;
use axum::Extension;
use axum::Router;
use axum::extract::{ConnectInfo, Form, Json, Query, State};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::SignInMethod;
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};
use webauthn_rs::prelude::{Passkey, PublicKeyCredential};

use super::auth::{Login, TrustedProxy, WebUser, check_csrf};
use super::limits::{Limited, client_key, username_key};
use super::pages::{WebApp, error_page, internal_error, no_store};
use super::passkey::{Ceremony, reverify_binding};
use super::passkey_pages::json_error;
use super::passkey_signin::{judge, readable, record_use};
use super::password;
use super::shell::{Nav, Shell};
use crate::http::{self, PeerAddr};

/// The routes of the page and the two ways that need no provider.
pub(super) fn routes() -> Router<WebApp> {
    Router::new()
        .route("/account/reverify", get(page))
        .route("/account/reverify/password", post(by_password))
        .route("/account/reverify/passkey/start", post(passkey_start))
        .route("/account/reverify/passkey/finish", post(passkey_finish))
        .route_layer(map_response(async |response: Response| no_store(response)))
}

/// The pages a re-verification may lead back to. A client names one with `next`, and anything
/// else is the Account page, so `next` is never a place to send the browser to.
const BACK: [&str; 4] = ["/account", "/setup", "/setup/password", "/setup/passkey"];

/// The page `next` names, if it is one of [`BACK`], else the Account page.
pub(super) fn back_to(next: Option<&str>) -> &'static str {
    BACK.iter()
        .copied()
        .find(|back| Some(*back) == next)
        .unwrap_or(BACK[0])
}

/// The page that confirms it's you and then leads back to `back`.
pub(super) fn reverify_href(back: &str) -> String {
    format!("/account/reverify?next={back}")
}

/// Where a success leads: `back`, and the Account page also says what happened.
pub(super) fn done_href(back: &str) -> String {
    if back == BACK[0] {
        format!("{back}?notice=reverified")
    } else {
        back.to_owned()
    }
}

impl WebApp {
    /// Records that the user of `user`'s cookie session proved who they are again, now.
    ///
    /// # Errors
    ///
    /// The page to answer with, if the login has no session or the store fails.
    pub(super) async fn reverified(&self, user: &WebUser) -> Result<(), Box<Response>> {
        let Login::Cookie(token) = &user.login else {
            return Err(Box::new(no_session()));
        };
        match self
            .inner
            .store
            .web_session_reverified(token, self.inner.clock.now())
            .await
        {
            Ok(true) => {
                info!(user = %user.user, "a user confirmed it's them again");
                Ok(())
            }
            Ok(false) => Err(Box::new(no_session())),
            Err(err) => Err(Box::new(internal_error(
                "cannot record a re-verification",
                &err,
            ))),
        }
    }
}

/// The answer for a login that has no session to re-verify: a proxy login.
fn no_session() -> Response {
    no_store(error_page(
        StatusCode::FORBIDDEN,
        "Not available",
        "You are logged in through the proxy in front of Doozers, so Doozers can't ask you to \
         confirm it's you. To add a way to sign in, ask an admin for a recovery link, or run \
         `doozers admin login-link` if you run Doozers.",
    ))
}

/// A way to confirm it's you with an identity provider.
struct ProviderLink {
    name: String,
    href: String,
}

/// What the page offers.
struct Methods {
    password: bool,
    /// The origin passkeys work at, if the user has a passkey and passkeys can be used here.
    passkey: Option<String>,
    providers: Vec<ProviderLink>,
}

impl Methods {
    fn none(&self) -> bool {
        !self.password && self.passkey.is_none() && self.providers.is_empty()
    }
}

impl WebApp {
    /// The methods `user` can confirm it's them with: only ones they have, and that can sign
    /// them in on this install (the same test the removal of a method counts with).
    async fn reverify_methods(&self, user: &WebUser, back: &str) -> doozers_store::Result<Methods> {
        let store = &self.inner.store;
        let password = store.user_password(&user.user).await?.is_some();
        let passkey = match self.inner.passkeys.relying_party() {
            Ok(rp) if !readable(&store.user_passkeys(&user.user).await?).is_empty() => {
                Some(rp.origin().to_owned())
            }
            _ => None,
        };
        let connections = store.user_oidc_connections(&user.user).await?;
        let csrf: String = url::form_urlencoded::byte_serialize(user.csrf.as_bytes()).collect();
        let providers = store
            .oidc_providers()
            .await?
            .into_iter()
            .filter(|provider| provider.enabled)
            .filter(|provider| {
                connections
                    .iter()
                    .any(|c| c.provider == provider.id && c.issuer == provider.issuer)
            })
            .map(|provider| ProviderLink {
                href: format!(
                    "/account/reverify/oidc/{}?csrf={csrf}&next={back}",
                    provider.id
                ),
                name: provider.display_name,
            })
            .collect();
        Ok(Methods {
            password,
            passkey,
            providers,
        })
    }
}

#[derive(Template)]
#[template(path = "web/reverify.html")]
struct ReverifyPage<'a> {
    shell: Shell,
    csrf: &'a str,
    back: &'static str,
    methods: Methods,
    error: Option<&'a str>,
}

/// The page, answering `status` with `error` above the forms if there is one.
async fn render(
    app: &WebApp,
    user: &WebUser,
    back: &'static str,
    status: StatusCode,
    error: Option<&str>,
) -> Response {
    let methods = match app.reverify_methods(user, back).await {
        Ok(methods) => methods,
        Err(err) => return internal_error("cannot look up the sign-in methods", &err),
    };
    let mut response = http::render(&ReverifyPage {
        shell: Shell::new(user, Nav::Account),
        csrf: &user.csrf,
        back,
        methods,
        error,
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    no_store(response)
}

#[derive(Debug, Deserialize)]
struct Next {
    #[serde(default)]
    next: Option<String>,
}

/// `GET /account/reverify?next=`: the user's own methods. A proxy login gets why it can't.
async fn page(State(app): State<WebApp>, user: WebUser, Query(query): Query<Next>) -> Response {
    if !matches!(user.login, Login::Cookie(_)) {
        return no_session();
    }
    render(
        &app,
        &user,
        back_to(query.next.as_deref()),
        StatusCode::OK,
        None,
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
    next: Option<String>,
}

/// `POST /account/reverify/password` (CSRF-checked): checks the user's own password, counted like a
/// login attempt of theirs, and on success records the re-verification and goes back. A wrong
/// password, and a user who has none, are the same 401; over a limit it is a 429, and nothing was
/// hashed.
async fn by_password(
    State(app): State<WebApp>,
    user: WebUser,
    trusted: Option<Extension<TrustedProxy>>,
    peer: Option<Extension<ConnectInfo<PeerAddr>>>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    if !matches!(user.login, Login::Cookie(_)) {
        return no_session();
    }
    let back = back_to(form.next.as_deref());
    let username = app
        .inner
        .channel
        .users()
        .get()
        .user(&user.user)
        .map(|found| found.username.clone())
        .unwrap_or_default();
    let client = client_key(
        trusted.is_some(),
        &headers,
        peer.as_ref().map(|Extension(c)| &c.0),
    );
    let attempt = match app.inner.logins.begin(
        &username_key(&username),
        &client,
        app.inner.clock.now(),
    ) {
        Ok(attempt) => attempt,
        Err(Limited { retry_after, first }) => {
            if first {
                warn!(user = %user.user, %client, "refusing password re-verifications: too many failed attempts");
            } else {
                debug!(user = %user.user, %client, "refused a password re-verification: too many failed attempts");
            }
            let mut response = render(
                &app,
                &user,
                back,
                StatusCode::TOO_MANY_REQUESTS,
                Some("Too many attempts. Wait a few minutes and try again."),
            )
            .await;
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(retry_after.as_secs().max(1)));
            return response;
        }
    };
    let stored = match app.inner.store.user_password(&user.user).await {
        Ok(stored) => stored,
        Err(err) => {
            app.inner.logins.cancel(attempt);
            return internal_error("cannot read a password hash", &err);
        }
    };
    // A user with no password is checked against the dummy hash, so the cost is the same.
    let matched = match app
        .inner
        .passwords
        .verify(
            &form.password,
            stored.as_ref().map(|stored| stored.hash.as_str()),
        )
        .await
    {
        Ok(matched) => matched && stored.is_some(),
        Err(password::Error::Busy) => {
            app.inner.logins.cancel(attempt);
            let mut response = render(
                &app,
                &user,
                back,
                StatusCode::SERVICE_UNAVAILABLE,
                Some("Doozers is busy checking other passwords. Try again in a moment."),
            )
            .await;
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(5));
            return response;
        }
        Err(err) => {
            app.inner.logins.cancel(attempt);
            return internal_error("cannot check a password", &err);
        }
    };
    if !matched {
        debug!(user = %user.user, "a password re-verification failed");
        return render(
            &app,
            &user,
            back,
            StatusCode::UNAUTHORIZED,
            Some("That isn't your password."),
        )
        .await;
    }
    app.inner.logins.succeeded(attempt);
    if let Err(response) = app.reverified(&user).await {
        return *response;
    }
    no_store(Redirect::to(&done_href(back)).into_response())
}

/// A passkey that didn't confirm it's you, whatever was wrong with it.
const DIDNT_WORK: &str = "That passkey didn't confirm it's you. Try again, or use another way.";

/// `POST /account/reverify/passkey/start` (CSRF-checked): the options of a sign-in with one of the
/// user's own passkeys, and the id of the ceremony, which is bound to the user and the login.
async fn passkey_start(
    State(app): State<WebApp>,
    user: WebUser,
    headers: HeaderMap,
    Json(_): Json<serde_json::Value>,
) -> Response {
    if check_csrf(&user, &headers, None).is_err() {
        return json_error(
            StatusCode::FORBIDDEN,
            "This page is out of date. Reload it and try again.",
        );
    }
    if !matches!(user.login, Login::Cookie(_)) {
        return json_error(
            StatusCode::FORBIDDEN,
            "A proxy login can't confirm it's you. Ask an admin for a recovery link.",
        );
    }
    let rp = match app.inner.passkeys.relying_party() {
        Ok(rp) => rp,
        Err(why) => return json_error(StatusCode::CONFLICT, why.explain()),
    };
    let stored = match app.inner.store.user_passkeys(&user.user).await {
        Ok(stored) => stored,
        Err(err) => return internal_error("cannot read passkeys", &err),
    };
    let passkeys = readable(&stored);
    if passkeys.is_empty() {
        return json_error(StatusCode::NOT_FOUND, "You have no passkeys to use.");
    }
    let (options, state) = match rp.webauthn().start_passkey_authentication(&passkeys) {
        Ok(started) => started,
        Err(err) => return internal_error("cannot start a passkey check", &err),
    };
    let ceremony = app.inner.passkeys.ceremonies.begin(
        &reverify_binding(&user.user, &user.csrf),
        Ceremony::Reverify { state },
        app.inner.clock.now(),
    );
    no_store(Json(json!({ "ceremony": ceremony, "options": options })).into_response())
}

#[derive(Debug, Deserialize)]
struct FinishRequest {
    ceremony: String,
    #[serde(default)]
    next: Option<String>,
    credential: PublicKeyCredential,
}

/// `POST /account/reverify/passkey/finish` (CSRF-checked): checks the browser's answer, which must
/// be from one of the user's own passkeys, and on success records the re-verification and answers
/// with where to go. Every failure is the same 401.
async fn passkey_finish(
    State(app): State<WebApp>,
    user: WebUser,
    headers: HeaderMap,
    Json(request): Json<FinishRequest>,
) -> Response {
    if check_csrf(&user, &headers, None).is_err() {
        return json_error(
            StatusCode::FORBIDDEN,
            "This page is out of date. Reload it and try again.",
        );
    }
    if !matches!(user.login, Login::Cookie(_)) {
        return json_error(
            StatusCode::FORBIDDEN,
            "A proxy login can't confirm it's you. Ask an admin for a recovery link.",
        );
    }
    let Ok(rp) = app.inner.passkeys.relying_party() else {
        return json_error(StatusCode::NOT_FOUND, "Passkeys aren't offered here.");
    };
    let Some(Ceremony::Reverify { state }) = app.inner.passkeys.ceremonies.finish(
        &request.ceremony,
        &reverify_binding(&user.user, &user.csrf),
        app.inner.clock.now(),
    ) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "This took too long, or was already used. Try again.",
        );
    };
    let outcome = rp
        .webauthn()
        .finish_passkey_authentication(&request.credential, &state);
    let stored = match app
        .inner
        .store
        .user_passkey_by_credential(request.credential.get_credential_id())
        .await
    {
        Ok(stored) => stored,
        Err(err) => return internal_error("cannot read a passkey", &err),
    };
    // It must be the user's own, whoever the library says it is.
    let Some(stored) = stored.filter(|stored| stored.user == user.user) else {
        return json_error(StatusCode::UNAUTHORIZED, DIDNT_WORK);
    };
    let Some(result) = judge(&stored, outcome) else {
        return json_error(StatusCode::UNAUTHORIZED, DIDNT_WORK);
    };
    let Ok(passkey) = serde_json::from_str::<Passkey>(&stored.credential) else {
        warn!(user = %user.user, passkey = %stored.id, "cannot read a stored passkey");
        return json_error(StatusCode::UNAUTHORIZED, DIDNT_WORK);
    };
    if let Err(response) = record_use(&app, &stored, passkey, &result).await {
        return *response;
    }
    if let Err(response) = app.reverified(&user).await {
        return *response;
    }
    info!(user = %user.user, method = SignInMethod::Passkey.as_str(), "confirmed it's them with a passkey");
    let back = back_to(request.next.as_deref());
    no_store(Json(json!({ "redirect": done_href(back) })).into_response())
}
