//! Removing an agent while the core runs (PLAN sections 8.7 and 11.2).
//!
//! # What removal does
//!
//! [`AgentManager::remove`], under the manager's lock (shared with creation and editing):
//!
//! 1. **Refuses** while a stored route names the agent ([`RemoveError::Routed`], listing them for
//!    the page to link to `/admin/routes`), and while the agent is being worked on
//!    ([`RemoveError::Busy`]): a session of it has a turn running, a holder, or queued messages
//!    (the session manager), or one of its jobs is running, an isolated run's turn included (the
//!    scheduler; see [`AgentActivity`]). `/stop` the chats, wait, and try again. Nothing is
//!    cascaded: the owner decides what each route should do instead. The routes are held
//!    ([`Routes::hold_unrouted`](crate::routes::Routes::hold_unrouted)) until the agent is gone,
//!    so no route can be made for it in between.
//! 2. **Takes the agent out of the running configuration**, so it can't be chatted with (the chat
//!    chooser, the agent pages, and the router, which drops a chat for an agent that doesn't
//!    exist, no longer have it) and no turn or job run can start for it, and its config jobs stop
//!    (the scheduler follows the configuration). Only then does it **check for work again**: a
//!    chat turn or job run that began between the first check and the take-out is seen, and the
//!    agent is put back with [`LiveConfig::add_agent`](crate::config::LiveConfig::add_agent)
//!    (a new incarnation, and its config job tasks start afresh) and the removal refused. A
//!    scheduled job whose time came in that instant finds no agent and stops its task for good;
//!    that one job doesn't run again until the next start.
//! 3. **Moves the agent's directory aside**: `agents/<agent>/` is renamed to
//!    `trash/agents/<agent>-<UTC time>/` under the data root (`YYYYMMDDTHHMMSSZ`, with `-1`, `-2`
//!    on a collision), so its `agent.toml`, `SOUL.md`, memory, job prompt files, and skills can be
//!    put back by hand (move the directory back to `agents/<agent>/`; a restart loads it). A
//!    rename within the data root is atomic and keeps everything; nothing is deleted. If the
//!    rename fails the agent is put back as in step 2 and nothing else changed. A failing
//!    directory fsync after the rename is logged, not an error: the move happened.
//! 4. **Deletes the agent's runtime jobs** (rows of `jobs`, with the last-run times of every job
//!    of the agent) and tells the scheduler, so the jobs it made with the `schedule` tool stop
//!    and can't run again if the id is used again. If the database fails here the agent is
//!    already gone; the error is returned inside [`Removed`] and logged. The jobs then can't run
//!    (the scheduler skips a job whose agent isn't configured) and are deleted by removing the
//!    agent again after recreating it, or by hand.
//!
//! # What stays
//!
//! Sessions, messages, summaries, usage, and inbox rows stay in the database, inert: they name an
//! agent that isn't configured, so a chat can't be continued, and each queued message's turn
//! would fail with "agent is not configured". Creating an agent with the same id again
//! reattaches them (its old chats continue with the new agent, under the new agent's prompt), and
//! the id is free to create at once. Outbox rows expire on their own. Blobs the history still
//! references stay; the sweep handles the rest. The agent's per-user memory under
//! `agents/<agent>/users/` moves with the directory; `users/<user>/` isn't the agent's.
//! `trash/` isn't read by Giles and isn't in backups.

use std::fs::{self, DirBuilder, File};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use giles_store::RouteId;
use giles_types::AgentId;
use tracing::{error, info, warn};

use super::manager::AgentManager;
use crate::config::AgentConfig;
use crate::routes::{HoldError, RouteError, RouteView};
use crate::scheduler::Scheduler;
use crate::sessions::Sessions;

/// Tells whether an agent is being worked on. The daemon's is the session manager.
#[async_trait]
pub trait AgentActivity: Send + Sync + std::fmt::Debug {
    /// Whether a turn is running for `agent`, or messages are queued for it.
    async fn is_busy(&self, agent: &AgentId) -> bool;
}

