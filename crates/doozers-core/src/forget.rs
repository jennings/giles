//! Forgetting a user (PLAN section 11.4): the operation behind **Remove** on the user pages of the
//! web UI. Design: `Docs/technical/storage.md#forgetting-a-user`.
//!
//! [`Forgetter::forget`] deletes everything Doozers keeps about a user, then the user. The order
//! is chosen so that a crash or a failure at any step leaves a user who is disabled, so unusable,
//! and who can be removed again: every step does nothing when its work is already done.
//!
//! 1. **Refuses** the admin's own removal ([`ForgetError::Yourself`]), and while the user is being
//!    worked for ([`ForgetError::Busy`]): a turn they asked for is running, a session of theirs
//!    has a turn running, a holder (a job's turn or reply), or queued messages, or the same is
//!    found in the database ([`UserActivity`]: the session manager, and the scheduler for a run of
//!    a job they made, an isolated run's turn included). Doozers does not stop the
//!    turn: the page tells the admin to have the user `/stop` it, or to wait, which is also what
//!    removing an agent or deleting a chat does. Nothing has changed when it is refused.
//! 2. **Disables** the user ([`Users::set_state`]), which is where the last-active-admin guard
//!    applies ([`ForgetError::LastAdmin`], nothing changed). A disabled user can't sign in and
//!    their messages are dropped, so no new work starts. Work that began before the change took
//!    effect is looked for once more; if it is found, the user is made active again if they were
//!    and the removal is refused.
//! 3. **Deletes their rows** in one transaction ([`Store::user_data_forget`]): direct and job
//!    sessions with their summaries, inbox and outbox rows, runtime jobs, web sessions, link codes,
//!    invites, password, passkeys, and OpenID Connect connections, SSH key rows; in group sessions
//!    their messages become tombstones and the summaries covering them are deleted for
//!    compaction to write again; usage rows keep their token counts without the user. Then it
//!    **denies their pending approvals** ([`ApprovalBoard::forget_user`]) and **tells the
//!    scheduler** their jobs are gone, which stops them.
//! 4. **Deletes their workspaces** through the environments' plugins
//!    ([`Workspaces::delete`]: a direct conversation's and a job run's), then removes the rows
//!    that name them. A plugin that can't be reached leaves the workspace `deleting` for the
//!    sweep, and the row, now without the user, stays until then.
//! 5. **Deletes their secrets**, everything under `users/<user>/` in the secret store (their SSH
//!    keys among them).
//! 6. **Deletes their private memories** in every agent's `memory.db`. A shared memory they wrote
//!    stays for everyone, without an author.
//! 7. **Removes `users/<user>/`** (`USER.md` and anything else in it).
//! 8. **Deletes the user** ([`Users::remove`]), whose row cascades to their identities.
//! 9. **Writes the summaries of group sessions again** ([`GroupSummaries`]). Summaries are
//!    cumulative, so deleting the ones that covered the user's messages (step 3) dropped a long
//!    group's whole compacted history, which may not fit a model's context. The agent loop
//!    compacts those sessions as a turn would, with the agent's compaction model, from the
//!    tombstoned messages. It starts right after step 3, because only that call knows which
//!    sessions lost summaries, and runs in the background: the admin doesn't wait for the model,
//!    and the removal's lock is released. It is best effort: a provider that can't be reached is
//!    logged, and the session's next turn compacts what is left.
//!
//! # What stays
//!
//! Blobs that only deleted messages referenced are left to the blob sweep, which finds them by
//! scanning the tables for references, so a message that is gone no longer keeps one. The
//! `trash/agents/` directories of agents removed earlier are not read by Doozers and are not
//! touched. An in-flight environment command or MCP connection of the user's, if one is left
//! after their turn ended, ends when it does.

use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use doozers_store::{
    DeleteReason, SessionId, Store, UserData, UserDataDeletion, UserState, WorkspaceOwner,
    WorkspaceRecord,
};
use doozers_types::{AgentId, ConversationId, UserId};
use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;
use tracing::{info, warn};

use crate::agent_loop::AgentLoop;
use crate::approvals::ApprovalBoard;
use crate::memory_dbs::MemoryDbs;
use crate::paths::DataRoot;
use crate::scheduler::{JobChanges, Scheduler};
use crate::secrets::{SecretError, SecretPath, SecretProvider};
use crate::sessions::{Clock, Sessions};
use crate::users::{User, UserError, Users};
use crate::workspaces::{WorkspaceError, Workspaces};

