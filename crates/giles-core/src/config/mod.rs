//! Configuration: `giles.toml`, `secrets.toml`, and `agents/<agent>/agent.toml` (PLAN sections
//! 7.7 and 11.2).
//!
//! [`Config::load`] reads and validates everything once, at startup; changes need a restart (PLAN
//! section 1.1), except agents created, edited, or removed in the web UI, which change the running
//! configuration through [`LiveConfig`]. Every problem is a [`ConfigError`] naming the file, the
//! key path, and the reason.
//! Unknown keys in `giles.toml` are ignored and reported ([`Config::unknown_keys`]), except inside
//! an instance's free-form `config` table, which belongs to the component; a recognized key with a
//! value of the wrong type is an error, as is every key of `agent.toml`. A `[toolset.<name>]`
//! table with `url` is ignored whatever else it holds, and reported separately
//! ([`Config::ignored_toolset_tables`]): remote toolsets are stored in the database. Secret
//! references are resolved during loading; resolved values are held as [`Secret`]s or in a
//! [`PluginConfig`], whose `Debug` output is redacted.

mod agent;
mod discovery;
mod error;
mod giles;
mod hash;
mod instances;
mod live;
mod model;
mod names;
mod plugin_config;
mod sandbox;
mod secrets;
#[cfg(test)]
pub(crate) mod test_toolsets;
mod toolset;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use giles_types::AgentId;
use serde::de::DeserializeOwned;

use crate::paths::DataRoot;

pub use agent::{
    ActiveHours, AgentConfig, AskTo, BusyMode, DEFAULT_APPROVAL_TIMEOUT,
    DEFAULT_MAX_ITERATIONS_JOBS, DEFAULT_MEMORY_CAP, Job, JobPrompt, JobSession, parse_cron,
};
pub use discovery::{discover_plugin, find_program};
pub use error::{ConfigError, IgnoredToolsetTable, KeyPath, Position, Segment, UnknownKey};
pub use giles::{CoreConfig, DEFAULT_HTTP_LISTEN, Identity, OwnerConfig};
pub use giles_types::Kind;
pub use hash::ConfigHash;
pub use instances::{
    Auth, ChannelInstance, ChannelRuntime, DEFAULT_MAX_BLOB, DEFAULT_TOOL_DEADLINE, Launch,
    OAuthClientCredentials, ProviderInstance, Remote, Sandbox, StdioServer, ToolsetInstance,
    ToolsetSpec, ToolsetTransport,
};
pub use live::{
    AgentExists, InstanceExists, LiveConfig, NoSuchAgent, NoSuchToolset, RenameToolsetError,
};
pub use model::ModelRef;
pub use names::{Builtins, Component, InstanceName, is_valid_name, plugin_binary};
pub use plugin_config::PluginConfig;
pub use sandbox::{Grants, NetworkGrant, SandboxSpec, check_grant};
pub use secrets::{MIN_SCRUB_LEN, REDACTED, Secret, Secrets, scrub};
pub use toolset::{ToolsetContext, ToolsetError, validate_toolset};

/// What [`Config::load_with`] needs besides the data root.
#[derive(Clone)]
pub struct LoadOptions<'a> {
    /// The components compiled into this core.
    pub builtins: Builtins,
    /// Looks up an environment variable, for `{ env = "NAME" }` references and the `PATH` that
    /// plugin binaries and toolset commands are found on.
    pub env: &'a dyn Fn(&str) -> Option<OsString>,
    /// The running `giles` binary, whose directory no grant may write to. `None` skips that check.
    pub giles_exe: Option<PathBuf>,
}

impl Default for LoadOptions<'static> {
    /// The v1 built-ins ([`Builtins::V1`]), the process environment, and
    /// [`std::env::current_exe`].
    fn default() -> Self {
        Self {
            builtins: Builtins::V1,
            env: &process_env,
            giles_exe: std::env::current_exe().ok(),
        }
    }
}

