//! Sign in with OpenID Connect, the protocol side: discovering a provider, starting an
//! authorization code flow, and turning a callback into the account that signed in. The pages that
//! use it are in [`oidc_pages`](super::oidc_pages). Design: `Docs/technical/web-channel.md`.
//!
//! The protocol is the [`openidconnect`] crate's: it parses the discovery document and checks its
//! issuer, builds the authorization request with PKCE, exchanges the code, and verifies the ID
//! token. This module sets it up so the checks can't be talked out of:
//!
//! - Every request goes through [`Http`], a client made by [`net`](crate::net) (the bundled roots,
//!   no redirects), with a timeout, a limit on the body, and a scheme policy: an `https` issuer's
//!   every endpoint must be `https`, and only an `http` issuer on a loopback host, which is for
//!   trying Doozers out, may use `http`.
//! - The verifier is the public client's, which refuses every HMAC algorithm (a client secret is
//!   no signing key, and a provider with no secret would give an attacker the key), and is told
//!   to accept only [`SIGNING_ALGORITHMS`]. The library refuses `alg: none` whatever it is told.
//!   It checks the signature against the provider's keys, `iss`, `aud` (and that no other
//!   audience is trusted), `exp`, and `nonce`; this module adds `azp`.
//! - The keys are the provider's JWKS, fetched with the discovery document and kept with it. A
//!   token signed with a `kid` that isn't there refreshes the keys, at most once a
//!   [`MIN_KEY_REFRESH`], and is checked once more; a provider that rotates keys needs no
//!   restart, and a stream of forged `kid`s can't make Doozers hammer the provider.
//! - A [`Flow`] is what the server remembers between the redirect and the callback: the nonce and
//!   the PKCE verifier. It is kept in a [`Pending`] table under the `state`, bound to the browser,
//!   and used once.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::http::Response;
use chrono::{DateTime, Utc};
use doozers_store::OidcProvider;
use doozers_types::{Timestamp, UserId};
use futures::StreamExt as _;
use openidconnect::core::{
    CoreAuthDisplay, CoreAuthenticationFlow, CoreClaimName, CoreClaimType, CoreClient,
    CoreClientAuthMethod, CoreGrantType, CoreIdTokenVerifier, CoreJsonWebKey, CoreJsonWebKeySet,
    CoreJweContentEncryptionAlgorithm, CoreJweKeyManagementAlgorithm, CoreJwsSigningAlgorithm,
    CoreResponseMode, CoreResponseType, CoreSubjectIdentifierType,
};
use openidconnect::{
    AdditionalProviderMetadata, AsyncHttpClient, AuthType, AuthUrl, AuthorizationCode,
    ClaimsVerificationError, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    HttpRequest, HttpResponse, IssuerUrl, JsonWebKeySetUrl, Nonce, PkceCodeChallenge,
    PkceCodeVerifier, ProviderMetadata, RedirectUrl, ResponseTypes, Scope,
    SignatureVerificationError, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use url::Url;

use super::limits::LoginLimits;
use super::login::is_loopback_host;
use super::pending::Pending;
use crate::config::Secret;

/// The signing algorithms an ID token may use: RSA (PKCS#1 v1.5 and PSS), ECDSA on P-256 and
/// P-384, and EdDSA. No HMAC, and not `none`.
pub(super) const SIGNING_ALGORITHMS: [CoreJwsSigningAlgorithm; 9] = [
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha384,
    CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha512,
    CoreJwsSigningAlgorithm::RsaSsaPssSha256,
    CoreJwsSigningAlgorithm::RsaSsaPssSha384,
    CoreJwsSigningAlgorithm::RsaSsaPssSha512,
    CoreJwsSigningAlgorithm::EcdsaP256Sha256,
    CoreJwsSigningAlgorithm::EcdsaP384Sha384,
    CoreJwsSigningAlgorithm::EdDsa,
];

/// How long a sign-in may take between the redirect and the callback.
pub(super) const FLOW_TTL: Duration = super::auth::OIDC_TTL;

/// The most sign-ins in flight at once.
pub(super) const MAX_FLOWS: usize = 10_000;

/// How long a discovery document and its keys are used before they are fetched again.
pub(super) const DISCOVERY_TTL: Duration = Duration::from_secs(60 * 60);

/// The least time between two refreshes of a provider's keys that an unknown `kid` makes.
pub(super) const MIN_KEY_REFRESH: Duration = Duration::from_secs(60);

/// How far the clocks of Doozers and a provider may differ: a token that expired this long ago is
/// still accepted, and one issued this far ahead isn't refused.
pub(super) const CLOCK_SKEW: Duration = Duration::from_secs(60);

/// How many sign-ins one client address may start in a [`WINDOW`](super::limits::WINDOW).
pub(super) const START_ATTEMPTS: u32 = 60;

/// How long one request to a provider may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The largest response body read, in bytes.
const MAX_BODY: usize = 1 << 20;

/// What a provider's discovery document says that the library doesn't read.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct PkceSupport {
    #[serde(default)]
    code_challenge_methods_supported: Option<Vec<String>>,
}

