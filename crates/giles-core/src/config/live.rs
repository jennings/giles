//! The configuration the running core uses: the one loaded at startup, plus the agents and toolsets
//! created, edited, renamed, and removed since. Nothing else changes while the core runs;
//! reload stays deferred (D28).

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use giles_types::AgentId;
use tokio::sync::watch;

use super::{AgentConfig, Config, InstanceName, Kind, ToolsetInstance, toolset};

/// The running core's configuration, shared by everything that needs to see agents and toolsets
/// changed at runtime: the registry, the session manager, the router, the scheduler, the toolset
/// host, and the core tools. Cheap to clone; clones share the same configuration.
///
/// [`get`](Self::get) returns a snapshot. A caller keeps one snapshot for the work at hand (a
/// turn, a tool call, a page), so it never sees half of a change.
///
/// State derived from the agents or toolsets follows them: [`subscribe`](Self::subscribe) says
/// when to look at the configuration again, and [`incarnation`](Self::incarnation) tells an agent
/// from another that took its id after a removal.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    state: Arc<RwLock<State>>,
    changes: Arc<watch::Sender<u64>>,
}

#[derive(Debug)]
struct State {
    config: Arc<Config>,
    incarnations: BTreeMap<AgentId, u64>,
    /// The last incarnation handed out.
    last: u64,
}

/// [`LiveConfig::add_agent`] refused an agent: one with the same id, ignoring ASCII case, is
/// already configured.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("there is already an agent `{0}`")]
pub struct AgentExists(pub AgentId);

/// [`LiveConfig::replace_agent`] found no agent with the id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("there is no agent `{0}`")]
pub struct NoSuchAgent(pub AgentId);

/// [`LiveConfig::add_toolset`] or [`LiveConfig::rename_toolset`] refused a name: another instance,
/// of any kind, already has it, ignoring ASCII case.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("there is already a {kind} `{existing}`")]
pub struct InstanceExists {
    /// The kind of the instance that has the name.
    pub kind: Kind,
    /// Its name, which may differ from the one asked for in case.
    pub existing: InstanceName,
}

/// [`LiveConfig::replace_toolset`] or [`LiveConfig::rename_toolset`] found no toolset with the
/// name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("there is no toolset `{0}`")]
pub struct NoSuchToolset(pub InstanceName);

