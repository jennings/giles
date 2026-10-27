# Plugin Transport

A gRPC plugin and the core talk over the two ends of one Unix socket pair (PLAN section 7.4). The
plugin is the gRPC server on its end, inherited as fd 3; the core is the client on the other. No
socket is ever bound or listened on, and there is exactly one HTTP/2 connection per plugin launch.

This page records how that works with `tonic`, as established by the transport spike. The regression
test `crates/giles-core/tests/grpc_socketpair.rs` pins every behavior described here; if an upgrade
changes one, that test fails and this page needs updating.

Versions verified: `tonic` 0.14.6 (with `tonic-health` and `tonic-prost` 0.14.6), `hyper` 1,
`hyper-util` 0.1.21, `tokio` 1.54, on macOS arm64 and Linux arm64 (kernel 6.12).

## Plugin side: serving on fd 3

```rust
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;

const PLUGIN_FD: RawFd = 3;

// SAFETY: `F_GETFD` only reads the descriptor flags; it fails with `EBADF` if fd 3 isn't open.
let flags = unsafe { libc::fcntl(PLUGIN_FD, libc::F_GETFD) };
if flags == -1 {
    return Err(io::Error::last_os_error()); // not launched by Giles: fd 3 isn't open
}
// SAFETY: fd 3 is open (checked above) and is the socket the core passed us; nothing else in the
// process uses or closes it, so the `OwnedFd` becomes its only owner.
let fd = unsafe { OwnedFd::from_raw_fd(PLUGIN_FD) };
// SAFETY: `F_SETFD` on a descriptor we own; it only changes the descriptor flags.
if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
    return Err(io::Error::last_os_error());
}
let std_stream = StdUnixStream::from(fd);
std_stream.set_nonblocking(true)?; // inherited fds are blocking; tokio needs non-blocking
let io = tokio::net::UnixStream::from_std(std_stream)?; // inside the runtime

let incoming = futures::stream::iter([Ok::<_, std::io::Error>(io)])
    .chain(futures::stream::pending());
Server::builder()
    .layer(tonic::service::InterceptorLayer::new(RequireBearer(expected)))
    .add_service(PluginServer::new(plugin).max_decoding_message_size(LIMIT)
        .max_encoding_message_size(LIMIT))
    .add_service(health_server.max_decoding_message_size(LIMIT)
        .max_encoding_message_size(LIMIT))
    .add_service(kind_server /* with the same two limits */)
    .serve_with_incoming_shutdown(incoming, shutdown_signal)
    .await?;
```

- `tonic` implements its `Connected` trait for `tokio::net::UnixStream`, so the stream goes to
  `serve_with_incoming*` as is; no wrapper type is needed. The connect info is `UdsConnectInfo`
  (peer address and credentials).
