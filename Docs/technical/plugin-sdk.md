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
connection. The comparison doesn't exit early on the first differing byte.

## State in the data directory

The SDK keeps its own state in `giles-sdk/` inside `GILES_PLUGIN_DATA`; plugin authors use the
rest of the directory.

```text
giles-sdk/
  blobs/<sha256>       blobs the plugin put, until the core releases them
  blobs/<sha256>.json  their media type and file name
  outbound/<sha256>    blobs the core put, until the Send that uses them ends
  tmp/                 partial writes
```

At startup the SDK empties `outbound/` and `tmp/`: a blob the core put is valid only until the RPC
that uses it ends, which can't outlive the process, and a file left in `tmp/` is an interrupted
write. Every file reaches its final name by a rename from `tmp/`, so no reader ever sees a partial
one.

## Base service

`PluginService` delegates `Describe` and `Configure` to the kind. `Configure` parses `config_json`
first and answers `INVALID_ARGUMENT` if it isn't JSON. `Shutdown` answers at once and signals the
server to shut down.

### Blobs

Blob handles are the SHA-256 of the content, as 64 lowercase hex digits, the same form as the
core's blob ids (PLAN section 8.3). That's this SDK's choice of plugin-local handle, not a protocol
guarantee: the protocol allows any handle, and an id a plugin sends is only its claim about the
content. Whether the core trusts it or hashes what `GetBlob` returns is the `RemoteChannel`
adapter's decision (2.18).

- Inbound: the plugin calls `Blobs::put` (or `put_reader`) and puts the returned `MediaRef` in
  `InboundMessage::attachments`. The blob and its `.json` sidecar (media type and name) are synced
  to disk before `put` returns, so a persisted event never refers to a blob a crash could lose. The
  sidecar is written first and deleted last. Putting the same bytes again keeps the one file, and
  the last media type and name win.
- `GetBlob` serves only those blobs: the header (`BlobRef` with the file's actual size), then the
  data in chunks of at most 1 MiB. An unknown id is `NOT_FOUND`; a malformed one is
  `INVALID_ARGUMENT`. A blob whose sidecar is gone (a release racing a new put of the same bytes)
  is served as `application/octet-stream`.
- `ReleaseBlob` deletes the named blobs, in both directions. Unknown ids are fine (releasing is
  idempotent); a malformed id fails the whole call before anything is deleted. Nothing is freed
  implicitly. Inbound blobs aren't counted: if two pending events carry the same bytes they share
  one file, so releasing it for the first event removes it for the second too, and a later
  `GetBlob` for it is `NOT_FOUND`. How the core handles that is up to the `RemoteChannel` adapter.
- Outbound: `PutBlob` must send the header first, then data chunks of at most 1 MiB, exactly the
  header's `size` in total; anything else is `INVALID_ARGUMENT` and leaves nothing behind. A
  non-empty header id must equal the SHA-256 of the data. The blob lands in `outbound/` (not
  synced: it's transient) and the response carries its handle. Uploads of the same bytes share the
  file and are counted: each `PutBlob` adds a reference, and each `Send` that used the handle drops
  one when it ends, however it ends; the file goes with the last reference, so overlapping `Send`s
  with the same attachment don't break each other. The channel's `send` reads it with
  `Blobs::open`, `read`, or `path`. `ReleaseBlob` of the id drops every reference at once, and the
  next start empties `outbound/`.
