use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::header::{CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_FRAME_OPTIONS};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::ServiceExt;

use super::assets::{self, HTMX, HTMX_SSE, IMMUTABLE};
use super::headers::CONTENT_SECURITY_POLICY_VALUE;
use super::*;

const SECURITY_HEADERS: [(&str, &str); 6] = [
    ("content-security-policy", CONTENT_SECURITY_POLICY_VALUE),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("x-frame-options", "DENY"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
];

async fn request(app: Router, method: Method, path: &str) -> Response {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    app.oneshot(request).await.unwrap()
}

async fn get_page(path: &str) -> (Response, String) {
    let response = request(RouterBuilder::new().build(), Method::GET, path).await;
    let (parts, body) = response.into_parts();
    let body = to_bytes(body, usize::MAX).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    (Response::from_parts(parts, Body::empty()), text)
}

fn assert_security_headers(what: &str, headers: &HeaderMap) {
    for (name, value) in SECURITY_HEADERS {
        let values: Vec<_> = headers.get_all(name).iter().collect();
        assert!(
            values.contains(&&value.parse().unwrap()),
            "{what}: {name} is {values:?}"
        );
    }
}

#[tokio::test]
async fn every_response_has_the_security_headers() {
    let stylesheet = assets::stylesheet_path();
    let cases = [
        (Method::GET, "/", StatusCode::OK),
        (Method::HEAD, "/", StatusCode::OK),
        (Method::GET, HTMX.path, StatusCode::OK),
        (Method::GET, HTMX_SSE.path, StatusCode::OK),
        (Method::HEAD, HTMX_SSE.path, StatusCode::OK),
        (
            Method::GET,
            assets::agent_form_script_path(),
            StatusCode::OK,
        ),
        (Method::GET, stylesheet, StatusCode::OK),
        (Method::GET, "/no/such/page", StatusCode::NOT_FOUND),
        (
            Method::GET,
            "/static/htmx-0.0.0.min.js",
            StatusCode::NOT_FOUND,
        ),
        (Method::POST, "/", StatusCode::METHOD_NOT_ALLOWED),
        (Method::DELETE, HTMX.path, StatusCode::METHOD_NOT_ALLOWED),
    ];
    for (method, path, status) in cases {
        let what = format!("{method} {path}");
        let response = request(RouterBuilder::new().build(), method, path).await;
        assert_eq!(response.status(), status, "{what}");
        assert_security_headers(&what, response.headers());
    }
}

#[tokio::test]
async fn merged_routes_get_the_headers_and_can_only_tighten_them() {
    async fn blob() -> Response {
        (
            [
                (CONTENT_SECURITY_POLICY, "sandbox"),
                (X_FRAME_OPTIONS, "SAMEORIGIN"),
            ],
            "<script>alert(1)</script>",
        )
            .into_response()
    }
    let extra = Router::new().route("/blobs/x", get(blob));
    let app = RouterBuilder::new().merge(extra).build();

    let response = request(app, Method::GET, "/blobs/x").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_security_headers("merged route", response.headers());
    let policies: Vec<_> = response
        .headers()
        .get_all(CONTENT_SECURITY_POLICY)
        .iter()
        .collect();
    assert_eq!(policies, ["sandbox", CONTENT_SECURITY_POLICY_VALUE]);
    let frame: Vec<_> = response.headers().get_all(X_FRAME_OPTIONS).iter().collect();
    assert_eq!(frame, ["DENY"]);
}

#[tokio::test]
async fn assets_are_served_whole_with_their_type_and_cached_forever() {
    for asset in assets::all() {
        let response = request(RouterBuilder::new().build(), Method::GET, asset.path).await;
        assert_eq!(response.status(), StatusCode::OK, "{}", asset.path);
        assert_eq!(response.headers()[CONTENT_TYPE], asset.content_type);
        assert_eq!(response.headers()[CACHE_CONTROL], IMMUTABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, asset.body, "{}", asset.path);
    }
    assert_eq!(HTMX.content_type, "text/javascript; charset=utf-8");
    assert_eq!(HTMX_SSE.content_type, "text/javascript; charset=utf-8");
    assert_eq!(assets::stylesheet().content_type, "text/css; charset=utf-8");
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The vendored files are byte-for-byte the npm release files whose checksums are recorded in
/// `Docs/technical/http.md`. Upgrading means fetching the new release, updating the path, and
/// updating both checksums.
#[test]
fn vendored_files_match_the_recorded_releases() {
    assert_eq!(
        sha256_hex(HTMX.body),
        "d6fdc75f204e6bdefa99b69bf1e6d4ac69b8a364f77929f45c13476b4000f717",
    );
    assert_eq!(
        sha256_hex(HTMX_SSE.body),
        "98a46496de0c3605fbffdce9167ba427bdd9553184f83f149c261891a92c0136",
    );
    // The version in the path is the version of the file.
    let htmx = std::str::from_utf8(HTMX.body).unwrap();
    assert!(htmx.contains(r#"version:"2.0.11""#));
    assert!(HTMX.path.contains("-2.0.11."));
}

#[test]
fn stylesheet_path_is_its_content_hash() {
    let stylesheet = assets::stylesheet();
    let hash = &sha256_hex(stylesheet.body)[..16];
    assert_eq!(stylesheet.path, format!("/static/giles-{hash}.css"));
}

#[test]
fn agent_form_script_path_is_its_content_hash() {
    let script = assets::agent_form_script();
    let hash = &sha256_hex(script.body)[..16];
    assert_eq!(script.path, format!("/static/agent-form-{hash}.js"));
    assert_eq!(script.content_type, "text/javascript; charset=utf-8");
}

/// The script's `derive` is `agents::agent_id_from_name` in JavaScript. Run by hand over the
/// same table as the Rust test when either changes; this pins the expression so the two can't
/// drift apart unnoticed.
#[test]
fn agent_form_script_derives_the_id_as_the_rust_function_does() {
    let script = std::str::from_utf8(assets::agent_form_script().body).unwrap();
    assert!(
        script.contains("text.toLowerCase().replace(/ /g, '-').replace(/[^a-z0-9-]/g, '')"),
        "{script}"
    );
}

#[tokio::test]
async fn home_page_renders_in_the_layout() {
    let (response, html) = get_page("/").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert!(html.starts_with("<!DOCTYPE html>"), "{html}");
    assert!(html.contains("<title>Giles</title>"), "{html}");
    assert!(html.contains(&format!("Giles {} is running.", env!("CARGO_PKG_VERSION"))));
    assert!(html.contains(&format!(
        r#"<link rel="stylesheet" href="{}">"#,
        assets::stylesheet_path()
    )));
    // htmx first: the extension registers itself with it.
    let htmx = html.find(&format!(r#"<script src="{}" defer></script>"#, HTMX.path));
    let sse = html.find(&format!(
        r#"<script src="{}" defer></script>"#,
        HTMX_SSE.path
    ));
    assert!(htmx.unwrap() < sse.unwrap(), "{html}");
    assert!(html.contains(r#""includeIndicatorStyles":false"#), "{html}");
}

#[tokio::test]
async fn pages_have_nothing_inline_the_policy_would_block() {
    for path in ["/", "/no/such/page"] {
        let (_, html) = get_page(path).await;
        assert_eq!(
            html.matches("<script").count(),
            html.matches("<script src=").count()
        );
        assert!(!html.contains("<style"), "{path}: {html}");
        assert!(!html.contains(" style="), "{path}: {html}");
    }
}

#[tokio::test]
async fn unknown_paths_get_the_not_found_page() {
    let (response, html) = get_page("/no/such/page").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html; charset=utf-8");
    assert!(html.contains("<title>Not found - Giles</title>"), "{html}");
    assert!(html.contains(HTMX.path), "not in the layout: {html}");
}

async fn http_get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn real_server_serves_and_stops_on_the_shutdown_token() {
    let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        RouterBuilder::new().build(),
        shutdown.clone(),
    ));

    let response = http_get(addr, "/").await;
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let lower = response.to_ascii_lowercase();
    assert!(
        lower.contains("\r\nx-frame-options: deny\r\n"),
        "{response}"
    );
    assert!(response.contains("is running."), "{response}");

    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), server).await;
    result.expect("server did not stop").unwrap().unwrap();
    assert!(TcpStream::connect(addr).await.is_err(), "still listening");
}

#[tokio::test]
async fn shutdown_drops_requests_still_open_at_the_deadline() {
    let hang = Router::new().route("/hang", get(std::future::pending::<()>));
    let app = RouterBuilder::new().merge(hang).build();
    let listener = bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shutdown = CancellationToken::new();
    let drain = Duration::from_millis(100);
    let server = tokio::spawn(serve_with_drain(listener, app, shutdown.clone(), drain));

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /hang HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    // Let the server start handling it, so it is in flight when shutdown begins.
    tokio::time::sleep(Duration::from_millis(100)).await;

    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), server).await;
    result
        .expect("server waited past the drain deadline")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn bind_reports_the_address_it_could_not_use() {
    let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = taken.local_addr().unwrap();
    let err = bind(addr).await.unwrap_err();
    assert_eq!(err.addr, addr);
    assert_eq!(err.source.kind(), std::io::ErrorKind::AddrInUse);
    assert_eq!(err.to_string(), format!("cannot listen for HTTP on {addr}"));
}

#[tokio::test]
async fn home_replaces_the_placeholder_page() {
    let app = RouterBuilder::new().home(get(|| async { "chats" })).build();
    let response = request(app, Method::GET, "/").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_security_headers("home", response.headers());
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body, "chats");
}

#[cfg(unix)]
#[tokio::test]
async fn serves_on_a_unix_socket_too() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("http.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(serve(
        listener,
        RouterBuilder::new().build(),
        shutdown.clone(),
    ));

    let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");

    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), server).await;
    result.expect("server did not stop").unwrap().unwrap();
}
