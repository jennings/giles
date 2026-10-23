# Command Line

Besides `giles serve` ([Installing and Running](running.md)), the `giles` command talks to the
running daemon. These commands take the same `--home <DIR>` option and `GILES_HOME` variable as
`giles serve`, so they find the daemon of that data root. If it isn't running they fail with
`giles is not running (nothing listens on <data root>/run/admin.sock)`.

They connect to the daemon's admin socket, `run/admin.sock`, and authenticate with the token in
`run/admin.token`. Both are readable only by the user Giles runs as, and the token changes every
time the daemon starts, so run these commands as that user.

## `giles status`

What the daemon is doing: its version and start time, whether this host can sandbox plugins, every
channel with its last reported status, and every plugin (channel plugins and MCP toolsets). The
web chat's [admin pages](web.md#admin-pages) show the same, with QR codes drawn as images.

```text
giles 0.1.0, running since 2026-10-23 09:12:04 PDT
Plugin sandbox: Landlock ABI 6 and seccomp filters

Channels:
  libera  irc (plugin): needs linking: enter the code 1234-5678 (until 2026-10-23 09:22:00 PDT)
  web     web (built in): connected (since 2026-10-23 09:12:04 PDT)

Plugins:
  libera (channel, irc)
    state:     running, pid 4242, since 2026-10-23 09:12:05 PDT
    channel:   connected
    location:  /home/me/.giles/plugins/giles-channel-irc
    version:   1.2.0
    sandbox:   on, Landlock ABI 6
    requests:  network: tcp:6697
```

For each plugin:

- `state`: `starting`, `running`, `idle` (a toolset stopped for idleness), `down, retrying at ...`,
  `failed` (not retried until its binary changes or it's restarted), or `stopped`. This version of
  Giles doesn't supervise plugins yet, so it shows `unknown` until it does.
- `error`: the last launch failure, crash, or failed health check.
- `channel`: the channel's last status. `needs linking` or `needs login` tells you what to do, for
  example the code to enter on your phone.
- `location`: the plugin's executable, or the address of a remote plugin.
- `sandbox`: `on` (with the Landlock ABI on Linux), `off` for instances with `unsandboxed = true`,
  or `none` for remote plugins, which Giles doesn't run. `lacking` lines name protections this
  host's kernel can't give the plugin ([Sandbox](configuration.md#sandbox)).
- `requests`: the sandbox grants the plugin says it needs. Giles never applies them by itself; add
  what you agree with to the instance's `sandbox` table.

## `giles usage`

Tokens used per agent, then per agent, provider instance, and model, with how many requests went to
a fallback model and the prompt cache hit rate: the share of input tokens read from the provider's
prompt cache.

```text
$ giles usage --since 7d
Usage per agent since 2026-10-16 09:30:00 PDT:

AGENT   REQUESTS  FALLBACKS  INPUT  OUTPUT  CACHE READ  CACHE WRITE  CACHE HIT
jeeves       212          3  41210   30544     1902330        98120      93.3%
...
```

`--since` limits the report to requests from a duration ago (`7d`, `12h`, `1h30m`), from midnight
of a date (`2026-10-01`, in your time zone), or from a time (`2026-10-01T09:00:00Z`). Without it the
report covers all requests.

## `giles admin login-link --user <user>`

Prints a one-time URL that logs `<user>` in to the web chat, for example from a phone. This Giles
has one user, `owner`. The link works once and expires after a while; when it does is printed on
standard error. Login links need the built-in `web` channel.
