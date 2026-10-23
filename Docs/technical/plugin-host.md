# Plugin Host

Code: `crates/giles-core/src/plugin_host/` (`giles_core::plugin_host`). Design: PLAN sections 7.3,
7.4, 7.7, 7.8, 11.6, 12.1, 12.4, and 13.4. The plugin side of the same steps is the
[Plugin SDK](plugin-sdk.md); the wire setup is [Plugin Transport](plugin-transport.md); the
trampoline is the [Plugin Sandbox](sandbox.md).

The plugin host launches gRPC channel plugins (provider plugins aren't supported in v1). It takes a
configured `[channel.<instance>]` without `address` from its binary to a configured plugin, and
hands back a `LaunchedPlugin` that supervision (TASKS.md 2.17) and the `RemoteChannel` adapter
(2.18) build on. It doesn't restart plugins or register channels yet.

## Launch

`Launcher::launch_channel(&ChannelInstance)` runs these steps under a `plugin{instance=<name>}`
span. Any failure after the spawn kills the plugin's whole process group before the error returns.

1. Sandboxed instances are refused at once if the startup host check failed (`Launcher::new` takes
   its result): one clear error instead of each launch failing in the trampoline.
2. Discovery: `config::discover_plugin` (`<root>/plugins/`, then `PATH`), the same lookup the
   config loader used, so the binary checked is the binary run. The path is then resolved
   (`canonicalize`): the file granted in the spec, recorded in the pid file, and run is one file.
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
10. `Describe` (10 s): `core_protocol_minor` 0 and no `core_features` yet (2.18 adds the ones its
    adapter understands). The kind must be `PLUGIN_KIND_CHANNEL`, the name must be the binary's
    `<name>`, and the channel capabilities must be present.
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
| The connection can't be set up | `Connect` | yes |
| `Describe` fails or times out | `Rpc` | yes |
| `Configure` fails | `Rpc` | no for `INVALID_ARGUMENT`, `FAILED_PRECONDITION`, `INTERNAL`, `UNIMPLEMENTED`; else yes |
| Wrong kind or name, no channel capabilities | `Mismatch` | no |
| A namespace that isn't the plugin's own | `Namespace` | no |

The failures before the handshake completes carry the plugin's last stderr lines (`stderr()`), and
their message ends with the last non-empty one, so a trampoline failure reads like
`the sandbox trampoline failed (exit status: 125): Error: ...`. Those lines were also logged as
they arrived.

## Spawning

The program is the trampoline, `<giles exe> sandbox-exec run/sandbox/<instance>.json -- <binary>`.
The trampoline inherits the core's environment and builds the plugin's scrubbed one itself. With
`unsandboxed = true` the host skips the trampoline, logs `running the plugin unsandboxed` at warn
level on every launch, and runs the binary itself with exactly the environment the trampoline
would give it (`sandbox::scrubbed_env`) and the data directory as working directory.

In the child, between `fork` and `exec` (`pre_exec`, async-signal-safe calls only):

- `setsid()`: the plugin leads a new session and process group, so the host can signal the whole
  group and a terminal's Ctrl-C doesn't reach plugins directly.
- The plugin's end of the socket pair becomes fd 3 with `dup2` (or, if it already is fd 3, its
  close-on-exec flag is cleared). Everything else the core opens is close-on-exec, and the
  trampoline passes fds 0 to 3 through `exec` (including macOS `sandbox-exec`).

The pid survives both `exec`s, so the spawned process is the plugin.

## The Sandbox Spec

Written with `Spec::to_json` to `run/sandbox/<instance>.json`, mode 0600 (via a temporary file
and a rename), outside every plugin's grants. From PLAN section 12.4:

| Field | Value |
| ----- | ----- |
| `data_dir` | `data/plugins/<instance>/` |
| `read` | the resolved plugin binary, then the `sandbox.read` grants |
| `write`, `unix`, `min_abi` | the grants as configured |
| `network` | the grant; the config loader already applied the channel default (on) |
| `execute` | none: the backends always allow the command itself |
| `unsandboxed` | the instance's `unsandboxed` |
| `env` | `GILES_PLUGIN_MAGIC` (the cookie) and `GILES_PLUGIN_PROTOCOLS` (`1`) |

The binary's directory isn't granted: a binary installed in, say, a home directory would expose
the whole directory. The trampoline adds `GILES_PLUGIN_DATA`, `HOME`, `TMPDIR`, `PATH`, and `LANG`.

## The Process

`PluginProcess` owns the child, its stdin, its pid file, and the stderr task:

- stderr is read line by line (lines cut at 4 KiB), scrubbed, and logged at info level with
  target `giles_core::plugin_host::stderr` under the instance's span, e.g.
  `INFO plugin{instance=irc}: giles_core::plugin_host::stderr: connected`. The last 20 lines are
  kept for error messages (`stderr_tail`).