impl AdditionalProviderMetadata for PkceSupport {}

type Metadata = ProviderMetadata<
    PkceSupport,
    CoreAuthDisplay,
    CoreClientAuthMethod,
    CoreClaimName,
    CoreClaimType,
    CoreGrantType,
    CoreJweContentEncryptionAlgorithm,
    CoreJweKeyManagementAlgorithm,
    CoreJsonWebKey,
    CoreResponseMode,
    CoreResponseType,
    CoreSubjectIdentifierType,
>;

/// The client the flow's requests are made with: the authorization and token endpoints are set.
type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointSet,
    EndpointNotSet,
>;

/// Why a provider or a sign-in can't be used.
#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    /// The provider can't be used as configured or as it answers. The text says why, for an admin
    /// or the log, never for the person signing in.
    #[error("{0}")]
    Provider(String),
    /// A sign-in must be refused: what the provider returned doesn't check out. For the log.
    #[error("{0}")]
    Refused(String),
}

/// The HTTP error the library gets, with the reason.
#[derive(Debug)]
struct HttpError(String);

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HttpError {}

/// The HTTP client the library calls ([module docs](self)).
struct Http {
    client: reqwest::Client,
    /// Whether only `https` requests are allowed: the issuer is `https`.
    https_only: bool,
}

impl<'c> AsyncHttpClient<'c> for Http {
    type Error = HttpError;
    type Future = Pin<Box<dyn Future<Output = Result<HttpResponse, HttpError>> + Send + 'c>>;

