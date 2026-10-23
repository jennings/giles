# Toolsets

Code: `crates/giles-core/src/toolsets/` (`giles_core::toolsets`). Design: PLAN section 7.10. A
toolset is an MCP server; the [MCP client](mcp.md) is the connection, and this module decides
how to reach each configured `[toolset.<instance>]`.

## Connecting

`ToolsetLauncher::new(root, trampoline, sandbox)` mirrors the [plugin host](plugin-host.md)'s
`Launcher`: `trampoline` is the `giles` binary (run as `giles sandbox-exec`), and `sandbox` is the
host check `serve` runs once; an error refuses every sandboxed server with that message.
`.env(f)` replaces the process environment for `PATH` lookups (tests).

`connect(&ToolsetInstance)` returns an `McpToolset` once the server has answered `initialize`:

- **Stdio** (`command`, or the first-party plugin `giles-toolset-<component>`): `resolve` finds the
  program, then `launch` starts it in its sandbox (below) and hands the command to
  `McpToolset::connect_stdio` with the `env` values as the secrets to scrub.
- **Remote** (`url`): `McpToolset::connect_http` with the bearer token from `auth`, if any.
  Config loading accepts only bearer tokens for toolsets; OAuth client credentials are for remote
  channels.

Errors are `giles_types::Error`, prefixed `toolset <instance>:`. `InvalidRequest` means retrying
won't help (the host can't sandbox, an invalid spec, the sandbox trampoline failing at launch
with 125 or macOS's 71, rejected credentials); `Unavailable` covers the rest (program not found, a
server that fails to start or answer, an unreachable URL).

## Finding the Program

`command` is an absolute path or a name looked up on the operator's `PATH` (`config::find_program`,
the same lookup config loading uses to protect binary directories). Without `command`, the plugin
`giles-toolset-<component>` is found in `<root>/plugins/`, then on `PATH`
(`config::discover_plugin`). `ResolvedProgram` keeps both the path as found (`found`) and with
links resolved (`program`). The trampoline runs `program`, so the file granted is the file run,
and 2.20 keys the tool-list cache on its SHA-256.

## Grants

The sandbox spec is the configured grants plus what running the server needs (`toolsets/grants.rs`,
PLAN section 7.10). Most servers are scripts installed by a package manager:
`/opt/homebrew/bin/mcp-server-filesystem` links into `/opt/homebrew/lib/node_modules/...`, whose
`#!/usr/bin/env node` runs `/opt/homebrew/bin/node`. The plan:

1. Collect the binaries involved: `found`, `program`, and if `program` starts with `#!`, the
   interpreter (`/usr/bin/env`, or e.g. a venv's `python`) and, for `env [options] <name>`, the
   program `<name>` looked up on the operator's `PATH`. Each is added again with its links
   resolved, since the sandbox checks real paths (a venv's `python` usually links into a shared
   Python installation).
2. Grant each one's **install prefix**: the directory holding `bin/` or `sbin/` for a program in
   one (`/opt/homebrew`, the venv), else the program's own directory; never `/` (`/bin/sh` gets
   `/bin`). The prefix goes to both `read` and `execute`, since interpreters need an `execute`
   grant and their libraries need `read`.
3. Each grant passes the same rule as configured grants (`config::check_grant`): it may not
   overlap the data root (other than the instance's own data directory) or the socket directory.
   It also may not be the operator's home directory or contain it (the launcher's `HOME`, compared
   as written and with links resolved), whatever the data root: `~/bin/server` would otherwise
   grant all of `~`, including `~/.ssh`. A prefix that fails narrows to the program's directory,
   then to the file itself. A bundled plugin inside the data root gets no grant at all; the
   backends always let the command itself run.
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
whatever else lives there: `/opt/homebrew/etc` and `/opt/homebrew/var` for a Homebrew command,
all of `~/.local` for `~/.local/bin/x`, all of `~/.cargo` (including `credentials.toml`) for
`~/.cargo/bin/x`. Only the home directory itself (and `/`) is ruled out. Operators who mind
install servers in a prefix of their own, such as a venv or `npm --prefix`.

## The Launch

Like a plugin launch ([Plugin Host](plugin-host.md)), without the gRPC parts:

- Data directory `data/plugins/<instance>/` and its `tmp/`, mode 0700, refused if it is a link
  (`plugin_host::prepare_data_dir`). Instance names are unique across kinds, so toolsets share the
  plugin data root (PLAN section 11.6).
- The spec: `read` and `execute` from the plan (plus configured `read`), configured `write`,
  `unix`, `min_abi`, `network` (off unless granted: the config loader's toolset default),
  `inherit_env` = the names in `env`, and `path` from the plan. No `env`: MCP servers aren't Giles
  plugins and get no handshake variables.
- Sandboxed: the spec goes to `run/sandbox/<instance>.json` (0600, atomically), and the command is
  `giles sandbox-exec <spec> -- <program> <args>` with the `env` values set on the trampoline's
  process. The trampoline passes exactly those names on (the sandbox's
  [inherited variables](sandbox.md#environment)), so credentials never touch the spec file or
  `argv`.
- `unsandboxed = true`: the program runs directly, with a warning at every launch, the same
  scrubbed environment (`sandbox::scrubbed_env` with the `env` values and the daemon's `LANG` as
  the inherited lookup), and the data directory as the working directory.
- `McpToolset::connect_stdio` spawns it as the leader of its own process group; dropping the
  connection kills the group, and the server exiting takes the rest of the group with it
  ([MCP Client](mcp.md#stdio)). A server also exits when its stdin closes, which covers a core
  that dies without stopping it.

`env` may not set `HOME`, `TMPDIR`, `PATH`, or `GILES_PLUGIN_DATA`: the sandbox derives them
(`sandbox::DERIVED_ENV`), and config loading refuses them.

## Tests

- Unit: shebang parsing, install prefixes, the plan for a linked script run by `env`, an
  interpreter link into another prefix, a prefix containing the data root, the home directory
  (never granted, also through a link and with the data root elsewhere), and a bundled plugin; the
  spec built from a plan (credentials named, not written).
- `crates/giles-core/tests/toolsets.rs` (`harness = false`): the test binary is both the
  trampoline and the server, found by name on a `PATH` through a link. It reports its environment,
  working directory, and a TCP connect to a local listener on stderr. Sandboxed (Seatbelt on
  macOS, Landlock where the kernel has it; elsewhere the launch is refused with the host check's
  message): exactly the scrubbed variables plus the `env` names, the credential passed, the data
  directory as working directory, the network denied, and with a `tcp:<port>` grant connected; the
  spec file is 0600 and holds the grants and the credential's name but not its value.
  Unsandboxed: the same environment, network open. Remote: listing tools over streamable HTTP with
  the bearer token, and a wrong token refused as `InvalidRequest`.
