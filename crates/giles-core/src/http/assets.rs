//! Static assets compiled into the binary: the vendored `htmx` and its SSE extension, the
//! stylesheet, and the scripts of the new-agent form and the MCP server form.
//!
//! Every asset's path changes whenever its content does (a version for the vendored files, a
//! content hash for our own), so all of them are served as `immutable` for a year. Pages link them
//! through these constants and functions (askama: `{{ crate::http::assets::HTMX.path }}`), never by
//! a hand-written path.

use std::sync::LazyLock;

use axum::Router;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use sha2::{Digest, Sha256};

/// `Content-Type` of JavaScript (RFC 9239).
const JAVASCRIPT: &str = "text/javascript; charset=utf-8";
/// `Content-Type` of CSS.
const CSS: &str = "text/css; charset=utf-8";
/// `Cache-Control` of every asset: its path changes with its content, so a cached copy never
/// goes stale.
pub(crate) const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// One file served from memory.
#[derive(Debug, Clone, Copy)]
pub struct Asset {
    /// The URL path, starting with `/static/`.
    pub path: &'static str,
    /// The `Content-Type` it is served with.
    pub content_type: &'static str,
    /// The file.
    pub body: &'static [u8],
}

/// `htmx` 2.0.11 (`htmx.org` on npm, `dist/htmx.min.js`, 0BSD).
pub const HTMX: Asset = Asset {
    path: "/static/htmx-2.0.11.min.js",
    content_type: JAVASCRIPT,
    body: include_bytes!("../../assets/vendor/htmx-2.0.11/htmx.min.js"),
};

/// The `htmx` SSE extension 2.2.4 (`htmx-ext-sse` on npm, `dist/sse.min.js`, 0BSD). Load it after
/// [`HTMX`]; it registers itself with `htmx`.
pub const HTMX_SSE: Asset = Asset {
    path: "/static/htmx-ext-sse-2.2.4.min.js",
    content_type: JAVASCRIPT,
    body: include_bytes!("../../assets/vendor/htmx-ext-sse-2.2.4/sse.min.js"),
};

const STYLESHEET_BODY: &[u8] = include_bytes!("../../assets/giles.css");

static STYLESHEET_PATH: LazyLock<String> =
    LazyLock::new(|| hashed_path("giles", STYLESHEET_BODY, "css"));

/// The URL path of the stylesheet: `/static/giles-<hash>.css`.
pub fn stylesheet_path() -> &'static str {
    STYLESHEET_PATH.as_str()
}

/// The stylesheet, at [`stylesheet_path`].
pub fn stylesheet() -> Asset {
    Asset {
        path: stylesheet_path(),
        content_type: CSS,
        body: STYLESHEET_BODY,
    }
}

const AGENT_FORM_SCRIPT_BODY: &[u8] = include_bytes!("../../assets/agent-form.js");

static AGENT_FORM_SCRIPT_PATH: LazyLock<String> =
    LazyLock::new(|| hashed_path("agent-form", AGENT_FORM_SCRIPT_BODY, "js"));

/// The URL path of the new-agent form's script: `/static/agent-form-<hash>.js`.
pub fn agent_form_script_path() -> &'static str {
    AGENT_FORM_SCRIPT_PATH.as_str()
}

/// The new-agent form's script, at [`agent_form_script_path`]: it fills the form's id from its name
/// as the owner types.
pub fn agent_form_script() -> Asset {
    Asset {
        path: agent_form_script_path(),
        content_type: JAVASCRIPT,
        body: AGENT_FORM_SCRIPT_BODY,
    }
}

const MCP_SERVER_FORM_SCRIPT_BODY: &[u8] = include_bytes!("../../assets/mcp-server-form.js");

static MCP_SERVER_FORM_SCRIPT_PATH: LazyLock<String> =
    LazyLock::new(|| hashed_path("mcp-server-form", MCP_SERVER_FORM_SCRIPT_BODY, "js"));

/// The URL path of the MCP server form's script: `/static/mcp-server-form-<hash>.js`.
pub fn mcp_server_form_script_path() -> &'static str {
    MCP_SERVER_FORM_SCRIPT_PATH.as_str()
}

/// The MCP server form's script, at [`mcp_server_form_script_path`]: it shows "Allow insecure
/// connections" only while the URL starts with `http:`.
pub fn mcp_server_form_script() -> Asset {
    Asset {
        path: mcp_server_form_script_path(),
        content_type: JAVASCRIPT,
        body: MCP_SERVER_FORM_SCRIPT_BODY,
    }
}

/// Every asset.
pub fn all() -> [Asset; 5] {
    [
        HTMX,
        HTMX_SSE,
        stylesheet(),
        agent_form_script(),
        mcp_server_form_script(),
    ]
}

/// `/static/<stem>-<hash>.<extension>`, the hash being the first 16 hex digits of the SHA-256 of
/// `body`: enough to make an accidental collision between two versions of one file implausible.
fn hashed_path(stem: &str, body: &[u8], extension: &str) -> String {
    let digest = Sha256::digest(body);
    let hash: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("/static/{stem}-{hash}.{extension}")
}

/// A `GET` (and `HEAD`) route for every asset.
pub(super) fn routes() -> Router {
    all().into_iter().fold(Router::new(), |router, asset| {
        router.route(asset.path, get(move || async move { response(asset) }))
    })
}

fn response(asset: Asset) -> Response {
    (
        [
            (CONTENT_TYPE, asset.content_type),
            (CACHE_CONTROL, IMMUTABLE),
        ],
        asset.body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_path_changes_with_the_content_only() {
        let a = hashed_path("giles", b"body { color: red }", "css");
        assert_eq!(a, hashed_path("giles", b"body { color: red }", "css"));
        assert_ne!(a, hashed_path("giles", b"body { color: blue }", "css"));
        let hash = a
            .strip_prefix("/static/giles-")
            .and_then(|rest| rest.strip_suffix(".css"))
            .unwrap();
        assert_eq!(hash.len(), 16);
        assert!(
            hash.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }
}
