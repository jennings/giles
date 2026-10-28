# Command line

Besides [`giles serve`](running.md), the `giles` command has these commands for managing Giles.

Run them as the user Giles runs as. They take the same `--home <DIR>` option and `GILES_HOME`
variable as `giles serve`.

All of them except `giles instance rename` need Giles to be running. If it isn't, they fail with
`giles is not running (nothing listens on <data root>/run/admin.sock)`.

## `giles status`

Shows what Giles is doing:

- its version and start time
- whether this host can sandbox plugins
- every channel and its last status
- every plugin, both channel plugins and MCP toolsets

The web chat's [admin pages](web.md#admin-pages) show the same, with QR codes drawn as images.

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
  github (toolset)
    state:     running, since 2026-10-23 09:12:06 PDT
    location:  https://api.githubcopilot.com/mcp/
    version:   1.0.0
    tools:     41
    sandbox:   none (remote)
```

For each plugin:

- `state`: `starting`, `running`, `idle` (a toolset stopped for idleness), `down, retrying at ...`,
  `failed`, or `stopped`. A `failed` plugin isn't retried until its binary changes or it's
  restarted. A `failed` toolset isn't retried until you restart it on the
  [admin page](web.md#admin-pages). The restart count in a toolset's `state` counts starts after a
  crash, a failed start, or a restart you asked for.
- `error`: the last launch failure, crash, or failed health check. For a toolset, the reason its
  last start failed or its server stopped.
- `tools`: how many tools a toolset offers to new conversations.
- `channel`: the channel's last status. `needs linking` or `needs login` tells you what to do, such
  as the code to enter on your phone.
- `location`: the plugin's executable, or the address of a remote plugin.
- `sandbox`: `on` (with the Landlock ABI on Linux), `off` for instances with `unsandboxed = true`,
  or `none` for remote plugins. `lacking` lines name the [sandbox](configuration.md#sandbox)
  protections this host's kernel can't give the plugin.
- `requests`: the sandbox grants the plugin says it needs. Giles never applies them by itself. Add
  the ones you agree with to the instance's `sandbox` table.

## `giles usage`

Shows tokens used per agent, then per agent, provider instance, and model. `FALLBACKS` counts
requests that went to a fallback model. `CACHE HIT` is the share of input tokens read from the
provider's prompt cache.

```text
$ giles usage --since 7d
Usage per agent since 2026-10-16 09:30:00 PDT:

AGENT   REQUESTS  FALLBACKS  INPUT  OUTPUT  CACHE READ  CACHE WRITE  CACHE HIT
jeeves       212          3  41210   30544     1902330        98120      93.3%
...
```

The report covers all requests by default. `--since` limits it to requests made since:

- a duration ago: `7d`, `12h`, `1h30m`
- midnight of a date, in your time zone: `2026-10-01`
- a time: `2026-10-01T09:00:00Z`

## `giles admin login-link --user <user>`

Prints a one-time URL that logs `<user>` in to the web chat, for example from a phone. The only user
is `owner`.

The link works once, within 10 minutes. The expiry time is printed on standard error. Login links
need the built-in `web` channel.

## `giles toolset`

Adds, changes, renames, and removes the remote MCP servers your agents use as
[toolsets](configuration.md#toolsets), with no restart. The [MCP servers](web.md#mcp-servers) admin
page shows the same toolsets.

- `giles toolset list` shows every toolset, where it's configured (`stored` or `giles.toml`), when
  it last changed, and why it isn't running if it failed.
- `giles toolset show <name>` prints a stored toolset's spec as TOML.
- `giles toolset set <name> --file <spec.toml>` creates the toolset, or replaces the spec of the one
  with that name. Give `-` as the file to read the spec from standard input.
- `giles toolset remove <name>` stops a stored toolset and removes it.
- `giles toolset rename <old> <new>` renames a stored toolset and its tool names. Giles rewrites the
  agents' `tools` and `ask` globs to match.
- `giles toolset restart <name>` restarts the toolset's server now, as the **Restart** button on the
  admin pages does.

```text
$ giles toolset list
NAME    SOURCE      UPDATED                  PROBLEM
files   giles.toml  -
github  stored      2026-10-27 14:03:12 PDT
notes   stored      2026-10-27 14:05:40 PDT  auth.bearer: no secret "notes" in /Users/me/.giles/secrets.toml
```

A spec has the keys of a remote server's `[toolset.<name>]` table, without the table's header line:

```toml
url = "https://api.githubcopilot.com/mcp/"
deadline = "2m"