    fn call(&'c self, request: HttpRequest) -> Self::Future {
        Box::pin(async move {
            let url = request.uri().to_string();
            match request.uri().scheme_str() {
                Some("https") => {}
                Some("http") if !self.https_only => {}
                _ => {
                    return Err(HttpError(format!(
                        "a request to {url} is refused: the provider's endpoints must be https"
                    )));
                }
            }
            let request = reqwest::Request::try_from(request)
                .map_err(|err| HttpError(format!("cannot make the request to {url}: {err}")))?;
            let response = self
                .client
                .execute(request)
                .await
                .map_err(|err| HttpError(format!("cannot reach {url}: {err}")))?;
            let mut builder = Response::builder().status(response.status());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            let mut chunks = response.bytes_stream();
            while let Some(chunk) = chunks.next().await {
                let chunk =
                    chunk.map_err(|err| HttpError(format!("cannot read from {url}: {err}")))?;
                if body.len() + chunk.len() > MAX_BODY {
                    return Err(HttpError(format!(
                        "the response of {url} is larger than {MAX_BODY} bytes"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            builder
                .body(body)
                .map_err(|err| HttpError(format!("cannot read the response of {url}: {err}")))
        })
    }
}

/// How many seconds before a re-verification began the provider's `auth_time` may be, for a clock
/// that runs a little differently from ours.
const AUTH_TIME_LEEWAY_SECS: i64 = 60;

/// What the browser's sign-in is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Purpose {
    /// Sign in as whoever has the account. Nobody is signed in yet.
    SignIn,
    /// Connect the account to `user`, who is signed in with an invite or recovery link, and whose
    /// login has the CSRF token `csrf`.
    Connect {
        /// The user who started it.
        user: UserId,
        /// The CSRF token of their login.
        csrf: String,
    },
    /// Prove that the account is connected to `user`, who is logged in, to confirm it's them (see
    /// [`reverify`](super::reverify)), and go on to `next`.
    Reverify {
        /// The user who started it.
        user: UserId,
        /// The CSRF token of their login.
        csrf: String,
        /// The page to go on to, one of the pages `reverify::back_to` knows.
        next: &'static str,
    },
}

impl Purpose {
    /// The user and the CSRF token of the login that started a sign-in that needs one: the only
    /// login that may finish it.
    pub(super) fn login(&self) -> Option<(&UserId, &str)> {
        match self {
            Self::SignIn => None,
            Self::Connect { user, csrf } | Self::Reverify { user, csrf, .. } => Some((user, csrf)),
        }
    }
}

/// A sign-in in flight: what the callback must find to be the one that started.
pub(super) struct Flow {
    /// The provider it went to.
    pub(super) provider: String,
    /// The issuer that provider had then.
    issuer: String,
    nonce: Nonce,
    /// When it began: a re-verification needs the user to have authenticated since.
    started_at: Timestamp,
    verifier: PkceCodeVerifier,
    /// What it is for.
    pub(super) purpose: Purpose,
}

/// A provider's discovery document, as far as sign-in uses it, and its keys.
pub(super) struct Discovered {
    issuer: IssuerUrl,
    fetched_at: Timestamp,
    authorization_endpoint: AuthUrl,
    token_endpoint: TokenUrl,
    jwks_uri: JsonWebKeySetUrl,
    auth_type: AuthType,
    https_only: bool,
    /// The provider's keys, and when they were fetched. Refreshing holds the lock, so two
    /// tokens with an unknown `kid` cause one request.
    keys: tokio::sync::Mutex<Keys>,
}

struct Keys {
    set: CoreJsonWebKeySet,
    fetched_at: Timestamp,
}

/// What the provider sent the browser back with.
pub(super) struct Returned<'a> {
    /// The authorization code.
    pub(super) code: &'a str,
    /// The issuer that answered, if the provider says which it is (RFC 9207).
    pub(super) issuer: Option<&'a str>,
}

/// The account that signed in, as the provider says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Identity {
    /// The issuer that vouched for it: the provider's, which the ID token's `iss` equals.
    pub(super) issuer: String,
    /// The provider's stable id for the account.
    pub(super) subject: String,
}

/// The providers' discovery documents and keys, and the sign-ins in flight.
pub(super) struct Oidc {
    client: Result<reqwest::Client, String>,
    discovered: Mutex<HashMap<String, Arc<Discovered>>>,
    flows: Pending<Flow>,
    /// Counts the sign-ins a client address starts.
    pub(super) starts: LoginLimits,
}

impl fmt::Debug for Oidc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Oidc").finish_non_exhaustive()
    }
}

impl Default for Oidc {
    fn default() -> Self {
        Self::new(FLOW_TTL, MAX_FLOWS)
    }
}

/// `error` and what caused it, as one sentence.
fn chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn utc(at: Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(at.as_millisecond()).unwrap_or(DateTime::UNIX_EPOCH)
}