fn process_env(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The whole validated configuration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Config {
    /// `[core]`.
    pub core: CoreConfig,
    /// `[owner]`.
    pub owner: OwnerConfig,
    /// `[provider.<instance>]`, by instance name.
    pub providers: BTreeMap<InstanceName, ProviderInstance>,
    /// `[channel.<instance>]`, by instance name.
    pub channels: BTreeMap<InstanceName, ChannelInstance>,
    /// The toolsets: `[toolset.<instance>]` tables without `url` after [`load`](Self::load), and
    /// the toolsets stored in the database, which the toolset manager adds through
    /// [`LiveConfig`], by instance name.
    pub toolsets: BTreeMap<InstanceName, ToolsetInstance>,
    /// Every `[toolset.<name>]` table of `giles.toml` with `url`, in file order.
    ignored_toolsets: Vec<IgnoredToolsetTable>,
    /// Every key of `giles.toml` that Giles doesn't recognize. They are ignored; the daemon logs
    /// each once at startup.
    pub unknown_keys: Vec<UnknownKey>,
    /// `[[routes]]` as written, unvalidated. Routes are managed in the web UI and stored in the
    /// database; these are imported once by [`Routes::open`](crate::routes::Routes::open) and then
    /// only reported.
    pub legacy_routes: Vec<toml::Value>,
    /// Every `agents/<agent>/agent.toml`, by agent id.
    pub agents: BTreeMap<AgentId, AgentConfig>,
}

impl Config {
    /// Loads and validates the configuration under `root` with the default [`LoadOptions`]: the
    /// v1 built-in components and the process environment.
    ///
    /// # Errors
    ///
    /// The first problem found, as a [`ConfigError`].
    pub fn load(root: &DataRoot) -> Result<Self, ConfigError> {
        Self::load_with(root, &LoadOptions::default())
    }

    /// Loads and validates the configuration under `root`.
    ///
    /// # Errors
    ///
    /// The first problem found, as a [`ConfigError`].
    pub fn load_with(root: &DataRoot, options: &LoadOptions<'_>) -> Result<Self, ConfigError> {
        let file = root.config_file();
        let source = read_file(&file).map_err(|e| match &e.source {
            Some(io) if io.kind() == io::ErrorKind::NotFound => ConfigError::file(
                &file,
                "not found: create it (see the configuration reference in the user guide)",
            ),
            _ => e,
        })?;
        Self::from_source(root, options, &file, &source).map_err(|e| e.located(&file, &source))
    }

    /// Validates `source`, the contents of `file` (`giles.toml`), and loads everything it refers
    /// to.
    fn from_source(
        root: &DataRoot,
        options: &LoadOptions<'_>,
        file: &Path,
        source: &str,
    ) -> Result<Self, ConfigError> {
        let (mut raw, mut unknown): (giles::RawGiles, Vec<KeyPath>) =
            parse_toml_lenient(file, source)?;
        // The same document as plain TOML, for hashing each instance's table as written.
        let document: toml::Table =
            toml::from_str(source).map_err(|e| ConfigError::syntax(file, source, &e))?;
        let hash = |kind: Kind, name: &str| {
            let table = document
                .get(kind.as_str())
                .and_then(|kind| kind.get(name))
                .and_then(toml::Value::as_table);
            ConfigHash::of(table.unwrap_or(&toml::Table::new()))
        };
        let core = raw.core(file)?;
        let owner = raw.owner(file)?;
        // A table with `url` is a remote toolset, which is stored in the database now: the table
        // is ignored whatever else it holds. The others are stdio servers.
        let mut ignored_toolsets = Vec::new();
        let mut toolset_specs = BTreeMap::new();
        for (name, table) in std::mem::take(&mut raw.toolset) {
            if giles::is_remote_toolset(&table) {
                ignored_toolsets.push(IgnoredToolsetTable::new(file, source, name));
                continue;
            }
            let path = KeyPath::root()
                .key(Kind::Toolset.as_str())
                .key(name.as_str());
            // A toolset ran a plugin named by `component` before `command` was required.
            if let Some(component) = table.get("component") {
                let binary = format!("giles-toolset-{}", component.as_str().unwrap_or(&name));
                return Err(ConfigError::at(
                    file,
                    path.key("component"),
                    format!(
                        "a toolset has no component: set `command` to the MCP server's program \
                         instead, e.g. {binary:?} on $PATH or its absolute path"
                    ),
                ));
            }
            let (spec, keys) = parse_value_lenient::<ToolsetSpec>(file, &path, table)?;
            unknown.extend(keys);
            toolset_specs.insert(name, spec);
        }
        ignored_toolsets.sort_by_key(IgnoredToolsetTable::order);
        let secrets = secrets::Secrets::load(&root.secrets_file())?;
        let resolver = secrets::Resolver {
            secrets: &secrets,
            env: options.env,
        };

        // Compared case-insensitively within a kind: on a case-insensitive file system (the macOS
        // default), `Libera` and `libera` would share a data directory and pid file. A provider,
        // a channel, and a toolset may share a name, because each kind has its own directories.
        let mut names: BTreeMap<(Kind, String), &str> = BTreeMap::new();
        let instances = raw
            .provider
            .keys()
            .map(|n| (n, Kind::Provider))
            .chain(raw.channel.keys().map(|n| (n, Kind::Channel)))
            .chain(toolset_specs.keys().map(|n| (n, Kind::Toolset)));
        for (name, kind) in instances {
            if let Some(other_name) = names.insert((kind, name.to_ascii_lowercase()), name) {
                return Err(ConfigError::at(
                    file,
                    KeyPath::root().key(kind.as_str()).key(name.as_str()),
                    toolset::name_clash(kind, other_name, name),
                ));
            }
        }

        let ctx = instances::Ctx {
            file,
            root,
            builtins: &options.builtins,
            resolver: &resolver,
            unknown: RefCell::default(),
        };
        let mut providers = BTreeMap::new();
        for (name, instance) in std::mem::take(&mut raw.provider) {
            let instance = ctx.provider(&name, instance, hash(Kind::Provider, &name))?;
            providers.insert(instance.name.clone(), instance);
        }
        let mut channels = BTreeMap::new();
        for (name, instance) in std::mem::take(&mut raw.channel) {
            let instance = ctx.channel(&name, instance, hash(Kind::Channel, &name))?;
            channels.insert(instance.name.clone(), instance);
        }
        let mut toolsets = BTreeMap::new();
        for (name, instance) in toolset_specs {
            let instance = ctx.toolset(&name, instance, hash(Kind::Toolset, &name))?;
            toolsets.insert(instance.name.clone(), instance);
        }

        // Directories holding binaries Giles runs may be read but not written by any plugin.
        let toolset_refs: Vec<&ToolsetInstance> = toolsets.values().collect();
        toolset::check_binary_dirs(file, root, options, channels.values(), &toolset_refs, None)?;

        let provider_names: BTreeSet<&str> = providers.keys().map(InstanceName::as_str).collect();
        let agents = load_agents(root, &provider_names)?;

        // In file order, as they are logged.
        let mut unknown_keys: Vec<UnknownKey> = unknown
            .into_iter()
            .chain(ctx.unknown.into_inner())
            .map(|key| UnknownKey::new(file, source, key))
            .collect();
        unknown_keys.sort_by_key(UnknownKey::order);

        Ok(Self {
            core,
            owner,
            providers,
            channels,
            toolsets,
            ignored_toolsets,
            unknown_keys,
            legacy_routes: std::mem::take(&mut raw.routes),
            agents,
        })
    }

    /// The `[toolset.<name>]` tables of `giles.toml` with `url`, in file order. Loading ignores
    /// them (remote toolsets are stored in the database); `giles serve` logs each at startup.
    pub fn ignored_toolset_tables(&self) -> &[IgnoredToolsetTable] {
        &self.ignored_toolsets
    }

    /// Validates `source` as the `agent.toml` of an agent `id` that would live in
    /// `agents/<id>/` under `root`, exactly as [`load`](Self::load) validates each agent at
    /// startup: the id as a directory name, every key, the models against this configuration's
    /// provider instances, and job prompt files in the agent's directory. Reads nothing else and
    /// writes nothing; the web UI checks an agent it creates with this before writing it.
    ///
    /// # Errors
    ///
    /// The first problem found, as a [`ConfigError`] in `agents/<id>/agent.toml` (in
    /// `agents/` for an invalid id).
    pub fn validate_agent(
        &self,
        root: &DataRoot,
        id: &str,
        source: &str,
    ) -> Result<AgentConfig, ConfigError> {
        if !is_valid_name(id) {
            return Err(ConfigError::file(
                &root.agents_dir(),
                names::invalid_name_reason("agent", id),
            ));
        }
        let providers = self.providers.keys().map(InstanceName::as_str).collect();
        agent::parse(id, &root.agents_dir().join(id), source, &providers)
    }
}

/// Loads every agent: each directory in `agents/` whose name doesn't start with `.`. Files there
/// are ignored.
fn load_agents(
    root: &DataRoot,
    providers: &BTreeSet<&str>,
) -> Result<BTreeMap<AgentId, AgentConfig>, ConfigError> {
    let dir = root.agents_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(ConfigError::io(&dir, "list", e)),
    };
    let mut agents = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|e| ConfigError::io(&dir, "list", e))?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let meta = fs::metadata(&path).map_err(|e| ConfigError::io(&path, "inspect", e))?;
        if !meta.is_dir() {
            continue;
        }
        if !is_valid_name(&name) {
            return Err(ConfigError::file(
                &path,
                names::invalid_name_reason("agent", &name),
            ));
        }
        if !path.join("agent.toml").is_file() {
            return Err(ConfigError::file(
                &path,
                "an agent directory needs an agent.toml",
            ));
        }
        let agent = agent::load(&name, &path, providers)?;
        agents.insert(agent.id.clone(), agent);
    }
    Ok(agents)
}