[auth]
bearer = { secret = "github" }
```

The keys are `url`, `auth`, `insecure`, `deadline`, `parallel`, and `skills`.
[Configuration](configuration.md#toolsets) describes each one. Put the token in
[`secrets.toml`](configuration.md#secrets) and set `auth = { bearer = { secret = "<name>" } }`. A
stored toolset takes no other form of token. `show` prints the secret's name, never the token. Giles
reads `secrets.toml` again each time you run `set`, so a new secret needs no restart.

A server that Giles runs on this machine, a stdio server, is a
[`[toolset.<name>]` table](configuration.md#toolsets) in `giles.toml`. `set` refuses a spec without
`url`.

Giles checks the spec the way it checks `giles.toml`. If it refuses the spec, `set` prints the key
with the problem and why, and changes nothing:

```text
$ giles toolset set docs --file docs.toml
Error: url: a stored toolset is a remote server: set `url`. Configure a stdio server as `[toolset.<name>]` in giles.toml
```

### Edit a toolset

To change a toolset, save its spec, edit the file, and set it again:

```console
$ giles toolset show github > github.toml
$ $EDITOR github.toml
$ giles toolset set github --file github.toml
Saved toolset `github`.
```

The first line of the `show` output is a comment, `# updated_at = <time>`. Leave it in. If someone
changes the toolset after your `show`, `set` refuses with `was changed since it was read`. Run
`show` again and redo your edit.

To make a new toolset from the spec of another, delete that line first. Without it, `set` creates
the toolset, or replaces the spec of one that has the name.

A toolset configured in `giles.toml` is listed, and `restart` works on it. `show`, `set`, `remove`,
and `rename` refuse it. Change it in `giles.toml`.

### Rename a toolset

A toolset's name is the prefix of its tools (`files__read`), so rename it with `rename` instead of
removing it and setting it again. Each agent's `tools` and `ask` globs that start with `<old>__`
change to `<new>__`, and the command lists them:

```console
$ giles toolset rename files documents
Renamed toolset `files` to `documents`.
  agent jeeves: tools `files__*` -> `documents__*`
  agent jeeves: ask `files__delete` -> `documents__delete`
Sessions that are running keep the old tool names; new sessions use the new ones.
```