#[cfg(test)]
mod tests;

/// Tells whether a user is being worked for. The daemon's is the session manager.
#[async_trait]
pub trait UserActivity: Send + Sync + std::fmt::Debug {
    /// Whether a turn is running for `user`, in a session of theirs or at their request, or
    /// messages of theirs are queued.
    async fn is_busy(&self, user: &UserId) -> bool;
}

/// The daemon's [`UserActivity`]: a user is busy while the session manager has work for them (a
/// turn, a hold, queued messages) or the scheduler has a run of a job they made going, an
/// isolated run's turn included, which the session manager doesn't know.
#[derive(Debug, Clone)]
pub struct DaemonUserActivity {
    /// The session manager.
    pub sessions: Sessions,
    /// The scheduler.
    pub scheduler: Scheduler,
}

#[async_trait]
impl UserActivity for DaemonUserActivity {
    async fn is_busy(&self, user: &UserId) -> bool {
        self.scheduler.user_busy(user) || self.sessions.user_busy(user).await
    }
}

/// Writes the summaries of a group session again. The daemon's is the agent loop, which compacts
/// as a turn does.
#[async_trait]
pub trait GroupSummaries: Send + Sync {
    /// Compacts group session `session`, whose summaries were deleted, until its history fits the
    /// model's context again. Failures are the implementation's to log: the next turn compacts
    /// what is left.
    async fn regenerate(&self, session: SessionId);
}

#[async_trait]
impl GroupSummaries for AgentLoop {
    async fn regenerate(&self, session: SessionId) {
        self.recompact_session(session).await;
    }
}

/// What [`Forgetter::new`] works with.
#[derive(Clone)]
pub struct ForgetOptions {
    /// The database.
    pub store: Store,
    /// The users: disabled first and removed last.
    pub users: Arc<Users>,
    /// The data root: the agents' `memory.db` files and `users/<user>/`.
    pub root: DataRoot,
    /// The agents' memory databases.
    pub memory_dbs: MemoryDbs,
    /// The secret store, from which everything under `users/<user>/` is deleted.
    pub secrets: Arc<dyn SecretProvider>,
    /// The workspaces, deleted through their environments.
    pub workspaces: Workspaces,
    /// The pending approvals, which are denied.
    pub approvals: Arc<ApprovalBoard>,
    /// Tells the scheduler that runtime jobs were deleted.
    pub jobs: JobChanges,
    /// Whether a user is being worked for, which keeps them from being removed.
    pub activity: Arc<dyn UserActivity>,
    /// Writes the summaries of group sessions again.
    pub summaries: Arc<dyn GroupSummaries>,
    /// The time source.
    pub clock: Arc<dyn Clock>,
}

/// Forgets users. Cheap to clone; the clones share one lock, so only one removal runs at a time.
#[derive(Clone)]
pub struct Forgetter {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Forgetter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forgetter").finish_non_exhaustive()
    }
}

struct Inner {
    options: ForgetOptions,
    /// The summaries being written again in the background.
    regenerating: TaskTracker,
    /// Held across a whole removal and its preview's reads of what a removal changes.
    one_at_a_time: Mutex<()>,
}

/// Why a removal is refused, which the confirmation page says before the admin presses the
/// button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The user is the admin asking.
    Yourself,
    /// The user is the last active admin.
    LastAdmin,
}

impl Refusal {
    /// The reason, in a sentence.
    pub fn message(self) -> &'static str {
        match self {
            Self::Yourself => {
                "You can't remove yourself. Ask another admin to remove your account."
            }
            Self::LastAdmin => {
                "This is the last active admin. Make another user an admin first: the last active \
                 admin can't be removed."
            }
        }
    }
}

/// The private memories of a user in one agent's `memory.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMemories {
    /// The agent.
    pub agent: AgentId,
    /// How many private memories the user has with it.
    pub memories: u64,
}

/// What removing a user would delete now, for the confirmation page.
#[derive(Debug, Clone, PartialEq)]
pub struct ForgetPreview {
    /// The user.
    pub user: User,
    /// What they have in the database.
    pub data: UserData,
    /// Their private memories, in each agent that has some.
    pub memories: Vec<AgentMemories>,
    /// How many secrets are in their space of the secret store, SSH keys included.
    pub secrets: usize,
    /// Whether `users/<user>/` exists.
    pub profile: bool,
    /// The workspaces that would be deleted, with the environments that hold them.
    pub workspaces: Vec<WorkspaceRecord>,
    /// How many approvals wait for their answer.
    pub approvals: usize,
    /// Whether they are being worked for now. While they are, removal is refused.
    pub busy: bool,
    /// Why removal would be refused, if it would be.
    pub refusal: Option<Refusal>,
}

