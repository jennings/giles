//! The registry (PLAN section 13.3): configured instance names to the trait objects that serve
//! them, the prefixed tool index, the per-agent skill index, and model resolution.
//!
//! [`Registry::from_config`] constructs the built-in components (PLAN section 4.1) from the
//! configuration. Components built elsewhere (plugin channels and environments by the plugin
//! host, MCP toolsets by the toolset host) are added later with [`Registry::register_channel`],
//! [`Registry::register_environment`], and [`Registry::register_toolset`]; until then the instance
//! is [pending](Registry::pending). Lookups clone an `Arc`, so callers never hold a lock across an
//! `await`.

mod skills;
pub mod tools;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use giles_types::{AgentId, Channel, Environment, EnvironmentInfo, Provider, Toolset};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::{
    AgentConfig, ChannelInstance, ChannelRuntime, Config, ConfigError, EnvironmentRuntime,
    InstanceName, KeyPath, Kind, LiveConfig, ModelRef, ProviderInstance, UnknownKey,
};
use crate::core_tools::CoreTool;
use crate::paths::{DataRoot, PathsError};
use crate::providers::MediaSource;

pub use tools::{
    CORE_TOOLS, IndexedTool, MAX_TOOL_NAME_CHARS, PREFIX_SEPARATOR, ToolIndex, ToolNameError,
    ToolSnapshot, ToolTarget, is_core_tool, prefix_tools, prefixed_name,
};

/// A registry operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// The configuration asks for something this build can't provide: a built-in component
    /// that isn't compiled in or not implemented yet, a component config it rejects, or a model
    /// on a provider instance that isn't registered. Names the file and key.
    #[error(transparent)]
    Config(Box<ConfigError>),
    /// A `register_*` call named an instance that isn't configured with that kind.
    #[error("there is no configured {kind} instance `{name}`")]
    NotConfigured {
        /// The kind registered.
        kind: Kind,
        /// The name given.
        name: InstanceName,
    },
    /// A model names a provider instance that has no registered provider.
    #[error("no provider is registered for instance `{0}`")]
    UnknownProvider(InstanceName),
    /// The agent lists no models.
    #[error("agent `{0}` has no models")]
    NoModels(AgentId),
    /// An agent's skill directory path couldn't be built.
    #[error(transparent)]
    Paths(#[from] PathsError),
    /// Skill discovery's blocking task failed.
    #[error("skill discovery failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// A core tool's name isn't one of [`CORE_TOOLS`].
    #[error(transparent)]
    ToolName(#[from] ToolNameError),
}

impl From<ConfigError> for RegistryError {
    fn from(e: ConfigError) -> Self {
        Self::Config(Box::new(e))
    }
}

/// A model to call: the provider object and the model id it takes.
#[derive(Clone)]
pub struct ResolvedModel {
    /// The provider instance, as written before the `/`.
    pub instance: InstanceName,
    /// The provider registered for it.
    pub provider: Arc<dyn Provider>,
    /// The model id, passed to the provider unchanged.
    pub model: String,
}

impl fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("instance", &self.instance)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// A configured channel, environment, or toolset with no implementation registered yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Its kind.
    pub kind: Kind,
    /// The instance.
    pub name: InstanceName,
    /// What will provide it.
    pub reason: &'static str,
}

/// A configured `[environment.<instance>]`, as the workspace code sees it: what the config says
/// and what the plugin host last registered. See [`Registry::environments`].
#[derive(Clone)]
#[non_exhaustive]
pub struct EnvironmentEntry {
    /// The instance.
    pub name: InstanceName,
    /// What the model is told about the environment; empty if the config sets none.
    pub description: String,
    /// Stop a workspace after this long without use.
    pub stop_after: Duration,
    /// Delete a workspace after this long without use.
    pub delete_after: Duration,
    /// The JSON Schema of the options a workspace takes ([`EnvironmentInfo::options_schema`]),
    /// as the plugin reported it after its last successful `Configure`. Kept while the plugin is
    /// down; `None` until the first connection.
    pub options_schema: Option<serde_json::Value>,
    /// How many times the plugin has connected since the core started: 0 before the first, and
    /// one more after each reconnection. A change means the plugin may have forgotten what
    /// happened while it was away, so whatever was learned from it earlier is worth checking.
    pub connections: u64,
    /// The plugin's client while it is connected; `None` while it is down, starting, or failed.
    pub client: Option<Arc<dyn Environment>>,
}