/// [`LiveConfig::rename_toolset`] refused a rename.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RenameToolsetError {
    /// There is no toolset with the old name.
    #[error(transparent)]
    NoSuchToolset(#[from] NoSuchToolset),
    /// Another instance has the new name.
    #[error(transparent)]
    Exists(#[from] InstanceExists),
}

/// The instance, of any kind, whose name is `name` ignoring ASCII case, other than the toolset
/// `except`, as the error a refused name reports.
fn instance_named(
    config: &Config,
    name: &InstanceName,
    except: Option<&InstanceName>,
) -> Option<InstanceExists> {
    toolset::instance_named(config, name.as_str(), except).map(|(kind, existing)| InstanceExists {
        kind,
        existing: existing.clone(),
    })
}

impl LiveConfig {
    /// Starts with `config`.
    pub fn new(config: Arc<Config>) -> Self {
        let incarnations: BTreeMap<AgentId, u64> = config.agents.keys().cloned().zip(1..).collect();
        let last = incarnations.values().copied().max().unwrap_or(0);
        Self {
            state: Arc::new(RwLock::new(State {
                config,
                incarnations,
                last,
            })),
            changes: Arc::new(watch::Sender::new(0)),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The configuration now.
    pub fn get(&self) -> Arc<Config> {
        self.read().config.clone()
    }

    /// Which agent `agent` is now: a number that is the same for as long as the agent is
    /// configured, edits included, and different for an agent that takes its id after a removal.
    /// Numbers aren't reused while the core runs. `None` if no agent has the id. State that
    /// belongs to one agent's life, such as a session's cached prompt, is only valid for its
    /// incarnation.
    pub fn incarnation(&self, agent: &AgentId) -> Option<u64> {
        self.read().incarnations.get(agent).copied()
    }

    /// A receiver that sees every later change to the agents and toolsets. It holds a counter that
    /// only goes up; read the configuration ([`get`](Self::get)) when it changes.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Applies `change` to the state under the write lock. It returns the new configuration, or
    /// `None` if it left everything as it was, and a result. Subscribers are told after the lock
    /// is released.
    fn change<T>(&self, change: impl FnOnce(&mut State) -> (Option<Arc<Config>>, T)) -> T {
        let (result, changed) = {
            let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
            let (next, result) = change(&mut state);
            let changed = next.is_some();
            if let Some(next) = next {
                state.config = next;
            }
            (result, changed)
        };
        if changed {
            self.changes
                .send_modify(|count| *count = count.wrapping_add(1));
        }
        result
    }

    /// Adds `agent` and returns the configuration with it. Snapshots taken before keep the
    /// configuration they had.
    ///
    /// Ids are compared ignoring ASCII case: on a case-insensitive file system (the macOS
    /// default) `Jeeves` and `jeeves` would share a directory.
    ///
    /// # Errors
    ///
    /// [`AgentExists`] (naming the configured agent) if the id is taken; nothing changes.
    pub fn add_agent(&self, agent: AgentConfig) -> Result<Arc<Config>, AgentExists> {
        self.change(|state| {
            if let Some(existing) = state
                .config
                .agents
                .keys()
                .find(|id| id.as_str().eq_ignore_ascii_case(agent.id.as_str()))
            {
                return (None, Err(AgentExists(existing.clone())));
            }
            state.last += 1;
            state.incarnations.insert(agent.id.clone(), state.last);
            let mut next = Config::clone(&state.config);
            next.agents.insert(agent.id.clone(), agent);
            let next = Arc::new(next);
            (Some(next.clone()), Ok(next))
        })
    }

    /// Replaces the configured agent with `agent`'s id (an edit): the same agent, with the same
    /// [`incarnation`](Self::incarnation). Snapshots taken before keep the old one.
    ///
    /// # Errors
    ///
    /// [`NoSuchAgent`] if no agent has that id; nothing changes.
    pub fn replace_agent(&self, agent: AgentConfig) -> Result<Arc<Config>, NoSuchAgent> {
        self.change(|state| {
            if !state.config.agents.contains_key(&agent.id) {
                return (None, Err(NoSuchAgent(agent.id.clone())));
            }
            let mut next = Config::clone(&state.config);
            next.agents.insert(agent.id.clone(), agent);
            let next = Arc::new(next);
            (Some(next.clone()), Ok(next))
        })
    }

    /// Removes the agent `id`. Snapshots taken before still have it. Returns it, or `None` (and
    /// changes nothing) if no agent has that id.
    pub fn remove_agent(&self, id: &AgentId) -> Option<AgentConfig> {
        self.change(|state| {
            if !state.config.agents.contains_key(id) {
                return (None, None);
            }
            state.incarnations.remove(id);
            let mut next = Config::clone(&state.config);
            let removed = next.agents.remove(id);
            (Some(Arc::new(next)), removed)
        })
    }

    /// Adds `toolset` and returns the configuration with it. Snapshots taken before keep the
    /// configuration they had. Starting it is the toolset host's job
    /// ([`ToolsetHost::add`](crate::toolsets::ToolsetHost::add)).
    ///
    /// Names are compared ignoring ASCII case and across kinds, as in `giles.toml`: instance
    /// names key data directories and tool prefixes, and are unique among providers, channels,
    /// and toolsets.
    ///
    /// # Errors
    ///
    /// [`InstanceExists`] (naming the instance that has the name) if it is taken; nothing
    /// changes.
    pub fn add_toolset(&self, toolset: ToolsetInstance) -> Result<Arc<Config>, InstanceExists> {
        self.change(|state| {
            if let Some(taken) = instance_named(&state.config, &toolset.name, None) {
                return (None, Err(taken));
            }
            let mut next = Config::clone(&state.config);
            next.toolsets.insert(toolset.name.clone(), toolset);
            let next = Arc::new(next);
            (Some(next.clone()), Ok(next))
        })
    }

    /// Replaces the configured toolset with `toolset`'s name (an edit). Snapshots taken before
    /// keep the old one.
    ///
    /// # Errors
    ///
    /// [`NoSuchToolset`] if no toolset has that name; nothing changes.
    pub fn replace_toolset(&self, toolset: ToolsetInstance) -> Result<Arc<Config>, NoSuchToolset> {
        self.change(|state| {
            if !state.config.toolsets.contains_key(&toolset.name) {
                return (None, Err(NoSuchToolset(toolset.name.clone())));
            }
            let mut next = Config::clone(&state.config);
            next.toolsets.insert(toolset.name.clone(), toolset);
            let next = Arc::new(next);
            (Some(next.clone()), Ok(next))
        })
    }

    /// Removes the toolset `name`. Snapshots taken before still have it. Returns it, or `None`
    /// (and changes nothing) if no toolset has that name.
    pub fn remove_toolset(&self, name: &InstanceName) -> Option<ToolsetInstance> {
        self.change(|state| {
            if !state.config.toolsets.contains_key(name) {
                return (None, None);
            }
            let mut next = Config::clone(&state.config);
            let removed = next.toolsets.remove(name);
            (Some(Arc::new(next)), removed)
        })
    }

    /// Gives the toolset `from` the name `to`, keeping everything else about it, and returns the
    /// configuration with it. Snapshots taken before keep the old name. Renaming a toolset to
    /// its own name, or to the same name in another case, is allowed. The caller removes the old
    /// name from the toolset host and adds the new one.
    ///
    /// # Errors
    ///
    /// [`RenameToolsetError::NoSuchToolset`] if no toolset is named `from`, and
    /// [`RenameToolsetError::Exists`] if another instance of any kind has the name `to`, ignoring
    /// ASCII case; nothing changes.
    pub fn rename_toolset(
        &self,
        from: &InstanceName,
        to: InstanceName,
    ) -> Result<Arc<Config>, RenameToolsetError> {
        self.change(|state| {
            if !state.config.toolsets.contains_key(from) {
                return (None, Err(NoSuchToolset(from.clone()).into()));
            }
            if let Some(taken) = instance_named(&state.config, &to, Some(from)) {
                return (None, Err(taken.into()));
            }
            let mut next = Config::clone(&state.config);
            if let Some(mut toolset) = next.toolsets.remove(from) {
                toolset.name = to.clone();
                next.toolsets.insert(to, toolset);
            }
            let next = Arc::new(next);
            (Some(next.clone()), Ok(next))
        })
    }
}
