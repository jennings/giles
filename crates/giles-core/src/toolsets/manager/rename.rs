//! Renaming a stored toolset: [`ToolsetManager::rename`] and [`ToolsetManager::rename_preview`].
//! The steps and their order are in the module docs of [`super`].

use std::path::PathBuf;

use giles_store::StoredToolset;
use giles_types::{AgentId, Timestamp};
use tracing::{info, warn};

use super::{Target, ToolsetManager, ToolsetManagerError, ToolsetView, lock};
use crate::agents::EditError;
use crate::agents::edit::{read_agent_toml, replace_agent_toml};
use crate::config::{AgentConfig, Config, InstanceName, ToolsetInstance, ToolsetSpec};
pub use crate::rename::GlobChange;
use crate::rename::{ToolsetGlobs, rewrite_toolset_globs};

/// The tool globs of one agent that a rename rewrites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentGlobChange {
    /// The agent's id.
    pub agent: String,
    /// The globs that change, `tools` before `ask`, in the order of the file.
    pub globs: Vec<GlobChange>,
}

/// What a rename would change beyond the toolset itself, as
/// [`rename_preview`](ToolsetManager::rename_preview) reports it before the owner confirms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamePreview {
    /// The agents whose `tools` or `ask` globs change, ordered by id. An agent none of whose
    /// globs start with `<old>__` isn't listed.
    pub agents: Vec<AgentGlobChange>,
}

/// What [`rename`](ToolsetManager::rename) did.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolsetRenamed {
    /// The name the toolset had, as stored. The name given to
    /// [`rename`](ToolsetManager::rename) matches it ignoring case, so this can be spelled
    /// differently.
    pub from: String,
    /// The toolset under its new name. If it couldn't be started under that name, the view's
    /// `failure` says why (the rename itself is done).
    pub view: ToolsetView,
    /// The agents whose globs were rewritten, as the preview listed them.
    pub agents: Vec<AgentGlobChange>,
}

/// One agent's `agent.toml`, rewritten.
struct AgentRewrite {
    id: AgentId,
    dir: PathBuf,
    before: String,
    after: String,
    /// The agent as it will be in the live configuration.
    config: AgentConfig,
    changes: Vec<GlobChange>,
}

/// Everything a rename will do, decided (and every check made) before anything changes.
struct Plan {
    /// The stored toolset as it is now.
    row: StoredToolset,
    new: InstanceName,
    /// The instance running under the old name, and the one that will run under the new name.
    running: Option<ToolsetInstance>,
    renamed: Option<ToolsetInstance>,
    agents: Vec<AgentRewrite>,
}

impl Plan {
    fn glob_changes(&self) -> Vec<AgentGlobChange> {
        self.agents
            .iter()
            .map(|agent| AgentGlobChange {
                agent: agent.id.to_string(),
                globs: agent.changes.clone(),
            })
            .collect()
    }
}

/// A completed step of a rename, enough to take it back.
enum Step {
    /// The running toolset was stopped and left the registry.
    Stopped,
    Wrote {
        dir: PathBuf,
        before: String,
        after: String,
    },
    /// The row has the new name.
    Renamed,
    /// The live configuration has the toolset under the new name.
    Configured,
}

impl ToolsetManager {
    /// What renaming the stored toolset `old` to `new` would change: runs every check of
    /// [`rename`](Self::rename) and changes nothing.
    ///
    /// # Errors
    ///
    /// What [`rename`](Self::rename) refuses before it changes anything: a name that is the same,
    /// invalid, or taken ([`ToolsetManagerError::Invalid`] at the root, or
    /// [`ToolsetManagerError::Exists`] for a stored toolset), agent globs that can't be rewritten
    /// ([`ToolsetManagerError::Refused`]), [`ToolsetManagerError::NotFound`],
    /// [`ToolsetManagerError::Unsupported`], [`ToolsetManagerError::Store`].
    pub async fn rename_preview(
        &self,
        old: &str,
        new: &str,
    ) -> Result<RenamePreview, ToolsetManagerError> {
        let _changing = self.changing.lock().await;
        let plan = self.plan_rename(old, new).await?;
        Ok(RenamePreview {
            agents: plan.glob_changes(),
        })
    }

