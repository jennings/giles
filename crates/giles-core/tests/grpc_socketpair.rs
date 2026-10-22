//! Plugin transport spike (PLAN section 7.4): `tonic` over one pre-connected Unix socket pair.
//!
//! The core is the gRPC client and the plugin the server, on the two ends of a `socketpair`. The
//! plugin serves on its end through `serve_with_incoming*` with a one-item stream; the core
//! connects through a connector that hands out its end once. These tests pin the behavior the
//! plugin SDK and the `Remote*` adapters rely on: bearer-token metadata, 16 MiB message limits,
//! many RPCs on the one HTTP/2 connection, and prompt failure (no reconnect, no hang) when the
//! plugin goes away. See `Docs/technical/plugin-transport.md`.

use std::convert::Infallible;
use std::io;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::channel::oneshot;
use futures::future::join_all;
use futures::stream::{self, Stream, StreamExt};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::body::Body;
use tonic::codegen::{BoxFuture, Service, http};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::server::NamedService;
use tonic::service::interceptor::InterceptedService;
use tonic::service::{Interceptor, InterceptorLayer};
use tonic::transport::server::Router;
use tonic::transport::{Channel, Endpoint, Server, Uri};
use tonic::{Code, Request, Response, Status};
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::{HealthCheckRequest, HealthCheckResponse};
use tonic_health::server::HealthReporter;
use tonic_prost::ProstCodec;
use tower::layer::util::{Identity, Stack};

/// Per-message limit both sides set (PLAN section 8.1), instead of `tonic`'s 4 MiB decode default.
const LIMIT: usize = 16 * 1024 * 1024;
/// The per-launch token the core writes to the plugin's stdin.
const TOKEN: &str = "per-launch-token";
/// Upper bound for anything that must not hang.
const DEADLINE: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// Bearer-token interceptors.

/// Client side: adds `authorization: Bearer <token>` to every call.
#[derive(Clone)]
struct AddBearer(MetadataValue<Ascii>);

impl AddBearer {
    fn new(token: &str) -> Self {
        Self(bearer_value(token))
    }
}

impl Interceptor for AddBearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.0.clone());
        Ok(request)
    }
}

/// Server side: rejects calls without the expected `authorization` value.
#[derive(Clone)]
struct RequireBearer(MetadataValue<Ascii>);

impl Interceptor for RequireBearer {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        match request.metadata().get("authorization") {
            Some(value) if *value == self.0 => Ok(request),
            _ => Err(Status::unauthenticated("missing or invalid plugin token")),
        }
    }
}

fn bearer_value(token: &str) -> MetadataValue<Ascii> {
    format!("Bearer {token}")
        .parse()
        .expect("token is valid ASCII metadata")
}

// ---------------------------------------------------------------------------------------------
// A tiny hand-written service with a payload of chosen size in either direction. Health messages
// are a few bytes, so the message limits need this; it goes through the same `tonic::server::Grpc`
// and `tonic_prost::ProstCodec` path generated services use, without codegen.

/// Request and response of `giles.test.Bulk/Exchange`.
#[derive(Clone, PartialEq, prost::Message)]
struct Payload {
    /// Length of `data` the server puts in its reply.
    #[prost(uint64, tag = "1")]
    reply_len: u64,
    #[prost(bytes = "vec", tag = "2")]
    data: Vec<u8>,
}

impl Payload {
    /// A payload whose protobuf encoding is exactly `encoded_len` bytes.
    fn of_encoded_len(encoded_len: usize, reply_len: u64) -> Self {
        let mut payload = Self {
            reply_len,
            data: Vec::new(),
        };
        // `data` costs a 1-byte tag, a varint length, and the bytes.
        let room = encoded_len - prost::Message::encoded_len(&payload) - 1;
        let data_len = (1..=10)
            .map(|varint| room - varint)
            .find(|&len| prost::encoding::encoded_len_varint(len as u64) == room - len)
            .expect("some varint width fits");
        payload.data = vec![0; data_len];
        assert_eq!(prost::Message::encoded_len(&payload), encoded_len);
        payload
    }

    /// The `reply_len` that makes the server's reply encode to exactly `encoded_len` bytes.
    fn reply_len_for(encoded_len: usize) -> u64 {
        let reply = Self::of_encoded_len(encoded_len, 0);
        u64::try_from(reply.data.len()).expect("fits in u64")
    }
}

#[derive(Clone)]
struct Bulk {
    limit: usize,
}

