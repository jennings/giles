//! Signing in with a passkey: the requests `passkey.js` makes from the login page. Adding one is in
//! [`passkey_pages`](super::passkey_pages). Design: `Docs/technical/web-channel.md`.
//!
//! A sign-in is two steps, and the second can be asked for two ways:
//!
//! 1. A discoverable passkey, with no username. `POST /login/passkey/discover` starts the
//!    ceremony (`start_discoverable_authentication`). The page uses the options twice: as a
//!    conditional request on load, so the browser offers the passkeys in the username field's
//!    autofill, and, without `mediation`, behind the "Sign in with a passkey" button.
//! 2. By username, for a non-resident key, which the browser can only use if the server names it.
//!    `POST /login/passkey/username` starts a ceremony with that user's passkeys in
//!    `allowCredentials`.
//!
//! `POST /login/passkey/finish` ends either: it takes the ceremony the start made, checks the
//! browser's answer, and signs the browser in.
//!
//! # The username step doesn't say who exists
//!
//! The answer for a username that isn't a user, one that has no passkeys, and one that can't sign
//! in is made of credential ids that `webauthn_rs::fake` invents from the name and a key kept in the
//! database, so it is the same every time and has the shape of a real one. A real user's answer
//! has their credential ids too, without the transports (a made-up id has none to give, and a
//! transport list would tell the two apart). Finishing a made-up ceremony fails like a wrong
//! passkey does.
//!
//! # What a sign-in checks
//!
//! Every POST carries the sign-in nonce ([`check_nonce`]) like the other sign-ins, and a ceremony
//! is bound to the nonce of the browser that started it. Starting is rate-limited per client
//! address with the password limiter's machinery ([`LoginLimits`](super::limits::LoginLimits), with
//! no limit per name). Finishing refuses a passkey whose user handle isn't its owner's, whose owner
//! can't sign in, and one the library refuses, which includes a signature counter that didn't
//! advance (a sign of a cloned authenticator): that is logged, and the passkey is left as it is.
//! Whatever the reason, the answer is the same 401. A success stores the new counter and the time,
//! and goes through `login::begin_session` with the method `passkey`, which makes a new session and
//! ends the browser's old one.

use axum::Extension;
use axum::Router;
use axum::extract::{ConnectInfo, Json, State};
use axum::http::header::{RETRY_AFTER, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use doozers_store::{SignInMethod, StoredPasskey};
use doozers_types::UserId;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, info, warn};
use webauthn_rs::fake::{FakePasskeyDistribution, WebauthnFakeCredentialGenerator};
use webauthn_rs::prelude::{
    AuthenticationResult, Passkey, PublicKeyCredential, RequestChallengeResponse, WebauthnError,
    WebauthnResult,
};
use webauthn_rs_proto::AllowCredentials;

use super::auth::{TrustedProxy, WebUser, check_nonce, clear_nonce_cookie, session_cookie};
use super::limits::{Limited, client_key};
use super::login::{begin_session, is_https};
use super::pages::{WebApp, internal_error, no_store};
use super::passkey::{Ceremony, RelyingParty, sign_in_binding, user_handle};
use super::passkey_pages::json_error;
use crate::http::PeerAddr;

/// The sign-in routes.
pub(super) fn routes() -> Router<WebApp> {
    Router::new()
        .route("/login/passkey/discover", post(discover))
        .route("/login/passkey/username", post(by_username))
        .route("/login/passkey/finish", post(finish))
}

/// How many ceremonies one client address may start per window ([`WINDOW`](super::limits::WINDOW)).
/// A page load starts one, and so does each button press.
pub(super) const START_ATTEMPTS: u32 = 60;

/// The username every start counts under: starts are limited by address only.
const COUNTED_AS: &str = "passkey";

/// The id a passkey lookup is made for when no user matches: no user has it (ids don't contain
/// NUL), and what the read finds is dropped.
const NO_USER: &str = "\0no user";

/// What every failed sign-in says, whatever failed.
const DIDNT_WORK: &str = "That passkey didn't log you in. Try again, or use another way to log in.";

/// A ceremony that is gone: expired, used, or from another browser.
const TOOK_TOO_LONG: &str = "This took too long, or was already used. Try again.";

#[derive(Debug, Deserialize)]
struct StartRequest {
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    username: String,
}

