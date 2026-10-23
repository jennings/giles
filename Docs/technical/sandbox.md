# Plugin Sandbox

Code: `crates/giles-core/src/sandbox/` (`giles_core::sandbox`) and the `sandbox-exec` subcommand in
`crates/giles/src/sandbox_exec.rs`. Design: PLAN section 12.

The core never sandboxes its own multi-threaded process: Landlock and seccomp restrict only the
calling thread. It launches every plugin (gRPC plugins and stdio MCP toolsets) through a
trampoline, a fresh single-threaded `giles` process that applies the sandbox and then `exec`s the
plugin in place:

```text
giles sandbox-exec <spec> -- <command> [args...]
```

## Status: Staged Backends

The trampoline, its spec, and the environment scrubbing are complete. The platform backends are
staged on purpose:

| Platform | Backend                      | Implemented by |
| -------- | ---------------------------- | -------------- |
| Linux    | Landlock and seccomp         | TASKS.md 2.13  |
| macOS    | Seatbelt through sandbox-exec | TASKS.md 2.14  |

Until each lands, its `apply` returns `SandboxError::NotImplemented`, and the trampoline exits
with status 125 and an error naming the missing sandbox and the task, without running the command.
It never pretends to sandbox. The only way to run a command today is a spec with
`"unsandboxed": true`, which is PLAN section 12's per-instance opt-out and logs a warning.

## Steps

`giles_core::sandbox::exec(spec, command)`, called by the subcommand:

1. Load the spec (`Spec::load`): parse the JSON (unknown fields are errors) and validate it.
2. Check the command: it must be an absolute path. The core resolves it first (2.19 resolves a
   toolset's `command` with the operator's `PATH`), so the file granted is the file run.
3. Check that the data directory exists and create `tmp/` in it (mode 0700) if missing.
4. Build the scrubbed environment (below).
5. Apply the platform backend, or with `unsandboxed`, log
   `running unsandboxed: ...` at warn level and skip it.
6. `exec` the command (`std::os::unix::process::CommandExt::exec`) with `env_clear()`, the scrubbed
   environment, and the data directory as the working directory.

`exec` replaces the trampoline, so the plugin keeps its pid (the core's pid file and signals reach
it directly) and its exit status is what the core sees. On any failure the trampoline prints
`Error: ...` on stderr, which the core logs under the instance's span, and exits with a status
from the contract below.

The trampoline stays single-threaded: `main` is synchronous, only `serve` builds a Tokio runtime,
and the `tracing` subscriber writes to stderr synchronously. Keep it that way.

## Exit Status

Like `env(1)` and `chroot(1)`, so the plugin host (2.16) and supervision (2.17) can tell the
trampoline's failures from the plugin's own exits without parsing stderr. The codes are constants
in `giles_core::sandbox`, and `SandboxError::exit_code` maps an error to its code.

| Status | Constant                 | Meaning                                                      |
| ------ | ------------------------ | ------------------------------------------------------------ |
| 125    | `EXIT_TRAMPOLINE_FAILED` | The trampoline failed before `exec`: unreadable or invalid spec, relative command, unusable data directory, sandbox not applied (including `NotImplemented` and, from 2.13, `min_abi`), or an invalid `GILES_LOG`/`RUST_LOG` filter |
| 126    | `EXIT_CANNOT_EXECUTE`    | `exec` found the command but couldn't run it (e.g. not executable) |
| 127    | `EXIT_NOT_FOUND`         | `exec` didn't find the command                               |
| 2      |                          | Command-line usage error (from `clap`); a core bug           |
| other  |                          | The plugin's own exit status                                 |

A 125 means configuration or host trouble: restarting with backoff won't fix it, so 2.17 should
log it and not retry until the configuration (or the binary) changes. 126 and 127 point at the
plugin binary. A plugin that itself exits 125 to 127 is indistinguishable, as with `env(1)`; the
trampoline's stderr line (`Error: ...`) tells them apart in the log.

## File Descriptors

