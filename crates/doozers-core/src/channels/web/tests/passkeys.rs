//! Passkeys through the real handlers, with a software authenticator (1Password's `passkey-rs`):
//! registering both kinds through an invite, and what must hold when it goes wrong.
//!
//! The browser is [`Browser`]: a `passkey-client` with an authenticator whose store keeps what a
//! real authenticator would (a passkey is discoverable only if it was registered as one). The pages
//! are called as the script calls them, with JSON.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::header::CACHE_CONTROL;
use base64::Engine;
use doozers_store::{PasskeyKind, UserState};
use passkey_authenticator::{
    Authenticator, CredentialStore, DiscoverabilitySupport, StoreInfo, UiHint, UserCheck,
    UserValidationMethod,
};
use passkey_client::{Client, DefaultClientData};
use passkey_crypto::AvailableBackend;
use passkey_types::Passkey as SoftPasskey;
use passkey_types::ctap2::make_credential::{
    Options, PublicKeyCredentialRpEntity, PublicKeyCredentialUserEntity,
};
use passkey_types::ctap2::{Aaguid, Ctap2Error, StatusCode as CtapStatus};
use passkey_types::webauthn::{
    CredentialCreationOptions, CredentialRequestOptions, PublicKeyCredentialDescriptor,
};
use serde_json::{Value, json};
use url::Url;

use super::admin::{open_users, session_of, stored_users};
use super::*;

/// Where passkeys work in these tests.
const ORIGIN: &str = "https://doozers.example.com";

/// The software authenticator's credentials, which a test can reach into.
#[derive(Clone, Default)]
struct SoftStore(Arc<Mutex<Vec<SoftPasskey>>>);

#[async_trait]
impl CredentialStore for SoftStore {
    type PasskeyItem = SoftPasskey;

    async fn find_credentials(
        &self,
        ids: Option<&[PublicKeyCredentialDescriptor]>,
        rp_id: &str,
        user_handle: Option<&[u8]>,
    ) -> Result<Vec<SoftPasskey>, CtapStatus> {
        let found: Vec<_> = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|passkey| passkey.rp_id == rp_id)
            .filter(|passkey| match ids {
                // A named credential is found whether or not it is discoverable.
                Some(ids) if !ids.is_empty() => {
                    ids.iter().any(|id| *id.id == *passkey.credential_id)
                }
                // Nothing named: only what the authenticator keeps for the site.
                _ => passkey.user_handle.is_some(),
            })
            .filter(|passkey| {
                user_handle.is_none_or(|handle| {
                    passkey
                        .user_handle
                        .as_ref()
                        .is_some_and(|stored| stored[..] == *handle)
                })
            })
            .cloned()
            .collect();
        if found.is_empty() {
            Err(Ctap2Error::NoCredentials.into())
        } else {
            Ok(found)
        }
    }

    async fn save_credential(
        &mut self,
        credential: SoftPasskey,
        _user: PublicKeyCredentialUserEntity,
        _rp: PublicKeyCredentialRpEntity,
        _options: Options,
    ) -> Result<(), CtapStatus> {
        let mut all = self.0.lock().unwrap();
        // A discoverable credential replaces the one for the same site and user.
        all.retain(|old| {
            credential.user_handle.is_none()
                || old.rp_id != credential.rp_id
                || old.user_handle != credential.user_handle
        });
        all.push(credential);
        Ok(())
    }

    async fn update_credential(&mut self, credential: &SoftPasskey) -> Result<(), CtapStatus> {
        let mut all = self.0.lock().unwrap();
        all.retain(|old| old.credential_id != credential.credential_id);
        all.push(credential.clone());
        Ok(())
    }

    async fn get_info(&self) -> StoreInfo {
        StoreInfo {
            discoverability: DiscoverabilitySupport::Full,
        }
    }
}

/// Approves everything, with user verification: what a fingerprint or a PIN does.
struct Approves;

#[async_trait]
impl UserValidationMethod for Approves {
    type PasskeyItem = SoftPasskey;

    async fn check_user<'a>(
        &self,
        _hint: UiHint<'a, SoftPasskey>,
        _presence: bool,
        _verification: bool,
    ) -> Result<UserCheck, Ctap2Error> {
        Ok(UserCheck {
            presence: true,
            verification: true,
        })
    }

    fn is_presence_enabled(&self) -> bool {
        true
    }

    fn is_verification_enabled(&self) -> Option<bool> {
        Some(true)
    }
}

/// The bytes `passkey-client` serializes as a JSON array, as the base64url string a browser's
/// script sends.
fn base64url(bytes: &Value) -> String {
    let bytes: Vec<u8> = serde_json::from_value(bytes.clone()).unwrap();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A browser with one authenticator, which does what the server's options say.
struct Browser {
    client: Client<SoftStore, Approves, AvailableBackend, public_suffix::PublicSuffixList, ()>,
    store: SoftStore,
    origin: Url,
}

impl Browser {
    /// An authenticator that counts its signatures when `counter`, as a hardware key does.
    fn new(counter: bool) -> Self {
        let store = SoftStore::default();
        let mut authenticator = Authenticator::new(
            Aaguid::new_empty(),
            store.clone(),
            Approves,
            AvailableBackend,
        );
        authenticator.set_make_credentials_with_signature_counter(counter);
        Self {
            client: Client::new(authenticator),
            store,
            origin: Url::parse(ORIGIN).unwrap(),
        }
    }

    /// `navigator.credentials.create` for the server's `options` (the `options` of its answer):
    /// what the browser sends back.
    async fn create(&mut self, options: &Value) -> Result<Value, passkey_client::WebauthnError> {
        let options: CredentialCreationOptions = serde_json::from_value(options.clone()).unwrap();
        let created = self
            .client
            .register(&self.origin, options, DefaultClientData)
            .await?;
        // What the script sends: the fields the server reads, bytes as base64url.
        let created = serde_json::to_value(created).unwrap();
        let response = &created["response"];
        let mut sent = json!({
            "id": created["id"],
            "rawId": base64url(&created["rawId"]),
            "type": created["type"],
            "response": {
                "attestationObject": base64url(&response["attestationObject"]),
                "clientDataJSON": base64url(&response["clientDataJSON"]),
                "transports": response["transports"],
            },
            "clientExtensionResults": {},
        });
        if let Some(props) = created["clientExtensionResults"].get("credProps") {
            sent["clientExtensionResults"]["credProps"] = props.clone();
        }
        Ok(sent)
    }

    /// `navigator.credentials.get`, with `mediation` removed as the script does for its button.
    async fn get(&mut self, options: &Value) -> Result<Value, passkey_client::WebauthnError> {
        let mut options = options.clone();
        options.as_object_mut().unwrap().remove("mediation");
        let options: CredentialRequestOptions = serde_json::from_value(options).unwrap();
        let asserted = self
            .client
            .authenticate(&self.origin, options, DefaultClientData)
            .await?;
        Ok(serde_json::to_value(asserted).unwrap())
    }

    /// The authenticator's credentials, as `(discoverable, counter)`.
    fn credentials(&self) -> Vec<(bool, Option<u32>)> {
        self.store
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|passkey| (passkey.user_handle.is_some(), passkey.counter))
            .collect()
    }

    /// Sets the signature counter of every credential.
    fn set_counter(&self, counter: u32) {
        for passkey in self.store.0.lock().unwrap().iter_mut() {
            passkey.counter = Some(counter);
        }
    }

    /// Changes the user handle of every discoverable credential, as another account's would be.
    fn set_user_handle(&self, handle: &[u8]) {
        for passkey in self.store.0.lock().unwrap().iter_mut() {
            if passkey.user_handle.is_some() {
                passkey.user_handle = Some(handle.to_vec().into());
            }
        }
    }
}