impl NamedService for Bulk {
    const NAME: &'static str = "giles.test.Bulk";
}

impl Service<http::Request<Body>> for Bulk {
    type Response = http::Response<Body>;
    type Error = Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let mut grpc = tonic::server::Grpc::new(ProstCodec::<Payload, Payload>::default())
            .max_decoding_message_size(self.limit)
            .max_encoding_message_size(self.limit);
        Box::pin(async move { Ok(grpc.unary(tower::service_fn(exchange), request).await) })
    }
}

async fn exchange(request: Request<Payload>) -> Result<Response<Payload>, Status> {
    let len = usize::try_from(request.into_inner().reply_len).expect("fits in usize");
    Ok(Response::new(Payload {
        reply_len: 0,
        data: vec![0; len],
    }))
}

// ---------------------------------------------------------------------------------------------
// Plugin side.

type PluginRouter = Router<Stack<InterceptorLayer<RequireBearer>, Identity>>;

/// The plugin's server: health and `Bulk` with `limit`, behind the bearer-token check.
fn plugin_router(limit: usize) -> (HealthReporter, PluginRouter) {
    let (reporter, health) = tonic_health::server::health_reporter();
    let router = Server::builder()
        .layer(InterceptorLayer::new(RequireBearer(bearer_value(TOKEN))))
        .add_service(
            health
                .max_decoding_message_size(limit)
                .max_encoding_message_size(limit),
        )
        .add_service(Bulk { limit });
    (reporter, router)
}

/// The connection as the only item of an `incoming` stream that then stays open, so the server
/// runs until its shutdown signal rather than until the stream ends.
fn single_connection(
    io: UnixStream,
) -> impl Stream<Item = Result<UnixStream, io::Error>> + Send + 'static {
    stream::iter([Ok(io)]).chain(stream::pending())
}

/// Spawns the plugin's server on `io` until `stop` fires (or is dropped).
fn spawn_plugin(
    io: UnixStream,
    limit: usize,
) -> (
    HealthReporter,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
) {
    let (reporter, router) = plugin_router(limit);
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(
        router.serve_with_incoming_shutdown(single_connection(io), async {
            let _ = stopped.await;
        }),
    );
    (reporter, stop, server)
}

// ---------------------------------------------------------------------------------------------
// Core side.

/// A connector that yields `io` on its first call and fails every later one, so the channel
/// can never reconnect.
fn one_shot_connector(
    io: UnixStream,
) -> impl Service<
    Uri,
    Response = TokioIo<UnixStream>,
    Error = io::Error,
    Future = impl Future<Output = io::Result<TokioIo<UnixStream>>> + Send,
> + Send
+ 'static {
    let mut io = Some(io);
    tower::service_fn(move |_: Uri| {
        let io = io.take();
        async move {
            io.map(TokioIo::new).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotConnected,
                    "plugin connection already used",
                )
            })
        }
    })
}

/// Connects the core's channel over `io`. The URI is never resolved; it only names the
/// `:authority`.
async fn connect(io: UnixStream) -> Result<Channel, tonic::transport::Error> {
    Endpoint::from_static("http://plugin")
        .connect_with_connector(one_shot_connector(io))
        .await
}

type AuthedChannel = InterceptedService<Channel, AddBearer>;

fn health_client(channel: Channel) -> HealthClient<AuthedChannel> {
    HealthClient::with_interceptor(channel, AddBearer::new(TOKEN))
        .max_decoding_message_size(LIMIT)
        .max_encoding_message_size(LIMIT)
}

fn bulk_client(channel: Channel, limit: usize) -> tonic::client::Grpc<AuthedChannel> {
    tonic::client::Grpc::new(InterceptedService::new(channel, AddBearer::new(TOKEN)))
        .max_decoding_message_size(limit)
        .max_encoding_message_size(limit)
}

async fn bulk_exchange(
    client: &mut tonic::client::Grpc<AuthedChannel>,
    payload: Payload,
) -> Result<Payload, Status> {
    client
        .ready()
        .await
        .map_err(|e| Status::unavailable(e.to_string()))?;
    let path = http::uri::PathAndQuery::from_static("/giles.test.Bulk/Exchange");
    let codec = ProstCodec::<Payload, Payload>::default();
    let response = client.unary(Request::new(payload), path, codec).await?;
    Ok(response.into_inner())
}

async fn check(client: &mut HealthClient<AuthedChannel>, service: &str) -> Result<i32, Status> {
    let request = HealthCheckRequest {
        service: service.to_owned(),
    };
    Ok(client.check(request).await?.into_inner().status)
}

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(DEADLINE, future)
        .await
        .expect("operation must not hang")
}

