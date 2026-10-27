# Plugin host

Code: `crates/giles-core/src/plugin_host/` (`giles_core::plugin_host`). Design: PLAN sections 4.3,
7.3, 7.4, 7.5, 7.6, 7.7, 7.8, 7.9, 8.3, 8.5, 11.6, 12.1, 12.4, and 13.4. The plugin side of the same
steps is the [Plugin SDK](plugin-sdk.md); the wire setup is [Plugin transport](plugin-transport.md);
the trampoline is the [Plugin sandbox](sandbox.md).

The plugin host launches gRPC channel plugins (provider plugins aren't supported in v1). It takes a
configured `[channel.<instance>]` without `address` from its binary to a configured plugin, wraps
the result in a `RemoteChannel`, registers that with the registry and the session manager, and
supervises it: health checks, relaunches with backoff, and the stop sequence. A
`[channel.<instance>]` with `address` is a remote plugin that someone else runs: the host connects
to it instead ([Remote plugins](#remote-plugins)), and supervises the connection the same way.

## Launch

`Launcher::launch_channel(&ChannelInstance)` runs these steps under a `plugin{instance=<name>}`
span. Any failure after the spawn kills the plugin's whole process group before the error returns.

1. Sandboxed instances are refused at once if the startup host check failed (`Launcher::new` takes
   its result): one clear error instead of each launch failing in the trampoline.
2. Discovery: `config::discover_plugin` (`<root>/plugins/`, then `PATH`), the same lookup the config
   loader used, so the binary checked is the binary run. The path is then resolved (`canonicalize`):
   the file granted in the spec, recorded in the pid file, and run is one file.
3. Orphan cleanup (below).
4. The data directory `data/plugins/<instance>/` and its `tmp/` are created with mode 0700. A data
   directory that is a symbolic link is refused, as the trampoline does.
5. The sandbox spec (below), validated.
6. A per-launch token: 32 bytes from the OS (`getrandom`), as 64 hex digits.
7. The spawn (below). The token is written to stdin as one line, and stdin stays open.
8. The handshake line: one line of at most 256 bytes from stdout within 10 s, parsed with
   `giles_proto::handshake::HandshakeLine::parse`; the protocol it names must be one the core
   offered. After the line, a task drains stdout and ignores it.
9. The gRPC connection over the core's end of the socket pair (below).
10. `Describe` (10 s): `core_protocol_minor` 0 and no `core_features` (v1 has no optional features
    yet; they announce additions within `v1`). The kind must be `PLUGIN_KIND_CHANNEL`, the name must
    be the binary's `<name>`, and the channel capabilities must be present with an output mode the
    core knows.
11. `Configure` (60 s, since plugins connect to their service in it) with the instance name and
    `PluginConfig::to_json()`, secrets resolved.
12. The returned identity namespace must pass `identity::validate_namespace(component, namespace)`.

## Failures

`LaunchError`, with `is_retryable()` for supervision. Non-retryable means not until the binary or
the configuration changes. Every message is safe to log: plugin-provided text (status messages, the
namespace, stderr) has the instance's secret values and the token replaced by `<redacted>`
(`config::scrub`).

| Failure | Variant | Retryable |
| ------- | ------- | --------- |
| Sandbox check failed at startup, instance sandboxed | `SandboxUnavailable` | no |
| No binary in `<root>/plugins/` or on `PATH` | `NotFound` | no |
| Data directory, spec file, pid file, token | `Prepare`, `Paths`, `Spec` | `Prepare` yes |
| The process can't be started | `Spawn` | yes |
| Exit status 125 from the trampoline, or 71 from macOS `sandbox-exec` (sandboxed only) | `Sandbox` | no |
| Exit status 76 before the line | `NoSharedProtocol` | no |
| Any other exit (or stdout closed) before the line | `Exited` | yes |
| No line within 10 s | `HandshakeTimeout` | yes |
| A malformed line | `Handshake` | yes |
| A protocol the core didn't offer | `Protocol` | no |
| The connection can't be set up (for a remote plugin also TCP, TLS, and 10 s for both) | `Connect` | yes |
| A remote plugin's OAuth token request fails | `Auth` | yes |
| `Describe` fails or times out | `Rpc` | yes |
| `Configure` fails | `Rpc` | no for `INVALID_ARGUMENT`, `FAILED_PRECONDITION`, `INTERNAL`, `UNIMPLEMENTED`; else yes |
| Wrong kind or name, no channel capabilities, unknown output mode | `Mismatch` | no |
| A namespace that isn't the plugin's own | `Namespace` | no |
| `connect_remote` on an instance without `address` (or `launch_channel` on one with it) | `NotRemote` (`NotLaunched`) | no |

Supervision retries every failure of a remote plugin anyway ([Remote plugins](#remote-plugins)).

The failures before the handshake completes carry the plugin's last stderr lines (`stderr()`), and
their message ends with the last non-empty one, so a trampoline failure reads like
`the sandbox trampoline failed (exit status: 125): Error: ...`. Those lines were also logged as they
arrived.

## Spawning

The program is the trampoline,
`<giles exe> sandbox-exec run/sandbox/<instance>.json -- <binary> socket`. The trampoline inherits
the core's environment and builds the plugin's scrubbed one itself. With `unsandboxed = true` the
host skips the trampoline, logs `running the plugin unsandboxed` at warn level on every launch, and
runs `<binary> socket` itself with exactly the environment the trampoline would give it
(`sandbox::scrubbed_env`) and the data directory as working directory.

`socket` (`giles_proto::handshake::SOCKET_COMMAND`) is the plugin's only argument. It tells a plugin
built on `ChannelPlugin::run` that the core launched it and that fd 3 is its socket; the same binary
run with `serve` is a [remote plugin](#remote-plugins). A plugin built before `run` ignores the
argument ([Plugin SDK](plugin-sdk.md#version-skew)).

In the child, between `fork` and `exec` (`pre_exec`, async-signal-safe calls only):

- `setsid()`: the plugin leads a new session and process group, so the host can signal the whole
  group and a terminal's Ctrl-C doesn't reach plugins directly.
- The plugin's end of the socket pair becomes fd 3 with `dup2` (or, if it already is fd 3, its
  close-on-exec flag is cleared). Everything else the core opens is close-on-exec, and the
  trampoline passes fds 0 to 3 through `exec` (including macOS `sandbox-exec`).

The pid survives both `exec`s, so the spawned process is the plugin.

## The sandbox spec

Written with `Spec::to_json` to `run/sandbox/<instance>.json`, mode 0600 (via a temporary file and a
rename), outside every plugin's grants. From PLAN section 12.4:

| Field | Value |
| ----- | ----- |
| `data_dir` | `data/plugins/<instance>/` |
| `read` | the resolved plugin binary, then the `sandbox.read` grants |
| `write`, `unix`, `min_abi` | the grants as configured |
| `network` | the grant; the config loader already applied the channel default (on) |
| `execute` | none: the backends always allow the command itself |
| `unsandboxed` | the instance's `unsandboxed` |
| `env` | `GILES_PLUGIN_MAGIC` (the cookie) and `GILES_PLUGIN_PROTOCOLS` (`1`) |

The binary's directory isn't granted: a binary installed in, say, a home directory would expose the
whole directory. The trampoline adds `GILES_PLUGIN_DATA`, `HOME`, `TMPDIR`, `PATH`, and `LANG`.

## The process

`PluginProcess` owns the child, its stdin, its pid file, and the stderr task:

- stderr is read line by line (lines cut at 4 KiB), scrubbed, and logged at info level with target
  `giles_core::plugin_host::stderr` under the instance's span, e.g.
  `INFO plugin{instance=irc}: giles_core::plugin_host::stderr: connected`. The last 20 lines are
  kept for error messages (`stderr_tail`).
- `wait()` and `exit_status()`: when the plugin has exited, `SIGKILL` to the rest of its process
  group, then reap it. Children can outlive the plugin, and this is the only moment the group can be
  signalled safely after the plugin is gone: the exit is observed without reaping (`waitid` with
  `WNOWAIT`), so the unreaped plugin still holds its pid, which is the group id, and no other
  process or group can have it. Once reaped, the group id could be reused, so the host never signals
  the group again. `wait` wakes on `SIGCHLD` (checking every second as a safety net) and is
  cancel-safe.
- `close_stdin()`: the plugin sees end-of-file, which a Giles plugin takes as the core being gone.
- `terminate(grace)`: `SIGTERM` to the process group, up to `grace` for the plugin to exit, then
  `kill()`.
- `kill()`: `SIGKILL` to the process group if the plugin is still unreaped, then waits for it (5 s
  at most, which kills the rest of the group as above) and removes the pid file.
- Dropping it while the plugin is unreaped sends `SIGKILL` to the group; dropping it always removes
  the pid file. A plugin seen to exit has already taken its group with it, so supervision can wait
  for a crash, drop the handle, and relaunch without leaving children behind.

A child that calls `setsid` itself leaves the group and escapes all of this; fully containing a
plugin's descendants needs more (a cgroup on Linux), as [Plugin sandbox](sandbox.md) notes.

## Pid files and orphans

`run/pids/<instance>.pid`, mode 0600, written right after the spawn: the pid and the resolved binary
path, one per line. A clean stop or a failed launch removes it.

If the core dies without stopping its plugins, each plugin sees stdin close and exits. For a plugin
that doesn't, the next launch of the instance first reads the pid file and kills the recorded
process (its group, if it leads one, else just it) only if both:

- the file recorded the binary about to be launched, and
- the process's executable is that binary: `/proc/<pid>/exe` on Linux (a binary replaced since,
  shown as `<path> (deleted)`, still counts), `proc_pidpath` on macOS.

So a pid reused by an unrelated program is never touched. The host then waits up to 5 s for the
orphan to go, so two copies never share the data directory, and removes the file either way.

## gRPC

`PluginConnection` is the one HTTP/2 connection, made with the one-shot connector from
[Plugin transport](plugin-transport.md) (no reconnect). `plugin()`, `channel()`, and `health()`
build clients with the 16 MiB limits; `transport()` is the intercepted channel for others.

`CallMetadata`, the interceptor on every client, adds `authorization: Bearer <token>` (marked
sensitive) and a W3C `traceparent`. PLAN section 13.4 asks for trace context in gRPC metadata, but
the core has no OpenTelemetry layer yet, so `tracing` spans have no trace ids to propagate. Until it
does, each call gets a fresh random trace id and parent id with the sampled flag off
(`00-<32 hex>-<16 hex>-00`), and the value is logged at debug level in the caller's span, which ties
the core's log of a call to anything the plugin logs for it. With OpenTelemetry, the interceptor
injects the current span's context instead; the SDK doesn't read the header yet.

## Handle

```rust
pub struct LaunchedPlugin {
    pub instance: InstanceName,
    pub component: String,
    pub binary: PathBuf,              // resolved
    pub sandboxed: bool,
    pub describe: DescribeResponse,   // checked
    pub identity_namespace: String,   // from Configure, checked
    pub connection: PluginConnection,
    pub process: PluginProcess,
    pub secrets: Arc<[Secret]>,       // the instance's secrets and the token, for `scrub`
    pub span: tracing::Span,          // plugin{instance=...}
}
```

`LaunchedPlugin::stop(grace)` is the stop sequence of PLAN section 7.5: `Shutdown` and closing
stdin, up to `grace` (`STOP_GRACE`, 10 s: the SDK drains for up to 5 s) for the plugin to exit, then
`terminate(TERM_GRACE)` (5 s), which also kills what's left of the group and removes the pid file.

## Remote plugins

A `[channel.<instance>]` with `address` (PLAN section 7.6) is a plugin someone runs themselves; the
config loader already checked the URL (`http` or `https`, no credentials, plain `http` only to
loopback unless `insecure = true`) and `auth` ([Configuration](config.md)). The SDK side is
`ChannelPlugin::serve_remote` ([Plugin SDK](plugin-sdk.md#remote-plugins)), which every shipped
plugin's `serve` subcommand calls ([Command line](plugin-sdk.md#command-line)).

`Launcher::connect_remote(&ChannelInstance)` (module `remote`), under a `plugin{instance=<name>}`
span:

1. Credentials. `auth = { bearer = ... }`: `authorization: Bearer <token>` (marked sensitive) on
   every call. `auth = { oauth = ... }`: a token request first (`oauth::TokenClient`, below), whose
   access token is the bearer token. No `auth`: no `authorization` header.
2. TCP to the URL's host and port (80 or 443 by default; a domain's addresses in turn), with
   `TCP_NODELAY`, then for `https` a TLS handshake with `tokio-rustls`: the SDK's
   `tls::client_config()` (the bundled Mozilla roots, never the OS store;
   `Launcher:: remote_tls_roots` replaces them, for tests) with ALPN `h2`, and the URL's host as the
   server name, a domain or an IP address. `insecure` never turns certificate checks off. Both steps
   together get `REMOTE_CONNECT_TIMEOUT` (10 s).
3. The gRPC connection over that one stream, made like a launched plugin's (the same one-shot
   connector, 16 MiB limits, `CallMetadata`), with HTTP/2 keepalive pings every 30 s while idle, 20
   s to answer, so a dead peer or path closes the connection between health checks. The `:authority`
   is the URL's `host:port`; the `:scheme` is `http` even over TLS, because the TLS is the
   connector's, not tonic's.
4. `Describe`, `Configure`, and the namespace check, exactly as for a launched plugin.

The result is a
`RemotePlugin { instance, component, url, describe, identity_namespace, connection, secrets, span }`.
`secrets` is a `Secrets`, a shared, updatable list for scrubbing: the instance's secret values, the
bearer token or the OAuth client secret and the current access tokens (`keep_fresh` adds each
refreshed one, below); the `RemoteChannel` and the health check scrub with its current contents.

The stream hyper gets is a wrapper (`Tracked`) with two tokens:

- `closed`, cancelled when hyper drops the stream, which it does when the HTTP/2 connection ends,
  however it ends (the plugin exits, the network fails, a keepalive goes unanswered, the core
  disconnects). `RemotePlugin::closed()` resolves then.
- `disconnect`, cancelled when the `RemotePlugin` is dropped (a `DropGuard` it owns). From then on
  every read and write fails with `ConnectionAborted`, and a pending read is woken (the wrapper
  polls the token's cancellation future in `poll_read`), so hyper ends the connection at once.
  Dropping the `PluginConnection` alone wouldn't: the registered `RemoteChannel` (and its release
  task, and the session manager's event pump) hold clones, and tonic keeps the connection alive
  while any clone exists, so the old connection would go on carrying traffic (the pump would reopen
  `Events` on it) next to the new one.

There is no reconnect on the same `RemotePlugin`; supervision connects again, and calls through an
old `RemoteChannel` fail.

`RemotePlugin::stop(grace)` is all the stopping the core does: `Shutdown`, waiting up to `grace`
(`REMOTE_SHUTDOWN_TIMEOUT`, 5 s) for the answer, then dropping the `RemotePlugin`, which closes the
connection as above. The SDK takes `Shutdown` as the core disconnecting and keeps serving. Dropping
a `RemotePlugin` disconnects the same way without `Shutdown`.

### OAuth client credentials

`oauth::TokenClient` (RFC 6749 section 4.4) posts `grant_type=client_credentials` (and `scope`, the
scopes joined by spaces, if any) as a form to `token_url`, authenticated with HTTP Basic, the client
id and secret each form-encoded first (section 2.3.1), over `net::tls_client_builder()` (bundled
roots, no redirects) with a 30 s timeout. The answer, at most 64 KiB, must be a JSON object with
`access_token` (visible ASCII, so it fits a header), `token_type` `Bearer` (any case) if present,
and `expires_in` in seconds if present (a number, or digits in a string as some servers send). A
non-2xx answer becomes `the token endpoint answered <status>: <error> (<error_description>)`, its
text without control characters and scrubbed of the client secret.

`oauth::keep_fresh` refreshes the token for as long as the connection lasts: `refresh_delay` after
each token (60 s before it expires, or halfway through its life if that's shorter, at least 1 s),
retrying failures after 1 s doubling to 60 s while calls go on with the old token. The interceptor
reads the current header through a shared `RwLock`. Each new token is added to the plugin's
`Secrets`, which keep the current and the previous token (an older one has been out of use for a
whole refresh period). A token without `expires_in` is kept until the connection ends. If the old
token expires before a refresh succeeds, calls fail with `UNAUTHENTICATED`, the next health check
fails, and supervision reconnects, which requests a new token first.

### Supervision of remote plugins

`PluginHost::start` supervises remote instances like launched ones, through a `RemoteLaunch` whose
`launch` is `connect_remote` followed by `ChannelWiring::attach_remote`, so each connection
registers its new `RemoteChannel` and attaches it to the session manager. The differences:

- No binary: no hash, no `Describe` cache, `PluginStatus::binary` stays `None` (the admin service
  shows the address as the location), and `Running { pid: None }`.
- Every failure is retried with backoff (1 s doubling to 5 min), whatever `is_retryable` says: the
  core can't see the plugin being started, upgraded, or restarted with another config, so it never
  waits in `Failed`. While it's down, the instance has no channel registered (or a stale one that
  fails every call), and `giles status` shows `backing off` with the last error.
- A crash is the connection closing (`the plugin disconnected`); a failed health check stops
  (disconnects) and reconnects the same way.
- Stopping, at shutdown or for a restart request, is `RemotePlugin::stop`.

## RemoteChannel

`RemoteChannel::new(&LaunchedPlugin, BlobStore, max_blob)` (or `from_remote(&RemotePlugin, ...)`)
implements `giles_types::Channel` over the plugin's `ChannelService` (PLAN sections 4.3, 8.3, 8.5).
One is built per launch or connection. Conversions are `giles_plugin_sdk::convert`, the SDK's own
rules, so the two sides can't disagree (`giles-core` depends on `giles-plugin-sdk` with only its
`tls` feature); they move fields rather than clone.

- `info()`: the identity namespace from `Configure`, `output` and `can_initiate` from the `Describe`
  capabilities.
- `events()` opens `Events`. Its request half carries the acks: `ack(id)` sends `EventAck` on the
  newest stream, which the session manager calls only after storing the event (see
  [Sessions](sessions.md)). Status events pass through as `ChannelEvent::Status`, which the session
  manager records for `giles status`.
- An event that fails conversion, including one with an unknown variant (a newer protocol's `oneof`
  decodes as none), is logged, acked by its `event_id`, and skipped (PLAN section 7.8), and its
  attachments are released unfetched. A response with no event at all has no id to ack and is
  skipped. Plugin text in logs and errors is scrubbed with the instance's secrets and the token.
- A failed `Events` stream (the plugin died, a transient attachment failure) ends the stream after
  one error; the session manager reopens it with backoff and the plugin resends what wasn't acked.
- `send` uploads each attachment with `PutBlob` (the header carries the core's id, the content's
  SHA-256, so the plugin can check the bytes; chunks of at most 1 MiB, read through the blob store's
  hash-checking reader) and then calls `Send` with the returned handles. The handles are the
  plugin's until that `Send` ends; the core doesn't release them. Errors keep their meaning:
  `UNAVAILABLE` is `Error::Unavailable` (the outbox retries it), and so are `CANCELLED`, `UNKNOWN`,
  and `DEADLINE_EXCEEDED`, which a call gets when the plugin dies or drains for a restart mid-call.
  `UNKNOWN` is also what the SDK sends for a plugin's own generic errors (`Error::Other`, via
  `status_from_error`), so those reach the core as `Unavailable` and are retried too.
  `INVALID_ARGUMENT` is `InvalidRequest`, `NOT_FOUND` is `NotFound`, anything else `Other`. An
  attachment missing from the blob store fails before anything is sent.

### Inbound attachments

The attachments are taken out of a message event before conversion, so a plugin may use any handle.
The core never trusts a handle as a claim about the content, not even one that looks like a SHA-256:
each attachment is fetched with `GetBlob` and streamed into the blob store, which hashes it, so the
stored `MediaRef` names what actually arrived. The media type and name come from the event's
`BlobRef` (the header's media type if the event's is empty); the size is what arrived.

Limits (PLAN section 8.3), checked before and during the transfer:

- The event's `size` over the instance's `max_blob`: refused without calling `GetBlob`.
- The header's `size` over `max_blob`: refused before reading data; dropping the stream cancels the
  transfer.
- More data than the header's `size`, a stream that ends short of it, no header first, a second
  header, or a chunk over 1 MiB: refused at that chunk, and nothing is stored (the store's temp file
  is deleted).

A refused attachment (those, or `GetBlob` answering `NOT_FOUND`, `INVALID_ARGUMENT`,
`FAILED_PRECONDITION`, `OUT_OF_RANGE`, `PERMISSION_DENIED`, or `UNIMPLEMENTED`, all of which a retry
would get again) is left out, logged with the details, and the message goes on with a note appended
to its text (after a blank line), so the agent can tell the sender. The note is
`[attachment "<name>" (<media type>, <size> bytes) not received: <reason>]`, the reason one of
`it is larger than this channel's limit of <max_blob> bytes`, `the channel sent it incorrectly`,
`the channel no longer has it`, or `the channel couldn't send it`.

Any other failure (the connection, another status, no answer to the `GetBlob` call or no chunk for
30 s (`BLOB_CHUNK_TIMEOUT`), the blob store) may pass: the stream yields an error, the event stays
unacked, and it comes again on the reopened stream.

An event's handles are released with `ReleaseBlob` right after the core has stored the event and
acked it, as PLAN section 8.3 orders (a background task per `RemoteChannel` sends them in order,
batching what is queued; failures are only logged). The SDK stores identical bytes once, so two
pending events can share a handle; it keeps a blob as long as an unacked event refers to it, so
releasing at the first ack doesn't take the blob from the second, however long the second waits. The
SDK also deletes a blob itself when the last event using it is acked, and sweeps blobs no unacked
event refers to at startup, so a release lost to a crash or a restart leaks nothing (see
[Plugin SDK](plugin-sdk.md#blobs)).

## Supervision

`plugin_host::supervisor` runs one task per plugin channel (PLAN section 7.5), under a
`supervisor{instance=<name>}` span (each launch adds its own `plugin{instance=<name>}` span inside
it). A launch is `Launcher::launch_channel` followed by `ChannelWiring::attach` (below), so every
launch, the first and each relaunch, registers its new `RemoteChannel` and attaches it to the
session manager, replacing the old one and its event pump. Remote plugins differ as described under
[Supervision of remote plugins](#supervision-of-remote-plugins).

While the plugin runs, the task waits for the first of:

- Shutdown (`stop_all`): `LaunchedPlugin::stop(STOP_GRACE)`: `Shutdown` with a deadline, then
  `SIGTERM`, then `SIGKILL` to the process group.
- A restart request (`PluginHost::restart`, for the admin service's `RestartPlugin`): the plugin is
  stopped the same way and launched again at once, with the backoff reset.
- The process exiting (`PluginProcess::wait`, which kills the rest of its group before reaping).
- A failed health check: every `HEALTH_INTERVAL` (30 s) a `grpc.health.v1` `Check` of
  `giles.channel.v1.ChannelService`, which must answer `SERVING` within `HEALTH_TIMEOUT` (10 s). One
  failure stops the plugin and counts as a crash.

After a crash or a failed check, the task relaunches after a delay from `FIRST_BACKOFF` (1 s),
doubling up to `MAX_BACKOFF` (5 min); the delay starts over at 1 s if the plugin ran for at least
`HEALTHY_RESET` (10 min). A failed launch is retried the same way if `LaunchError::is_retryable`.
One that isn't (the sandbox trampoline's 125 or macOS `sandbox-exec`'s 71, no shared protocol (76),
a `Describe` mismatch, a rejected config, a bad namespace, no binary, no sandbox on this host) is
logged as `cannot launch the plugin; not retrying until its binary changes`, and the task hashes the
binary (SHA-256 of the file `Launcher::find_binary` resolves, the same lookup as the launch) every
`BINARY_RECHECK` (1 min) and relaunches once the hash differs from the failed launch's (a binary
appearing or disappearing counts), or on a restart request. A plugin that exits after a successful
launch is always retried: the trampoline's codes only mean something before the handshake. Shutdown
interrupts any wait at once.

### Status

`PluginHost::statuses()` and `status(instance)` return `PluginStatus`, for `giles status` and the
admin service:

```rust
pub struct PluginStatus {
    pub instance: InstanceName,
    pub component: String,
    pub binary: Option<PathBuf>,          // found at the last lookup, links resolved
    pub state: PluginState,               // Starting | Running { pid: Option<u32>, since } |
                                          // BackingOff { until } | Failed | Stopped
    pub restarts: u32,                    // relaunches since the core started
    pub last_error: Option<String>,       // last launch failure, crash, or failed check; kept
    pub describe: Option<DescribeInfo>,   // version, output, can_initiate, requested_grants
    pub describe_cached: bool,            // describe came from the cache
}
```

Messages in `last_error` are safe to show: `LaunchError` messages and health-check statuses are
scrubbed of the instance's secrets and the token. The channel's own status (`Connected`,
`NeedsAuth`, ...) is recorded by the session manager (`Sessions::channel_status`).

`PluginHost` implements `admin::PluginControl` for the [Admin service](admin.md): `plugins()`
returns the statuses as `PluginRuntime`s (the state as is; the version and `requested_grants`, field
for field as `SandboxGrants`, from `describe`), and `restart` is `PluginHost::restart`. `serve`
shares one `Arc<PluginHost>` between the two.

### The Describe cache

`DescribeCache` (PLAN section 7.9) keeps what each instance's binary reported in `Describe`, as
`DescribeInfo`, in `<root>/cache/describe/<instance>.json` (mode 0600, written atomically): the
binary's SHA-256, the instance's config hash (`ChannelInstance::config_hash`, taken before secrets
are resolved), and the answer with its version. Each launch rewrites it. Before each launch, and at
each binary check, supervision looks up the entry for the binary and config at hand and shows it in
`PluginStatus::describe` (with `describe_cached`) until the plugin is up, so `giles status` can show
capabilities and requested grants of a plugin that is failing or backing off. The lookup can't know
the version before a launch, so it matches the binary and the config and returns the version
recorded with them. A damaged file is a miss.

## Startup and shutdown

`PluginHost::new(launcher, ChannelWiring { registry, sessions, blobs })` and then `start(&config)`
start supervising every plugin channel after the sessions start, and return once each has had its
first launch attempt, before `giles started` (see [The `giles` binary](daemon.md)). Each launched
plugin is wired at once by `ChannelWiring::attach(&plugin, max_blob)` (a remote one by
`attach_remote`): its `RemoteChannel` is registered with `Registry::register_channel` (so replies
and `Registry::pending` see it) and attached with
`Sessions::attach_channel(instance, component, channel)` (so its events flow). A shutdown signal
during startup stops waiting for the first attempts. At shutdown, after the sessions have stopped,
`stop_all()` stops every supervisor, which stops its plugin (or abandons its launch in progress,
killing its process group), and waits for them; nothing is relaunched afterwards. A plugin that
crashed stays registered (its `RemoteChannel` fails every call) until its relaunch replaces it.

## Tests

- Unit tests: the spec built from grants, exit classification, `Describe` checks, retryability, pid
  file parsing, executable lookup, `traceparent` and the shared `authorization` header, a remote
  plugin's dial target (authority, default ports, TLS server names, IPv6), its stream wrapper (a
  disconnect wakes a pending read and fails it and every write with `ConnectionAborted`; dropping it
  reports the connection closed), `Secrets::replace`, and the process (fd 3, token, own group, stdin
  end-of-file, group kill on `terminate`, on drop, and when the plugin exits by itself, `SIGTERM`
  ignored, stderr tail).
- `plugin_host/oauth/tests.rs`: the token client against a fake token endpoint on `127.0.0.1` (Basic
  auth with form-encoded id and secret, the form body with and without scopes, error answers, an
  oversized answer, no endpoint), response parsing, `refresh_delay`, and `keep_fresh` replacing the
  header after each refresh (and adding each token to the scrubbed secrets, the oldest dropped),
  keeping the old token through a failed refresh, and stopping when dropped.
- `plugin_host/remote_channel/tests.rs`: `RemoteChannel` against a fake plugin served in-process
  over a socket pair, for what the SDK can't produce: unknown and invalid events acked and skipped,
  attachments hashed whatever the handle claims and each event's handles released right after its
  ack (also when two events share a handle), `max_blob` on the event's and the header's size, lying
  and malformed transfers refused and nothing stored, `NOT_FOUND` notes, a transient `GetBlob`
  failure leaving the event unacked, `PutBlob` before `Send`, and `Send` errors mapped and scrubbed.
- `plugin_host/supervisor/tests.rs`: supervision against fake launches in paused Tokio time: backoff
  1 s doubling to 5 min after crashes and failed launches, the reset after 10 minutes up (and none
  after 9), health checks every 30 s and a failed one stopping and relaunching the plugin, a failure
  that won't pass waiting until the binary's hash changes, restart requests, shutdown of a running
  plugin and during a backoff, and the `Describe` cache shown while the plugin is down (only for the
  binary it came from). `describe_cache.rs` unit tests: matching on binary and config, rewriting,
  mode 0600, a damaged file.
- `crates/giles-core/tests/plugin_host.rs`: shell scripts as plugins (and as a fake trampoline):
  handshake timeout, malformed line, unoffered protocol, exit 76, an early exit with scrubbed stderr
  under the instance span and exactly the scrubbed environment, a trampoline exiting 125 (and the
  spec file it got), the host check refusing sandboxed instances, a missing binary, a group kill
  after a failed `Describe`, and orphan cleanup that spares an unrelated process.
- `plugins/giles-channel-echo/tests/host.rs`: the echo plugin end to end, unsandboxed: `Describe`,
  `Configure` with a resolved secret, the data directory and pid file, health over the connection,
  `stop`, a rejected config (scrubbed), a binary installed under another name, and stdin
  end-of-file.
- `plugins/giles-channel-echo/tests/remote_channel.rs`: the echo plugin as a channel of a running
  core (runtime, sessions with a recording turn runner, `PluginHost`): an injected message becomes a
  turn, is acked (the SDK's spool empties), and the reply reaches `Send`; a status event is
  recorded; attachments both ways (two events sharing bytes, released afterwards; `PutBlob` read
  back by the plugin); an attachment over `max_blob` left out with a note; registration clears
  `pending()`, and `stop_all` stops everything.
- `plugins/giles-channel-echo/tests/supervision.rs`: a plugin killed with `SIGKILL` is relaunched
  and its new channel carries messages both ways; a restart request does the same; a plugin exiting
  76 is `Failed` until a restart request finds a working binary; the `Describe` cache is written and
  shown (`describe_cached`) when the same binary and config then fail to start; the admin service
  (over `run/admin.sock`) shows the running plugin's state, pid, version, binary, and requested
  grants, and `RestartPlugin` relaunches it.
- `plugins/giles-channel-echo/tests/host_sandbox.rs`: the echo plugin through the trampoline (the
  test binary acting as `giles sandbox-exec`, since Cargo gives that package no path to `giles`): a
  working sandboxed launch where the host can sandbox (macOS, Linux with Landlock); on a Linux
  kernel without Landlock, a clean non-retryable failure with the trampoline's message.
- `crates/giles/tests/serve.rs`: `giles serve` with plugins that fail to launch (one through the
  real trampoline, refused by `min_abi = 1000` on Linux and by its missing `/bin/sh` grant on macOS)
  keeps running, logs the retryable one's backoff and the other as waiting for a new binary, and
  stops cleanly; a shutdown signal while a plugin launches exits at once and kills the plugin.
- `plugins/giles-channel-echo/tests/remote.rs`: the echo binary as a remote plugin on `127.0.0.1`
  (`serve --listen`) of a running core: a bearer token, messages both ways, `Running` without a pid
  or binary; stopping the core leaves the plugin running, and the next core (configuring it again)
  gets the message that arrived in between; a wrong token backing off with `UNAUTHENTICATED`; an
  unreachable address backing off until the plugin comes up, a killed plugin reported as
  `the plugin disconnected` and reconnected once it's back; OAuth client credentials from a fake
  token endpoint (and a refused request backing off, scrubbed); TLS with a test certificate
  authority, refused with the bundled roots and accepted with `remote_tls_roots`; after a restart
  request, calls through the first connection's `RemoteChannel` (still referenced) fail and nothing
  reaches the plugin through it, while the new connection carries messages.
