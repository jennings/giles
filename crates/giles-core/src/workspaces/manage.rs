//! What the web UI's workspaces page needs: an overview of every workspace joined with what the
//! plugins say now, and stopping and deleting a workspace by id on an operator's say-so.
//!
//! The overview asks every connected plugin for its workspaces at the same time and never caches.
//! A plugin that is down or fails leaves its workspaces' live fields [`Live::Unknown`], and the
//! environment's [`EnvironmentView::problem`] says why.
//!
//! An operator may stop or delete a workspace a running turn is using; the page asks for
//! confirmation first ([`Workspaces::in_use`]). The turn's next workspace call finds the workspace
//! stopped (and starts it again) or deleted (and the model is told an operator deleted it).
//! Docs/technical/environments.md describes the whole picture.

use std::collections::{HashMap, HashSet};

use futures::future::join_all;
use giles_types::{Workspace, WorkspaceId};

use super::{DeleteReason, WorkspaceRecord, WorkspaceState, Workspaces};

/// Why stopping or deleting a workspace on an operator's say-so didn't happen. The messages are
/// for the operator.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManageError {
    /// There is no such workspace, or it is deleted already.
    #[error("There is no such workspace. It may have been deleted already.")]
    NotFound,
    /// The workspace's plugin call hasn't finished.
    #[error("The workspace is still being created. Try again in a moment.")]
    StillCreating,
    /// The workspace's environment is no longer configured.
    #[error("The environment {environment:?} is not configured any more.")]
    UnknownEnvironment {
        /// The environment instance.
        environment: String,
    },
    /// The workspace's plugin isn't connected.
    #[error("The environment {environment:?} is not connected right now. Try again later.")]
    Unreachable {
        /// The environment instance.
        environment: String,
    },
    /// The plugin failed the call.
    #[error("The environment failed: {0}")]
    Environment(#[source] giles_types::Error),
    /// The database failed.
    #[error("Cannot use the workspace table: {0}")]
    Store(#[from] giles_store::Error),
}

/// What a plugin said about a tracked workspace just now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Live {
    /// The plugin lists it.
    Known(Box<Workspace>),
    /// The plugin answered and doesn't have it. The next reconciliation marks it deleted.
    Missing,
    /// The plugin is down or failed, so nothing is known.
    Unknown,
}

/// A workspace Giles tracks, `creating` or `active`, with the plugin's view of it.
#[derive(Debug, Clone)]
pub struct WorkspaceView {
    /// Giles's row.
    pub record: WorkspaceRecord,
    /// Whether a running turn holds a lease on it right now.
    pub in_use: bool,
    /// What the plugin said.
    pub live: Live,
}

/// An environment instance as the overview saw it.
#[derive(Debug, Clone)]
pub struct EnvironmentView {
    /// The instance name.
    pub name: String,
    /// Free space where the plugin keeps workspaces, if it said.
    pub free_bytes: Option<u64>,
    /// Why the plugin's workspaces aren't known now, as a phrase that follows the environment's
    /// name (not connected, failed); `None` when it answered.
    pub problem: Option<String>,
}

/// Every tracked workspace for the workspaces page.
#[derive(Debug, Clone, Default)]
pub struct Overview {
    /// Every environment that is configured or has a workspace row, by name.
    pub environments: Vec<EnvironmentView>,
    /// The `creating` and `active` workspaces, newest first.
    pub workspaces: Vec<WorkspaceView>,
}

impl Workspaces {
    /// The overview of every environment's workspaces. Plugins are asked at the same time.
    ///
    /// # Errors
    ///
    /// [`ManageError::Store`].
    pub async fn overview(&self) -> Result<Overview, ManageError> {
        let rows = self.inner.store.workspaces_all().await?;
        let entries = self
            .registry()
            .map(|registry| registry.environments())
            .unwrap_or_default();

        let asked = join_all(entries.iter().map(|entry| async move {
            let result = match &entry.client {
                None => Err("is not connected right now".to_owned()),
                Some(client) => client
                    .list_workspaces()
                    .await
                    .map_err(|error| format!("failed to list its workspaces: {error}")),
            };
            (entry.name.to_string(), result)
        }))
        .await;

        let mut answers: HashMap<String, HashMap<WorkspaceId, Workspace>> = HashMap::new();
        let mut environments = Vec::new();
        for (name, result) in asked {
            match result {
                Ok(list) => {
                    environments.push(EnvironmentView {
                        name: name.clone(),
                        free_bytes: list.free_bytes,
                        problem: None,
                    });
                    answers.insert(
                        name,
                        list.workspaces
                            .into_iter()
                            .map(|w| (w.id.clone(), w))
                            .collect(),
                    );
                }
                Err(problem) => environments.push(EnvironmentView {
                    name,
                    free_bytes: None,
                    problem: Some(problem),
                }),
            }
        }
        // Rows of an environment that is no longer configured.
        let configured: HashSet<String> = entries.iter().map(|e| e.name.to_string()).collect();
        for row in &rows {
            if !configured.contains(&row.environment)
                && !environments.iter().any(|e| e.name == row.environment)
            {
                environments.push(EnvironmentView {
                    name: row.environment.clone(),
                    free_bytes: None,
                    problem: Some("is not configured any more".to_owned()),
                });
            }
        }
        environments.sort_by(|a, b| a.name.cmp(&b.name));

        let mut workspaces = Vec::new();
        for row in rows {
            if !matches!(row.state, WorkspaceState::Creating | WorkspaceState::Active) {
                continue;
            }
            let live = match answers.get(&row.environment) {
                // A workspace being created isn't expected to be listed yet.
                Some(_) if row.state == WorkspaceState::Creating => Live::Unknown,
                Some(listed) => listed
                    .get(&row.id)
                    .map_or(Live::Missing, |w| Live::Known(Box::new(w.clone()))),
                None => Live::Unknown,
            };
            workspaces.push(WorkspaceView {
                in_use: self.in_use(&row.id),
                record: row,
                live,
            });
        }
        workspaces.sort_by(|a, b| {
            b.record
                .created_at
                .cmp(&a.record.created_at)
                .then_with(|| a.record.id.cmp(&b.record.id))
        });
        Ok(Overview {
            environments,
            workspaces,
        })
    }