    /// Renames the stored toolset `old` (ignoring ASCII case) to `new`, and everything that goes
    /// with its name: see the module docs of [`super`] for the steps and their undo. A toolset
    /// that failed to start is renamed too, and started if it validates now. Renaming it to
    /// its own name in another case is allowed. Sessions that run keep the old tool names; a
    /// call to one gets `tool unavailable`.
    ///
    /// # Errors
    ///
    /// [`ToolsetManagerError::NotFound`] if no stored toolset has the name;
    /// [`ToolsetManagerError::Unsupported`] if it is a toolset of `giles.toml`;
    /// [`ToolsetManagerError::Invalid`] (at the root) if `new` is the same name, isn't valid, is
    /// reserved, is taken by an instance of `giles.toml`, or, for a running
    /// toolset, if its spec doesn't validate under the new name; [`ToolsetManagerError::Exists`]
    /// if another stored toolset has `new`;
    /// [`ToolsetManagerError::Refused`] if an agent has globs that match the toolset's tools
    /// without starting with `<old>__` (`github*`), listed in the reason, or an `agent.toml` that
    /// can't be edited; [`ToolsetManagerError::Store`]. In all of these nothing changed.
    /// [`ToolsetManagerError::Failed`] if a step failed after the first change: the earlier steps
    /// were undone, except what the error lists.
    pub async fn rename(
        &self,
        old: &str,
        new: &str,
    ) -> Result<ToolsetRenamed, ToolsetManagerError> {
        let _changing = self.changing.lock().await;
        // The agents' files are rewritten, so no agent edit may run meanwhile.
        let _agents = self.agent_lock.lock().await;
        let plan = self.plan_rename(old, new).await?;
        let mut done = Vec::new();
        let renamed = match self.carry_out(&plan, &mut done).await {
            Ok(row) => row,
            Err((step, error)) => {
                let undo_errors = self.take_back(&plan, done).await;
                warn!(toolset = %plan.row.name, %step, %error, "a toolset rename failed");
                return Err(ToolsetManagerError::Failed {
                    step,
                    error,
                    undo_errors,
                });
            }
        };
        match &plan.renamed {
            Some(instance) => {
                lock(&self.state).running.remove(&plan.row.name);
                match self.host.add(instance).await {
                    Ok(()) => {
                        lock(&self.state).running.insert(renamed.name.clone());
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        warn!(toolset = %renamed.name, %reason, "a renamed toolset is not started");
                        self.withdraw(&renamed.name, reason);
                    }
                }
            }
            None => {
                lock(&self.state).failed.remove(&plan.row.name);
                if let Err(reason) = self.start_stored(&renamed).await {
                    warn!(toolset = %renamed.name, %reason, "a renamed toolset is not started");
                    self.set_failed(&renamed.name, reason);
                }
            }
        }
        info!(from = %plan.row.name, to = %renamed.name, "stored toolset renamed");
        Ok(ToolsetRenamed {
            from: plan.row.name.clone(),
            view: self.view_of(&renamed),
            agents: plan.glob_changes(),
        })
    }

    /// Makes every check of a rename and decides every step, changing nothing.
    async fn plan_rename(&self, old: &str, new: &str) -> Result<Plan, ToolsetManagerError> {
        let rows = self.store.toolsets().await?;
        let row = match self.target(&rows, old) {
            Target::Stored(row) => row,
            Target::File => return Err(ToolsetManagerError::Unsupported),
            Target::Missing => return Err(ToolsetManagerError::NotFound),
        };

        // The new name: valid and free among every kind of instance, as for a new toolset. The
        // old name is exempt, so the case of a name can change. A remote toolset has no data
        // directory, so data under `data/plugins/<new>/` (left by an earlier instance) is no
        // reason to refuse, as it isn't for a new toolset.
        if row.name == new {
            return Err(ToolsetManagerError::whole(
                "the toolset already has that name",
            ));
        }
        let new_name = InstanceName::new(new).map_err(ToolsetManagerError::whole)?;
        if rows
            .iter()
            .any(|other| other.name.eq_ignore_ascii_case(new) && other.name != row.name)
        {
            return Err(ToolsetManagerError::Exists);
        }
        self.check_name(new).map_err(ToolsetManagerError::invalid)?;

        let config = self.config.get();
        let old_name = InstanceName::new(row.name.as_str()).ok();
        let running = match &old_name {
            Some(name) if lock(&self.state).running.contains(&row.name) => {
                config.toolsets.get(name.as_str()).cloned()
            }
            _ => None,
        };
        let renamed = match (&running, &old_name) {
            (Some(_), Some(old_name)) => {
                let spec = ToolsetSpec::from_json(&row.spec)
                    .map_err(|_| ToolsetManagerError::whole("the stored spec can't be read"))?;
                Some(
                    self.check(new, &spec, Some(old_name))
                        .map_err(ToolsetManagerError::invalid)?,
                )
            }
            _ => None,
        };

        // A stored toolset that lost its name to an instance of `giles.toml` at startup isn't
        // that instance: the agents' globs under the name are the instance's, and are left alone.
        let agents = if self.file_instance(&config, &row.name).is_some() {
            Vec::new()
        } else {
            self.plan_agents(&config, &row.name, new).await?
        };
        Ok(Plan {
            row,
            new: new_name,
            running,
            renamed,
            agents,
        })
    }

