# Command line

Besides [`giles serve`](running.md), the `giles` command has these commands for managing Giles.

Run them as the user Giles runs as. They take the same `--home <DIR>` option and `GILES_HOME`
variable as `giles serve`.

Most of them need Giles to be running. If it isn't, they fail with
`giles is not running (nothing listens on <data root>/run/admin.sock)`. The commands that rename an
instance work differently. `giles channel rename` and `giles provider rename` need Giles stopped.
`giles toolset rename` works either way.

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
- `giles toolset rename <old> <new>` renames a toolset and its tool names. Giles rewrites the
  agents' `tools` and `ask` globs to match. With Giles stopped, it renames toolsets configured in
  `giles.toml` too.
- `giles toolset restart <name>` restarts the toolset's server now, as the **Restart** button on the
  admin pages does.

```text
$ giles toolset list
NAME    SOURCE      UPDATED                  PROBLEM
files   giles.toml  -
github  stored      2026-10-27 14:03:12 PDT
notes   stored      2026-10-27 14:05:40 PDT  auth.bearer: no secret "notes" in /Users/me/.giles/secrets.toml
```

A spec is a TOML file with the remote server's keys:

```toml
url = "https://api.githubcopilot.com/mcp/"
deadline = "2m"

[auth]
bearer = { secret = "github" }
```

| Key        | Default | Meaning                                                       |
| ---------- | ------- | ------------------------------------------------------------- |
| `url`      |         | The server's `https://` (or `http://`) URL                    |
| `auth`     | none    | `{ bearer = { secret = "<name>" } }`                          |
| `insecure` | `false` | Allow plain `http://` to a host other than this machine       |
| `deadline` | `"5m"`  | How long one tool call may run                                |
| `parallel` | `false` | The toolset's tools may run in parallel                       |
| `skills`   | `[]`    | Absolute paths of skill directories the toolset offers        |