/// What every start checks first: passkeys are offered, the nonce is the browser's, and the client
/// is under its limit. The nonce on success.
fn begin<'a>(
    app: &'a WebApp,
    trusted: bool,
    peer: Option<&PeerAddr>,
    headers: &HeaderMap,
    nonce: Option<&'a str>,
) -> Result<(&'a RelyingParty, &'a str), Box<Response>> {
    let rp = app.inner.passkeys.relying_party().map_err(|_| {
        Box::new(json_error(
            StatusCode::NOT_FOUND,
            "Passkeys aren't offered here. Use a password.",
        ))
    })?;
    check_nonce(headers, nonce).map_err(|_| {
        Box::new(json_error(
            StatusCode::FORBIDDEN,
            "This page is out of date. Reload it and try again.",
        ))
    })?;
    let client = client_key(trusted, headers, peer);
    match app
        .inner
        .passkey_starts
        .begin(COUNTED_AS, &client, app.inner.clock.now())
    {
        // Counted for good: a start is a request that costs memory whether or not it signs in.
        Ok(_) => {}
        Err(Limited { retry_after, first }) => {
            if first {
                warn!(%client, "refusing passkey sign-in starts: too many");
            } else {
                debug!(%client, "refused a passkey sign-in start: too many");
            }
            let seconds = retry_after.as_secs().max(1);
            let mut response = json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many attempts. Wait a few minutes and try again.",
            );
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(seconds));
            return Err(Box::new(response));
        }
    }
    Ok((rp, nonce.unwrap_or_default()))
}

/// The answer to a start: the id of the ceremony, and the options for the browser.
fn started(ceremony: &str, options: &impl Serialize) -> Response {
    no_store(Json(json!({ "ceremony": ceremony, "options": options })).into_response())
}

/// `POST /login/passkey/discover`: the options of a sign-in that names no user.
async fn discover(
    State(app): State<WebApp>,
    trusted: Option<Extension<TrustedProxy>>,
    peer: Option<Extension<ConnectInfo<PeerAddr>>>,
    headers: HeaderMap,
    Json(request): Json<StartRequest>,
) -> Response {
    let (rp, nonce) = match begin(
        &app,
        trusted.is_some(),
        peer.as_ref().map(|Extension(c)| &c.0),
        &headers,
        request.nonce.as_deref(),
    ) {
        Ok(started) => started,
        Err(refused) => return *refused,
    };
    let (options, state) = match rp.webauthn().start_discoverable_authentication() {
        Ok(started) => started,
        Err(err) => return internal_error("cannot start a passkey sign-in", &err),
    };
    let ceremony = app.inner.passkeys.ceremonies.begin(
        &sign_in_binding(nonce),
        Ceremony::Discover(state),
        app.inner.clock.now(),
    );
    started(&ceremony, &options)
}

/// The credential ids the username step offers for a name that has no passkeys to offer.
async fn made_up_ids(app: &WebApp, username: &str) -> Result<Vec<Vec<u8>>, String> {
    let generator = app
        .inner
        .passkeys
        .fake
        .get_or_try_init(|| async {
            let mut candidate = [0; 32];
            getrandom::fill(&mut candidate).map_err(|err| err.to_string())?;
            let key = app
                .inner
                .store
                .passkey_fake_key(candidate)
                .await
                .map_err(|err| err.to_string())?;
            WebauthnFakeCredentialGenerator::<FakePasskeyDistribution>::new(&key)
                .map_err(|err| err.to_string())
        })
        .await?;
    generator
        .generate(username.as_bytes())
        .map_err(|err| err.to_string())
}

/// The user's passkeys as the library holds them: those whose stored credential can be read.
fn readable(stored: &[StoredPasskey]) -> Vec<Passkey> {
    stored
        .iter()
        .filter_map(|passkey| match serde_json::from_str(&passkey.credential) {
            Ok(credential) => Some(credential),
            Err(err) => {
                warn!(user = %passkey.user, passkey = %passkey.id, error = %err, "cannot read a stored passkey");
                None
            }
        })
        .collect()
}