    /// The rewrite of every agent's `agent.toml` that names the toolset `old`. Reads the files
    /// as the agent edit form does, with the rules `giles instance rename` has for its globs, and
    /// refuses, naming every glob, if some glob can't be rewritten safely or some file can't be
    /// edited.
    async fn plan_agents(
        &self,
        config: &Config,
        old: &str,
        new: &str,
    ) -> Result<Vec<AgentRewrite>, ToolsetManagerError> {
        let dirs: Vec<(AgentId, PathBuf)> = config
            .agents
            .values()
            .map(|agent| (agent.id.clone(), agent.dir.clone()))
            .collect();
        let texts = blocking(move || {
            dirs.into_iter()
                .map(|(id, dir)| {
                    let text = read_agent_toml(&dir).map_err(|error| match error {
                        EditError::Io { path, source } => {
                            (true, format!("cannot read {path}: {source}"))
                        }
                        other => (false, other.form_message()),
                    });
                    (id, dir, text)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|error| ToolsetManagerError::Failed {
            step: "reading the agents' files".to_owned(),
            error,
            undo_errors: Vec::new(),
        })?;

        let mut ambiguous = Vec::new();
        let mut rewrites = Vec::new();
        for (id, dir, text) in texts {
            let before = match text {
                Ok(text) => text,
                Err((true, error)) => {
                    return Err(ToolsetManagerError::Failed {
                        step: format!("reading the agent `{id}`"),
                        error,
                        undo_errors: Vec::new(),
                    });
                }
                Err((false, error)) => {
                    return Err(ToolsetManagerError::Refused {
                        reason: format!(
                            "the agent `{id}` can't be checked for globs that name the \
                             toolset: {error}"
                        ),
                    });
                }
            };
            match rewrite_toolset_globs(&before, old, new) {
                Err(error) => {
                    return Err(ToolsetManagerError::Refused {
                        reason: format!(
                            "the agent `{id}` can't be checked for globs that name the \
                             toolset: its agent.toml is {error}; fix it by hand"
                        ),
                    });
                }
                Ok(ToolsetGlobs::Unchanged) => {}
                Ok(ToolsetGlobs::Ambiguous(globs)) => ambiguous.push((id, globs)),
                Ok(ToolsetGlobs::Rewritten { text, changes }) => {
                    rewrites.push((id, dir, before, text, changes));
                }
            }
        }
        if !ambiguous.is_empty() {
            let list: Vec<String> = ambiguous
                .iter()
                .map(|(id, globs)| format!("agent `{id}`: {}", globs.join(", ")))
                .collect();
            return Err(ToolsetManagerError::Refused {
                reason: format!(
                    "these globs match `{old}__` tools without starting with `{old}__`, so they \
                     would stop matching after the rename to {new:?}: {}; rewrite them first \
                     (for example to `{old}__*`)",
                    list.join("; ")
                ),
            });
        }

        let mut agents = Vec::new();
        for (id, dir, before, after, changes) in rewrites {
            let config_of_agent = config
                .validate_agent(&self.root, id.as_str(), &after)
                .map_err(|error| ToolsetManagerError::Refused {
                    reason: format!(
                        "the agent `{id}` would not load after its globs are rewritten: {error}"
                    ),
                })?;
            agents.push(AgentRewrite {
                id,
                dir,
                before,
                after,
                config: config_of_agent,
                changes,
            });
        }
        Ok(agents)
    }

    /// Does the steps of `plan` up to the live configuration, recording each in `done` as it is
    /// taken. Returns the renamed row, or the step that failed and why.
    async fn carry_out(
        &self,
        plan: &Plan,
        done: &mut Vec<Step>,
    ) -> Result<StoredToolset, (String, String)> {
        if let Some(instance) = &plan.running {
            self.host.remove(&instance.name).await;
            done.push(Step::Stopped);
        }

        for agent in &plan.agents {
            let why = format!("rewriting {}", agent.dir.join("agent.toml").display());
            let (dir, before) = (agent.dir.clone(), agent.before.clone());
            let after = agent.after.clone();
            let (d, b, a) = (dir.clone(), before.clone(), after.clone());
            blocking(move || replace_agent_toml(&d, &b, &a).map_err(|error| edit_reason(&error)))
                .await
                .and_then(|result| result)
                .map_err(|error| (why, error))?;
            done.push(Step::Wrote { dir, before, after });
        }

        let renamed = self
            .store
            .toolset_rename(&plan.row.name, plan.new.as_str(), Timestamp::now())
            .await
            .map_err(|error| {
                (
                    "renaming the toolset in the database".to_owned(),
                    error.to_string(),
                )
            })?;
        done.push(Step::Renamed);

        if let (Some(old), Some(new)) = (&plan.running, &plan.renamed) {
            let why = "renaming the toolset in the running configuration";
            self.config
                .rename_toolset(&old.name, new.name.clone())
                .map_err(|error| (why.to_owned(), error.to_string()))?;
            done.push(Step::Configured);
            self.config
                .replace_toolset(new.clone())
                .map_err(|error| (why.to_owned(), error.to_string()))?;
        }
        for agent in &plan.agents {
            if let Err(error) = self.config.replace_agent(agent.config.clone()) {
                warn!(agent = %agent.id, %error, "an agent rewritten by a toolset rename is gone");
            }
        }
        Ok(renamed)
    }

    /// Takes the `done` steps of `plan` back, newest first. Returns what couldn't be undone.
    async fn take_back(&self, plan: &Plan, done: Vec<Step>) -> Vec<String> {
        let mut errors = Vec::new();
        for step in done.into_iter().rev() {
            match step {
                Step::Configured => {
                    if let (Some(old), Some(new)) = (&plan.running, &plan.renamed) {
                        let back = self
                            .config
                            .rename_toolset(&new.name, old.name.clone())
                            .map(drop)
                            .map_err(|error| error.to_string())
                            .and_then(|()| {
                                self.config
                                    .replace_toolset(old.clone())
                                    .map(drop)
                                    .map_err(|error| error.to_string())
                            });
                        if let Err(error) = back {
                            errors.push(format!(
                                "put the toolset back in the running configuration: {error}"
                            ));
                        }
                    }
                }
                Step::Renamed => {
                    if let Err(error) = self
                        .store
                        .toolset_restore(plan.new.as_str(), &plan.row)
                        .await
                    {
                        errors.push(format!("put the toolset's row back: {error}"));
                    }
                }
                Step::Wrote { dir, before, after } => {
                    let path = dir.join("agent.toml");
                    let back = blocking(move || {
                        replace_agent_toml(&dir, &after, &before)
                            .map_err(|error| edit_reason(&error))
                    })
                    .await
                    .and_then(|result| result);
                    if let Err(error) = back {
                        errors.push(format!("put {} back: {error}", path.display()));
                    }
                }
                Step::Stopped => {
                    if let Some(old) = &plan.running
                        && let Err(error) = self.host.add(old).await
                    {
                        errors.push(format!("start the toolset again: {error}"));
                    }
                }
            }
        }
        errors
    }
}

/// Runs `work` where it may block, for the file system steps.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| error.to_string())
}

/// Why `agent.toml` couldn't be replaced, for the error of a step.
fn edit_reason(error: &EditError) -> String {
    match error {
        EditError::Stale => "the file changed while the rename ran".to_owned(),
        other => other.form_message(),
    }
}