Put the token in [`secrets.toml`](configuration.md#secrets) and name it in `auth`. A stored toolset
takes no other form of token. `show` prints the secret's name, never the token. Giles reads
`secrets.toml` again each time you run `set`, so a new secret needs no restart.

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

A toolset configured in `giles.toml` is listed, and `restart` works on it. `show`, `set`, and
`remove` refuse it. Change it in `giles.toml`. `rename` refuses it while Giles runs. With Giles
stopped, [it renames the table](#rename-a-toolset).

### Rename a toolset

A toolset's name is the prefix of its tools (`files__read`), so rename it with `rename` instead of
removing it and setting it again. Each agent's `tools` and `ask` globs that start with `<old>__`
change to `<new>__`, and the command lists them.

While Giles runs, `rename` renames a stored toolset, with no restart:

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

A toolset configured in `giles.toml` can't be renamed while Giles runs. The command refuses and
tells you to stop Giles first.

With Giles stopped, `rename` renames a stored toolset or the `[toolset.<old>]` table in
`giles.toml`, which keeps its comments and formatting. It also moves the toolset's data directory
and prints what it changed:

```console
$ giles toolset rename github gh
Renamed toolset instance "github" to "gh".
  giles.toml: [toolset.github] -> [toolset.gh]
  agents/jeeves/agent.toml: 2 reference(s) updated
  moved data/toolset/github -> data/toolset/gh
Start giles again to use the new name.
```

For a stored toolset, the command renames it in the database, leaves `giles.toml` alone, and says so
on a `database: the stored toolset ... is now ...` line. `<old>` can be in any case, and `<new>` can
be the same name in another case (`Notes` to `notes`). The command ignores a `[toolset.<name>]`
table with `url`, as Giles does, and leaves it in `giles.toml`.

A provider or channel with the same name keeps its own table and data.

If you rename a `[toolset.<name>]` table without the command, run it before you start Giles again.
It accepts the table already being renamed and moves the data, as
[`giles channel rename`](#giles-channel-rename-old-new) does for a channel.

Names use only letters, digits, and `-`. The command changes nothing and prints the reason if:

- `<new>` is the name of another toolset, in `giles.toml` or stored in Giles.
- `<new>` already has data. Renaming never merges two instances.
- `<new>` is `new` (in any case), for a stored toolset.
- `giles.toml` already has an ignored `[toolset.<new>]` table with `url`, and you rename a toolset
  configured in `giles.toml`. Remove the ignored table and run the command again.
- `<old>` isn't a toolset, in `giles.toml` or stored in Giles, and has no toolset data.
- An agent has a glob that matches the toolset's tools without starting with `<old>__`, such as
  `files*`. The error lists them. Change them (to `files__*`, say) and run the command again.
- `giles.toml` or an `agent.toml` can't be read or isn't valid TOML.

If a step fails, the command undoes the earlier ones and says what failed. If undoing a step fails
too, the error lists what to fix by hand. If the command is interrupted, for example by a power
loss, run it again to finish.

## `giles backup <dir>`

Writes a snapshot of the data root into an existing directory while Giles runs. It writes the
database (`giles-<time>.db`) and a tar of the configuration, Markdown files, and attachments
(`giles-<time>.tar`), then prints the two paths. See [Backups](backups.md) for what's included and
how to restore.

## `giles channel rename <old> <new>`

Renames a channel, such as `libera` in `[channel.libera]`, with Giles stopped. Sessions, history,
memory, and the plugin's data are stored under the channel's name. Renaming the table in
`giles.toml` alone would leave them behind, so use this command instead.

Stop Giles first. Otherwise the command fails with `giles is already running ... stop it first`.

```console
$ giles channel rename libera libera-chat
Renamed channel instance "libera" to "libera-chat".
  database: 3 sessions, 10 inbox rows, 0 outbox rows, 0 usage rows, 0 messages, 2 routes
  giles.toml: [channel.libera] -> [channel.libera-chat]
  giles.toml: 1 [[routes]] rule(s) updated (Giles no longer reads them once they are imported)
  agents/jeeves/memory.db: 2 group memories moved to "libera-chat"
  moved data/channel/libera -> data/channel/libera-chat
Start giles again to use the new name.
```

The command renames:

- the channel's database rows and stored [routes](configuration.md#routes)
- the group memories in each agent's `memory.db`
- the channel's directories, including the per-group `MEMORY.md` files
- the `[channel.<old>]` table in `giles.toml`, keeping its comments and formatting

If the table has no `component` key, the command adds `component = "<old>"`, so the channel keeps
running the same component. You don't need to change `[owner] identities`, because they're named
after the network, not the channel. [Instance rename](../technical/instance-rename.md) lists
everything the command touches.

The command renames a channel and nothing else. A provider or toolset with the same name keeps its
own table and data.

If you rename a table in `giles.toml` without this command, `giles serve` starts the renamed channel
with no sessions, inbox, or directories of its own, and leaves the old name's data where it is. To
move the data to the renamed table, run the command before you start Giles again. It accepts the
table already being renamed.

Once Giles has run the renamed channel, the command refuses, because `<new>` already has data. If
the channel has no sessions or other data of its own yet, remove `data/channel/<new>` and run the
command. Otherwise you have to merge the two by hand.

Names use only letters, digits, and `-`. `<new>` must not differ from another channel's name only in
case. The command changes nothing and refuses if:

- `<old>` isn't a channel, and has no channel data.
- `<new>` is also configured or already has data. Renaming never merges two instances.
- `giles.toml` or an `agent.toml` can't be read or isn't valid TOML.

If a step fails, the command undoes the earlier ones and says what failed. If it's interrupted, for
example by a power loss, run the same command again to finish. If undoing a failed rename also
fails, the error lists what to fix by hand.

## `giles provider rename <old> <new>`

Renames a provider, such as `anthropic` in `[provider.anthropic]`, with Giles stopped. Usage records
and the provider's data are stored under the provider's name, and your agents' files name its models
(`anthropic/claude`). Use this command instead of renaming the table in `giles.toml`.

Stop Giles first. Otherwise the command fails with `giles is already running ... stop it first`.

```console
$ giles provider rename anthropic claude
Renamed provider instance "anthropic" to "claude".
  database: 0 sessions, 0 inbox rows, 0 outbox rows, 212 usage rows, 1840 messages, 0 routes
  giles.toml: [provider.anthropic] -> [provider.claude]
  agents/jeeves/agent.toml: 2 reference(s) updated
Start giles again to use the new name.
```

The command renames:

- the provider's usage rows, and the provider named in the history of its replies
- the provider's directories
- the `[provider.<old>]` table in `giles.toml`, keeping its comments and formatting
- references in each agent's `agent.toml` to models starting with `<old>/`

It renames a provider and nothing else. These work as they do for
[`giles channel rename`](#giles-channel-rename-old-new):

- the `component` key
- a table you renamed by hand
- the names the command accepts
- what makes the command refuse
- what the command does when a step fails

A channel or toolset with the same name keeps its own table and data.