impl Oidc {
    /// A table of sign-ins that live for `ttl`, at most `max` of them.
    pub(super) fn new(ttl: Duration, max: usize) -> Self {
        let client = crate::net::tls_client_builder()
            .and_then(|builder| {
                builder
                    .timeout(REQUEST_TIMEOUT)
                    .build()
                    .map_err(doozers_types::Error::other)
            })
            .map_err(|err| err.to_string());
        Self {
            client,
            discovered: Mutex::new(HashMap::new()),
            flows: Pending::new(ttl, max),
            starts: LoginLimits::new(u32::MAX, START_ATTEMPTS, super::limits::WINDOW),
        }
    }

    fn http(&self, https_only: bool) -> Result<Http, Error> {
        match &self.client {
            Ok(client) => Ok(Http {
                client: client.clone(),
                https_only,
            }),
            Err(err) => Err(Error::Provider(format!(
                "cannot make an HTTP client: {err}"
            ))),
        }
    }

    /// The discovery document of `issuer` and its keys, as fetched less than [`DISCOVERY_TTL`]
    /// ago, or fetched now ([`fetch`](Self::fetch)).
    ///
    /// # Errors
    ///
    /// [`Error::Provider`], saying why the provider can't be used.
    pub(super) async fn discover(
        &self,
        issuer: &str,
        now: Timestamp,
    ) -> Result<Arc<Discovered>, Error> {
        if let Some(found) = self
            .discovered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(issuer)
            .filter(|found| {
                now.as_millisecond() - found.fetched_at.as_millisecond()
                    < i64::try_from(DISCOVERY_TTL.as_millis()).unwrap_or(i64::MAX)
            })
        {
            return Ok(found.clone());
        }
        self.fetch(issuer, now).await
    }

    /// Fetches the discovery document of `issuer` and its keys, and keeps them.
    ///
    /// # Errors
    ///
    /// [`Error::Provider`], saying why the provider can't be used: the issuer isn't a usable URL,
    /// the provider can't be reached, or its document is wrong (another issuer, no code flow, no
    /// PKCE with S256, no signing algorithm Doozers accepts, an endpoint that isn't `https`).
    pub(super) async fn fetch(
        &self,
        issuer: &str,
        now: Timestamp,
    ) -> Result<Arc<Discovered>, Error> {
        let url = Url::parse(issuer)
            .map_err(|err| Error::Provider(format!("the issuer isn't a URL: {err}")))?;
        let https_only = match (url.scheme(), url.host_str()) {
            ("https", Some(_)) => true,
            ("http", Some(host)) if is_loopback_host(host) => false,
            ("http", Some(_)) => {
                return Err(Error::Provider(
                    "the issuer must be an https URL (http works only on localhost)".to_owned(),
                ));
            }
            _ => {
                return Err(Error::Provider(
                    "the issuer must be an https URL".to_owned(),
                ));
            }
        };
        if url.query().is_some() || url.fragment().is_some() {
            return Err(Error::Provider(
                "the issuer must have no query or fragment".to_owned(),
            ));
        }
        let issuer_url = IssuerUrl::new(issuer.to_owned())
            .map_err(|err| Error::Provider(format!("the issuer isn't a URL: {err}")))?;
        let http = self.http(https_only)?;
        let metadata = Metadata::discover_async(issuer_url.clone(), &http)
            .await
            .map_err(|err| {
                Error::Provider(format!(
                    "cannot read the provider's discovery document: {}",
                    chain(&err)
                ))
            })?;
        let found = Arc::new(Self::check(metadata, issuer_url, https_only, now)?);
        let mut discovered = self
            .discovered
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let ttl = i64::try_from(DISCOVERY_TTL.as_millis()).unwrap_or(i64::MAX);
        discovered.retain(|_, old| now.as_millisecond() - old.fetched_at.as_millisecond() < ttl);
        discovered.insert(issuer.to_owned(), found.clone());
        Ok(found)
    }

