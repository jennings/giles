# Configuration

`giles_core::config` reads and validates `giles.toml`, `secrets.toml`, and every
`agents/<agent>/agent.toml` (PLAN sections 1.1, 7.7, and 11.2). The user-facing reference is
[Configuration](../user/configuration.md).

## Loading

```rust
// The v1 built-ins and the process environment.
let config: Config = Config::load(&data_root)?;
// Explicit built-ins and environment (tests, or a core built with fewer features).
let options = LoadOptions { builtins, env: &|name| lookup(name) };
let config: Config = Config::load_with(&data_root, &options)?;
```

`Config::load` runs once, before `giles serve` builds its runtime; configuration reload is deferred
(D28). It reads files only through `DataRoot` (`config_file`, `secrets_file`, `agents_dir`), and
returns the first problem as a `ConfigError`. Everything else in the core takes the resulting
`Config` and never reads the files again (agent Markdown files are read per turn by their owners,
not here).

The steps:

1. Read `giles.toml` (UTF-8) and deserialize it into private `Raw*` structs. Every struct has
   `#[serde(deny_unknown_fields)]`; the only place unknown keys pass is an instance's `config`.
2. Validate `[core]` and `[owner]`.
3. Load `secrets.toml` ([Secrets](#secrets)).
4. Check that instance names are unique across the three kinds, ignoring ASCII case (they share
   the data directory namespace, `data/plugins/<instance>/`, and pid files, and macOS file systems
   are case-insensitive by default), then validate each instance into its typed form, resolving
   secret references.
5. Load every agent: each directory in `agents/` not starting with `.` must have a valid name and
   an `agent.toml`. Models are checked against the configured provider instances.
6. Validate `[[routes]]` against the channel instances and agents.

`Config` holds typed, validated values only: `InstanceName`, `AgentId`, `ModelRef`,
`std::time::Duration`, `jiff::tz::TimeZone`, `croner::Cron`, `url::Url`. Defaults are applied
during validation and exported as constants (`DEFAULT_HTTP_LISTEN`, `DEFAULT_MAX_BLOB`,
`DEFAULT_TOOL_DEADLINE`, `DEFAULT_MAX_ITERATIONS_JOBS`, `DEFAULT_MEMORY_CAP`,
`DEFAULT_APPROVAL_TIMEOUT`).

## Errors

`ConfigError` has public fields `file`, `position` (line and column, both from 1), `key` (a
`KeyPath`, root for file-level problems), `reason`, and `source` (an `io::Error` for read
failures). It displays as `<file>:<line>:<column>: <key>: <reason>`, leaving out what isn't known.

- TOML syntax errors use the parser's message and span. The parser's own `Display` is never used,
  because it quotes the source line.
- Type errors and unknown keys come from serde. The document is deserialized through
  `serde_path_to_error`, which tracks the key path (including the unknown key itself), and the
  `toml` error's span gives the position.
- Semantic errors (a route naming a missing agent, a key that doesn't apply to an instance's shape)
  are raised after deserialization with the key path. `ConfigError::located` then fills in the
  position: on the error path only, the file is parsed again into toml's span-keeping `DeTable`,
  and the key path is walked to the deepest part present (the key of a table entry, or an array
  item). A missing key therefore points at its parent table.

`KeyPath` displays as TOML writes keys: bare keys joined by `.`, other keys quoted, array indexes
in brackets (`toolset.files.skills[1]`, `owner."odd key"`).

## Instances

Each instance kind has a raw struct with every core-owned key as an `Option`, so validation can
tell "unset" from "default" and reject keys that don't apply to the instance's shape:

| Type                | Shapes (enum)                                                    |
| ------------------- | ---------------------------------------------------------------- |
| `ProviderInstance`  | Built-in only (provider plugins are deferred)                    |
| `ChannelInstance`   | `ChannelRuntime::{BuiltIn, Plugin(Launch), Remote(Remote)}`      |
| `ToolsetInstance`   | `ToolsetTransport::{Stdio(StdioServer), Http(Remote)}`           |

Component resolution is `Builtins::resolve(kind, component)`: a name in the built-in list for that
kind is `Component::BuiltIn`; anything else is `Component::Plugin` with binary
`giles-<kind>-<component>` (`plugin_binary`). The built-in list is a parameter, `Builtins`, so the
core can pass the components its Cargo features compiled in; `Builtins::V1` lists every v1
built-in (`anthropic` and `openai` providers, `web` channel). Finding the binary in
`<root>/plugins/` or on `$PATH` is the plugin host's job, at launch.

A stdio toolset's `command` is kept as written (an absolute path or a bare program name); resolving
it on the operator's `$PATH` and adding its install prefix to the sandbox grants happens at launch
(PLAN section 7.10).

Remote URLs (`address`, `url`) are parsed with `url`, must be `http` or `https` with a host and no
credentials, and plain `http` needs `insecure = true` unless the host is `localhost`,
`127.0.0.0/8`, or `::1` (PLAN section 7.6).

The `config` table becomes a `PluginConfig`, a JSON object for `Configure` (PLAN section 7.7).
TOML datetimes become their TOML text; infinite and NaN floats, which JSON can't carry, are
errors.

## Secrets

`secrets.toml` is optional. If it exists it must be a regular file with no group or other
permission bits (`mode & 0o077 == 0`, checked on the file `fs::metadata` follows) and a flat table
of strings; otherwise loading fails, even if nothing refers to it.

A reference is an inline table with exactly one key, `secret` or `env`, whose value is a non-empty
string. References are resolved in three places:

- Anywhere in an instance's `config` table, including inside arrays. A one-key `secret`/`env`
  table there is always a reference, so components can't use that shape as data.
- Credentials: `auth.bearer`, `auth.oauth.client_secret`, and the values of a stdio toolset's
  `env`. These also accept a literal string.

`{ env = ... }` reads the process environment through `LoadOptions::env`; an unset variable or a
non-UTF-8 value is an error. A missing secret names the secrets file, and says when it doesn't
exist.

Keeping values out of output:

- Credentials are `Secret`, whose `Debug` and `Display` print `<redacted>`; `Secret::expose` is
  the only way to the value.
- `PluginConfig` remembers the key paths that held references and the values they resolved to.
  Its `Debug` and `redacted()` put `<redacted>` at those paths; only `to_json()`, for `Configure`,
  has the values.
- `scrub(text, &[&Secret])` replaces secret values in text Giles didn't write but logs or shows:
  a component's `Configure` error (`PluginConfig::scrub`), or a plugin's or MCP server's stderr,
  with the instance's `auth`, `env`, and `PluginConfig::secrets()` values. It makes one pass,
  taking the longest matching value at each position, so overlapping values can't garble the
  output and an inserted `<redacted>` is never matched again. Values shorter than
  `MIN_SCRUB_LEN` (4 bytes) are skipped: replacing every `1` or `443` in a message would garble
  it, and a value that short protects nothing.
- `config`, `auth`, and `env` are deserialized as raw `toml::Value`s and checked by hand, because
  serde's type errors quote the offending value (`invalid type: string "sk-...", expected a map`).
  The same goes for `secrets.toml`, whose errors give only the key and the value's type.
- TOML syntax errors use the parser's message and position, never its `Display`, which quotes the
  source line.

`Config`'s `Debug` output is therefore safe to log. Literal strings in a `config` table are not
secrets as far as Giles knows; the user guide tells operators to use references for credentials.

## Values

| Value       | Parser                     | Notes                                               |
| ----------- | -------------------------- | --------------------------------------------------- |
| Names       | `is_valid_name`            | `[a-zA-Z0-9-]+`; instances, components, agents, jobs |
| Durations   | `parse_duration`           | `<int><unit>` parts, units `ms s m h d`; a small hand-written parser, so errors can say exactly what is expected |
| Sizes       | `parse_size`               | integer bytes, or `<int>[ ]<B/KiB/MiB/GiB>`         |
| Models      | `ModelRef: FromStr`        | split at the first `/`; the provider must be a valid instance name |
| Time zones  | `jiff::tz::TimeZone::get`  | the system tzdb, else the copy bundled by jiff's `tzdb-bundle-always` (containers often lack `/usr/share/zoneinfo`) |
| Cron        | `croner`                   | five fields only (seconds and years disallowed)     |
| Identities  | `Identity::new`            | `<namespace>:<id>`; the namespace may contain `:`, so matching compares the whole string |

## v1 Choices

- `[owner]` replaces users and roles (PLAN section 1.1): there is no `[roles]`, no `users/`, no
  `unknown_senders`, no route `user`, and no job `deliver_to` (every job delivers to the owner);
  those keys are rejected as unknown. `ask_to = "deliver_to"` keeps PLAN's spelling and means the
  owner. The owner's time zone is `[owner] timezone`, which stands in for `user.toml`'s.
- `prompt_file` must exist at startup, so a typo shows up then rather than at the first run.
- A toolset with neither `command` nor `url` runs the first-party plugin
  `giles-toolset-<component>`, mirroring how channel plugins are found.
