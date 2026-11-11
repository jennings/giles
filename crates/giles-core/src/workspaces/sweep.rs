//! The workspace sweep and the reconciliation of the `workspaces` table with what the
//! environment plugins hold (PLAN section 8.8).
//!
//! The sweep ([`Workspaces::sweep_once`]) runs every few minutes ([`WorkspaceSweep`]). For each
//! environment instance it:
//!
//! 1. prunes `deleted` rows older than the instance's `delete_after`;
//! 2. if the plugin is connected, reconciles (below), then looks at each `active` workspace no
//!    turn has leased: it **deletes** one whose owner was inactive for `delete_after` (a
//!    conversation's newest message counts, as does the workspace's last use), and otherwise
//!    **stops** one the plugin lists as running that was last used more than `stop_after` ago.
//!
//! Reconciliation ([`Workspaces::reconcile`]) also runs whenever a plugin connects. It takes a
//! snapshot of the rows, then asks the plugin for its workspaces, and:
//!
//! - deletes workspaces the plugin has that no row names, once they are older than
//!   [`RECONCILE_GRACE`] (a create in flight has a row, but a create that failed halfway, or a
//!   restored backup, doesn't);
//! - finishes `deleting` rows: the plugin is told to delete, then the row is `deleted`;
//! - marks `active` rows the plugin doesn't have `deleted` ([`DeleteReason::RemovedOutsideGiles`]),
//!   so the model is told;
//! - gives up on `creating` rows older than the grace period (they become `deleting`).
//!
//! Workspaces a turn has leased are never stopped or deleted by either. Docs/technical/environments.md
//! has the whole picture.

use std::collections::HashSet;
use std::time::Duration;

use giles_types::{Environment, Workspace, WorkspaceId, WorkspaceState as PluginState};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::{DeleteReason, WorkspaceState, Workspaces};
use crate::blobs::SweepSchedule;
use crate::config::InstanceName;
use crate::registry::EnvironmentEntry;

/// How long after startup the first sweep runs: late enough for plugins to connect and for
/// startup recovery to finish.
pub const SWEEP_FIRST_DELAY: Duration = Duration::from_secs(60);

/// The time between sweeps, from the end of one to the start of the next. A workspace is stopped
/// or deleted at most this much later than its `stop_after` or `delete_after`.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How old a workspace the plugin has but no row names, or a `creating` row, must be before
/// reconciliation gives up on it. A create in flight is younger.
pub const RECONCILE_GRACE: Duration = Duration::from_secs(60 * 60);

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// What a sweep or a reconciliation did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Workspaces stopped for being idle.
    pub stopped: u64,
    /// Workspaces deleted because their owner was inactive.
    pub deleted_inactive: u64,
    /// `deleting` rows the plugin confirmed.
    pub cleaned: u64,
    /// `active` rows the plugin no longer had, now `deleted`.
    pub gone: u64,
    /// Workspaces the plugin had that no row named, deleted.
    pub unknown_deleted: u64,
    /// `creating` rows older than the grace period, given up on.
    pub stale_creating: u64,
    /// `deleted` rows removed after `delete_after`.
    pub pruned: u64,
}

impl SweepReport {
    fn total(&self) -> u64 {
        let Self {
            stopped,
            deleted_inactive,
            cleaned,
            gone,
            unknown_deleted,
            stale_creating,
            pruned,
        } = *self;
        stopped + deleted_inactive + cleaned + gone + unknown_deleted + stale_creating + pruned
    }

    fn add(&mut self, other: Self) {
        self.stopped += other.stopped;
        self.deleted_inactive += other.deleted_inactive;
        self.cleaned += other.cleaned;
        self.gone += other.gone;
        self.unknown_deleted += other.unknown_deleted;
        self.stale_creating += other.stale_creating;
        self.pruned += other.pruned;
    }
}

/// The background tasks of the workspace sweep: the periodic sweep, and the reconciliation on
/// each plugin connection.
#[derive(Debug)]
pub struct WorkspaceSweep {
    tasks: TaskTracker,
}

impl WorkspaceSweep {
    /// Waits until the tasks have stopped. Call it after cancelling the shutdown token. A sweep
    /// in progress is dropped where it is: every step is idempotent, and the next run, or the
    /// next start, finishes it.
    pub async fn closed(&self) {
        self.tasks.wait().await;
    }
}

