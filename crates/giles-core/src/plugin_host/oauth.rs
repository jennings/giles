//! OAuth 2.0 client credentials (RFC 6749 section 4.4) for remote plugins (PLAN section 7.6):
//! the access token the core sends as its bearer token, fetched before connecting and refreshed
//! before it expires.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use tokio_util::task::AbortOnDropHandle;
use tonic::metadata::{Ascii, MetadataValue};
use tracing::{Instrument as _, debug, warn};
use url::form_urlencoded;

use super::remote::Secrets;
use crate::config::{OAuthClientCredentials, Secret};
use crate::net;

/// How long one token request may take.
pub const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

/// A token is refreshed this long before it expires, or halfway through its life if that's
/// shorter.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// The shortest wait before a refresh, so a token that expires at once doesn't spin.
const MIN_REFRESH: Duration = Duration::from_secs(1);

/// The first delay before retrying a failed refresh; it doubles up to [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(1);

/// The longest delay between refresh attempts.
const MAX_RETRY: Duration = Duration::from_secs(60);

/// The largest token response read.
const MAX_RESPONSE: usize = 64 << 10;

/// An access token from the token endpoint.
#[derive(Debug)]
pub(crate) struct AccessToken {
    /// The token.
    pub(crate) value: Secret,
    /// How long it's valid, if the endpoint said.
    pub(crate) expires_in: Option<Duration>,
}

impl AccessToken {
    /// `Bearer <token>` as a (sensitive) header value.
    pub(crate) fn header(&self) -> MetadataValue<Ascii> {
        let mut value: MetadataValue<Ascii> = format!("Bearer {}", self.value.expose())
            .parse()
            .expect("parse_token checked the characters");
        value.set_sensitive(true);
        value
    }
}

/// Requests tokens from one token endpoint.
#[derive(Debug)]
pub(crate) struct TokenClient {
    http: reqwest::Client,
    credentials: OAuthClientCredentials,
}

impl TokenClient {
    /// A client for `credentials`, with the core's TLS settings ([`net::tls_client_builder`]).
    pub(crate) fn new(credentials: OAuthClientCredentials) -> Result<Self, String> {
        let http = net::tls_client_builder()
            .and_then(|builder| {
                builder
                    .timeout(TOKEN_TIMEOUT)
                    .build()
                    .map_err(giles_types::Error::other)
            })
            .map_err(|e| format!("cannot set up the HTTP client: {e}"))?;
        Ok(Self { http, credentials })
    }

    /// Requests a token: `POST` to the token endpoint with `grant_type=client_credentials` and
    /// the scopes, authenticated with HTTP Basic (the client id and secret form-encoded first,
    /// RFC 6749 section 2.3.1). The error is safe to log once scrubbed of the client secret.
    pub(crate) async fn fetch(&self) -> Result<AccessToken, String> {
        let credentials = &self.credentials;
        let body = request_body(&credentials.scopes);
        let encode = |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        let response = self
            .http
            .post(credentials.token_url.clone())
            .basic_auth(
                encode(&credentials.client_id),
                Some(encode(credentials.client_secret.expose())),
            )
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("the token endpoint can't be reached: {e}"))?;
        let status = response.status();
        let body = read_limited(response)
            .await
            .map_err(|e| format!("reading the token endpoint's answer: {e}"))?;
        if !status.is_success() {
            return Err(error_message(status, &body));
        }
        parse_token(&body)
    }
}

/// The form body of a token request.
fn request_body(scopes: &[String]) -> String {
    let mut form = form_urlencoded::Serializer::new(String::new());
    form.append_pair("grant_type", "client_credentials");
    if !scopes.is_empty() {
        form.append_pair("scope", &scopes.join(" "));
    }
    form.finish()
}