    /// Checks that a provider's document can be used for a sign-in, and keeps what a sign-in
    /// needs from it.
    fn check(
        metadata: Metadata,
        issuer: IssuerUrl,
        https_only: bool,
        now: Timestamp,
    ) -> Result<Discovered, Error> {
        let provider = |what: &str| Error::Provider(format!("the provider {what}"));
        // The library compared the document's issuer with the one asked for; say it again here so
        // that nothing below depends on it.
        if metadata.issuer() != &issuer {
            return Err(provider("names another issuer in its discovery document"));
        }
        let token_endpoint = metadata
            .token_endpoint()
            .cloned()
            .ok_or_else(|| provider("has no token endpoint"))?;
        if !metadata
            .response_types_supported()
            .iter()
            .any(|types| *types == ResponseTypes::new(vec![CoreResponseType::Code]))
        {
            return Err(provider("doesn't support the authorization code flow"));
        }
        if let Some(methods) = &metadata
            .additional_metadata()
            .code_challenge_methods_supported
            && !methods.iter().any(|method| method == "S256")
        {
            return Err(provider("doesn't support PKCE with S256"));
        }
        if !metadata
            .id_token_signing_alg_values_supported()
            .iter()
            .any(|alg| SIGNING_ALGORITHMS.contains(alg))
        {
            return Err(provider(
                "signs ID tokens with no algorithm Doozers accepts (RS256, PS256, ES256, ES384, \
                 EdDSA, and their kin)",
            ));
        }
        let allowed =
            |url: &Url| url.scheme() == "https" || (!https_only && url.scheme() == "http");
        let authorization_endpoint = metadata.authorization_endpoint().clone();
        for (what, url) in [
            ("authorization endpoint", authorization_endpoint.url()),
            ("token endpoint", token_endpoint.url()),
            ("key set", metadata.jwks_uri().url()),
        ] {
            if !allowed(url) {
                return Err(provider(&format!("has a {what} that isn't https")));
            }
        }
        // Basic is the default of the specification; a provider that lists only the form body
        // gets that.
        let auth_type = match metadata.token_endpoint_auth_methods_supported() {
            Some(methods)
                if !methods.contains(&CoreClientAuthMethod::ClientSecretBasic)
                    && methods.contains(&CoreClientAuthMethod::ClientSecretPost) =>
            {
                AuthType::RequestBody
            }
            _ => AuthType::BasicAuth,
        };
        Ok(Discovered {
            issuer,
            fetched_at: now,
            authorization_endpoint,
            token_endpoint,
            jwks_uri: metadata.jwks_uri().clone(),
            auth_type,
            https_only,
            keys: tokio::sync::Mutex::new(Keys {
                set: metadata.jwks().clone(),
                fetched_at: now,
            }),
        })
    }

    /// The library's client for `provider` at `found`, with `secret` if it has one.
    fn client(
        found: &Discovered,
        provider: &OidcProvider,
        secret: Option<&Secret>,
        redirect_uri: &str,
    ) -> Result<Client, Error> {
        let redirect = RedirectUrl::new(redirect_uri.to_owned())
            .map_err(|err| Error::Provider(format!("the redirect URI isn't a URL: {err}")))?;
        let mut client = CoreClient::new(
            ClientId::new(provider.client_id.clone()),
            found.issuer.clone(),
            CoreJsonWebKeySet::new(Vec::new()),
        )
        .set_auth_uri(found.authorization_endpoint.clone())
        .set_token_uri(found.token_endpoint.clone())
        .set_redirect_uri(redirect)
        .set_auth_type(found.auth_type.clone());
        if let Some(secret) = secret {
            client = client.set_client_secret(ClientSecret::new(secret.expose().to_owned()));
        }
        Ok(client)
    }