impl EnvironmentEntry {
    /// Whether the plugin is connected: calls to [`client`](Self::client) can succeed.
    pub fn reachable(&self) -> bool {
        self.client.is_some()
    }
}

impl fmt::Debug for EnvironmentEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvironmentEntry")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("stop_after", &self.stop_after)
            .field("delete_after", &self.delete_after)
            .field("reachable", &self.reachable())
            .field("connections", &self.connections)
            .finish_non_exhaustive()
    }
}

/// What the plugin host registered for one environment instance.
#[derive(Default)]
struct EnvironmentState {
    client: Option<Arc<dyn Environment>>,
    options_schema: Option<serde_json::Value>,
    connections: u64,
}

type Map<T> = RwLock<BTreeMap<InstanceName, Arc<T>>>;

fn get<T: ?Sized>(map: &Map<T>, name: &str) -> Option<Arc<T>> {
    map.read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(name)
        .cloned()
}

fn all<T: ?Sized>(map: &Map<T>) -> Vec<(InstanceName, Arc<T>)> {
    map.read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(name, item)| (name.clone(), item.clone()))
        .collect()
}

fn insert<T: ?Sized>(map: &Map<T>, name: &InstanceName, item: Arc<T>) -> Option<Arc<T>> {
    map.write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(name.clone(), item)
}

/// Configured instances and the objects serving them. Shared as `Arc<Registry>`.
pub struct Registry {
    root: DataRoot,
    config: LiveConfig,
    providers: Map<dyn Provider>,
    channels: Map<dyn Channel>,
    toolsets: Map<dyn Toolset>,
    environments: RwLock<BTreeMap<InstanceName, EnvironmentState>>,
    /// Counts every change to `environments`: a registration or a withdrawal.
    environment_changes: watch::Sender<u64>,
    tools: ToolIndex,
    /// The core tools' implementations, by name.
    core_tools: RwLock<BTreeMap<String, Arc<dyn CoreTool>>>,
    /// The built-in web channel, typed (it's in `channels` too).
    #[cfg(feature = "channel-web")]
    web: std::sync::OnceLock<Arc<crate::channels::web::WebChannel>>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn names<T: ?Sized>(map: &Map<T>) -> Vec<InstanceName> {
            all(map).into_iter().map(|(name, _)| name).collect()
        }
        f.debug_struct("Registry")
            .field("root", &self.root)
            .field("providers", &names(&self.providers))
            .field("channels", &names(&self.channels))
            .field("toolsets", &names(&self.toolsets))
            .field(
                "environments",
                &self
                    .environments
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .keys()
                    .collect::<Vec<_>>(),
            )
            .field("tools", &self.tools.snapshot().len())
            .finish_non_exhaustive()
    }
}

impl Registry {
    /// A registry for `config` with nothing registered. [`from_config`](Self::from_config) is
    /// the daemon's constructor; this one is for tests that register fakes.
    pub fn new(root: DataRoot, config: Arc<Config>) -> Self {
        Self {
            root,
            config: LiveConfig::new(config),
            providers: RwLock::default(),
            channels: RwLock::default(),
            toolsets: RwLock::default(),
            environments: RwLock::default(),
            environment_changes: watch::channel(0).0,
            tools: ToolIndex::new(),
            core_tools: RwLock::default(),
            #[cfg(feature = "channel-web")]
            web: std::sync::OnceLock::new(),
        }
    }

