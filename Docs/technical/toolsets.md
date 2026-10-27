# Toolsets

Code: `crates/giles-core/src/toolsets/` (`giles_core::toolsets`). Design: PLAN sections 7.5, 7.6,
7.9, and 7.10. A toolset is an MCP server; the [MCP client](mcp.md) is the connection, and this
module decides how to reach each configured `[toolset.<instance>]` (`ToolsetLauncher`) and keeps it
going for the life of the daemon (`ToolsetHost`).

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
2. Registers a `ManagedToolset` with `Registry::register_toolset`, once, for the life of the daemon.
   It never changes, so the dispatcher needs nothing beyond the registry.
3. Spawns the instance's supervisor task (`toolsets/lifecycle.rs`, under a `toolset{instance}`
   span), which owns the connection and makes the first start.

`started()` waits for every first start to succeed or fail; `serve` waits for it alongside the
plugin launches before logging `giles started`. `stop()` cancels the supervisors and waits for them,
dropping every connection (a stdio server's process group is killed).

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
| `Stopped` | (ends the task)                               | fails: Giles is stopping                |

Every start lists the tools, offers them, and stores them in the cache. A start whose listing fails
keeps offering the previous list.

**Crashes and failed starts** (PLAN section 7.5): the server exiting (with its status), a lost
connection, or a start that fails with a retryable error leads to `Down` with a restart after
`RESTART_BACKOFF_MIN` (1 s), doubling up to `RESTART_BACKOFF_MAX` (5 min); a connection that lasted
`HEALTHY_RESET` (10 min) starts the backoff over. A non-retryable failure (`InvalidRequest`: the
host can't sandbox, the sandbox trampoline failed with 125 or macOS's 71, rejected credentials, no
shared protocol) isn't retried until Giles restarts, and the instance's tools are withdrawn, cached
or not: every call would fail. While a stdio server will be retried, its tools stay offered, and
calls fail with the reason and when the next attempt is due, as for a server that's restarting.

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
  cleanly.
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