    /// Stops the workspace `id` on an operator's say-so: the plugin is asked to stop it, which
    /// ends its commands and background processes, and nothing else changes. The next workspace
    /// tool call starts it again. Succeeds for a workspace that is stopped already, and for one a
    /// running turn is using (the page asks for confirmation first, see
    /// [`in_use`](Self::in_use)).
    ///
    /// # Errors
    ///
    /// [`ManageError::NotFound`] and [`StillCreating`](ManageError::StillCreating) for a
    /// workspace that isn't `active`; [`UnknownEnvironment`](ManageError::UnknownEnvironment),
    /// [`Unreachable`](ManageError::Unreachable), and [`Environment`](ManageError::Environment)
    /// for the plugin; [`Store`](ManageError::Store).
    pub async fn stop_by_id(&self, id: &WorkspaceId) -> Result<WorkspaceRecord, ManageError> {
        let record = self.managed_workspace(id).await?;
        let client = self.client_of(&record)?;
        client
            .stop_workspace(id)
            .await
            .map_err(ManageError::Environment)?;
        tracing::info!(workspace = %id, environment = %record.environment, "an operator stopped a workspace");
        Ok(record)
    }

    /// Deletes the workspace `id` on an operator's say-so ([`DeleteReason::Operator`], so the
    /// model is told on its next workspace call). If a running turn is using it, the plugin is
    /// asked to stop it first, which ends the turn's command, and the turn's next workspace call
    /// finds the workspace deleted. Like [`Workspaces::delete`], the owner's slot is free at once
    /// and the returned row is `Deleted`, or `Deleting` if the plugin couldn't be told
    /// (unreachable, or it failed), in which case the sweep and the next connection finish the
    /// job.
    ///
    /// # Errors
    ///
    /// [`ManageError::NotFound`] and [`StillCreating`](ManageError::StillCreating) for a
    /// workspace that isn't `active`; [`Store`](ManageError::Store).
    pub async fn delete_by_id(&self, id: &WorkspaceId) -> Result<WorkspaceRecord, ManageError> {
        self.managed_workspace(id).await?;
        // Hold off the sweep if no turn is using the workspace; a turn that is can't be waited
        // for, and its own calls find the row deleted.
        let _claim = self.try_claim(id);
        let now = self.inner.clock.now();
        let Some(mut record) = self
            .inner
            .store
            .workspace_begin_delete(id, DeleteReason::Operator, now)
            .await?
        else {
            return Err(ManageError::NotFound);
        };
        tracing::info!(workspace = %id, environment = %record.environment, "an operator deleted a workspace");
        if let Ok(client) = self.client_of(&record)
            && let Err(error) = client.stop_workspace(id).await
        {
            // Deleting is what was asked for, and the delete below reports its own failure.
            tracing::debug!(workspace = %id, %error, "cannot stop the workspace before deleting it");
        }
        if self.delete_in_plugin(&record).await {
            record.state = WorkspaceState::Deleted;
        }
        Ok(record)
    }

    /// The `active` workspace `id`, for the pages that act on it.
    ///
    /// # Errors
    ///
    /// [`ManageError::NotFound`] and [`StillCreating`](ManageError::StillCreating) for a
    /// workspace that isn't `active`; [`Store`](ManageError::Store).
    pub async fn managed_workspace(
        &self,
        id: &WorkspaceId,
    ) -> Result<WorkspaceRecord, ManageError> {
        match self.inner.store.workspace(id).await? {
            Some(record) if record.state == WorkspaceState::Active => Ok(record),
            Some(record) if record.state == WorkspaceState::Creating => {
                Err(ManageError::StillCreating)
            }
            _ => Err(ManageError::NotFound),
        }
    }

    /// Whether a running turn holds a lease on the workspace now.
    pub fn in_use(&self, id: &WorkspaceId) -> bool {
        let lock = self
            .inner
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned();
        lock.is_some_and(|lock| lock.try_write_owned().is_err())
    }

    /// The connected plugin that holds `record`.
    fn client_of(
        &self,
        record: &WorkspaceRecord,
    ) -> Result<std::sync::Arc<dyn giles_types::Environment>, ManageError> {
        match self.reachable(&record.environment) {
            Ok((_, client)) => Ok(client),
            Err(super::WorkspaceError::UnknownEnvironment { name }) => {
                Err(ManageError::UnknownEnvironment { environment: name })
            }
            Err(_) => Err(ManageError::Unreachable {
                environment: record.environment.clone(),
            }),
        }
    }
}
