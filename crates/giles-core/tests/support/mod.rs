//! A local HTTP server that replays hand-written model API responses, and helpers for running
//! completions against it. Shared by the provider tests; each uses a part of it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures::StreamExt;
use giles_core::providers::MediaSource;
use giles_types::{
    Block, BlockKind, CompletionEvent, Error, MediaId, MediaRef, Message, MessageMeta, Provider,
    Role, Timestamp, Usage,
};
use serde_json::Value;
use socket2::{Domain, Socket, Type};

/// A request the test server received.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// `GET`, `POST`, ...
    pub method: String,
    /// The path and query.
    pub uri: String,
    /// The request headers.
    pub headers: HeaderMap,
    /// The request body.
    pub body: Bytes,
}

impl Recorded {
    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    /// A header's value, if present.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }
}

/// What the test server answers.
#[derive(Clone)]
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(&'static str, &'static str)>,
    /// The response body.
    pub body: Vec<u8>,
}

impl Reply {
    /// A 200 event stream with `body`.
    pub fn sse(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "text/event-stream; charset=utf-8")],
            body,
        }
    }

    /// A JSON response.
    pub fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("content-type", "application/json")],
            body: body.into(),
        }
    }
}

/// The bytes of `tests/fixtures/<dir>/<name>`.
pub fn fixture(dir: &str, name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{dir}/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// A loopback address that refuses connections for as long as the returned socket lives.
///
/// The socket is bound but never listened on, so connects to the address are refused, and no
/// other socket can take the port while it is held. Keep the socket alive until the test is done.
pub fn refused_address() -> (Socket, SocketAddr) {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    let addr = socket.local_addr().unwrap().as_socket().unwrap();
    (socket, addr)
}

type Responder = Arc<dyn Fn(&Recorded) -> Reply + Send + Sync>;

#[derive(Clone)]
struct ServerState {
    requests: Arc<Mutex<Vec<Recorded>>>,
    respond: Responder,
}

/// A local HTTP server on 127.0.0.1 that records requests and answers with `respond`. Bodies
/// are sent in 7-byte chunks, so SSE events and UTF-8 characters straddle chunk boundaries.
pub struct Server {
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Server {
    /// Starts a server answering every request with `respond`.
    pub async fn start(respond: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Self {
        let state = ServerState {
            requests: Arc::default(),
            respond: Arc::new(respond),
        };
        let requests = state.requests.clone();
        let app = axum::Router::new().fallback(handle).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, requests }
    }

    /// The requests received so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn handle(State(state): State<ServerState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let recorded = Recorded {
        method: parts.method.to_string(),
        uri: parts.uri.to_string(),
        headers: parts.headers,
        body: axum::body::to_bytes(body, usize::MAX).await.unwrap(),
    };
    let reply = (state.respond)(&recorded);
    state.requests.lock().unwrap().push(recorded);
    let chunks: Vec<Result<Bytes, Infallible>> = reply
        .body
        .chunks(7)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    let mut response = Response::builder().status(reply.status);
    for (name, value) in reply.headers {
        response = response.header(name, value);
    }
    response
        .body(Body::from_stream(futures::stream::iter(chunks)))
        .unwrap()
}

/// Media bytes by id.
#[derive(Default)]
pub struct Media(pub HashMap<MediaId, Bytes>);

#[async_trait]
impl MediaSource for Media {
    async fn read(&self, media: &MediaRef) -> giles_types::Result<Bytes> {
        self.0
            .get(&media.id)
            .cloned()
            .ok_or_else(|| Error::NotFound(media.id.to_string()))
    }
}

/// A message stored at the Unix epoch.
pub fn message(role: Role, content: Vec<Block>) -> Message {
    Message {
        role,
        content,
        meta: MessageMeta::new(Timestamp::UNIX_EPOCH),
    }
}

/// Runs a completion to the end and returns every item.
pub async fn complete(
    provider: &dyn Provider,
    req: giles_types::CompletionRequest,
) -> Vec<Result<CompletionEvent, Error>> {
    provider.complete(req).await.unwrap().collect().await
}

/// The events, which must all be `Ok`.
pub async fn events(
    provider: &dyn Provider,
    req: giles_types::CompletionRequest,
) -> Vec<CompletionEvent> {
    complete(provider, req)
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect()
}

/// The error `complete` returns before streaming.
pub async fn complete_error(provider: &dyn Provider, req: giles_types::CompletionRequest) -> Error {
    match provider.complete(req).await {
        Ok(_) => panic!("expected an error"),
        Err(error) => error,
    }
}

/// The blocks of a response, as the core stores them: the `BlockStop` blocks in order.
pub fn blocks(events: &[CompletionEvent]) -> Vec<Block> {
    events
        .iter()
        .filter_map(|event| match event {
            CompletionEvent::BlockStop { block, .. } => Some(block.clone()),
            _ => None,
        })
        .collect()
}

/// A `Usage` event.
pub fn usage(input: u64, output: u64, read: u64, write: u64) -> CompletionEvent {
    CompletionEvent::Usage(Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: read,
        cache_write_tokens: write,
    })
}

/// A `BlockStart` without a tool.
pub fn block_start(index: u32, kind: BlockKind) -> CompletionEvent {
    CompletionEvent::BlockStart {
        index,
        kind,
        tool: None,
    }
}
