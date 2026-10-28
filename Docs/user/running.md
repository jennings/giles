# Installing and running

This page covers installing Giles, starting it, and what it needs from the machine. Giles runs on
Linux and macOS.

## Installing

Build Giles from source with Cargo. If you use `rustup`, it installs the Rust toolchain the
repository asks for.

```sh
git clone https://github.com/jennings/giles
cd giles
cargo install --locked --path crates/giles
```

This puts the `giles` binary in `~/.cargo/bin/`. You can also
[run Giles in a Linux container](container.md).

## The data root

Giles keeps everything in one directory, the data root. It uses the first of these that's set:

1. `--home <DIR>`, accepted by every subcommand: `giles serve --home /srv/giles`.
2. The `GILES_HOME` environment variable.
3. `~/.giles`.

The data root looks like this:

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
  trash/            agents removed in the web UI, kept (made when you first remove one)
  run/              runtime files: the admin socket and token, the lock, plugin pid files and
                    sandbox specs
  cache/            caches Giles can rebuild
```

[Backups](backups.md) explains `giles backup` and the snapshot Giles takes before an upgrade.

Giles regularly deletes stored attachments that nothing refers to anymore. Attachments in your
conversations are never removed this way.

Unix socket paths are limited to about 100 bytes. If `run/admin.sock` would be longer, Giles puts
its sockets in a private directory under `$TMPDIR` and logs a warning naming it. Give `giles`
commands the same data root path so they find it.

## Running the daemon

```sh
giles serve
```

`giles serve` runs in the foreground and logs to standard error. It reads its
[configuration](configuration.md) from `giles.toml` and every `agents/<agent>/agent.toml`. The
smallest valid `giles.toml` declares the owner:

```toml
[owner]
identities = ["web:owner"]
```

Giles doesn't start if the configuration is missing or invalid. When a provider or channel rejects
its `config`, the error names the file, line, and key.

Giles logs `giles started` once every [toolset](configuration.md#toolsets) has started or failed. A
toolset that fails doesn't stop Giles. It's logged as `cannot start the toolset; retrying`, with the
reason, and retried with a growing delay of up to 5 minutes. Some failures can't be fixed by
retrying, such as rejected credentials or a host that can't sandbox. Those are logged once, and the
toolset isn't retried until you restart it on the [admin page](web.md#admin-pages) or restart Giles.

Only one `giles serve` can use a data root at a time. A second one exits at once with
`giles is already running with the data root ... as pid <pid>`. To run more than one Giles on a
machine, give each its own data root.

### Stopping

Stop Giles with Ctrl-C or SIGTERM. It shuts down and exits with status 0. If shutdown takes too
long, a second Ctrl-C or SIGTERM makes it exit at once with status 130. Any other non-zero status
means it failed to start, and the error is on standard error.

Stopping Giles loses no messages. If an agent was answering, that answer isn't resumed. After the
restart, Giles says "Interrupted by a restart; resend if needed." in that conversation. Messages
still waiting for an answer are answered after the restart.

## The web interface

`giles serve` also runs a web server, by default on `http://127.0.0.1:8080/`. The `giles started`
log line shows the address, for example `http=http://127.0.0.1:8080/`. To change it, set
[`core.http_listen`](configuration.md#core) in `giles.toml`:

```toml
[core]
http_listen = "127.0.0.1:9090"
```

The default only accepts connections from the same machine. If the address is already in use,
`giles serve` fails at startup with `cannot listen for HTTP on <address>`.

The web interface does not handle TLS, and Giles is very new software.
**Do not expose it to the public Internet**. Using [Tailscale](https://www.tailscale.com) to make it
available to your network is recommended.

With a [web channel](web.md) configured, the home page is your list of chats. The
[admin pages](web.md#admin-pages) at `/admin` show what `giles status` shows, including the QR code
of a channel waiting to be linked. Without a web channel, the server only shows that Giles is
running.

## Plugins

`giles serve` starts every channel plugin and restarts one that exits or stops answering, with a
growing delay of up to 5 minutes.

Restarting doesn't help with some failures:

- This machine can't sandbox plugins.
- The plugin was built for another protocol version.
- The plugin rejects its `config`.

Giles logs these as `cannot launch the plugin; not retrying until its binary changes`. It tries
again when the plugin's binary changes or Giles restarts.

## Plugin sandbox check

Giles runs the plugins it launches in a sandbox. A plugin that can't be sandboxed doesn't start. The
sandbox needs:

- **Linux:** Landlock enabled (kernel 5.13 or later, with `landlock` in the kernel's `lsm=` list)
  and seccomp. In a container, it's the host kernel that matters. Newer kernels enforce more, and
  [`min_abi`](configuration.md#sandbox) can require them.
- **macOS:** `/usr/bin/sandbox-exec`, able to apply a sandbox profile. It can't when Giles itself
  runs inside another sandbox that forbids it.

`giles serve` checks this at startup and logs the result. Check the second line of `giles status`:

```text
Plugin sandbox: Landlock ABI 6 and seccomp filters
```

If the check fails, Giles logs one warning naming the cause, for example:

```text
WARN giles::serve: plugin sandbox unavailable on this host; sandboxed plugins will not start
error=Landlock is built into the kernel but not enabled: add `landlock` to the `lsm=` kernel boot
parameter
```

To run a plugin without a sandbox, set `channel.<name>.unsandboxed = true` in `giles.toml`, or
`toolset.<name>.unsandboxed = true` for a stdio toolset.

## Logging

Giles logs at the `info` level by default. To change that, set `GILES_LOG` to a filter:

```sh
GILES_LOG=debug giles serve
GILES_LOG=info,giles=debug giles serve
```

`RUST_LOG` works too, but `GILES_LOG` wins. The filter syntax is that of `tracing-subscriber`'s
[`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html).