The trampoline doesn't touch file descriptors. Everything it inherits without `FD_CLOEXEC` reaches
the plugin: stdin (the token pipe), stdout (the handshake line), stderr, and fd 3 (the plugin's end
of the socket pair, PLAN section 7.4). The spec file and anything else the trampoline opens use
`O_CLOEXEC` (Rust's default), so they don't leak. The core is responsible for passing nothing else
without `FD_CLOEXEC`. The integration test hands the trampoline a file as fd 3 and checks that the
plugin writes to it, reads stdin, and writes to stdout and stderr.

## Spec Format

`giles_core::sandbox::Spec`, serialized as one JSON object. The spec is a file, not an argument,
so it isn't limited by `ARG_MAX` or visible in `ps`. 2.16 should write it with `Spec::to_json`
(which validates first) to a file outside every plugin's grants, e.g. `run/sandbox/<instance>.json`
with mode 0600, and pass its path.

```json
{
  "data_dir": "/home/me/.giles/data/plugins/irc",
  "read": ["/opt/homebrew"],
  "write": [],
  "execute": ["/opt/homebrew/bin"],
  "network": ["tcp:6697"],
  "unix_sockets": false,
  "min_abi": 4,
  "unsandboxed": false,
  "env": {"GILES_PLUGIN_MAGIC": "...", "GILES_PLUGIN_PROTOCOLS": "1"},
  "path": "/usr/bin:/bin"
}
```

| Field          | Default            | Meaning                                                             |
| -------------- | ------------------ | ------------------------------------------------------------------- |
| `data_dir`     | required           | The instance's data directory; must exist. Always writable.         |
| `read`         | `[]`               | Extra paths readable recursively.                                   |
| `write`        | `[]`               | Extra writable paths.                                               |
| `execute`      | `[]`               | Extra paths under which programs may run. The command always may.   |
| `network`      | `false`            | `false`: no IP sockets. `true`: all IP traffic. `["tcp:<port>"]`: TCP connects to those remote ports only. |
| `unix_sockets` | `false`            | Allow `socket(AF_UNIX, ...)`. `socketpair` is always allowed.       |
| `min_abi`      | unset              | Lowest Landlock ABI to accept (Linux; ignored on macOS).            |
| `unsandboxed`  | `false`            | Skip the backend, with a warning. The environment is still scrubbed. |
| `env`          | `{}`               | Variables to pass: `GILES_*` names and `LANG`.                      |
| `path`         | `/usr/bin:/bin`    | The command's `PATH`.                                               |

Validation (`Spec::validate`, no filesystem access):

- Every path (`data_dir`, grants, each `path` entry) must be absolute, without `..` components (they
  would defeat grant overlap checks) and without NUL bytes.
- `env` names are `LANG` or `GILES_` plus at least one of `A-Z`, `0-9`, `_`; `GILES_PLUGIN_DATA` is
  derived, so it's rejected. Values can't contain NUL.
- `network` grants are exactly `tcp:<port>` with a port from 1 to 65535. Duplicates collapse; an
  empty list means `false`. No grant allows binding a port: plugins never listen.
- `min_abi` is at least 1.

The spec is the trampoline's input, not configuration. 1.1 parses the `sandbox = { ... }` grants in
`giles.toml` and rejects grants that overlap `GILES_HOME` or the plugin directories (PLAN section
12.4). 2.16 and 2.19 build the spec from those grants plus the per-kind defaults: `network`
defaults to `true` for providers and channels and to `false` for stdio toolsets, and 2.19 adds a
toolset's install prefix to `read` and `execute`. The spec's own defaults are the most
restrictive values, so a field the core forgets denies rather than allows.

## Environment

`scrubbed_env(spec, inherited_lang)` is the plugin's whole environment (PLAN section 12.4):

| Variable            | Value                                                          |
| ------------------- | -------------------------------------------------------------- |
| `GILES_*`           | From `env`, e.g. `GILES_PLUGIN_MAGIC`, `GILES_PLUGIN_PROTOCOLS` |
| `GILES_PLUGIN_DATA` | `data_dir`                                                     |
| `HOME`              | `data_dir`                                                     |
| `TMPDIR`            | `data_dir/tmp`                                                 |
| `PATH`              | `path`, else `/usr/bin:/bin`                                   |
| `LANG`              | From `env`, else the trampoline's own `LANG`, else unset       |

Nothing else from the trampoline's environment reaches the plugin. In particular the core's
`GILES_HOME`, `GILES_LOG`, and any secrets in its environment don't. The token travels on stdin,
never in the environment.

## Backends

An internal trait in `sandbox/backend.rs`, one implementation per platform selected with `cfg`
(`sandbox/linux.rs`, `sandbox/macos.rs`; other platforms fail to compile):

```rust
pub(crate) struct Launch {
    pub program: PathBuf,     // absolute
    pub args: Vec<OsString>,  // without argv[0]
}

pub(crate) trait SandboxBackend {
    fn apply(spec: &Spec, launch: Launch) -> Result<Launch, SandboxError>;
}
```

`apply` runs in the trampoline after steps 1 to 4 and right before `exec`. On error nothing runs.
A backend either restricts the current process and returns `launch` unchanged (Linux: the Landlock
ruleset and then the seccomp filter, both inherited across `exec`), or returns a wrapped launch
(macOS: `/usr/bin/sandbox-exec -p <profile> <program> <args>`). Backends add their platform's
default read grants (the command, system libraries, TLS certificates, DNS config) themselves.
Backend-specific failures become new `SandboxError` variants (the enum is `#[non_exhaustive]`).

## Host Check

`giles_core::sandbox::check_host() -> Result<HostReport, HostError>` (backed by
`SandboxBackend::check_host`) tells whether this host can apply the sandbox at all, so a broken
host produces one clear error instead of every plugin failing (PLAN section 12.1). The probes are
real now; they change nothing and cost a few syscalls.

Linux, in order, stopping at the first failure:

1. Landlock: `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)` through
   `libc::syscall` returns the newest ABI. `ENOSYS` means the kernel is older than 5.13 or built
   without `CONFIG_SECURITY_LANDLOCK`, or that a container's seccomp profile blocks
   `landlock_create_ruleset` (profiles that deny unlisted syscalls with `ENOSYS`, like Podman's
   default, look exactly like a kernel without Landlock); `EOPNOTSUPP` means it's built in but
   not enabled (add `landlock` to the `lsm=` boot parameter); anything else (e.g. `EPERM` from a
   container's seccomp profile) is reported as a probe failure.
2. seccomp, the way systemd checks it: `prctl(PR_GET_SECCOMP)` fails with `EINVAL` without
   `CONFIG_SECCOMP` (mode 2, already filtered by a container runtime, is fine: filters stack); then
   `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, NULL)` fails with `EFAULT` when filters are supported
   and `EINVAL` when not, without installing anything.

The report is `HostReport::Linux { landlock_abi }`. It doesn't apply `min_abi`: that's per launch,
in the backend (2.13).

macOS: `/usr/bin/sandbox-exec` must be an executable file; the report is
`HostReport::Macos { sandbox_exec }`.

`giles serve` runs the check at startup, before `giles started`, and logs `plugin sandbox
available` with the report, or a warning `plugin sandbox unavailable on this host; sandboxed
plugins will not start` with the error. It's only a warning because no plugin is launched yet. The
plugin host (2.16) should call `check_host` once at startup and, when any configured instance is
sandboxed, refuse to launch those instances with this one error rather than letting each launch
fail in the trampoline.