/// Reads a configuration file as UTF-8.
fn read_file(path: &Path) -> Result<String, ConfigError> {
    let bytes = fs::read(path).map_err(|e| ConfigError::io(path, "read", e))?;
    String::from_utf8(bytes).map_err(|e| {
        let offset = e.utf8_error().valid_up_to();
        let valid = String::from_utf8_lossy(&e.as_bytes()[..offset]);
        ConfigError {
            position: Some(Position::of_offset(&valid, offset)),
            ..ConfigError::file(path, "not valid UTF-8")
        }
    })
}

/// Parses `source` (the contents of `file`) as `T`, tracking the key path of any error.
fn parse_toml<T: DeserializeOwned>(file: &Path, source: &str) -> Result<T, ConfigError> {
    let deserializer =
        toml::Deserializer::parse(source).map_err(|e| ConfigError::syntax(file, source, &e))?;
    serde_path_to_error::deserialize(deserializer)
        .map_err(|e| ConfigError::deserialize(file, source, &e))
}

/// Like [`parse_toml`], but keys `T` doesn't have are not errors: they are returned, in the order
/// the parser meets them. A key `T` has with a value it can't take is still an error, found
/// whether or not unknown keys come before it. Only the outermost unknown key is returned (not the
/// keys inside an unknown table).
fn parse_toml_lenient<T: DeserializeOwned>(
    file: &Path,
    source: &str,
) -> Result<(T, Vec<KeyPath>), ConfigError> {
    let deserializer =
        toml::Deserializer::parse(source).map_err(|e| ConfigError::syntax(file, source, &e))?;
    let mut unknown = Vec::new();
    let mut record = |path: serde_ignored::Path<'_>| unknown.push(KeyPath::from(&path));
    let value = serde_path_to_error::deserialize(serde_ignored::Deserializer::new(
        deserializer,
        &mut record,
    ))
    .map_err(|e| ConfigError::deserialize(file, source, &e))?;
    Ok((value, unknown))
}

/// Like [`parse_toml_lenient`], for `value`, the value at `path` of `file`: errors and unknown
/// keys are at their full path. An error has no position yet; loading fills it in from the path.
fn parse_value_lenient<T: DeserializeOwned>(
    file: &Path,
    path: &KeyPath,
    value: toml::Value,
) -> Result<(T, Vec<KeyPath>), ConfigError> {
    let mut unknown = Vec::new();
    let mut record = |key: serde_ignored::Path<'_>| unknown.push(path.join(&KeyPath::from(&key)));
    let value =
        serde_path_to_error::deserialize(serde_ignored::Deserializer::new(value, &mut record))
            .map_err(|e: serde_path_to_error::Error<toml::de::Error>| {
                ConfigError::at(
                    file,
                    path.join(&KeyPath::from(e.path())),
                    e.inner().message(),
                )
            })?;
    Ok((value, unknown))
}

#[cfg(test)]
mod tests;
