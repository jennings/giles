//! Who is asking: the [`WebUser`] extractor and the CSRF check for forms.
//!
//! Authentication itself (the session cookie and the trusted-proxy header, TASKS.md 2.23) runs as
//! a middleware that puts a [`WebUser`] into the request's extensions. Handlers (and the admin
//! pages, 3.8) only extract it: a request without one gets the login page.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use giles_types::{NativeId, UserId};
use sha2::{Digest, Sha256};

use super::pages::{error_page, login_required};

/// The header htmx requests carry the CSRF token in (set on the page with `hx-headers`).
pub const CSRF_HEADER: &str = "x-csrf-token";

/// The form field plain form posts carry the CSRF token in.
pub const CSRF_FIELD: &str = "csrf";

/// An authenticated web user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUser {
    /// The user (v1: always the owner).
    pub user: UserId,
    /// The native identifier in the `web` namespace: what messages from this user carry as
    /// `InboundMessage::sender`, matched against `[owner] identities` as `web:<sender>`.
    pub sender: NativeId,
    /// This login's CSRF token: every form post must carry it ([`check_csrf`]).
    pub csrf: String,
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
}
