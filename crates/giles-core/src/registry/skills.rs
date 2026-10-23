//! The per-agent skill index (PLAN section 6.6): which skill directories an agent sees.

use giles_types::AgentId;
use tracing::warn;

use super::{Registry, RegistryError};
use crate::paths::PathsError;
use crate::skills::{self, SkillSet, SkillSource};

impl Registry {
    /// The skill sources for `agent`, lowest precedence first: every configured toolset's
    /// `skills` directories (by instance name, then in listed order; their skills are named
    /// `<instance>__<skill>`), then `<root>/skills/`, then `agents/<agent>/skills/`. Local skills
    /// are bare, so only an agent skill can override a global one.
    ///
    /// # Errors
    ///
    /// [`PathsError::InvalidName`] if `agent` isn't a valid directory name.
    pub fn skill_sources(&self, agent: &AgentId) -> Result<Vec<SkillSource>, PathsError> {
        let toolsets = self.config.toolsets.values().flat_map(|toolset| {
            toolset.skills.iter().map(|dir| SkillSource {
                dir: dir.clone(),
                prefix: Some(toolset.name.to_string()),
            })
        });
        let local = skills::agent_sources(&self.root, agent.as_str())?;
        Ok(toolsets.chain(local).collect())
    }

    /// Discovers `agent`'s skills from [`skill_sources`](Self::skill_sources) on a blocking
    /// thread. Skills that fail to load are logged and skipped.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Paths`] for an invalid agent name, [`RegistryError::Task`] if the
    /// blocking task fails.
    pub async fn agent_skills(&self, agent: &AgentId) -> Result<SkillSet, RegistryError> {
        let sources = self.skill_sources(agent)?;
        let discovery = tokio::task::spawn_blocking(move || skills::discover(&sources)).await?;
        for warning in &discovery.warnings {
            warn!(%agent, %warning, "skill skipped");
        }
        Ok(discovery.skills)
    }
}
