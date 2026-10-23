//! Who is asking: the [`WebUser`] extractor, the CSRF check for forms, and the pieces of the web
//! login (PLAN section 3.4): the session cookie and the trusted-proxy marker.
//!
//! Authentication runs as a middleware (`login::authenticate`, which
//! [`WebApp::install`](super::WebApp::install) puts in front of every page) that puts a
//! [`WebUser`] into the request's extensions. Handlers (and the admin pages, 3.8) only extract it:
//! a request without one gets the login page.

use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use giles_types::{NativeId, UserId};
use sha2::{Digest, Sha256};

use super::pages::{error_page, login_required};

/// The header htmx requests carry the CSRF token in (set on the page with `hx-headers`).
pub const CSRF_HEADER: &str = "x-csrf-token";

/// The form field plain form posts carry the CSRF token in.
pub const CSRF_FIELD: &str = "csrf";

/// The session cookie.
pub const SESSION_COOKIE: &str = "giles_session";

/// How long a web session lasts (PLAN section 3.4).
pub const WEB_SESSION_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How the user logged in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Login {
    /// With the session cookie holding this token (a `web_sessions` row).
    Cookie(String),
    /// Through the trusted proxy, which names the user on every request.
    Proxy,
    /// Injected by a test.
    #[cfg(test)]
    Test,
}

/// An authenticated web user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUser {
    /// The user (v1: always the owner).
    pub user: UserId,
    /// The native identifier in the `web` namespace: what messages from this user carry as
    /// `InboundMessage::sender`, matched against `[owner] identities` as `web:<sender>`. A login
    /// link's user id for a cookie login; the proxy's header value for a proxy login.
    pub sender: NativeId,
    /// This login's CSRF token: every form post must carry it ([`check_csrf`]).
    pub csrf: String,
    pub(super) login: Login,
}

impl WebUser {
    /// Whether logging out means anything: a cookie login, not a proxy's.
    pub fn can_log_out(&self) -> bool {
        matches!(self.login, Login::Cookie(_))
    }
}

/// Marks a request that came through the trusted-proxy listener: only then is the identity header
/// (and `X-Forwarded-Proto`) believed.
#[derive(Debug, Clone, Copy)]
pub(super) struct TrustedProxy;

/// The CSRF token of a cookie login: derived from the session token, which only the browser has
/// (the cookie is `HttpOnly`), so it needs no storage and survives restarts.
pub(super) fn cookie_csrf(token: &str) -> String {
    hex(&Sha256::new()
        .chain_update(b"giles web csrf\0")
        .chain_update(token.as_bytes())
        .finalize())
}

/// The CSRF token of a proxy login: keyed with the channel's random per-run key, since the
/// native id itself is no secret.
pub(super) fn proxy_csrf(key: &[u8; 32], sender: &NativeId) -> String {
    hex(&Sha256::new()
        .chain_update(key)
        .chain_update(Sha256::digest(sender.as_str().as_bytes()))
        .finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The value of cookie `name` in the request's `Cookie` headers, if present and non-empty.
pub(super) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, value)| *key == name && !value.is_empty())
        .map(|(_, value)| value)
}

/// `Set-Cookie` for a new session: `HttpOnly`, `SameSite=Lax`, the whole site, 30 days, and
/// `Secure` when served over HTTPS.
pub(super) fn session_cookie(token: &str, https: bool) -> HeaderValue {
    let max_age = WEB_SESSION_TTL.as_secs();
    let secure = if https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("a hex token is a valid header value")
}

/// `Set-Cookie` that deletes the session cookie.
pub(super) fn clear_session_cookie(https: bool) -> HeaderValue {
    let secure = if https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("a valid header value")
}

impl<S: Send + Sync> FromRequestParts<S> for WebUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<WebUser>()
            .cloned()
            .ok_or_else(login_required)
    }
}

/// Checks a state-changing request's CSRF token: the [`CSRF_HEADER`] (htmx) or the
/// [`CSRF_FIELD`] form field must equal the user's token. On top of `SameSite=Lax` cookies, this
/// stops another site from posting forms (or a page an agent's reply links to) as the user.
///
/// # Errors
///
/// [`CsrfRejected`]: answer with it (a 403 page).
pub fn check_csrf(
    user: &WebUser,
    headers: &HeaderMap,
    field: Option<&str>,
) -> Result<(), CsrfRejected> {
    let given = headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .or(field);
    // Compare digests, so the time taken says nothing about how much of the token matched.
    let matches = given.is_some_and(|given| {
        Sha256::digest(given.as_bytes()) == Sha256::digest(user.csrf.as_bytes())
    });
    if matches { Ok(()) } else { Err(CsrfRejected) }
}

/// A request without the right CSRF token ([`check_csrf`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsrfRejected;

impl IntoResponse for CsrfRejected {
    fn into_response(self) -> Response {
        error_page(
            StatusCode::FORBIDDEN,
            "Form expired",
            "This form is out of date. Reload the page and try again.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> WebUser {
        WebUser {
            user: UserId::new("owner").unwrap(),
            sender: NativeId::new("owner").unwrap(),
            csrf: "token".into(),
            login: Login::Test,
        }
    }

    #[test]
    fn csrf_comes_from_the_header_or_the_field_and_must_match() {
        let mut headers = HeaderMap::new();
        assert!(check_csrf(&user(), &headers, Some("token")).is_ok());
        assert!(check_csrf(&user(), &headers, None).is_err());
        assert!(check_csrf(&user(), &headers, Some("tokeN")).is_err());
        assert!(check_csrf(&user(), &headers, Some("")).is_err());
        headers.insert(CSRF_HEADER, "token".parse().unwrap());
        assert!(check_csrf(&user(), &headers, None).is_ok());
        // The header wins over the field.
        headers.insert(CSRF_HEADER, "wrong".parse().unwrap());
        let err = check_csrf(&user(), &headers, Some("token")).unwrap_err();
        assert_eq!(err.into_response().status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let mut headers = HeaderMap::new();
        assert_eq!(cookie(&headers, SESSION_COOKIE), None);
        headers.append("cookie", "a=1; giles_session=abc ; b=2".parse().unwrap());
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("abc"));
        let mut headers = HeaderMap::new();
        headers.append("cookie", "giles_session=".parse().unwrap());
        headers.append("cookie", "x=y; giles_session=def".parse().unwrap());
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("def"));
        headers.insert("cookie", "giles_sessionx=1".parse().unwrap());
        assert_eq!(cookie(&headers, SESSION_COOKIE), None);
    }

    #[test]
    fn session_cookies_are_http_only_lax_and_secure_over_https() {
        assert_eq!(
            session_cookie("t", false),
            "giles_session=t; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax"
        );
        assert_eq!(
            session_cookie("t", true),
            "giles_session=t; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax; Secure"
        );
        assert_eq!(
            clear_session_cookie(true),
            "giles_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
        );
    }

    #[test]
    fn csrf_tokens_are_per_login_and_unguessable_from_public_data() {
        let a = cookie_csrf("token-a");
        assert_eq!(a.len(), 64);
        assert_eq!(a, cookie_csrf("token-a"));
        assert_ne!(a, cookie_csrf("token-b"));
        let owner = NativeId::new("owner").unwrap();
        let p = proxy_csrf(&[1; 32], &owner);
        assert_eq!(p, proxy_csrf(&[1; 32], &owner));
        assert_ne!(p, proxy_csrf(&[2; 32], &owner), "keyed per run");
        assert_ne!(p, proxy_csrf(&[1; 32], &NativeId::new("other").unwrap()));
    }
}