/// A connected pair as `tokio` streams, through the same `std` conversion the SDK uses on fd 3.
fn tokio_pair() -> (UnixStream, UnixStream) {
    let (core_end, plugin_end) = StdUnixStream::pair().expect("socketpair");
    (into_tokio(core_end), into_tokio(plugin_end))
}

/// `tokio` requires the socket to be non-blocking before `from_std`; an inherited fd is not.
fn into_tokio(stream: StdUnixStream) -> UnixStream {
    stream.set_nonblocking(true).expect("set non-blocking");
    UnixStream::from_std(stream).expect("register with tokio")
}

/// A plugin on a fresh pair with the standard limit, and the core's connected channel.
async fn start() -> (HealthReporter, oneshot::Sender<()>, Channel) {
    let (core_end, plugin_end) = tokio_pair();
    let (reporter, stop, _server) = spawn_plugin(plugin_end, LIMIT);
    let channel = bounded(connect(core_end)).await.expect("connect");
    (reporter, stop, channel)
}

// ---------------------------------------------------------------------------------------------
// Tests.

#[tokio::test]
async fn health_check_works_over_the_socket_pair() {
    let (_reporter, _stop, channel) = start().await;
    let mut health = health_client(channel);
    assert_eq!(
        bounded(check(&mut health, "")).await.unwrap(),
        ServingStatus::Serving as i32
    );
    let unknown = bounded(check(&mut health, "nope")).await.unwrap_err();
    assert_eq!(unknown.code(), Code::NotFound);
}

#[tokio::test]
async fn one_item_incoming_without_pending_returns_but_keeps_serving() {
    // The literal shape: `serve_with_incoming` with a one-item stream. The server future
    // completes as soon as the stream ends, but the accepted connection runs on a detached task
    // and keeps serving. So the future's completion says nothing about the connection.
    let (core_end, plugin_end) = tokio_pair();
    let (_reporter, router) = plugin_router(LIMIT);
    let incoming = stream::iter([Ok::<_, io::Error>(plugin_end)]);
    bounded(router.serve_with_incoming(incoming))
        .await
        .expect("server returns once the incoming stream ends");

    let mut health = health_client(bounded(connect(core_end)).await.expect("connect"));
    assert_eq!(
        bounded(check(&mut health, "")).await.unwrap(),
        ServingStatus::Serving as i32
    );
}

#[tokio::test]
async fn calls_without_the_bearer_token_are_unauthenticated() {
    let (_reporter, _stop, channel) = start().await;

    let mut anonymous = HealthClient::new(channel.clone());
    let status = bounded(anonymous.check(HealthCheckRequest::default()))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    let mut wrong = HealthClient::with_interceptor(channel.clone(), AddBearer::new("guess"));
    let status = bounded(wrong.check(HealthCheckRequest::default()))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    // The rejections don't hurt the shared connection.
    let mut health = health_client(channel);
    assert_eq!(
        bounded(check(&mut health, "")).await.unwrap(),
        ServingStatus::Serving as i32
    );
}

#[tokio::test]
async fn messages_of_exactly_16_mib_pass_both_ways() {
    let (_reporter, _stop, channel) = start().await;
    let mut bulk = bulk_client(channel, LIMIT);

    let request = Payload::of_encoded_len(LIMIT, 0);
    let reply = bounded(bulk_exchange(&mut bulk, request)).await.unwrap();
    assert!(reply.data.is_empty());

    let request = Payload::of_encoded_len(64, Payload::reply_len_for(LIMIT));
    let reply = bounded(bulk_exchange(&mut bulk, request)).await.unwrap();
    assert_eq!(prost::Message::encoded_len(&reply), LIMIT);
}

#[tokio::test]
async fn oversized_request_is_refused_by_the_client_encoder() {
    let (_reporter, _stop, channel) = start().await;
    let mut bulk = bulk_client(channel.clone(), LIMIT);
    let status = bounded(bulk_exchange(
        &mut bulk,
        Payload::of_encoded_len(LIMIT + 1, 0),
    ))
    .await
    .unwrap_err();
    // tonic 0.14 loses the encoder's `OutOfRange`: the failed request body makes hyper reset the
    // stream, and the caller sees only an `Internal` h2 error. Callers that want a clear error
    // must check the size before sending (see the technical doc).
    assert_eq!(status.code(), Code::Internal, "{status:?}");
    assert!(status.message().contains("h2 protocol error"), "{status:?}");

    // The reset stream doesn't break the connection.
    let mut health = health_client(channel);
    assert_eq!(
        bounded(check(&mut health, "")).await.unwrap(),
        ServingStatus::Serving as i32
    );
}

