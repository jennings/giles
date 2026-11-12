//! Environment workspaces (PLAN section 8.8): the core's side of the workspaces environment
//! plugins hold for the model's tools. [`Workspaces`] is the service the core tools call: it
//! creates, opens, and deletes workspaces for their owners, keeps the `workspaces` table
//! (Docs/technical/storage.md) in step with the plugins, and hands out leases that keep the
//! sweep from stopping or deleting a workspace a turn is using. The sweep and the reconciliation
//! on connect are in [`sweep`], and what the web UI's workspaces page does (an overview, and
//! stopping and deleting a workspace on an operator's say-so, leased or not) is in [`manage`];
//! Docs/technical/environments.md describes the whole life cycle.
//!
//! A workspace has one owner: a conversation (agent, channel instance, conversation id, the key
//! of a session) or an isolated job run ([`WorkspaceOwner::of_session`]). The core assigns the
//! workspace id and the plugin labels its resources with it, so nothing but the id ties the
//! table to the plugin.
//!
//! # Leases
//!
//! A turn holds the lease of its workspace from the first [`Workspaces::open`] until the turn
//! ends. The turn carries a [`LeaseSlot`] ([`TurnContext::lease`](crate::core_tools::TurnContext));
//! it is a shared lock per workspace, so any number of calls of the turn share one lease, and a
//! sweep that wants to stop or delete the workspace takes the exclusive side without waiting
//! ([`Workspaces::try_claim`]) and skips it if it can't. A call that opens the workspace while a
//! sweep holds the claim waits for the sweep to finish. Leases are in memory: a restart drops
//! them with the turns.

#[cfg(test)]
pub(crate) mod fake;
mod manage;
mod sweep;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use giles_store::{NewWorkspace, Store};
use giles_types::{Environment, StartOutcome, Timestamp, WorkspaceId};
use serde_json::Value;
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use crate::registry::{EnvironmentEntry, Registry};
use crate::sessions::Clock;

pub use giles_store::{DeleteReason, WorkspaceOwner, WorkspaceRecord, WorkspaceState};
pub use manage::{EnvironmentView, Live, ManageError, Overview, WorkspaceView};
pub use sweep::{RECONCILE_GRACE, SWEEP_FIRST_DELAY, SWEEP_INTERVAL, SweepReport, WorkspaceSweep};

/// Why a workspace call failed. The messages are written for the model: a core tool shows
/// [`to_string`](ToString::to_string) as its error result.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WorkspaceError {
    /// The owner has a workspace already; there is one per owner.
    #[error(
        "you already have a workspace ({}, in the environment {:?}); delete or reset it before \
         creating another",
        .current.id, .current.environment
    )]
    AlreadyHasWorkspace {
        /// The workspace the owner has.
        current: Box<WorkspaceRecord>,
    },
    /// No `[environment.<name>]` is configured.
    #[error("there is no environment named {name:?}")]
    UnknownEnvironment {
        /// The name.
        name: String,
    },
    /// The environment's plugin isn't connected.
    #[error("the environment {environment:?} is not reachable right now; try again later")]
    Unreachable {
        /// The environment instance.
        environment: String,
    },
    /// The owner has no workspace.
    #[error("you have no workspace; create one first")]
    NoWorkspace,
    /// The owner's workspace is still being created.
    #[error("your workspace is still being created; try again in a moment")]
    NotReady,
    /// The environment no longer has the owner's workspace. The row was marked deleted, so the
    /// owner has no workspace now and can create another.
    #[error(
        "your workspace {id} no longer exists in the environment {environment:?} (it was removed \
         outside Giles); create a new one"
    )]
    Gone {
        /// The workspace.
        id: WorkspaceId,
        /// Its environment instance.
        environment: String,
    },
    /// The environment refused the request, e.g. creation options it doesn't accept. The reason
    /// is the plugin's.
    #[error("{0}")]
    Refused(String),
    /// The environment failed in another way, e.g. its backend is down.
    #[error("the environment failed: {0}")]
    Environment(#[source] giles_types::Error),
    /// The database failed.
    #[error("cannot use the workspace table: {0}")]
    Store(#[from] giles_store::Error),
}

impl WorkspaceError {
    /// Maps an error of an environment call: a refused request keeps the plugin's message.
    fn from_plugin(error: giles_types::Error) -> Self {
        match error {
            giles_types::Error::InvalidRequest(message) => Self::Refused(message),
            other => Self::Environment(other),
        }
    }
}

/// What [`Workspaces::recreate`] did.
#[derive(Debug)]
pub struct Recreation {
    /// The workspace that was deleted, as [`Workspaces::delete`] returns it.
    pub discarded: WorkspaceRecord,
    /// The new workspace, or why creating it failed (the owner then has none).
    pub created: Result<WorkspaceRecord, WorkspaceError>,
}

/// A workspace, opened for a turn.
pub struct Opened {
    /// The workspace's row. `last_used_at` is the time of this open.
    pub record: WorkspaceRecord,
    /// The environment's client: run commands and file calls in `record.id` through it. The turn
    /// holds the workspace's lease until it ends.
    pub client: Arc<dyn Environment>,
    /// What the model should be told before the result of its call, if the workspace had to be
    /// started or recreated.
    pub notice: Option<String>,
}

impl fmt::Debug for Opened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opened")
            .field("record", &self.record)
            .field("notice", &self.notice)
            .finish_non_exhaustive()
    }
}