- Adopting fd 3 is the only `unsafe` part. Do it first thing in `main`, before any other code could
  open or close descriptors:
  - `F_GETFD` before `from_raw_fd` proves fd 3 is open, which `from_raw_fd` requires.
  - `FD_CLOEXEC` right after: fd 3 had to be inheritable to survive the exec, and neither `OwnedFd`,
    `std`, nor `tokio` sets the flag. Without it, any child the plugin spawns inherits the gRPC
    socket, keeping the connection open after the plugin dies (so the core doesn't notice) and
    handing the child a raw handle to the plugin's server.
  - Nothing checks that the fd is a socket; a cheap check is `peer_addr()`, which fails with
    `ENOTSOCK` otherwise.
- `UnixStream::from_std` requires the socket to be non-blocking already and must run inside the
  runtime.

### The incoming stream must stay open

The obvious one-item stream, `stream::iter([Ok(io)])`, works but behaves badly in both serve
variants, because `tonic` serves each accepted connection on its own detached task (`tokio::spawn`)
and treats the end of the incoming stream as the end of the server:

| Server call                                        | What happens                                   |
| -------------------------------------------------- | ---------------------------------------------- |
| `serve_with_incoming(once)`                        | Returns `Ok` right after accepting; the connection keeps serving on its detached task. The returned future says nothing about the connection, and dropping it stops nothing. |
| `serve_with_incoming_shutdown(once, signal)`       | The end of the stream starts a graceful shutdown at once: the connection closes as soon as it's idle, and the core's next call fails with `Unavailable`. |
| `serve_with_incoming_shutdown(once + pending, signal)` | Serves until `signal`, then shuts down gracefully and returns once the connection is closed. Use this. |

Graceful shutdown sends HTTP/2 `GOAWAY`: calls started afterwards fail with `Cancelled`, calls in
flight finish, and the server waits for every open stream to end. A long-lived stream (a health
`Watch`, a channel's event stream) therefore holds the drain open until the client drops it, so the
SDK has to bound the drain itself.

## Core side: connecting with a one-shot connector

```rust
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
                io::Error::new(io::ErrorKind::NotConnected, "plugin connection already used")
            })
        }
    })
}

let channel = Endpoint::from_static("http://plugin") // never resolved; only sets `:authority`
    .connect_with_connector(one_shot_connector(core_end))
    .await?;
let mut plugin = PluginClient::with_interceptor(channel.clone(), AddBearer(token.clone()))
    .max_decoding_message_size(LIMIT)
    .max_encoding_message_size(LIMIT);
let mut health = HealthClient::with_interceptor(channel, AddBearer(token))
    .max_decoding_message_size(LIMIT)
    .max_encoding_message_size(LIMIT);
```

- The connector's response must implement `hyper`'s I/O traits, hence `TokioIo`; it needs
  `hyper-util` with the `tokio` feature and `tower` with `util`.
- `connect_with_connector` returns once it has written the HTTP/2 client preface; it doesn't wait
  for the server's `SETTINGS`. So it fails if the plugin's end is already closed, but succeeding
  doesn't mean the plugin is serving. Readiness is the handshake line on stdout, then `Describe`
  (section 7.4).
- A `Channel` is a cheap clone; every client made from it shares the one connection. Calls, server
  streams, and large messages all run concurrently over it.

### No reconnect

`tonic`'s channel reconnects on demand: after the connection drops, the next call asks the connector
for a new one. The one-shot connector refuses, so the call fails at once with `Unavailable` and the
connector's message, and so does every later call. Nothing retries or waits, which is the behavior
section 7.4 wants (the core restarts the plugin instead).

When the plugin dies, its end of the pair closes with it. Streams open at that moment end with
`Unknown` ("h2 protocol error: error reading a body from connection"); new calls get `Unavailable`
as above.

## Bearer token

The token check is a `tonic` interceptor, applied to the whole server with `layer` so it covers
every service, health included:

```rust
#[derive(Clone)]
struct RequireBearer(MetadataValue<Ascii>); // "Bearer <token>"

impl Interceptor for RequireBearer {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        match request.metadata().get("authorization") {
            Some(value) if *value == self.0 => Ok(request),
            _ => Err(Status::unauthenticated("missing or invalid plugin token")),
        }
    }
}
```

The client side is the mirror image: `AddBearer` inserts the same value with
`request.metadata_mut().insert("authorization", ...)`, attached to each generated client with
`with_interceptor`. A missing or wrong token gets `Unauthenticated`, and the rejection doesn't
affect the connection or other calls. (`MetadataValue`'s `==` isn't constant-time; the token is a
second layer on a private socket, but the SDK may as well compare in constant time.)

## Message size limits

`tonic` defaults to a 4 MiB limit on received messages and no limit on sent ones. There is no
server-wide or channel-wide setting: each generated server and client (and a hand-written
`tonic::server::Grpc` or `tonic::client::Grpc`) takes `max_decoding_message_size` and
`max_encoding_message_size`, so the SDK and the `Remote*` adapters set both on every service and
client. The limit counts the encoded protobuf message, excluding gRPC's 5-byte frame header: a
message of exactly 16 MiB passes in both directions, and one byte more is refused.

| Oversized message | Refused by     | The caller sees                                                    |
| ----------------- | -------------- | ------------------------------------------------------------------ |
| Request           | Client encoder | `Internal`, "h2 protocol error: http2 error" (see below)            |
| Request           | Server decoder | `OutOfRange`, "Error, decoded message length too large: found N bytes, the limit is: M bytes" |
| Response          | Server encoder | `OutOfRange`, "Error, encoded message length too large: ..."      |
| Response          | Client decoder | `OutOfRange`, "Error, decoded message length too large: ..."      |

None of them breaks the connection. The first row is a `tonic` 0.14 wart: the encoder's `OutOfRange`
is lost because the failed request body makes `hyper` reset the stream, and the client reports only
the reset. A caller that wants a clear error (the `Remote*` adapter with an oversized
`CompleteRequest`, say) has to compare `prost::Message::encoded_len` with the limit before sending.
