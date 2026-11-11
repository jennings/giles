# Docker environment

The design of `giles-environment-docker`, the first environment plugin (issue #25, built in
issue #90). It runs each workspace as a container plus a named volume, through the Docker Engine
API, and serves `giles.environment.v1` on the plugin SDK ([Plugin SDK](plugin-sdk.md)). User-facing
setup and settings are in [Docker environment](../user/docker-environment.md).

The crate is a library with a thin binary. `DockerEnvironment` implements `giles_types::Environment`
directly, so the SDK serves it with no adapter. `main` calls `EnvironmentPlugin::run`, which gives
the plugin both launched mode (`socket`) and remote mode (`serve`), and parses the `config` of the
instance with `Config` (`deny_unknown_fields`). Its settings come from `[environment.<name>.config]`
when launched, and from the `[config]` table of the `serve --config` file when remote.

The Engine API client is `bollard`, over the unix socket only (`pipe` feature, no TLS stack). It is
the one place the engine is named: `socket`, else the `unix://` path in `DOCKER_HOST`, else
`/var/run/docker.sock`. A launched plugin gets the sandbox's cleaned-up environment, so only `serve`
ever sees `DOCKER_HOST`. Docker, Podman's compatible API, Docker Desktop, OrbStack, and Colima all
answer the calls the plugin makes. The integration tests run on rootless Podman.

## State lives in labels

The plugin keeps nothing of its own, not even in its data directory. A restart, or a new plugin
process on the same engine, finds every workspace again from labels:

| Label | On | Meaning |
| --- | --- | --- |
| `giles.instance` | volume, container | The `[environment.<name>]` instance, so two instances on one engine never see each other's workspaces |
| `giles.workspace` | volume, container | The workspace id the core chose |
| `giles.image` | volume, container | The image as requested, normalized (`docker.io/library/ubuntu:24.04`) |
| `giles.image-digest` | volume, container | What the container is created from: the repository digest, or the image ID for an image with none (a local build) |
| `giles.image-id` | volume, container | The image ID at creation, which image removal uses |
| `giles.pulled` | volume, container | `true` if this plugin pulled the image |
| `giles.created` | volume, container | Creation time, RFC 3339 |

The volume is the source of truth, because it outlives the container: `list_workspaces` lists the
instance's volumes and pairs each with its container, if there is one. A volume without a container
is a stopped workspace that `start_workspace` can recreate. A container without a volume is never
listed as a workspace.

The volume and container are both named `giles-<instance>.<id>`. Ids contain no `.`, so the last `.`
separates the id and two instances can't produce the same name. Everything else is found by label,
never by name, and a result is checked against the labels again after the engine's filter.

Per-workspace calls hold an in-memory lock on the id (`start_workspace` takes it only when the
container isn't already running). It keeps two concurrent calls from both recreating a container,
and holds no state that matters after a restart.

## Creating a workspace

`create_workspace` takes `{ "image": "..." }` and nothing else (`deny_unknown_fields`, and the
schema says `additionalProperties: false`). It runs these steps and removes whatever it made if one
fails:

1. Parse the image and check the allowlist ([Images](#images)). A refusal lists the patterns.
2. Refuse an id that is in use with `InvalidRequest`, which is how the `Environment` trait reports
   what the protocol calls `ALREADY_EXISTS`.
3. Find the image on the engine, or pull it with the registry's credentials, then record its pinned
   name and ID.
4. Create the volume with the labels.
5. Create the container: `Init: true`, user `0`, working directory `/workspace`, the volume bound at
   `/workspace`, `HOME=/workspace/.home` and the configured `env`, the configured memory, CPU, and
   process limits, and the configured network. The model sets nothing else: there are no mounts,
   capabilities, devices, or privileged mode, and nothing is read from the options.
6. Copy the helper in with an archive upload, start the container, and run
   `mkdir -p /workspace/.home`. The `mkdir` is also the first use of `/bin/sh`, so an image without
   one fails here with `InvalidRequest`.

The container runs as root. On a rootless engine that is an unprivileged user on the host, and it
lets the model install packages and always own the volume, whatever user the image sets. The
entrypoint is the helper's `idle` subcommand, so the container stays up on any image, including one
whose own command exits at once. `Init: true` puts the engine's init process in front of it to reap
orphans.

## Start, stop, delete, list

`start_workspace` handles four cases:

- A running container is `Running`, after one list call and no lock.
- A stopped container is started, or resumed if paused: `Started`, with a detail that processes have
  ended and files and packages are intact.
- No container but a volume: the container is recreated from the pinned image. If the image is gone
  from the engine, it is pulled again by digest. An image with no repository digest can't be pulled
  again, and the error says so. `Recreated` has a detail that files under `/workspace` are intact
  and anything installed elsewhere is gone.
- Neither is `NotFound`.

`stop_workspace` stops the container with a five second grace period and succeeds if it already is.
The helper's `idle` process has no signal handlers, so SIGTERM ends it at once.

`delete_workspace` removes the container (with its anonymous volumes), then the volume, then
[removes the image if it can](#removing-images). Each step ignores a missing resource, so a delete
that failed halfway can be repeated.

`list_workspaces` reports state from the container, the volume's size from the engine's disk usage
report when it gives one, and the free space where volumes live. The Engine API has no call for free
space, so the plugin runs the helper's `df /workspace` in a running workspace and reports `None`
when none is running.

## Images

The allowlist and the reference parser are pure functions in `image.rs`.

A reference normalizes the way Docker's does:

- The registry defaults to `docker.io`, and `index.docker.io` and its aliases fold into it.
- A single-component Docker Hub name is under `library/`.
- The first component is a registry only if it contains `.` or `:` or is `localhost`.
- The tag defaults to `latest`, unless there is a digest.

A pattern uses the same syntax and normalization. A `*` is accepted only as the whole tag, and a
pattern can't pin a digest. A reference pinned by digest (`name@sha256:...`) matches a pattern whose
tag is `*` and no other, because a digest can't be compared with a tag, even if the reference also
names one. The built-in list is `ubuntu`, `debian`, `alpine`, `python`, `node`, `golang`, `rust`,
`ruby`, and `eclipse-temurin`, each with `*`. `default_images = false` drops it.

The workspace pins the image when it is created. The pinned name is the repository digest the engine
lists for the requested repository (Docker lists Hub images without the `docker.io/library/`
defaults and Podman with them, so both sides are normalized before comparing), or the image ID. A
recreated container starts from that, so a moving tag such as `latest` can't change the software
under the agent in the middle of a conversation.

### Removing images

An image is removed after a delete only if all of these hold:

- The workspace's `giles.pulled` is `true`. A workspace created from an image that an earlier
  workspace of the same instance pulled inherits `true`, so the last one out removes it.
- No remaining volume of the instance records the same image ID.
- The engine allows it. Removal is by image ID without `force`, so the engine refuses an image that
  a container (of anything) uses or that has other names, and the plugin leaves it.

An image that was already on the engine is never removed, and neither is one pulled by another
instance. A failed create removes the image that attempt pulled by the same rule.

## The helper

`giles-workspace-helper` (crate `crates/giles-workspace-helper`) is a small static Linux binary that
the plugin copies to `/.giles/bin/giles-workspace-helper` in every container. Its name doesn't start
with `giles-environment-`, which would make plugin discovery treat it as a plugin. It has no async
runtime. It exists because the container can't be assumed to have anything but `/bin/sh`, and
because the Engine API can't kill an exec.

| Subcommand | Purpose |
| --- | --- |
| `idle` | The container's main process |
| `run --id ID -- CMD...` | The process-group wrapper |
| `kill --id ID` | Kill the group of `run --id ID` |
| `find`, `grep` | Search with `globset`, `ignore`, and `regex`; JSON lines out, ending in `{"truncated":bool}` |
| `read`, `write` | Whole-file access; `read` prints `{"size":N}` and then up to `--max` bytes |
| `df` | Free bytes where a path lives |

`find` and `grep` walk with `ignore` configured to skip `.git` and honor `.gitignore` files whether
or not a `.git` exists, include dotfiles, and sort the paths as strings so the order doesn't depend
on directory layout. `grep` skips files with a NUL byte in the first 8 KiB. Failures print a message
on standard error and exit 1, or 66 for a missing path, and the plugin turns those into
`InvalidRequest` and `NotFound`. `write` reads all of standard input before it opens the file, so a
broken transfer doesn't truncate it, and it opens the existing file in place, which keeps its
permissions.

### Finding and shipping the helper

The helper is built for the engine's architecture, which can differ from the plugin's (a plugin on
macOS driving a Linux VM). At connect time the plugin reads the engine's `info.Architecture`, maps
`amd64` and `x86_64` to `x86_64` and `arm64` and `aarch64` to `aarch64`, and loads
`<helper_dir>/giles-workspace-helper-<arch>`, where `helper_dir` defaults to the directory of the
plugin's own executable. A missing file fails the plugin's configure step, with a message that names
the path and the `cargo build --target <arch>-unknown-linux-musl` command that makes it.

The linker for the musl targets is `rust-lld` (`.cargo/config.toml`), and both targets are in
`rust-toolchain.toml`, so the cross build needs no C toolchain. The
[container image](container-image.md) builds the helper for its own architecture and installs it
next to the plugin. The archive upload that copies it into a container also creates `/.giles/run`
with mode 1777, so a command that runs as another user can still record its group.

## Running commands

Every command runs as one exec of
`/.giles/bin/giles-workspace-helper run --id <uuid> -- /bin/sh -c <command>`, in `/workspace`, with
no TTY and standard input closed. The id is a fresh UUID, not the engine's exec id, because the
plugin needs it before the exec exists.

`run` starts the command in a new process group and writes the group id to `/.giles/run/<id>`. It
gives the command's output pipes to itself and copies them to its own standard output and standard
error, which the engine multiplexes back to the plugin. It exits with the command's status, or 128 +
N when signal N ended it, and removes its record. It doesn't wait for background processes that
still hold the output pipes, because it stops reading once the command has exited and the pipes are
empty: `sleep 300 &` in a command doesn't hold the call open. Those processes keep running, which is
how a model starts a server.

### Cancelling

The Engine API has no call that kills an exec, so cancellation is a second exec, `kill --id <uuid>`,
which sends SIGKILL to the group recorded for that id. Anything that stayed in the group dies, and
`run` reports 137. A process that moved itself to another group or session with `setsid` escapes,
and so does one that was already running before the command, such as a daemon.

Two things trigger a kill:

- The timeout. The plugin runs `kill`, marks the exit `timed_out`, and keeps draining output for up
  to ten seconds. The exit code is whatever the engine recorded, normally 137.
- Dropping the stream, or the future that creates it. A guard that is armed before the first request
  goes out spawns the kill when it drops, unless the exit event was already produced, so cancelling
  at any point in the setup still kills the command.

`kill` can run before `run` has recorded the group: the cancellation arrives while the exec is still
starting. `run` writes its record first and then looks for a marker file `<id>.kill`, and `kill`
looks for the record and, if there is none, writes the marker and looks again. Each side writes
before it reads, so at least one sees the other. If `kill` sees the record the second time it
removes its marker, and if `run` sees the marker it kills its own group at once. A `kill` for a
command that already finished leaves a marker nothing reads, a few bytes in the container's writable
layer, which is why the plugin only kills a command whose exit it hasn't produced.

### Output limit and exit status

The plugin forwards output pieces as they arrive. It counts the bytes of standard output and
standard error together, sends up to `output_limit`, and reports the rest as `dropped_bytes` in the
exit event, which is always the last event. The exit code comes from inspecting the exec after the
output ends. The engine can take a moment to record it, so the plugin polls for up to five seconds.
A zero timeout is `InvalidRequest`.

## Files

`read_file`, `write_file`, `find`, and `grep` are helper calls under the same wrapper with a 60
second timeout, so a runaway search is killed like a command. File content goes through standard
input and output, not the Engine API's archive calls, because the helper handles symlinks, modes,
and missing parent directories the same way for every image and the archive API doesn't. A path that
starts with `-` gets a `./` prefix so the helper never reads it as a flag. Relative paths need no
handling, because every exec starts in `/workspace`.

## Tests

The unit tests cover the reference parser, the allowlist, the configuration, the helper archive, and
the helper's output parsing. `crates/giles-workspace-helper/tests/cli.rs` runs the helper binary on
the host. The integration tests in `plugins/giles-environment-docker/tests/` run against a real
Docker API socket: `docker.rs` calls `DockerEnvironment` directly, and `serve.rs` runs the binary
with `serve --config` and drives it over gRPC. They skip, printing why, when no socket answers
(`GILES_REQUIRE_DOCKER=1` makes that a failure, as in [CI](ci.md)). The helper comes from
`GILES_HELPER_DIR` or is cross-built for the engine's architecture.
