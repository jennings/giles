# Configuration

This page lists every setting in Giles' configuration files. Both kinds live in the
[data root](running.md#the-data-root):

- `giles.toml`: the core, the owner, and component instances.
- `agents/<agent>/agent.toml`: one per agent.

Giles reads them when `giles serve` starts, so restart it after you edit them. Giles never changes
`giles.toml`.

Agents you create or edit in [the web UI](web.md#agents) are saved to `agent.toml` and take effect
at once. The web UI keeps the rest of the file as you wrote it, comments included.

If a file has an error, `giles serve` refuses to start. It prints one error naming the file, the
line and column when known, the key, and what is wrong:

```text
Error: /home/me/.giles/giles.toml:12:12: channel.libera.max_blob: invalid size "50MB": expected a
number of bytes or an integer and a unit (B, KiB, MiB, GiB), e.g. "50 MiB"
```

A key Giles doesn't recognize in `giles.toml` doesn't stop it. Giles ignores the key and logs a
warning, so check the log after an edit. A misspelled key leaves its setting at the default.

```text
WARN /home/me/.giles/giles.toml:7:1: unknown key `core.http_listn` (ignored)
WARN /home/me/.giles/giles.toml:12:30: unknown key `provider.anthropic.config.modle` (ignored)
```

In `agent.toml`, an unknown key is an error.

## Value formats

- **Names** of instances, components, agents, and jobs use only letters, digits, and `-`:
  `irc-libera`, `jeeves`, `check-in`.
- **Durations** are strings of one or more `<integer><unit>` parts, with units `ms`, `s`, `m`, `h`,
  and `d`: `"30s"`, `"10m"`, `"1h30m"`.
- **Sizes** are a number of bytes, or a string with a unit `B`, `KiB`, `MiB`, or `GiB`: `52428800`,
  `"50 MiB"`, `"512KiB"`.
- **Models** are `<provider instance>/<model id>`, split at the first `/`. In
  `"openrouter/anthropic/claude-sonnet-4.5"` the instance is `openrouter` and the model id is
  `anthropic/claude-sonnet-4.5`.
- **Time zones** are IANA names, such as `"America/Los_Angeles"`.
- **Identities** are `<namespace>:<native id>`, such as `"whatsapp:+15551234567"`,
  `"email:me@example.com"`, or `"irc:libera.chat:account:stephen"`. Each channel's page gives its
  namespace and id format.

## `giles.toml`

### `[core]`

Settings of the core itself. Optional.

| Key                 | Default            | Meaning                                                           |
| ------------------- | ------------------ | ----------------------------------------------------------------- |
| `http_listen`       | `"127.0.0.1:8080"` | Address of the HTTP server (web chat, admin pages, login links)   |
| `retired_instances` | `[]`               | Names of instances you removed on purpose, whose data stays       |
| `journal_mode`      | `"wal"`            | SQLite journal mode of `data/giles.db`: `wal`, `delete`, `truncate`, `persist` |

`retired_instances` tells Giles to ignore the data a removed instance left behind when it checks for
a half-done rename. No instance table may use a name listed there. See
[Renaming an instance](cli.md#giles-instance-rename-old-new).

To reach Giles from other machines, or from outside a container, set `http_listen` to
`"0.0.0.0:8080"`. The web interface does not handle TLS, so read
[The web interface](running.md#the-web-interface) before you do.

#### `journal_mode`

Leave this at `"wal"` unless Giles can't start with it. WAL needs shared memory, which network file
systems don't provide. These include NFS, SMB, and some container and FUSE mounts.

On such a file system, Giles stops at startup with an error that names this option. Set
`journal_mode = "delete"`, or better, move `~/.giles` to a local disk.

| Value        | Journal                                                                         |
| ------------ | ------------------------------------------------------------------------------- |
| `"wal"`      | Write-ahead log: reads don't wait for writes. Needs shared-memory support       |
| `"delete"`   | Rollback journal file, deleted after each write. Works everywhere               |
| `"truncate"` | Like `delete`, but empties the journal file instead of deleting it              |
| `"persist"`  | Like `delete`, but keeps the journal file and marks it unused                   |

Values are case-insensitive. Giles refuses SQLite's `memory` and `off` modes, because a crash or
power loss during a write could then corrupt the database or lose data.

To switch modes, change the value and restart Giles. It converts the existing database in place.
First stop any other program that has `giles.db` open, such as the `sqlite3` shell.

### `[owner]`

Giles v1 serves one person, the owner. Required.

| Key          | Default                  | Meaning                                                     |
| ------------ | ------------------------ | ----------------------------------------------------------- |
| `identities` | required                 | The owner's identities, at least one, no duplicates         |
| `default`    | the only identity        | Where jobs and other proactive messages go; required when there are several identities |
| `timezone`   | the agent's, else system | The owner's time zone, for message times and job schedules  |

Giles drops and logs these messages:

- messages from any identity not listed here
- messages whose sender the channel couldn't verify
- messages in group conversations, which v1 doesn't support

An identity is `<namespace>:<id>`. Each channel decides its namespace and the form of its ids, for
example:

| Channel  | Example identity                  |
| -------- | --------------------------------- |
| Web      | `web:owner`                       |
| IRC      | `irc:libera.chat:account:stephen` |
| Email    | `email:me@example.com`            |
| WhatsApp | `whatsapp:+15551234567`           |
| Discord  | `discord:82198898841029460`       |

Giles logs a message from an unlisted identity with the identity as the channel reported it, so you
can copy it here.

Identities match exactly, capitalization included. Email domains are the exception:
`email:me@Example.com` matches `me@example.com`, but not `Me@example.com`.

```toml
[owner]
identities = ["whatsapp:+15551234567", "email:me@example.com"]
default = "whatsapp:+15551234567"
timezone = "America/Los_Angeles"
```

### Instances

Providers, channels, and toolsets are configured as instances, one table each:

- `[provider.<name>]`
- `[channel.<name>]`
- `[toolset.<name>]`

**Choose instance names with care.** Sessions, history, tool names (`<instance>__<tool>`), and the
instance's data directory are keyed by the name. To change it, use
[`giles instance rename`](cli.md#giles-instance-rename-old-new).

Names are unique across all three kinds, ignoring case. `Libera` and `libera` clash.

`component` says what runs the instance, and defaults to the instance name. Giles uses its built-in
component of that kind and name, if it has one. Otherwise the instance runs the plugin
`giles-<kind>-<component>`, found in `<data root>/plugins/` or on `$PATH`.

Two IRC networks are two instances of one component:

```toml
[channel.irc-libera]
component = "irc"          # runs giles-channel-irc
config = { server = "irc.libera.chat", nick = "jeeves", network = "libera.chat" }

[channel.irc-oftc]
component = "irc"
config = { server = "irc.oftc.net", nick = "jeeves", network = "oftc.net" }
```

Giles passes the `config` table to the component without checking it. The component checks it when
it starts, and each component's page documents its keys. Every other key of an instance table
belongs to Giles.

#### Providers

The built-in providers are:

- `anthropic`: the Anthropic Messages API.
- `openai`: any OpenAI-compatible Chat Completions API, such as OpenAI, OpenRouter, or Ollama.

Provider plugins aren't supported yet, so `component` must name one of these. Their `config` keys
are in [Providers](providers.md).

| Key         | Meaning                                         |
| ----------- | ----------------------------------------------- |
| `component` | `anthropic` or `openai`; default: the instance name |
| `config`    | Settings for the provider                       |

```toml
[provider.anthropic]
config = { api_key = { secret = "anthropic" } }

[provider.ollama]
component = "openai"
config = { base_url = "http://127.0.0.1:11434/v1" }
```

#### Channels

The built-in channel is `web`. Its `config` keys, `public_url` and `proxy`, are in
[Web Chat](web.md#logging-in).

Any other component is a plugin. Giles launches it, or with `address`, connects to it where someone
else runs it.

| Key           | Applies to        | Default    | Meaning                                                  |
| ------------- | ----------------- | ---------- | -------------------------------------------------------- |
| `component`   | all               | name       | The component                                            |
| `config`      | all               | `{}`       | Settings for the component                               |
| `max_blob`    | plugins           | `"50 MiB"` | The largest attachment Giles accepts from the plugin     |
| `sandbox`     | launched plugins  |            | Extra sandbox grants ([Sandbox](#sandbox))               |
| `unsandboxed` | launched plugins  | `false`    | Run without a sandbox (logged as a warning at every launch) |
| `address`     | remote plugins    |            | `https://host:port` (or `http://`) of a running plugin   |
| `auth`        | remote plugins    | none       | `{ bearer = <credential> }` or `{ oauth = { ... } }`, below |
| `insecure`    | remote plugins    | `false`    | Allow plain `http://` to a host other than this machine  |

An attachment larger than `max_blob`, or one the plugin fails to hand over, doesn't stop the
message. The agent gets the message without it, plus a line saying why, so it can tell the sender.

Without `insecure`, plain `http://` is accepted only for `localhost`, `127.0.0.0/8`, and `::1`.
Without TLS, anyone on the network path could inject messages.

An address can't contain a user name or password. Use `auth` instead:

```toml
[channel.mail]
component = "email"
address = "https://mail-bridge.example.com:7443"

[channel.mail.auth.oauth]
token_url = "https://auth.example.com/token"
client_id = "giles"
client_secret = { secret = "mail-bridge" }
scopes = ["channel"]
```

`oauth` uses the OAuth 2.0 client credentials grant, with these keys:

- `token_url`: `https://`, or `http://` on this machine.
- `client_id`.
- `client_secret`: a credential.
- `scopes`: optional.

##### Remote plugins

A channel with `address` is a plugin you run yourself, on this machine or another one. The plugins
that ship with Giles [run this way](remote-plugins.md), and
[you can write your own](writing-a-plugin.md#running-a-plugin-remotely). Giles never starts, stops,
or sandboxes it, and the plugin keeps its own state.

- While Giles can't connect, it runs without that channel and keeps retrying. That includes when the
  plugin refuses Giles' token or its certificate doesn't check out. `giles status` shows the
  instance as `down, retrying at ...` with the last error, or once connected, as `running` with no
  pid.
- With `https://`, the plugin's certificate must be valid for the host in `address`. It must be
  issued by a public certificate authority in Giles' built-in copy of the Mozilla list. Giles
  doesn't use the system's trust store.
- `insecure = true` only allows plain `http://`. It never turns certificate checks off.
- `auth = { bearer = ... }` sends `Authorization: Bearer <token>` on every call. Without `auth`,
  Giles sends no credentials.
- With `oauth`, Giles gets an access token from `token_url` before connecting, using HTTP Basic auth
  with the client id and secret. It sends that token, and gets a new one before it expires.
- When Giles stops, the plugin keeps running. Messages that arrive meanwhile wait in it until Giles
  connects again.

#### Toolsets

A toolset is an MCP server. A `[toolset.<name>]` table configures a stdio server, which Giles
launches and talks to over its standard input and output. It runs one of two ways:

- `command`: an absolute path or a program name looked up on `$PATH`, with `args`.
- No `command`: the first-party plugin `giles-toolset-<component>` (the component defaults to the
  instance name).

A remote server, one Giles reaches over the network, is added in the [web UI](web.md#mcp-servers) or
with [`giles toolset set`](cli.md#giles-toolset). Giles ignores a `[toolset.<name>]` table that has
`url`, and logs a warning naming it at every start.

| Key           | Default   | Meaning                                                       |
| ------------- | --------- | ------------------------------------------------------------- |
| `command`     |           | The program to run                                            |
| `component`   | name      | The plugin to run when there's no `command`                   |
| `args`        | `[]`      | Arguments                                                     |
| `env`         | `{}`      | Environment variables to set, name to credential              |
| `idle_stop`   | off       | Stop the server after this long without calls (a duration); its tools stay offered and it restarts on the next call |
| `sandbox`     |           | Extra sandbox grants ([Sandbox](#sandbox))                    |
| `unsandboxed` | `false`   | Run without a sandbox (logged as a warning at every launch)   |
| `deadline`    | `"5m"`    | How long one tool call may run; then it's cancelled and the model told it timed out |
| `parallel`    | `false`   | The toolset's tools may run in parallel with each other and with read-only core tools, up to 8 at once |
| `skills`      | `[]`      | Absolute paths of skill directories the toolset offers        |

Toolsets have no `config` table. Configure a server through its `args`.

No plugin may write to a `skills` directory. Giles refuses a `skills` path inside the plugins' data
directories, and any `write` or `unix` sandbox grant that overlaps one.

Giles looks up a stdio server's `command` on its own `$PATH`, and runs it in the
[sandbox](#sandbox). The server may read and run everything in the command's install prefix. That's
the directory above `bin/`, such as `/opt/homebrew` or a Python virtual environment, else the
command's own directory.

If the command is a script, its interpreter gets the same, including one found through
`#!/usr/bin/env node`. A prefix that is or contains your home directory, or would contain the data
root, narrows to the command's directory. `~/bin/server` gets `~/bin`, never `~`.

**A prefix grants everything under it.** A Homebrew command gets all of `/opt/homebrew`, including
its `etc` and `var`. `~/.cargo/bin/server` gets all of `~/.cargo`, including `credentials.toml`.

To keep a server away from those, install it in a prefix of its own, such as a Python virtual
environment or one made with `npm install --prefix`. Don't give any toolset a `write` grant on an
interpreter's installation, such as a Node or Python directory, because other toolsets run that
interpreter.

- Install servers ahead of time. `npx -y` or `uvx` at run time need the network and writable package
  caches, which the sandbox doesn't grant.
- A server that needs the network needs a grant, such as `sandbox = { network = ["tcp:443"] }`.
- The server gets only its `env`, plus `LANG`, `HOME` and `TMPDIR` inside its data directory,
  `GILES_PLUGIN_DATA` (its data directory), and a `PATH` of the directories above followed by
  `/usr/bin:/bin`. `env` can't set `HOME`, `TMPDIR`, `PATH`, or `GILES_PLUGIN_DATA`.

Giles starts every toolset when it starts, and [keeps it running](running.md#running-the-daemon). A
server's tools stay offered while the server is stopped. If a server changes its list of tools while
it runs, conversations that start afterwards get the new list.

Images, audio, and files a tool returns are kept like attachments, up to 50 MiB each. The model sees
them if its provider takes that type, and can send them on with `send_attachment`.

The model can also give an attachment to a toolset's tool. Giles copies it into the server's
`TMPDIR` for the length of the call and passes the file's path.

```toml
[toolset.files]
command = "/opt/homebrew/bin/mcp-server-filesystem"
args = ["/Users/me/Documents"]
sandbox = { read = ["/Users/me/Documents"] }
idle_stop = "30m"
```

### Sandbox

Every plugin Giles launches, channel plugins and stdio toolsets, runs in a sandbox. By default it
can:

- read its own binary, system libraries, TLS certificates, and DNS configuration
- read and write its own data directory

Channel plugins may use the network, and stdio toolsets may not. `sandbox` grants more:

```toml
sandbox = { network = ["tcp:443"], read = ["/opt/homebrew"], write = ["/Users/me/Notes"] }
```

| Key       | Meaning                                                                          |
| --------- | -------------------------------------------------------------------------------- |
| `network` | `true` for all IP traffic, `false` for none, or TCP ports as `["tcp:443"]` (enforced on macOS, and on Linux with Landlock ABI 4, kernel 6.7 or later; on older Linux kernels all ports are open, and on Linux UDP is always open with any network grant). Name lookups work with any network grant |
| `read`    | Absolute paths the plugin may read                                               |
| `write`   | Absolute paths the plugin may read and write                                     |
| `unix`    | Absolute paths of Unix sockets the plugin may connect to (on Linux before Landlock ABI 9, kernel 7.1, any entry allows connecting to every socket) |
| `min_abi` | Linux only: refuse to start the plugin unless the kernel's Landlock ABI is at least this (table below) |

On Linux, what the sandbox can enforce depends on the kernel's Landlock ABI. Giles logs the ABI at
every launch, and warns when the kernel lacks something the plugin's grants rely on.

To refuse older kernels, set `min_abi`. A plugin whose kernel is below it doesn't start, and the log
says why.

| `min_abi` | Kernel | Guarantees                                                                       |
| --------- | ------ | -------------------------------------------------------------------------------- |
| 1         | 5.13   | Files: read, write, and execute grants (the minimum; Giles needs Landlock)       |
| 3         | 6.2    | Files can't be truncated outside write grants by Landlock itself                 |
| 4         | 6.7    | `network` port lists are enforced, and plugins can't listen on TCP ports         |
| 6         | 6.12   | The plugin can't signal processes outside its sandbox, Giles included            |
| 9         | 7.1    | `unix` grants allow exactly the listed sockets (they must exist when the plugin starts) |

On Linux, grants must exist when the plugin starts. A path created later isn't covered.

No grant may touch the data root, except the instance's own data directory. A path inside the data
root, or one containing it, is an error. That includes your home directory when the data root is
`~/.giles`.

For a path that goes through a symbolic link, Giles checks where the link leads when Giles starts. A
link created or changed later isn't checked, so don't grant directories that other programs can
rewire.

`write` and `unix` grants may not touch a directory holding a program Giles runs. That's the `giles`
binary itself, and each plugin or toolset command. Granting `write = ["/opt/homebrew"]` would let a
plugin replace `giles` or another plugin. Reading those directories is fine.

A plugin can create symbolic links wherever it may write, so Giles never follows links there. Write
`write` grants, and grants inside the plugin's data directory, with the real path, in the same upper
and lower case as on disk. A plugin won't start if its data directory is a symbolic link.

`unsandboxed = true` turns the sandbox off for one instance, and can't be combined with `sandbox`.
Built-in components and remote plugins don't take these keys.

### Secrets

Keep credentials out of `giles.toml`. Put them in `secrets.toml` in the data root, a flat table of
names to strings:

```toml
anthropic = "sk-ant-..."
github = "ghp_..."
```

Giles refuses to start if group or others have any access to `secrets.toml`. Make it private with
`chmod 600 ~/.giles/secrets.toml`. You don't need the file if nothing refers to it.

Refer to a secret with `{ secret = "<name>" }`. To take a value from Giles' own environment instead,
use `{ env = "<VARIABLE>" }`. A reference works in these places:

- Anywhere in an instance's `config` table, at any depth. There, a table whose only key is `secret`
  or `env` is always a reference.
- As a credential: `auth.bearer`, `auth.oauth.client_secret`, and the values of a toolset's `env`. A
  credential may also be a plain string.

```toml
[provider.anthropic]
config = { api_key = { secret = "anthropic" } }

[toolset.github]
command = "/opt/homebrew/bin/github-mcp-server"
args = ["stdio"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = { secret = "github" } }
```

Giles refuses to start if a secret is missing or a variable is unset. Logs and error messages name
the secret or variable, never the value. Where Giles shows an instance's `config`, values from
references read `<redacted>`.

### Routes

Routes pick the agent for each conversation. You manage them in [the web UI](web.md#routes), not in
`giles.toml`, and a change applies to the next message without a restart.

Giles tries routes in order, and the first match wins. A route matches when every key it sets
matches. Conversation ids match exactly, capitalization included.

Giles drops and logs messages in a conversation no route matches, including
[chat commands](commands.md).

| Key            | Meaning                                                            |
| -------------- | ------------------------------------------------------------------ |
| channel        | Match this channel instance                                        |
| kind           | Match `direct` or `group` conversations (v1 ignores groups)        |
| conversation   | Match this conversation id, as the channel reports it, e.g. `#giles` |
| agent          | Required: the agent, a directory under `agents/`                   |

A route that sets only `agent` matches everything, so put it last. The web UI refuses a route whose
channel instance or agent doesn't exist. It also refuses a route that could never be used, such as a
duplicate, or a move that would put a route before one it hides.

A fresh install has no routes, so add them in the web UI. Earlier versions kept routes in
`giles.toml`:

```toml
[[routes]]
channel = "irc-libera"
conversation = "#giles"
agent = "jeeves"

[[routes]]                 # everything else
agent = "jeeves"
```

Giles imports `[[routes]]` like these, in order, the first time it starts after the upgrade. A route
to a channel instance or agent that doesn't exist, or with a key a route doesn't have, stops startup
and names the route.

After the import, Giles never reads `[[routes]]` again, even if you delete every route in the web
UI. It warns at every start while `giles.toml` still has them, so remove them from the file.

## `agents/<agent>/agent.toml`

Each directory under `agents/` is an agent, named by the directory, and must contain `agent.toml`.
Giles ignores files in `agents/` and directories whose names start with `.`.

| Key                   | Default         | Meaning                                                           |
| --------------------- | --------------- | ----------------------------------------------------------------- |
| `name`                |                 | Display name                                                      |
| `models`              | required        | Models in order; v1 uses the first                                |
| `compaction_model`    | `models`        | The model that summarizes old history when a session gets long    |
| `tools`               | `[]`            | Tools the agent may use: globs over tool names, e.g. `"github__*"`, `"memory_*"` |
| `ask`                 | `[]`            | Globs of tools that need your approval before each call           |
| `max_iterations`      | no limit        | Model requests with tools per interactive turn: tools run after at most N−1 of them, then one final request without tools |
| `max_iterations_jobs` | `25`            | The same for job turns                                            |
| `busy_mode`           | `"steer"`       | New messages during a turn: `"steer"` shows them to the model at the next tool step; `"queue"` keeps them for the next turn |
| `memory_cap`          | `2200`          | Most characters of core memories in each scope (shared, each user, each group) |
| `approval_timeout`    | `"10m"`         | How long an approval request waits before it is denied            |
| `timezone`            | system          | The agent's time zone, used when `[owner]` sets none              |
| `idle_reset`          | off             | Start a new session after this long without messages              |

A toolset's tools are named `<instance>__<tool>`. The core tools have bare names:

- `memory_add`, `memory_edit`, `memory_forget`, `memory_recall`, and `memory_search`. `"memory_*"`
  allows all five.
- `load_skill`
- `schedule`
- `send_attachment`

`memory_cap` limits the core memories, the ones in the agent's prompt. See
[Skills and Memory](skills-and-memory.md#memory).

When a conversation would fill about 80% of the model's context window, `compaction_model`
summarizes the older messages. The model then sees the summary plus the most recent messages. The
full transcript stays in the database.

A cheaper model works well for compaction, if its own context window can hold a good part of the
conversation. Giles takes a model's context window from the provider. For OpenAI-compatible servers,
the `context_window` in [the provider's config](providers.md) applies too, also when the server
doesn't list the model under that exact id. Without either, Giles assumes 128,000 tokens.

A call to a tool matching `ask` waits until you answer with one of the
[approval commands](commands.md). The call is denied if nobody answers within `approval_timeout`, or
if you send any other message. The wait doesn't count against the toolset's `deadline`.

### `[[jobs]]`

Scheduled prompts. Each job's reply is sent to the owner's default identity, unless the reply is
exactly `NO_REPLY`.

| Key            | Default      | Meaning                                                             |
| -------------- | ------------ | ------------------------------------------------------------------- |
| `id`           | required     | A name, unique within the agent. Keep it stable: run history is keyed by it |
| `cron`         | required     | Five-field cron schedule: minute, hour, day of month, month, day of week |
| `prompt`       |              | The prompt text                                                     |
| `prompt_file`  |              | A file in the agent's directory holding the prompt, read at each run |
| `session`      | `"isolated"` | `"isolated"`: a fresh context each run; `"dm"`: a turn in your DM   |
| `dm_excerpt`   |              | For `isolated` jobs: include this many recent messages of your DM   |
| `idle_only`    | `false`      | Wait while a turn runs in your DM                                   |
| `active_hours` |              | Run only between these times, `"HH:MM-HH:MM"`; `"22:00-06:00"` spans midnight |
| `model`        | `models`     | A model for this job                                                |
| `ask_to`       |              | `"deliver_to"`: ask you to approve `ask` tools; otherwise they are denied |

Exactly one of `prompt` and `prompt_file` is required. A `prompt_file` is a relative path inside the
agent's directory, and must exist when Giles starts.

Schedules and `active_hours` use the owner's time zone, else the agent's, else the system's.

- A reply of exactly `NO_REPLY` sends nothing and leaves nothing behind. Any other reply goes to
  your default identity, and is added to your DM with the agent there, so you can answer it.
- A `"dm"` job runs as its own turn in that DM, after any turn already running there. Until you've
  messaged the agent on that identity, it runs like an `isolated` job.
- Times missed while Giles was stopped are made up by one run when it starts. A time outside
  `active_hours` is skipped.
- With `ask_to`, approval prompts go to your default identity, and you answer them in your DM there.
  Before you have a DM with the agent, `ask` tools are denied.

The agent can also schedule jobs with the `schedule` tool when you ask, such as "remind me at 5" or
"every weekday at 9, ask me for my stand-up notes". Allow it with `schedule` in `tools`. Its jobs
run once or on a cron schedule, in your time zone.

The agent can list your jobs, including the ones in `agent.toml`, which it can't change. It can
cancel the jobs it scheduled. You can have up to 20 such jobs with each agent, and a scheduled job
can't schedule more jobs.

## Full example

The agent also uses `github`, a remote toolset added in the [web UI](web.md#adding-a-remote-server).

```toml
# ~/.giles/giles.toml
[core]
http_listen = "127.0.0.1:8080"

[owner]
identities = ["web:owner", "irc:libera.chat:account:stephen"]
default = "irc:libera.chat:account:stephen"
timezone = "America/Los_Angeles"

[provider.anthropic]
config = { api_key = { secret = "anthropic" } }

[provider.ollama]
component = "openai"
config = { base_url = "http://127.0.0.1:11434/v1" }

[channel.web]

[channel.libera]
component = "irc"
sandbox = { network = ["tcp:6697"] }

[channel.libera.config]
server = "irc.libera.chat:6697"
network = "libera.chat"
nick = "jeeves"
channels = ["#giles"]
sasl = { password = { secret = "libera" } }

[toolset.files]
command = "/opt/homebrew/bin/mcp-server-filesystem"
args = ["/Users/stephen/Documents"]
sandbox = { read = ["/Users/stephen/Documents"], write = ["/Users/stephen/Documents/Inbox"] }
idle_stop = "30m"
parallel = true
```

```toml
# ~/.giles/secrets.toml (mode 0600)
anthropic = "sk-ant-api03-..."
libera = "correct horse battery staple"
```

```toml
# ~/.giles/agents/jeeves/agent.toml
name = "Jeeves"
models = ["anthropic/claude-sonnet-4-5"]
compaction_model = "ollama/qwen3:8b"
tools = ["memory_*", "load_skill", "schedule", "github__*", "files__*"]
ask = ["github__create_*"]
max_iterations_jobs = 25
memory_cap = 2200
approval_timeout = "10m"
timezone = "America/Los_Angeles"
idle_reset = "12h"

[[jobs]]
id = "morning"
cron = "0 8 * * 1-5"
prompt_file = "morning.md"

[[jobs]]
id = "check-in"
cron = "*/30 * * * *"
session = "dm"
idle_only = true
active_hours = "08:00-22:00"
model = "ollama/qwen3:8b"
prompt = "Is there anything I should know about? Reply NO_REPLY if not."
```
