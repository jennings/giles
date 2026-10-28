# Configuration

`giles_core::config` reads and validates `giles.toml`, `secrets.toml`, and every
`agents/<agent>/agent.toml` (PLAN sections 1.1, 7.7, and 11.2). The user-facing reference is
[Configuration](../user/configuration.md).

## Loading

```rust
// The v1 built-ins and the process environment.
let config: Config = Config::load(&data_root)?;
// Explicit built-ins, environment, and giles binary (tests, or a core built with fewer features).
let options = LoadOptions { builtins, env: &|name| lookup(name), giles_exe: Some(exe) };
let config: Config = Config::load_with(&data_root, &options)?;
```

`Config::load` runs once, before `giles serve` builds its runtime; configuration reload is deferred
(D28). It reads files only through `DataRoot` (`config_file`, `secrets_file`, `agents_dir`), and
returns the first problem as a `ConfigError`. Everything else in the core takes the resulting
`Config` and never reads the files again (agent Markdown files are read per turn by their owners,
not here).

The steps:

1. Read `giles.toml` (UTF-8) and deserialize it into private `Raw*` structs through
   `parse_toml_lenient`. The structs have no `#[serde(deny_unknown_fields)]`: the document goes
   through `serde_ignored` (inside `serde_path_to_error`), so the keys the structs don't have are
   exactly the ones serde skips, and the set of recognized keys can't drift from the types. Each
   skipped key becomes an `UnknownKey` (file, position, full key path) in `Config::unknown_keys`;
   only the outermost key of an unknown table is reported. An instance's `config` and `env` tables
   and `[[routes]]` are raw `toml::Value`s that serde never skips, so they produce none. `auth`
   tables are checked by hand in `instances.rs` (they hold credentials, so serde never sees them)
   and report their own unknown keys through `Ctx::unknown`. `[toolset.<name>]` tables are kept as
   `toml::Value`s too (`RawGiles.toolset`): one with `url` is set aside
   ([Ignored toolset tables](#ignored-toolset-tables)), and each other one is then deserialized into
   a `ToolsetSpec` the same lenient way, its unknown keys and errors at their full key path.
2. Validate `[core]` and `[owner]`. `core.retired_instances` holds valid instance names that appear
   once and that no `[provider.*]`, `[channel.*]`, or `[toolset.*]` table uses (an ignored
   `[toolset.*]` table doesn't count); the core only stores them, for the startup check in
   [Instance Rename](instance-rename.md#startup-check). A toolset stored in the database can't be
   checked here, since loading reads no database: the toolset manager refuses a stored toolset with
   a retired name when it is saved and at startup. `core.journal_mode` is parsed with
   `giles_store::JournalMode::from_str` (case-insensitive; `memory` and `off` and anything unknown
   are errors at `core.journal_mode`) and defaults to WAL ([Storage](storage.md#journal-modes));
   `Runtime::start` passes it to the store.
3. Load `secrets.toml` ([Secrets](#secrets)).
4. Check that instance names are unique across the three kinds, ignoring ASCII case (they share the
   data directory namespace, `data/plugins/<instance>/`, and pid files, and macOS file systems are
   case-insensitive by default), then validate each instance into its typed form, resolving secret
   references. Ignored toolset tables take no part. A toolset goes through `Ctx::toolset`, the same
   function [`validate_toolset`](#validating-one-toolset) uses.
5. Load every agent: each directory in `agents/` not starting with `.` must have a valid name and an
   `agent.toml`. Models are checked against the configured provider instances.
6. Keep `[[routes]]` as written (`Config::legacy_routes`, unvalidated). Routes are stored in the
   database and managed in the web UI; the one-time import of these rules, which validates them, is
   [Identity and Routing](identity-and-routing.md#routing).

`Config::validate_agent(root, id, source)` runs step 5 for one agent that isn't on disk yet: the id
as a directory name, then the same parser and checks as a loaded `agent.toml` (models against this
configuration's provider instances, job prompt files in the agent's directory). It writes nothing;
the web UI validates an agent it creates with it before writing the files
([Web channel](web-channel.md)).

`Config` holds typed, validated values only: `InstanceName`, `AgentId`, `ModelRef`,
`std::time::Duration`, `jiff::tz::TimeZone`, `croner::Cron`, `url::Url`. Defaults are applied during
validation and exported as constants (`DEFAULT_HTTP_LISTEN`, `DEFAULT_MAX_BLOB`,
`DEFAULT_TOOL_DEADLINE`, `DEFAULT_MAX_ITERATIONS_JOBS`, `DEFAULT_MEMORY_CAP`,
`DEFAULT_APPROVAL_TIMEOUT`).

## Errors

`ConfigError` has public fields `file`, `position` (line and column, both from 1), `key` (a
`KeyPath`, root for file-level problems), `reason`, and `source` (an `io::Error` for read failures).
It displays as `<file>:<line>:<column>: <key>: <reason>`, leaving out what isn't known.

- TOML syntax errors use the parser's message and span. The parser's own `Display` is never used,
  because it quotes the source line.
- Type errors come from serde. The document is deserialized through `serde_path_to_error`, which
  tracks the key path, and the `toml` error's span gives the position. `agent.toml` is strict: its
  `Raw*` structs keep `#[serde(deny_unknown_fields)]`, so an unknown key there is one of these
  errors.
- Semantic errors (a route naming a missing agent, a key that doesn't apply to an instance's shape)
  are raised after deserialization with the key path. `ConfigError::located` then fills in the
  position: on the error path only, the file is parsed again into toml's span-keeping `DeTable`, and
  the key path is walked to the deepest part present (the key of a table entry, or an array item). A
  missing key therefore points at its parent table.

`KeyPath` displays as TOML writes keys: bare keys joined by `.`, other keys quoted, array indexes in
brackets (`toolset.files.skills[1]`, `owner."odd key"`).

## Unknown keys

`giles.toml` alone is lenient about keys it doesn't know: `Config::unknown_keys` lists each as an
`UnknownKey { file, position, key }`, displayed as
`<file>:<line>:<column>: unknown key `<key path>` (ignored)`. The position is found with
`error::locate`, as for errors. Only a load that succeeds returns them; an error in the file,
wherever it is, wins over any number of unknown keys, so a typo next to a wrong-typed key doesn't
change the error.

`serde_ignored` reports a key when the derived `Deserialize` skips it, so it covers every typed
table (`[core]`, `[owner]`, instance tables) and recognizes exactly what the `Raw*` structs declare.
A table serde skips as a whole is one entry (`[cores]` is `cores`, not `cores.x`). Hand-checked
tables report theirs through `Ctx::unknown`: the keys of an `auth` table next to its method, and of
an `oauth` table.

Some tables stay strict, because a typo there would not be harmless:

- `SandboxSpec` keeps `#[serde(deny_unknown_fields)]`: a misspelled `network` or `min_abi` would
  leave the default (network on, no ABI floor) and widen the plugin's sandbox. The error is
  positioned at the key like any other.
- An `auth` table with no `bearer` or `oauth` is an error: ignoring it would leave a remote without
  credentials.
- `[[routes]]` stay raw `toml::Value`s here, so `giles.toml` loading says nothing about them; the
  import (`routes::import_rules`, `LegacyRoute` with `deny_unknown_fields`) rejects a stray key,
  because a misspelled `channel` or `conversation` would make a route match everything.
- `agent.toml` (see above) and the web channel's `proxy` table.

An instance's `config` table is opaque to loading. The built-in components check it when the
registry builds them, and are lenient the same way: `AnthropicConfig::from_json`,
`OpenAiConfig::from_json` (also in `models.<id>` tables) and `WebConfig::from_json` (except `proxy`)
put unknown keys in their `unknown_keys` instead of failing. `Registry::from_config` turns them into
`UnknownKey`s with full paths (`provider.anthropic.config.modle`), and logs each once at `warn`;
they are not added to `Config::unknown_keys`, which only carries what loading `giles.toml` found.
Out-of-process plugins validate their own `config` in `Configure` and fail only their instance.

`giles serve` logs the `UnknownKey`s found while loading once, right after the load, at `warn`;
there is no status field for them. The configuration is read only at startup, so later changes (an
agent created in the web UI) log nothing again.

The hash of an instance's table (`ConfigHash`) covers the table as written, unknown keys included,
so editing one only refreshes the toolset's cached tool list.

## Instances

Each instance kind has a raw struct with every core-owned key as an `Option`, so validation can tell
"unset" from "default" and reject keys that don't apply to the instance's shape:

| Type                | Shapes (enum)                                                    |
| ------------------- | ---------------------------------------------------------------- |
| `ProviderInstance`  | Built-in only (provider plugins are deferred)                    |
| `ChannelInstance`   | `ChannelRuntime::{BuiltIn, Plugin(Launch), Remote(Remote)}`      |
| `ToolsetInstance`   | `ToolsetTransport::{Stdio(StdioServer), Http(Remote)}`           |

Component resolution is `Builtins::resolve(kind, component)`: a name in the built-in list for that
kind is `Component::BuiltIn`; anything else is `Component::Plugin` with binary
`giles-<kind>-<component>` (`plugin_binary`). The built-in list is a parameter, `Builtins`, so the
core can pass the components its Cargo features compiled in; `Builtins::V1` lists every v1 built-in
(`anthropic` and `openai` providers, `web` channel). Finding the binary in `<root>/plugins/` or on
`$PATH` is the plugin host's job, at launch. Only providers and channels have a component.

A stdio toolset's `command` is required and kept as written (an absolute path or a bare program
name); resolving it on the operator's `$PATH` and adding its install prefix to the sandbox grants
happens at launch (PLAN section 7.10). A table without `command` is an error at `toolset.<name>`.
Every toolset of `giles.toml` is a stdio one: a remote toolset (`url`) is stored in the database,
and its table in the file is ignored.

A toolset has no component. `component` in a `[toolset.<name>]` table without `url` is an error at
`toolset.<name>.component`, checked on the raw table before it is read as a `ToolsetSpec` (which has
no such key). Such a table used to run a first-party plugin binary named after the component, so the
reason names that binary as the program to set in `command`.

Remote URLs (`address`, `url`) are parsed with `url`, must be `http` or `https` with a host and no
credentials, and plain `http` needs `insecure = true` unless the host is `localhost`, `127.0.0.0/8`,
or `::1` (PLAN section 7.6).

The `config` table becomes a `PluginConfig`, a JSON object for `Configure` (PLAN section 7.7). TOML
datetimes become their TOML text; infinite and NaN floats, which JSON can't carry, are errors.

## Ignored toolset tables

Remote toolsets are stored in the database and managed in the web UI, the admin RPCs, and
`giles toolset`; `giles.toml` keeps the toolsets Giles runs as a process. A login to the web UI must
not choose a program for Giles to run, its arguments, environment, or sandbox grants, while
connecting to a remote server runs nothing on the host.

Loading therefore ignores every `[toolset.<name>]` table that has `url`, whatever else it holds
(`giles::is_remote_toolset`). It isn't deserialized, so nothing in it is checked: a wrong type or an
unknown key inside doesn't stop startup and isn't reported as an unknown key. Each such table
becomes an `IgnoredToolsetTable { file, position, name }`, the position found with `error::locate`
at `toolset.<name>`, and `Config::ignored_toolset_tables` lists them in file order. `giles serve`
logs one warning for each right after the load, after the unknown keys, at every start (it names the
file, not the position):

```text
giles.toml: [toolset.github] is ignored: remote toolsets are managed in the web UI (Admin → MCP servers) or with giles toolset set
```

An ignored table's name isn't part of the unique-name check or of the `retired_instances` check, and
the `Config` doesn't have it, so a stored toolset may use the name. Nothing is migrated: the owner
adds each remote server again in the web UI or with `giles toolset set`.

## Validating one toolset

Validation of a toolset is separate from reading `giles.toml`, so the same checks run on a toolset
that comes from elsewhere, such as a spec stored as JSON, against the configuration that is running
now. The pieces, all in `giles_core::config`:

- `ToolsetSpec`: one toolset as written, with the keys of a `[toolset.<name>]` table (`command`,
  `args`, `env`, `url`, `auth`, `insecure`, `deadline`, `idle_stop`, `parallel`, `skills`,
  `sandbox`, `unsandboxed`). Loading deserializes each `[toolset.<name>]` table without `url` into
  it, and `ToolsetSpec::from_json` reads the same keys from JSON; it also serializes. Nothing is
  checked beyond each value's type. Credentials stay unresolved: `env` and `auth` are raw values
  holding plain strings or `{ secret = ... }` / `{ env = ... }` references, and its `Debug` leaves
  them out. `sandbox` is a `SandboxSpec`.
- `validate_toolset(name, &spec, &ToolsetContext) -> Result<ToolsetInstance, ToolsetError>`: the
  rules of `Ctx::toolset` (which keys fit a stdio or a remote toolset, `command`, `url`, `env`
  names, `auth`, durations, skill paths, grants against the data root and socket directory,
  credentials resolved), then the checks that involve other instances: the name against every
  provider, channel, and toolset of the configuration (`instance_named` finds the clash and
  `name_clash` words it, the message `giles.toml` gives), and the binary directories
  (`check_binary_dirs`). `ToolsetContext::replacing` names the toolset being edited or renamed: that
  one instance is left out of the name check and of the other instances' grants and directories.
  `None` adds a toolset, so every existing instance of the name, of any kind, is a clash. The
  toolset's own name edits it. Its old name renames it: the new name must be free, and a change only
  in case (`github` to `GitHub`) is allowed because the old instance is left out.
- `ToolsetContext { root, secrets, options, config, replacing }`: the data root (with its socket
  directory), a `Secrets` loaded from `secrets.toml` by the caller (so a new secret needs no
  restart), the `LoadOptions` (built-ins, the environment for `{ env = ... }` references and `PATH`,
  the `giles` binary), the `Config` whose other instances the toolset must fit with, and the toolset
  being replaced, if any.
- `ToolsetError { key, reason }`: `key` is relative to the toolset's table (`env.TOKEN`,
  `sandbox.write[0]`, `skills[1]`), so a form can show the reason next to the field; it is the root
  when the problem is the name or a clash. `Ctx` raises `ConfigError`s at `toolset.<name>.<key>` in
  the file being loaded; `validate_toolset` runs `Ctx` with no file and strips that prefix, so the
  reasons are byte for byte the ones `giles.toml` loading gives. Like them, they never quote a
  credential.

With one toolset the binary-directory check only reports what involves it: the toolset's own `write`
and `unix` grants against every binary directory (the `giles` binary, channel plugins, every stdio
server including its own, every toolset's `skills`), and each other instance's grants against this
toolset's server and `skills` directories. The error then names the other instance's key
(`toolset.other.sandbox.write[0]`) in the reason, with an empty `key`. Problems among other
instances that were already there (a binary that appeared in a granted directory after startup) are
not blamed on the toolset being validated, while loading `giles.toml` still rejects them. `skills`
overlapping the plugin data root is checked for the toolset being validated only.

`ToolsetManager` ([Toolsets](toolsets.md#stored-toolsets)) is the caller for toolsets stored in the
database. It reads `secrets.toml` for every validation and adds its own rules. A name that a
`giles.toml` instance of any kind uses, or that `[core] retired_instances` lists, is refused, and so
is `new`, the address of the web form for a new toolset. A name only an
[ignored table](#ignored-toolset-tables) has is free. A stored toolset is a remote server: a spec
without `url` is refused as an `Invalid` at `url`, and an `auth.bearer` that isn't
`{ secret = "<name>" }` at `auth.bearer`. Stdio toolsets of `giles.toml` keep accepting every
credential form in `env`.

## Secrets

`secrets.toml` is optional. If it exists it must be a regular file with no group or other permission
bits (`mode & 0o077 == 0`, checked on the file `fs::metadata` follows) and a flat table of strings;
otherwise loading fails, even if nothing refers to it.

A reference is an inline table with exactly one key, `secret` or `env`, whose value is a non-empty
string. References are resolved in three places:

- Anywhere in an instance's `config` table, including inside arrays. A one-key `secret`/`env` table
  there is always a reference, so components can't use that shape as data.
- Credentials: `auth.bearer`, `auth.oauth.client_secret`, and the values of a stdio toolset's `env`.
  These also accept a literal string.

`{ env = ... }` reads the process environment through `LoadOptions::env`; an unset variable or a
non-UTF-8 value is an error. A missing secret names the secrets file, and says when it doesn't
exist.

Keeping values out of output:

- Credentials are `Secret`, whose `Debug` and `Display` print `<redacted>`; `Secret::expose` is the
  only way to the value.
- `PluginConfig` remembers the key paths that held references and the values they resolved to. Its
  `Debug` and `redacted()` put `<redacted>` at those paths; only `to_json()`, for `Configure`, has
  the values.
- `scrub(text, &[&Secret])` replaces secret values in text Giles didn't write but logs or shows: a
  component's `Configure` error (`PluginConfig::scrub`), or a plugin's or MCP server's stderr, with
  the instance's `auth`, `env`, and `PluginConfig::secrets()` values. It makes one pass, taking the
  longest matching value at each position, so overlapping values can't garble the output and an
  inserted `<redacted>` is never matched again. Values shorter than `MIN_SCRUB_LEN` (4 bytes) are
  skipped: replacing every `1` or `443` in a message would garble it, and a value that short
  protects nothing.
- `config`, `auth`, and `env` are deserialized as raw `toml::Value`s and checked by hand, because
  serde's type errors quote the offending value (`invalid type: string "sk-...", expected a map`).
  The same goes for `secrets.toml`, whose errors give only the key and the value's type.
- TOML syntax errors use the parser's message and position, never its `Display`, which quotes the
  source line.

`Config`'s `Debug` output is therefore safe to log. Literal strings in a `config` table are not
secrets as far as Giles knows; the user guide tells operators to use references for credentials.

## Sandbox grants

`sandbox` is accepted only where the core launches the process (channel plugins without `address`,
stdio toolsets) and not together with `unsandboxed = true`. It becomes
`Sandbox { unsandboxed, grants: Grants { network, read, write, unix, min_abi } }`, the input the
sandbox trampoline's spec is built from (PLAN section 12):

- `network`: `NetworkGrant::{None, All, TcpPorts(sorted, unique)}`. Unset means the kind's default,
  applied here: `All` for channels, `None` for stdio toolsets. An empty port list is `None`.
- `read`, `write`, `unix`: absolute paths without `..`, normalized lexically (no `.`, repeated or
  trailing separators). `unix` holds socket paths: on Linux any entry also allows `socket(AF_UNIX)`;
  Seatbelt can restrict by path.
- `min_abi`: at least 1; only Linux enforces it.

Every `read`, `write`, and `unix` path is checked against the protected directories: the data root
and the socket directory (which differs from `<root>/run` when socket paths fall back to `$TMPDIR`).
The data root contains the plugin directory `<root>/plugins/`, so plugin binaries there are covered.
A grant is rejected if, in any of its spellings, it is a protected directory, inside one, or
contains one (`Path::starts_with`, so whole components: `/srv/giles-backup` is not inside
`/srv/giles`), unless that spelling is inside the instance's own data directory,
`DataRoot::plugin_data_dir(instance)`.

The spellings of a path are the lexical path and, when it differs, the path with symlinks resolved:
the longest existing ancestor is canonicalized and the rest appended. Protected directories and the
own data directory are expanded the same way (on macOS, `/var` and `/tmp` are symlinks into
`/private`). Limits: the check sees the filesystem at startup. A symlink created or retargeted
afterwards, or a bind mount, can make a granted path lead into the data root; Landlock and Seatbelt
resolve paths again when the sandbox is applied, so the trampoline should re-run the same check on
the spec it receives. `check_grant(root, instance, path)` exposes the rule for paths the core adds
itself, such as a stdio server's install prefix.

### Binary directories

The "plugin directories" of PLAN section 12.4 are the directories holding binaries Giles runs. A
plugin that could replace one would run its code at the next launch, possibly unsandboxed or with
another instance's grants, so after every instance is validated the loader collects:

- the directory of the running `giles` binary, which is also the `sandbox-exec` trampoline
  (`LoadOptions::giles_exe`, default `std::env::current_exe()`);
- for each launched channel plugin and each stdio toolset, the directory of its binary as found now:
  `discover_plugin(root, kind, component, env)` for channel plugins (`<root>/plugins/`, then each
  absolute `PATH` entry, executable regular files only; PLAN section 7.3), and
  `find_program(command, env)` for a toolset `command` (as given if absolute, else on `PATH`).

Each binary contributes the parent of every spelling of its path, so a symlinked binary protects
both the link's directory and its target's. `write` and `unix` grants overlapping one of these
directories, in either direction, are rejected (`BinaryDirs::check`); `read` grants are allowed,
since the core itself adds a stdio server's install prefix to its read grants (PLAN section 7.10). A
binary that isn't found protects nothing here; the plugin host reports it at launch. The plugin host
and stdio launcher should call `discover_plugin` and `find_program` rather than searching
themselves, so what is launched is what was checked.

Collecting these directories and checking the grants against them is `check_binary_dirs`
(`config/toolset.rs`): loading calls it once with every instance, and `validate_toolset` with one
toolset in focus ([Validating one toolset](#validating-one-toolset)).

## Instance config hash

Every instance carries `config_hash: ConfigHash`, the "hash of the instance config taken before
secrets are resolved" in the tool discovery cache key (PLAN section 7.9). It covers the whole
`[<kind>.<instance>]` table as written, core keys and `config` alike, with references hashed as the
reference (`{ secret = "github" }`), not the value. So changing a secret's value doesn't refresh the
cache, renaming the secret does, and the hash never depends on or reveals a secret.

A toolset can also be hashed from its `ToolsetSpec`, with `ToolsetSpec::config_hash`: the spec is
serialized to a `toml::Table` (unset keys left out, durations as whole milliseconds, so `"5m"` and
`"300s"` hash alike) and hashed the same way. The hash of a spec that has no keys beyond the spec's,
no durations, and values written the way the spec serializes them equals the hash of the
`giles.toml` table that holds the same keys, so the same toolset keeps its cache key whichever way
it was configured. Loading `giles.toml` still hashes the table as written, so cache keys of file
toolsets don't change.

The file is parsed a second time as a plain `toml::Table` and each instance's table is hashed with a
canonical encoding, so the hash is the same for every way of writing the same TOML value: key order,
inline versus standard tables, dotted keys, quoting, comments, whitespace, and number spelling.
Version 1 of the algorithm is SHA-256 (`sha2`) over the domain tag `giles-config-hash-v1\n` followed
by:

| Value    | Encoding                                                                   |
| -------- | -------------------------------------------------------------------------- |
| string   | `s`, byte length (u64 LE), UTF-8 bytes                                     |
| integer  | `i`, i64 LE                                                                |
| float    | `f`, IEEE 754 bits (u64 LE); `-0.0` as `0.0`, every NaN as `0x7ff8000000000000` |
| boolean  | `b`, one byte 0 or 1                                                       |
| datetime | `d`, then its TOML text as a string                                        |
| array    | `a`, item count (u64 LE), each item                                        |
| table    | `t`, entry count (u64 LE), then each entry in byte order of keys: key as a string, value |

Every value starts with a type tag and every variable-length part with its length, so different
values can't encode to the same bytes. Keys are sorted explicitly, since `toml::Table` keeps
insertion order if any crate in the build enables toml's `preserve_order`. Two pinned test vectors,
computed independently from this table, guard the algorithm; changing it needs a new domain tag.
`ConfigHash` displays as 64 lowercase hex digits.

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

`parse_duration` and `parse_size` (and the serde types `HumanDuration` and `ByteSize`) live in
`giles_plugin_sdk::config`, so plugins parse their own `config` tables the same way.

## v1 choices

- `[owner]` replaces users and roles (PLAN section 1.1): there is no `[roles]`, no `users/`, no
  `unknown_senders`, no route `user`, and no job `deliver_to` (every job delivers to the owner);
  those keys are rejected as unknown. `ask_to = "deliver_to"` keeps PLAN's spelling and means the
  owner. The owner's time zone is `[owner] timezone`, which stands in for `user.toml`'s.
- `prompt_file` must exist at startup, so a typo shows up then rather than at the first run.
