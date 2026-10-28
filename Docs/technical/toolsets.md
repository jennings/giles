# Toolsets

Code: `crates/giles-core/src/toolsets/` (`giles_core::toolsets`). Design: PLAN sections 7.5, 7.6,
7.9, and 7.10. A toolset is an MCP server; the [MCP client](mcp.md) is the connection, and this
module decides how to reach each configured `[toolset.<instance>]` (`ToolsetLauncher`) and keeps it
going until it is removed or the daemon stops (`ToolsetHost`).

## Connecting

`ToolsetLauncher::new(root, trampoline, sandbox, blobs)` mirrors the [plugin host](plugin-host.md)'s
`Launcher`: `trampoline` is the `giles` binary (run as `giles sandbox-exec`), and `sandbox` is the
host check `serve` runs once; an error refuses every sandboxed server with that message. `blobs` is
the runtime's blob store, where every connection keeps the media its tools return
([MCP Client](mcp.md#mapping-mcp-to-the-toolset-trait)). `.env(f)` replaces the process environment
for `PATH` lookups (tests).

`connect(&ToolsetInstance)` returns an `McpToolset` once the server has answered `initialize`:

- **Stdio** (`command`, or the first-party plugin `giles-toolset-<component>`): `resolve` finds the
  program, then `launch` starts it in its sandbox (below) and hands the command to
  `McpToolset::connect_stdio` with the `env` values as the secrets to scrub.
- **Remote** (`url`): `McpToolset::connect_http` with the bearer token from `auth`, if any. Config
  loading accepts only bearer tokens for toolsets; OAuth client credentials are for remote channels.

Errors are `giles_types::Error`, prefixed `toolset <instance>:`. `InvalidRequest` means retrying
won't help (the host can't sandbox, an invalid spec, the sandbox trampoline failing at launch with
125 or macOS's 71, rejected credentials); `Unavailable` covers the rest (program not found, a server
that fails to start or answer, an unreachable URL).

## Finding the program

`command` is an absolute path or a name looked up on the operator's `PATH` (`config::find_program`,
the same lookup config loading uses to protect binary directories). Without `command`, the plugin
`giles-toolset-<component>` is found in `<root>/plugins/`, then on `PATH`
(`config::discover_plugin`). `ResolvedProgram` keeps both the path as found (`found`) and with links
resolved (`program`). The trampoline runs `program`, so the file granted is the file run, and the
[tool-list cache](#tool-list-cache) is keyed on its SHA-256.

## Grants

The sandbox spec is the configured grants plus what running the server needs (`toolsets/grants.rs`,
PLAN section 7.10). Most servers are scripts installed by a package manager:
`/opt/homebrew/bin/mcp-server-filesystem` links into `/opt/homebrew/lib/node_modules/...`, whose
`#!/usr/bin/env node` runs `/opt/homebrew/bin/node`. The plan:

1. Collect the binaries involved: `found`, `program`, and if `program` starts with `#!`, the
   interpreter (`/usr/bin/env`, or e.g. a venv's `python`) and, for `env [options] <name>`, the
   program `<name>` looked up on the operator's `PATH`. Each is added again with its links resolved,
   since the sandbox checks real paths (a venv's `python` usually links into a shared Python
   installation).
2. Grant each one's **install prefix**: the directory holding `bin/` or `sbin/` for a program in one
   (`/opt/homebrew`, the venv), else the program's own directory; never `/` (`/bin/sh` gets `/bin`).
   The prefix goes to both `read` and `execute`, since interpreters need an `execute` grant and
   their libraries need `read`.
3. Each grant passes the same rule as configured grants (`config::check_grant`): it may not overlap
   the data root (other than the instance's own data directory) or the socket directory. It also may
   not be the operator's home directory or contain it (the launcher's `HOME`, compared as written
   and with links resolved), whatever the data root: `~/bin/server` would otherwise grant all of
   `~`, including `~/.ssh`. A prefix that fails narrows to the program's directory, then to the file
   itself. A bundled plugin inside the data root gets no grant at all; the backends always let the
   command itself run.
4. Grants inside another grant are dropped; the rest are sorted.

The server's `PATH` is the directory of `found`, then that of a program found through `env`, then
`/usr/bin:/bin`, so `#!/usr/bin/env node` finds the same `node` it would for the operator. Paths
containing `..` (or `:`, for `PATH`) are left out; the spec refuses them.

Configured `write` and `unix` grants can't touch a directory holding the `giles` binary, a plugin,
or a toolset's command (config loading checks that). That doesn't cover script interpreters (the
`#!` interpreter and a program found through `env`): their prefixes are granted read and execute
here, but config loading doesn't know them, so a `write` grant on, say, a Node installation would
let that server replace the `node` other toolsets run. Don't grant `write` on an interpreter's
installation.

**How wide a prefix is.** PLAN section 7.10 grants whole install prefixes, which is what package
managers need (Homebrew's `node` loads libraries from `/opt/homebrew/opt/*`), and it exposes
whatever else lives there: `/opt/homebrew/etc` and `/opt/homebrew/var` for a Homebrew command, all
of `~/.local` for `~/.local/bin/x`, all of `~/.cargo` (including `credentials.toml`) for
`~/.cargo/bin/x`. Only the home directory itself (and `/`) is ruled out. Operators who mind install
servers in a prefix of their own, such as a venv or `npm --prefix`.

## The launch

Like a plugin launch ([Plugin host](plugin-host.md)), without the gRPC parts:

- Data directory `data/plugins/<instance>/` and its `tmp/`, mode 0700, refused if it is a link
  (`plugin_host::prepare_data_dir`). Instance names are unique across kinds, so toolsets share the
  plugin data root (PLAN section 11.6).
- The spec: `read` and `execute` from the plan (plus configured `read`), configured `write`, `unix`,
  `min_abi`, `network` (off unless granted: the config loader's toolset default), `inherit_env` =
  the names in `env`, and `path` from the plan. No `env`: MCP servers aren't Giles plugins and get
  no handshake variables.
- Sandboxed: the spec goes to `run/sandbox/<instance>.json` (0600, atomically), and the command is
  `giles sandbox-exec <spec> -- <program> <args>` with the `env` values set on the trampoline's
  process. The trampoline passes exactly those names on (the sandbox's
  [inherited variables](sandbox.md#environment)), so credentials never touch the spec file or
  `argv`.
- `unsandboxed = true`: the program runs directly, with a warning at every launch, the same scrubbed
  environment (`sandbox::scrubbed_env` with the `env` values and the daemon's `LANG` as the
  inherited lookup), and the data directory as the working directory.
- `McpToolset::connect_stdio` spawns it as the leader of its own process group; dropping the
  connection kills the group, and the server exiting takes the rest of the group with it
  ([MCP Client](mcp.md#stdio)). A server also exits when its stdin closes, which covers a core that
  dies without stopping it.
- The connection's media: results to the blob store, and `$media` inputs copied into the server's
  `TMPDIR` (`McpMedia::new(blobs).files_in(spec.tmp_dir())`), which the sandbox lets it read
  ([MCP Client](mcp.md#mapping-mcp-to-the-toolset-trait)).

`env` may not set `HOME`, `TMPDIR`, `PATH`, or `GILES_PLUGIN_DATA`: the sandbox derives them
(`sandbox::DERIVED_ENV`), and config loading refuses them.

## Lifecycle

`ToolsetHost::start(launcher, &registry)` runs in `serve` right after the registry is built
([The `giles` Binary](daemon.md#giles-serve-lifecycle)). For each configured toolset it:

1. Looks up the [tool-list cache](#tool-list-cache) (stdio servers only) and offers a hit at once.
2. Registers a `ManagedToolset` with `Registry::register_toolset`. It stays registered until the
   toolset is [replaced or removed](#changing-toolsets-while-giles-runs), so the dispatcher needs
   nothing beyond the registry.
3. Spawns the instance's supervisor task (`toolsets/lifecycle.rs`, under a `toolset{instance}`
   span), which owns the connection and makes the first start.

`started()` waits for every first start to succeed or fail; `serve` waits for it alongside the
plugin launches before logging `giles started`. `stop()` cancels every supervisor and waits for
them, dropping every connection (a stdio server's process group is killed). It is for shutdown;
after it, nothing starts any more.

`ManagedToolset` is the instance's face:

- `list_tools` and `tool_changes` report what the supervisor offers (a `watch` channel), so the
  registry's tool index follows the cache, each start, and each `list_changed`. As always, a new
  list reaches new sessions only; a running session keeps its snapshot (PLAN section 6.4).
- `call` asks the supervisor for the connection over a channel and calls it. The lease it gets holds
  the connection and counts the call as in flight until the returned events are dropped, so idle
  stop never cuts a call short.
- `list_skills` and `get_skill` read the instance's `skills` directories (`skills::discover` on a
  blocking thread, bare names, a later directory overriding an earlier one, broken skills logged and
  skipped), whether or not the server runs. The agent's skill index reads the same directories
  through `Registry::skill_sources` with the `<instance>__` prefix (PLAN sections 6.6 and 7.10),
  which is what the prompt lists and `load_skill` loads, files included.

The supervisor is a loop over four states:

| State     | Leaves on                                     | A call                                  |
| --------- | --------------------------------------------- | --------------------------------------- |
| `Running` | the connection closing, idle stop, shutdown   | gets the connection                     |
| `Idle`    | a call (relaunches, then answers it), shutdown | waits for the relaunch                  |
| `Down`    | the retry time (starts again), shutdown       | fails at once: `Unavailable` with the reason and `retry_after` |
| `Stopped` | (ends the task)                               | fails: `Unavailable`, the toolset is stopped |

Every start lists the tools, offers them, and stores them in the cache. A start whose listing fails
keeps offering the previous list.

**Crashes and failed starts** (PLAN section 7.5): the server exiting (with its status), a lost
connection, or a start that fails with a retryable error leads to `Down` with a restart after
`RESTART_BACKOFF_MIN` (1 s), doubling up to `RESTART_BACKOFF_MAX` (5 min); a connection that lasted
`HEALTHY_RESET` (10 min) starts the backoff over. A non-retryable failure (`InvalidRequest`: the
host can't sandbox, the sandbox trampoline failed with 125 or macOS's 71, rejected credentials, no
shared protocol) isn't retried until it's restarted by hand or Giles restarts, and the instance's
tools are withdrawn, cached or not: every call would fail. While a stdio server will be retried, its
tools stay offered, and calls fail with the reason and when the next attempt is due, as for a server
that's restarting.

**Remote servers** (PLAN section 7.6): the same backoff, but while one is unreachable (at boot or
later) its tools are withdrawn, so new sessions don't get them; sessions that already have them get
`tool unavailable` from the dispatcher, which checks the live index. Giles runs degraded meanwhile:
the other toolsets and everything else work. Remote servers never stop for idleness.

**Idle stop** (`idle_stop`, stdio only): once no call has been in flight for `idle_stop` (counted
from the end of the last call, or the start), the supervisor drops the connection, killing the
server, and goes `Idle`. The tools stay offered (the in-memory list, the same one the cache holds),
and the next call relaunches the server, which also refreshes the list. A server stopped for
idleness didn't crash, so the restart backoff starts over at 1 s; otherwise a server whose
`idle_stop` is under `HEALTHY_RESET` would never reset it.

**Reporting and restart**: each supervisor keeps a `Report` that `ToolsetHost` hands to the admin
service as a `PluginRuntime` (`ToolsetHost` implements `PluginControl`, see
[Admin Service](admin.md#seams-for-later-subsystems)):

| Field          | Value                                                                              |
| -------------- | ---------------------------------------------------------------------------------- |
| `state`        | `Starting` while a start is under way (and before the first), `Running`, `Idle`, `BackingOff { until }` (a `Down` that will be retried), `Failed` (a `Down` that won't), `Stopped` |
| `restarts`     | starts after a crash, a failed start, or a restart request; not the first start, and not the relaunch of an idle server for a call |
| `last_error`   | the reason of the last failed start or crash, as failed calls see it (already scrubbed); kept after the toolset recovers |
| `binary`       | the program of the latest connection (`ResolvedProgram::program`, links resolved); unset for a remote server |
| `version`      | `serverInfo.version` of the latest connection                                      |
| `tool_count`   | the number of tools offered to new sessions, so the cached list while a stdio server starts, and none while a remote server is down |

`pid` is never set: a stdio server runs under the sandbox trampoline, so the process the core
spawned isn't the server, and a remote server has none.

`PluginControl::restart(instance)` (`Control::request_restart`) wakes the supervisor whatever state
it is in, and the supervisor drops the connection and starts again at once. It starts the backoff
over and counts a restart. `Failed` toolsets get a fresh try, as a Giles restart would give them. A
restart while a start is under way abandons that attempt and begins another, so a hung start can be
retried. The old connection closes when its last call ends, so a call still running keeps the old
server alive until it finishes. `restart` answers `false` for an instance the host doesn't
supervise, such as a toolset that couldn't be registered.

## Changing toolsets while Giles runs

A toolset can be added, replaced, and removed without restarting Giles. Each call takes the toolset
as it is, or is to be, in the registry's
[live configuration](registry.md#the-running-configuration): the caller changes the `LiveConfig`
first (`add_toolset`, `replace_toolset`, `remove_toolset`) and then tells the host, because
`register_toolset` checks the live configuration. The host serializes its changes, so two of them
never interleave.

- `add(&ToolsetInstance)` does what boot does for one toolset: the cache lookup, `register_toolset`
  (so the cached tools are offered at once), and a new supervisor. It doesn't wait for the first
  start. A supervisor still running under the same name is stopped first, so a second `add` never
  leaves two. It fails with `RegistryError::NotConfigured` if the instance isn't in the live
  configuration, and does nothing once the host has been stopped.
- `replace(&ToolsetInstance)` stops the old supervisor, then adds the new instance under the same
  name. Stopping drops the old connection, which kills a stdio server's process group, and a call
  still running on it ends as `Unavailable` (`toolset <instance>: stopped`). Its `config_hash`
  differs from the old one's, so the cache lookup misses and nothing is offered until the new server
  answers. Registering replaces the old object and starts a new tool-index generation, which drops
  the old object's watcher, so a late `list_changed` from the old server can't overwrite the new
  list (it can't arrive at all: its connection is gone).
- `remove(&InstanceName)` stops the supervisor, then calls `Registry::unregister_toolset`, which
  withdraws the tools from new sessions. A call still running ends `Unavailable`, and a session that
  took its tools earlier still lists them, but the dispatcher answers a call to one with
  `tool unavailable`: the registry has no such toolset.

Every supervisor has its own cancellation token, a child of the host's, so a stop affects one
toolset and `stop()` still stops all. A stopped supervisor's task ends, and the host waits for that
before it registers the replacement or unregisters the toolset. `stop()` takes the same lock as a
change, so a change in progress finishes first and none starts a supervisor that `stop()` doesn't
wait for. The state the admin service reports follows: a removed toolset is gone from `plugins()`,
and a replaced one starts over as `Starting`.

## Stored toolsets

`toolsets/manager.rs`: the remote MCP servers that the web UI manages are stored in the database
([Storage](storage.md#toolsets)), and `ToolsetManager` is the one place that changes them. There is
one per core, built in `giles serve` after the host has started the toolsets of `giles.toml`. The
web pages, the admin RPCs, and the CLI call it; none touches the table, the live configuration, or
the host. Both sources work side by side, and instance names stay unique across them.

A stored toolset is a remote server. A web login must not be able to choose a program for Giles to
run on the host, its arguments, its environment, or its sandbox grants: installing and running code
on the host is the operator's, so a stdio server is a `[toolset.<name>]` table of `giles.toml`.
Connecting to a remote server is not.

`ToolsetManagerOptions` holds the store, the data root, the `LiveConfig`, and the `ToolsetHost`.
`ToolsetManager::start(options)` loads the stored toolsets, and the operations are:

- `list()` is every toolset ordered by name ignoring case: the stored ones, each a `ToolsetView`
  with its `spec` (a `ToolsetSpec`, credentials as references), its `created_at` and `updated_at`,
  and a `failure` if it isn't running; and the ones of `giles.toml`, as `ToolsetSource::File` views
  without a spec. `get(name)` is one of them. Names match ignoring ASCII case.
- `add(name, &spec)`, `update(name, &spec, expected_updated_at)`, `rename(old, new)`, and
  `remove(name)`. The error is a `ToolsetManagerError`: `Invalid { key, reason }` (the key is the
  spec key that has the problem, `auth.bearer`, or the root for the name or a conflict), `Exists`
  (the name is stored, ignoring case), `NotFound`, `Stale`, `Refused { reason }` (a rename that an
  agent's globs stand in the way of), `Failed { step, error, undo_errors }` (a step of a rename
  failed and was undone), `Unsupported` (the name is a toolset of `giles.toml`, which is read-only
  here), and `Store`. `is_invalid_input()` is true for everything the owner can fix in the form:
  `Invalid`, `Exists`, `Stale`, `Unsupported`, and `Refused`.

A spec is stored as JSON with the keys of the `[toolset.<name>]` table (`ToolsetSpec`'s serde form,
so a duration is `"300000ms"`). A bearer token is a `{ secret = ... }` reference, so no secret value
is in the database.

### One change

One async lock spans each change, as `Routes` does for routes. A change:

1. Looks the name up in the rows. `add` refuses a stored name (`Exists`). `update` and `remove`
   refuse a name that only `giles.toml` has (`Unsupported`) or that nothing has (`NotFound`);
   `update` also compares `expected_updated_at` with the row's (`Stale`).
2. Validates the spec with `validate_toolset` ([Configuration](config.md#validating-one-toolset))
   against the running configuration, with `secrets.toml` read again: a secret added to the file is
   found by the next save, with no restart. Before it, in this order, the manager refuses the name
   `new` (ignoring case), which is the address of the web form for a new toolset (an `Invalid` at
   the root), a name that a `giles.toml` instance uses (the reason is
   ``the name is taken by `[<kind>.<name>]` in giles.toml``, whatever else is wrong with the spec),
   a name in `[core] retired_instances`, which lists names that are not configured, a spec without
   `url` (an `Invalid` at `url`: a stdio server goes in `giles.toml`), and an `auth.bearer` that
   isn't `{ secret = "<name>" }` (an `{ env = ... }` reference or a plain token). `validate_toolset`
   then refuses the keys of a stdio server (`command`, `component`, `args`, `env`, `idle_stop`,
   `sandbox`, `unsandboxed`) next to `url`, each at its key. Nothing is stored when any of these
   fails.
3. Writes the row. The store checks the stale token too, so a write can't overwrite a change that
   isn't the one the caller read.
4. Changes the `LiveConfig` (`add_toolset` or `replace_toolset`), then the host (`add` or
   `replace`), so new sessions get the new tool list and running sessions keep their snapshot. If
   this step fails after the row is written (it can't unless the live configuration and the manager
   disagree), the host has unregistered the toolset, so the entry is taken out of the live
   configuration again (`withdraw`) and the toolset is failed with the reason: it shows as one
   failed stored toolset, not as a toolset of `giles.toml`, and can be saved or removed. The error
   is an `Invalid` at the root.

`remove` deletes the row, takes the toolset out of the live configuration, and stops it in the host,
which withdraws its tools. A remote toolset has no data directory, and its entry in the
[tool-list cache](#tool-list-cache) is never offered, so nothing else is cleaned up. Agents' `tools`
and `ask` patterns that name the toolset's tools are left alone: they match nothing until a toolset
of that name returns.

### Renaming

A toolset's name is its tool prefix and the start of the globs agents use in `tools` and `ask`, so
`rename(old, new)` is more than a change to the row
([Instance rename](instance-rename.md#renaming-while-giles-runs) has the full account, and the
offline `giles instance rename` does the same for a stored toolset while Giles is stopped).
`rename_preview(old, new)` makes the checks and returns a `RenamePreview { agents }`, the
`AgentGlobChange { agent, globs }` of every agent whose `tools` or `ask` globs would change, each
`GlobChange { key, from, to }` with the list it is in (`tools` or `ask`) and the glob before and
after. `rename` returns a `ToolsetRenamed { from, view, agents }`: the name the toolset had as
stored, the toolset under its new name, and the same list. Under the manager's lock, and the agent
manager's lock once the checks start (see
[Instance rename](instance-rename.md#renaming-while-giles-runs)), a rename:

1. Checks the new name and the agents. The name must be valid, not `new`, free among the stored
   toolsets (`Exists`) and the instances of `giles.toml` and `retired_instances` (`Invalid` at the
   root), as for a new toolset. The old name is exempt, so a rename that changes only the case of a
   stored toolset's name is allowed. A running toolset's spec is validated again under the new name,
   with `secrets.toml` read again. Then each agent's `agent.toml` is read. A glob that matches the
   tools without starting with `<old>__` (`github*`) can't be rewritten and refuses the rename:
   `Refused { reason }` names each glob and its agent, and so does a file that isn't valid TOML or
   that the form can't edit. Nothing has changed at this point, and `rename_preview` stops here.
2. Stops the toolset in the host, which unregisters it: a session that lists its old tools gets
   `tool unavailable`.
3. Rewrites each agent's `agent.toml` with the agent edit machinery (`agents::edit`, a temporary
   file renamed over it), and
4. renames the row.
5. Renames the toolset in the `LiveConfig`, replaces the agents there, and starts it under the new
   name (`ToolsetHost::add`). A toolset that failed to start is renamed too, then started if it
   validates now, else it stays failed under the new name with the new reason (`view.failure`).

A stored toolset is remote: it has no data directory, and its tool-list cache entry is never
offered, so nothing on disk moves but the agents' files. A step that fails undoes the earlier ones:
the agents' files are put back, the row is restored exactly (`Store::toolset_restore`), and the
toolset is started again under its old name. The error is `Failed { step, error, undo_errors }`;
`undo_errors` lists what couldn't be put back. A stored toolset whose name an instance of
`giles.toml` took at startup (it shows as failed) is renamed in the database only: the agents' globs
under that name are the instance's.

### Startup

`start` takes every row in name order and runs the same checks as a change, then adds it to the live
configuration and the host. The host has started the toolsets of `giles.toml` first, so a name that
a `giles.toml` instance (of any kind) also uses stays with that instance and the stored one fails. A
stored toolset that fails isn't started: it is logged (`a stored toolset is not started`) and kept
with its reason, a missing secret, a spec that a stored toolset can't have, a spec that can't be
read, or the name. **Startup never fails on a stored toolset**; only the database can't be read.

The manager is also a `PluginControl`, which `serve` merges with the plugin host and the toolset
host. Its `plugins()` is one `Failed` `PluginRuntime` per stored toolset that isn't running, with
the reason as `last_error`. Its `restart(name)` accepts a failed stored toolset (and answers `false`
for any other name) and runs `retry(name)` in the background: it reads `secrets.toml` again,
validates the stored spec against the live configuration as startup did, and starts the toolset, or
leaves it failed with the new reason. `retry` is public, and returns the `ToolsetView`.
`ListPlugins` and `Status` list a runtime that the configuration doesn't have as a toolset, after
the configured instances, so `giles status` shows the failed toolset and why. The fix is in the web
UI, which needs Giles running: saving the toolset again validates it and starts it (`update` on a
failed toolset adds it), and removing it deletes the row.

Giles doesn't wait for a stored toolset's first start before it reports that it started, as it does
for the toolsets of `giles.toml`; it starts in the background.

## Tool-list cache

`toolsets/cache.rs`, PLAN section 7.9: one JSON file per instance in `<root>/cache/toolsets/`
(written atomically, mode 0600), holding the format version, the key, and the bare tool list:

| Key part         | Stdio                                     | Remote         |
| ---------------- | ----------------------------------------- | -------------- |
| `source`         | `sha256:<hex>` of `program`               | the URL        |
| `config_hash`    | `ToolsetInstance::config_hash` (before secrets are resolved, [Configuration](config.md)) | the same |
| `server_version` | `serverInfo.version` from `initialize` (`McpToolset::server_version`) | the same |

Every start stores the fresh list under the full key, and each `list_changed` stores the new list
under the same key. The server version is only known once the server runs, so the lookup at boot
matches `source` and `config_hash`: a different binary (an upgrade) or an edited instance table is a
miss, and nothing is offered until the server answers. On a hit the cached tools are offered from
registration on, while the server starts, and if that start fails they stay offered until a start
succeeds. The version catches upgrades the binary hash can't see (an interpreted server whose
launcher script doesn't change): the start that sees a new version re-lists and replaces the entry.
Remote servers' lists are stored too, but never offered from the cache, since a down remote server's
tools must not be (PLAN section 7.6). A missing, unreadable, or foreign-format file is a miss; a
failed write is logged.

## Tests

- Unit: shebang parsing, install prefixes, the plan for a linked script run by `env`, an interpreter
  link into another prefix, a prefix containing the data root, the home directory (never granted,
  also through a link and with the data root elsewhere), and a bundled plugin; the spec built from a
  plan (credentials named, not written); the cache (hit only for the stored source and config hash,
  replaced by each store, 0600, damaged or foreign files ignored, the digest).
- Lifecycle (`toolsets/tests.rs`), with a fake connector and fake servers on paused time: boot
  starts and registers every toolset and `stop` drops every connection; a cache hit is offered
  before the server answers, then refreshed with the reported version; a changed binary or config
  misses; idle stop waits for calls in flight, keeps the tools, resets the restart backoff, and the
  next call relaunches; a remote server never idles; crash restarts at 1 s, then 2 s, and after ten
  healthy minutes at 1 s again, with calls failing with `retry_after` meanwhile; an unreachable
  remote server leaves the rest running, retries at 1, 2, and 4 s, and its tools come and go with
  it; rejected credentials aren't retried; a stdio server whose start fails for good withdraws its
  cached tools; `list_changed` reaches new snapshots and the cache, not running sessions; a `skills`
  directory is served by bare name and indexed for the agent with the prefix, its skill's files
  listed for `load_skill`.
- `crates/giles/tests/serve.rs`: a stdio toolset that exits and an unreachable remote one are logged
  and retried, with their tools registered (nothing pending) and `giles` running and stopping
  cleanly. With stored toolsets, all remote, one valid and two that can't start (a bearer token from
  the environment, a name that `giles.toml` uses): `giles` runs, the valid one is started and
  supervised, `giles status` lists the others as failed with their reasons, and the log names them.
- `crates/giles-core/tests/toolsets.rs` (`harness = false`): the test binary is both the trampoline
  and the server, found by name on a `PATH` through a link. It reports its environment, working
  directory, and a TCP connect to a local listener on stderr. Sandboxed (Seatbelt on macOS, Landlock
  where the kernel has it; elsewhere the launch is refused with the host check's message): exactly
  the scrubbed variables plus the `env` names, the credential passed, the data directory as working
  directory, the network denied, and with a `tcp:<port>` grant connected; the spec file is 0600 and
  holds the grants and the credential's name but not its value. An attachment passed as
  `{"$media": "<id>"}` reaches the server as a path in its `TMPDIR` that it can read inside the
  sandbox, and the copy is gone after the call. Unsandboxed: the same environment, network open, the
  same media hand-over, and a copy a crash left behind removed at the next start (the server's own
  files stay). Remote: listing tools over streamable HTTP with the bearer token, and a wrong token
  refused as `InvalidRequest`.
- Runtime changes (`toolsets/tests.rs`): adding a toolset registers it, offers its cached tools
  while it starts, and gives new snapshots its tools but not running sessions; replacing it drops
  the old connection (once the call running on it is dropped), ends that call as unavailable, misses
  the cache, offers the new list, and the old connection can't announce a list any more; removing it
  withdraws its tools, ends its calls, drops it from the admin report, and the name can be added
  again; a stopped host starts nothing. The registry's tests cover registering a toolset added to
  the live configuration and unregistering one, and the dispatcher's cover a call from a session
  that lists a removed toolset (`tool unavailable`) and the next batch using an edited toolset's
  `deadline`.
- The manager (`toolsets/manager_tests.rs`, with the fake connector of `tests.rs`, on a real
  database and data root): add, update, and remove reach the database, the live configuration, the
  registry, and the host (an edit restarts the toolset and drops the old connection; a stale update
  and an invalid one change nothing); an invalid spec is refused with its key and nothing is stored;
  a spec without `url`, with a key of a stdio server, or with a bearer token that isn't a
  `{ secret = ... }` reference is refused at that key; a missing secret is refused, then accepted
  after it is added to `secrets.toml` with no restart, and only the reference is stored; startup
  adds the valid stored toolset beside a `giles.toml` toolset and reports each failure (missing
  secret, a name taken by a toolset or a channel of `giles.toml` with the exact reason, a stdio
  spec, a damaged spec) as failed, and saving or removing a failed toolset works and leaves the
  instance that kept the name running; toolsets of `giles.toml` can't be changed, and a stored name
  is unique ignoring case. The admin tests cover a `Failed` runtime that the configuration doesn't
  have in `ListPlugins` and `Status` (also when a `giles.toml` instance has the name, which keeps
  the host's report), and restarting it. Renaming: a running toolset offers its tools under the new
  prefix, and the agents' files and the live configuration say the new name (and the preview lists
  the same globs and changes nothing); a name that is taken (by a stored toolset, or by an instance
  or the retired list of `giles.toml`), reserved, invalid, or the same is refused, and so are globs
  that match by the old name and an agent file that can't be edited, with the globs listed and
  nothing changed, while data left under the new name is not; a failed agent write is undone (the
  agents, the row, and the running toolset are as before); a failed toolset is renamed and started
  once it validates, and one that lost its name to `giles.toml` leaves the instance's globs alone;
  the case of a name changes. The rename tests (`rename/tests.rs`) cover stored toolsets in the
  startup check and the offline rename of a stored toolset: the row and the agents' globs; the
  refusals; the case of a name; a failure undone with the row put back exactly; and a run that
  stopped after the row finished by running it again.
