//! Security headers on every HTTP response. The reasons for each are in `Docs/technical/http.md`.

use axum::http::header::{
    CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::Response;

/// The base Content Security Policy. Scripts, styles, and connections (htmx requests and
/// server-sent events) only from this origin, with no inline script or style; images also from
/// `data:` URLs, which the HTML renderer keeps; no plugins, no `<base>`, no framing, and forms only
/// post back here.
pub(crate) const CONTENT_SECURITY_POLICY_VALUE: &str = "default-src 'self'; \
     script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; \
     object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

const CROSS_ORIGIN_OPENER_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-opener-policy");
const CROSS_ORIGIN_RESOURCE_POLICY: HeaderName =
    HeaderName::from_static("cross-origin-resource-policy");

/// Headers with one fixed value, replacing whatever a handler set.
const FIXED: [(HeaderName, &str); 5] = [
    (X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (REFERRER_POLICY, "no-referrer"),
    (X_FRAME_OPTIONS, "DENY"),
    (CROSS_ORIGIN_OPENER_POLICY, "same-origin"),
    (CROSS_ORIGIN_RESOURCE_POLICY, "same-origin"),
];

/// The response middleware (`axum::middleware::map_response`).
pub(super) async fn add_security_headers(mut response: Response) -> Response {
    apply(response.headers_mut());
    response
}

/// Adds the base policy next to any policy a handler set (browsers enforce every
/// `Content-Security-Policy` header, so a handler can only tighten it, e.g. with `sandbox` for an
/// untrusted blob) and overwrites the fixed headers, so no handler can weaken them.
fn apply(headers: &mut HeaderMap) {
    headers.append(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY_VALUE),
    );
    for (name, value) in FIXED {
        headers.insert(name, HeaderValue::from_static(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_policy_is_kept_and_fixed_headers_are_overwritten() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
        headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
        headers.insert(REFERRER_POLICY, HeaderValue::from_static("unsafe-url"));
        apply(&mut headers);

        let policies: Vec<_> = headers.get_all(CONTENT_SECURITY_POLICY).iter().collect();
        assert_eq!(policies, ["sandbox", CONTENT_SECURITY_POLICY_VALUE]);
        assert_eq!(headers.get_all(X_FRAME_OPTIONS).iter().count(), 1);
        assert_eq!(headers[X_FRAME_OPTIONS], "DENY");
        assert_eq!(headers[REFERRER_POLICY], "no-referrer");
    }

    #[test]
    fn policy_has_the_required_directives_and_no_unsafe_sources() {
        let directives: Vec<&str> = CONTENT_SECURITY_POLICY_VALUE.split("; ").collect();
        for required in [
            "default-src 'self'",
            "script-src 'self'",
            "style-src 'self'",
            "img-src 'self' data:",
            "connect-src 'self'",
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
            "form-action 'self'",
        ] {
            assert!(directives.contains(&required), "missing {required:?}");
        }
        assert!(!CONTENT_SECURITY_POLICY_VALUE.contains("unsafe"));
        assert!(HeaderValue::from_str(CONTENT_SECURITY_POLICY_VALUE).is_ok());
    }
}