/// Where a turn keeps its workspace lease. Cloning shares the slot; the lease is released when
/// the last clone is dropped (the turn ends), or when [`Workspaces::open`] replaces it with the
/// lease of another workspace (the old one was deleted and a new one created in the same turn).
#[derive(Debug, Clone, Default)]
pub struct LeaseSlot(Arc<Mutex<Option<Lease>>>);

impl LeaseSlot {
    /// The workspace the turn holds a lease on, if any.
    pub fn held(&self) -> Option<WorkspaceId> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|lease| lease.id.clone())
    }

    fn set(&self, lease: Lease) {
        // The old lease, if any, is released after the lock is, outside it.
        let old = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(lease);
        drop(old);
    }
}

/// A shared hold on one workspace: nothing stops or deletes it while a lease exists.
struct Lease {
    id: WorkspaceId,
    _guard: OwnedRwLockReadGuard<()>,
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease").field("id", &self.id).finish()
    }
}

/// The workspace service. Cheap to clone; the clones share their state.
#[derive(Clone)]
pub struct Workspaces {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    /// Weak, because the registry holds the core tools that hold this service: a strong
    /// reference would keep the registry, and everything it owns, alive for ever.
    registry: Weak<Registry>,
    clock: Arc<dyn Clock>,
    /// One lock per workspace that has been leased: leases take the shared side, the sweep the
    /// exclusive side. Entries nobody holds are dropped by the sweep.
    locks: Mutex<HashMap<WorkspaceId, Arc<RwLock<()>>>>,
    /// Serialises the sweep and the reconciliations, so two never act on one workspace.
    maintenance: tokio::sync::Mutex<()>,
}

impl fmt::Debug for Workspaces {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workspaces").finish_non_exhaustive()
    }
}

impl Workspaces {
    /// A service over `store`, reaching environment plugins through `registry`, reading the time
    /// from `clock`.
    pub fn new(store: Store, registry: Arc<Registry>, clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                registry: Arc::downgrade(&registry),
                clock,
                locks: Mutex::default(),
                maintenance: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// Settles what the previous process left: every `creating` row (its creation was cut off)
    /// and every job run's `active` workspace (the run died with the process) becomes `deleting`,
    /// so owners can create again and the plugins are asked to delete what is left. Returns how
    /// many rows changed. Call it once at startup, before tools run.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Store`].
    pub async fn recover(&self) -> Result<u64, WorkspaceError> {
        let store = &self.inner.store;
        let now = self.inner.clock.now();
        Ok(store.workspaces_fail_creating(now, now).await?
            + store.workspaces_end_job_runs(now).await?)
    }

    /// The owner's workspace, if it has one (`creating` or `active`).
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Store`].
    pub async fn current(
        &self,
        owner: &WorkspaceOwner,
    ) -> Result<Option<WorkspaceRecord>, WorkspaceError> {
        Ok(self.inner.store.workspace_current(owner).await?)
    }

    /// The owner's most recently deleted workspace, if it has had one: when, why
    /// ([`WorkspaceRecord::deleted_at`], [`WorkspaceRecord::delete_reason`]), and the environment
    /// and options it had, so the model can create it again. Workspaces whose creation failed
    /// aren't included.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Store`].
    pub async fn last_deleted(
        &self,
        owner: &WorkspaceOwner,
    ) -> Result<Option<WorkspaceRecord>, WorkspaceError> {
        Ok(self.inner.store.workspace_last_deleted(owner).await?)
    }