/// The daemon's [`AgentActivity`]: an agent is busy while the session manager has work for it (a
/// chat turn, a hold, queued messages) or the scheduler has a job run of it going.
#[derive(Debug, Clone)]
pub struct DaemonActivity {
    /// The session manager.
    pub sessions: Sessions,
    /// The scheduler.
    pub scheduler: Scheduler,
}

#[async_trait]
impl AgentActivity for DaemonActivity {
    async fn is_busy(&self, agent: &AgentId) -> bool {
        self.scheduler.agent_busy(agent) || self.sessions.agent_busy(agent).await
    }
}

/// A route that names an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRef {
    /// The route's stable id, which the routes page anchors on (`/admin/routes#route-<id>`).
    pub id: RouteId,
    /// The 1-based position in the route list.
    pub position: usize,
    /// What the route matches, in words.
    pub criteria: String,
}

impl From<RouteView> for RouteRef {
    fn from(view: RouteView) -> Self {
        Self {
            id: view.id,
            position: view.position,
            criteria: view.route.criteria(),
        }
    }
}

/// What removing an agent would do now, for the confirmation page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalPreview {
    /// The agent.
    pub id: AgentId,
    /// The routes that name it; while there are any, removal is refused.
    pub routes: Vec<RouteRef>,
    /// How many runtime jobs (made with the `schedule` tool) would be deleted.
    pub runtime_jobs: usize,
    /// How many `[[jobs]]` of its `agent.toml` would stop (they're in the files kept).
    pub config_jobs: usize,
    /// Whether it is being worked on now; while it is, removal is refused.
    pub busy: bool,
}

impl RemovalPreview {
    /// Whether removal would be refused now.
    pub fn is_blocked(&self) -> bool {
        self.busy || !self.routes.is_empty()
    }
}

/// What removing an agent did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// Where the agent's directory is now.
    pub trash: PathBuf,
    /// How many runtime jobs were deleted.
    pub runtime_jobs: usize,
    /// How many `[[jobs]]` of its `agent.toml` stopped.
    pub config_jobs: usize,
    /// Why its runtime jobs couldn't be deleted, if the database failed.
    pub jobs_error: Option<String>,
}