/// `POST /login/passkey/username`: the options of a sign-in by username. The answer for a name that
/// isn't a user, has no passkeys, or can't sign in has made-up credential ids and is otherwise
/// the same as for one that does.
async fn by_username(
    State(app): State<WebApp>,
    trusted: Option<Extension<TrustedProxy>>,
    peer: Option<Extension<ConnectInfo<PeerAddr>>>,
    headers: HeaderMap,
    Json(request): Json<StartRequest>,
) -> Response {
    let (rp, nonce) = match begin(
        &app,
        trusted.is_some(),
        peer.as_ref().map(|Extension(c)| &c.0),
        &headers,
        request.nonce.as_deref(),
    ) {
        Ok(started) => started,
        Err(refused) => return *refused,
    };
    let channel = &app.inner.channel;
    let found = {
        let directory = channel.users().get();
        directory
            .by_username(request.username.trim())
            .filter(|found| channel.can_sign_in(&found.id))
            .map(|found| found.id.clone())
    };
    // The passkeys are read for every name, a user or not, so the work doesn't say which.
    let lookup = found
        .clone()
        .unwrap_or_else(|| UserId::new(NO_USER).expect("not empty"));
    let stored = match app.inner.store.user_passkeys(&lookup).await {
        Ok(stored) => stored,
        Err(err) => return internal_error("cannot read passkeys", &err),
    };
    let passkeys = readable(&stored);
    let webauthn = rp.webauthn();
    let (mut options, ceremony) = match found.filter(|_| !passkeys.is_empty()) {
        Some(user) => match webauthn.start_passkey_authentication(&passkeys) {
            Ok((options, state)) => (options, Ceremony::ByUsername { user, state }),
            Err(err) => return internal_error("cannot start a passkey sign-in", &err),
        },
        None => {
            // Keyed on the whole name as it is looked up (trimmed, ASCII case folded), not cut:
            // a longer name than any user has must not share its ids with a shorter one.
            let name = request.username.trim().to_ascii_lowercase();
            let ids = match made_up_ids(&app, &name).await {
                Ok(ids) => ids,
                Err(err) => {
                    return internal_error(
                        "cannot make up credential ids",
                        &std::io::Error::other(err),
                    );
                }
            };
            // A made-up answer is a discoverable one with its list swapped, so everything else
            // about it is what a real one has.
            let (mut options, _) = match webauthn.start_discoverable_authentication() {
                Ok(started) => started,
                Err(err) => return internal_error("cannot start a passkey sign-in", &err),
            };
            options.public_key.allow_credentials = ids
                .into_iter()
                .map(|id| AllowCredentials {
                    type_: "public-key".to_owned(),
                    id,
                    transports: None,
                })
                .collect();
            (options, Ceremony::Fake)
        }
    };
    normalize(&mut options);
    let id = app.inner.passkeys.ceremonies.begin(
        &sign_in_binding(nonce),
        ceremony,
        app.inner.clock.now(),
    );
    started(&id, &options)
}

/// Makes the options of every username answer the same shape: no transports on the credentials,
/// no conditional mediation, and none of the extensions of the discoverable request.
fn normalize(options: &mut RequestChallengeResponse) {
    options.mediation = None;
    options.public_key.extensions = None;
    options.public_key.hints = None;
    for credential in &mut options.public_key.allow_credentials {
        credential.transports = None;
    }
}

#[derive(Debug, Deserialize)]
struct FinishRequest {
    #[serde(default)]
    nonce: Option<String>,
    ceremony: String,
    credential: PublicKeyCredential,
}

fn failed() -> Response {
    json_error(StatusCode::UNAUTHORIZED, DIDNT_WORK)
}

/// What the library said about a passkey's answer. A counter that went backwards is warned about.
fn judge(
    stored: &StoredPasskey,
    outcome: WebauthnResult<AuthenticationResult>,
) -> Option<AuthenticationResult> {
    match outcome {
        Ok(result) => Some(result),
        Err(WebauthnError::CredentialPossibleCompromise) => {
            warn!(
                user = %stored.user,
                passkey = %stored.id,
                "refused a passkey whose signature counter did not advance: its authenticator may be cloned",
            );
            None
        }
        Err(err) => {
            debug!(user = %stored.user, error = %err, "a passkey sign-in failed");
            None
        }
    }
}

/// Checks the answer to a sign-in with a discoverable passkey. `None`: it doesn't sign anyone in.
async fn check_discoverable(
    app: &WebApp,
    rp: &RelyingParty,
    state: webauthn_rs::prelude::DiscoverableAuthentication,
    credential: &PublicKeyCredential,
) -> Result<Option<(StoredPasskey, Passkey, AuthenticationResult)>, Box<Response>> {
    let webauthn = rp.webauthn();
    let Ok((handle, id)) = webauthn.identify_discoverable_authentication(credential) else {
        return Ok(None);
    };
    let stored = app
        .inner
        .store
        .user_passkey_by_credential(id)
        .await
        .map_err(|err| Box::new(internal_error("cannot read a passkey", &err)))?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    if user_handle(&stored.user) != handle {
        warn!(user = %stored.user, "refused a passkey that names another user");
        return Ok(None);
    }
    if !app.inner.channel.can_sign_in(&stored.user) {
        warn!(user = %stored.user, "refused a passkey of a user who can't sign in");
        return Ok(None);
    }
    let Ok(passkey) = serde_json::from_str::<Passkey>(&stored.credential) else {
        warn!(user = %stored.user, passkey = %stored.id, "cannot read a stored passkey");
        return Ok(None);
    };
    let outcome = webauthn.finish_discoverable_authentication_v2(credential, state, &passkey);
    Ok(judge(&stored, outcome).map(|result| (stored, passkey, result)))
}