impl Workspaces {
    /// Starts the periodic sweep on `schedule` ([`SWEEP_FIRST_DELAY`] and [`SWEEP_INTERVAL`] in
    /// the daemon) and the reconciliation on every plugin connection, until `shutdown` is
    /// cancelled. Stop it before the plugins: it calls them.
    pub fn start_sweep(
        &self,
        schedule: SweepSchedule,
        shutdown: CancellationToken,
    ) -> WorkspaceSweep {
        let tasks = TaskTracker::new();
        let service = self.clone();
        let token = shutdown.clone();
        tasks.spawn(async move {
            let mut wait = schedule.first_delay;
            loop {
                tokio::select! {
                    biased;
                    () = token.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                tokio::select! {
                    biased;
                    () = token.cancelled() => return,
                    report = service.sweep_once() => log_report(report),
                }
                wait = schedule.interval;
            }
        });
        let service = self.clone();
        tasks.spawn(async move { service.reconcile_on_connect(shutdown).await });
        tasks.close();
        WorkspaceSweep { tasks }
    }

    /// Runs one sweep over every configured environment instance now. A step that fails is
    /// logged and the sweep goes on; what failed is tried again at the next sweep.
    pub async fn sweep_once(&self) -> SweepReport {
        let inner = &self.inner;
        let _serial = inner.maintenance.lock().await;
        let now = inner.clock.now();
        let mut total = SweepReport::default();
        let Some(registry) = self.registry() else {
            return total;
        };
        for entry in registry.environments() {
            let mut report = SweepReport::default();
            match inner
                .store
                .workspaces_prune(entry.name.as_str(), super::before(now, entry.delete_after))
                .await
            {
                Ok(pruned) => report.pruned = pruned,
                Err(error) => {
                    tracing::warn!(environment = %entry.name, %error, "cannot prune deleted workspaces");
                }
            }
            if let Some(client) = entry.client.clone()
                && let Err(error) = self.sweep_environment(&entry, &client, &mut report).await
            {
                tracing::warn!(environment = %entry.name, %error, "workspace sweep failed");
            }
            total.add(report);
        }
        self.forget_idle_locks();
        total
    }

    /// Reconciles the rows of environment instance `environment` with the workspaces its plugin
    /// holds, as is done on every connection (see the [module](self) documentation).
    ///
    /// # Errors
    ///
    /// The plugin's or the database's error; nothing is retried here.
    pub async fn reconcile(&self, environment: &str) -> Result<SweepReport, BoxError> {
        let _serial = self.inner.maintenance.lock().await;
        let entry = self
            .registry()
            .and_then(|registry| registry.environment(environment))
            .ok_or_else(|| format!("no environment named {environment:?}"))?;
        let client = entry
            .client
            .clone()
            .ok_or_else(|| format!("the environment {environment:?} is not reachable"))?;
        let mut report = SweepReport::default();
        self.reconcile_environment(&entry, &client, &mut report)
            .await?;
        Ok(report)
    }

    /// Reconciles each environment as its plugin connects (or reconnects).
    async fn reconcile_on_connect(self, shutdown: CancellationToken) {
        let Some(registry) = self.registry() else {
            return;
        };
        let mut changes = registry.environment_changes();
        drop(registry);
        let mut seen: std::collections::HashMap<InstanceName, u64> =
            std::collections::HashMap::new();
        loop {
            let Some(registry) = self.registry() else {
                return;
            };
            let entries = registry.environments();
            drop(registry);
            for entry in entries {
                let Some(client) = entry.client.clone() else {
                    continue;
                };
                if seen.get(&entry.name) == Some(&entry.connections) {
                    continue;
                }
                seen.insert(entry.name.clone(), entry.connections);
                let work = async {
                    let _serial = self.inner.maintenance.lock().await;
                    let mut report = SweepReport::default();
                    let result = self
                        .reconcile_environment(&entry, &client, &mut report)
                        .await;
                    (report, result)
                };
                let (report, result) = tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    done = work => done,
                };
                match result {
                    Ok(_) if report.total() == 0 => {
                        tracing::debug!(environment = %entry.name, "workspaces reconciled; nothing to change");
                    }
                    Ok(_) => tracing::info!(
                        environment = %entry.name,
                        ?report,
                        "workspaces reconciled with the environment"
                    ),
                    Err(error) => {
                        tracing::warn!(environment = %entry.name, %error, "cannot reconcile workspaces");
                    }
                }
            }
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                changed = changes.changed() => if changed.is_err() { return },
            }
        }
    }

    /// Reconciles, then deletes or stops the idle workspaces of one connected environment.
    async fn sweep_environment(
        &self,
        entry: &EnvironmentEntry,
        client: &std::sync::Arc<dyn Environment>,
        report: &mut SweepReport,
    ) -> Result<(), BoxError> {
        let inner = &self.inner;
        let listed = self.reconcile_environment(entry, client, report).await?;
        let running: HashSet<&WorkspaceId> = listed
            .iter()
            .filter(|workspace| workspace.state == PluginState::Running)
            .map(|workspace| &workspace.id)
            .collect();
        let now = inner.clock.now();
        for row in inner.store.workspaces_of(entry.name.as_str()).await? {
            if row.state != WorkspaceState::Active {
                continue;
            }
            // A turn using it, or about to: leave it be.
            let Some(_claim) = self.try_claim(&row.id) else {
                continue;
            };
            // What the snapshot said may be stale: a call touched it, or it was deleted.
            let Some(row) = inner
                .store
                .workspace(&row.id)
                .await?
                .filter(|row| row.state == WorkspaceState::Active)
            else {
                continue;
            };
            let activity = match inner.store.workspace_owner_activity(&row.owner).await? {
                Some(at) => at.max(row.last_used_at),
                None => row.last_used_at,
            };
            if super::elapsed(now, activity) > entry.delete_after {
                if let Some(record) = inner
                    .store
                    .workspace_begin_delete(&row.id, DeleteReason::Inactive, now)
                    .await?
                {
                    tracing::info!(
                        workspace = %record.id,
                        environment = %entry.name,
                        owner = %record.owner,
                        "deleting a workspace whose owner has been inactive"
                    );
                    report.deleted_inactive += 1;
                    self.delete_in_plugin(&record).await;
                }
            } else if super::elapsed(now, row.last_used_at) > entry.stop_after
                && running.contains(&row.id)
            {
                match client.stop_workspace(&row.id).await {
                    Ok(()) => {
                        tracing::info!(workspace = %row.id, environment = %entry.name, "stopped an idle workspace");
                        report.stopped += 1;
                    }
                    Err(error) => {
                        tracing::warn!(workspace = %row.id, environment = %entry.name, %error, "cannot stop an idle workspace");
                    }
                }
            }
        }
        Ok(())
    }

    /// The reconciliation of the module documentation. Returns the plugin's workspaces.
    async fn reconcile_environment(
        &self,
        entry: &EnvironmentEntry,
        client: &std::sync::Arc<dyn Environment>,
        report: &mut SweepReport,
    ) -> Result<Vec<Workspace>, BoxError> {
        let inner = &self.inner;
        let now = inner.clock.now();
        report.stale_creating += inner
            .store
            .workspaces_fail_creating(super::before(now, RECONCILE_GRACE), now)
            .await?;
        // The rows come first: a row that is `active` in this snapshot was `active` before the
        // plugin is asked, so the plugin not having it means it is gone. Asked first, a workspace
        // created in between would look removed.
        let rows = inner.store.workspaces_of(entry.name.as_str()).await?;
        let listed = client.list_workspaces().await?.workspaces;

        let known: HashSet<&WorkspaceId> = rows.iter().map(|row| &row.id).collect();
        for workspace in &listed {
            if known.contains(&workspace.id)
                || super::elapsed(now, workspace.created_at) <= RECONCILE_GRACE
            {
                continue;
            }
            match client.delete_workspace(&workspace.id).await {
                Ok(()) => {
                    tracing::info!(workspace = %workspace.id, environment = %entry.name, "deleted a workspace Giles has no record of");
                    report.unknown_deleted += 1;
                }
                Err(error) => {
                    tracing::warn!(workspace = %workspace.id, environment = %entry.name, %error, "cannot delete a workspace Giles has no record of");
                }
            }
        }

        let held: HashSet<&WorkspaceId> = listed.iter().map(|workspace| &workspace.id).collect();
        for row in &rows {
            match row.state {
                WorkspaceState::Deleting => {
                    // Leased: the turn that deleted it may still be running commands in it.
                    let Some(_claim) = self.try_claim(&row.id) else {
                        continue;
                    };
                    if self.delete_in_plugin(row).await {
                        report.cleaned += 1;
                    }
                }
                WorkspaceState::Active => {
                    let lost = !held.contains(&row.id)
                        && inner
                            .store
                            .workspace_mark_gone(&row.id, DeleteReason::RemovedOutsideGiles, now)
                            .await?;
                    if lost {
                        tracing::warn!(
                            workspace = %row.id,
                            environment = %entry.name,
                            "the environment no longer has the workspace; marked deleted"
                        );
                        report.gone += 1;
                    }
                }
                _ => {}
            }
        }
        Ok(listed)
    }
}

fn log_report(report: SweepReport) {
    if report.total() == 0 {
        tracing::debug!("workspace sweep: nothing to do");
    } else {
        tracing::info!(?report, "workspace sweep");
    }
}