/// Why an agent wasn't removed. Nothing changed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RemoveError {
    /// No such agent is configured.
    #[error("there is no agent `{0}`")]
    NotFound(String),
    /// Routes name the agent.
    #[error("{} conversations to this agent", routes_phrase(.0))]
    Routed(Vec<RouteRef>),
    /// The agent is being worked on.
    #[error(
        "the agent is working on a chat or a scheduled job now; stop it or wait until it's done"
    )]
    Busy,
    /// Moving the directory failed.
    #[error("cannot move {path} to the trash: {source}")]
    Io {
        /// What was being moved.
        path: String,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// The routes couldn't be read.
    #[error("cannot read the routes: {0}")]
    Routes(#[from] RouteError),
    /// The database failed.
    #[error("cannot read the agent's jobs: {0}")]
    Store(#[from] giles_store::Error),
    /// The blocking task moving the directory failed.
    #[error("removing the agent failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

fn routes_phrase(routes: &[RouteRef]) -> String {
    match routes {
        [_] => "1 route sends".to_owned(),
        routes => format!("{} routes send", routes.len()),
    }
}

impl RemoveError {
    /// Whether it's the owner's to fix (else it's the server's problem).
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::NotFound(_) | Self::Routed(_) | Self::Busy)
    }
}

impl AgentManager {
    /// What removing agent `id` would do, for the confirmation page.
    ///
    /// # Errors
    ///
    /// [`RemoveError::NotFound`], or [`RemoveError::Routes`] or [`RemoveError::Store`] if what
    /// the page lists can't be read.
    pub async fn removal_preview(&self, id: &str) -> Result<RemovalPreview, RemoveError> {
        let agent = self.configured_agent(id)?;
        let routes = self
            .routes
            .view()
            .await?
            .into_iter()
            .filter(|view| view.route.agent == agent.id)
            .map(RouteRef::from)
            .collect();
        let runtime_jobs = self.store.jobs(Some(agent.id.clone()), None).await?.len();
        Ok(RemovalPreview {
            busy: self.activity.is_busy(&agent.id).await,
            routes,
            runtime_jobs,
            config_jobs: agent.jobs.len(),
            id: agent.id,
        })
    }

    /// Removes agent `id` (see the [module docs](self)).
    ///
    /// # Errors
    ///
    /// A [`RemoveError`]; the files, the configuration, and the jobs are as they were.
    pub async fn remove(&self, id: &str) -> Result<Removed, RemoveError> {
        let _one_at_a_time = self.lock.lock().await;
        let agent = self.configured_agent(id)?;
        if self.activity.is_busy(&agent.id).await {
            return Err(RemoveError::Busy);
        }
        let hold = match self.routes.hold_unrouted(&agent.id).await {
            Ok(hold) => hold,
            Err(HoldError::Routed(routes)) => {
                return Err(RemoveError::Routed(
                    routes.into_iter().map(RouteRef::from).collect(),
                ));
            }
            Err(HoldError::Routes(error)) => return Err(RemoveError::Routes(error)),
        };
        // Out of the running configuration first, so nothing new can start for the agent; then
        // look for work again. What began before is seen now; what begins after finds no agent.
        let live = self.registry.live_config();
        let taken_out = live
            .remove_agent(&agent.id)
            .unwrap_or_else(|| agent.clone());
        let put_back = |taken_out: AgentConfig| {
            if let Err(error) = live.add_agent(taken_out) {
                error!(agent = %agent.id, %error, "cannot put the agent back");
            }
        };
        if self.activity.is_busy(&agent.id).await {
            put_back(taken_out);
            return Err(RemoveError::Busy);
        }
        let from = agent.dir.clone();
        let trash = self.registry.root().trash_dir().join("agents");
        let name = agent.id.to_string();
        let stamp = jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%SZ")
            .to_string();
        let moved =
            tokio::task::spawn_blocking(move || move_aside(&from, &trash, &name, &stamp)).await;
        let trashed = match moved {
            Ok(Ok(trashed)) => trashed,
            Ok(Err((path, source))) => {
                put_back(taken_out);
                return Err(RemoveError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
            Err(error) => {
                put_back(taken_out);
                return Err(error.into());
            }
        };
        drop(hold);
        info!(agent = %agent.id, trash = %trashed.display(), "agent removed");

        let (runtime_jobs, jobs_error) = match self.store.agent_jobs_delete(agent.id.clone()).await
        {
            Ok(deleted) => (deleted, None),
            Err(error) => {
                error!(agent = %agent.id, %error, "cannot delete the removed agent's runtime jobs");
                (0, Some(error.to_string()))
            }
        };
        self.jobs.notify();
        Ok(Removed {
            trash: trashed,
            runtime_jobs,
            config_jobs: agent.jobs.len(),
            jobs_error,
        })
    }
}

/// Renames the agent directory `from` to `trash/<name>-<stamp>` (`-1`, `-2`, ... after the stamp if
/// that exists), creating `trash` (mode 0700, and its parent), and fsyncs the directories
/// changed (a failure there is logged). Returns the new place, or the path that failed and why.
fn move_aside(
    from: &Path,
    trash: &Path,
    name: &str,
    stamp: &str,
) -> Result<PathBuf, (PathBuf, io::Error)> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(trash)
        .map_err(|e| (trash.to_owned(), e))?;
    let mut attempt = 0_u32;
    let to = loop {
        let candidate = match attempt {
            0 => trash.join(format!("{name}-{stamp}")),
            n => trash.join(format!("{name}-{stamp}-{n}")),
        };
        attempt += 1;
        if fs::symlink_metadata(&candidate).is_ok() {
            continue;
        }
        match fs::rename(from, &candidate) {
            Ok(()) => break candidate,
            // Appeared since the check (or is a non-empty directory): another name.
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) => {}
            Err(e) => return Err((from.to_owned(), e)),
        }
    };
    // The move happened; a directory that can't be fsynced is worth a log line, not an error that
    // would say nothing changed.
    for dir in [Some(trash), from.parent()].into_iter().flatten() {
        if let Err(error) = File::open(dir).and_then(|d| d.sync_all()) {
            warn!(dir = %dir.display(), %error, "cannot fsync a directory after moving an agent to the trash");
        }
    }
    Ok(to)
}

/// Shared by [`AgentManager::removal_preview`] and [`AgentManager::remove`].
impl AgentManager {
    fn configured_agent(&self, id: &str) -> Result<AgentConfig, RemoveError> {
        AgentId::new(id)
            .ok()
            .and_then(|agent| self.registry.config().agents.get(&agent).cloned())
            .ok_or_else(|| RemoveError::NotFound(id.to_owned()))
    }
}

/// What a test double of [`AgentActivity`] says.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeActivity {
    /// The agents that are busy.
    pub busy: std::sync::Mutex<std::collections::HashSet<AgentId>>,
}

