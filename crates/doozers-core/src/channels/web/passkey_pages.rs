//! Adding a passkey: the setup page and the two requests of a registration. Design:
//! `Docs/technical/web-channel.md`.
//!
//! - `GET /setup/passkey` lists the user's passkeys and has the two buttons that add one.
//! - `POST /setup/passkey/start` and `POST /setup/passkey/finish` are the registration, called by
//!   `passkey.js` with JSON. Only a browser that signed in with an invite or recovery link may use
//!   any of them, as for a password: a 30-day session from any other source isn't enough to take
//!   over the account's way in. Both check CSRF, and the ceremony in between is bound to the
//!   login that started it.
//!
//! # The two buttons
//!
//! **Add a passkey** registers a discoverable credential: the passkey is kept by the authenticator,
//! so the browser can offer it with no username. **Add a non-resident key** registers one that
//! isn't, for a hardware key with few slots for passkeys. `start_passkey_registration` always asks
//! for `residentKey: "discouraged"`, so for the first button [`require_discoverable`] sets
//! `residentKey: "required"` and `requireResidentKey: true` on the options it sends. The library
//! doesn't read either field when it verifies the registration, which the tests rely on (a
//! software authenticator keeps a passkey discoverable only if it was asked to), so a library
//! upgrade that starts reading them can't break this unnoticed. The one check made here is the
//! browser's own report: if it says a credential asked to be discoverable wasn't made as one
//! (`credProps.rk` is false), the registration is refused.

use askama::Template;
use axum::Router;
use axum::extract::{Json, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use doozers_store::{NewPasskey, PasskeyKind};
use serde::Deserialize;
use serde_json::json;
use tracing::{debug, info, warn};
use webauthn_rs::prelude::{CreationChallengeResponse, Passkey, RegisterPublicKeyCredential};
use webauthn_rs_proto::ResidentKeyRequirement;

use super::auth::{WebUser, check_csrf};
use super::pages::{WebApp, internal_error, no_store};
use super::passkey::{Ceremony, registration_binding, user_handle};
use super::signin::{SetupOption, needs_a_link};
use crate::http;

/// The longest name a passkey is given, in characters.
const MAX_NAME_CHARS: usize = 64;

/// The registration routes.
pub(super) fn routes() -> Router<WebApp> {
    Router::new()
        .route("/setup/passkey", get(setup_page))
        .route("/setup/passkey/start", post(start))
        .route("/setup/passkey/finish", post(finish))
}

/// A JSON answer that says what went wrong, in words for the user.
pub(super) fn json_error(status: StatusCode, message: &str) -> Response {
    no_store((status, Json(json!({ "error": message }))).into_response())
}

fn out_of_date() -> Response {
    json_error(
        StatusCode::FORBIDDEN,
        "This page is out of date. Reload it and try again.",
    )
}

fn needs_a_link_json() -> Response {
    json_error(
        StatusCode::FORBIDDEN,
        "You add a passkey from a login that came from an invite or recovery link. Ask an admin \
         for a link, or run `doozers admin login-link` if you run Doozers.",
    )
}

fn took_too_long() -> Response {
    json_error(
        StatusCode::BAD_REQUEST,
        "This took too long, or was already used. Start again.",
    )
}

/// How a kind is said to a user.
fn kind_label(kind: PasskeyKind) -> &'static str {
    match kind {
        PasskeyKind::Discoverable => "Passkey",
        PasskeyKind::NonResident => "Non-resident key",
    }
}

/// The name for a passkey the user didn't name, or named with nothing but spaces: its kind and
/// its number among the user's.
fn default_name(kind: PasskeyKind, existing: usize) -> String {
    format!("{} {}", kind_label(kind), existing + 1)
}

/// The name a user typed, trimmed, without control characters, and cut to [`MAX_NAME_CHARS`]; or
/// the default when nothing is left.
fn clean_name(typed: &str, kind: PasskeyKind, existing: usize) -> String {
    let name: String = typed
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect();
    let name = name.trim().to_owned();
    if name.is_empty() {
        default_name(kind, existing)
    } else {
        name
    }
}

/// Makes the options of a registration ask for a discoverable credential, which
/// `start_passkey_registration` never does. Fails if the library built none of the
/// authenticator selection to change, which would otherwise register a key that can't be found
/// without a username.
fn require_discoverable(options: &mut CreationChallengeResponse) -> Result<(), &'static str> {
    let selection = options
        .public_key
        .authenticator_selection
        .as_mut()
        .ok_or("the library sent no authenticator selection")?;
    selection.resident_key = Some(ResidentKeyRequirement::Required);
    selection.require_resident_key = true;
    Ok(())
}

