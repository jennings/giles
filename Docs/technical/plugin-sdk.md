# Plugin SDK

`giles-plugin-sdk` is the plugin side of PLAN sections 7.4 and 8: everything a gRPC plugin must do
between being launched by the core and serving its kind service. The core side (launch,
supervision, the `Remote*` adapters) lives in `giles-core`. The wire setup it builds on is in
[Plugin Transport](plugin-transport.md); the shared constants are `giles_proto::handshake` (see
[gRPC Protocols](protocols.md#launch-handshake)).

## Startup

The SDK does these steps in order, and any failure ends the process with a message on stderr:

1. Magic cookie: `GILES_PLUGIN_MAGIC` must equal `giles_proto::handshake::MAGIC_COOKIE`. Without
   it, someone ran the binary by hand: the plugin says that it's a Giles plugin, launched by Giles,
   and exits with status 1. Nothing else is touched, fd 3 included.
2. Protocol: `GILES_PLUGIN_PROTOCOLS` is a comma-separated list of majors (spaces around entries
   are allowed). The SDK picks the highest one it also speaks (now only 1). A missing or malformed
   list exits with status 1; a list with no shared major exits with status 76
   (`EXIT_NO_SHARED_PROTOCOL`), naming both sides' versions, so the core knows not to retry the
   same binary.
3. Data directory: `GILES_PLUGIN_DATA` must be an absolute path to an existing directory.
4. Fd 3: adopted as described in [Plugin Transport](plugin-transport.md). Before taking ownership,
   the SDK checks with `F_GETFD` that it's open and with `fstat` that it's a socket, so a missing
   fd 3 (whose number the runtime may already have reused for its own kqueue or epoll descriptor)
   is reported instead of being closed. It then sets `FD_CLOEXEC` and non-blocking mode, and checks
   `peer_addr()` to refuse a socket that isn't a Unix socket.
5. Token: the first line of stdin, ended by `\n` (or `\r\n`), 1 to 1024 visible ASCII characters,
   so `Bearer <token>` is a valid header value. Stdin ending first is an error.

A dedicated thread (`giles-plugin-stdin`) reads the token and then keeps reading stdin until
end-of-file. It's a plain thread, not tokio's blocking pool: a blocking read can't be cancelled, and
a pending blocking task would hold up the runtime's shutdown. End-of-file means the core is gone,
and it's the only time the plugin exits on its own (PLAN section 7.4).

Once the server runs, the SDK prints `1|<protocol>|fd|3|grpc` on stdout and flushes it. After that
the SDK never writes to stdout; the core drains and ignores it.

## Authentication

A `tonic` interceptor layered over the whole server, health included, requires the metadata
`authorization: Bearer <token>`; anything else gets `UNAUTHENTICATED` without affecting the
connection. The comparison doesn't exit early on the first differing byte. A remote plugin checks
the token its `RemoteAuth` names instead ([Remote Plugins](#remote-plugins)).

## State in the data directory

The SDK keeps its own state in `giles-sdk/` inside `GILES_PLUGIN_DATA`; plugin authors use the
rest of the directory.

```text
giles-sdk/
  blobs/<sha256>       blobs the plugin put, until no unacked event refers to them
  blobs/<sha256>.json  their media type and file name
  outbound/<sha256>    blobs the core put, until the Send that uses them ends
  events/<seq>.pb      events the core hasn't acked yet
  tmp/                 partial writes
```

At startup the SDK empties `outbound/` and `tmp/`: a blob the core put is valid only until the RPC
that uses it ends, which can't outlive the process, and a file left in `tmp/` is an interrupted
write. It also deletes the inbound blobs in `blobs/` that no event in `events/` refers to (see
[Blobs](#blobs)). Every file reaches its final name by a rename from `tmp/`, so no reader ever sees
a partial one.

## Base service

`PluginService` delegates `Describe` and `Configure` to the kind (see [Channels](#channels)).
`Configure` parses `config_json` first and answers `INVALID_ARGUMENT` if it isn't JSON. `Shutdown`
answers at once and then starts the bounded shutdown described under [Lifecycle](#lifecycle); in a
remote plugin it only ends the core's open `Events` stream ([Remote Plugins](#remote-plugins)).

### Blobs

Blob handles are the SHA-256 of the content, as 64 lowercase hex digits, the same form as the
core's blob ids (PLAN section 8.3). That's this SDK's choice of plugin-local handle, not a protocol
guarantee: the protocol allows any handle, and an id a plugin sends is only its claim about the
content. The core's `RemoteChannel` never trusts it: it always fetches with `GetBlob` and hashes
what arrives ([Plugin Host](plugin-host.md#inbound-attachments)).

- Inbound: the plugin calls `Blobs::put` (or `put_reader`) and puts the returned `MediaRef` in
  `InboundMessage::attachments`. The blob and its `.json` sidecar (media type and name) are synced
  to disk before `put` returns, so a persisted event never refers to a blob a crash could lose. The
  sidecar is written first and deleted last. Putting the same bytes again keeps the one file, and
  the last media type and name win.
- `GetBlob` serves only those blobs: the header (`BlobRef` with the file's actual size), then the
  data in chunks of at most 1 MiB. An unknown id is `NOT_FOUND`; a malformed one is
  `INVALID_ARGUMENT`. A blob whose sidecar is missing is served as `application/octet-stream`.
- Inbound blobs live as long as something refers to them. Identical bytes share one file, so the
  SDK counts, in memory, the attachments of spooled (unacked) events and the `put`s whose event
  hasn't reached the spool yet (counted before the files are written, so an ack of another event
  with the same bytes can't delete a blob a new event is about to use). Acking an event deletes
  each of its blobs that nothing else refers to; that is what frees inbound blobs. An event that
  never reaches the spool (a repeat of one already pending, or one the protocol can't carry) gives
  its `put`s up the same way. A crash between deleting the acked event's file and deleting its
  blobs, or a blob put and never sent, leaves files no spooled event refers to, and startup deletes
  them: the core acks only after storing an event and its attachments, so an unspooled blob is
  never needed again.
- `ReleaseBlob` deletes the named inbound blobs that nothing refers to and leaves the rest:
  releasing one an unacked event still carries (a second event with the same bytes, or the event
  whose ack hasn't arrived yet) does nothing, and the ack deletes it later. It never touches
  `outbound/`. Unknown ids are fine (releasing is idempotent); a malformed id fails the whole call
  before anything is deleted. The core's `RemoteChannel` releases an event's handles right after
  acking it (PLAN section 8.3), so the release usually finds them gone already.
- Outbound: `PutBlob` must send the header first, then data chunks of at most 1 MiB, exactly the
  header's `size` in total; anything else is `INVALID_ARGUMENT` and leaves nothing behind. A
  non-empty header id must equal the SHA-256 of the data. The blob lands in `outbound/` (not
  synced: it's transient) and the response carries its handle. Uploads of the same bytes share the
  file and are counted: each `PutBlob` adds a reference, and each `Send` that used the handle drops
  one when it ends, however it ends; the file goes with the last reference, so overlapping `Send`s
  with the same attachment don't break each other. An inbound blob with the same bytes is a
  separate file with its own lifetime. The channel's `send` reads it with `Blobs::open`, `read`,
  or `path`, and the next start empties `outbound/`.

## Public API

```rust
#[tokio::main]
async fn main() -> ExitCode {
    ChannelPlugin::new("irc", env!("CARGO_PKG_VERSION"))
        .output(OutputMode::Blocks)
        .can_initiate(true)
        .requested_grants(Grants::default())
        .serve(|ctx: Configure| async move {
            let config: IrcConfig = ctx.parse_config()?;
            IrcChannel::connect(config, ctx.data_dir(), ctx.blobs().clone()).await
        })
        .await
}
```

- `ChannelPlugin::new(name, version)` with builder methods `output`, `can_initiate`, and
  `requested_grants` (a `Grants { network_all, network_ports, read, write }`, mirroring
  `SandboxGrants`). These answer `Describe` before the channel exists.
- `serve(configure)` takes `Fn(Configure) -> impl Future<Output = Result<impl Channel>>` and
  returns the process's `ExitCode`. It uses the ambient tokio runtime; `#[tokio::main]` is enough.
  Fd 3 is adopted after the runtime started, which is why step 4 above checks it's a socket before
  taking ownership. `serve_remote(RemoteServer, configure)` runs the same channel as a remote
  plugin ([Remote Plugins](#remote-plugins)); `RemoteServer` and `RemoteAuth` are re-exported at
  the crate root.
- `Configure` gives `instance()`, `config()` (the JSON `Value`), `parse_config::<T>()` (serde,
  errors as `Error::InvalidRequest("invalid config: ...")` naming the key), `data_dir()`, and
  `blobs()`.
- `Blobs`: `put`, `put_reader`, `open`, `read`, `path`.
- `convert`: the `giles-types` and wire conversions in both directions, plus the `Error` and
  `Status` mapping (below), public so the core's adapters and tests can share them.
- `config`: `parse_duration` and `parse_size`, and the serde field types `HumanDuration` and
  `ByteSize` built on them, so a plugin's `config` table writes durations (`"10m"`) and sizes
  (`"50 MiB"`) like `giles.toml` does. The core's own config uses the same functions
  ([Configuration](config.md#values)).
- Re-exports `giles_types` and `async_trait`, so a plugin uses the same versions as the SDK.

## Lifecycle

`serve` runs the server on its own task, prints the handshake line, and then waits for one of:

- stdin end-of-file: the core is gone;
- `Shutdown`: the core asks the plugin to stop (PLAN section 7.5's drain hint).

Either way it cancels the shutdown token (ending every `Events` stream and the event pump), marks
the server `NOT_SERVING` in health, and starts `tonic`'s graceful shutdown. Calls in flight get
`DRAIN_TIMEOUT` (5 s) to finish; open streams that the core doesn't end (a health `Watch`) are cut
after that, and `serve` returns. The SDK never exits for any other reason, and `serve` installs no
signal handlers: the core's SIGTERM and SIGKILL keep their default effect, which is safe because
unacked events are already on disk. (`serve_remote` stops on `SIGINT` or `SIGTERM` by default,
with the same drain.)

Health (`grpc.health.v1`): the overall service `""` and `giles.plugin.v1.PluginService` are
`SERVING` from the start; `giles.channel.v1.ChannelService` is `NOT_SERVING` until `Configure`
succeeds. At shutdown `""` becomes `NOT_SERVING`.

## Channels

### Configure

`Configure` calls the plugin's `configure` closure with the parsed config, under a lock so calls
don't overlap. On success the SDK checks the channel against what `Describe` promised: its
`Channel::info()` must have the same `output` and `can_initiate`, and its `identity_namespace` must
follow the core's rule (`giles_core::identity::validate_namespace`): the plugin name itself or
`<name>:...` (so plugin `echo` may report `echo` or `echo:test`, but not `echoer`), with every
`:`-separated part non-empty and free of whitespace and control characters. A mismatch is a plugin
bug and fails with `INTERNAL`, naming the namespace. It returns the namespace and marks
`ChannelService` serving. A failed `Configure` can be retried. After a successful one, the same
`Configure` again (the same instance name and config: a remote plugin's core reconnecting) gets the
same namespace without calling `configure`, and any other is refused with `FAILED_PRECONDITION`;
`Events` and `Send` before the first are refused the same way.

`Configure::parse_config` deserializes through `serde_path_to_error`, so every error names the key
path, e.g. `invalid config: tls.verify: invalid type: string "yes", expected a boolean`.

### Events and persistence

Delivery is at least once (PLAN section 8.5), and the SDK makes it durable for every channel:

1. The first `Events` call starts the pump: it calls `Channel::events()` once and moves each event
   into the spool. An event the wire can't carry (a newer `giles-types` variant) is logged and
   dropped. A stream error item is logged, and the pump waits before polling again (100 ms,
   doubling to 10 s while errors continue, reset by the next event), so a broken source doesn't
   spin. If the channel's stream ends, the pump stops and the next `Events` call calls
   `Channel::events()` again.
2. The spool writes each event, encoded as the wire `ChannelEvent`, to `events/<seq>.pb` (via
   `tmp/`, synced, renamed, directory synced) before offering it to any stream. The pump persists
   an event before it polls the channel's stream for the next one, so a channel whose stream is
   polled again knows the previous event is on disk: a push source can drop its own copy then.
   Persisting retries with backoff (100 ms to 10 s) rather than losing the event.
3. An event whose id is already pending is not stored again (a channel resending after its stream
   restarted). Once acked, the same id would be stored and sent again; the core deduplicates.
4. Each `Events` stream first sends every pending event in order, including those left by earlier
   streams and earlier processes, then new ones as they're persisted. A newer `Events` call ends
   the older stream, so there's one at a time.
5. An ack deletes the event's file and then calls `Channel::ack`. Acks for unknown ids are
   ignored. `Channel::ack` may therefore name an event emitted by an earlier run of the plugin.

At startup the spool loads `events/`. A file that doesn't decode is renamed to `<name>.corrupt` and
logged rather than deleted; a second file with an id already loaded is deleted; numbering continues
after the highest file seen.

### Send

`Send` converts the request (`INVALID_ARGUMENT` for a missing target or a bad id), checks that every
attachment handle exists (`INVALID_ARGUMENT` otherwise), refuses a `recipient` if the plugin didn't
declare `can_initiate`, and calls `Channel::send`. The returned message id goes back in
`SendResponse`. When the RPC ends, however it ends, it releases its reference to each distinct
blob the core put for it (see [Blobs](#blobs)).

### Conversions and errors

`convert` moves fields between `giles-types` and `giles.channel.v1` following the tables in
[gRPC Protocols](protocols.md#mapping-to-giles-types). Wire to domain validates ids, requires
"Always set" fields, and refuses unspecified enum values and unknown `oneof` variants. Timestamps
keep nanoseconds in `0..1e9`, so a time before 1970 has negative seconds and positive nanoseconds.

The blob conversions assume this SDK's handles: a wire `BlobRef` id must parse as a SHA-256
`MediaId`. The protocol allows any plugin-local handle (see [gRPC Protocols](protocols.md)), so
the helpers refuse plugins with other handles, and a parsed id is only the plugin's claim about the
content. The core's `RemoteChannel` uses these helpers but takes a message's attachments out
before converting it, so it accepts any handle, and it never trusts one: it fetches the bytes with
`GetBlob` and hashes them.

| `giles_types::Error`          | gRPC status        | Back to `Error` (`error_from_status`) |
| ----------------------------- | ------------------ | ------------------------------------- |
| `InvalidId`, `InvalidRequest` | `INVALID_ARGUMENT` | `InvalidRequest`                      |
| `NotFound`                    | `NOT_FOUND`        | `NotFound`                            |
| `Unavailable`                 | `UNAVAILABLE`      | `Unavailable` (no `retry_after`)      |
| anything else                 | `UNKNOWN`          | `Other`, carrying the status          |

`Unavailable`'s `retry_after` doesn't cross the wire in v1; the outbox retries with its own backoff.

## Remote Plugins

A plugin someone runs themselves, which the core connects to over TCP (PLAN section 7.6), calls
`ChannelPlugin::serve_remote(server, configure)` instead of `serve`. The services, `Configure`,
persistence, and blobs are the same; what differs:

- `RemoteServer::new(listener, data_dir, auth)` takes a bound `tokio::net::TcpListener`, the data
  directory (created with mode 0700 if missing; `giles-sdk/` inside it as above), and a
  `RemoteAuth`: `Bearer(token)` (exact match, as for a launched plugin; an empty token refuses
  every call, with a warning, so an unset setting can't open the plugin), `Verify(fn)` (the token
  after `Bearer `, never empty, for tokens an OAuth server issued to the core), or `None`. There
  are no startup checks, no fd 3, no stdin, and no handshake line.
- `.tls(rustls::ServerConfig)` serves TLS (`tls` feature), setting the `h2` ALPN protocol. Each
  connection's handshake runs on its own task with a 10 s limit, so a slow or failing client
  doesn't hold up the accept loop; failures are logged at info level with the peer address, as is
  every accepted connection. Without it, the server speaks HTTP/2 over plain TCP.
- `.shutdown_on(future)` replaces the default stop signal, `SIGINT` or `SIGTERM`. Then the server
  drains as under [Lifecycle](#lifecycle) and `serve_remote` returns `Ok(())`; it returns an
  `io::Error` if the data directory or the SDK's state can't be opened or the server fails.
- The core may connect, disconnect, and connect again (its restarts, network failures). Every
  connection calls `Describe` and `Configure`; the channel built by the first successful
  `Configure` stays (a repeated identical `Configure` gets the same answer, a different config is
  refused until the plugin restarts), and the pump keeps spooling events between connections, so
  the next `Events` stream starts with everything the last one left unacked.
- `Shutdown` (`base::OnShutdown::Detach`) ends the current `Events` stream and nothing else: the
  core is disconnecting, and the plugin keeps serving. The health status stays `SERVING`.

## TLS

The SDK makes no TLS connections of its own: the core reaches a launched plugin over a private
socket pair. Channel plugins make their own (IRC, IMAP, SMTP, WhatsApp), so the default `tls`
feature exposes `giles_plugin_sdk::tls`: `root_store()` (the Mozilla roots compiled in from
`webpki-root-certs`), `client_config()` (a `rustls::ClientConfig` with those roots, the safe
default protocol versions, and the `aws-lc-rs` provider named explicitly), and
`client_config_with_roots(roots)` (the same with other roots), and re-exports `rustls`. This is the
same trust store as the core's built-in providers, never the OS store, which a sandboxed plugin may
not be able to read. `server_config(cert_chain_pem, key_pem)` is a remote plugin's server config
(the chain with the server's certificate first; a PKCS #8, PKCS #1, or SEC1 key that must match
it; no client certificates). A plugin that needs no TLS can turn the feature off.

## The echo plugin

`plugins/giles-channel-echo` (`publish = false`) is a channel plugin for tests of the plugin host,
built on the SDK. It talks to nobody; tests drive it through files in its data directory:

| Path            | Meaning                                                                     |
| --------------- | --------------------------------------------------------------------------- |
| `inject/*.json` | Inbound events to emit, in file-name order (write elsewhere, rename in)     |
| `sent.jsonl`    | One line per `Send`: `message_id` (`echo-<n>`), `message`, `attachment_texts` |
| `acks.jsonl`    | One line per event the core acked: `event_id`                               |

An injection is `{"event": <ChannelEvent>, "attachments": [{"text", "media_type", "name"}]}`, the
event in `giles-types` JSON and `attachments` optional (message events only; each is stored with
`Blobs::put` and appended to the message's attachments, so `GetBlob` and `ReleaseBlob` can be
exercised). The plugin deletes an injection when the SDK polls its stream again, that is once the
event is persisted, so after that only the SDK's spool has it: killing the plugin then shows that
the SDK resends it. A file that doesn't parse is renamed to `*.bad`.

Config, all optional, unknown keys refused: `namespace` (default `"echo"`); `echo` (default
`false`; when set, every sent message comes back as an inbound message from `echo_sender`, default
`"echo-user"`, in the same conversation, with event and message id `echo-<n>` and the attachments
stored again as the plugin's own blobs); `unavailable` (recipients whose sends fail with
`UNAVAILABLE`). It reports `output = blocks` and `can_initiate = true`.

With arguments it runs as a remote plugin, the example of `serve_remote`:
`giles-channel-echo --listen <address> --data <dir> [--tls-cert <pem> --tls-key <pem>]` accepts
`Bearer $GILES_ECHO_TOKEN` (anything if that isn't set), prints `listening on <address>` on stdout
once bound (for port 0), and runs until `SIGINT` or `SIGTERM`, with `<dir>` as its data directory.

Its integration tests (`plugins/giles-channel-echo/tests/plugin.rs`) launch the binary exactly as
the core will: a `socketpair`, the plugin's end made fd 3 with `dup2` in `pre_exec`, the token on
stdin, the scrubbed environment, then the handshake line, then every RPC through the generated
clients with a bearer interceptor and 16 MiB limits.

### Using it from other tests

Cargo gives integration tests the path of a binary (`env!("CARGO_BIN_EXE_giles-channel-echo")`)
only within the binary's own package, and builds the binary before running them. So tests that
launch the echo plugin through the real host (plugin launch, supervision, `RemoteChannel`) belong in
`plugins/giles-channel-echo/tests/`, with `giles-core` as a dev-dependency of the echo crate; that
works for `cargo test --workspace`, `cargo test -p giles-channel-echo`, and any target or build
directory. A discovery test can symlink that path into `<root>/plugins/`.

From another package there's no reliable path: test executables live in the build directory
(`CARGO_BUILD_BUILD_DIR`), binaries in the target directory, and `cargo test -p giles-core` doesn't
build the echo binary at all.