/// Checks the answer to a sign-in of `user` by username. `None`: it doesn't sign anyone in.
async fn check_by_username(
    app: &WebApp,
    rp: &RelyingParty,
    user: &UserId,
    state: &webauthn_rs::prelude::PasskeyAuthentication,
    credential: &PublicKeyCredential,
) -> Result<Option<(StoredPasskey, Passkey, AuthenticationResult)>, Box<Response>> {
    let outcome = rp
        .webauthn()
        .finish_passkey_authentication(credential, state);
    let stored = app
        .inner
        .store
        .user_passkey_by_credential(credential.get_credential_id())
        .await
        .map_err(|err| Box::new(internal_error("cannot read a passkey", &err)))?;
    let Some(stored) = stored.filter(|stored| stored.user == *user) else {
        return Ok(None);
    };
    if credential
        .get_user_unique_id()
        .is_some_and(|handle| handle != user_handle(user).as_bytes())
    {
        warn!(user = %user, "refused a passkey that names another user");
        return Ok(None);
    }
    if !app.inner.channel.can_sign_in(user) {
        warn!(user = %user, "refused a passkey of a user who can't sign in");
        return Ok(None);
    }
    let Ok(passkey) = serde_json::from_str::<Passkey>(&stored.credential) else {
        warn!(user = %user, passkey = %stored.id, "cannot read a stored passkey");
        return Ok(None);
    };
    Ok(judge(&stored, outcome).map(|result| (stored, passkey, result)))
}

/// Records that `stored` answered a ceremony: its credential, with the signature counter `result`
/// moved, and the time. Done for every answer the library accepted, whatever it is used for.
pub(super) async fn record_use(
    app: &WebApp,
    stored: &StoredPasskey,
    mut passkey: Passkey,
    result: &AuthenticationResult,
) -> Result<(), Box<Response>> {
    passkey.update_credential(result);
    let credential = serde_json::to_string(&passkey)
        .map_err(|err| Box::new(internal_error("cannot serialize a passkey", &err)))?;
    app.inner
        .store
        .user_passkey_used(stored.id, &credential, app.inner.clock.now())
        .await
        .map_err(|err| Box::new(internal_error("cannot record the use of a passkey", &err)))?;
    Ok(())
}

/// `POST /login/passkey/finish`: checks the browser's answer, and signs it in as the passkey's user
/// with `{"redirect": "/"}` and the session cookie.
async fn finish(
    State(app): State<WebApp>,
    user: Option<Extension<WebUser>>,
    trusted: Option<Extension<TrustedProxy>>,
    headers: HeaderMap,
    Json(request): Json<FinishRequest>,
) -> Response {
    let Ok(rp) = app.inner.passkeys.relying_party() else {
        return json_error(
            StatusCode::NOT_FOUND,
            "Passkeys aren't offered here. Use a password.",
        );
    };
    if check_nonce(&headers, request.nonce.as_deref()).is_err() {
        return json_error(
            StatusCode::FORBIDDEN,
            "This page is out of date. Reload it and try again.",
        );
    }
    let nonce = request.nonce.as_deref().unwrap_or_default();
    let Some(ceremony) = app.inner.passkeys.ceremonies.finish(
        &request.ceremony,
        &sign_in_binding(nonce),
        app.inner.clock.now(),
    ) else {
        return json_error(StatusCode::BAD_REQUEST, TOOK_TOO_LONG);
    };
    let checked = match ceremony {
        Ceremony::Discover(state) => check_discoverable(&app, rp, state, &request.credential).await,
        Ceremony::ByUsername { user, state } => {
            check_by_username(&app, rp, &user, &state, &request.credential).await
        }
        Ceremony::Fake | Ceremony::Register { .. } => Ok(None),
    };
    let (stored, passkey, result) = match checked {
        Ok(Some(signed)) => signed,
        Ok(None) => return failed(),
        Err(response) => return *response,
    };
    if let Err(response) = record_use(&app, &stored, passkey, &result).await {
        return *response;
    }

    info!(user = %stored.user, method = SignInMethod::Passkey.as_str(), "signed in to the web chat");
    let https = is_https(&app.inner.channel, &headers, trusted.is_some());
    let token = match begin_session(
        &app,
        stored.user,
        SignInMethod::Passkey,
        user.as_ref().map(|Extension(user)| user),
    )
    .await
    {
        Ok(token) => token,
        Err(response) => return *response,
    };
    let mut response = Json(json!({ "redirect": "/" })).into_response();
    response
        .headers_mut()
        .append(SET_COOKIE, session_cookie(&token, https));
    response
        .headers_mut()
        .append(SET_COOKIE, clear_nonce_cookie(https));
    no_store(response)
}