/// Reads at most [`MAX_RESPONSE`] bytes of the body.
async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > MAX_RESPONSE {
            return Err(format!("it is longer than {MAX_RESPONSE} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A failed token request's message: the status and, from an RFC 6749 error response, `error`
/// and `error_description`.
fn error_message(status: reqwest::StatusCode, body: &[u8]) -> String {
    let mut message = format!("the token endpoint answered {status}");
    if let Ok(serde_json::Value::Object(error)) = serde_json::from_slice(body) {
        if let Some(code) = error.get("error").and_then(|v| v.as_str()) {
            message.push_str(": ");
            message.push_str(&printable(code));
        }
        if let Some(description) = error.get("error_description").and_then(|v| v.as_str()) {
            message.push_str(" (");
            message.push_str(&printable(description));
            message.push(')');
        }
    }
    message
}

/// `text` without control characters, at most 200 characters, for a log line.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// Parses a successful token response (RFC 6749 section 5.1): `access_token` (visible ASCII, so
/// it fits a header), `token_type` `Bearer` if present (any case), and `expires_in` in seconds if
/// present (a number, or a string of digits as some servers send).
fn parse_token(body: &[u8]) -> Result<AccessToken, String> {
    let invalid = |why: &str| format!("the token endpoint's answer is invalid: {why}");
    let serde_json::Value::Object(token) =
        serde_json::from_slice(body).map_err(|_| invalid("not a JSON object"))?
    else {
        return Err(invalid("not a JSON object"));
    };
    let value = token
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| invalid("no access_token"))?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(invalid("access_token is empty or not visible ASCII"));
    }
    match token.get("token_type") {
        None => {}
        Some(serde_json::Value::String(kind)) if kind.eq_ignore_ascii_case("bearer") => {}
        Some(other) => {
            return Err(invalid(&format!(
                "token_type {} is not Bearer",
                printable(&other.to_string())
            )));
        }
    }
    let expires_in = match token.get("expires_in") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        Some(_) => None,
    };
    if token.get("expires_in").is_some_and(|v| !v.is_null()) && expires_in.is_none() {
        return Err(invalid("expires_in is not a number of seconds"));
    }
    Ok(AccessToken {
        value: Secret::new(value.to_owned()),
        expires_in: expires_in.map(Duration::from_secs),
    })
}

/// How long after receiving a token valid for `expires_in` to refresh it: [`REFRESH_MARGIN`]
/// before it expires, or halfway through its life if that's sooner, but not before
/// [`MIN_REFRESH`].
pub(crate) fn refresh_delay(expires_in: Duration) -> Duration {
    expires_in
        .saturating_sub((expires_in / 2).min(REFRESH_MARGIN))
        .max(MIN_REFRESH)
}

/// The current `Bearer <token>` header, shared with the call interceptor.
pub(crate) type SharedHeader = Arc<RwLock<MetadataValue<Ascii>>>;

/// Keeps `header` fresh: refreshes the token [`refresh_delay`] after each one it got (`expires_in`
/// is the first's, `first`), retrying failures from [`FIRST_RETRY`] doubling to [`MAX_RETRY`]
/// (calls go on with the old token meanwhile). Each new token joins `secrets`, which keep the
/// current and the previous token (the one before that has been replaced for a whole refresh
/// period). Stops when the returned handle is dropped. Errors are logged scrubbed of `secrets`.
pub(crate) fn keep_fresh(
    client: TokenClient,
    header: SharedHeader,
    expires_in: Option<Duration>,
    first: Secret,
    secrets: Secrets,
) -> AbortOnDropHandle<()> {
    let task = async move {
        let mut expires_in = expires_in;
        let (mut previous, mut current): (Option<Secret>, Secret) = (None, first);
        // A token without an expiry is kept until the connection ends.
        while let Some(lifetime) = expires_in {
            tokio::time::sleep(refresh_delay(lifetime)).await;
            let mut retry = FIRST_RETRY;
            let token = loop {
                match client.fetch().await {
                    Ok(token) => break token,
                    Err(error) => {
                        warn!(
                            error = %secrets.scrub(&error),
                            retry_in = ?retry,
                            "cannot refresh the OAuth access token",
                        );
                        tokio::time::sleep(retry).await;
                        retry = (retry * 2).min(MAX_RETRY);
                    }
                }
            };
            secrets.replace(previous.as_ref(), token.value.clone());
            *header.write().unwrap_or_else(PoisonError::into_inner) = token.header();
            previous = Some(std::mem::replace(&mut current, token.value));
            expires_in = token.expires_in;
            debug!(?expires_in, "refreshed the OAuth access token");
        }
    };
    AbortOnDropHandle::new(tokio::spawn(task.in_current_span()))
}

#[cfg(test)]
mod tests;
