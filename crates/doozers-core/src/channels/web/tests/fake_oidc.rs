//! An OpenID Connect provider for tests, in this process on a loopback port: a discovery
//! document, a key set, and a token endpoint that signs ID tokens with ES256 keys it generates.
//! It checks what a real provider would of the authorization request and of the token request
//! (the client, the redirect URI, the single-use code, PKCE with S256), and a test can make it
//! misbehave in the ways [`Tamper`] lists, to see that the sign-in refuses the result.
//!
//! The browser is the test: it takes the authorization URL Doozers redirected to and hands it to
//! [`FakeIdp::approve`], which answers with the URL the provider would send the browser back to.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aws_lc_rs::hmac;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use axum::extract::{Form, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use url::Url;

/// A signing key.
pub(super) struct Key {
    pub(super) kid: String,
    pair: EcdsaKeyPair,
}

impl Key {
    pub(super) fn new(kid: &str) -> Self {
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .expect("a key");
        Self {
            kid: kid.to_owned(),
            pair: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref())
                .expect("a key pair"),
        }
    }

    /// The key as a JWK.
    fn jwk(&self) -> Value {
        let point = self.pair.public_key().as_ref();
        json!({
            "kty": "EC",
            "crv": "P-256",
            "kid": self.kid,
            "use": "sig",
            "alg": "ES256",
            "x": URL_SAFE_NO_PAD.encode(&point[1..33]),
            "y": URL_SAFE_NO_PAD.encode(&point[33..65]),
        })
    }

    fn sign(&self, input: &str) -> Vec<u8> {
        self.pair
            .sign(&SystemRandom::new(), input.as_bytes())
            .expect("a signature")
            .as_ref()
            .to_vec()
    }
}

/// How the provider misbehaves when it signs the next ID tokens. The default is a good token.
#[derive(Clone, Default)]
pub(super) struct Tamper {
    /// The `iss` claim.
    pub(super) issuer: Option<String>,
    /// The `aud` claim.
    pub(super) audience: Option<Value>,
    /// The `azp` claim.
    pub(super) azp: Option<String>,
    /// The `sub` claim.
    pub(super) subject: Option<String>,
    /// Seconds from now to `exp` (the default is 300).
    pub(super) expires_in: Option<i64>,
    /// Seconds from now to `iat` (the default is 0).
    pub(super) issued_in: Option<i64>,
    /// The `nonce` claim: `Some(None)` leaves it out.
    pub(super) nonce: Option<Option<String>>,
    /// The `kid` in the header.
    pub(super) kid: Option<String>,
    /// The `alg` in the header. `none` signs nothing, and `HS256` signs with the client secret.
    pub(super) alg: Option<String>,
    /// The key that signs, by `kid`, published or not.
    pub(super) signer: Option<String>,
    /// Flips a bit of the signature.
    pub(super) corrupt_signature: bool,
    /// Answers the token request with no ID token.
    pub(super) no_id_token: bool,
}

/// What the provider has been asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Requests {
    pub(super) discovery: usize,
    pub(super) jwks: usize,
    pub(super) token: usize,
}

struct Grant {
    nonce: String,
    challenge: String,
    redirect_uri: String,
    subject: String,
}

struct Inner {
    issuer: String,
    client_id: String,
    client_secret: Option<String>,
    /// The signing keys, and whether the key set publishes each.
    keys: Vec<(Key, bool)>,
    tamper: Tamper,
    discovery: Value,
    /// Whether the token endpoint wants the client's credentials in the form body.
    form_credentials: bool,
    codes: HashMap<String, Grant>,
    requests: Requests,
    /// The form of the last token request.
    last_token_form: HashMap<String, String>,
}

/// The fake provider. It stops when dropped.
pub(super) struct FakeIdp {
    /// The issuer: `http://127.0.0.1:<port>`.
    pub(super) issuer: String,
    shared: Arc<Mutex<Inner>>,
    server: JoinHandle<()>,
}

