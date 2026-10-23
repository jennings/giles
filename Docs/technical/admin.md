# Admin Service

`giles_core::admin` serves `giles.admin.v1.AdminService` (PLAN section 8.7,
[gRPC Protocols](protocols.md)) on the admin socket. The `giles` CLI is its client; the web admin
pages will be another.

## One Core per Data Root

`giles serve` takes `InstanceLock::acquire` right after creating the data root's layout, before it
loads the configuration: an exclusive, non-blocking `flock` on `run/giles.lock` (mode 0600), into
which it then writes its pid. A second `giles serve` on the same data root fails at once with
`giles is already running with the data root <root> as pid <pid> ...` and touches nothing beyond
creating the layout: not the database, not the plugins' pid files, not the socket.

The kernel drops the lock when the process exits, however it exits, so a crash never leaves a stale
lock behind. The descriptor is close-on-exec, so plugins don't inherit it. The lock is what makes a
leftover `admin.sock` safe to delete: whoever holds it is the only core that could be listening.

## Socket and Token

`AdminListener::bind(&root, &lock)` runs after the HTTP listener is bound and before the sessions
start (it can fail; nothing after the sessions may):

1. `DataRoot::check_socket_dir` (the `$TMPDIR` fallback must be a private directory of ours).
2. A fresh token, 32 bytes from the OS random number generator as 64 hex digits, written to
   `run/admin.token` through a temporary file (mode 0600) and a rename, so a client never reads a
   partial token. Every start replaces it.
3. Remove `admin.sock` if it exists (a crash left it), bind it, and set its mode to 0600. Its
   directory is 0700 already; the mode is a second layer.

`AdminListener::serve(service, shutdown)` serves until the shutdown token is cancelled, waits up to
`DRAIN_TIMEOUT` (5 s) for open calls, then removes the socket and the token file. `serve` awaits it
after stopping the plugins and releases the lock last.

Every call goes through an interceptor that compares the `authorization` metadata with
`Bearer <token>` in constant time; anything else is `UNAUTHENTICATED` ("missing or invalid admin
token"). File modes don't separate the core from its plugins, which run as the same user; the token
is outside every plugin's grants (config validation rejects grants overlapping the data root). Both
directions use 16 MiB message limits.

`admin::connect(&root)` is the client side: `check_socket_dir`, connect to the socket, then read the
token (after the socket answers, so it's the running core's). A missing socket, a refused
connection (a socket a dead core left), or a fallback socket directory that doesn't exist is
`ConnectError::NotRunning`. The returned `AdminClient` adds the token to every call.

## RPCs

| RPC               | Behavior                                                                                    |
| ----------------- | ------------------------------------------------------------------------------------------- |
| `Status`          | Version, start time, `config_error` (always unset in v1: an invalid configuration stops startup, and there is no reload), the host's sandbox (`SandboxHost`: the startup check's report or error, and the Landlock ABI on Linux), every configured channel with its runtime and last recorded status (from `Sessions::channel_statuses`, TASKS.md 2.3), and the plugins as in `ListPlugins`. |
| `ListPlugins`     | Every configured channel plugin (launched or remote) and MCP toolset, channels first, each sorted by name. v1 providers are all built in, so none is listed. |
| `RestartPlugin`   | `NOT_FOUND` for anything not listed by `ListPlugins`; `UNAVAILABLE` without plugin supervision; `FAILED_PRECONDITION` if supervision doesn't know the instance; else `PluginControl::restart`. |
| `ListSessions`    | `Store::list_sessions`: newest activity first, optionally one agent's, open ones unless `include_ended`. `INVALID_ARGUMENT` for an invalid agent id. |
| `ResetSession`    | Ends the session like `/new` (`Store::end_session` at the current time; a running turn finishes, and the conversation's next message opens a new session). `NOT_FOUND`, `FAILED_PRECONDITION` if it already ended, `INVALID_ARGUMENT` for a malformed id. |
| `CreateLoginLink` | `LoginLinks::create_login_link(&user, now)`; `UNAVAILABLE` without the web channel. `UnknownUser` maps to `NOT_FOUND`, `Refused` to `FAILED_PRECONDITION`. |
| `UsageReport`     | `Store::usage_by_model(since, until)`: one row per agent, provider instance, and model. |
| `Backup`          | `UNIMPLEMENTED` until backups (TASKS.md 4.1).                                               |

Store failures are logged and answered with `INTERNAL`.

### Plugin Instances

Each `PluginInstance` combines three sources:

- The configuration: kind, component (empty for a toolset that runs a command or a URL), location
  (the program as configured, or the URL), and the sandbox. For a sandboxed local instance,
  `SandboxStatus.missing` is `HostReport::lacking` for its grants (Landlock ABI shortfalls on Linux;
  nothing on macOS), or one entry with the host's error when the host can't sandbox at all.
  Unsandboxed instances have `enabled = false`; remote ones no sandbox.
- The channel statuses recorded since startup (`channel_status`, channels only).
- Plugin supervision, through `PluginControl::plugins()`: state (`Running` sets `pid` and
  `started_at`, `BackingOff` sets `retry_at`), the binary found (replaces the location), restarts,
  the last error, the version, and the `requested_grants` from `Describe`. Without supervision, or
  for an instance it doesn't report, these stay unset and `state` is `UNSPECIFIED`.

## Seams for Later Subsystems

`AdminOptions` takes trait objects, so the service is tested with fakes and later subsystems plug
in without changing it:

```rust
pub trait ChannelStatuses: Send + Sync {
    fn channel_statuses(&self) -> BTreeMap<InstanceName, ChannelStatusRecord>;
}
// Implemented by `Sessions`.

pub trait PluginControl: Send + Sync {
    fn plugins(&self) -> Vec<PluginRuntime>;
    fn restart(&self, instance: &str) -> bool;
}
pub struct PluginRuntime {
    pub instance: InstanceName,
    pub binary: Option<PathBuf>,
    pub state: PluginRunState, // Starting, Running { pid: Option<u32>, since }, Idle,
                               // BackingOff { until }, Failed, Stopped
    pub restarts: u32,
    pub last_error: Option<String>,
    pub version: Option<String>,
    pub requested_grants: Option<giles_proto::plugin::v1::SandboxGrants>,
}

pub trait LoginLinks: Send + Sync {
    fn create_login_link(&self, user: &UserId, now: Timestamp)
        -> Result<LoginLink, LoginLinkError>; // LoginLink { url, expires_at }
}                                             // LoginLinkError::{UnknownUser, Refused}(message)
```

The built-in web channel implements `LoginLinks` (its `UnknownUser` becomes `UnknownUser`,
`NoWebIdentity` becomes `Refused`); `serve` passes `registry.web_channel()`, so without a
`[channel.web]` instance `CreateLoginLink` answers `UNAVAILABLE`. The plugin host implements
`PluginControl` ([Plugin Host](plugin-host.md#status)): `plugins()` maps each `PluginStatus` field
for field (`Running { pid }` becomes `pid: Some(pid)`, and the version and `requested_grants` come
from the live or cached `Describe` answer), and `restart` is `PluginHost::restart`. `serve` passes
it as `plugins`. Toolsets are supervised by the toolset host ([Toolsets](toolsets.md#lifecycle)),
which doesn't report to the admin service yet, so their lifecycle fields stay unset.
