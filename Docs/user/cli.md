# Command Line

Besides `giles serve` ([Installing and Running](running.md)), the `giles` command talks to the
running daemon (except `giles instance rename`, which needs it stopped). These commands take the
same `--home <DIR>` option and `GILES_HOME` variable as `giles serve`, so they find the daemon of
that data root. If it isn't running they fail with
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

## `giles backup <dir>`

Writes a consistent snapshot of the data root into an existing directory while the daemon runs: the
database (`giles-<time>.db`) and a tar of the configuration, Markdown files, and attachments
(`giles-<time>.tar`). It prints the two paths. A relative `<dir>` is relative to where you run the
command. See [Backups](backups.md) for what's included and how to restore.

## `giles instance rename <old> <new>`

Renames an instance, with Giles stopped. An instance name (`libera` in `[channel.libera]`) is
permanent: sessions, history, the inbox, group memory, and the plugin's data directory are all
stored under it, so changing the table's name in `giles.toml` alone would leave all of that behind
and start a new, empty instance. This command renames everything together:

- the database rows that name the instance (sessions, inbound and outbound messages, usage, the
  provider recorded on past replies, and the [routes](configuration.md#routes) Giles stores, which
  keep their order),
- the directories `data/plugins/<old>/` and `agents/*/groups/<old>/`, and the instance's cache and
  pid files,
- the `[<kind>.<old>]` table in `giles.toml`, with its comments, formatting, and sub-tables, and the
  `[[routes]]` that name a renamed channel (Giles imports those once and then ignores them, so this
  only matters before its first start with routes in the database, or to keep a leftover block
  consistent). An instance without a `component` key runs the
  component named like the instance (`[channel.web]` is the built-in web channel), so the command
  adds `component = "<old>"` to keep it running the same thing,
- in each agent's `agent.toml`: for a renamed provider, the `models`, `compaction_model`, and job
  `model` values that start with `<old>/`; for a renamed toolset, the `tools` and `ask` globs that
  start with `<old>__`.

```console
$ giles instance rename libera libera-chat
Renamed channel instance "libera" to "libera-chat".
  database: 3 sessions, 10 inbox rows, 0 outbox rows, 0 usage rows, 0 messages, 2 routes
  giles.toml: [channel.libera] -> [channel.libera-chat]
  giles.toml: 1 [[routes]] rule(s) updated (Giles no longer reads them once they are imported)
  moved data/plugins/libera -> data/plugins/libera-chat
Start giles again to use the new name.
```

Unlike the other commands on this page, this one needs the daemon stopped: it takes the same lock
`giles serve` does, and fails with `giles is already running ... stop it first` otherwise. Names
use only letters, digits, and `-`, and `<new>` must not differ from another instance's name only
in case. It refuses, changing nothing, if:

- `<old>` isn't configured and has no data,
- `<new>` is also configured, or already has data (database rows or directories): renaming never
  merges two instances. Routes that already name `<new>` aren't data, and stay,
- `giles.toml` or an `agent.toml` can't be read or isn't valid TOML,
- a renamed toolset's `tools` or `ask` globs match its tools by the old name without starting with
  `<old>__` (`github*`, `git*`). Giles can't tell what you meant and leaving them would stop them
  matching (an `ask` entry would stop asking), so it lists them: change them (to `github__*`, say)
  and run the command again. Globs such as `*`, `memory*` and `*__create_issue` are fine.

The rename is all or nothing as far as Giles can tell. The database changes happen in one
transaction and the file changes inside it, and if any step fails, the earlier ones are undone and
the error says what failed. If the process is killed in the middle (a power loss), run the same
command again: it finishes what was left, treating directories that already moved as done. If
undoing a failed rename also fails, the error lists what to fix by hand.

What the command leaves alone:

- Tool names in old conversations: a renamed toolset's past calls still read `<old>__<tool>`.
- Your identities: `irc:libera:account:me` is named after the network the channel reports, not
  after the instance, so `[owner] identities` stay as they are.

### If you renamed the table by hand

Renaming `[channel.libera]` to `[channel.libera-chat]` in `giles.toml` without this command leaves
the old name's data orphaned. Giles notices at startup: if the configuration has an instance with
no data of its own while some other name that is no longer configured still has data, `giles serve`
refuses to start and says so:

```text
giles.toml configures an instance that has no data, while a name it no longer configures still
has data. That looks like a rename done in giles.toml alone ...
  "libera" has 3 sessions, 10 inbox rows; configured without data: [channel.libera-chat]
If it was a rename, stop giles and run `giles instance rename libera libera-chat` ...
```

Run the command it suggests. It accepts the table already being renamed and moves the data to
match. The check compares names of the same kind (database rows of a channel, say, can only be a
renamed channel), so adding an instance or removing one doesn't trigger it on its own.

If you removed an instance on purpose, and added an unrelated one that the check mistakes for a
rename, tell it the old name is retired, and its data stays where it is:

```toml
[core]
retired_instances = ["old-irc"]
```

The list is also the way out if an instance you have never used (a provider, say, with no usage
yet) shows up next to a removed one. Giles never deletes the data of a retired or removed name.
