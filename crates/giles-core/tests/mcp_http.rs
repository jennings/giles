//! The MCP client over streamable HTTP, against the `rmcp` test server served on 127.0.0.1 behind a
//! bearer token check.
#![allow(missing_docs)]

#[path = "support/mcp_server.rs"]
mod mcp_server;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use giles_core::config::Secret;
use giles_core::mcp::{HttpServer, McpToolset};
use giles_types::{
    AgentId, Block, CallContext, Error, ToolCall, ToolCallEvent, ToolCallHandle, Toolset,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};

use self::mcp_server::{Shared, TOOLS, TestServer, no_input};

const TOKEN: &str = "s3cret-token";
/// Gets a 403 whose body echoes the `Authorization` header, as some servers do.
const FORBIDDEN_TOKEN: &str = "forbidden-token-123";
const WRONG_TOKEN: &str = "wrong-token-456";
const WAIT: Duration = Duration::from_secs(10);

/// Serves a fresh test server at `http://127.0.0.1:<port>/mcp`, requiring `Bearer TOKEN`.
async fn serve() -> (String, Arc<Shared>) {
    let server = TestServer::default();
    let shared = server.shared.clone();
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(require_bearer));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await });
    (url, shared)
}

/// Passes `Bearer TOKEN`. Rejects other requests the ways real servers do: no token gets a bare
/// 401, [`FORBIDDEN_TOKEN`] a 403 without `WWW-Authenticate` that echoes the header, and any other
/// token a 401 with `WWW-Authenticate`.
async fn require_bearer(request: Request, next: Next) -> Response {
    let authorization = request.headers().get(AUTHORIZATION);
    match authorization.and_then(|value| value.to_str().ok()) {
        Some(value) if value == format!("Bearer {TOKEN}") => next.run(request).await,
        None => StatusCode::UNAUTHORIZED.into_response(),
        Some(value) if value == format!("Bearer {FORBIDDEN_TOKEN}") => {
            (StatusCode::FORBIDDEN, format!("you sent {value}")).into_response()
        }
        Some(_) => (StatusCode::UNAUTHORIZED, [(WWW_AUTHENTICATE, "Bearer")]).into_response(),
    }
}

fn server(url: &str, token: Option<&str>) -> HttpServer {
    let mut server = HttpServer::new(url.parse().unwrap());
    server.bearer = token.map(Secret::new);
    server
}

async fn call(toolset: &McpToolset, name: &str, input: Value) -> ToolCallHandle {
    let call = ToolCall {
        id: format!("toolu_{name}"),
        name: name.to_owned(),
        input,
        media: Vec::new(),
    };
    let ctx = CallContext {
        agent: AgentId::new("agent").unwrap(),
        requester: None,
        conversation: None,
    };
    toolset.call(call, ctx).await.unwrap()
}

async fn next(handle: &mut ToolCallHandle) -> Option<giles_types::Result<ToolCallEvent>> {
    tokio::time::timeout(WAIT, handle.events.next())
        .await
        .expect("timed out waiting for a call event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_right_token_lists_calls_and_cancels() {
    let (url, shared) = serve().await;
    let toolset = McpToolset::connect_http("remote", &server(&url, Some(TOKEN)))
        .await
        .unwrap();

    let tools = toolset.list_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, TOOLS);

    let mut steps = call(
        &toolset,
        "progress_steps",
        json!({"steps": 2, "gated": true}),
    )
    .await;
    for step in [1.0, 2.0] {
        assert!(
            matches!(
                next(&mut steps).await,
                Some(Ok(ToolCallEvent::Progress { progress, .. })) if progress == step
            ),
            "progress {step}"
        );
        shared.gate.add_permits(1);
    }
    assert_eq!(
        next(&mut steps).await.unwrap().unwrap(),
        ToolCallEvent::Result {
            content: vec![Block::Text {
                text: "done".to_owned()
            }],
            is_error: false,
        }
    );
    assert!(next(&mut steps).await.is_none());

    let mut hang = call(&toolset, "hang", no_input()).await;
    assert!(matches!(
        next(&mut hang).await,
        Some(Ok(ToolCallEvent::Progress { .. }))
    ));
    drop(hang);
    tokio::time::timeout(WAIT, async {
        loop {
            let notified = shared.cancelled_notify.notified();
            if *shared.cancelled.lock().unwrap() == 1 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("the server never saw the cancellation");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_or_missing_token_is_rejected() {
    let (url, _shared) = serve().await;
    for token in [Some(WRONG_TOKEN), Some(FORBIDDEN_TOKEN), None] {
        match McpToolset::connect_http("remote", &server(&url, token)).await {
            Err(Error::InvalidRequest(message)) => {
                assert!(
                    message.starts_with("MCP server remote rejected the credentials"),
                    "{message}"
                );
                if let Some(token) = token {
                    assert!(!message.contains(token), "{message}");
                }
                if token == Some(FORBIDDEN_TOKEN) {
                    // The echoed token was in the response body, and was scrubbed.
                    assert!(message.contains("you sent Bearer <redacted>"), "{message}");
                }
            }
            other => panic!("expected InvalidRequest for {token:?}, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_server_is_unavailable() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    drop(listener);
    match McpToolset::connect_http("remote", &server(&url, Some(TOKEN))).await {
        Err(error @ Error::Unavailable { .. }) => assert!(error.is_retryable()),
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