#[tokio::test]
async fn oversized_request_is_refused_by_the_server_decoder() {
    let (_reporter, _stop, channel) = start().await;
    // A client that doesn't limit what it sends, e.g. a buggy or older core.
    let mut bulk = bulk_client(channel.clone(), usize::MAX);
    let status = bounded(bulk_exchange(
        &mut bulk,
        Payload::of_encoded_len(LIMIT + 1, 0),
    ))
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::OutOfRange, "{status:?}");
    assert!(
        status
            .message()
            .contains("decoded message length too large")
    );

    // One refused message doesn't break the connection.
    let mut health = health_client(channel);
    assert_eq!(
        bounded(check(&mut health, "")).await.unwrap(),
        ServingStatus::Serving as i32
    );
}

#[tokio::test]
async fn oversized_response_is_refused_by_the_server_encoder() {
    let (_reporter, _stop, channel) = start().await;
    let mut bulk = bulk_client(channel, usize::MAX);
    let request = Payload::of_encoded_len(64, Payload::reply_len_for(LIMIT + 1));
    let status = bounded(bulk_exchange(&mut bulk, request))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::OutOfRange, "{status:?}");
    assert!(
        status
            .message()
            .contains("encoded message length too large")
    );
}

#[tokio::test]
async fn oversized_response_is_refused_by_the_client_decoder() {
    // A plugin that doesn't limit what it sends.
    let (core_end, plugin_end) = tokio_pair();
    let (_reporter, _stop, _server) = spawn_plugin(plugin_end, usize::MAX);
    let channel = bounded(connect(core_end)).await.expect("connect");
    let mut bulk = bulk_client(channel, LIMIT);
    let request = Payload::of_encoded_len(64, Payload::reply_len_for(LIMIT + 1));
    let status = bounded(bulk_exchange(&mut bulk, request))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::OutOfRange, "{status:?}");
    assert!(
        status
            .message()
            .contains("decoded message length too large")
    );
}

#[tokio::test]
async fn concurrent_rpcs_share_the_one_connection() {
    // The connector refuses a second connection, so everything below runs on one HTTP/2
    // connection: open server streams, unary calls, and 1 MiB exchanges, all at once.
    let (reporter, _stop, channel) = start().await;
    let health = health_client(channel.clone());

    reporter.set_serving::<Bulk>().await;
    let mut watches = Vec::new();
    for _ in 0..8 {
        let request = HealthCheckRequest {
            service: Bulk::NAME.to_owned(),
        };
        let mut watch = bounded(health.clone().watch(request))
            .await
            .unwrap()
            .into_inner();
        let first = bounded(watch.message()).await.unwrap().unwrap();
        assert_eq!(first.status, ServingStatus::Serving as i32);
        watches.push(watch);
    }

    let checks = (0..16).map(|_| {
        let mut health = health.clone();
        async move { check(&mut health, "").await }
    });
    let exchanges = (0..8).map(|_| {
        let mut bulk = bulk_client(channel.clone(), LIMIT);
        async move {
            let request = Payload::of_encoded_len(1 << 20, 1 << 20);
            bulk_exchange(&mut bulk, request).await
        }
    });
    let (checks, exchanges) =
        bounded(futures::future::join(join_all(checks), join_all(exchanges))).await;
    assert!(
        checks
            .into_iter()
            .all(|r| r.unwrap() == ServingStatus::Serving as i32)
    );
    assert!(
        exchanges
            .into_iter()
            .all(|r| r.unwrap().data.len() == 1 << 20)
    );

    // Every open stream still delivers.
    reporter.set_not_serving::<Bulk>().await;
    for watch in &mut watches {
        let next: Option<HealthCheckResponse> = bounded(watch.message()).await.unwrap();
        assert_eq!(next.unwrap().status, ServingStatus::NotServing as i32);
    }
}