impl WebApp {
    /// The setup page's entry for passkeys, if `user` may add one. It is there even when passkeys
    /// aren't available, so the page it leads to can say why.
    pub(super) async fn passkey_setup_option(
        &self,
        user: &WebUser,
    ) -> doozers_store::Result<Option<SetupOption>> {
        if !self.may_add_sign_in_method(user).await? {
            return Ok(None);
        }
        let have = self.inner.store.user_passkeys(&user.user).await?.len();
        let available = self.inner.passkeys.relying_party().is_ok();
        Ok(Some(SetupOption {
            label: if have == 0 {
                "Passkey"
            } else {
                "Add another passkey"
            }
            .to_owned(),
            description: if !available {
                "Not available here. The page says why.".to_owned()
            } else if have == 0 {
                "Log in with your fingerprint, face, screen lock, password manager, or a security \
                 key."
                    .to_owned()
            } else {
                format!(
                    "You have {have} passkey{}. Add one for another device.",
                    if have == 1 { "" } else { "s" }
                )
            },
            href: "/setup/passkey".to_owned(),
        }))
    }
}

struct PasskeyRow {
    name: String,
    kind: &'static str,
    added: String,
    last_used: String,
}

#[derive(Template)]
#[template(path = "web/setup_passkey.html")]
struct SetupPasskeyPage<'a> {
    /// Why passkeys can't be added, if they can't.
    unavailable: Option<&'static str>,
    origin: &'a str,
    csrf: &'a str,
    saved: bool,
    passkeys: Vec<PasskeyRow>,
}

#[derive(Debug, Deserialize)]
struct SetupQuery {
    #[serde(default)]
    saved: Option<String>,
}

/// `GET /setup/passkey`.
async fn setup_page(
    State(app): State<WebApp>,
    user: WebUser,
    Query(query): Query<SetupQuery>,
) -> Response {
    match app.may_add_sign_in_method(&user).await {
        Ok(true) => {}
        Ok(false) => return needs_a_link(),
        Err(err) => return internal_error("cannot look up a web session", &err),
    }
    let stored = match app.inner.store.user_passkeys(&user.user).await {
        Ok(stored) => stored,
        Err(err) => return internal_error("cannot read passkeys", &err),
    };
    let time = |at: jiff::Timestamp| at.strftime("%Y-%m-%d %H:%M UTC").to_string();
    let passkeys = stored
        .into_iter()
        .map(|passkey| PasskeyRow {
            name: passkey.name,
            kind: kind_label(passkey.kind),
            added: time(passkey.created_at),
            last_used: passkey.last_used_at.map_or("never".to_owned(), time),
        })
        .collect();
    let relying_party = app.inner.passkeys.relying_party();
    no_store(http::render(&SetupPasskeyPage {
        unavailable: relying_party.as_ref().err().map(|why| why.explain()),
        origin: relying_party.as_ref().map_or("", |rp| rp.origin()),
        csrf: &user.csrf,
        saved: query.saved.is_some(),
        passkeys,
    }))
}

#[derive(Debug, Deserialize)]
struct StartRequest {
    kind: String,
}

/// `POST /setup/passkey/start`: the options for the browser's `navigator.credentials.create`, and
/// the id of the ceremony they belong to.
async fn start(
    State(app): State<WebApp>,
    user: WebUser,
    headers: HeaderMap,
    Json(request): Json<StartRequest>,
) -> Response {
    if check_csrf(&user, &headers, None).is_err() {
        return out_of_date();
    }
    match app.may_add_sign_in_method(&user).await {
        Ok(true) => {}
        Ok(false) => return needs_a_link_json(),
        Err(err) => return internal_error("cannot look up a web session", &err),
    }
    let rp = match app.inner.passkeys.relying_party() {
        Ok(rp) => rp,
        Err(why) => return json_error(StatusCode::CONFLICT, why.explain()),
    };
    let kind = match request.kind.as_str() {
        "discoverable" => PasskeyKind::Discoverable,
        "non_resident" => PasskeyKind::NonResident,
        _ => return json_error(StatusCode::BAD_REQUEST, "Choose which kind of key to add."),
    };
    let (username, display_name) = {
        let directory = app.inner.channel.users().get();
        match directory.user(&user.user) {
            Some(found) => (found.username.clone(), found.display_name.clone()),
            None => return out_of_date(),
        }
    };
    let existing = match app.inner.store.user_passkeys(&user.user).await {
        Ok(existing) => existing,
        Err(err) => return internal_error("cannot read passkeys", &err),
    };
    // The browser refuses to make a second credential on an authenticator that has one of these.
    let exclude: Vec<_> = existing
        .iter()
        .map(|passkey| passkey.credential_id.clone())
        .collect();
    let started = rp.webauthn().start_passkey_registration(
        user_handle(&user.user),
        &username,
        &display_name,
        (!exclude.is_empty()).then_some(exclude),
    );
    let (mut options, state) = match started {
        Ok(started) => started,
        Err(err) => return internal_error("cannot start a passkey registration", &err),
    };
    if kind == PasskeyKind::Discoverable
        && let Err(why) = require_discoverable(&mut options)
    {
        return internal_error(
            "cannot ask for a discoverable passkey",
            &std::io::Error::other(why),
        );
    }
    let ceremony = app.inner.passkeys.ceremonies.begin(
        &registration_binding(&user.user, &user.csrf),
        Ceremony::Register { kind, state },
        app.inner.clock.now(),
    );
    no_store(Json(json!({ "ceremony": ceremony, "options": options })).into_response())
}

