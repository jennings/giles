# Installing and Running

Giles runs on Linux and macOS.

## Installing

Giles is built from source with Cargo. The repository pins its Rust toolchain in
`rust-toolchain.toml`, so `rustup` installs the right one automatically.

```sh
git clone https://github.com/jennings/giles
cd giles
cargo install --locked --path crates/giles
```

This puts the `giles` binary in `~/.cargo/bin/`.

## Running the Daemon

```sh
giles serve
```

`giles serve` runs in the foreground and logs to standard error. On first start it creates the data
root (below). It runs until it receives SIGINT (Ctrl-C) or SIGTERM, then shuts down and exits with
status 0. If shutdown takes too long, a second Ctrl-C or SIGTERM makes it exit at once with status
130. Any other non-zero status means it failed to start; the error is printed on standard error.

## The Data Root

Everything Giles keeps (configuration, secrets, agents, conversation history, attachments, plugin
state) lives in one directory, the data root. Giles picks it in this order:

1. `--home <DIR>`, accepted by every subcommand: `giles serve --home /srv/giles`.
2. The `GILES_HOME` environment variable.
3. `~/.giles`.

On start, `giles serve` creates any missing directories of the layout, readable only by you (mode
0700):

```text
~/.giles/
  giles.toml        your configuration (you write it)
  secrets.toml      API keys and passwords (you write it; make it mode 0600)
  agents/           one directory per agent
  users/            one directory per user
  skills/           global skills
  plugins/          plugin binaries
  data/             the database, attachments, and plugin state
  backups/          backups and pre-upgrade snapshots
  run/              runtime files: the admin socket and token, plugin pid files
  cache/            caches Giles can rebuild
```

Giles never loosens permissions you set on these directories, except `run/`, which it always resets
to 0700.

To run more than one Giles on a machine, give each its own data root.

### Long Paths

Unix sockets can't have long paths (about 100 bytes). If your data root is deep enough that
`run/admin.sock` would be too long, Giles puts its sockets in a private directory under `$TMPDIR`
instead and logs a warning naming it. Nothing else moves. The directory's name is derived from the
data root path, so `giles` commands that talk to the daemon find it when given the same data root
path.

## Logging

Giles logs at the `info` level by default. Set `GILES_LOG` (or `RUST_LOG`; `GILES_LOG` wins) to a
filter to change that:

```sh
GILES_LOG=debug giles serve
GILES_LOG=info,giles=debug giles serve
```

The filter syntax is that of `tracing-subscriber`'s
[`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html).
An invalid filter is an error at startup.

## Plugin Sandbox Check

Giles runs the plugins it launches in a sandbox, and a plugin that can't be sandboxed doesn't
start. At startup, `giles serve` checks that this machine can sandbox at all and logs the result:

- Linux: Landlock must be enabled (kernel 5.13 or later, with `landlock` in the kernel's `lsm=`
  list) and seccomp must be available. In a container, it's the host kernel that matters.
- macOS: `/usr/bin/sandbox-exec` must be present.

If the check fails, Giles logs one warning naming the cause, for example:

```text
WARN giles::serve: plugin sandbox unavailable on this host; sandboxed plugins will not start
error=Landlock is built into the kernel but not enabled: add `landlock` to the `lsm=` kernel boot
parameter
```
