//! Configuration: `giles.toml` and `agents/<agent>/agent.toml` (PLAN section 11.2).
//!
//! [`Config::load`] reads and validates everything once, at startup; changes need a restart (PLAN
//! section 1.1). Every problem is a [`ConfigError`] naming the file, the key path, and the reason.
//! Unknown keys are errors everywhere except inside an instance's free-form `config` table, which
//! belongs to the component.

mod agent;
mod error;
mod giles;
mod instances;
mod model;
mod names;
mod plugin_config;
mod units;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use giles_types::AgentId;
use serde::de::DeserializeOwned;

use crate::paths::DataRoot;

pub use agent::{
    ActiveHours, AgentConfig, AskTo, BusyMode, DEFAULT_APPROVAL_TIMEOUT,
    DEFAULT_MAX_ITERATIONS_JOBS, DEFAULT_MEMORY_CAP, Job, JobPrompt, JobSession,
};
pub use error::{ConfigError, KeyPath, Position, Segment};
pub use giles::{CoreConfig, DEFAULT_HTTP_LISTEN, Identity, OwnerConfig, Route};
pub use instances::{
    ChannelInstance, ChannelRuntime, DEFAULT_MAX_BLOB, DEFAULT_TOOL_DEADLINE, Launch, Program,
    ProviderInstance, Remote, Sandbox, StdioServer, ToolsetInstance, ToolsetTransport,
};
pub use model::ModelRef;
pub use names::{Builtins, Component, InstanceName, Kind, is_valid_name, plugin_binary};
pub use plugin_config::PluginConfig;
pub use units::{parse_duration, parse_size};

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
    /// Loads and validates the configuration under `root` with the v1 built-in components
    /// ([`Builtins::V1`]).
    ///
    /// # Errors
    ///
    /// The first problem found, as a [`ConfigError`].
    pub fn load(root: &DataRoot) -> Result<Self, ConfigError> {
        Self::load_with(root, &Builtins::V1)
    }

    /// Loads and validates the configuration under `root`; `builtins` are the components compiled
    /// into this core.
    ///
    /// # Errors
    ///
    /// The first problem found, as a [`ConfigError`].
    pub fn load_with(root: &DataRoot, builtins: &Builtins) -> Result<Self, ConfigError> {
        let file = root.config_file();
        let source = read_file(&file).map_err(|e| match &e.source {
            Some(io) if io.kind() == io::ErrorKind::NotFound => ConfigError::file(
                &file,
                "not found: create it (see the configuration reference in the user guide)",
            ),
            _ => e,
        })?;
        Self::from_source(root, builtins, &file, &source).map_err(|e| e.located(&file, &source))
    }

    /// Validates `source`, the contents of `file` (`giles.toml`), and loads everything it refers
    /// to.
    fn from_source(
        root: &DataRoot,
        builtins: &Builtins,
        file: &Path,
        source: &str,
    ) -> Result<Self, ConfigError> {
        let mut raw: giles::RawGiles = parse_toml(file, source)?;
        let core = raw.core();
        let owner = raw.owner(file)?;

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

        let ctx = instances::Ctx { file, builtins };
        let mut providers = BTreeMap::new();
        for (name, instance) in std::mem::take(&mut raw.provider) {
            let instance = ctx.provider(&name, instance)?;
            providers.insert(instance.name.clone(), instance);
        }
        let mut channels = BTreeMap::new();
        for (name, instance) in std::mem::take(&mut raw.channel) {
            let instance = ctx.channel(&name, instance)?;
            channels.insert(instance.name.clone(), instance);
        }
        let mut toolsets = BTreeMap::new();
        for (name, instance) in std::mem::take(&mut raw.toolset) {
            let instance = ctx.toolset(&name, instance)?;
            toolsets.insert(instance.name.clone(), instance);
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