#[tokio::test]
#[allow(unsafe_code)]
async fn server_runs_on_a_stream_adopted_from_a_raw_fd() {
    // What the SDK does with the inherited fd 3: take ownership of the raw fd, build a `std`
    // stream from it, make it non-blocking, and hand it to `tokio`.
    let (core_end, plugin_end) = StdUnixStream::pair().expect("socketpair");
    let raw = plugin_end.into_raw_fd();
    // SAFETY: `into_raw_fd` just released `raw`; nothing else owns or closes it, so the
    // `OwnedFd` becomes its only owner (as fd 3 is the SDK's alone after exec).
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    let plugin_end = into_tokio(StdUnixStream::from(owned));

    let (_reporter, _stop, _server) = spawn_plugin(plugin_end, LIMIT);
    let channel = bounded(connect(into_tokio(core_end)))
        .await
        .expect("connect");
    let mut health = health_client(channel);
    assert_eq!(
        bounded(check(&mut health, "")).await.unwrap(),
        ServingStatus::Serving as i32
    );
}

#[tokio::test]
async fn connect_fails_when_the_plugin_end_is_already_closed() {
    let (core_end, plugin_end) = tokio_pair();
    drop(plugin_end);
    assert!(bounded(connect(core_end)).await.is_err());
}

#[tokio::test]
async fn calls_fail_promptly_after_the_plugin_dies() {
    // The plugin runs on its own runtime and thread; dropping that runtime drops every task,
    // including the detached connection task, which closes the socket as a dying process would.
    let (core_end, plugin_end) = StdUnixStream::pair().expect("socketpair");
    let (kill, killed) = oneshot::channel::<()>();
    let plugin = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("plugin runtime");
        runtime.block_on(async move {
            let (_reporter, _stop, _server) = spawn_plugin(into_tokio(plugin_end), LIMIT);
            let _ = killed.await;
        });
    });

    let channel = bounded(connect(into_tokio(core_end)))
        .await
        .expect("connect");
    let mut health = health_client(channel);
    let request = HealthCheckRequest::default();
    let mut watch = bounded(health.watch(request)).await.unwrap().into_inner();
    let first = bounded(watch.message()).await.unwrap().unwrap();
    assert_eq!(first.status, ServingStatus::Serving as i32);

    kill.send(()).expect("plugin is running");
    plugin.join().expect("plugin thread");

    // The open stream ends with an error, and later calls fail (the connector refuses to
    // reconnect) instead of waiting.
    let status = bounded(watch.message()).await.unwrap_err();
    assert_eq!(status.code(), Code::Unknown, "{status:?}");
    for _ in 0..2 {
        let status = bounded(check(&mut health, "")).await.unwrap_err();
        assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    }
}

#[tokio::test]
async fn shutdown_signal_drains_and_closes_the_connection() {
    let (core_end, plugin_end) = tokio_pair();
    let (_reporter, stop, mut server) = spawn_plugin(plugin_end, LIMIT);
    let mut health = health_client(bounded(connect(core_end)).await.expect("connect"));
    let mut watch = bounded(health.watch(HealthCheckRequest::default()))
        .await
        .unwrap()
        .into_inner();
    bounded(watch.message()).await.unwrap().unwrap();

    // Graceful shutdown waits for open streams; new calls are refused meanwhile.
    stop.send(()).expect("server is running");
    let draining = tokio::time::timeout(Duration::from_millis(200), &mut server).await;
    assert!(draining.is_err(), "server must wait for the open stream");
    let status = bounded(check(&mut health, "")).await.unwrap_err();
    assert_eq!(status.code(), Code::Cancelled, "{status:?}");

    // Once the last stream ends, the connection closes and the server returns.
    drop(watch);
    bounded(server)
        .await
        .expect("server task")
        .expect("graceful shutdown");
    let status = bounded(check(&mut health, "")).await.unwrap_err();
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
}

#[tokio::test]
async fn one_item_incoming_with_a_shutdown_signal_closes_the_connection() {
    // The pitfall `single_connection` avoids: with a shutdown signal, the end of the incoming
    // stream starts a graceful shutdown, so the connection closes as soon as it's idle.
    let (core_end, plugin_end) = tokio_pair();
    let (_reporter, router) = plugin_router(LIMIT);
    let (_stop, stopped) = oneshot::channel::<()>();
    let incoming = stream::iter([Ok::<_, io::Error>(plugin_end)]);
    let server = tokio::spawn(router.serve_with_incoming_shutdown(incoming, async {
        let _ = stopped.await;
    }));
    let mut health = health_client(bounded(connect(core_end)).await.expect("connect"));

    bounded(server)
        .await
        .expect("server task")
        .expect("server returns although no shutdown was signalled");
    let status = bounded(check(&mut health, "")).await.unwrap_err();
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
}
