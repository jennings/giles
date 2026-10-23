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
root (below). It then reads `giles.toml` and every `agents/<agent>/agent.toml`
([Configuration](configuration.md)) and refuses to start if they're missing or invalid; the
smallest valid `giles.toml` declares the owner:

```toml
[owner]
identities = ["web:owner"]
```

Next it opens its database (`data/giles.db`, created on first start) and builds the components the
configuration names. A provider or channel whose `config` it rejects stops startup with an error
naming the file, line, and key. Channel plugins start next ([Plugins](#plugins)). Toolsets can't be
started yet: each one is logged as `instance not started`.

It runs until it receives SIGINT (Ctrl-C) or SIGTERM, then shuts down and exits with status 0. If
shutdown takes too long, a second Ctrl-C or SIGTERM makes it exit at once with status 130. Any other
non-zero status means it failed to start; the error is printed on standard error.

Only one `giles serve` can use a data root at a time. A second one exits at once with
`giles is already running with the data root ... as pid <pid>`; stop the first one, or give the
second its own data root.

Messages are saved as they arrive, so stopping Giles loses none. If it stops while an agent is
answering, that answer is not resumed: at the next start Giles says "Interrupted by a restart;
resend if needed." in the conversation, as soon as that channel is up. Messages still waiting for
an answer are answered after the restart.

## The Web Interface

`giles serve` also runs a web server, by default on `http://127.0.0.1:8080/`. The `giles started`
log line shows the address, for example `http=http://127.0.0.1:8080/`. Set `http_listen` in the
`[core]` table of `giles.toml` to change it ([Configuration](configuration.md#core)):

```toml
[core]
http_listen = "127.0.0.1:9090"
```

The default only accepts connections from the same machine. If the address is in use (another
Giles, or any other server on that port), `giles serve` fails at startup with
`cannot listen for HTTP on <address>`. Port `0` picks any free port, which the log line then shows.

Giles speaks plain HTTP. To reach it from another machine, put a reverse proxy that terminates TLS
(Caddy, nginx, Tailscale Serve) in front of it rather than exposing the port directly.

With a `[channel.web]` instance configured, the web server's home page is your list of chats
([Web Chat](web.md)); without one it only shows that Giles is running.

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
  run/              runtime files: the admin socket and token, the lock, plugin pid files and
                    sandbox specs
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

## Plugins

`giles serve` launches every channel plugin at startup and keeps it running:

- Every 30 seconds it checks that each plugin still answers. A plugin that exits or fails the check
  is stopped (with its child processes) and started again after a delay: 1 second, then 2, 4, and
  so on up to 5 minutes. A plugin that then runs for 10 minutes starts over at 1 second.
- A plugin that fails in a way restarting won't fix, such as no sandbox on this machine, a
  plugin built for another protocol version, or a `config` the plugin rejects, is logged as
  `cannot launch the plugin; not retrying until its binary changes` and left alone until its binary
  changes (Giles checks once a minute) or Giles restarts.
- A message a plugin received but Giles hadn't stored yet when the plugin went down is delivered
  again once it's back.

Giles remembers what each plugin binary reported about itself (its version, capabilities, and
requested sandbox grants) in `cache/`, so it can show them even while the plugin isn't running.

## Plugin Sandbox Check

Giles runs the plugins it launches in a sandbox, and a plugin that can't be sandboxed doesn't
start. At startup, `giles serve` checks that this machine can sandbox at all and logs the result:

- Linux: Landlock must be enabled (kernel 5.13 or later, with `landlock` in the kernel's `lsm=`
  list) and seccomp must be available. In a container, it's the host kernel that matters. The log
  line names the Landlock ABI; newer kernels enforce more (see
  [`min_abi`](configuration.md#sandbox)).
- macOS: `/usr/bin/sandbox-exec` must be present and able to apply a sandbox profile. It can't
  when Giles itself runs inside another sandbox that forbids it.

If the check fails, Giles logs one warning naming the cause, for example:

```text
WARN giles::serve: plugin sandbox unavailable on this host; sandboxed plugins will not start
error=Landlock is built into the kernel but not enabled: add `landlock` to the `lsm=` kernel boot
parameter
```