/// A POST of JSON, as the script makes it.
async fn post_json(
    app: axum::Router,
    path: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> Response {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(CONTENT_TYPE, "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    app.oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

/// The JSON a response carries.
async fn json_of(response: Response) -> Value {
    let text = body_text(response).await;
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{err}: {text}"))
}

/// An environment whose web channel has `public_url` (when given), with the users of
/// [`stored_users`].
async fn env_at(public_url: Option<&str>) -> Env {
    let config = match public_url {
        Some(url) => WebConfig::from_json(&json!({ "public_url": url })).unwrap(),
        None => WebConfig::default(),
    };
    env_with(config, &["web:owner"]).await
}

/// A logged-in browser's headers: its session cookie and CSRF token.
struct Login {
    cookie: String,
    csrf: String,
}

impl Login {
    fn new(token: String) -> Self {
        Self {
            cookie: session(&token),
            csrf: auth::cookie_csrf(&token),
        }
    }

    fn headers(&self) -> [(&str, &str); 2] {
        [("cookie", &self.cookie), ("x-csrf-token", &self.csrf)]
    }
}

/// Logs `user` in with an invite or recovery link.
async fn login(env: &Env, user: &str) -> Login {
    Login::new(super::admin::session_of(env, user).await)
}

/// What a registration page does: start, let `browser` make the credential, finish. The answers of
/// both requests.
async fn register(
    env: &Env,
    login: &Login,
    browser: &mut Browser,
    kind: &str,
    name: &str,
) -> (Value, Response) {
    let response = post_json(
        env.main_app(),
        "/setup/passkey/start",
        &login.headers(),
        &json!({ "kind": kind }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let started = json_of(response).await;
    let created = browser
        .create(&started["options"])
        .await
        .unwrap_or_else(|err| panic!("{err:?}"));
    let response = post_json(
        env.main_app(),
        "/setup/passkey/finish",
        &login.headers(),
        &json!({ "ceremony": started["ceremony"], "name": name, "credential": created }),
    )
    .await;
    (started, response)
}

/// `POST /setup/passkey/start` for a discoverable passkey, with `headers`.
async fn start(env: &Env, headers: &[(&str, &str)]) -> Response {
    post_json(
        env.main_app(),
        "/setup/passkey/start",
        headers,
        &json!({ "kind": "discoverable" }),
    )
    .await
}

#[tokio::test]
async fn the_setup_page_adds_both_kinds_of_passkey_and_lists_them() {
    let env = env_at(Some(ORIGIN)).await;
    let users = open_users(&env).await;
    let newbie = add_user(&users, "newbie", doozers_store::Role::Member).await;
    let invite = invite_token(&env, &newbie, InviteKind::Invite).await;
    let response = redeem(env.main_app(), &invite, &[]).await;
    let login = Login::new(cookie_token(set_cookie(&response)));

    // The setup page offers passkeys.
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup",
        &[("cookie", &login.cookie)],
        None,
    )
    .await;
    let html = body_text(response).await;
    assert!(
        html.contains(r#"<a href="/setup/passkey">Passkey</a>"#),
        "{html}"
    );

    // The page has both buttons and says when to use each, and loads the one script.
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup/passkey",
        &[("cookie", &login.cookie)],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    let html = body_text(response).await;
    for expected in [
        r#"data-passkey-add="discoverable">Add a passkey</button>"#,
        r#"data-passkey-add="non_resident">Add a non-resident key</button>"#,
        "few slots for passkeys",
        "CTAP 2.0",
        &format!(r#"data-origin="{ORIGIN}""#),
        &format!(r#"data-csrf="{}""#, login.csrf),
        &format!(
            r#"<script src="{}" defer></script>"#,
            crate::http::assets::passkey_script_path()
        ),
    ] {
        assert!(html.contains(expected), "{expected}: {html}");
    }
    assert!(!html.contains("Your passkeys"), "none yet: {html}");

    // A phone's passkey, and a hardware key that keeps none.
    let mut phone = Browser::new(false);
    let mut key = Browser::new(true);
    let (started, response) = register(&env, &login, &mut phone, "discoverable", "My phone").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    assert_eq!(
        json_of(response).await,
        json!({ "redirect": "/setup/passkey?saved=1" })
    );
    // The options the discoverable button asks for, over the library's `discouraged`.
    let selection = &started["options"]["publicKey"]["authenticatorSelection"];
    assert_eq!(selection["residentKey"], "required");
    assert_eq!(selection["requireResidentKey"], true);
    assert_eq!(selection["userVerification"], "required");
    assert_eq!(started["options"]["publicKey"]["attestation"], "none");
    assert_eq!(
        started["options"]["publicKey"]["rp"]["id"],
        "doozers.example.com"
    );
    assert_eq!(started["options"]["publicKey"]["rp"]["name"], "Doozers");
    assert_eq!(started["options"]["publicKey"]["user"]["name"], "newbie");
    assert_eq!(
        started["options"]["publicKey"]["user"]["displayName"],
        "NEWBIE"
    );
    assert!(
        started["options"]["publicKey"]
            .get("excludeCredentials")
            .is_none()
    );

    let (started, response) = register(&env, &login, &mut key, "non_resident", "").await;
    assert_eq!(response.status(), StatusCode::OK);
    let selection = &started["options"]["publicKey"]["authenticatorSelection"];
    assert_eq!(selection["residentKey"], "discouraged");
    assert_eq!(selection["requireResidentKey"], false);
    assert_eq!(selection["userVerification"], "required");
    // The user already has a passkey, so the browser is told not to make a second on the same
    // authenticator.
    assert_eq!(
        started["options"]["publicKey"]["excludeCredentials"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // The authenticators kept what they were asked to keep: the passkey is discoverable, so a
    // sign-in with no username can find it, and the key's isn't.
    assert_eq!(phone.credentials(), [(true, None)]);
    assert_eq!(key.credentials(), [(false, Some(0))]);

    // Both are stored, the kind decided by the button, the name by the user or by the page.
    let stored = env.store.user_passkeys(&newbie).await.unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].kind, PasskeyKind::Discoverable);
    assert_eq!(stored[0].name, "My phone");
    assert_eq!(stored[1].kind, PasskeyKind::NonResident);
    assert_eq!(stored[1].name, "Non-resident key 2");
    assert!(stored.iter().all(|passkey| passkey.last_used_at.is_none()));
    // What is stored is the library's credential, with a public key and no private one.
    let credential: Value = serde_json::from_str(&stored[0].credential).unwrap();
    let credential = &credential["cred"];
    assert!(credential["cred"].is_object(), "{credential}");
    assert_eq!(credential["counter"], 0);
    assert!(!stored[0].credential.contains("private"), "{credential}");

    // The page lists them.
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup/passkey?saved=1",
        &[("cookie", &login.cookie)],
        None,
    )
    .await;
    let html = body_text(response).await;
    for expected in [
        "Your passkey is saved",
        "Your passkeys",
        "My phone: Passkey, added ",
        "Non-resident key 2: Non-resident key, added ",
        "last used never",
    ] {
        assert!(html.contains(expected), "{expected}: {html}");
    }
}

#[tokio::test]
async fn the_discoverable_button_is_what_makes_a_passkey_discoverable() {
    // A library that built `discouraged` and a server that didn't override it would make a key
    // that a username-less sign-in can't find. Registered through the non-resident button, the
    // same authenticator keeps nothing it can offer without a username.
    let env = env_at(Some(ORIGIN)).await;
    stored_users(&env).await;
    let login = login(&env, "mia").await;
    let mut browser = Browser::new(false);
    let (_, response) = register(&env, &login, &mut browser, "non_resident", "Key").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(browser.credentials(), [(false, None)]);
    // The same authenticator, through the discoverable button, keeps one it can offer without a
    // username.
    let mut phone = Browser::new(false);
    let (_, response) = register(&env, &login, &mut phone, "discoverable", "Phone").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(phone.credentials(), [(true, None)]);
}

#[tokio::test]
async fn a_passkey_the_browser_says_isnt_discoverable_is_refused_as_one() {
    let env = env_at(Some(ORIGIN)).await;
    stored_users(&env).await;
    let login = login(&env, "mia").await;
    let mut browser = Browser::new(false);
    let started = json_of(
        post_json(
            env.main_app(),
            "/setup/passkey/start",
            &login.headers(),
            &json!({ "kind": "discoverable" }),
        )
        .await,
    )
    .await;
    let mut created = browser.create(&started["options"]).await.unwrap();
    created["clientExtensionResults"] = json!({ "credProps": { "rk": false } });
    let response = post_json(
        env.main_app(),
        "/setup/passkey/finish",
        &login.headers(),
        &json!({ "ceremony": started["ceremony"], "credential": created }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error = json_of(response).await["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(error.contains("Add a non-resident key"), "{error}");
    let mia = UserId::new("mia").unwrap();
    assert!(env.store.user_passkeys(&mia).await.unwrap().is_empty());
}

#[tokio::test]
async fn registering_needs_an_invite_or_recovery_login_and_its_csrf_token() {
    let env = env_at(Some(ORIGIN)).await;
    stored_users(&env).await;
    let mia = UserId::new("mia").unwrap();
    // A login that came from a password isn't enough to add a way in.
    let token = "p".repeat(64);
    env.store
        .web_session_create(
            &token,
            mia.clone(),
            SignInMethod::Password,
            Timestamp::now(),
            Timestamp::now()
                .checked_add(std::time::Duration::from_secs(3600))
                .unwrap(),
        )
        .await
        .unwrap();
    let password = Login::new(token);
    let response = start(&env, &password.headers()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        json_of(response).await["error"]
            .as_str()
            .unwrap()
            .contains("Confirm it's you first")
    );
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup/passkey",
        &[("cookie", &password.cookie)],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup",
        &[("cookie", &password.cookie)],
        None,
    )
    .await;
    let html = body_text(response).await;
    assert!(!html.contains("/setup/passkey"), "{html}");

    // Nobody logged in gets the login-required page.
    let response = start(&env, &[]).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = call(env.main_app(), Method::GET, "/setup/passkey", &[], None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A recovery login still needs the CSRF token of that login.
    let recovery = login(&env, "mia").await;
    let response = start(&env, &[("cookie", &recovery.cookie)]).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = start(
        &env,
        &[("cookie", &recovery.cookie), ("x-csrf-token", "wrong")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        json_of(response).await["error"]
            .as_str()
            .unwrap()
            .contains("out of date")
    );
    let response = start(&env, &recovery.headers()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(env.store.user_passkeys(&mia).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_registration_belongs_to_the_login_that_started_it_and_finishes_once() {
    let env = env_at(Some(ORIGIN)).await;
    stored_users(&env).await;
    let mia = login(&env, "mia").await;
    let root = login(&env, "root").await;
    let mut browser = Browser::new(false);
    let started = json_of(
        post_json(
            env.main_app(),
            "/setup/passkey/start",
            &mia.headers(),
            &json!({ "kind": "discoverable" }),
        )
        .await,
    )
    .await;
    let created = browser.create(&started["options"]).await.unwrap();
    let body = json!({ "ceremony": started["ceremony"], "credential": created });

    // Another user's login can't finish it, and the attempt ends it.
    let response = post_json(
        env.main_app(),
        "/setup/passkey/finish",
        &root.headers(),
        &body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = post_json(
        env.main_app(),
        "/setup/passkey/finish",
        &mia.headers(),
        &body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        json_of(response).await["error"]
            .as_str()
            .unwrap()
            .contains("took too long")
    );

    // A fresh one finishes once.
    let (_, response) = register(&env, &mia, &mut Browser::new(false), "discoverable", "A").await;
    assert_eq!(response.status(), StatusCode::OK);
    let mia_id = UserId::new("mia").unwrap();
    assert_eq!(env.store.user_passkeys(&mia_id).await.unwrap().len(), 1);
    let started = json_of(
        post_json(
            env.main_app(),
            "/setup/passkey/start",
            &mia.headers(),
            &json!({ "kind": "non_resident" }),
        )
        .await,
    )
    .await;
    let created = Browser::new(false)
        .create(&started["options"])
        .await
        .unwrap();
    let body = json!({ "ceremony": started["ceremony"], "credential": created });
    let first = post_json(
        env.main_app(),
        "/setup/passkey/finish",
        &mia.headers(),
        &body,
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let again = post_json(
        env.main_app(),
        "/setup/passkey/finish",
        &mia.headers(),
        &body,
    )
    .await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    assert_eq!(env.store.user_passkeys(&mia_id).await.unwrap().len(), 2);

    // A kind that isn't one is refused.
    let response = post_json(
        env.main_app(),
        "/setup/passkey/start",
        &mia.headers(),
        &json!({ "kind": "both" }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn names_are_trimmed_cut_and_defaulted() {
    let env = env_at(Some(ORIGIN)).await;
    stored_users(&env).await;
    let login = login(&env, "mia").await;
    let long = format!("  {}\u{7}  ", "x".repeat(100));
    for (kind, name) in [("discoverable", long.as_str()), ("discoverable", "   ")] {
        let (_, response) = register(&env, &login, &mut Browser::new(false), kind, name).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let names: Vec<_> = env
        .store
        .user_passkeys(&UserId::new("mia").unwrap())
        .await
        .unwrap()
        .into_iter()
        .map(|passkey| passkey.name)
        .collect();
    assert_eq!(names, ["x".repeat(64), "Passkey 2".to_owned()]);
}

#[tokio::test]
async fn without_a_secure_public_url_passkeys_are_unavailable_and_the_page_says_why() {
    for (public_url, why) in [
        (None, "doesn&#39;t know its own address"),
        (Some("http://doozers.example.com"), "isn&#39;t secure"),
        (Some("http://127.0.0.1:8080"), "is an IP address"),
    ] {
        let env = env_at(public_url).await;
        stored_users(&env).await;
        let login = login(&env, "mia").await;
        let response = call(
            env.main_app(),
            Method::GET,
            "/setup",
            &[("cookie", &login.cookie)],
            None,
        )
        .await;
        let html = body_text(response).await;
        assert!(
            html.contains("Not available here"),
            "{public_url:?}: {html}"
        );
        let response = call(
            env.main_app(),
            Method::GET,
            "/setup/passkey",
            &[("cookie", &login.cookie)],
            None,
        )
        .await;
        let html = body_text(response).await;
        assert!(html.contains(why), "{public_url:?}: {html}");
        assert!(html.contains("public_url"), "{public_url:?}: {html}");
        assert!(!html.contains("data-passkey-add"), "{public_url:?}: {html}");
        let response = post_json(
            env.main_app(),
            "/setup/passkey/start",
            &login.headers(),
            &json!({ "kind": "discoverable" }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT, "{public_url:?}");
    }
    // A browser treats localhost as secure, so passkeys work there.
    let env = env_at(Some("http://localhost:8080")).await;
    stored_users(&env).await;
    let login = login(&env, "mia").await;
    let response = post_json(
        env.main_app(),
        "/setup/passkey/start",
        &login.headers(),
        &json!({ "kind": "discoverable" }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let started = json_of(response).await;
    assert_eq!(started["options"]["publicKey"]["rp"]["id"], "localhost");
}

#[tokio::test]
async fn the_script_is_served_under_the_policy_that_allows_no_inline_script() {
    let env = env_at(Some(ORIGIN)).await;
    stored_users(&env).await;
    let login = login(&env, "mia").await;
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup/passkey",
        &[("cookie", &login.cookie)],
        None,
    )
    .await;
    let policy = response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(policy.contains("script-src 'self';"), "{policy}");
    assert!(policy.contains("connect-src 'self';"), "{policy}");
    assert!(!policy.contains("unsafe"), "{policy}");
    let html = body_text(response).await;
    // Every script on the page is a file, and the page has no handler attributes.
    assert_eq!(
        html.matches("<script").count(),
        html.matches("<script src=").count(),
        "{html}"
    );
    assert!(!html.contains(" onclick="), "{html}");
    assert!(!html.contains(" style="), "{html}");
}

// --- Signing in --------------------------------------------------------------------------------

/// A browser on the login page: the nonce its forms carry, as cookie and body field.
struct Page {
    cookie: String,
}

impl Page {
    fn new() -> Self {
        Self {
            cookie: format!("{NONCE_COOKIE}={NONCE}"),
        }
    }

    /// `POST /login/passkey/<step>` with the nonce, as the script sends it.
    async fn start(&self, env: &Env, step: &str, extra: Value) -> Response {
        let mut body = json!({ "nonce": NONCE });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        post_json(
            env.main_app(),
            &format!("/login/passkey/{step}"),
            &[("cookie", &self.cookie)],
            &body,
        )
        .await
    }

    /// `POST /login/passkey/finish`.
    async fn finish(&self, env: &Env, ceremony: &Value, credential: &Value) -> Response {
        self.finish_with(
            env,
            &[("cookie", &self.cookie)],
            NONCE,
            ceremony,
            credential,
        )
        .await
    }

    async fn finish_with(
        &self,
        env: &Env,
        headers: &[(&str, &str)],
        nonce: &str,
        ceremony: &Value,
        credential: &Value,
    ) -> Response {
        post_json(
            env.main_app(),
            "/login/passkey/finish",
            headers,
            &json!({ "nonce": nonce, "ceremony": ceremony, "credential": credential }),
        )
        .await
    }
}

/// What the script sends of an assertion: the fields the server reads, bytes as base64url.
fn asserted(asserted: &Value) -> Value {
    let response = &asserted["response"];
    json!({
        "id": asserted["id"],
        "rawId": base64url(&asserted["rawId"]),
        "type": asserted["type"],
        "response": {
            "authenticatorData": base64url(&response["authenticatorData"]),
            "clientDataJSON": base64url(&response["clientDataJSON"]),
            "signature": base64url(&response["signature"]),
            "userHandle": if response["userHandle"].is_null() {
                Value::Null
            } else {
                Value::String(base64url(&response["userHandle"]))
            },
        },
    })
}

/// A sign-in through `step` (`discover`, or `username` with `extra`): start, let `browser` answer,
/// finish. The answers of the start and of the finish.
async fn sign_in(env: &Env, browser: &mut Browser, step: &str, extra: Value) -> (Value, Response) {
    let page = Page::new();
    let response = page.start(env, step, extra).await;
    assert_eq!(response.status(), StatusCode::OK);
    let started = json_of(response).await;
    let answered = browser
        .get(&started["options"])
        .await
        .unwrap_or_else(|err| panic!("{err:?}"));
    let response = page
        .finish(env, &started["ceremony"], &asserted(&answered))
        .await;
    (started, response)
}

/// A user `newbie` with a discoverable passkey on `phone` and a non-resident one on `key`, and
/// the environment.
async fn with_both_kinds() -> (Env, UserId, Browser, Browser) {
    let env = env_at(Some(ORIGIN)).await;
    let users = open_users(&env).await;
    let newbie = add_user(&users, "newbie", doozers_store::Role::Member).await;
    let invite = invite_token(&env, &newbie, InviteKind::Invite).await;
    let response = redeem(env.main_app(), &invite, &[]).await;
    let login = Login::new(cookie_token(set_cookie(&response)));
    let mut phone = Browser::new(false);
    let mut key = Browser::new(true);
    let (_, response) = register(&env, &login, &mut phone, "discoverable", "Phone").await;
    assert_eq!(response.status(), StatusCode::OK);
    let (_, response) = register(&env, &login, &mut key, "non_resident", "Key").await;
    assert_eq!(response.status(), StatusCode::OK);
    (env, newbie, phone, key)
}

/// The session a successful finish set, if it did: its token.
fn signed_in_token(response: &Response) -> String {
    let cookies = set_cookies(response);
    assert_eq!(cookies.len(), 2, "{cookies:?}");
    assert!(
        cookies[0].ends_with("; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax; Secure"),
        "{cookies:?}"
    );
    assert_eq!(
        cookies[1],
        format!("{NONCE_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict; Secure")
    );
    cookie_token(cookies[0])
}

/// Asserts that `response` signs nobody in, and says why in the one way it says anything.
async fn assert_didnt_work(response: Response, what: &str) {
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{what}");
    assert!(
        set_cookies(&response)
            .iter()
            .all(|cookie| !cookie.starts_with(SESSION_COOKIE)),
        "{what}"
    );
    let body = json_of(response).await;
    assert_eq!(
        body,
        json!({ "error": "That passkey didn't log you in. Try again, or use another way to log in." }),
        "{what}"
    );
}

#[tokio::test]
async fn both_kinds_of_passkey_sign_in_one_through_each_step() {
    let (env, newbie, mut phone, mut key) = with_both_kinds().await;

    // A discoverable passkey signs in with no username.
    let (started, response) = sign_in(&env, &mut phone, "discover", json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    let options = &started["options"];
    assert_eq!(options["mediation"], "conditional");
    assert_eq!(options["publicKey"]["rpId"], "doozers.example.com");
    assert_eq!(options["publicKey"]["userVerification"], "required");
    assert_eq!(options["publicKey"]["allowCredentials"], json!([]));
    let token = signed_in_token(&response);
    assert_eq!(json_of(response).await, json!({ "redirect": "/" }));
    let session = env
        .store
        .web_session(&token, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.user, newbie);
    assert_eq!(session.method, SignInMethod::Passkey);
    // The session works.
    let response = call(
        env.main_app(),
        Method::GET,
        "/",
        &[("cookie", &session_cookie_header(&token))],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // A key that keeps no passkey can't be found without a username.
    let page = Page::new();
    let started = json_of(page.start(&env, "discover", json!({})).await).await;
    assert!(key.get(&started["options"]).await.is_err());

    // With the username, the server names the user's credentials, and the key answers.
    let (started, response) =
        sign_in(&env, &mut key, "username", json!({ "username": "NewBie" })).await;
    assert_eq!(response.status(), StatusCode::OK);
    let token = signed_in_token(&response);
    let options = &started["options"];
    assert!(options.get("mediation").is_none());
    assert_eq!(
        options["publicKey"]["allowCredentials"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let session = env
        .store
        .web_session(&token, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (session.user, session.method),
        (newbie.clone(), SignInMethod::Passkey)
    );

    // Both passkeys were used, and the key's counter moved.
    let stored = env.store.user_passkeys(&newbie).await.unwrap();
    assert!(
        stored.iter().all(|passkey| passkey.last_used_at.is_some()),
        "{stored:?}"
    );
    let credential: Value = serde_json::from_str(&stored[1].credential).unwrap();
    assert_eq!(credential["cred"]["counter"], 1);
}

fn session_cookie_header(token: &str) -> String {
    session(token)
}

#[tokio::test]
async fn a_sign_in_makes_a_new_session_and_ends_the_browsers_old_one() {
    let (env, newbie, mut phone, _) = with_both_kinds().await;
    let old = session_of(&env, "newbie").await;
    let page = Page::new();
    let started = json_of(page.start(&env, "discover", json!({})).await).await;
    let answered = phone.get(&started["options"]).await.unwrap();
    let cookie = format!("{NONCE_COOKIE}={NONCE}; {}", session(&old));
    let response = page
        .finish_with(
            &env,
            &[("cookie", &cookie)],
            NONCE,
            &started["ceremony"],
            &asserted(&answered),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let token = signed_in_token(&response);
    assert_ne!(token, old);
    let now = Timestamp::now();
    assert!(env.store.web_session(&old, now).await.unwrap().is_none());
    assert!(env.store.web_session(&token, now).await.unwrap().is_some());
    let _ = newbie;
}

#[tokio::test]
async fn the_username_step_answers_alike_for_a_real_user_and_for_names_that_have_nothing() {
    let (env, _, _, _) = with_both_kinds().await;
    stored_users(&env).await;
    // `mia` is a user with no passkeys, `off` a user who can't sign in, and `ghost` isn't one.
    let users = open_users(&env).await;
    let off = add_user(&users, "off", doozers_store::Role::Member).await;
    users
        .set_state(&off, UserState::Disabled, Timestamp::now())
        .await
        .unwrap();

    let page = Page::new();
    let mut answers = Vec::new();
    for name in ["newbie", "mia", "ghost", "off"] {
        let response = page
            .start(&env, "username", json!({ "username": name }))
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{name}");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{name}");
        answers.push((name, json_of(response).await));
    }
    for (name, answer) in &answers {
        let object = answer.as_object().unwrap();
        assert_eq!(
            object.keys().map(String::as_str).collect::<Vec<_>>(),
            ["ceremony", "options"],
            "{name}"
        );
        assert_eq!(answer["ceremony"].as_str().unwrap().len(), 32, "{name}");
        let public_key = answer["options"]["publicKey"].as_object().unwrap();
        let mut keys: Vec<_> = public_key.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "allowCredentials",
                "challenge",
                "rpId",
                "timeout",
                "userVerification"
            ],
            "{name}"
        );
        assert!(answer["options"].get("mediation").is_none(), "{name}");
        assert_eq!(public_key["rpId"], "doozers.example.com", "{name}");
        assert_eq!(
            public_key["timeout"],
            answers[0].1["options"]["publicKey"]["timeout"]
        );
        // Every listed credential is only a type and an id: no transports, which a made-up id
        // couldn't have.
        for credential in public_key["allowCredentials"].as_array().unwrap() {
            let mut keys: Vec<_> = credential
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, ["id", "type"], "{name}");
            assert_eq!(credential["type"], "public-key", "{name}");
            assert!(credential["id"].as_str().unwrap().len() >= 20, "{name}");
        }
    }
    // The real user's answer lists their two credentials.
    let listed = |name: &str| {
        answers.iter().find(|(n, _)| *n == name).unwrap().1["options"]["publicKey"]["allowCredentials"]
            .as_array()
            .unwrap()
            .clone()
    };
    assert_eq!(listed("newbie").len(), 2);

    // A made-up answer is the same every time for a name, whatever its case, differs between
    // names, and is the same after a restart (the key is stored).
    let ids = |value: &Value| -> Vec<String> {
        value["options"]["publicKey"]["allowCredentials"]
            .as_array()
            .unwrap()
            .iter()
            .map(|credential| credential["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let again = json_of(
        page.start(&env, "username", json!({ "username": "GHOST" }))
            .await,
    )
    .await;
    let ghost = &answers.iter().find(|(n, _)| *n == "ghost").unwrap().1;
    assert_eq!(ids(&again), ids(ghost));
    let restarted = WebApp::new(
        env.channel.clone(),
        env.store.clone(),
        env.blobs.clone(),
        env.registry.live_config().clone(),
        env.jobs.clone(),
        env.shutdown.clone(),
    );
    let response = post_json(
        restarted.install(RouterBuilder::new()).build(),
        "/login/passkey/username",
        &[("cookie", &page.cookie)],
        &json!({ "nonce": NONCE, "username": "ghost" }),
    )
    .await;
    assert_eq!(ids(&json_of(response).await), ids(ghost));
    let mut distinct = std::collections::HashSet::new();
    for n in 0..40 {
        let name = format!("nobody-{n}");
        let answer = json_of(
            page.start(&env, "username", json!({ "username": name }))
                .await,
        )
        .await;
        distinct.insert(ids(&answer));
    }
    assert!(distinct.len() > 20, "{} different answers", distinct.len());

    // Finishing a made-up ceremony fails as a wrong passkey does for a real user.
    let junk = json!({
        "id": "AAAA", "rawId": "AAAA", "type": "public-key",
        "response": { "authenticatorData": "AAAA", "clientDataJSON": "AAAA", "signature": "AAAA", "userHandle": null },
    });
    for name in ["ghost", "mia", "newbie"] {
        let started = json_of(
            page.start(&env, "username", json!({ "username": name }))
                .await,
        )
        .await;
        let response = page.finish(&env, &started["ceremony"], &junk).await;
        assert_didnt_work(response, name).await;
    }
}

#[tokio::test]
async fn a_disabled_users_passkey_is_refused_like_any_other_failure() {
    let (env, newbie, mut phone, mut key) = with_both_kinds().await;
    let users = open_users(&env).await;
    users
        .set_state(&newbie, UserState::Disabled, Timestamp::now())
        .await
        .unwrap();
    let (_, response) = sign_in(&env, &mut phone, "discover", json!({})).await;
    assert_didnt_work(response, "discoverable").await;
    // Through the username step the disabled user's answer is made up, so the key's credential
    // isn't on the list, and it can't answer.
    let page = Page::new();
    let started = json_of(
        page.start(&env, "username", json!({ "username": "newbie" }))
            .await,
    )
    .await;
    assert!(key.get(&started["options"]).await.is_err());
    let stored = env.store.user_passkeys(&newbie).await.unwrap();
    assert!(stored.iter().all(|passkey| passkey.last_used_at.is_none()));
}

#[tokio::test]
async fn a_passkey_that_names_another_user_is_refused() {
    let (env, newbie, mut phone, _) = with_both_kinds().await;
    stored_users(&env).await;
    // The phone's passkey is newbie's, but the authenticator reports the handle of another user
    // (a well-formed one, which the library accepts). The server's own record of the owner
    // decides, so neither step signs anyone in.
    let mia = UserId::new("mia").unwrap();
    phone.set_user_handle(super::super::passkey::user_handle(&mia).as_bytes());
    let before = env.store.user_passkeys(&newbie).await.unwrap();
    let (_, response) = sign_in(&env, &mut phone, "discover", json!({})).await;
    assert_didnt_work(response, "discoverable").await;
    // The same through the username step, for the real owner: the answer names the other user.
    let (_, response) = sign_in(
        &env,
        &mut phone,
        "username",
        json!({ "username": "newbie" }),
    )
    .await;
    assert_didnt_work(response, "by username").await;
    // Nothing was recorded as used.
    assert_eq!(env.store.user_passkeys(&newbie).await.unwrap(), before);
    assert!(before.iter().all(|passkey| passkey.last_used_at.is_none()));
}

#[tokio::test]
async fn a_signature_counter_that_does_not_advance_is_refused_and_leaves_the_passkey() {
    let (env, newbie, _, mut key) = with_both_kinds().await;
    for _ in 0..2 {
        let (_, response) =
            sign_in(&env, &mut key, "username", json!({ "username": "newbie" })).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    let before = env.store.user_passkeys(&newbie).await.unwrap()[1].clone();
    let counter: Value = serde_json::from_str(&before.credential).unwrap();
    assert_eq!(counter["cred"]["counter"], 2);

    // The authenticator's counter goes back: a clone of it has been used. Not a sign-in.
    key.set_counter(0);
    let (_, response) = sign_in(&env, &mut key, "username", json!({ "username": "newbie" })).await;
    assert_didnt_work(response, "a counter that went back").await;
    let after = env.store.user_passkeys(&newbie).await.unwrap()[1].clone();
    assert_eq!(after.credential, before.credential);
    assert_eq!(after.last_used_at, before.last_used_at);

    // Past the stored one, it signs in again.
    key.set_counter(9);
    let (_, response) = sign_in(&env, &mut key, "username", json!({ "username": "newbie" })).await;
    assert_eq!(response.status(), StatusCode::OK);
    let after = env.store.user_passkeys(&newbie).await.unwrap()[1].clone();
    let counter: Value = serde_json::from_str(&after.credential).unwrap();
    assert_eq!(counter["cred"]["counter"], 10);
}

#[tokio::test]
async fn every_sign_in_post_needs_its_browsers_nonce() {
    let (env, _, mut phone, _) = with_both_kinds().await;
    let page = Page::new();
    let body = json!({ "nonce": NONCE });
    // A start without the cookie, with another cookie, with another nonce, and with none.
    let other = format!("{NONCE_COOKIE}=ffffffffffffffffffffffffffffffff");
    for (headers, body) in [
        (vec![], body.clone()),
        (vec![("cookie", other.as_str())], body.clone()),
        (
            vec![("cookie", page.cookie.as_str())],
            json!({ "nonce": "wrong" }),
        ),
        (vec![("cookie", page.cookie.as_str())], json!({})),
    ] {
        for step in ["discover", "username"] {
            let response = post_json(
                env.main_app(),
                &format!("/login/passkey/{step}"),
                &headers,
                &body,
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{step} {headers:?} {body}"
            );
            assert!(
                json_of(response).await["error"]
                    .as_str()
                    .unwrap()
                    .contains("out of date")
            );
        }
    }

    // A ceremony belongs to the browser that started it: another nonce can't finish it, even
    // with a good answer, and the attempt ends it.
    let started = json_of(page.start(&env, "discover", json!({})).await).await;
    let answered = asserted(&phone.get(&started["options"]).await.unwrap());
    let theirs = "0123456789abcdef0123456789abcde0";
    let cookie = format!("{NONCE_COOKIE}={theirs}");
    let response = page
        .finish_with(
            &env,
            &[("cookie", &cookie)],
            theirs,
            &started["ceremony"],
            &answered,
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = page.finish(&env, &started["ceremony"], &answered).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // And the finish itself needs a nonce.
    let started = json_of(page.start(&env, "discover", json!({})).await).await;
    let answered = asserted(&phone.get(&started["options"]).await.unwrap());
    let response = page
        .finish_with(&env, &[], NONCE, &started["ceremony"], &answered)
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // No session was made by any of it.
    let response = page.finish(&env, &started["ceremony"], &answered).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the ceremony outlived a refused post"
    );
}

#[tokio::test]
async fn a_ceremony_signs_in_once() {
    let (env, _, mut phone, _) = with_both_kinds().await;
    let page = Page::new();
    let started = json_of(page.start(&env, "discover", json!({})).await).await;
    let answered = asserted(&phone.get(&started["options"]).await.unwrap());
    let first = page.finish(&env, &started["ceremony"], &answered).await;
    assert_eq!(first.status(), StatusCode::OK);
    let again = page.finish(&env, &started["ceremony"], &answered).await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    assert!(
        json_of(again).await["error"]
            .as_str()
            .unwrap()
            .contains("took too long")
    );
    // A registration's ceremony isn't a sign-in's.
    let login = login(&env, "newbie").await;
    let registration = json_of(
        post_json(
            env.main_app(),
            "/setup/passkey/start",
            &login.headers(),
            &json!({ "kind": "discoverable" }),
        )
        .await,
    )
    .await;
    let response = page
        .finish(&env, &registration["ceremony"], &answered)
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn starting_is_limited_per_client_address() {
    let env = env_at(Some(ORIGIN)).await;
    let page = Page::new();
    let from = |peer: &'static str| {
        let (env, page) = (&env, &page);
        async move {
            let mut request = Request::builder()
                .method(Method::POST)
                .uri("/login/passkey/discover")
                .header(CONTENT_TYPE, "application/json")
                .header("cookie", &page.cookie)
                .body(Body::from(json!({ "nonce": NONCE }).to_string()))
                .unwrap();
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(crate::http::PeerAddr(Some(
                    peer.parse().unwrap(),
                ))));
            env.main_app().oneshot(request).await.unwrap()
        }
    };
    for _ in 0..crate::channels::web::passkey_signin::START_ATTEMPTS {
        assert_eq!(from("192.0.2.1").await.status(), StatusCode::OK);
    }
    let response = from("192.0.2.1").await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key("retry-after"));
    // Another address is another count.
    assert_eq!(from("192.0.2.2").await.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_login_page_offers_passkeys_above_a_folded_password_form() {
    let env = env_at(Some(ORIGIN)).await;
    let response = call(env.main_app(), Method::GET, "/login", &[], None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let nonce = nonce_of(set_cookie(&response));
    let policy = response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        policy.contains("script-src 'self';") && !policy.contains("unsafe"),
        "{policy}"
    );
    let html = body_text(response).await;
    for expected in [
        r#"<section id="passkey-signin" hidden"#.to_owned(),
        format!(r#"data-nonce="{nonce}""#),
        format!(r#"data-origin="{ORIGIN}""#),
        r#"data-discover-url="/login/passkey/discover""#.to_owned(),
        r#"data-username-url="/login/passkey/username""#.to_owned(),
        r#"data-finish-url="/login/passkey/finish""#.to_owned(),
        r#"autocomplete="username webauthn""#.to_owned(),
        "Sign in with a passkey</button>".to_owned(),
        r#"<details class="password-signin">"#.to_owned(),
        "<summary>Use a password instead</summary>".to_owned(),
        format!(
            r#"<script src="{}" defer></script>"#,
            crate::http::assets::passkey_script_path()
        ),
    ] {
        assert!(html.contains(&expected), "{expected}: {html}");
    }
    assert!(
        html.find("passkey-signin").unwrap() < html.find("password-signin").unwrap(),
        "{html}"
    );
    assert_eq!(
        html.matches("<script").count(),
        html.matches("<script src=").count(),
        "{html}"
    );

    // Without passkeys the password form is shown, with no script of ours.
    for public_url in [None, Some("http://doozers.example.com")] {
        let env = env_at(public_url).await;
        let response = call(env.main_app(), Method::GET, "/login", &[], None).await;
        let html = body_text(response).await;
        assert!(!html.contains("passkey"), "{public_url:?}: {html}");
        assert!(!html.contains("<details"), "{public_url:?}: {html}");
        assert!(
            html.contains("<h2>Log in with a password</h2>"),
            "{public_url:?}: {html}"
        );
        let response = Page::new().start(&env, "discover", json!({})).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{public_url:?}");
        let response = Page::new().finish(&env, &json!("x"), &json!({})).await;
        assert!(response.status().is_client_error(), "{public_url:?}");
    }
}

// --- Confirming it's you with a passkey -------------------------------------------------------

/// `mia`, who has one discoverable passkey on the returned browser, and a session of hers that
/// signed in with a password, which is what a stolen cookie is.
async fn with_a_passkey_and_a_password_login() -> (Env, Browser, Login) {
    let env = env_at(Some(ORIGIN)).await;
    stored_users(&env).await;
    let recovery = login(&env, "mia").await;
    let mut browser = Browser::new(false);
    let (_, response) = register(&env, &recovery, &mut browser, "discoverable", "Phone").await;
    assert_eq!(response.status(), StatusCode::OK);
    let stolen = password_login(&env, "mia").await;
    (env, browser, stolen)
}

async fn password_login(env: &Env, user: &str) -> Login {
    let token = random_hex(32);
    let now = Timestamp::now();
    env.store
        .web_session_create(
            &token,
            UserId::new(user).unwrap(),
            SignInMethod::Password,
            now,
            now.checked_add(std::time::Duration::from_secs(3600))
                .unwrap(),
        )
        .await
        .unwrap();
    Login::new(token)
}

/// What the page's script does to confirm it's you: start, let `browser` answer, finish. The answers
/// of both requests.
async fn reverify(
    env: &Env,
    login: &Login,
    browser: &mut Browser,
    next: &str,
) -> (Value, Response) {
    let response = post_json(
        env.main_app(),
        "/account/reverify/passkey/start",
        &login.headers(),
        &json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let started = json_of(response).await;
    let answered = browser
        .get(&started["options"])
        .await
        .unwrap_or_else(|err| panic!("{err:?}"));
    let response = post_json(
        env.main_app(),
        "/account/reverify/passkey/finish",
        &login.headers(),
        &json!({
            "ceremony": started["ceremony"],
            "next": next,
            "credential": asserted(&answered),
        }),
    )
    .await;
    (started, response)
}

async fn begin(env: &Env, login: &Login) -> Response {
    post_json(
        env.main_app(),
        "/account/reverify/passkey/start",
        &login.headers(),
        &json!({}),
    )
    .await
}

async fn finish(env: &Env, login: &Login, body: Value) -> Response {
    post_json(
        env.main_app(),
        "/account/reverify/passkey/finish",
        &login.headers(),
        &body,
    )
    .await
}

async fn reverified_at(env: &Env, login: &Login) -> Option<Timestamp> {
    let token = login
        .cookie
        .strip_prefix(&format!("{SESSION_COOKIE}="))
        .unwrap();
    env.store
        .web_session(token, Timestamp::now())
        .await
        .unwrap()
        .unwrap()
        .reverified_at
}

#[tokio::test]
async fn confirming_with_a_passkey_lets_that_session_add_a_passkey() {
    let (env, mut phone, stolen) = with_a_passkey_and_a_password_login().await;
    let mia = UserId::new("mia").unwrap();

    // A stolen cookie can't add a passkey, and its page offers the user's own passkey.
    let response = start(&env, &stolen.headers()).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        json_of(response).await["error"]
            .as_str()
            .unwrap()
            .contains("Confirm it's you first")
    );
    let page = call(
        env.main_app(),
        Method::GET,
        "/account/reverify?next=/setup/passkey",
        &[("cookie", &stolen.cookie)],
        None,
    )
    .await;
    let html = body_text(page).await;
    assert!(html.contains(r#"id="passkey-reverify""#), "{html}");
    assert!(html.contains(r#"data-next="/setup/passkey""#), "{html}");
    assert!(
        html.contains(&format!(r#"data-origin="{ORIGIN}""#)),
        "{html}"
    );
    assert!(
        html.contains(crate::http::assets::passkey_script_path()),
        "{html}"
    );

    // The user's passkey answers, and the session is confirmed.
    let (started, response) = reverify(&env, &stolen, &mut phone, "/setup/passkey").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    assert_eq!(
        json_of(response).await,
        json!({ "redirect": "/setup/passkey" })
    );
    assert_eq!(
        started["options"]["publicKey"]["allowCredentials"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(reverified_at(&env, &stolen).await.is_some());
    let used = env.store.user_passkeys(&mia).await.unwrap();
    assert!(used[0].last_used_at.is_some());

    // Now that session adds a second passkey, as an invite's would.
    let mut key = Browser::new(true);
    let (_, response) = register(&env, &stolen, &mut key, "non_resident", "Key").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(env.store.user_passkeys(&mia).await.unwrap().len(), 2);

    // Another session of the user didn't confirm anything.
    let other = password_login(&env, "mia").await;
    assert_eq!(
        start(&env, &other.headers()).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn a_confirmation_ceremony_is_the_users_own_and_belongs_to_one_login_and_works_once() {
    let (env, mut phone, stolen) = with_a_passkey_and_a_password_login().await;
    let root_login = login(&env, "root").await;
    let mut roots = Browser::new(false);
    let (_, response) = register(&env, &root_login, &mut roots, "discoverable", "Root").await;
    assert_eq!(response.status(), StatusCode::OK);

    // Another user's passkey can't answer the options of this user's.
    let started = json_of(begin(&env, &stolen).await).await;
    assert!(roots.get(&started["options"]).await.is_err());

    // A ceremony finishes only in the login that started it.
    let answered = phone.get(&started["options"]).await.unwrap();
    let elsewhere = password_login(&env, "mia").await;
    let response = finish(
        &env,
        &elsewhere,
        json!({ "ceremony": started["ceremony"], "credential": asserted(&answered) }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    // It was used up by trying, so the right login can't finish it now either.
    let response = finish(
        &env,
        &stolen,
        json!({ "ceremony": started["ceremony"], "credential": asserted(&answered) }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(reverified_at(&env, &stolen).await, None);
    assert_eq!(reverified_at(&env, &elsewhere).await, None);

    // A ceremony finishes once.
    let (started, response) = reverify(&env, &stolen, &mut phone, "/account").await;
    assert_eq!(response.status(), StatusCode::OK);
    let answered = phone.get(&started["options"]).await.unwrap();
    let response = finish(
        &env,
        &stolen,
        json!({ "ceremony": started["ceremony"], "credential": asserted(&answered) }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Each request needs the login's CSRF token and a login.
    for headers in [
        vec![("cookie", stolen.cookie.as_str())],
        vec![
            ("cookie", stolen.cookie.as_str()),
            ("x-csrf-token", "wrong"),
        ],
    ] {
        let response = post_json(
            env.main_app(),
            "/account/reverify/passkey/start",
            &headers,
            &json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let response = post_json(
        env.main_app(),
        "/account/reverify/passkey/start",
        &[],
        &json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A user with no passkeys has none to use.
    let mia_without = password_login(&env, "root").await;
    let started = begin(&env, &mia_without).await;
    assert_eq!(started.status(), StatusCode::OK, "root has one");
    let nobody = {
        let users = open_users(&env).await;
        let new = doozers_store::NewUser {
            id: UserId::new("nokey").unwrap(),
            username: "nokey".to_owned(),
            display_name: "No key".to_owned(),
            role: doozers_store::Role::Member,
            timezone: None,
            default_identity: None,
        };
        let web = doozers_store::NewIdentity {
            namespace: "web".to_owned(),
            native_id: "nokey".to_owned(),
            verification: doozers_store::Verification::Admin,
        };
        users
            .create(new, vec![web], Timestamp::now())
            .await
            .unwrap();
        password_login(&env, "nokey").await
    };
    assert_eq!(begin(&env, &nobody).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn without_a_secure_public_url_a_passkey_cannot_confirm_and_is_not_offered() {
    let env = env_at(None).await;
    stored_users(&env).await;
    let mia = UserId::new("mia").unwrap();
    // A passkey stored before the install lost its address.
    env.store
        .user_passkey_add(
            doozers_store::NewPasskey {
                user: mia,
                credential_id: vec![1, 2, 3],
                credential: "{}".to_owned(),
                kind: PasskeyKind::Discoverable,
                name: "Old".to_owned(),
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    let session = password_login(&env, "mia").await;
    let response = post_json(
        env.main_app(),
        "/account/reverify/passkey/start",
        &session.headers(),
        &json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let page = call(
        env.main_app(),
        Method::GET,
        "/account/reverify",
        &[("cookie", &session.cookie)],
        None,
    )
    .await;
    assert!(!body_text(page).await.contains("passkey-reverify"));
}