    /// Starts a sign-in at `provider` for `purpose`, which the provider sends back to
    /// `redirect_uri`: remembers it, bound to `binding`, and returns the authorization URL to send
    /// the browser to.
    ///
    /// # Errors
    ///
    /// [`Error::Provider`], if the provider can't be discovered or used.
    pub(super) async fn begin(
        &self,
        provider: &OidcProvider,
        redirect_uri: &str,
        binding: &str,
        purpose: Purpose,
        now: Timestamp,
    ) -> Result<Url, Error> {
        let found = self.discover(&provider.issuer, now).await?;
        let client = Self::client(&found, provider, None, redirect_uri)?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let nonce = Nonce::new_random();
        let state = self.flows.begin(
            binding,
            Flow {
                provider: provider.id.clone(),
                issuer: provider.issuer.clone(),
                nonce: nonce.clone(),
                started_at: now,
                verifier,
                purpose: purpose.clone(),
            },
            now,
        );
        let mut request = client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                move || CsrfToken::new(state),
                move || nonce,
            )
            .set_pkce_challenge(challenge);
        // Confirming it's the user must not be answered by a provider's session that is already
        // there: it has to ask the user to authenticate again.
        if matches!(purpose, Purpose::Reverify { .. }) {
            request = request.set_max_age(std::time::Duration::ZERO);
        }
        // `openid` is always asked for.
        for scope in provider.scopes.iter().filter(|scope| *scope != "openid") {
            request = request.add_scope(Scope::new(scope.clone()));
        }
        Ok(request.url().0)
    }

    /// Takes the sign-in `state` out: it must exist, be unexpired, and have been started by the
    /// browser `binding` names. Whether or not it matches, it is gone afterwards.
    pub(super) fn take_flow(&self, state: &str, binding: &str, now: Timestamp) -> Option<Flow> {
        self.flows.finish(state, binding, now)
    }

    /// Finishes `flow`, which came back to `provider` with what `returned` has: exchanges the code, and checks the ID token it gets. `secret` is the
    /// provider's client secret, if it has one.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] for a sign-in that doesn't check out, [`Error::Provider`] if the
    /// provider can't be discovered or used.
    pub(super) async fn complete(
        &self,
        provider: &OidcProvider,
        secret: Option<&Secret>,
        redirect_uri: &str,
        flow: Flow,
        returned: Returned<'_>,
        now: Timestamp,
    ) -> Result<Identity, Error> {
        let refuse = |why: String| Error::Refused(why);
        if flow.provider != provider.id || flow.issuer != provider.issuer {
            return Err(refuse(
                "the provider was changed after the sign-in began".to_owned(),
            ));
        }
        // A provider that says which issuer answered (RFC 9207) must say this one.
        if let Some(issuer) = returned.issuer
            && issuer != provider.issuer
        {
            return Err(refuse(format!(
                "the callback names another issuer, {issuer}"
            )));
        }
        let found = self.discover(&provider.issuer, now).await?;
        let client = Self::client(&found, provider, secret, redirect_uri)?;
        let http = self.http(found.https_only)?;
        let tokens = client
            .exchange_code(AuthorizationCode::new(returned.code.to_owned()))
            .set_pkce_verifier(flow.verifier)
            .request_async(&http)
            .await
            .map_err(|err| refuse(format!("the token request failed: {}", chain(&err))))?;
        let id_token = tokens
            .id_token()
            .ok_or_else(|| refuse("the token response has no ID token".to_owned()))?;
        let mut refreshed = false;
        loop {
            let (set, fetched_at) = {
                let keys = found.keys.lock().await;
                (keys.set.clone(), keys.fetched_at)
            };
            let verifier = Self::verifier(provider, &found.issuer, set, now);
            match id_token.claims(&verifier, &flow.nonce) {
                Ok(claims) => {
                    let audiences = claims.audiences();
                    let authorized = claims.authorized_party().map(|party| party.as_str());
                    // `azp` names the client the token was issued to: it is required when there
                    // are several audiences, and is ours whenever it is there.
                    if authorized.is_some_and(|party| party != provider.client_id)
                        || (audiences.len() > 1 && authorized.is_none())
                    {
                        return Err(refuse(
                            "the ID token's authorized party isn't this client".to_owned(),
                        ));
                    }
                    // A re-verification needs proof that the user authenticated for it: the ID
                    // token says when (`auth_time`, which `max_age` makes the provider send), and
                    // it must be since the flow began, within a little clock difference.
                    if matches!(flow.purpose, Purpose::Reverify { .. })
                        && !claims.auth_time().is_some_and(|at| {
                            at.timestamp() >= flow.started_at.as_second() - AUTH_TIME_LEEWAY_SECS
                        })
                    {
                        return Err(refuse(
                            "the provider didn't authenticate the user again".to_owned(),
                        ));
                    }
                    let subject = claims.subject().as_str();
                    if subject.is_empty() {
                        return Err(refuse("the ID token has no subject".to_owned()));
                    }
                    return Ok(Identity {
                        issuer: claims.issuer().as_str().to_owned(),
                        subject: subject.to_owned(),
                    });
                }
                Err(ClaimsVerificationError::SignatureVerification(
                    SignatureVerificationError::NoMatchingKey,
                )) if !refreshed => {
                    refreshed = true;
                    if !self.refresh_keys(&found, fetched_at, now).await? {
                        return Err(refuse(
                            "the ID token is signed with a key the provider doesn't list"
                                .to_owned(),
                        ));
                    }
                }
                Err(err) => {
                    return Err(refuse(format!("the ID token is refused: {}", chain(&err))));
                }
            }
        }
    }

    /// The verifier of ID tokens `provider` issues, which checks them against `keys` as of `now`.
    fn verifier<'a>(
        provider: &OidcProvider,
        issuer: &IssuerUrl,
        keys: CoreJsonWebKeySet,
        now: Timestamp,
    ) -> CoreIdTokenVerifier<'a> {
        let skew = chrono::Duration::from_std(CLOCK_SKEW).unwrap_or_default();
        let lenient = utc(now) - skew;
        let latest = utc(now) + skew;
        CoreIdTokenVerifier::new_public_client(
            ClientId::new(provider.client_id.clone()),
            issuer.clone(),
            keys,
        )
        .set_allowed_algs(SIGNING_ALGORITHMS)
        .set_time_fn(move || lenient)
        .set_issue_time_verifier_fn(move |issued| {
            if issued > latest {
                Err("the ID token was issued in the future".to_owned())
            } else {
                Ok(())
            }
        })
    }

    /// Fetches the keys of `found` again, unless that was done less than [`MIN_KEY_REFRESH`] ago.
    /// `seen` is when the keys were fetched that the caller found no match in: if they were
    /// fetched again since (by another token with the same new `kid`), there is nothing to fetch,
    /// and the caller should check again. Whether the caller should check again.
    async fn refresh_keys(
        &self,
        found: &Discovered,
        seen: Timestamp,
        now: Timestamp,
    ) -> Result<bool, Error> {
        let mut keys = found.keys.lock().await;
        if keys.fetched_at != seen {
            return Ok(true);
        }
        let age = now.as_millisecond() - keys.fetched_at.as_millisecond();
        if age < i64::try_from(MIN_KEY_REFRESH.as_millis()).unwrap_or(i64::MAX) {
            return Ok(false);
        }
        let http = self.http(found.https_only)?;
        keys.set = CoreJsonWebKeySet::fetch_async(&found.jwks_uri, &http)
            .await
            .map_err(|err| {
                Error::Provider(format!("cannot fetch the provider's keys: {}", chain(&err)))
            })?;
        keys.fetched_at = now;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::{StatusCode, header::LOCATION};
    use axum::routing::get;

    use super::*;

    fn now() -> Timestamp {
        Timestamp::from_second(1_800_000_000).unwrap()
    }

    fn http(https_only: bool) -> Http {
        Http {
            client: crate::net::tls_client_builder().unwrap().build().unwrap(),
            https_only,
        }
    }

    fn get_request(url: &str) -> HttpRequest {
        axum::http::Request::builder()
            .uri(url)
            .body(Vec::new())
            .unwrap()
    }

    /// A server on a loopback port, and its address.
    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn an_https_issuers_requests_must_be_https() {
        let refused = http(true)
            .call(get_request("http://127.0.0.1:9/token"))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("must be https"), "{refused}");
        // Another scheme is refused whatever the issuer is, and the refusal is before any request.
        for https_only in [true, false] {
            let refused = http(https_only)
                .call(get_request("ftp://127.0.0.1:9/token"))
                .await
                .unwrap_err();
            assert!(refused.to_string().contains("must be https"), "{refused}");
        }
        // An http issuer on a loopback host may use http (this one has nothing to answer).
        let reached = http(false)
            .call(get_request("http://127.0.0.1:9/token"))
            .await
            .unwrap_err();
        assert!(reached.to_string().contains("cannot reach"), "{reached}");
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        static TARGET: AtomicUsize = AtomicUsize::new(0);
        let base = serve(
            Router::new()
                .route(
                    "/start",
                    get(|| async { (StatusCode::FOUND, [(LOCATION, "/target")]) }),
                )
                .route(
                    "/target",
                    get(|| async {
                        TARGET.fetch_add(1, Ordering::SeqCst);
                        "reached"
                    }),
                ),
        )
        .await;
        let response = http(false)
            .call(get_request(&format!("{base}/start")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND);
        assert_eq!(
            TARGET.load(Ordering::SeqCst),
            0,
            "the redirect was followed"
        );
    }

    #[tokio::test]
    async fn a_response_over_a_mebibyte_is_refused() {
        let base = serve(
            Router::new()
                .route("/big", get(|| async { vec![b'x'; MAX_BODY + 1] }))
                .route("/fits", get(|| async { vec![b'x'; MAX_BODY] })),
        )
        .await;
        let refused = http(false)
            .call(get_request(&format!("{base}/big")))
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("larger than"), "{refused}");
        let fits = http(false)
            .call(get_request(&format!("{base}/fits")))
            .await
            .unwrap();
        assert_eq!(fits.body().len(), MAX_BODY);
    }

    fn metadata(change: impl FnOnce(&mut serde_json::Value)) -> Metadata {
        let mut document = serde_json::json!({
            "issuer": "https://idp.example.com",
            "authorization_endpoint": "https://idp.example.com/authorize",
            "token_endpoint": "https://idp.example.com/token",
            "jwks_uri": "https://idp.example.com/jwks",
            "response_types_supported": ["code"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": ["RS256"],
        });
        change(&mut document);
        serde_json::from_value(document).unwrap()
    }

    fn check(metadata: Metadata, issuer: &str, https_only: bool) -> Result<Discovered, Error> {
        Oidc::check(
            metadata,
            IssuerUrl::new(issuer.to_owned()).unwrap(),
            https_only,
            now(),
        )
    }

    #[test]
    fn an_https_issuers_document_may_name_only_https_endpoints() {
        check(metadata(|_| {}), "https://idp.example.com", true).unwrap();
        for (field, what) in [
            ("authorization_endpoint", "authorization endpoint"),
            ("token_endpoint", "token endpoint"),
            ("jwks_uri", "key set"),
        ] {
            let insecure = metadata(|doc| {
                doc[field] = "http://idp.example.com/x".into();
            });
            let err = check(insecure, "https://idp.example.com", true)
                .err()
                .unwrap_or_else(|| panic!("{field} accepted"));
            assert!(
                err.to_string()
                    .contains(&format!("has a {what} that isn't https")),
                "{field}: {err}"
            );
        }
        // An http issuer, which is only allowed on a loopback host, may use http endpoints.
        let local = metadata(|doc| {
            for (field, path) in [
                ("issuer", ""),
                ("authorization_endpoint", "/authorize"),
                ("token_endpoint", "/token"),
                ("jwks_uri", "/jwks"),
            ] {
                doc[field] = format!("http://127.0.0.1:8080{path}").into();
            }
        });
        check(local, "http://127.0.0.1:8080", false).unwrap();
    }
}
