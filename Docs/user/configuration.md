# Configuration

Giles reads its configuration once, when `giles serve` starts; after a change, restart it. Two kinds
of TOML file, both in the data root ([Installing and Running](running.md)):

- `giles.toml`: the core, the owner, and component instances.
- `agents/<agent>/agent.toml`: one per agent.

Giles never writes `giles.toml`. It writes `agent.toml` and `SOUL.md` only for an agent you
create in the web UI ([Agents](web.md#agents)); that agent is added to the running configuration
at once, and its files are ordinary ones you can edit by hand like the rest (edits take effect at
the next restart). If anything in these files is wrong, `giles serve` refuses to start and
prints one error naming the file, the line and column when known, the key, and what is wrong (the
data root's directories are still created, so a first start without `giles.toml` sets them up):

```text
Error: /home/me/.giles/giles.toml:12:12: channel.libera.max_blob: invalid size "50MB": expected a
number of bytes or an integer and a unit (B, KiB, MiB, GiB), e.g. "50 MiB"
```

Unknown keys are errors too, so a typo never silently does nothing. The one exception is an
instance's `config` table, which belongs to the component (see [Instances](#instances)).

## Value Formats

- **Names** of instances, components, agents, and jobs use only letters, digits, and `-`:
  `irc-libera`, `jeeves`, `check-in`.
- **Durations** are strings of one or more `<integer><unit>` parts, with units `ms`, `s`, `m`, `h`,
  and `d`: `"30s"`, `"10m"`, `"1h30m"`.
- **Sizes** are a number of bytes, or a string with a unit `B`, `KiB`, `MiB`, or `GiB`: `52428800`,
  `"50 MiB"`, `"512KiB"`.
- **Models** are `<provider instance>/<model id>`, split at the first `/`: in
  `"openrouter/anthropic/claude-sonnet-4.5"` the instance is `openrouter` and the model id is
  `anthropic/claude-sonnet-4.5`.
- **Time zones** are IANA names, e.g. `"America/Los_Angeles"`. Giles uses the system's time zone
  database, or a copy built into it when the system has none.
- **Identities** are `<namespace>:<native id>`, e.g. `"whatsapp:+15551234567"`,
  `"email:me@example.com"`, `"irc:libera.chat:account:stephen"`. Each channel's documentation
  gives its namespace and id format.

## `giles.toml`

### `[core]`

Settings of the core itself. Optional.

| Key                 | Default            | Meaning                                                           |
| ------------------- | ------------------ | ----------------------------------------------------------------- |
| `http_listen`       | `"127.0.0.1:8080"` | Address of the HTTP server (web chat, admin pages, login links)   |
| `retired_instances` | `[]`               | Names of instances you removed on purpose, whose data stays       |
| `journal_mode`      | `"wal"`            | SQLite journal mode of `data/giles.db`: `wal`, `delete`, `truncate`, `persist` |

`retired_instances` is a list of instance names, each used by no instance table. It tells the
startup check for a half-done rename to ignore data kept under that name; see
[Renaming an instance](cli.md#giles-instance-rename-old-new).

To reach Giles from other machines (or from outside a container), listen on `"0.0.0.0:8080"` and put
a TLS-terminating reverse proxy in front of it. Giles binds the address at startup and refuses to
start if it can't (see [The Web Interface](running.md#the-web-interface)).

#### `journal_mode`

The journal is how SQLite keeps a write from corrupting the database if the machine dies in the
middle of it. The default, `"wal"` (write-ahead log), lets Giles read the database while it writes
and is what Giles is built for. Leave it alone unless Giles can't start with it: WAL needs shared
memory, which network file systems (NFS, SMB, some container and FUSE mounts) don't provide. There
Giles stops at startup with an error that names this option, and `journal_mode = "delete"` is the
way out. Moving `~/.giles` to a local disk is better still.

| Value        | Journal                                                                         |
| ------------ | ------------------------------------------------------------------------------- |
| `"wal"`      | Write-ahead log: reads don't wait for writes. Needs shared-memory support       |
| `"delete"`   | Rollback journal file, deleted after each write. Works everywhere               |
| `"truncate"` | Like `delete`, but empties the journal file instead of deleting it              |
| `"persist"`  | Like `delete`, but keeps the journal file and marks it unused                   |

Values are case-insensitive. SQLite also has `memory` and `off`; Giles refuses them at startup
because a crash or power loss during a write could then corrupt the database or lose data.

Switching needs nothing from you except a restart, as with every `[core]` setting: change the
value and start Giles, and it converts the existing database in place, in either direction. (Stop
any other program that has `giles.db` open, such as the `sqlite3` shell, first. Leaving WAL needs
the database to itself.)

In the `delete`, `truncate`, and `persist` modes:

- A write waits for reads in progress, and reads wait for a write that is committing, instead of
  running at the same time. Giles's reads and writes are short, so you won't notice, and none
  fails. A backup (`giles backup`, or the one before an upgrade) holds writes for as long as it
  takes to copy the database, which grows with the size of the database.
- Writes use a full sync to disk, which is slower than in WAL mode but keeps the database safe on
  a power loss.
- `giles.db-journal` exists while a write runs, and in `truncate` and `persist` also between
  writes; it is removed when Giles stops.

See [Backups](backups.md) for restoring in any mode.

### `[owner]`

Giles v1 serves one person, the owner. Required.

| Key          | Default                  | Meaning                                                     |
| ------------ | ------------------------ | ----------------------------------------------------------- |
| `identities` | required                 | The owner's identities, at least one, no duplicates         |
| `default`    | the only identity        | Where jobs and other proactive messages go; required when there are several identities |
| `timezone`   | the agent's, else system | The owner's time zone, for message times and job schedules  |

Messages from any identity not listed here are dropped and logged, as are messages a channel
couldn't verify the sender of, and messages in group conversations, which v1 doesn't support.

An identity is `<namespace>:<id>`. Each channel decides its namespace and the form of its ids, for
example:

| Channel  | Example identity                  |
| -------- | --------------------------------- |
| Web      | `web:owner`                       |
| IRC      | `irc:libera.chat:account:stephen` |
| Email    | `email:me@example.com`            |
| WhatsApp | `whatsapp:+15551234567`           |

A message counts as the owner's if its sender, or any other id the channel reports for the same
person (WhatsApp reports both the phone number and an internal id), is listed.

Identities match exactly, capitalization included, except that email domains ignore case
(`email:me@Example.com` matches `me@example.com`, but not `Me@example.com`). A message from an
unlisted identity is logged with the identity as the channel reported it, so you can copy it here.

```toml
[owner]
identities = ["whatsapp:+15551234567", "email:me@example.com"]
default = "whatsapp:+15551234567"
timezone = "America/Los_Angeles"
```

### Instances

Providers, channels, and toolsets are configured as instances, one table each: `[provider.<name>]`,
`[channel.<name>]`, `[toolset.<name>]`. The instance name is permanent: sessions, history, tool
names (`<instance>__<tool>`), and the instance's data directory are keyed by it. Names are unique
across all three kinds, ignoring case (`Libera` and `libera` clash).

`component` says what runs the instance and defaults to the instance name. If Giles has a built-in
component of that kind with that name, it's used; otherwise the instance runs the plugin
`giles-<kind>-<component>`, found in `<data root>/plugins/` or on `$PATH`. Two IRC networks are two
instances of one component:

```toml
[channel.irc-libera]
component = "irc"          # runs giles-channel-irc
config = { server = "irc.libera.chat", nick = "jeeves", network = "libera.chat" }

[channel.irc-oftc]
component = "irc"
config = { server = "irc.oftc.net", nick = "jeeves", network = "oftc.net" }
```

Every key of an instance table belongs to Giles, except `config`: that table is passed to the
component as JSON, unchecked by Giles, and the component validates it when it starts. Its keys are
documented with each component. A key that doesn't apply to an instance (for example `address` on
the built-in web channel) is an error.

#### Providers

The built-in providers are `anthropic` (the Anthropic Messages API) and `openai` (any
OpenAI-compatible Chat Completions API: OpenAI, OpenRouter, Ollama). Provider plugins aren't
supported yet, so `component` must name one of these.

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

The built-in channel is `web`; its `config` keys (`public_url`, `proxy`) are in
[Web Chat](web.md#logging-in). Any other component is a plugin, which Giles either launches itself
or, with `address`, connects to where someone else runs it.

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

An attachment larger than `max_blob` (or one the plugin fails to hand over) doesn't stop the
message: the agent gets the message without it, plus a line such as
`[attachment "clip.mov" (video/quicktime, 73400320 bytes) not received: it is larger than this
channel's limit of 52428800 bytes]`, so it can tell the sender.

Plain `http://` is accepted without `insecure` only for `localhost`, `127.0.0.0/8`, and `::1`:
without TLS, anyone on the network path could inject messages. Addresses can't contain a user name
or password; use `auth`:

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

`oauth` uses the OAuth 2.0 client credentials grant: `token_url` (`https://`, or `http://` on this
machine), `client_id`, `client_secret` (a credential), and optional `scopes`.

##### Remote Plugins

A channel with `address` is a plugin you run yourself, on this machine or another one (see
[Writing a Plugin](writing-a-plugin.md#running-a-plugin-remotely)); Giles never starts, stops, or
sandboxes it, and the plugin keeps its own state.

- Giles connects at startup. While the plugin can't be reached (or refuses Giles' token, or the
  certificate doesn't check out), Giles runs without that channel and tries again after 1 s, then
  2 s, 4 s, and so on up to every 5 minutes. A lost connection is retried the same way, and so is
  any other failure, since only you can see when the plugin is fixed. `giles status` shows the
  instance as `down, retrying at ...` with the last error, or as `running` with no pid.
- With `https://`, the plugin's certificate must be valid for the host in `address` and issued by
  a public certificate authority that Giles knows (its built-in copy of the Mozilla list; the
  system's trust store isn't used). `insecure = true` only allows plain `http://`; it never turns
  certificate checks off.
- `auth = { bearer = ... }` sends `Authorization: Bearer <token>` on every call. With `oauth`,
  Giles asks the token endpoint for an access token before connecting (HTTP Basic with the client
  id and secret) and sends that instead, getting a new one a minute before it expires (or halfway
  through its life if it's short). Without `auth`, Giles sends no credentials.
- When Giles stops, it tells the plugin it's disconnecting and closes the connection; the plugin
  keeps running, and messages that arrive meanwhile wait in it until Giles connects again.

#### Toolsets

A toolset is an MCP server. It runs one of three ways:

- `command`: a stdio server Giles launches, an absolute path or a program name looked up on
  `$PATH`, with `args`.
- `url`: a remote server over streamable HTTP. Giles doesn't run or sandbox it.
- Neither: a stdio server from the first-party plugin `giles-toolset-<component>` (the component
  defaults to the instance name).

| Key           | Applies to | Default   | Meaning                                                       |
| ------------- | ---------- | --------- | ------------------------------------------------------------- |
| `command`     | stdio      |           | The program to run                                            |
| `component`   | stdio      | name      | The plugin to run when there's no `command`                   |
| `args`        | stdio      | `[]`      | Arguments                                                     |
| `env`         | stdio      | `{}`      | Environment variables to set, name to credential              |
| `idle_stop`   | stdio      | off       | Stop the server after this long without calls (a duration); its tools stay offered and it restarts on the next call |
| `sandbox`     | stdio      |           | Extra sandbox grants ([Sandbox](#sandbox))                    |
| `unsandboxed` | stdio      | `false`   | Run without a sandbox (logged as a warning at every launch)   |
| `url`         | remote     |           | The server's `https://` (or `http://`) URL                    |
| `auth`        | remote     | none      | `{ bearer = <credential> }`                                   |
| `insecure`    | remote     | `false`   | Allow plain `http://` to a host other than this machine       |
| `deadline`    | all        | `"5m"`    | How long one tool call may run; then it's cancelled and the model told it timed out |
| `parallel`    | all        | `false`   | The toolset's tools may run in parallel with each other and with read-only core tools, up to 8 at once |
| `skills`      | all        | `[]`      | Absolute paths of skill directories the toolset offers        |

Toolsets have no `config` table: a stdio server is configured through its `args`. No plugin may
write to a `skills` directory: Giles refuses a `skills` path inside the plugins' data directories,
and any `write` or `unix` sandbox grant that overlaps one.

Giles looks a stdio server's `command` up on its own `$PATH` and runs it in the
[sandbox](#sandbox), granting it read and execute access to the command's install prefix: the
directory above `bin/` (`/opt/homebrew` for `/opt/homebrew/bin/mcp-server-filesystem`, or a Python
virtual environment), else the command's own directory. If the command is a script, its
interpreter gets the same, including one found through `#!/usr/bin/env node`, and the server's
`PATH` is those directories followed by `/usr/bin:/bin`. A prefix that is your home directory (or
contains it) or would contain the data root narrows to the command's directory: `~/bin/server`
gets `~/bin`, never `~`.

A prefix grants everything under it: all of `/opt/homebrew` (including its `etc` and `var`) for a
Homebrew command, all of `~/.local` for `~/.local/bin/server`, all of `~/.cargo` (including
`credentials.toml`) for `~/.cargo/bin/server`. To keep a server away from those, install it in a
prefix of its own, such as a Python virtual environment or `npm install --prefix`. And don't give
any toolset a `write` grant on an interpreter's installation (a Node or Python directory): another
toolset runs that interpreter.

- Install servers ahead of time. `npx -y` or `uvx` at run time need the network and writable
  package caches, which the sandbox doesn't grant.
- A server that needs the network needs a grant, such as `sandbox = { network = ["tcp:443"] }`.
- The server's environment is its `env` plus `LANG` from Giles, `HOME` and `TMPDIR` inside its data
  directory, the `PATH` above, and `GILES_PLUGIN_DATA` (its data directory); nothing else from
  Giles' environment reaches it. `env` can't set `HOME`, `TMPDIR`, `PATH`, or
  `GILES_PLUGIN_DATA`.

Giles starts every toolset when it starts, and keeps it running: a stdio server that crashes is
restarted, and a remote server that can't be reached is retried, both with a growing delay
([Installing and Running](running.md#running-the-daemon)). It remembers each toolset's list of tools
in `<data root>/cache/toolsets/`, so a stdio server's tools are offered to the model even while the
server is stopped (for `idle_stop`, or after a crash) and from the moment Giles starts, before the
server answers. The remembered list is used only for the same program (by content) and the same
`[toolset.<name>]` table, and every start of the server lists the tools afresh. A list the server
changes while it runs reaches conversations that start afterwards.

Images, audio, and files a tool returns are kept like attachments (up to 50 MiB each): the model
sees them if its provider takes that type, and can send them on with `send_attachment`. The other
way, the model can give an attachment to a stdio toolset's tool: Giles copies it into the server's
`TMPDIR` for the length of the call and passes the file's path. Remote toolsets don't get
attachments.

```toml
[toolset.files]
command = "/opt/homebrew/bin/mcp-server-filesystem"
args = ["/Users/me/Documents"]
sandbox = { read = ["/Users/me/Documents"] }
idle_stop = "30m"

[toolset.github]
url = "https://api.githubcopilot.com/mcp/"
auth = { bearer = { env = "GITHUB_TOKEN" } }
deadline = "2m"
```

### Sandbox

Every plugin Giles launches (channel plugins and stdio toolsets) runs in a sandbox. By default it
can read its own binary, system libraries, TLS certificates, and DNS configuration, and read and
write only its own data directory, `<data root>/data/plugins/<instance>/`. Channel plugins may use
the network; stdio toolsets may not. `sandbox` grants more:

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

On Linux the sandbox is Landlock plus a system call filter, and what Landlock can enforce depends
on the kernel. Giles uses the newest Landlock ABI the kernel offers and logs it at every launch
(`sandboxing with Landlock and seccomp landlock_abi=6`), with a warning that lists what's missing
when the kernel lacks something the plugin's grants rely on. To refuse older kernels instead, set
`min_abi`; a plugin whose kernel is below it doesn't start (the log says why):

| `min_abi` | Kernel | Guarantees                                                                       |
| --------- | ------ | -------------------------------------------------------------------------------- |
| 1         | 5.13   | Files: read, write, and execute grants (the minimum; Giles needs Landlock)       |
| 3         | 6.2    | Files can't be truncated outside write grants by Landlock itself                 |
| 4         | 6.7    | `network` port lists are enforced, and plugins can't listen on TCP ports         |
| 6         | 6.12   | The plugin can't signal processes outside its sandbox, Giles included            |
| 9         | 7.1    | `unix` grants allow exactly the listed sockets (they must exist when the plugin starts) |

On Linux, grants must exist when the plugin starts: Landlock grants files, not names, so a path
created later isn't covered.

No grant may touch the data root, except the instance's own data directory: a path inside the
data root, or one containing it (such as your home directory when the data root is `~/.giles`), is
an error. Otherwise a plugin could read `secrets.toml` or edit `giles.toml`. The same goes for the
private directory holding Giles' sockets when they live outside the data root. Paths that go
through a symbolic link are checked where the link leads, as it is when Giles starts; a link
created or changed later isn't, so don't grant directories that other programs can rewire.

`write` and `unix` grants also may not touch a directory holding a program Giles runs: the
`giles` binary itself, and each plugin or toolset command, wherever Giles finds it
(`<data root>/plugins/`, then `$PATH`). Granting `write = ["/opt/homebrew"]` would let a plugin
replace `giles` or another plugin. Reading those directories is fine.

A plugin can create symbolic links wherever it may write, so Giles never follows links there: a
plugin won't start if its data directory is a symbolic link, and a grant inside its data directory
or a `write` grant means exactly the path written, even if the plugin later puts a link there.
Write such grants with the real path, in the same upper and lower case as on disk.

`unsandboxed = true` turns the sandbox off for one instance, and can't be combined with `sandbox`.
Built-in components run inside Giles and remote plugins run elsewhere, so neither takes these keys.

### Secrets

Keep credentials out of `giles.toml`. Put them in `secrets.toml` in the data root, a flat table of
names to strings:

```toml
anthropic = "sk-ant-..."
github = "ghp_..."
```

Giles refuses to start if group or others have any access to `secrets.toml`; make it private with
`chmod 600 ~/.giles/secrets.toml`. The file is optional when nothing refers to it.

Refer to a secret with `{ secret = "<name>" }`, or take a value from Giles' own environment with
`{ env = "<VARIABLE>" }`. A reference works:

- anywhere in an instance's `config` table, at any depth (a table whose only key is `secret` or
  `env` is always a reference there);
- as a credential: `auth.bearer`, `auth.oauth.client_secret`, and the values of a toolset's
  `env`. A credential may also be a plain string.

```toml
[provider.anthropic]
config = { api_key = { secret = "anthropic" } }

[toolset.github]
url = "https://api.githubcopilot.com/mcp/"
auth = { bearer = { env = "GITHUB_TOKEN" } }
```

Giles resolves references when it starts and fails if a secret is missing or a variable is unset.
Components receive plain values and never read `secrets.toml` or the environment themselves. Error
messages and logs name the key and the secret or variable, never the value; where Giles shows an
instance's `config`, values that came from references read `<redacted>`.

### Routes

Routes pick the agent for each conversation, and they aren't in `giles.toml`: Giles stores them
and you manage them in the web UI ([Routes](web.md#routes)), where a change applies to the next
message with no restart.

Routes are tried in order and the first match wins. A route matches when every key it sets matches.
Conversation ids match exactly, capitalization included. Messages in a conversation no route
matches are dropped and logged, including [chat commands](commands.md).

| Key            | Meaning                                                            |
| -------------- | ------------------------------------------------------------------ |
| channel        | Match this channel instance                                        |
| kind           | Match `direct` or `group` conversations (v1 ignores groups)        |
| conversation   | Match this conversation id, as the channel reports it, e.g. `#giles` |
| agent          | Required: the agent, a directory under `agents/`                   |

A route with no keys but the agent matches everything, so it belongs last. The web UI checks a
route like it checks configuration: the channel instance and the agent must exist, and a route that
could never be used is refused: one an earlier route already covers, such as a duplicate, or a
change that would put a route before one it would hide.

Earlier versions kept routes in `giles.toml`:

```toml
[[routes]]
channel = "irc-libera"
conversation = "#giles"
agent = "jeeves"

[[routes]]                 # everything else
agent = "jeeves"
```

If your `giles.toml` has `[[routes]]` like these, Giles imports them the first time it starts after
the upgrade, in order, after checking them like any configuration (a route to a channel instance or
agent that doesn't exist stops startup and names the route). It does this once. After that it never
reads `[[routes]]` again, even if you delete every route in the web UI, and it logs a warning at
every start while `giles.toml` still has them, and the Routes page says so: remove them from the
file. A fresh install has no routes: add them in the web UI.

## `agents/<agent>/agent.toml`

Each directory under `agents/` is an agent, named by the directory, and must contain `agent.toml`.
Files in `agents/` and directories whose names start with `.` are ignored.

| Key                   | Default         | Meaning                                                           |
| --------------------- | --------------- | ----------------------------------------------------------------- |
| `name`                |                 | Display name                                                      |
| `models`              | required        | Models in order; v1 uses the first                                |
| `compaction_model`    | `models`        | The model that summarizes old history when a session gets long    |
| `tools`               | `[]`            | Tools the agent may use: globs over tool names, e.g. `"github__*"`, `"memory*"` |
| `ask`                 | `[]`            | Globs of tools that need your approval before each call           |
| `max_iterations`      | no limit        | Model requests with tools per interactive turn: tools run after at most N−1 of them, then one final request without tools |
| `max_iterations_jobs` | `25`            | The same for job turns                                            |
| `busy_mode`           | `"steer"`       | New messages during a turn: `"steer"` shows them to the model at the next tool step; `"queue"` keeps them for the next turn |
| `memory_cap`          | `2200`          | Characters per memory file the agent writes                       |
| `approval_timeout`    | `"10m"`         | How long an approval request waits before it is denied            |
| `timezone`            | system          | The agent's time zone, used when `[owner]` sets none              |
| `idle_reset`          | off             | Start a new session after this long without messages              |

Tool names are `<instance>__<tool>` for toolsets and bare for the core tools (`memory`,
`memory_search`, `load_skill`, `schedule`, `send_attachment`). A call to a tool the agent isn't
allowed, or one its toolset no longer offers, fails with `tool unavailable` and nothing runs.

When a turn reaches its iteration limit, the tools the model asked for in that last request don't
run; Giles makes one more request without tools, so the model tells you where it stopped. `/stop`
ends a turn with no iteration limit.

When a conversation gets long (its next request would fill about 80% of the model's context
window), Giles first has `compaction_model` summarize the older messages, keeping facts, decisions,
and open tasks, and the model then sees the summary plus the most recent messages. The full
transcript stays in the database. A cheaper model works well for this, as long as its own context
window is large enough to read a good part of the conversation at once. Compaction also refreshes
the memory shown to the model, so what it saved to memory during the conversation now shows in its
prompt. Giles takes a model's context window from the provider; for OpenAI-compatible servers the
`context_window` you set in the provider's config ([Providers](providers.md)) applies too, also
when the server doesn't list the model under that exact id. Without either, Giles assumes 128,000
tokens. If writing the summary fails, Giles logs it and goes on without it, and tries again on
your next message.

With `busy_mode = "steer"`, a message you send while the agent is running tools reaches it as soon
as the tools already running finish: tool calls it asked for but hadn't started yet are skipped
(the model is told "Skipped to read a new message") so it can read your message first. With
`"queue"`, the agent finishes what it's doing and answers your message in a new turn.

`ask` uses the same globs as `tools`: a call to a matching tool waits until you answer the prompt
Giles sends with `yes <code>`, `yes! <code>` (also allow that tool for the rest of the
conversation's session), or `no <code>` (see [Chat Commands](commands.md)). Nobody answering
within `approval_timeout` denies it, and so does any other message from you. The wait doesn't
count against the toolset's `deadline`.

### `[[jobs]]`

Scheduled prompts. Each job's output is sent to the owner's default identity, unless the reply is
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

Exactly one of `prompt` and `prompt_file` is required. A `prompt_file` must exist when Giles starts
and be a relative path inside the agent's directory. Schedules and `active_hours` use the owner's
time zone, else the agent's, else the system's.

How jobs run:

- A reply of exactly `NO_REPLY` sends nothing and leaves nothing behind. Any other reply is sent to
  your default identity (retried until its channel is up) and added to your DM with the agent on
  that identity, so you can answer it there.
- A `"dm"` job runs as its own turn in that DM, after any turn already running there; until you
  have messaged the agent on that identity it runs like an `isolated` job.
- Times missed while Giles was stopped are made up by one run when it starts. A time outside
  `active_hours` is skipped.
- With `ask_to`, approval prompts are sent to your default identity; answer them in your DM there.
  Before you have a DM with the agent, `ask` tools are denied.

The agent can also schedule jobs itself with the `schedule` tool when you ask ("remind me at 5",
"every weekday at 9, ask me for my stand-up notes"): one-off or on a cron schedule, in your time
zone. It can list your jobs (and the ones in `agent.toml`, which it can't change) and cancel yours.
Allow it with `schedule` in `tools`. You can have up to 20 such jobs with each agent, and a
scheduled job can't schedule more jobs.

## Full Example

This configuration, with `GITHUB_TOKEN` set in Giles' environment, is checked by Giles' test suite,
so it stays valid.

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

[toolset.github]
url = "https://api.githubcopilot.com/mcp/"
auth = { bearer = { env = "GITHUB_TOKEN" } }
deadline = "2m"

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
tools = ["memory", "memory_search", "load_skill", "schedule", "github__*", "files__*"]
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
