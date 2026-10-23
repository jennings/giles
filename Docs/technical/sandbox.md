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

## Status

The trampoline, its spec, the environment scrubbing, and the macOS backend are complete. The Linux
backend is staged on purpose:

| Platform | Backend                       | Implemented by          |
| -------- | ----------------------------- | ----------------------- |
| Linux    | Landlock and seccomp          | TASKS.md 2.13 (pending) |
| macOS    | Seatbelt through sandbox-exec | TASKS.md 2.14 (done)    |

Until the Linux backend lands, its `apply` returns `SandboxError::NotImplemented`, and the
trampoline exits with status 125 and an error naming the missing sandbox and the task, without
running the command. It never pretends to sandbox. The only way to run a command on Linux today is
a spec with `"unsandboxed": true`, which is PLAN section 12's per-instance opt-out and logs a
warning.

## Steps

`giles_core::sandbox::exec(spec, command)`, called by the subcommand:

1. Load the spec (`Spec::load`): parse the JSON (unknown fields are errors) and validate it.
2. Check the command: it must be an absolute path. The core resolves it first (2.19 resolves a
   toolset's `command` with the operator's `PATH`), so the file granted is the file run.
3. Check that the data directory is a directory and not a symbolic link (`lstat`; see
   [Writable roots and links](#writable-roots-and-links)), and create `tmp/` in it (mode 0700) if
   missing.
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
| 125    | `EXIT_TRAMPOLINE_FAILED` | The trampoline failed before `exec`: unreadable or invalid spec, relative command, unusable data directory (including one that is a symbolic link), sandbox not applied (including `NotImplemented`, writable grants whose links keep changing, and, from 2.13, `min_abi`), or an invalid `GILES_LOG`/`RUST_LOG` filter |
| 126    | `EXIT_CANNOT_EXECUTE`    | `exec` found the command but couldn't run it (e.g. not executable) |
| 127    | `EXIT_NOT_FOUND`         | `exec` didn't find the command                               |
| 2      |                          | Command-line usage error (from `clap`); a core bug           |
| 71     |                          | macOS: `sandbox-exec` couldn't apply the profile or `exec` the command (below) |
| other  |                          | The plugin's own exit status                                 |

A 125 means configuration or host trouble: restarting with backoff won't fix it, so 2.17 should
log it and not retry until the configuration (or the binary) changes. 126 and 127 point at the
plugin binary. A plugin that itself exits 125 to 127 is indistinguishable, as with `env(1)`; the
trampoline's stderr line (`Error: ...`) tells them apart in the log.

On macOS, `sandbox-exec` runs the command, and it exits with status 71 (`EX_OSERR`) when it can't
apply the profile (`sandbox-exec: sandbox_apply: ...`, e.g. inside another sandbox that forbids
it) or can't `exec` the command (`sandbox-exec: execvp() of '...' failed: ...`). The backend
therefore checks the command first (exists, is a regular file, `access(X_OK)`) and fails with 126
or 127 like the Linux path. A 71 remains possible when applying fails, when the profile itself
forbids the `exec` (a script whose interpreter has no `execute` grant), or when the command
changes between the check and the `exec`. 2.17 should treat it like 125. (`sandbox-exec` exits 65
for a profile it can't parse, which would be a bug in the generator.)

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
  "unix": [],
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
| `unix`         | `[]`               | Unix sockets the plugin may connect to. Non-empty also allows `socket(AF_UNIX, ...)` on Linux (below). `socketpair` is always allowed. |
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
restrictive values, so a field the core forgets denies rather than allows. The config's
`sandbox.unix` list goes to `unix` unchanged.

`unix` is enforced differently per platform. Linux (2.13): the seccomp filter allows the `AF_UNIX`
family iff the list is non-empty; Landlock can only restrict connecting to a named socket by path
from ABI 9, so below that a plugin with any `unix` grant can connect to every socket it can reach.
macOS (2.14): the Seatbelt profile allows connecting to exactly the listed paths.

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

## macOS: Seatbelt

Code: `sandbox/macos.rs` (backend, host check), `sandbox/macos/profile.rs` (the rules),
`sandbox/macos/paths.rs` (real paths). Tests: unit tests there, and
`crates/giles/tests/sandbox_macos.rs`, which runs probes (`/bin/sh`, `/bin/cat`, `/usr/bin/nc`, and
the test binary itself as a Rust program) under the real `giles sandbox-exec` and checks each grant
from both sides.

`apply` checks the command (see [Exit Status](#exit-status)), generates a profile for the spec, and
returns `/usr/bin/sandbox-exec -p <profile> <program> <args>`. `sandbox-exec` applies the profile
to itself and `exec`s the program, so the pid, fds 0 to 3, the scrubbed environment, and the
working directory all pass through, and every child the plugin starts inherits the profile. The
profile is an argument, so `ps` shows it; it holds paths, never secrets.

### Paths

Seatbelt matches the path the kernel resolved: every symbolic link followed, each component in
the spelling stored on disk. `literal` and `subpath` compare bytes, so a grant written `/tmp/x`, or
`/Users/Me` on a case-insensitive volume, would never match. Every path in the profile comes from
`paths::resolve`, one walk per path that is the only source of its result: it opens each
component relative to the previous one without following links (`openat` with `O_NOFOLLOW` and
`O_EVTONLY`, which needs no read permission), reads each link it meets itself and continues with
the target, and takes each directory's real path from the opened descriptor (`F_GETPATH`), which
also gives the stored spelling (`/tmp` becomes `/private/tmp`, `/var/folders` becomes
`/private/var/folders`). A missing component and the rest of the path are appended as spelled, so
a grant that doesn't exist yet still names where it will appear. Grants are resolved again at
every launch, which is what makes links dangerous (see
[Writable Roots and Links](#writable-roots-and-links)).

Reaching a path also takes `lstat` on each component on the way, including links (`/tmp`,
`/opt/homebrew/bin/node`), and `getcwd` walks up from the data directory. The walk records what it
looked up, and the profile allows `file-read-metadata` on exactly those paths (`literal`, never
`subpath`), plus `/etc` and `/var`.

Paths are SBPL string literals: `"` and `\` are escaped with `\`, and every other byte (newlines,
non-UTF-8) stands for itself; tests run `sandbox-exec` with data directories named like
`x")(allow default)(literal "` to prove a path can't add rules. The profile is built as bytes, so
non-UTF-8 paths work.

### Writable Roots and Links

Resolving at launch means a link decides what the launch grants. The plugin can create links
anywhere it can write, so a link it planted on one run would choose its grants on the next, and a
child it left running (children can outlive the plugin) can flip one while the next launch
resolves. Without the rules below, a plugin could replace its data directory with a link to `~`
or `GILES_HOME`, exit, and after the restart read `secrets.toml` and rewrite `giles.toml`. Three
rules close it:

- The writable roots themselves (the data directory and each `write` grant) can't be removed,
  renamed, or replaced: after the rule that makes them writable, the profile has
  `(deny file-write-unlink file-write-create (literal <root>))` for each, and later rules win.
  Everything inside them stays writable.
- The trampoline refuses a data directory that is a symbolic link (`lstat`, exit 125), on every
  platform, before creating `tmp/` in it.
- No link at or below a writable root is ever followed. Each path is resolved by one walk that
  stops following links once it reaches a root: the rest of the path is appended as spelled. A
  `unix` grant `<data>/agent.sock` that the plugin replaced with a link to `docker.sock` therefore
  still grants `<data>/agent.sock`, which the kernel resolves to `docker.sock` and Seatbelt then
  denies. A link planted at any time fails closed the same way, and so does a wrongly cased
  spelling below a root (`<data>/Notes` for a stored `notes`): write grants there with the stored
  case.

The roots are resolved first (`writable_roots`). A root inside another root (a `write` grant in the
data directory) mustn't follow links in the outer one, but which roots contain which is known only
once they're resolved, so they're resolved repeatedly, each round adding as a stop every root that
some root's walk followed a link inside, until no walk does. That takes at most one round per
root; a set that doesn't settle (links changing during the rounds) is refused with
`SandboxError::WritableLink` (exit 125). Every other path is then walked once with all roots as
stops.

The check and the resolution must be one step. An earlier version checked each path for planted
links and then resolved it again with `realpath`. A racing child flipping a link between the two
got an outside directory granted on about a third of launches. The integration test
`a_racing_link_never_redirects_a_grant` reproduces that race against the real trampoline.

Links outside the writable roots (`/tmp`, `/opt/homebrew/bin/node`, a link the operator made) are
followed. A grant changed later by someone other than the plugin is the operator's business, as
the user guide says.

Note for 2.13: Landlock's `path_beneath` rules have the same class of bug, because opening each
grant's file descriptor follows links at launch, and the same race. Linux needs the same design:
refuse a data directory that is a link (shared, already in `prepare_data_dir`); resolve each path
once, opening components with `openat(O_PATH | O_NOFOLLOW)` and following no link at or below a
writable root, and build the rule from that descriptor (not from a path resolved again); and stop
the plugin from replacing the roots themselves. A Landlock grant on a directory doesn't cover the
directory's own entry in its parent (removing or renaming it needs rights on the parent), so that
last part may hold already; verify it with a test like `data_dir_cannot_be_swapped_for_a_link`.

Note for 2.16 and 2.17: a plugin's children can outlive it, which is what makes the race above
reachable. Stopping or restarting a plugin must kill its whole process tree (e.g. start it in its
own process group and signal the group), not just the plugin's pid. A child can leave the group
with `setsid`, so a complete answer needs more (on Linux a cgroup; on macOS, e.g., a dedicated
user per plugin); the resolution above doesn't depend on it.

### Rules

The profile is `(version 1)`, `(deny default)`, then these rules, in this order (a later rule wins
over an earlier one). Every rule except
`process-fork` has filters, and a rule whose filter list would be empty (no `unix` grants, network
off) is left out, because an `(allow <operation>)` without filters allows it everywhere. Each system
entry was kept only if a probe (`/bin/sh`, Homebrew Python and Node, the `giles` binary) failed
without it, or because PLAN section 12.4 lists it as a default; the reason is next to each.

| Rule | Allows | Why |
| ---- | ------ | --- |
| `process-exec` | `(literal <command>)`, `(subpath <execute>)` | Running the command and programs under `execute` grants; nothing else, not even `/bin/sh` |
| `process-fork` | (unfiltered) | Shells and runtimes fork for subprocesses (`/bin/sh` and Python's `subprocess` fail without it); children inherit the profile |
| `signal` | `(target same-sandbox)` | The plugin and its children; separate `sandbox-exec` runs are separate sandboxes, so other plugins, the core, and everything else are out of reach. `(target self)` would stop a plugin from stopping its own children |
| `sysctl-read` | `hw.*` | Rust's `std` aborts without `hw.pagesize_compat`, Node aborts without `hw.ncpu`, Python's `os.cpu_count()` is `None` |
| `sysctl-read` | `kern.hostname`, `kern.osrelease`, `kern.ostype`, `kern.version` | `uname`; Node aborts without any of them |
| `file-read*` | `(literal "/")` | dyld opens `/` and aborts every process without it; lists only the root directory |
| `file-read*` | `/usr/lib`, `/usr/share`, `/System/Library` | System libraries (a PLAN default): `dlopen`, frameworks and their resources, locales, terminfo |
| `file-read*` | `/private/etc/ssl` | TLS certificates (a PLAN default) for OpenSSL-based runtimes |
| `file-read*` | `/private/etc/localtime`, `/private/var/db/timezone` | Local time; runtimes fall back to UTC without them |
| `file-read*` | `/private/etc/hosts`, `/private/etc/resolv.conf`, `/private/var/run/resolv.conf` | DNS configuration (a PLAN default) for runtimes with their own resolver; `getaddrinfo` doesn't need them |
| `file-read*` | `/private/etc/services`, `/private/etc/protocols` | `getservbyname` and `getprotobyname` fail without them |
| `file-read*` | `/dev/random`, `/dev/urandom` | Reading randomness from the devices fails without them |
| `file-read*` | `/private/var/select/sh` | `/bin/sh` reads it to choose the shell and prints an error on every start without it |
| `file-read* file-write-data` | `/dev/null` | Redirects to and from `/dev/null` |
| `file-read-metadata` | `/etc`, `/var`, and the lookup paths above | Time zones and name resolution go through the `/etc` and `/var` links (they fail without them); `getcwd` and links in grants need the rest |
| `file-read*` | `(literal <command>)`, `(subpath <read>)`, `(subpath <execute>)` | The plugin binary and the grants |
| `file-read* file-write*` | `(subpath <data_dir>)`, `(subpath <write>)` | The data directory (with its `tmp/`, the plugin's `TMPDIR`) and `write` grants |
| `deny file-write-unlink file-write-create` | `(literal <data_dir>)`, `(literal <write>)` | The roots themselves can't be removed, renamed, or replaced ([Writable Roots and Links](#writable-roots-and-links)) |
| `network-outbound` | `(remote ip "*:*")` for `network = true`, else `(remote tcp "*:<port>")` per granted port | IPv4 and IPv6 outbound; a port list allows TCP to those ports and no UDP |
| `network-outbound` | `(remote unix-socket (path-literal "/private/var/run/mDNSResponder"))` with any network grant | `getaddrinfo` resolves every name that isn't in `/etc/hosts` through mDNSResponder's socket, not a Mach service; without it every lookup fails |
| `network-outbound` | `(remote unix-socket (path-literal <unix>))` per `unix` grant | Connecting to exactly those sockets |

Everything else is denied, notably:

- Mach lookups: the allowlist is empty, because nothing a plugin needs failed without one. No
  LaunchServices (`com.apple.coreservices.launchservicesd`, so no opening URLs or apps outside the
  sandbox), no pasteboard, no `com.apple.SecurityServer` or `trustd` (the SDK uses `rustls` with
  bundled roots), no `notification_center`, `logd`, `opendirectoryd` (`getpwuid` fails; `HOME` is
  set), or `configd` (no system proxy settings).
- `network-inbound` and `network-bind`: a plugin can't listen on any port or create a Unix socket
  file, even in its data directory. The inherited fd 3 and `socketpair` need no rule.
- IPC (`ipc-posix-sem`, `ipc-posix-shm`): Python's `multiprocessing` locks don't work.
- IOKit, other sysctls, `process-info` on other processes, `/dev/tty`, `/private/etc/passwd`, and
  every other file.

A few denials are logged at each start without breaking anything: `/dev/dtracehelper`,
`/System/Cryptexes` and the dyld cache directory (dyld maps the shared cache without them), and
libSystem's startup sysctls (`kern.bootargs`, `kern.osvariant_status`,
`security.mac.lockdown_mode_state`, ...). They're visible with
`log show --predicate 'eventMessage CONTAINS "deny("'` and are expected.

Worth knowing, compared with Linux:

- A script's interpreter needs an `execute` grant like any other program (`#!/usr/bin/env node`
  needs `/usr/bin/env` and `node`); without one `sandbox-exec` exits 71.
- Port lists (`network = ["tcp:443"]`) are enforced on every macOS, where Linux needs Landlock
  ABI 4, and they allow no UDP.
- `unix` grants are enforced per path, where Linux below Landlock ABI 9 only allows or denies
  `AF_UNIX` as a whole.

The smoke check for this backend was Homebrew Python and Node under `giles sandbox-exec` with
`read` and `execute` set to `/opt/homebrew` and `network = true`: TLS context with certificates,
the local time zone, `os.cpu_count`, name resolution through mDNSResponder (`<host>.local`) and
`/etc/hosts` (`localhost`), a TCP connection to a local listener, SQLite and `tempfile` in the
data directory, and a subprocess all worked, and reading a file in the home directory was denied.

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

macOS: `/usr/bin/sandbox-exec` must be an executable file, and
`sandbox-exec -p '(version 1)(allow default)' /usr/bin/true` must succeed (it fails, for example,
when Giles itself runs inside a sandbox that forbids applying another; the error then carries
`sandbox-exec`'s status and stderr). That one process spawn is the only cost. The report is
`HostReport::Macos { sandbox_exec }`.

`giles serve` runs the check at startup, before `giles started`, and logs `plugin sandbox
available` with the report, or a warning `plugin sandbox unavailable on this host; sandboxed
plugins will not start` with the error. It's only a warning because no plugin is launched yet. The
plugin host (2.16) should call `check_host` once at startup and, when any configured instance is
sandboxed, refuse to launch those instances with this one error rather than letting each launch
fail in the trampoline.
