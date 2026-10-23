//! Configuration: `giles.toml`, `secrets.toml`, and `agents/<agent>/agent.toml` (PLAN sections
//! 7.7 and 11.2).
//!
//! [`Config::load`] reads and validates everything once, at startup; changes need a restart (PLAN
//! section 1.1). Every problem is a [`ConfigError`] naming the file, the key path, and the reason.
//! Unknown keys are errors everywhere except inside an instance's free-form `config` table, which
//! belongs to the component. Secret references are resolved during loading; resolved values are
//! held as [`Secret`]s or in a [`PluginConfig`], whose `Debug` output is redacted.

mod agent;
mod discovery;
mod error;
mod giles;
mod hash;
mod instances;
mod model;
mod names;
mod plugin_config;
mod sandbox;
mod secrets;
mod units;

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
pub use error::{ConfigError, KeyPath, Position, Segment};
pub use giles::{CoreConfig, DEFAULT_HTTP_LISTEN, Identity, OwnerConfig, Route};
pub use hash::ConfigHash;
pub use instances::{
    Auth, ChannelInstance, ChannelRuntime, DEFAULT_MAX_BLOB, DEFAULT_TOOL_DEADLINE, Launch,
    OAuthClientCredentials, Program, ProviderInstance, Remote, Sandbox, StdioServer,
    ToolsetInstance, ToolsetTransport,
};
pub use model::ModelRef;
pub use names::{Builtins, Component, InstanceName, Kind, is_valid_name, plugin_binary};
pub use plugin_config::PluginConfig;
pub use sandbox::{Grants, NetworkGrant, check_grant};
pub use secrets::{MIN_SCRUB_LEN, REDACTED, Secret, scrub};
pub use units::{parse_duration, parse_size};

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
    /// `[toolset.<instance>]`, by instance name.
    pub toolsets: BTreeMap<InstanceName, ToolsetInstance>,
    /// `[[routes]]`, in file order.
    pub routes: Vec<Route>,
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
        let mut raw: giles::RawGiles = parse_toml(file, source)?;
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
        let core = raw.core();
        let owner = raw.owner(file)?;
        let secrets = secrets::Secrets::load(&root.secrets_file())?;
        let resolver = secrets::Resolver {
            secrets: &secrets,
            env: options.env,
        };

        // Compared case-insensitively: on a case-insensitive file system (the macOS default),
        // `Libera` and `libera` would share a data directory and pid file.
        let mut kinds: BTreeMap<String, (Kind, &str)> = BTreeMap::new();
        let instances = raw
            .provider
            .keys()
            .map(|n| (n, Kind::Provider))
            .chain(raw.channel.keys().map(|n| (n, Kind::Channel)))
            .chain(raw.toolset.keys().map(|n| (n, Kind::Toolset)));
        for (name, kind) in instances {
            if let Some((other, other_name)) = kinds.insert(name.to_ascii_lowercase(), (kind, name))
            {
                return Err(ConfigError::at(
                    file,
                    KeyPath::root().key(kind.as_str()).key(name.as_str()),
                    format!(
                        "instance names must be unique across kinds, ignoring case, and \
                         `[{other}.{other_name}]` also uses {name:?}"
                    ),
                ));
            }
        }

        let ctx = instances::Ctx {
            file,
            root,
            builtins: &options.builtins,
            resolver: &resolver,
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
        for (name, instance) in std::mem::take(&mut raw.toolset) {
            let instance = ctx.toolset(&name, instance, hash(Kind::Toolset, &name))?;
            toolsets.insert(instance.name.clone(), instance);
        }

        // Directories holding binaries Giles runs may be read but not written by any plugin.
        let mut binary_dirs = sandbox::BinaryDirs::default();
        if let Some(exe) = &options.giles_exe {
            binary_dirs.add(exe, "the giles binary".to_owned());
        }
        let mut sandboxes = Vec::new();
        for channel in channels.values() {
            if let ChannelRuntime::Plugin(launch) = &channel.runtime {
                if let Some(binary) =
                    discover_plugin(root, Kind::Channel, &channel.component, options.env)
                {
                    let what = format!("channel {}'s plugin, {}", channel.name, binary.display());
                    binary_dirs.add(&binary, what);
                }
                sandboxes.push((Kind::Channel, &channel.name, &launch.sandbox));
            }
        }
        for toolset in toolsets.values() {
            if let ToolsetTransport::Stdio(stdio) = &toolset.transport {
                let binary = match &stdio.program {
                    Program::Command(command) => find_program(command, options.env),
                    Program::Plugin { component, .. } => {
                        discover_plugin(root, Kind::Toolset, component, options.env)
                    }
                };
                if let Some(binary) = binary {
                    let what = format!("toolset {}'s server, {}", toolset.name, binary.display());
                    binary_dirs.add(&binary, what);
                }
                sandboxes.push((Kind::Toolset, &toolset.name, &stdio.sandbox));
            }
        }
        // Toolset skill directories are read by the core for the model (PLAN section 6.6): no
        // plugin may write there, so none can race those reads.
        for toolset in toolsets.values() {
            for (i, dir) in toolset.skills.iter().enumerate() {
                if sandbox::overlaps(dir, &root.plugin_data_root()) {
                    return Err(ConfigError::at(
                        file,
                        KeyPath::root()
                            .key("toolset")
                            .key(toolset.name.as_str())
                            .key("skills")
                            .index(i),
                        format!(
                            "{} overlaps the plugins' data directories, {}",
                            dir.display(),
                            root.plugin_data_root().display()
                        ),
                    ));
                }
                binary_dirs.add_dir(dir, format!("toolset {}'s skills", toolset.name));
            }
        }
        for (kind, name, sandbox) in sandboxes {
            let path = KeyPath::root()
                .key(kind.as_str())
                .key(name.as_str())
                .key("sandbox");
            binary_dirs.check(file, &path, &sandbox.grants)?;
        }

        let provider_names: BTreeSet<&str> = providers.keys().map(InstanceName::as_str).collect();
        let agents = load_agents(root, &provider_names)?;

        let channel_names = channels.keys().map(InstanceName::as_str).collect();
        let agent_names = agents.keys().map(AgentId::as_str).collect();
        let routes = raw.routes(file, &channel_names, &agent_names)?;

        Ok(Self {
            core,
            owner,
            providers,
            channels,
            toolsets,
            routes,
            agents,
        })
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

#[cfg(test)]
mod tests;
