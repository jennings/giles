# Container image

The `Dockerfile` at the repository root builds a Linux image with `giles` and the first-party
plugins (PLAN section 14.2); [Running in a container](../user/container.md) covers using it. It is a
two-stage BuildKit build: `docker build .` works on any machine with Docker, and nothing is built on
the host first.

## Build context

`.dockerignore` is an allowlist: the workspace manifests and lockfile, `rust-toolchain.toml`,
`crates/`, `plugins/`, `proto/`, and the license files. Everything else stays out, including
`target/`, `.jj/`, `.git/`, `spikes/` (not workspace members), the docs, and the CI files, so
changes to those don't invalidate the build's layer cache.

## Build stage

The build stage starts from the official `rust:1-bookworm` image: Debian bookworm, the same glibc as
the runtime stage, with the C toolchain that `aws-lc-sys` (rustls' crypto) and the bundled SQLite
need. It copies `rust-toolchain.toml` alone and runs `rustup toolchain install`, which installs the
pinned toolchain from it, so the version lives only in that file and the toolchain layer is rebuilt
only when the pin changes. Protobuf code generation uses the vendored `protoc` (see
[Cargo workspace](workspace.md)); no system `protoc` is installed.

It then copies the context and runs one `cargo build --release --locked` for the binaries the image
ships: `giles`, `giles-channel-irc`, `giles-channel-email`, `giles-channel-whatsapp`, and
`giles-channel-discord`. `giles-channel-echo` is a test-only plugin and stays out. The list is the
`bins` variable in that `RUN` step; a new first-party plugin is added there.

Three BuildKit cache mounts keep rebuilds incremental: the Cargo registry and git checkouts
(`/usr/local/cargo/registry` and `/usr/local/cargo/git`) and `target/`. Each has a fixed `id` and
`sharing=locked`, so concurrent builds wait for each other instead of corrupting a shared target
directory. A cache mount isn't part of the layer, so the step copies the binaries out to `/out`,
which the runtime stage copies from.

`whatsapp-rust`, the library under the WhatsApp plugin, is the one dependency that doesn't come from
crates.io: it's a git dependency pinned to a commit by `Cargo.toml` and `Cargo.lock`
([WhatsApp Channel](whatsapp.md#the-library-and-why-its-a-git-dependency)). `--locked` still
applies, and Cargo fetches it from GitHub with its built-in git support, so the build stage needs
network access to `github.com` and no `git` binary. The git cache mount keeps the checkout between
builds. A from-scratch release build of the plugin alone took about two minutes on a 10-core arm64
Mac.

The cache mounts live in the builder, so they help repeated local builds. In CI they don't: the
GitHub Actions cache (`type=gha`) saves layers, not cache mounts, and each run starts a fresh
builder, so whenever the sources change the Rust build step rebuilds every crate from scratch. Only
a push that leaves the build context unchanged skips it, through the cached layer. Persisting the
mounts across runs (for example by exporting them into the Actions cache) is a later improvement.

## Runtime stage

The runtime stage is `debian:bookworm-slim` with three packages:

- `ca-certificates` and `tzdata`, for plugins and MCP servers that use the system's roots and time
  zone database. (Giles' own TLS clients trust the bundled Mozilla roots, and `jiff` falls back to a
  built-in copy of the time zone database.)
- `tini`, which runs as PID 1. Plugins and MCP servers run in their own process groups, and Giles
  kills a whole group when it stops one; descendants it didn't spawn itself are reparented to PID 1,
  and `giles` as PID 1 wouldn't reap them, so they would stay zombies. `tini` reaps them and
  forwards signals, so `docker stop`'s SIGTERM still reaches `giles`.

The binaries go in `/usr/local/bin/`, where plugin discovery finds them on `$PATH`
([Plugin host](plugin-host.md)), so the data root holds only data. The license texts go in
`/usr/share/doc/giles/`.

The image runs as `giles`, uid and gid 10001: a fixed id, so a bind-mounted data root can be
`chown`ed to it, outside the ranges of system accounts and of the first login accounts. `/data` is
created owned by it with mode 0700 and declared a `VOLUME`; `GILES_HOME=/data` points Giles at it,
and it is also the working directory. A fresh named volume copies that ownership from the image.

The WhatsApp plugin is in the image but inert until an instance of component `whatsapp` is
configured: the core launches a plugin only for a configured instance, so the smoke test (which
configures none) never starts it, never opens a connection to WhatsApp, and only checks that the
binary is there. A configured instance keeps its session in `/data/data/channel/<instance>/`, on the
volume, and needs outbound HTTPS only.

The Discord plugin is the same: in the image, inert until an instance of component `discord` is
configured, so the smoke test only checks that the binary is there and never connects to Discord. A
configured instance keeps its catch-up state (`discord-state.json`) in
`/data/data/channel/<instance>/` and needs outbound TCP 443 only (REST, the Gateway, and the CDN).

`EXPOSE 8080` documents the web port; `giles.toml` must still set `http_listen = "0.0.0.0:8080"`,
since the default listens on loopback only. The entrypoint is `tini -- giles` with `serve` as the
default command, so the container runs `giles serve`, and arguments after the image name replace the
subcommand (`docker run --rm <image> --version`).

The OCI labels name the source repository (which links a GitHub Container Registry package to it),
the title, a description, and the license: `(MIT OR Apache-2.0) AND AGPL-3.0-only`. The repository
is `MIT OR Apache-2.0`, except `plugins/giles-channel-whatsapp`, which is `AGPL-3.0-only`
([Licensing](whatsapp.md#licensing)); the image ships that binary, so its label names both. The CI
label override in `ci.yml` sets the same expression, since GitHub's own repository license detection
would otherwise replace it.

`/usr/share/doc/giles/` holds `LICENSE-MIT` and `LICENSE-APACHE`, the AGPL text
(`LICENSE-AGPL-3.0`), and `THIRD-PARTY`, which names the AGPL component of the WhatsApp plugin and
is the source offer for its binary: the repository at the image's commit (the
`org.opencontainers.image.revision` label, set by the CI metadata step) and the pinned
`whatsapp-rust` revision. The files come from the repository (`plugins/giles-channel-whatsapp/` for
the last two), and the smoke test checks that they're in the image.

## The sandbox in a container

The plugin sandbox needs no extra privileges: Docker's default seccomp profile allows the
`landlock_*` syscalls and installing seccomp filters, and Landlock needs no capabilities. Landlock
comes from the host's kernel, which the startup check reports ([Plugin sandbox](sandbox.md)). The CI
image job checks it on GitHub's Ubuntu runners ([Continuous integration](ci.md)); Apple's
`container` VMs, used for local Linux checks, have no Landlock, and the check reports that.

## Not yet

- `linux/arm64` images. The build is portable (it builds natively on arm64), but CI publishes only
  `linux/amd64`, since building under QEMU emulation is too slow; an arm64 runner can add it.
- A `minimal` image with `giles` alone, as a second final stage.