    /// Builds every built-in component `config` configures. Built-in providers read attachment
    /// bytes from `media`. Plugin channels and toolsets are left [pending](Self::pending). Every
    /// model an agent names is on a configured provider (config loading checks that), so after
    /// this succeeds every model resolves.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Config`] naming the `giles.toml` key (with its line and column) of the
    /// instance at fault.
    pub fn from_config(
        root: DataRoot,
        config: Arc<Config>,
        media: Arc<dyn MediaSource>,
    ) -> Result<Self, RegistryError> {
        let registry = Self::new(root, config);
        let config = registry.config();
        // Keys the built-in components ignored, under `giles.toml`'s `[<kind>.<instance>.config]`.
        let mut unknown = Vec::new();
        for instance in config.providers.values() {
            let (provider, keys) = build_provider(instance, &media).map_err(|(key, reason)| {
                let path = KeyPath::root()
                    .key("provider")
                    .key(instance.name.as_str())
                    .key(key);
                registry.giles_toml_error(path, reason)
            })?;
            insert(&registry.providers, &instance.name, provider);
            let at = KeyPath::root()
                .key("provider")
                .key(instance.name.as_str())
                .key("config");
            unknown.extend(keys.iter().map(|key| at.join(key)));
        }
        for instance in config.channels.values() {
            if let ChannelRuntime::BuiltIn = instance.runtime {
                let (channel, keys) =
                    build_channel(&registry, instance).map_err(|(key, reason)| {
                        let path = KeyPath::root()
                            .key("channel")
                            .key(instance.name.as_str())
                            .key(key);
                        registry.giles_toml_error(path, reason)
                    })?;
                insert(&registry.channels, &instance.name, channel);
                let at = KeyPath::root()
                    .key("channel")
                    .key(instance.name.as_str())
                    .key("config");
                unknown.extend(keys.iter().map(|key| at.join(key)));
            }
        }
        registry.report_unknown_keys(unknown);
        Ok(registry)
    }

    /// Logs the keys that built-in components ignored, once each.
    fn report_unknown_keys(&self, keys: Vec<KeyPath>) {
        if keys.is_empty() {
            return;
        }
        let file = self.root.config_file();
        // Only to give the keys positions; without the file they are logged without one.
        let source = std::fs::read_to_string(&file).unwrap_or_default();
        for key in keys {
            warn!("{}", UnknownKey::new(&file, &source, key));
        }
    }

    /// An error at `key` in `giles.toml`, with the key's position if the file can be read again
    /// (only on this error path, so loading never keeps the source around).
    fn giles_toml_error(&self, key: KeyPath, reason: String) -> ConfigError {
        let file = self.root.config_file();
        let error = ConfigError::at(&file, key, reason);
        match std::fs::read_to_string(&file) {
            Ok(source) => error.located(&file, &source),
            Err(_) => error,
        }
    }

    /// The data root.
    pub fn root(&self) -> &DataRoot {
        &self.root
    }

    /// The configuration now: the one the registry was built from, plus the agents created since
    /// ([`live_config`](Self::live_config)). A snapshot; keep it for the work at hand.
    pub fn config(&self) -> Arc<Config> {
        self.config.get()
    }

    /// The shared configuration that agents created at runtime are added to.
    pub fn live_config(&self) -> &LiveConfig {
        &self.config
    }

    /// The tool index.
    pub fn tools(&self) -> &ToolIndex {
        &self.tools
    }