- `wait()` and `exit_status()`: when the plugin has exited, `SIGKILL` to the rest of its process
  group, then reap it. Children can outlive the plugin, and this is the only moment the group can
  be signalled safely after the plugin is gone: the exit is observed without reaping
  (`waitid` with `WNOWAIT`), so the unreaped plugin still holds its pid, which is the group id,
  and no other process or group can have it. Once reaped, the group id could be reused, so the
  host never signals the group again. `wait` wakes on `SIGCHLD` (checking every second as a
  safety net) and is cancel-safe.
- `close_stdin()`: the plugin sees end-of-file, which a Giles plugin takes as the core being gone.
- `terminate(grace)`: `SIGTERM` to the process group, up to `grace` for the plugin to exit, then
  `kill()`.
- `kill()`: `SIGKILL` to the process group if the plugin is still unreaped, then waits for it (5 s
  at most, which kills the rest of the group as above) and removes the pid file.
- Dropping it while the plugin is unreaped sends `SIGKILL` to the group; dropping it always
  removes the pid file. A plugin seen to exit has already taken its group with it, so supervision
  can wait for a crash, drop the handle, and relaunch without leaving children behind.

A child that calls `setsid` itself leaves the group and escapes all of this; fully containing a
plugin's descendants needs more (a cgroup on Linux), as [Plugin Sandbox](sandbox.md) notes.

## Pid Files and Orphans

`run/pids/<instance>.pid`, mode 0600, written right after the spawn: the pid and the resolved
binary path, one per line. A clean stop or a failed launch removes it.

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
[Plugin Transport](plugin-transport.md) (no reconnect). `plugin()`, `channel()`, and `health()`
build clients with the 16 MiB limits; `transport()` is the intercepted channel for others.

`CallMetadata`, the interceptor on every client, adds `authorization: Bearer <token>` (marked
sensitive) and a W3C `traceparent`. PLAN section 13.4 asks for trace context in gRPC metadata,
but the core has no OpenTelemetry layer yet, so `tracing` spans have no trace ids to propagate.
Until it does, each call gets a fresh random trace id and parent id with the sampled flag off
(`00-<32 hex>-<16 hex>-00`), and the value is logged at debug level in the caller's span, which
ties the core's log of a call to anything the plugin logs for it. With OpenTelemetry, the
interceptor injects the current span's context instead; the SDK doesn't read the header yet.

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
    pub span: tracing::Span,          // plugin{instance=...}
}
```

`LaunchedPlugin::stop(grace)` is the stop sequence of PLAN section 7.5: `Shutdown` and closing
stdin, up to `grace` (`STOP_GRACE`, 10 s: the SDK drains for up to 5 s) for the plugin to exit, then
`terminate(TERM_GRACE)` (5 s), which also kills what's left of the group and removes the pid file.

## Startup and Shutdown

`PluginHost::new(launcher)` and then `launch_all(&config)` launch every plugin channel concurrently
after the sessions start and before `giles started` (see [The `giles` Binary](daemon.md)). A failure
is logged as `cannot launch the plugin; the instance stays pending` with the error and whether it's
retryable; nothing retries it yet (2.17). Each plugin is kept as soon as it's up; a shutdown signal
during startup drops the launches still running, which kills their process groups. At shutdown,
after the sessions have stopped, `stop_all(STOP_GRACE)` stops every launched plugin concurrently.

## Tests

- Unit tests: the spec built from grants, exit classification, `Describe` checks, retryability,
  pid file parsing, executable lookup, `traceparent`, and the process (fd 3, token, own group,
  stdin end-of-file, group kill on `terminate`, on drop, and when the plugin exits by itself,
  `SIGTERM` ignored, stderr tail).
- `crates/giles-core/tests/plugin_host.rs`: shell scripts as plugins (and as a fake trampoline):
  handshake timeout, malformed line, unoffered protocol, exit 76, an early exit with scrubbed
  stderr under the instance span and exactly the scrubbed environment, a trampoline exiting 125
  (and the spec file it got), the host check refusing sandboxed instances, a missing binary, a
  group kill after a failed `Describe`, and orphan cleanup that spares an unrelated process.
- `plugins/giles-channel-echo/tests/host.rs`: the echo plugin end to end, unsandboxed: `Describe`,
  `Configure` with a resolved secret, the data directory and pid file, health over the
  connection, `stop`, a rejected config (scrubbed), a binary installed under another name, stdin
  end-of-file, and `PluginHost` start and stop.
- `plugins/giles-channel-echo/tests/host_sandbox.rs`: the echo plugin through the trampoline (the
  test binary acting as `giles sandbox-exec`, since Cargo gives that package no path to `giles`):
  a working sandboxed launch on macOS; on Linux, until 2.13, a clean non-retryable failure with the
  trampoline's message.
- `crates/giles/tests/serve.rs`: `giles serve` with plugins that fail to launch (one through the
  real trampoline) keeps running, logs them as pending, and stops cleanly; a shutdown signal while
  a plugin launches exits at once and kills the plugin.
