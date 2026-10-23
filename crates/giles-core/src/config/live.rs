//! The configuration the running core uses: the one loaded at startup, plus the agents created
//! since (TASKS.md 5.1). Nothing else changes while the core runs; reload stays deferred (D28).

use std::sync::{Arc, PoisonError, RwLock};

use giles_types::AgentId;

use super::{AgentConfig, Config};

/// The running core's configuration, shared by everything that needs to see agents added at
/// runtime: the registry, the session manager, the router, and the core tools. Cheap to clone;
/// clones share the same configuration.
///
/// [`get`](Self::get) returns a snapshot. A caller keeps one snapshot for the work at hand (a
/// turn, a tool call, a page), so it never sees half of a change.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    current: Arc<RwLock<Arc<Config>>>,
}

/// [`LiveConfig::add_agent`] refused an agent: one with the same id, ignoring ASCII case, is
/// already configured.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("there is already an agent `{0}`")]
pub struct AgentExists(pub AgentId);

impl LiveConfig {
    /// Starts with `config`.
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            current: Arc::new(RwLock::new(config)),
        }
    }

    /// The configuration now.
    pub fn get(&self) -> Arc<Config> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        let mut current = self.current.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(existing) = current
            .agents
            .keys()
            .find(|id| id.as_str().eq_ignore_ascii_case(agent.id.as_str()))
        {
            return Err(AgentExists(existing.clone()));
        }
        let mut next = Config::clone(&current);
        next.agents.insert(agent.id.clone(), agent);
        let next = Arc::new(next);
        *current = next.clone();
        Ok(next)
    }
}
