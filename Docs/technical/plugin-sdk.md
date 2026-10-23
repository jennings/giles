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
