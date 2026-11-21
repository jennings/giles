//! Who is asking: the [`WebUser`] extractor, the CSRF check for forms, and the pieces of the web
//! login (PLAN section 3.4): the session cookie, the sign-in form nonce, and the trusted-proxy
//! marker.
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
use doozers_store::Role;
use doozers_types::{NativeId, UserId};
use sha2::{Digest, Sha256};

use super::pages::{error_page, login_required};
use super::random_hex;

/// The header htmx requests carry the CSRF token in (set on the page with `hx-headers`).
pub const CSRF_HEADER: &str = "x-csrf-token";

/// The form field plain form posts carry the CSRF token in.
pub const CSRF_FIELD: &str = "csrf";

/// The session cookie.
pub const SESSION_COOKIE: &str = "doozers_session";

/// The cookie that holds the nonce of a sign-in form.
pub const NONCE_COOKIE: &str = "doozers_login_nonce";

/// How long a sign-in form's nonce cookie lasts.
const NONCE_TTL: Duration = Duration::from_secs(15 * 60);

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
    /// The user.
    pub user: UserId,
    /// The user's role when the request arrived. A change of role applies to the next request.
    /// [`may_administer`](super::admin::may_administer) reads it.
    pub role: Role,
    /// The native identifier in the `web` namespace: what messages from this user carry as
    /// `InboundMessage::sender`, matched against the users' identities as `web:<sender>`. A login
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
        .chain_update(b"doozers web csrf\0")
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

/// A fresh sign-in form nonce: 128 random bits as 32 lowercase hex digits.
fn new_nonce() -> String {
    random_hex(16)
}

fn is_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The nonce a sign-in form should carry, which the response must also set as the
/// [`nonce_cookie`]: the browser's own, if it has a well-formed one (so a second tab's form still
/// works), else a new one.
pub(super) fn nonce_for_form(headers: &HeaderMap) -> String {
    cookie(headers, NONCE_COOKIE)
        .filter(|value| is_nonce(value))
        .map_or_else(new_nonce, str::to_owned)
}

/// `Set-Cookie` for a sign-in form's nonce: `HttpOnly`, `SameSite=Strict` (another site's form
/// post never carries it), the whole site, 15 minutes, and `Secure` when served over HTTPS.
pub(super) fn nonce_cookie(nonce: &str, https: bool) -> HeaderValue {
    let max_age = NONCE_TTL.as_secs();
    let secure = if https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{NONCE_COOKIE}={nonce}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}"
    ))
    .expect("a hex nonce is a valid header value")
}

/// `Set-Cookie` that deletes the nonce cookie, once a sign-in succeeded.
pub(super) fn clear_nonce_cookie(https: bool) -> HeaderValue {
    let secure = if https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{NONCE_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{secure}"
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

/// Checks a sign-in POST's nonce: the form's `nonce` field must equal the browser's
/// [`NONCE_COOKIE`]. A sign-in has no session to hold a CSRF token yet, so this is what stops
/// another site from posting its own credentials or link from the victim's browser (login CSRF):
/// the nonce cookie is `SameSite=Strict`, and only a page this site served knows its value. Every
/// POST that signs a browser in checks it before anything else.
///
/// # Errors
///
/// [`NonceRejected`]: answer with it (a 403 page).
pub fn check_nonce(headers: &HeaderMap, field: Option<&str>) -> Result<(), NonceRejected> {
    // Compare digests, so the time taken says nothing about how much of the nonce matched.
    let matches = cookie(headers, NONCE_COOKIE)
        .filter(|value| is_nonce(value))
        .zip(field)
        .is_some_and(|(cookie, field)| {
            Sha256::digest(cookie.as_bytes()) == Sha256::digest(field.as_bytes())
        });
    if matches { Ok(()) } else { Err(NonceRejected) }
}

/// A sign-in POST without the nonce of its browser's form ([`check_nonce`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonceRejected;

impl IntoResponse for NonceRejected {
    fn into_response(self) -> Response {
        error_page(
            StatusCode::FORBIDDEN,
            "Form expired",
            "This sign-in form didn't come from this browser, or it expired. Reload the page you \
             came from and try again.",
        )
    }
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
            role: Role::Admin,
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
    fn the_nonce_must_be_the_one_in_the_browsers_cookie() {
        let nonce = "0123456789abcdef0123456789abcdef";
        let mut headers = HeaderMap::new();
        assert!(check_nonce(&headers, Some(nonce)).is_err(), "no cookie");
        headers.append("cookie", format!("{NONCE_COOKIE}={nonce}").parse().unwrap());
        assert!(check_nonce(&headers, Some(nonce)).is_ok());
        assert!(check_nonce(&headers, None).is_err(), "no field");
        assert!(check_nonce(&headers, Some("")).is_err());
        assert!(check_nonce(&headers, Some(&nonce.replace('0', "1"))).is_err());
        // A cookie that isn't a nonce never matches, even when the field repeats it.
        let mut headers = HeaderMap::new();
        headers.append("cookie", format!("{NONCE_COOKIE}=abc").parse().unwrap());
        assert!(check_nonce(&headers, Some("abc")).is_err());
        let err = check_nonce(&headers, None).unwrap_err();
        assert_eq!(err.into_response().status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_form_reuses_the_browsers_nonce_and_makes_one_otherwise() {
        let made = nonce_for_form(&HeaderMap::new());
        assert!(is_nonce(&made), "{made}");
        assert_ne!(made, nonce_for_form(&HeaderMap::new()));
        let mut headers = HeaderMap::new();
        headers.append("cookie", format!("{NONCE_COOKIE}={made}").parse().unwrap());
        assert_eq!(nonce_for_form(&headers), made);
        let mut headers = HeaderMap::new();
        headers.append(
            "cookie",
            format!("{NONCE_COOKIE}=not-a-nonce").parse().unwrap(),
        );
        assert!(is_nonce(&nonce_for_form(&headers)));
    }

    #[test]
    fn nonce_cookies_are_http_only_strict_and_secure_over_https() {
        let nonce = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            nonce_cookie(nonce, false),
            format!("{NONCE_COOKIE}={nonce}; Path=/; Max-Age=900; HttpOnly; SameSite=Strict")
        );
        assert_eq!(
            nonce_cookie(nonce, true),
            format!(
                "{NONCE_COOKIE}={nonce}; Path=/; Max-Age=900; HttpOnly; SameSite=Strict; Secure"
            )
        );
        assert_eq!(
            clear_nonce_cookie(false),
            format!("{NONCE_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict")
        );
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let mut headers = HeaderMap::new();
        assert_eq!(cookie(&headers, SESSION_COOKIE), None);
        headers.append("cookie", "a=1; doozers_session=abc ; b=2".parse().unwrap());
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("abc"));
        let mut headers = HeaderMap::new();
        headers.append("cookie", "doozers_session=".parse().unwrap());
        headers.append("cookie", "x=y; doozers_session=def".parse().unwrap());
        assert_eq!(cookie(&headers, SESSION_COOKIE), Some("def"));
        headers.insert("cookie", "doozers_sessionx=1".parse().unwrap());
        assert_eq!(cookie(&headers, SESSION_COOKIE), None);
    }

    #[test]
    fn session_cookies_are_http_only_lax_and_secure_over_https() {
        assert_eq!(
            session_cookie("t", false),
            "doozers_session=t; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax"
        );
        assert_eq!(
            session_cookie("t", true),
            "doozers_session=t; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax; Secure"
        );
        assert_eq!(
            clear_session_cookie(true),
            "doozers_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Secure"
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