    /// Creates a workspace for `owner` in the environment instance `environment`, with
    /// `options` (a JSON object matching the environment's schema; the plugin validates them),
    /// and returns it, `active`.
    ///
    /// The owner's slot is taken before the plugin is called and given back if creating fails.
    /// The plugin call runs to its end even if this future is dropped (the turn was cancelled),
    /// so the row never waits on a call nobody follows.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::AlreadyHasWorkspace`] naming the current one;
    /// [`UnknownEnvironment`](WorkspaceError::UnknownEnvironment) and
    /// [`Unreachable`](WorkspaceError::Unreachable) for an environment that isn't configured or
    /// connected; [`Refused`](WorkspaceError::Refused) for options that aren't an object or that
    /// the plugin refused (its `InvalidRequest`, worded for the model);
    /// [`Environment`](WorkspaceError::Environment) for any other plugin failure.
    pub async fn create(
        &self,
        owner: &WorkspaceOwner,
        environment: &str,
        options: Value,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        let (_, client) = self.reachable(environment)?;
        if !options.is_object() {
            return Err(WorkspaceError::Refused(
                "the options must be a JSON object ({} for none)".to_owned(),
            ));
        }
        let inner = &self.inner;
        let id = new_id();
        let inserted = inner
            .store
            .workspace_insert(NewWorkspace {
                id: id.clone(),
                environment: environment.to_owned(),
                owner: owner.clone(),
                options: options.clone(),
                now: inner.clock.now(),
            })
            .await;
        if let Err(giles_store::Error::Conflict(_)) = &inserted
            && let Some(current) = inner.store.workspace_current(owner).await?
        {
            return Err(WorkspaceError::AlreadyHasWorkspace {
                current: Box::new(current),
            });
        }
        inserted?;

        let service = self.clone();
        let owner = owner.clone();
        let task =
            tokio::spawn(async move { service.finish_create(owner, id, client, options).await });
        task.await.unwrap_or_else(|join| {
            Err(WorkspaceError::Environment(giles_types::Error::other(
                format!("creating the workspace failed: {join}"),
            )))
        })
    }

    /// The plugin call of [`create`](Self::create) and what follows it.
    async fn finish_create(
        self,
        owner: WorkspaceOwner,
        id: WorkspaceId,
        client: Arc<dyn Environment>,
        options: Value,
    ) -> Result<WorkspaceRecord, WorkspaceError> {
        let store = &self.inner.store;
        match client.create_workspace(&id, options).await {
            Ok(created) => {
                match store
                    .workspace_activate(
                        &id,
                        &created.description,
                        &created.working_directory,
                        &created.persistence,
                    )
                    .await
                {
                    Ok(record) => Ok(record),
                    // Deleted while it was being created: the delete found the row `creating`
                    // and marked it, and the sweep removes what the plugin made.
                    Err(giles_store::Error::Conflict(_)) => Err(WorkspaceError::NoWorkspace),
                    Err(error) => Err(error.into()),
                }
            }
            Err(error) => {
                tracing::warn!(workspace = %id, %owner, %error, "creating a workspace failed");
                // The plugin may have made some of it before failing: have it deleted.
                let now = self.inner.clock.now();
                if let Some(record) = store
                    .workspace_begin_delete(&id, DeleteReason::CreationFailed, now)
                    .await?
                {
                    self.delete_in_plugin(&record).await;
                }
                Err(WorkspaceError::from_plugin(error))
            }
        }
    }

    /// Deletes the owner's workspace, if it has one, and returns it as it is now: `Deleted`, or
    /// `Deleting` if the plugin couldn't be told (it is unreachable, or failed), in which case
    /// the sweep and the next connection finish the job. The owner's slot is free either way, at
    /// once. Leases don't matter here: the owner's own turn holds one while it deletes.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::Store`].
    pub async fn delete(
        &self,
        owner: &WorkspaceOwner,
        reason: DeleteReason,
    ) -> Result<Option<WorkspaceRecord>, WorkspaceError> {
        let now = self.inner.clock.now();
        let Some(mut record) = self
            .inner
            .store
            .workspace_begin_delete_for(owner, reason, now)
            .await?
        else {
            return Ok(None);
        };
        if self.delete_in_plugin(&record).await {
            record.state = WorkspaceState::Deleted;
        }
        Ok(Some(record))
    }