#[cfg(test)]
#[async_trait]
impl AgentActivity for FakeActivity {
    async fn is_busy(&self, agent: &AgentId) -> bool {
        self.busy.lock().unwrap().contains(agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_is_moved_to_a_free_name_in_a_private_trash() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("agents");
        let trash = root.path().join("trash/agents");
        let make = |name: &str| {
            let dir = agents.join(name);
            fs::create_dir_all(dir.join("users/owner")).unwrap();
            fs::write(dir.join("SOUL.md"), name).unwrap();
            fs::write(dir.join("users/owner/MEMORY.md"), "likes tea").unwrap();
            dir
        };

        let first = make("jeeves");
        let moved = move_aside(&first, &trash, "jeeves", "20261024T120000Z").unwrap();
        assert_eq!(moved, trash.join("jeeves-20261024T120000Z"));
        assert!(!first.exists());
        assert_eq!(fs::read_to_string(moved.join("SOUL.md")).unwrap(), "jeeves");
        assert_eq!(
            fs::read_to_string(moved.join("users/owner/MEMORY.md")).unwrap(),
            "likes tea"
        );
        use std::os::unix::fs::PermissionsExt;
        for dir in [root.path().join("trash"), trash.clone()] {
            let mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }

        // The same id removed again in the same second: another name, nothing overwritten.
        let second = make("jeeves");
        fs::write(second.join("SOUL.md"), "second").unwrap();
        let again = move_aside(&second, &trash, "jeeves", "20261024T120000Z").unwrap();
        assert_eq!(again, trash.join("jeeves-20261024T120000Z-1"));
        let third = make("jeeves");
        let more = move_aside(&third, &trash, "jeeves", "20261024T120000Z").unwrap();
        assert_eq!(more, trash.join("jeeves-20261024T120000Z-2"));
        assert_eq!(fs::read_to_string(moved.join("SOUL.md")).unwrap(), "jeeves");
        assert_eq!(fs::read_to_string(again.join("SOUL.md")).unwrap(), "second");

        // A directory that isn't there is an error naming it, and nothing is made up.
        let (path, _) =
            move_aside(&agents.join("ghost"), &trash, "ghost", "20261024T120000Z").unwrap_err();
        assert_eq!(path, agents.join("ghost"));
        assert!(!trash.join("ghost-20261024T120000Z").exists());
    }

    #[test]
    fn the_routes_phrase_counts() {
        let route = |position| RouteRef {
            id: RouteId::new(i64::try_from(position).unwrap()),
            position,
            criteria: "any".into(),
        };
        assert_eq!(
            RemoveError::Routed(vec![route(1)]).to_string(),
            "1 route sends conversations to this agent"
        );
        assert_eq!(
            RemoveError::Routed(vec![route(1), route(3)]).to_string(),
            "2 routes send conversations to this agent"
        );
    }
}