impl ForgetPreview {
    /// Whether removal would be refused now.
    pub fn is_blocked(&self) -> bool {
        self.busy || self.refusal.is_some()
    }
}

/// What forgetting a user did.
#[derive(Debug, Clone, PartialEq)]
pub struct Forgotten {
    /// The user as they were.
    pub user: User,
    /// The rows changed in the database.
    pub data: UserData,
    /// The conversations of their deleted direct sessions, as (channel instance, conversation id):
    /// what a channel keeps live for one (the web channel's chat state) is the channel's to drop.
    pub conversations: Vec<(String, ConversationId)>,
    /// Private memories deleted.
    pub memories: u64,
    /// Shared memories that stay, now without an author.
    pub shared_memories: u64,
    /// Secrets deleted.
    pub secrets: usize,
    /// Live workspaces deleted.
    pub workspaces: usize,
    /// Pending approvals denied.
    pub approvals: usize,
    /// Whether `users/<user>/` was removed.
    pub profile: bool,
    /// The group sessions whose summaries were written again, as far as the model allowed.
    pub group_sessions: usize,
}

/// Why a user wasn't forgotten (or not completely: a failure after the first change leaves the
/// user disabled, and removing them again finishes the job).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ForgetError {
    /// There is no such user.
    #[error("there is no user `{0}`")]
    NoSuchUser(UserId),
    /// The admin asked to remove themselves.
    #[error("{}", Refusal::Yourself.message())]
    Yourself,
    /// The user is the last active admin.
    #[error("{}", Refusal::LastAdmin.message())]
    LastAdmin,
    /// The user is being worked for. Nothing was removed, and the user is as they were.
    #[error(
        "the user has a chat or a scheduled job running, or messages waiting; have them stop it \
         (`/stop` in the chat) or wait until it's done"
    )]
    Busy,
    /// The database failed.
    #[error("database error: {0}")]
    Store(#[from] doozers_store::Error),
    /// A workspace couldn't be deleted.
    #[error("cannot delete a workspace: {0}")]
    Workspaces(#[from] WorkspaceError),
    /// The secret store failed.
    #[error("the secret store failed: {0}")]
    Secrets(#[from] SecretError),
    /// A directory couldn't be read or removed.
    #[error("cannot remove {path}: {source}")]
    Io {
        /// What was being read or removed.
        path: String,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// A workspace was created for the user while they were being removed. The removal didn't
    /// finish: removing the user again does.
    #[error("a workspace of the user appeared while they were being removed; remove them again")]
    WorkspaceAppeared,
    /// Disabling or deleting the user failed.
    #[error("cannot change the user: {0}")]
    User(UserError),
}

impl ForgetError {
    /// Whether the admin can act on it (the page shows it as a refusal, not a failure): the user
    /// doesn't exist, it's the admin or the last admin, or the user is busy.
    pub fn is_refusal(&self) -> bool {
        matches!(
            self,
            Self::NoSuchUser(_) | Self::Yourself | Self::LastAdmin | Self::Busy
        )
    }

    fn user(error: UserError) -> Self {
        match error {
            UserError::NoSuchUser(user) => Self::NoSuchUser(user),
            UserError::LastAdmin => Self::LastAdmin,
            other => Self::User(other),
        }
    }
}

impl Forgetter {
    /// A forgetter over `options`.
    pub fn new(options: ForgetOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                options,
                regenerating: TaskTracker::new(),
                one_at_a_time: Mutex::new(()),
            }),
        }
    }

    fn options(&self) -> &ForgetOptions {
        &self.inner.options
    }

    fn find(&self, user: &UserId) -> Result<User, ForgetError> {
        self.options()
            .users
            .list()
            .get()
            .user(user)
            .cloned()
            .ok_or_else(|| ForgetError::NoSuchUser(user.clone()))
    }

    /// Why `acting` can't remove `user`, if they can't.
    fn refusal(&self, acting: &UserId, user: &User) -> Option<Refusal> {
        if &user.id == acting {
            return Some(Refusal::Yourself);
        }
        let directory = self.options().users.list().get();
        let last = user.is_admin()
            && !directory
                .users()
                .iter()
                .any(|other| other.id != user.id && other.is_admin());
        last.then_some(Refusal::LastAdmin)
    }

    /// The agents that have a `memory.db`, by id.
    async fn memory_agents(&self) -> Result<Vec<AgentId>, ForgetError> {
        let dir = self.options().root.agents_dir();
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(ForgetError::Io {
                    path: dir.display().to_string(),
                    source,
                });
            }
        };
        let mut agents = Vec::new();
        loop {
            let entry = entries
                .next_entry()
                .await
                .map_err(|source| ForgetError::Io {
                    path: dir.display().to_string(),
                    source,
                })?;
            let Some(entry) = entry else { break };
            let Some(agent) = entry
                .file_name()
                .to_str()
                .and_then(|name| AgentId::new(name).ok())
            else {
                continue;
            };
            if tokio::fs::try_exists(entry.path().join("memory.db"))
                .await
                .unwrap_or(false)
            {
                agents.push(agent);
            }
        }
        agents.sort();
        Ok(agents)
    }

    /// The secrets in `user`'s space of the secret store.
    async fn user_secrets(&self, user: &UserId) -> Result<Vec<SecretPath>, ForgetError> {
        Ok(self
            .options()
            .secrets
            .list(None)
            .await?
            .into_iter()
            .map(|secret| secret.path)
            .filter(|path| path.owner() == Some(user.as_str()))
            .collect())
    }

    /// `users/<user>/`, if the id is a valid directory name.
    fn profile_dir(&self, user: &UserId) -> Option<std::path::PathBuf> {
        self.options().root.user_dir(user.as_str()).ok()
    }

    /// What removing `user` would delete, for the confirmation page, and why it would be refused.
    ///
    /// # Errors
    ///
    /// [`ForgetError::NoSuchUser`], or the error of reading what the page lists.
    pub async fn preview(
        &self,
        acting: &UserId,
        user: &UserId,
    ) -> Result<ForgetPreview, ForgetError> {
        let options = self.options();
        let found = self.find(user)?;
        let mut memories = Vec::new();
        for agent in self.memory_agents().await? {
            let count = options
                .memory_dbs
                .get(&agent)
                .await?
                .user_memories(user)
                .await?;
            if count > 0 {
                memories.push(AgentMemories {
                    agent,
                    memories: count,
                });
            }
        }
        let profile = match self.profile_dir(user) {
            Some(dir) => tokio::fs::try_exists(&dir).await.unwrap_or(false),
            None => false,
        };
        Ok(ForgetPreview {
            refusal: self.refusal(acting, &found),
            data: options.store.user_data(user).await?,
            memories,
            secrets: self.user_secrets(user).await?.len(),
            profile,
            workspaces: options.store.user_workspaces(user).await?,
            approvals: options.approvals.pending_for(user),
            busy: options.activity.is_busy(user).await,
            user: found,
        })
    }

    /// Forgets `user`, for the admin `acting` (see the [module docs](self)).
    ///
    /// # Errors
    ///
    /// [`ForgetError::NoSuchUser`], [`Yourself`](ForgetError::Yourself),
    /// [`LastAdmin`](ForgetError::LastAdmin), and [`Busy`](ForgetError::Busy) change nothing. Any
    /// other error leaves the user disabled with some of their data deleted: removing them again
    /// finishes it.
    pub async fn forget(&self, acting: &UserId, user: &UserId) -> Result<Forgotten, ForgetError> {
        let options = self.options();
        let _one_at_a_time = self.inner.one_at_a_time.lock().await;
        let found = self.find(user)?;
        if &found.id == acting {
            return Err(ForgetError::Yourself);
        }
        if options.activity.is_busy(user).await {
            return Err(ForgetError::Busy);
        }
        // The last-admin guard is the disable: a user who is disabled already isn't an active
        // admin, and a removal that failed halfway is finished.
        let was_active = found.is_active();
        if was_active {
            options
                .users
                .set_state(user, UserState::Disabled, options.clock.now())
                .await
                .map_err(ForgetError::user)?;
        }
        if options.activity.is_busy(user).await {
            self.reactivate(user, was_active).await;
            return Err(ForgetError::Busy);
        }

        let sessions = options.store.user_session_ids(user).await?;
        let (data, conversations, group_sessions) =
            match options.store.user_data_forget(user).await? {
                UserDataDeletion::Done {
                    data,
                    conversations,
                    group_sessions,
                } => (*data, conversations, group_sessions),
                UserDataDeletion::Busy => {
                    self.reactivate(user, was_active).await;
                    return Err(ForgetError::Busy);
                }
            };
        // Started now and not at the end, so a failure further on doesn't lose them: the sessions
        // are known only from this call, since a retry's finds no summaries left to delete. They
        // run in the background, so the admin doesn't wait for the model, and without the lock.
        for session in &group_sessions {
            let (summaries, session) = (options.summaries.clone(), *session);
            self.inner
                .regenerating
                .spawn(async move { summaries.regenerate(session).await });
        }
        let approvals = options.approvals.forget_user(user, &sessions);
        options.jobs.notify();

        let workspaces = self.delete_workspaces(user).await?;
        let secrets = self.delete_secrets(user).await?;
        let (memories, shared_memories) = self.delete_memories(user).await?;
        let profile = self.delete_profile(user).await?;
        let removed = options
            .users
            .remove(user)
            .await
            .map_err(ForgetError::user)?;
        info!(
            %user,
            by = %acting,
            sessions = data.direct_sessions + data.job_sessions,
            memories,
            secrets,
            workspaces,
            "user forgotten"
        );
        Ok(Forgotten {
            user: removed,
            data,
            conversations,
            memories,
            shared_memories,
            secrets,
            workspaces,
            approvals,
            profile,
            group_sessions: group_sessions.len(),
        })
    }

    /// Waits until the summaries being written again in the background are done. For tests and
    /// for shutdown.
    pub async fn settled(&self) {
        let tracker = &self.inner.regenerating;
        tracker.close();
        tracker.wait().await;
        tracker.reopen();
    }

    /// Makes `user` active again after a removal was refused as busy, if they were active.
    async fn reactivate(&self, user: &UserId, was_active: bool) {
        if !was_active {
            return;
        }
        let options = self.options();
        if let Err(error) = options
            .users
            .set_state(user, UserState::Active, options.clock.now())
            .await
        {
            warn!(%user, %error, "cannot make a user active again after a refused removal");
        }
    }

    /// Deletes the live workspaces of `user` through their plugins and removes the rows. Returns
    /// how many it deleted.
    async fn delete_workspaces(&self, user: &UserId) -> Result<usize, ForgetError> {
        let options = self.options();
        let live = options.store.user_workspaces(user).await?;
        let mut deleted = 0;
        for record in live {
            let reason = match record.owner {
                WorkspaceOwner::Conversation { .. } => DeleteReason::ChatDeleted,
                WorkspaceOwner::Job { .. } => DeleteReason::JobRunEnded,
            };
            if options
                .workspaces
                .delete(&record.owner, reason)
                .await?
                .is_some()
            {
                deleted += 1;
            }
        }
        match options.store.workspaces_forget_user(user).await {
            Ok(_) => Ok(deleted),
            // One was created after the list was read. Data is deleted already, so this is a
            // failure that removing the user again finishes, not a refusal.
            Err(doozers_store::Error::Conflict(_)) => Err(ForgetError::WorkspaceAppeared),
            Err(error) => Err(error.into()),
        }
    }

    /// Deletes every secret in the user's space. Returns how many.
    async fn delete_secrets(&self, user: &UserId) -> Result<usize, ForgetError> {
        let mut deleted = 0;
        for path in self.user_secrets(user).await? {
            if self.options().secrets.delete(&path).await? {
                deleted += 1;
            }
        }
        Ok(deleted)
    }

    /// Deletes the user's private memories in every agent's `memory.db`. Returns how many it
    /// deleted and how many shared memories lost their author.
    async fn delete_memories(&self, user: &UserId) -> Result<(u64, u64), ForgetError> {
        let (mut deleted, mut detached) = (0, 0);
        for agent in self.memory_agents().await? {
            let store = self.options().memory_dbs.get(&agent).await?;
            let (gone, kept) = store.forget_user(user).await?;
            deleted += gone;
            detached += kept;
        }
        Ok((deleted, detached))
    }

    /// Removes `users/<user>/`. Returns whether it was there.
    async fn delete_profile(&self, user: &UserId) -> Result<bool, ForgetError> {
        let Some(dir) = self.profile_dir(user) else {
            return Ok(false);
        };
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(ForgetError::Io {
                path: dir.display().to_string(),
                source,
            }),
        }
    }
}