impl Drop for FakeIdp {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn lock(shared: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeIdp {
    /// A provider that knows the client `client_id` (with `client_secret` if it has one) and
    /// signs with one key, `k1`.
    pub(super) async fn start(client_id: &str, client_secret: Option<&str>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let discovery = json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["ES256"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
        });
        let shared = Arc::new(Mutex::new(Inner {
            issuer: issuer.clone(),
            client_id: client_id.to_owned(),
            client_secret: client_secret.map(str::to_owned),
            keys: vec![(Key::new("k1"), true)],
            tamper: Tamper::default(),
            discovery,
            form_credentials: false,
            codes: HashMap::new(),
            requests: Requests::default(),
            last_token_form: HashMap::new(),
        }));
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(discovery_document))
            .route("/jwks", get(key_set))
            .route("/token", post(token))
            .with_state(shared.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            issuer,
            shared,
            server,
        }
    }

    /// Changes how it signs from now on.
    pub(super) fn tamper(&self, change: impl FnOnce(&mut Tamper)) {
        change(&mut lock(&self.shared).tamper);
    }

    /// Back to good tokens.
    pub(super) fn behave(&self) {
        lock(&self.shared).tamper = Tamper::default();
    }

    /// Edits the discovery document.
    pub(super) fn edit_discovery(&self, change: impl FnOnce(&mut Value)) {
        change(&mut lock(&self.shared).discovery);
    }

    /// Adds a signing key `kid`, published or not.
    pub(super) fn add_key(&self, kid: &str, published: bool) {
        lock(&self.shared).keys.push((Key::new(kid), published));
    }

    /// Wants the client's credentials in the form body of the token request, and refuses HTTP
    /// Basic.
    pub(super) fn form_credentials(&self, on: bool) {
        lock(&self.shared).form_credentials = on;
        self.edit_discovery(|doc| {
            doc["token_endpoint_auth_methods_supported"] = if on {
                json!(["client_secret_post"])
            } else {
                json!(["client_secret_basic", "client_secret_post"])
            };
        });
    }

    /// What it has been asked.
    pub(super) fn requests(&self) -> Requests {
        lock(&self.shared).requests.clone()
    }

    /// The form of the last token request.
    pub(super) fn last_token_form(&self) -> HashMap<String, String> {
        lock(&self.shared).last_token_form.clone()
    }

    /// The user approves the authorization request `url` as `subject`: checks it as a provider
    /// would, and returns the URL that sends the browser back, with a code. `Err` says what is
    /// wrong with the request.
    pub(super) fn approve(&self, url: &str, subject: &str) -> Result<String, String> {
        let url = Url::parse(url).map_err(|err| err.to_string())?;
        let mut inner = lock(&self.shared);
        if url.as_str().split('?').next() != Some(&format!("{}/authorize", inner.issuer)) {
            return Err(format!("not this provider's authorization endpoint: {url}"));
        }
        let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
        let get = |name: &str| {
            query
                .get(name)
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or_else(|| format!("no {name}"))
        };
        if get("response_type")? != "code" {
            return Err("not the code flow".to_owned());
        }
        if get("client_id")? != inner.client_id {
            return Err("an unknown client".to_owned());
        }
        if !get("scope")?.split(' ').any(|scope| scope == "openid") {
            return Err("no openid scope".to_owned());
        }
        if get("code_challenge_method")? != "S256" {
            return Err("PKCE isn't S256".to_owned());
        }
        let state = get("state")?;
        let redirect_uri = get("redirect_uri")?;
        let code = crate::channels::web::random_hex(16);
        inner.codes.insert(
            code.clone(),
            Grant {
                nonce: get("nonce")?,
                challenge: get("code_challenge")?,
                redirect_uri: redirect_uri.clone(),
                subject: subject.to_owned(),
            },
        );
        let mut back = Url::parse(&redirect_uri).map_err(|err| err.to_string())?;
        back.query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &state);
        Ok(back.to_string())
    }
}

async fn discovery_document(State(shared): State<Arc<Mutex<Inner>>>) -> Json<Value> {
    let mut inner = lock(&shared);
    inner.requests.discovery += 1;
    Json(inner.discovery.clone())
}

async fn key_set(State(shared): State<Arc<Mutex<Inner>>>) -> Json<Value> {
    let mut inner = lock(&shared);
    inner.requests.jwks += 1;
    let keys: Vec<Value> = inner
        .keys
        .iter()
        .filter(|(_, published)| *published)
        .map(|(key, _)| key.jwk())
        .collect();
    Json(json!({ "keys": keys }))
}