    /// Registers a core tool: its implementation, and its spec in the tool index (replacing any
    /// earlier tool of that name).
    ///
    /// # Errors
    ///
    /// [`ToolNameError::NotCoreTool`] if its name isn't one of [`CORE_TOOLS`].
    pub fn register_core_tool(&self, tool: Arc<dyn CoreTool>) -> Result<(), ToolNameError> {
        let spec = tool.spec();
        let name = spec.name.clone();
        self.tools.set_core_tool(spec)?;
        self.core_tools
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name, tool);
        Ok(())
    }

    /// The implementation of the core tool `name`, if one is registered.
    pub fn core_tool(&self, name: &str) -> Option<Arc<dyn CoreTool>> {
        self.core_tools
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// Registers the provider for the configured provider instance `name`, replacing (and
    /// returning) any earlier one.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotConfigured`] if `[provider.<name>]` doesn't exist.
    pub fn register_provider(
        &self,
        name: &InstanceName,
        provider: Arc<dyn Provider>,
    ) -> Result<Option<Arc<dyn Provider>>, RegistryError> {
        self.check_configured(Kind::Provider, name)?;
        Ok(insert(&self.providers, name, provider))
    }

    /// Registers the channel for the configured channel instance `name` (the plugin host's
    /// `RemoteChannel`, or a built-in), replacing (and returning) any earlier one. Consuming its
    /// events is the caller's job.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotConfigured`] if `[channel.<name>]` doesn't exist.
    pub fn register_channel(
        &self,
        name: &InstanceName,
        channel: Arc<dyn Channel>,
    ) -> Result<Option<Arc<dyn Channel>>, RegistryError> {
        self.check_configured(Kind::Channel, name)?;
        Ok(insert(&self.channels, name, channel))
    }

    /// Registers the toolset for the configured toolset instance `name`, replacing (and
    /// returning) any earlier one, and loads its tools into the index: subscribes to
    /// [`Toolset::tool_changes`] first, then indexes [`Toolset::list_tools`], then applies every
    /// later list the stream yields (new sessions see them; running ones keep their
    /// snapshot). A failed `list_tools` is logged and keeps the instance's previous tools (none
    /// on first registration) until a change arrives. Must be called inside a Tokio runtime.
    /// The instance must be in the [live configuration](Self::live_config), so a toolset added at
    /// runtime ([`LiveConfig::add_toolset`](crate::config::LiveConfig::add_toolset)) can be
    /// registered.
    ///
    /// Cancel-safe: nothing changes until `list_tools` has returned, and then the object, its
    /// tools, and its change watcher are installed together without another `await`. Dropping
    /// the future while `list_tools` runs leaves the previous object (and its watcher) in place.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotConfigured`] if `[toolset.<name>]` doesn't exist.
    pub async fn register_toolset(
        &self,
        name: &InstanceName,
        toolset: Arc<dyn Toolset>,
    ) -> Result<Option<Arc<dyn Toolset>>, RegistryError> {
        self.check_configured(Kind::Toolset, name)?;
        let changes = toolset.tool_changes();
        let listed = toolset.list_tools().await;
        let old = insert(&self.toolsets, name, toolset);
        let generation = self.tools.begin(name);
        match listed {
            Ok(tools) => self.tools.apply(name, generation, tools),
            Err(error) => warn!(toolset = %name, %error, "cannot list the toolset's tools"),
        }
        if let Some(changes) = changes {
            self.tools.watch(name, generation, changes);
        }
        Ok(old)
    }

    /// Removes the toolset registered for instance `name` and withdraws its tools from the
    /// index: sessions started after this don't offer them, and a running session's snapshot
    /// still lists them, but a call to one finds no toolset and fails as unavailable. The
    /// toolset's change watcher is stopped, and an update it still had in flight is dropped.
    /// Returns the object, or `None` if none was registered. The instance's configuration is
    /// untouched; take it out of [`live_config`](Self::live_config) separately.
    pub fn unregister_toolset(&self, name: &InstanceName) -> Option<Arc<dyn Toolset>> {
        let old = self
            .toolsets
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name);
        self.tools.remove_toolset(name);
        old
    }

    /// Registers `client` as the connected plugin of the configured environment instance `name`,
    /// with the `info` it reported after `Configure`, replacing any earlier client and schema. The
    /// instance's [`connections`](EnvironmentEntry::connections) count goes up and
    /// [`environment_changes`](Self::environment_changes) notifies. Returns the earlier client.
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotConfigured`] if `[environment.<name>]` doesn't exist.
    pub fn register_environment(
        &self,
        name: &InstanceName,
        client: Arc<dyn Environment>,
        info: EnvironmentInfo,
    ) -> Result<Option<Arc<dyn Environment>>, RegistryError> {
        self.check_configured(Kind::Environment, name)?;
        let old = {
            let mut states = self
                .environments
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            let state = states.entry(name.clone()).or_default();
            state.options_schema = Some(info.options_schema);
            state.connections += 1;
            state.client.replace(client)
        };
        self.environment_changes
            .send_modify(|changes| *changes += 1);
        Ok(old)
    }

    /// Withdraws the client of environment instance `name`, which has disconnected: the instance
    /// is no longer [reachable](EnvironmentEntry::reachable), and calls through a client taken
    /// earlier fail as the plugin's connection is gone. Its schema stays. Returns the client, or
    /// `None` if none was registered. The instance's configuration is untouched.
    pub fn unregister_environment(&self, name: &InstanceName) -> Option<Arc<dyn Environment>> {
        let old = self
            .environments
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(name)
            .and_then(|state| state.client.take());
        if old.is_some() {
            self.environment_changes
                .send_modify(|changes| *changes += 1);
        }
        old
    }

    fn check_configured(&self, kind: Kind, name: &InstanceName) -> Result<(), RegistryError> {
        let config = self.config();
        let configured = match kind {
            Kind::Provider => config.providers.contains_key(name),
            Kind::Channel => config.channels.contains_key(name),
            Kind::Toolset => config.toolsets.contains_key(name),
            Kind::Environment => config.environments.contains_key(name),
        };
        if configured {
            Ok(())
        } else {
            Err(RegistryError::NotConfigured {
                kind,
                name: name.clone(),
            })
        }
    }

    /// The provider registered for instance `name`.
    pub fn provider(&self, name: &str) -> Option<Arc<dyn Provider>> {
        get(&self.providers, name)
    }

    /// The channel registered for instance `name`.
    pub fn channel(&self, name: &str) -> Option<Arc<dyn Channel>> {
        get(&self.channels, name)
    }

    /// The built-in web channel, typed, if `giles.toml` configures one: its pages
    /// ([`WebApp`](crate::channels::web::WebApp)), approval prompts, and login links
    /// need more than [`Channel`].
    #[cfg(feature = "channel-web")]
    pub fn web_channel(&self) -> Option<Arc<crate::channels::web::WebChannel>> {
        self.web.get().cloned()
    }

    /// The toolset registered for instance `name`.
    pub fn toolset(&self, name: &str) -> Option<Arc<dyn Toolset>> {
        get(&self.toolsets, name)
    }

    /// The configured environment instance `name`, with its client if it is connected.
    pub fn environment(&self, name: &str) -> Option<EnvironmentEntry> {
        let config = self.config();
        let instance = config.environments.get(name)?;
        let states = self
            .environments
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        Some(environment_entry(instance, states.get(name)))
    }

    /// Every configured environment instance, by name, connected or not.
    pub fn environments(&self) -> Vec<EnvironmentEntry> {
        let config = self.config();
        let states = self
            .environments
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        config
            .environments
            .values()
            .map(|instance| environment_entry(instance, states.get(&instance.name)))
            .collect()
    }

    /// A receiver that wakes whenever an environment plugin connects (a registration through
    /// [`register_environment`](Self::register_environment)) or disconnects
    /// ([`unregister_environment`](Self::unregister_environment)). The value only counts the
    /// changes; read [`environments`](Self::environments) to see what is connected.
    pub fn environment_changes(&self) -> watch::Receiver<u64> {
        self.environment_changes.subscribe()
    }

    /// Every registered provider, by instance name.
    pub fn providers(&self) -> Vec<(InstanceName, Arc<dyn Provider>)> {
        all(&self.providers)
    }

    /// Every registered channel, by instance name.
    pub fn channels(&self) -> Vec<(InstanceName, Arc<dyn Channel>)> {
        all(&self.channels)
    }

    /// Every registered toolset, by instance name.
    pub fn toolsets(&self) -> Vec<(InstanceName, Arc<dyn Toolset>)> {
        all(&self.toolsets)
    }

    /// Configured channels, environments, and toolsets that nothing has registered yet, by kind
    /// and name, with the part of Giles that will start them.
    pub fn pending(&self) -> Vec<Pending> {
        let config = self.config();
        let channels = config
            .channels
            .values()
            .filter(|c| self.channel(c.name.as_str()).is_none())
            .map(|c| Pending {
                kind: Kind::Channel,
                name: c.name.clone(),
                reason: match c.runtime {
                    ChannelRuntime::Plugin(_) => {
                        "registered by the plugin host once the plugin is up"
                    }
                    ChannelRuntime::Remote(_) => "pending task 3.7 (remote plugins)",
                    ChannelRuntime::BuiltIn => "the built-in channel was not constructed",
                },
            });
        let environments = config
            .environments
            .values()
            .filter(|e| {
                self.environment(e.name.as_str())
                    .is_none_or(|e| !e.reachable())
            })
            .map(|e| Pending {
                kind: Kind::Environment,
                name: e.name.clone(),
                reason: match e.runtime {
                    EnvironmentRuntime::Plugin(_) => {
                        "registered by the plugin host once the plugin is up"
                    }
                    EnvironmentRuntime::Remote(_) => {
                        "registered by the plugin host once it has connected to the plugin"
                    }
                },
            });
        let toolsets = config
            .toolsets
            .values()
            .filter(|t| self.toolset(t.name.as_str()).is_none())
            .map(|t| Pending {
                kind: Kind::Toolset,
                name: t.name.clone(),
                reason: "registered by the toolset host",
            });
        channels.chain(environments).chain(toolsets).collect()
    }

    /// Logs one line per [pending](Self::pending) instance and a summary of what's registered.
    pub fn log_summary(&self) {
        for pending in self.pending() {
            info!(
                kind = %pending.kind,
                instance = %pending.name,
                reason = pending.reason,
                "instance not started",
            );
        }
        info!(
            providers = self.providers().len(),
            channels = self.channels().len(),
            toolsets = self.toolsets().len(),
            environments = self.environments().iter().filter(|e| e.reachable()).count(),
            tools = self.tools.snapshot().len(),
            pending = self.pending().len(),
            "registry ready",
        );
    }

    /// The provider and model id for `model`.
    ///
    /// # Errors
    ///
    /// [`RegistryError::UnknownProvider`] if no provider is registered for its instance.
    pub fn resolve_model(&self, model: &ModelRef) -> Result<ResolvedModel, RegistryError> {
        let provider = self
            .provider(model.provider.as_str())
            .ok_or_else(|| RegistryError::UnknownProvider(model.provider.clone()))?;
        Ok(ResolvedModel {
            instance: model.provider.clone(),
            provider,
            model: model.model.clone(),
        })
    }

    /// The model `agent`'s turns use: the first in `models` (v1 has no fallback, PLAN section
    /// 1.1).
    ///
    /// # Errors
    ///
    /// [`RegistryError::NoModels`] or [`RegistryError::UnknownProvider`].
    pub fn agent_model(&self, agent: &AgentConfig) -> Result<ResolvedModel, RegistryError> {
        let model = agent
            .models
            .first()
            .ok_or_else(|| RegistryError::NoModels(agent.id.clone()))?;
        self.resolve_model(model)
    }

    /// The model that writes `agent`'s compaction summaries: `compaction_model`, else the
    /// [`agent_model`](Self::agent_model).
    ///
    /// # Errors
    ///
    /// As [`agent_model`](Self::agent_model).
    pub fn compaction_model(&self, agent: &AgentConfig) -> Result<ResolvedModel, RegistryError> {
        match &agent.compaction_model {
            Some(model) => self.resolve_model(model),
            None => self.agent_model(agent),
        }
    }
}

/// `instance` as the config says and `state` says it is connected.
fn environment_entry(
    instance: &crate::config::EnvironmentInstance,
    state: Option<&EnvironmentState>,
) -> EnvironmentEntry {
    EnvironmentEntry {
        name: instance.name.clone(),
        description: instance.description.clone(),
        stop_after: instance.stop_after,
        delete_after: instance.delete_after,
        options_schema: state.and_then(|state| state.options_schema.clone()),
        connections: state.map_or(0, |state| state.connections),
        client: state.and_then(|state| state.client.clone()),
    }
}

/// A built-in component and the keys of its `config` table that it ignored, or the `giles.toml` key
/// under the instance table and the reason it can't be built.
type Built<T> = Result<(Arc<T>, Vec<KeyPath>), (&'static str, String)>;

/// Builds a built-in provider; the error is the key under `[provider.<instance>]` and the
/// reason. One arm per built-in component name.
#[cfg_attr(
    not(any(feature = "provider-anthropic", feature = "provider-openai")),
    allow(unused_variables)
)]
fn build_provider(
    instance: &ProviderInstance,
    media: &Arc<dyn MediaSource>,
) -> Built<dyn Provider> {
    #[cfg(any(feature = "provider-anthropic", feature = "provider-openai"))]
    let invalid = |e: giles_types::Error| ("config", instance.config.scrub(&e.to_string()));
    match instance.component.as_str() {
        #[cfg(feature = "provider-anthropic")]
        "anthropic" => {
            use crate::providers::anthropic::{AnthropicConfig, AnthropicProvider};
            let config = AnthropicConfig::from_json(&instance.config.to_json()).map_err(invalid)?;
            let unknown = config.unknown_keys.clone();
            let provider = AnthropicProvider::new(config, media.clone()).map_err(invalid)?;
            Ok((Arc::new(provider), unknown))
        }
        #[cfg(not(feature = "provider-anthropic"))]
        "anthropic" => Err((
            "component",
            "this giles was built without the `provider-anthropic` feature".to_owned(),
        )),
        #[cfg(feature = "provider-openai")]
        "openai" => {
            use crate::providers::openai::{OpenAiConfig, OpenAiProvider};
            let config = OpenAiConfig::from_json(&instance.config.to_json()).map_err(invalid)?;
            let unknown = config.unknown_keys.clone();
            let provider = OpenAiProvider::new(config, media.clone()).map_err(invalid)?;
            Ok((Arc::new(provider), unknown))
        }
        #[cfg(not(feature = "provider-openai"))]
        "openai" => Err((
            "component",
            "this giles was built without the `provider-openai` feature".to_owned(),
        )),
        other => Err(("component", format!("no built-in provider {other:?}"))),
    }
}

/// Builds a built-in channel; the error is the key under `[channel.<instance>]` and the reason.
/// One arm per built-in component name.
#[cfg_attr(not(feature = "channel-web"), allow(unused_variables))]
fn build_channel(registry: &Registry, instance: &ChannelInstance) -> Built<dyn Channel> {
    match instance.component.as_str() {
        #[cfg(feature = "channel-web")]
        "web" => {
            use crate::channels::web::WebChannel;
            // One at most: two would both serve `/` and the trusted-proxy socket.
            if let Some(first) = registry.web.get() {
                return Err((
                    "component",
                    format!(
                        "only one web channel instance is allowed, and `[channel.{}]` is one",
                        first.instance()
                    ),
                ));
            }
            let web = Arc::new(WebChannel::from_instance(instance, &registry.config())?);
            registry.web.get_or_init(|| web.clone());
            let unknown = web.config().unknown_keys.clone();
            Ok((web, unknown))
        }
        #[cfg(not(feature = "channel-web"))]
        "web" => Err((
            "component",
            "this giles was built without the `channel-web` feature".to_owned(),
        )),
        other => Err(("component", format!("no built-in channel {other:?}"))),
    }
}
