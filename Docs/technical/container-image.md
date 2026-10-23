# Container Image

The `Dockerfile` at the repository root builds a Linux image with `giles` and the first-party
plugins (PLAN section 14.2); [Running in a Container](../user/container.md) covers using it. It is a
two-stage BuildKit build: `docker build .` works on any machine with Docker, and nothing is built on
the host first.

## Build Context

`.dockerignore` is an allowlist: the workspace manifests and lockfile, `rust-toolchain.toml`,
`crates/`, `plugins/`, `proto/`, and the license files. Everything else stays out, including
`target/`, `.jj/`, `.git/`, `spikes/` (not workspace members), the docs, and the CI files, so
changes to those don't invalidate the build's layer cache.

## Build Stage

The build stage starts from the official `rust:1-bookworm` image: Debian bookworm, the same glibc as
the runtime stage, with the C toolchain that `aws-lc-sys` (rustls' crypto) and the bundled SQLite
need. It copies `rust-toolchain.toml` alone and runs `rustup toolchain install`, which installs the
pinned toolchain from it, so the version lives only in that file and the toolchain layer is rebuilt
only when the pin changes. Protobuf code generation uses the vendored `protoc` (see
[Cargo Workspace](workspace.md)); no system `protoc` is installed.

It then copies the context and runs one `cargo build --release --locked` for the binaries the image
ships: `giles`, `giles-channel-irc`, and `giles-channel-email`. `giles-channel-echo` is a test-only
plugin and stays out. The list is the `bins` variable in that `RUN` step; a new first-party plugin
is added there.

Three BuildKit cache mounts keep rebuilds incremental: the Cargo registry and git checkouts
(`/usr/local/cargo/registry` and `/usr/local/cargo/git`) and `target/`. Each has a fixed `id` and
`sharing=locked`, so concurrent builds wait for each other instead of corrupting a shared target
directory. A cache mount isn't part of the layer, so the step copies the binaries out to `/out`,
which the runtime stage copies from.

## Runtime Stage

The runtime stage is `debian:bookworm-slim` with three packages:

- `ca-certificates` and `tzdata`, for plugins and MCP servers that use the system's roots and time
  zone database. (Giles' own TLS clients trust the bundled Mozilla roots, and `jiff` falls back to a
  built-in copy of the time zone database.)
- `tini`, which runs as PID 1. Plugins and MCP servers run in their own process groups, and Giles
  kills a whole group when it stops one; descendants it didn't spawn itself are reparented to
  PID 1, and `giles` as PID 1 wouldn't reap them, so they would stay zombies. `tini` reaps them and
  forwards signals, so `docker stop`'s SIGTERM still reaches `giles`.

The binaries go in `/usr/local/bin/`, where plugin discovery finds them on `$PATH`
([Plugin Host](plugin-host.md)), so the data root holds only data. The license texts go in
`/usr/share/doc/giles/`.

The image runs as `giles`, uid and gid 10001: a fixed id, so a bind-mounted data root can be
`chown`ed to it, outside the ranges of system accounts and of the first login accounts. `/data` is
created owned by it with mode 0700 and declared a `VOLUME`; `GILES_HOME=/data` points Giles at it,
and it is also the working directory. A fresh named volume copies that ownership from the image.

`EXPOSE 8080` documents the web port; `giles.toml` must still set `http_listen = "0.0.0.0:8080"`,
since the default listens on loopback only. The entrypoint is `tini -- giles` with `serve` as the
default command, so the container runs `giles serve`, and arguments after the image name replace
the subcommand (`docker run --rm <image> --version`).

The OCI labels name the source repository (which links a GitHub Container Registry package to it),
the title, a description, and the license, `MIT OR Apache-2.0`.

## The Sandbox in a Container

The plugin sandbox needs no extra privileges: Docker's default seccomp profile allows the
`landlock_*` syscalls and installing seccomp filters, and Landlock needs no capabilities. Landlock
comes from the host's kernel, which the startup check reports ([Plugin Sandbox](sandbox.md)).
Apple's `container` VMs, used for local Linux checks, have no Landlock, and the check reports that.

## Not Yet

- A `minimal` image with `giles` alone, as a second final stage.
- The WhatsApp plugin, added to `bins` when it lands.