    /// Deletes the owner's workspace and creates another in the same environment with the same
    /// options (`/env reset`). Returns `None` if the owner has no workspace. The environment
    /// must be configured and connected, which is checked before anything is deleted, so a reset
    /// that can't succeed doesn't cost the files. Leases don't matter: a turn that is using the
    /// workspace finds the new one on its next call.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NotReady`] while the workspace is being created;
    /// [`UnknownEnvironment`](WorkspaceError::UnknownEnvironment) and
    /// [`Unreachable`](WorkspaceError::Unreachable) if its environment can't be used (nothing was
    /// deleted); [`Store`](WorkspaceError::Store). A failure of the new workspace's creation is in
    /// [`Recreation::created`], since the old one is gone by then.
    pub async fn recreate(
        &self,
        owner: &WorkspaceOwner,
        reason: DeleteReason,
    ) -> Result<Option<Recreation>, WorkspaceError> {
        let Some(current) = self.inner.store.workspace_current(owner).await? else {
            return Ok(None);
        };
        if current.state != WorkspaceState::Active {
            return Err(WorkspaceError::NotReady);
        }
        self.reachable(&current.environment)?;
        let Some(discarded) = self.delete(owner, reason).await? else {
            return Ok(None);
        };
        let created = self
            .create(owner, &current.environment, current.options)
            .await;
        Ok(Some(Recreation { discarded, created }))
    }

    /// Opens the owner's workspace for a call: takes the turn's lease on it (kept in `slot`
    /// until the turn ends), has the plugin start it if it is stopped or recreate what it runs
    /// on, and notes the use. Call it before every workspace tool call.
    ///
    /// # Errors
    ///
    /// [`WorkspaceError::NoWorkspace`] if the owner has none (see
    /// [`last_deleted`](Self::last_deleted)); [`NotReady`](WorkspaceError::NotReady) while it is
    /// being created; [`UnknownEnvironment`](WorkspaceError::UnknownEnvironment) and
    /// [`Unreachable`](WorkspaceError::Unreachable) if its environment was removed from the
    /// configuration or isn't connected; [`Gone`](WorkspaceError::Gone) if the environment
    /// doesn't have it any more; [`Environment`](WorkspaceError::Environment) for another plugin
    /// failure.
    pub async fn open(
        &self,
        owner: &WorkspaceOwner,
        slot: &LeaseSlot,
    ) -> Result<Opened, WorkspaceError> {
        let inner = &self.inner;
        let Some(record) = inner.store.workspace_current(owner).await? else {
            return Err(WorkspaceError::NoWorkspace);
        };
        if record.state != WorkspaceState::Active {
            return Err(WorkspaceError::NotReady);
        }
        let (_, client) = self.reachable(&record.environment)?;

        // Lease before starting, so a sweep that checked the workspace a moment ago can't stop
        // it under this call. The turn's earlier lease on this workspace is kept (taking the
        // shared lock again behind a waiting sweep would deadlock the turn).
        if slot.held().as_ref() != Some(&record.id) {
            let guard = self.lock_of(&record.id).read_owned().await;
            slot.set(Lease {
                id: record.id.clone(),
                _guard: guard,
            });
            // The sweep that made this call wait may have deleted the workspace.
            let still = inner.store.workspace(&record.id).await?;
            if still.is_none_or(|row| row.state != WorkspaceState::Active) {
                return Err(WorkspaceError::NoWorkspace);
            }
        }

        let outcome = match client.start_workspace(&record.id).await {
            Ok(outcome) => outcome,
            Err(giles_types::Error::NotFound(_)) => {
                let now = inner.clock.now();
                inner
                    .store
                    .workspace_mark_gone(&record.id, DeleteReason::RemovedOutsideGiles, now)
                    .await?;
                tracing::warn!(
                    workspace = %record.id,
                    environment = %record.environment,
                    "the environment no longer has a workspace; marked deleted"
                );
                return Err(WorkspaceError::Gone {
                    id: record.id,
                    environment: record.environment,
                });
            }
            Err(error) => return Err(WorkspaceError::from_plugin(error)),
        };
        let now = inner.clock.now();
        inner.store.workspace_touch(&record.id, now).await?;
        let mut record = record;
        record.last_used_at = record.last_used_at.max(now);
        Ok(Opened {
            record,
            client,
            notice: notice(&outcome),
        })
    }