fn oauth_error(status: StatusCode, error: &str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": error })))
}

async fn token(
    State(shared): State<Arc<Mutex<Inner>>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    let mut inner = lock(&shared);
    inner.requests.token += 1;
    inner.last_token_form = form.clone();

    // The client authenticates with HTTP Basic or in the form, with its secret if it has one.
    let basic = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|value| STANDARD.decode(value).ok())
        .and_then(|value| String::from_utf8(value).ok())
        .and_then(|value| {
            // The ids and secrets of these tests have nothing to percent-decode.
            let (id, secret) = value.split_once(':')?;
            Some((id.to_owned(), secret.to_owned()))
        });
    let (id, secret) = match basic {
        Some(_) if inner.form_credentials => {
            return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
        }
        Some((id, secret)) => (Some(id), Some(secret)),
        None => (
            form.get("client_id").cloned(),
            form.get("client_secret").cloned(),
        ),
    };
    let secret_ok = match &inner.client_secret {
        Some(expected) => secret.as_deref() == Some(expected.as_str()),
        None => true,
    };
    if id.as_deref() != Some(inner.client_id.as_str()) || !secret_ok {
        return oauth_error(StatusCode::UNAUTHORIZED, "invalid_client");
    }

    if form.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type");
    }
    // A code works once.
    let Some(grant) = form.get("code").and_then(|code| inner.codes.remove(code)) else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    if form.get("redirect_uri") != Some(&grant.redirect_uri) {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let verifier = form.get("code_verifier").map_or("", String::as_str);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    if verifier.len() < 43 || challenge != grant.challenge {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_grant");
    }

    let mut response = json!({
        "access_token": "access-token",
        "token_type": "Bearer",
        "expires_in": 3600,
    });
    if !inner.tamper.no_id_token {
        response["id_token"] = Value::String(mint(&inner, &grant));
    }
    (StatusCode::OK, Json(response))
}

/// The ID token for `grant`, as [`Tamper`] says.
fn mint(inner: &Inner, grant: &Grant) -> String {
    let tamper = &inner.tamper;
    let now = doozers_types::Timestamp::now().as_second();
    let mut claims = json!({
        "iss": tamper.issuer.clone().unwrap_or_else(|| inner.issuer.clone()),
        "sub": tamper.subject.clone().unwrap_or_else(|| grant.subject.clone()),
        "aud": tamper.audience.clone().unwrap_or_else(|| json!(inner.client_id)),
        "exp": now + tamper.expires_in.unwrap_or(300),
        "iat": now + tamper.issued_in.unwrap_or(0),
        // An address no connection is made on: an account is never found by it.
        "email": "someone@example.com",
        "email_verified": true,
    });
    match &tamper.nonce {
        None => claims["nonce"] = json!(grant.nonce),
        Some(Some(nonce)) => claims["nonce"] = json!(nonce),
        Some(None) => {}
    }
    if let Some(azp) = &tamper.azp {
        claims["azp"] = json!(azp);
    }
    let signer = tamper
        .signer
        .as_deref()
        .unwrap_or_else(|| inner.keys[0].0.kid.as_str());
    let key = &inner
        .keys
        .iter()
        .find(|(key, _)| key.kid == signer)
        .expect("a key to sign with")
        .0;
    let alg = tamper.alg.clone().unwrap_or_else(|| "ES256".to_owned());
    let header = json!({
        "alg": alg,
        "typ": "JWT",
        "kid": tamper.kid.clone().unwrap_or_else(|| key.kid.clone()),
    });
    let encode = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
    let input = format!("{}.{}", encode(&header), encode(&claims));
    let mut signature = match alg.as_str() {
        "none" => Vec::new(),
        "HS256" => {
            let secret = inner.client_secret.clone().unwrap_or_default();
            let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
            hmac::sign(&key, input.as_bytes()).as_ref().to_vec()
        }
        _ => key.sign(&input),
    };
    if tamper.corrupt_signature
        && let Some(first) = signature.first_mut()
    {
        *first ^= 1;
    }
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
}
