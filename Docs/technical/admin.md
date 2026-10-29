# Admin service

`giles_core::admin` serves `giles.admin.v1.AdminService` (PLAN section 8.7,
[gRPC Protocols](protocols.md)) on the admin socket. The `giles` CLI is its client over the socket;
the web admin pages are another, calling the same `Arc<AdminService>` in-process behind the web
login ([Web Channel](web-channel.md#admin-pages)).

## One core per data root

`giles serve` takes `InstanceLock::acquire` right after creating the data root's layout, before it
loads the configuration: an exclusive, non-blocking `flock` on `run/giles.lock` (mode 0600), into
which it then writes its pid. A second `giles serve` on the same data root fails at once with
`giles is already running with the data root <root> as pid <pid> ...` and touches nothing beyond
creating the layout: not the database, not the plugins' pid files, not the socket.

The kernel drops the lock when the process exits, however it exits, so a crash never leaves a stale
lock behind. The descriptor is close-on-exec, so plugins don't inherit it. The lock is what makes a
leftover `admin.sock` safe to delete: whoever holds it is the only core that could be listening.

## Socket and token

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
token (after the socket answers, so it's the running core's). A missing socket, a refused connection
(a socket a dead core left), or a fallback socket directory that doesn't exist is
`ConnectError::NotRunning`. The returned `AdminClient` adds the token to every call.

## RPCs

| RPC               | Behavior                                                                                    |
| ----------------- | ------------------------------------------------------------------------------------------- |
| `Status`          | Version, start time, `config_error` (always unset in v1: an invalid configuration stops startup, and there is no reload), the host's sandbox (`SandboxHost`: the startup check's report or error, and the Landlock ABI on Linux), every configured channel with its runtime and last recorded status (from `Sessions::channel_statuses`), and the plugins as in `ListPlugins`. |
| `ListPlugins`     | Every configured channel plugin (launched or remote) and MCP toolset, channels first, each sorted by name. v1 providers are all built in, so none is listed. A configured instance takes the first supervision report with its kind and name (the plugin host's and the toolset host's come before the toolset manager's). A report left over is a stored toolset that failed to start (`ToolsetManager`, [Toolsets](toolsets.md#stored-toolsets)): it is listed after the configured instances as a toolset in state `FAILED` with the reason in `last_error`, also when a `giles.toml` instance took its name. |
| `RestartPlugin`   | The request names the `kind` (`InstanceKind`) and the instance: `INVALID_ARGUMENT` for `UNSPECIFIED` or a number the protocol doesn't name, before anything else is looked up. `NOT_FOUND` for an instance of that kind that `ListPlugins` doesn't list (a configured instance, or an instance the control reports, so a failed stored toolset is found; a same-named instance of another kind doesn't count, and a provider never is one); `UNAVAILABLE` without plugin supervision; `FAILED_PRECONDITION` if supervision doesn't know the instance; else `PluginControl::restart(kind, instance)`. For a failed stored toolset that is the manager's: it re-reads `secrets.toml`, validates the stored spec again, and starts it in the background (still failing, it stays `FAILED` with the new reason). |
| `ListSessions`    | `Store::list_sessions`: newest activity first, optionally one agent's, open ones unless `include_ended`. `INVALID_ARGUMENT` for an invalid agent id. |
| `ResetSession`    | Ends the session like `/new` (`Store::end_session` at the current time; a running turn finishes, and the conversation's next message opens a new session). `NOT_FOUND`, `FAILED_PRECONDITION` if it already ended, `INVALID_ARGUMENT` for a malformed id. |
| `CreateLoginLink` | `LoginLinks::create_login_link(&user, now)`; `UNAVAILABLE` without the web channel. `UnknownUser` maps to `NOT_FOUND`, `Refused` to `FAILED_PRECONDITION`. |
| `UsageReport`     | `Store::usage_by_model(since, until)`: one row per agent, provider instance, and model. |
| `Backup`          | `backup::create` into `dir` (absolute, existing): `VACUUM INTO` plus a tar of the files; answers the two paths written. See [Backups](backups.md). |
| `ListToolsets`    | `ToolsetAdmin::list`: every stored toolset and every toolset of `giles.toml`, ordered by name ignoring case. See [Toolset RPCs](#toolset-rpcs). |
| `GetToolset`      | `ToolsetAdmin::get`: one toolset, the name matched ignoring case. `NOT_FOUND` if there is none. |
| `SetToolset`      | Creates a stored toolset or replaces the spec of the one with that name: `ToolsetAdmin::update`, or `add` when there is no such toolset and the call has no `expected_updated_at`. |
| `RemoveToolset`   | `ToolsetAdmin::remove`: deletes a stored toolset and stops it. Answers the name the toolset has. |
| `RenameToolset`   | `ToolsetAdmin::rename`: renames a stored toolset and the globs of the agents that name its tools, with no restart. Answers the toolset under its new name (its `failure` says why it didn't start under it) and the agents whose globs were rewritten, each with its `tools` and `ask` globs before and after. See [Toolset RPCs](#toolset-rpcs). |

Store failures are logged and answered with `INTERNAL`.

### Toolset RPCs

The five toolset RPCs call `ToolsetManager` ([Toolsets](toolsets.md#stored-toolsets)) through the
`ToolsetAdmin` trait, so validation, the live change, and the stored data are the manager's. They
answer `UNAVAILABLE` when `AdminOptions::toolsets` is `None`. `RestartPlugin` is the restart: the
manager is part of the `MergedControl` that handles it.

A `Toolset` message has the name as it was written, the `source` (`STORED` or `GILES_TOML`), the
spec as TOML text, `updated_at`, and `failure`. The spec is `ToolsetSpec::to_toml`: the keys of a
`[toolset.<name>]` table at the top level, with the bearer token as the reference it was stored as
(`{ secret = "name" }`), so no call ever resolves a secret or shows its value. Durations are whole
milliseconds (`"300000ms"`), as in the stored JSON. A toolset of `giles.toml` has no `spec_toml` and
no `updated_at`. A stored toolset whose JSON can't be read has no `spec_toml` either, and `failure`
says why. `updated_at` is the stale-update token: the stored time, to the millisecond.

`SetToolset` reads `spec_toml` with `ToolsetSpec::from_toml`, which refuses a key the spec doesn't
have (`giles.toml` only warns about one), so a typo isn't dropped silently. It then calls `update`,
which finds a stored name ignoring case, keeps its spelling, and compares `expected_updated_at` when
the call has one. Only a name that isn't there, from a call without `expected_updated_at`, goes on
to `add`. A call with `expected_updated_at` for a toolset that isn't there is `NOT_FOUND`, because
the caller read a toolset that has since gone. The response carries the toolset as stored.

The manager stores only remote servers. A spec without `url` (a stdio server, which belongs in
`giles.toml`) is `Invalid` at `url`, and a bearer token that isn't `{ secret = "name" }` is
`Invalid` at `auth.bearer`.

| What happened                                              | Status                                                                 |
| ---------------------------------------------------------- | ---------------------------------------------------------------------- |
| `spec_toml` isn't a spec, or the manager refuses it (`Invalid`) | `INVALID_ARGUMENT`. The message is `<key>: <reason>`, or the reason alone for the toolset as a whole. TOML syntax errors give their line and column. |
| `expected_updated_at` isn't a time                          | `INVALID_ARGUMENT`                                                     |
| Another call created the name between `update` and `add` (`Exists`) | `ALREADY_EXISTS`                                               |
| No toolset has the name (`NotFound`)                        | `NOT_FOUND`                                                            |
| The toolset changed since `expected_updated_at` (`Stale`)   | `FAILED_PRECONDITION`                                                  |
| The name is a toolset of `giles.toml` (`Unsupported`)       | `FAILED_PRECONDITION`                                                  |
| The database failed (`Store`)                               | `INTERNAL`, logged                                                     |
| `RenameToolset` refuses the new name, or an agent's globs (`Invalid`, `Refused`) | `INVALID_ARGUMENT`. The message is the manager's reason, which names each glob that can't be rewritten. |
| `RenameToolset` finds another stored toolset with the new name (`Exists`) | `ALREADY_EXISTS`, naming the new name                                  |
| A step of `RenameToolset` failed (`Failed`)                 | `INTERNAL`, logged. The message has the step, why, and what the undo could not put back. |
| No toolset manager                                          | `UNAVAILABLE`                                                          |

`RenameToolset` takes `old` and `new`. `old` matches ignoring case, and `NOT_FOUND` and
`FAILED_PRECONDITION` (a toolset of `giles.toml`) are about it. The manager's checks and steps, and
the undo of a failed step, are in [Toolsets](toolsets.md#stored-toolsets). The response's agents are
the manager's `ToolsetRenamed::agents`, ordered by agent id, and its `old` is
`ToolsetRenamed::from`, the name the toolset had as stored.

### `giles toolset`

The CLI (`crates/giles/src/toolset.rs`) is a client of these RPCs. `show` prints the spec after an
`# updated_at = <RFC 3339 time>` comment. `set` reads that comment from the file, wherever it is,
and sends the time as `expected_updated_at`, so editing what `show` wrote can't overwrite a change
made meanwhile. A file without the comment replaces whatever is there, or creates the toolset. When
the daemon answers `NOT_FOUND` to a call that sent a time (a copy of another toolset's `show`
output), `set` says to remove the comment. `restart` looks the toolset up with `GetToolset` and
restarts it under the name that answers, so it restarts only toolsets and gets the spelling right.

`rename` prints the old and the new name as the daemon stored them (`RenameToolsetResponse.old` and
`toolset.name`, which can differ in case from the arguments), each glob of each agent that was
rewritten, and says that running sessions keep the old tool names. A toolset that couldn't start
under its new name is reported on standard error, since the rename itself is done. A refusal is
printed with the daemon's reason.

### Plugin instances

Each `PluginInstance` combines three sources:

- The configuration: kind, component (empty for a toolset, which has none), location (the program as
  configured, or the URL), and the sandbox. For a sandboxed local instance, `SandboxStatus.missing`
  is `HostReport::lacking` for its grants (Landlock ABI shortfalls on Linux; nothing on macOS), or
  one entry with the host's error when the host can't sandbox at all. Unsandboxed instances have
  `enabled = false`; remote ones no sandbox.
- The channel statuses recorded since startup (`channel_status`, channels only).
- Supervision, through `PluginControl::plugins()`: state (`Running` sets `pid` and `started_at`,
  `BackingOff` sets `retry_at`), the binary found (replaces the location), restarts, the last error,
  the version, the `requested_grants` from `Describe` (channel plugins), and `tool_count` (toolsets:
  the number of tools offered to new sessions). Without supervision, or for an instance it doesn't
  report, these stay unset and `state` is `UNSPECIFIED`.

## Seams for later subsystems

`AdminOptions` takes trait objects, so the service is tested with fakes and later subsystems plug in
without changing it:

```rust
pub trait ChannelStatuses: Send + Sync {
    fn channel_statuses(&self) -> BTreeMap<InstanceName, ChannelStatusRecord>;
}
// Implemented by `Sessions`.

pub trait PluginControl: Send + Sync {
    fn plugins(&self) -> Vec<PluginRuntime>;
    fn restart(&self, kind: Kind, instance: &str) -> bool;
}
pub struct PluginRuntime {
    pub kind: Kind,
    pub instance: InstanceName,
    pub binary: Option<PathBuf>,
    pub state: PluginRunState, // Starting, Running { pid: Option<u32>, since }, Idle,
                               // BackingOff { until }, Failed, Stopped
    pub restarts: u32,
    pub last_error: Option<String>,
    pub version: Option<String>,
    pub requested_grants: Option<giles_proto::plugin::v1::SandboxGrants>,
    pub tool_count: Option<u32>, // toolsets only
}

pub trait LoginLinks: Send + Sync {
    fn create_login_link(&self, user: &UserId, now: Timestamp)
        -> Result<LoginLink, LoginLinkError>; // LoginLink { url, expires_at }
}                                             // LoginLinkError::{UnknownUser, Refused}(message)

pub trait ToolsetAdmin: Send + Sync {          // async, through `async_trait`
    async fn list(&self) -> Result<Vec<ToolsetView>, ToolsetManagerError>;
    async fn get(&self, name: &str) -> Result<ToolsetView, ToolsetManagerError>;
    async fn add(&self, name: &str, spec: &ToolsetSpec) -> Result<ToolsetView, ToolsetManagerError>;
    async fn update(&self, name: &str, spec: &ToolsetSpec, expected_updated_at: Option<Timestamp>)
        -> Result<ToolsetView, ToolsetManagerError>;
    async fn remove(&self, name: &str) -> Result<Removed, ToolsetManagerError>;
    async fn rename(&self, old: &str, new: &str) -> Result<ToolsetRenamed, ToolsetManagerError>;
}                                              // Implemented by `ToolsetManager`.
```

The built-in web channel implements `LoginLinks` (its `UnknownUser` becomes `UnknownUser`,
`NoWebIdentity` becomes `Refused`); `serve` passes `registry.web_channel()`, so without a
`[channel.web]` instance `CreateLoginLink` answers `UNAVAILABLE`. The plugin host implements
`PluginControl` ([Plugin Host](plugin-host.md#status)): `plugins()` maps each `PluginStatus` field
for field (a remote plugin runs with no pid, and the version and `requested_grants` come from the
live or cached `Describe` answer), and `restart` stops and launches the channel again (`false` for
any other kind, since the host supervises channels only). So does the toolset host
([Toolsets](toolsets.md#lifecycle)): each supervisor's report becomes a `PluginRuntime` (state,
restarts, last error, the binary found, the server's version from `initialize`, and `tool_count`;
`pid` and `requested_grants` stay unset), and `restart` (for `Kind::Toolset` only) is
`Control::request_restart`, which drops the connection and starts again at once, with the backoff
reset (a `Failed` toolset gets a fresh try). `serve` passes both as one `PluginControl`,
`MergedControl`: `plugins()` is the instances of every host, and `restart(kind, instance)` goes to
the first host that supervises an instance of that kind with that name. Each host accepts only its
own kind (the plugin host channels, the toolset host and the manager toolsets), so a channel and a
toolset with the same name are restarted independently. `RestartPlugin` therefore restarts channel
plugins and toolsets alike.

`ToolsetManager` implements `ToolsetAdmin` method for method, and `serve` passes it in
`AdminOptions::toolsets`. The admin service tests plug in a fake that keeps the toolsets in a map,
so each RPC and status code is tested without a database or a host.