    /// The configured, connected environment `name`.
    fn reachable(
        &self,
        name: &str,
    ) -> Result<(EnvironmentEntry, Arc<dyn Environment>), WorkspaceError> {
        let entry = self
            .registry()
            .and_then(|registry| registry.environment(name))
            .ok_or_else(|| WorkspaceError::UnknownEnvironment {
                name: name.to_owned(),
            })?;
        match entry.client.clone() {
            Some(client) => Ok((entry, client)),
            None => Err(WorkspaceError::Unreachable {
                environment: name.to_owned(),
            }),
        }
    }

    /// The registry, unless it is gone (the core is shutting down).
    fn registry(&self) -> Option<Arc<Registry>> {
        self.inner.registry.upgrade()
    }

    /// Asks the plugin to delete `record`'s workspace and, if it did, marks the row `deleted`.
    /// A plugin that is unreachable or fails leaves the row `deleting` for the sweep. Returns
    /// whether the row is `deleted` now.
    async fn delete_in_plugin(&self, record: &WorkspaceRecord) -> bool {
        let Some(client) = self
            .registry()
            .and_then(|registry| registry.environment(&record.environment))
            .and_then(|entry| entry.client)
        else {
            tracing::info!(
                workspace = %record.id,
                environment = %record.environment,
                "the environment is not reachable; the workspace is deleted when it is"
            );
            return false;
        };
        if let Err(error) = client.delete_workspace(&record.id).await {
            tracing::warn!(
                workspace = %record.id,
                environment = %record.environment,
                %error,
                "cannot delete the workspace; will retry"
            );
            return false;
        }
        match self.inner.store.workspace_finish_delete(&record.id).await {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(workspace = %record.id, %error, "cannot record the workspace's deletion");
                false
            }
        }
    }

    fn lock_of(&self, id: &WorkspaceId) -> Arc<RwLock<()>> {
        self.inner
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(id.clone())
            .or_default()
            .clone()
    }

    /// Takes the exclusive hold the sweep needs to stop or delete the workspace, or `None` if a
    /// turn holds a lease on it. Held until dropped; opening the workspace waits for it.
    fn try_claim(&self, id: &WorkspaceId) -> Option<OwnedRwLockWriteGuard<()>> {
        self.lock_of(id).try_write_owned().ok()
    }

    /// Drops the lock entries nobody holds.
    fn forget_idle_locks(&self) {
        self.inner
            .locks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|_, lock| Arc::strong_count(lock) > 1);
    }
}

/// What the model is told when the workspace had to be started or recreated.
fn notice(outcome: &StartOutcome) -> Option<String> {
    match outcome {
        StartOutcome::Started { detail } => Some(join(
            "The workspace was stopped and has been started again. Background processes from \
             before have ended.",
            detail,
        )),
        StartOutcome::Recreated { detail } => Some(join(
            "The workspace's environment was gone and has been recreated. Files under the \
             working directory survived.",
            detail,
        )),
        _ => None,
    }
}

fn join(what: &str, detail: &str) -> String {
    if detail.trim().is_empty() {
        what.to_owned()
    } else {
        format!("{what} {}", detail.trim())
    }
}

/// A fresh workspace id: `ws-` and 20 random hex digits.
fn new_id() -> WorkspaceId {
    let mut bytes = [0_u8; 10];
    getrandom::fill(&mut bytes).expect("the OS random number generator failed");
    let mut id = String::from("ws-");
    for byte in bytes {
        id.push_str(&format!("{byte:02x}"));
    }
    WorkspaceId::new(id).expect("lowercase hex is a valid workspace id")
}

/// How long `then` was before `now`; zero if it was later.
fn elapsed(now: Timestamp, then: Timestamp) -> std::time::Duration {
    let millis = now.as_millisecond().saturating_sub(then.as_millisecond());
    std::time::Duration::from_millis(u64::try_from(millis).unwrap_or(0))
}

/// `now` minus `by`, saturating at the earliest timestamp.
fn before(now: Timestamp, by: std::time::Duration) -> Timestamp {
    let millis = i64::try_from(by.as_millis()).unwrap_or(i64::MAX);
    Timestamp::from_millisecond(now.as_millisecond().saturating_sub(millis))
        .unwrap_or(Timestamp::MIN)
}