#[derive(Debug, Deserialize)]
struct FinishRequest {
    ceremony: String,
    #[serde(default)]
    name: String,
    credential: RegisterPublicKeyCredential,
}

/// `POST /setup/passkey/finish`: checks what the browser made and stores it.
async fn finish(
    State(app): State<WebApp>,
    user: WebUser,
    headers: HeaderMap,
    Json(request): Json<FinishRequest>,
) -> Response {
    if check_csrf(&user, &headers, None).is_err() {
        return out_of_date();
    }
    match app.may_add_sign_in_method(&user).await {
        Ok(true) => {}
        Ok(false) => return needs_a_link_json(),
        Err(err) => return internal_error("cannot look up a web session", &err),
    }
    let rp = match app.inner.passkeys.relying_party() {
        Ok(rp) => rp,
        Err(why) => return json_error(StatusCode::CONFLICT, why.explain()),
    };
    let Some(Ceremony::Register { kind, state }) = app.inner.passkeys.ceremonies.finish(
        &request.ceremony,
        &registration_binding(&user.user, &user.csrf),
        app.inner.clock.now(),
    ) else {
        return took_too_long();
    };
    let passkey: Passkey = match rp
        .webauthn()
        .finish_passkey_registration(&request.credential, &state)
    {
        Ok(passkey) => passkey,
        Err(err) => {
            debug!(user = %user.user, error = %err, "a passkey registration failed");
            return json_error(
                StatusCode::BAD_REQUEST,
                "Your browser or key didn't complete the registration. Try again.",
            );
        }
    };
    // The browser says whether it made a discoverable credential. The library doesn't check, and
    // the browser can't be forced to, but one that says it didn't is telling us the passkey
    // wouldn't be offered without a username.
    let rk = request
        .credential
        .extensions
        .cred_props
        .as_ref()
        .and_then(|props| props.rk);
    if kind == PasskeyKind::Discoverable && rk == Some(false) {
        info!(user = %user.user, "refused a passkey that wasn't made discoverable");
        return json_error(
            StatusCode::BAD_REQUEST,
            "This key can't keep a passkey that works without a username. Use \"Add a \
             non-resident key\" for it instead.",
        );
    }
    let credential = match serde_json::to_string(&passkey) {
        Ok(credential) => credential,
        Err(err) => return internal_error("cannot serialize a passkey", &err),
    };
    let existing = match app.inner.store.user_passkeys(&user.user).await {
        Ok(existing) => existing.len(),
        Err(err) => return internal_error("cannot read passkeys", &err),
    };
    let new = NewPasskey {
        user: user.user.clone(),
        credential_id: passkey.cred_id().clone(),
        credential,
        kind,
        name: clean_name(&request.name, kind, existing),
    };
    match app
        .inner
        .store
        .user_passkey_add(new, app.inner.clock.now())
        .await
    {
        Ok(stored) => {
            info!(
                user = %user.user,
                kind = stored.kind.as_str(),
                "a passkey was added"
            );
            no_store(Json(json!({ "redirect": "/setup/passkey?saved=1" })).into_response())
        }
        Err(doozers_store::Error::Conflict(_)) => {
            warn!(user = %user.user, "a passkey that is registered already was registered again");
            json_error(
                StatusCode::CONFLICT,
                "This passkey is already registered. Pick another key or device.",
            )
        }
        Err(err) => internal_error("cannot store a passkey", &err),
    }
}
