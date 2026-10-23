# Configuration

Giles reads its configuration once, when `giles serve` starts; after a change, restart it. Two kinds
of TOML file, both in the data root ([Installing and Running](running.md)):

- `giles.toml`: the core, the owner, component instances, and routes.
- `agents/<agent>/agent.toml`: one per agent.

Giles never writes these files. If anything in them is wrong, `giles serve` refuses to start and
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

| Key           | Default            | Meaning                                                           |
| ------------- | ------------------ | ----------------------------------------------------------------- |
| `http_listen` | `"127.0.0.1:8080"` | Address of the HTTP server (web chat, admin pages, login links)   |

To reach Giles from other machines (or from outside a container), listen on `"0.0.0.0:8080"` and put
a TLS-terminating reverse proxy in front of it. Giles binds the address at startup and refuses to
start if it can't (see [The Web Interface](running.md#the-web-interface)).

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
config = { server = "irc.libera.chat:6697" }

[channel.irc-oftc]
component = "irc"
config = { server = "irc.oftc.net:6697" }
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

The built-in channel is `web`. Any other component is a plugin, which Giles either launches itself
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

`oauth` uses the OAuth 2.0 client credentials grant: `token_url` (`https://`), `client_id`,
`client_secret` (a credential), and optional `scopes`.

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
| `idle_stop`   | stdio      | off       | Stop the server after this long without calls (a duration); it restarts on the next call |
| `sandbox`     | stdio      |           | Extra sandbox grants ([Sandbox](#sandbox))                    |
| `unsandboxed` | stdio      | `false`   | Run without a sandbox (logged as a warning at every launch)   |
| `url`         | remote     |           | The server's `https://` (or `http://`) URL                    |
| `auth`        | remote     | none      | `{ bearer = <credential> }`                                   |
| `insecure`    | remote     | `false`   | Allow plain `http://` to a host other than this machine       |
| `deadline`    | all        | `"5m"`    | How long one tool call may run                                |
| `parallel`    | all        | `false`   | The toolset's tools may run in parallel with each other and with read-only core tools |
| `skills`      | all        | `[]`      | Absolute paths of skill directories the toolset offers        |

Toolsets have no `config` table: a stdio server is configured through its `args`.

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
| `network` | `true` for all IP traffic, `false` for none, or TCP ports as `["tcp:443"]` (enforced on macOS, and on Linux with Landlock ABI 4, kernel 6.7 or later; on older Linux kernels all ports are open). Name lookups work with any network grant |
| `read`    | Absolute paths the plugin may read                                               |
| `write`   | Absolute paths the plugin may read and write                                     |
| `unix`    | Absolute paths of Unix sockets the plugin may connect to (on Linux before Landlock ABI 9, kernel 7.1, any entry allows connecting to every socket) |
| `min_abi` | Linux only: refuse to start the plugin unless the kernel's Landlock ABI is at least this |

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

### `[[routes]]`

Routes pick the agent for each conversation. They are tried in order and the first match wins; a
route matches when every key it sets matches. Conversation ids match exactly, capitalization
included. Messages in a conversation no route matches are dropped and logged, including
[chat commands](commands.md).

| Key            | Meaning                                                            |
| -------------- | ------------------------------------------------------------------ |
| `channel`      | Match this channel instance                                        |
| `kind`         | Match `"direct"` or `"group"` conversations (v1 ignores groups)    |
| `conversation` | Match this conversation id, as the channel reports it, e.g. `"#giles"` |
| `agent`        | Required: the agent, a directory under `agents/`                   |

```toml
[[routes]]
channel = "irc-libera"
conversation = "#giles"
agent = "jeeves"

[[routes]]                 # everything else
agent = "jeeves"
```

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
| `max_iterations`      | no limit        | Model requests per interactive turn                               |
| `max_iterations_jobs` | `25`            | Model requests per job turn                                       |
| `busy_mode`           | `"steer"`       | New messages during a turn: `"steer"` shows them to the model at the next tool step; `"queue"` keeps them for the next turn |
| `memory_cap`          | `2200`          | Characters per memory file the agent writes                       |
| `approval_timeout`    | `"10m"`         | How long an approval request waits before it is denied            |
| `timezone`            | system          | The agent's time zone, used when `[owner]` sets none              |
| `idle_reset`          | off             | Start a new session after this long without messages              |

Tool names are `<instance>__<tool>` for toolsets and bare for the core tools (`memory`,
`memory_search`, `load_skill`, `schedule`, `send_attachment`). `/stop` ends a turn with no
iteration limit.

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
nick = "jeeves"
channels = ["#giles"]
sasl_password = { secret = "libera" }

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

[[routes]]
channel = "libera"
conversation = "#giles"
agent = "jeeves"

[[routes]]
agent = "jeeves"
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