A session that is running keeps the old tool names, and its calls to them fail with
`tool unavailable`. New sessions use the new names. The [admin pages](web.md#admin-pages) rename a
toolset too.

Names use only letters, digits, and `-`. The command changes nothing and prints the reason if:

- `<new>` is the name of another toolset or instance, or is retired.
- An agent has a glob that matches the toolset's tools without starting with `<old>__`, such as
  `files*`. The error lists them. Change them (to `files__*`, say) and run the command again.

If a step fails, the command undoes the earlier ones and says what failed. If undoing a step fails
too, the error lists what to fix by hand.

## `giles backup <dir>`

Writes a snapshot of the data root into an existing directory while Giles runs. It writes the
database (`giles-<time>.db`) and a tar of the configuration, Markdown files, and attachments
(`giles-<time>.tar`), then prints the two paths. See [Backups](backups.md) for what's included and
how to restore.

## `giles instance rename <old> <new>`

Renames an instance, such as `libera` in `[channel.libera]`. Sessions, history, memory, and plugin
data are stored under the instance's name. Renaming the table in `giles.toml` alone would leave them
behind, so use this command instead.

Stop Giles first. Otherwise the command fails with `giles is already running ... stop it first`.

```console
$ giles instance rename libera libera-chat
Renamed channel instance "libera" to "libera-chat".
  database: 3 sessions, 10 inbox rows, 0 outbox rows, 0 usage rows, 0 messages, 2 routes
  giles.toml: [channel.libera] -> [channel.libera-chat]
  giles.toml: 1 [[routes]] rule(s) updated (Giles no longer reads them once they are imported)
  agents/jeeves/memory.db: 2 group memories moved to "libera-chat"
  moved data/plugins/libera -> data/plugins/libera-chat
Start giles again to use the new name.
```

A toolset made in the web UI or with `giles toolset` is a remote server stored in the database, with
no table in `giles.toml`. The command renames the stored toolset and the globs that name its tools,
leaves `giles.toml` alone, and says so on a `database: the stored toolset ... is now ...` line:

```console
$ giles instance rename files documents
Renamed toolset instance "files" to "documents".
  database: 0 sessions, 0 inbox rows, 0 outbox rows, 0 usage rows, 0 messages, 0 routes
  database: the stored toolset "files" is now "documents"
  agents/jeeves/agent.toml: 2 reference(s) updated
Start giles again to use the new name.
```

For a stored toolset, `<old>` can be in any case, and `<new>` can be the same name in another case
(`Notes` to `notes`).

The command renames:

- the instance's database rows and stored [routes](configuration.md#routes)
- the group memories in each agent's `memory.db`
- the instance's directories, including the per-group `MEMORY.md` files
- the `[<kind>.<old>]` table in `giles.toml`, keeping its comments and formatting, or the stored
  toolset
- references in each agent's `agent.toml`: model names starting with `<old>/` for a provider, and
  `tools` and `ask` globs starting with `<old>__` for a toolset

If the table has no `component` key, the command adds `component = "<old>"`, so the instance keeps
running the same component. You don't need to change `[owner] identities`, because they're named
after the network, not the instance. [Instance rename](../technical/instance-rename.md) lists
everything the command touches.

To rename a stored toolset while Giles runs, use [`giles toolset rename`](#rename-a-toolset).

Names use only letters, digits, and `-`. `<new>` must not differ from another instance's name only
in case. The command changes nothing and refuses if:

- `<old>` isn't configured and has no data.
- `<new>` is also configured or already has data. Renaming never merges two instances.
- `<old>` is a stored toolset and `<new>` is `new` (in any case) or listed in
  `core.retired_instances`.
- `giles.toml` or an `agent.toml` can't be read or isn't valid TOML.
- A renamed toolset's `tools` or `ask` globs match its tools without starting with `<old>__`, such
  as `github*` or `git*`. The command lists them. Change them (to `github__*`, say) and run it
  again. Globs such as `*`, `memory*` and `*__create_issue` are fine.

If a step fails, the command undoes the earlier ones and says what failed. If it's interrupted, for
example by a power loss, run the same command again to finish. If undoing a failed rename also
fails, the error lists what to fix by hand.

### If you renamed the table by hand

If you rename `[channel.libera]` to `[channel.libera-chat]` in `giles.toml` without this command,
`giles serve` refuses to start:

```text
giles.toml configures an instance that has no data, while a name it no longer configures still
has data. That looks like a rename done in giles.toml alone ...
  "libera" has 3 sessions, 10 inbox rows; configured without data: [channel.libera-chat]
If it was a rename, stop giles and run `giles instance rename libera libera-chat` ...
```

Run the command it suggests. It accepts the table already being renamed and moves the data to match.

Giles can mistake an unrelated new instance for a rename, for example if you removed one instance
and added another. List the old name in `core.retired_instances` to tell Giles it was removed on
purpose:

```toml
[core]
retired_instances = ["old-irc"]
```

Giles never deletes the data of a retired or removed name.
