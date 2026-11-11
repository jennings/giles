//! `workspaces`: the environment workspaces the core created for owners (PLAN 8.8), and what
//! became of them. Docs/technical/environments.md describes the life cycle.
//!
//! The store keeps the rows and enforces their rules (one `creating` or `active` workspace per
//! owner, the state transitions below); talking to the plugin that holds the workspace is the
//! core's business (`giles_core::workspaces`).
//!
//! The states move `creating` → `active` → `deleting` → `deleted`. A `creating` row that never
//! got an answer goes straight to `deleting` ([`Store::workspaces_fail_creating`]), and an
//! `active` one the plugin no longer has goes straight to `deleted`
//! ([`Store::workspace_mark_gone`]). Only a `creating` or `active` row holds its owner's slot.

use std::fmt;

use giles_types::{AgentId, ConversationId, Timestamp, WorkspaceId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::time::to_millis;
use crate::{
    Error, Result, Session, SessionId, Store, get_opt_time, get_time, parse_id, query_all,
};

/// Who a workspace belongs to. A conversation owns at most one workspace at a time, and so does
/// an isolated job run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WorkspaceOwner {
    /// A conversation with an agent: the key of a session (PLAN 6.2).
    Conversation {
        /// The agent.
        agent: AgentId,
        /// The channel instance.
        channel: String,
        /// The conversation on that channel.
        conversation: ConversationId,
    },
    /// An isolated job run, by its job session. Nothing else uses the workspace.
    Job {
        /// The job session's id (`sessions.id`; the session may be deleted before the workspace).
        session: SessionId,
    },
}

impl fmt::Display for WorkspaceOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conversation {
                agent,
                channel,
                conversation,
            } => write!(f, "{agent} in {channel}/{conversation}"),
            Self::Job { session } => write!(f, "job session {session}"),
        }
    }
}

impl WorkspaceOwner {
    /// The owner of the workspace a turn in `session` uses: the session's key (agent, channel
    /// instance, conversation id) for a conversation, whether it is a person's turn or a `dm`
    /// job's, and the session itself for an isolated job run.
    pub fn of_session(session: &Session) -> Self {
        match session.key() {
            Some(key) => Self::Conversation {
                agent: key.agent,
                channel: key.channel,
                conversation: key.conversation,
            },
            None => Self::Job {
                session: session.id,
            },
        }
    }
}

/// The columns an owner is stored in: agent, channel, conversation, job session.
type OwnerColumns<'a> = (
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<i64>,
);

impl WorkspaceOwner {
    fn columns(&self) -> OwnerColumns<'_> {
        match self {
            Self::Conversation {
                agent,
                channel,
                conversation,
            } => (
                Some(agent.as_str()),
                Some(channel),
                Some(conversation.as_str()),
                None,
            ),
            Self::Job { session } => (None, None, None, Some(session.get())),
        }
    }
}

/// Where a workspace is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceState {
    /// The plugin's `CreateWorkspace` is in flight. Holds the owner's slot.
    Creating,
    /// Created. Holds the owner's slot.
    Active,
    /// The owner's slot is free, but the plugin hasn't confirmed the delete: it may have been
    /// unreachable. The sweep retries it.
    Deleting,
    /// The plugin confirmed the delete, or the workspace was found gone.
    Deleted,
}

impl WorkspaceState {
    /// The value stored in the `state` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Active => "active",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "creating" => Self::Creating,
            "active" => Self::Active,
            "deleting" => Self::Deleting,
            "deleted" => Self::Deleted,
            other => return Err(Error::corrupt(format!("unknown workspace state {other:?}"))),
        })
    }
}

/// Why a workspace was deleted. Stored, so the model can be told after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeleteReason {
    /// The user reset the workspace (`/env reset`).
    UserReset,
    /// The model reset it.
    ModelReset,
    /// The model deleted it.
    ModelDelete,
    /// The sweep deleted it: its owner was inactive for the instance's `delete_after`.
    Inactive,
    /// It belonged to an isolated job run, which ended.
    JobRunEnded,
    /// The environment no longer has it: removed by hand, or lost with the backend's state.
    RemovedOutsideGiles,
    /// Its chat was archived.
    ChatArchived,
    /// Creating it failed or never finished. Nothing the model needs to be told: it saw the
    /// error, or the process died.
    CreationFailed,
}

impl DeleteReason {
    /// The value stored in the `delete_reason` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserReset => "user_reset",
            Self::ModelReset => "model_reset",
            Self::ModelDelete => "model_delete",
            Self::Inactive => "inactive",
            Self::JobRunEnded => "job_run_ended",
            Self::RemovedOutsideGiles => "removed_outside_giles",
            Self::ChatArchived => "chat_archived",
            Self::CreationFailed => "creation_failed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "user_reset" => Self::UserReset,
            "model_reset" => Self::ModelReset,
            "model_delete" => Self::ModelDelete,
            "inactive" => Self::Inactive,
            "job_run_ended" => Self::JobRunEnded,
            "removed_outside_giles" => Self::RemovedOutsideGiles,
            "chat_archived" => Self::ChatArchived,
            "creation_failed" => Self::CreationFailed,
            other => return Err(Error::corrupt(format!("unknown delete reason {other:?}"))),
        })
    }
}

/// The reason in words, for the model and the logs.
impl fmt::Display for DeleteReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UserReset => "the user reset it",
            Self::ModelReset => "you reset it",
            Self::ModelDelete => "you deleted it",
            Self::Inactive => "it was deleted after a long time without use",
            Self::JobRunEnded => "the job run it belonged to ended",
            Self::RemovedOutsideGiles => "it was removed outside Giles",
            Self::ChatArchived => "its chat was archived",
            Self::CreationFailed => "creating it failed",
        })
    }
}

/// A row of `workspaces`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRecord {
    /// The id the core assigned, which the plugin labels its resources with.
    pub id: WorkspaceId,
    /// The `[environment.<name>]` instance that holds it.
    pub environment: String,
    /// Who it belongs to.
    pub owner: WorkspaceOwner,
    /// The options it was created with, a JSON object.
    pub options: serde_json::Value,
    /// What the plugin said the workspace is; `None` until it was created.
    pub description: Option<String>,
    /// The absolute directory commands start in; `None` until it was created.
    pub working_directory: Option<String>,
    /// What survives a stop and a recreate; `None` until it was created.
    pub persistence: Option<String>,
    /// Where it is in its life.
    pub state: WorkspaceState,
    /// When the row was inserted.
    pub created_at: Timestamp,
    /// When a workspace tool call last started it ([`Store::workspace_touch`]); the creation time
    /// until then.
    pub last_used_at: Timestamp,
    /// When the owner's slot was freed; set exactly when the state is `Deleting` or `Deleted`.
    pub deleted_at: Option<Timestamp>,
    /// Why; set exactly when `deleted_at` is.
    pub delete_reason: Option<DeleteReason>,
}

/// A workspace about to be created.
#[derive(Debug, Clone)]
pub struct NewWorkspace {
    /// Its id, chosen by the core: 1 to 64 of `a-z`, `0-9`, `-`.
    pub id: WorkspaceId,
    /// The environment instance that will hold it.
    pub environment: String,
    /// Who it is for.
    pub owner: WorkspaceOwner,
    /// The creation options, a JSON object.
    pub options: serde_json::Value,
    /// Now: `created_at` and `last_used_at`.
    pub now: Timestamp,
}

const COLUMNS: &str = "id, environment, agent, channel, conversation, job_session, options, \
                       description, working_directory, persistence, state, created_at, \
                       last_used_at, deleted_at, delete_reason";

const OWNER_IS: &str = "agent IS ?1 AND channel IS ?2 AND conversation IS ?3 AND job_session IS ?4";

fn from_row(row: &Row<'_>) -> Result<WorkspaceRecord> {
    let job_session: Option<i64> = row.get(5)?;
    let owner = match job_session {
        Some(session) => WorkspaceOwner::Job {
            session: SessionId::new(session),
        },
        None => WorkspaceOwner::Conversation {
            agent: parse_id(row.get(2)?, AgentId::new)?,
            channel: row.get(3)?,
            conversation: parse_id(row.get(4)?, ConversationId::new)?,
        },
    };
    let reason: Option<String> = row.get(14)?;
    Ok(WorkspaceRecord {
        id: parse_id(row.get(0)?, WorkspaceId::new)?,
        environment: row.get(1)?,
        owner,
        options: serde_json::from_str(&row.get::<_, String>(6)?)?,
        description: row.get(7)?,
        working_directory: row.get(8)?,
        persistence: row.get(9)?,
        state: WorkspaceState::parse(&row.get::<_, String>(10)?)?,
        created_at: get_time(row, 11)?,
        last_used_at: get_time(row, 12)?,
        deleted_at: get_opt_time(row, 13)?,
        delete_reason: reason.as_deref().map(DeleteReason::parse).transpose()?,
    })
}

fn find(conn: &Connection, id: &WorkspaceId) -> Result<Option<WorkspaceRecord>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {COLUMNS} FROM workspaces WHERE id = ?1"))?;
    stmt.query_row([id.as_str()], |row| Ok(from_row(row)))
        .optional()?
        .transpose()
}

fn require(conn: &Connection, id: &WorkspaceId) -> Result<WorkspaceRecord> {
    find(conn, id)?.ok_or_else(|| Error::not_found(format!("workspace {id}")))
}

fn current(conn: &Connection, owner: &WorkspaceOwner) -> Result<Option<WorkspaceRecord>> {
    let (agent, channel, conversation, job) = owner.columns();
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM workspaces
         WHERE {OWNER_IS} AND state IN ('creating', 'active')"
    ))?;
    stmt.query_row(params![agent, channel, conversation, job], |row| {
        Ok(from_row(row))
    })
    .optional()?
    .transpose()
}

/// Frees the slot of `id` if it still holds one: `creating` or `active` becomes `deleting`.
fn begin_delete(
    conn: &Connection,
    id: &WorkspaceId,
    reason: DeleteReason,
    now: Timestamp,
) -> Result<Option<WorkspaceRecord>> {
    let changed = conn
        .prepare_cached(
            "UPDATE workspaces SET state = 'deleting', deleted_at = ?2, delete_reason = ?3
             WHERE id = ?1 AND state IN ('creating', 'active')",
        )?
        .execute(params![id.as_str(), to_millis(now), reason.as_str()])?;
    if changed == 0 {
        return Ok(None);
    }
    find(conn, id)
}

impl Store {
    /// Inserts a `creating` workspace for its owner.
    ///
    /// # Errors
    ///
    /// [`Error::Conflict`] naming the workspace the owner has already, if it has a `creating` or
    /// `active` one, or `id` is taken. Nothing changes.
    pub async fn workspace_insert(&self, new: NewWorkspace) -> Result<WorkspaceRecord> {
        let options = serde_json::to_string(&new.options)?;
        self.write(move |conn| {
            if let Some(held) = current(conn, &new.owner)? {
                return Err(Error::conflict(format!(
                    "{} already has the workspace {}",
                    new.owner, held.id
                )));
            }
            if find(conn, &new.id)?.is_some() {
                return Err(Error::conflict(format!("workspace id {} is taken", new.id)));
            }
            let (agent, channel, conversation, job) = new.owner.columns();
            let now = to_millis(new.now);
            conn.prepare_cached(
                "INSERT INTO workspaces (id, environment, agent, channel, conversation,
                     job_session, options, state, created_at, last_used_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'creating', ?8, ?8)",
            )?
            .execute(params![
                new.id.as_str(),
                new.environment,
                agent,
                channel,
                conversation,
                job,
                options,
                now
            ])?;
            require(conn, &new.id)
        })
        .await
    }

    /// Records what the plugin reported for a `creating` workspace and makes it `active`.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if there is no such workspace; [`Error::Conflict`] if it isn't
    /// `creating` (it was deleted meanwhile). Nothing changes.
    pub async fn workspace_activate(
        &self,
        id: &WorkspaceId,
        description: &str,
        working_directory: &str,
        persistence: &str,
    ) -> Result<WorkspaceRecord> {
        let id = id.clone();
        let (description, working_directory, persistence) = (
            description.to_owned(),
            working_directory.to_owned(),
            persistence.to_owned(),
        );
        self.write(move |conn| {
            let held = require(conn, &id)?;
            if held.state != WorkspaceState::Creating {
                return Err(Error::conflict(format!(
                    "workspace {id} is {}, not creating",
                    held.state.as_str()
                )));
            }
            conn.prepare_cached(
                "UPDATE workspaces SET state = 'active', description = ?2,
                     working_directory = ?3, persistence = ?4
                 WHERE id = ?1",
            )?
            .execute(params![
                id.as_str(),
                description,
                working_directory,
                persistence
            ])?;
            require(conn, &id)
        })
        .await
    }

    /// The workspace `id`, in any state.
    pub async fn workspace(&self, id: &WorkspaceId) -> Result<Option<WorkspaceRecord>> {
        let id = id.clone();
        self.read(move |conn| find(conn, &id)).await
    }

    /// The owner's `creating` or `active` workspace, if it has one.
    pub async fn workspace_current(
        &self,
        owner: &WorkspaceOwner,
    ) -> Result<Option<WorkspaceRecord>> {
        let owner = owner.clone();
        self.read(move |conn| current(conn, &owner)).await
    }

    /// The owner's most recently deleted (`deleting` or `deleted`) workspace, if it has one:
    /// what the model is told when it has none now. A workspace whose creation failed doesn't
    /// count.
    pub async fn workspace_last_deleted(
        &self,
        owner: &WorkspaceOwner,
    ) -> Result<Option<WorkspaceRecord>> {
        let owner = owner.clone();
        self.read(move |conn| {
            let (agent, channel, conversation, job) = owner.columns();
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM workspaces
                 WHERE {OWNER_IS} AND state IN ('deleting', 'deleted')
                   AND delete_reason <> 'creation_failed'
                 ORDER BY deleted_at DESC, created_at DESC LIMIT 1"
            ))?;
            stmt.query_row(params![agent, channel, conversation, job], |row| {
                Ok(from_row(row))
            })
            .optional()?
            .transpose()
        })
        .await
    }

    /// Every workspace of the environment instance, in every state, oldest first.
    pub async fn workspaces_of(&self, environment: &str) -> Result<Vec<WorkspaceRecord>> {
        let environment = environment.to_owned();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM workspaces WHERE environment = ?1
                 ORDER BY created_at, id"
            ))?;
            query_all(&mut stmt, [environment], from_row)
        })
        .await
    }

    /// Notes that an `active` workspace was used at `now`. The time never moves back.
    pub async fn workspace_touch(&self, id: &WorkspaceId, now: Timestamp) -> Result<()> {
        let id = id.clone();
        self.write(move |conn| {
            conn.prepare_cached(
                "UPDATE workspaces SET last_used_at = max(last_used_at, ?2)
                 WHERE id = ?1 AND state = 'active'",
            )?
            .execute(params![id.as_str(), to_millis(now)])?;
            Ok(())
        })
        .await
    }

    /// Frees the owner's slot: its `creating` or `active` workspace becomes `deleting` at `now`
    /// for `reason`. Returns it, or `None` if the owner has none.
    pub async fn workspace_begin_delete_for(
        &self,
        owner: &WorkspaceOwner,
        reason: DeleteReason,
        now: Timestamp,
    ) -> Result<Option<WorkspaceRecord>> {
        let owner = owner.clone();
        self.write(move |conn| match current(conn, &owner)? {
            Some(held) => begin_delete(conn, &held.id, reason, now),
            None => Ok(None),
        })
        .await
    }

    /// As [`workspace_begin_delete_for`](Self::workspace_begin_delete_for), for the workspace
    /// `id`. Returns `None` if it isn't `creating` or `active` (it was deleted already).
    pub async fn workspace_begin_delete(
        &self,
        id: &WorkspaceId,
        reason: DeleteReason,
        now: Timestamp,
    ) -> Result<Option<WorkspaceRecord>> {
        let id = id.clone();
        self.write(move |conn| begin_delete(conn, &id, reason, now))
            .await
    }

    /// Records that the plugin confirmed the delete: `deleting` becomes `deleted`, keeping the
    /// time and reason. Returns whether the workspace was `deleting`.
    pub async fn workspace_finish_delete(&self, id: &WorkspaceId) -> Result<bool> {
        let id = id.clone();
        self.write(move |conn| {
            Ok(conn
                .prepare_cached(
                    "UPDATE workspaces SET state = 'deleted' WHERE id = ?1 AND state = 'deleting'",
                )?
                .execute([id.as_str()])?
                == 1)
        })
        .await
    }

    /// Records that the environment no longer has an `active` workspace: it becomes `deleted` at
    /// `now` for `reason`, with nothing left to clean up. Returns whether it was `active`.
    pub async fn workspace_mark_gone(
        &self,
        id: &WorkspaceId,
        reason: DeleteReason,
        now: Timestamp,
    ) -> Result<bool> {
        let id = id.clone();
        self.write(move |conn| {
            Ok(conn
                .prepare_cached(
                    "UPDATE workspaces SET state = 'deleted', deleted_at = ?2, delete_reason = ?3
                     WHERE id = ?1 AND state = 'active'",
                )?
                .execute(params![id.as_str(), to_millis(now), reason.as_str()])?
                == 1)
        })
        .await
    }

    /// Gives up on `creating` rows inserted at or before `until`: they become `deleting` at `now`
    /// with [`DeleteReason::CreationFailed`], so the owner can create again and the plugin is
    /// asked to delete whatever the attempt left. Returns how many.
    pub async fn workspaces_fail_creating(&self, until: Timestamp, now: Timestamp) -> Result<u64> {
        self.write(move |conn| {
            let changed = conn
                .prepare_cached(
                    "UPDATE workspaces SET state = 'deleting', deleted_at = ?2,
                         delete_reason = 'creation_failed'
                     WHERE state = 'creating' AND created_at <= ?1",
                )?
                .execute(params![to_millis(until), to_millis(now)])?;
            Ok(changed as u64)
        })
        .await
    }

    /// Frees the slots of every `active` workspace owned by a job run, for
    /// [`DeleteReason::JobRunEnded`]: at startup, no job run is alive, so none of them has an
    /// owner any more. They become `deleting` at `now`. Returns how many.
    pub async fn workspaces_end_job_runs(&self, now: Timestamp) -> Result<u64> {
        self.write(move |conn| {
            let changed = conn
                .prepare_cached(
                    "UPDATE workspaces SET state = 'deleting', deleted_at = ?1,
                         delete_reason = 'job_run_ended'
                     WHERE state = 'active' AND job_session IS NOT NULL",
                )?
                .execute([to_millis(now)])?;
            Ok(changed as u64)
        })
        .await
    }

    /// Removes the `deleted` rows of the environment instance deleted before `before`. Returns
    /// how many.
    pub async fn workspaces_prune(&self, environment: &str, before: Timestamp) -> Result<u64> {
        let environment = environment.to_owned();
        self.write(move |conn| {
            let removed = conn
                .prepare_cached(
                    "DELETE FROM workspaces
                     WHERE environment = ?1 AND state = 'deleted' AND deleted_at < ?2",
                )?
                .execute(params![environment, to_millis(before)])?;
            Ok(removed as u64)
        })
        .await
    }

    /// When the owner last did anything: for a conversation, the newest message across every
    /// session of its key, ended ones included, or when the newest session started if it has no
    /// messages. `None` for a job (it has no history of its own to speak of) and for a
    /// conversation with no session.
    pub async fn workspace_owner_activity(
        &self,
        owner: &WorkspaceOwner,
    ) -> Result<Option<Timestamp>> {
        let WorkspaceOwner::Conversation {
            agent,
            channel,
            conversation,
        } = owner
        else {
            return Ok(None);
        };
        let (agent, channel, conversation) =
            (agent.to_string(), channel.clone(), conversation.to_string());
        self.read(move |conn| {
            let millis: Option<i64> = conn
                .prepare_cached(
                    "SELECT max(coalesce(
                         (SELECT m.created_at FROM messages m
                          WHERE m.session_id = s.id ORDER BY m.seq DESC LIMIT 1),
                         s.started_at))
                     FROM sessions s
                     WHERE s.agent = ?1 AND s.channel = ?2 AND s.conversation = ?3",
                )?
                .query_row([agent, channel, conversation], |row| row.get(0))?;
            millis.map(crate::time::from_millis).transpose()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::SessionKind;
    use crate::sessions::tests::{at, key, store};

    fn conversation(name: &str) -> WorkspaceOwner {
        let key = key(name);
        WorkspaceOwner::Conversation {
            agent: key.agent,
            channel: key.channel,
            conversation: key.conversation,
        }
    }

    fn job(session: i64) -> WorkspaceOwner {
        WorkspaceOwner::Job {
            session: SessionId::new(session),
        }
    }

    fn new(id: &str, owner: WorkspaceOwner, secs: i64) -> NewWorkspace {
        NewWorkspace {
            id: WorkspaceId::new(id).unwrap(),
            environment: "box".into(),
            owner,
            options: json!({"image": "alpine"}),
            now: at(secs),
        }
    }

    fn id(id: &str) -> WorkspaceId {
        WorkspaceId::new(id).unwrap()
    }

    async fn active(store: &Store, id_: &str, owner: WorkspaceOwner, secs: i64) {
        store.workspace_insert(new(id_, owner, secs)).await.unwrap();
        store
            .workspace_activate(&id(id_), "alpine", "/work", "files persist")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_workspace_is_created_then_activated_with_what_the_plugin_reported() {
        let (_dir, store) = store().await;
        let owner = conversation("c1");
        let created = store
            .workspace_insert(new("ws-1", owner.clone(), 5))
            .await
            .unwrap();
        assert_eq!(created.state, WorkspaceState::Creating);
        assert_eq!(created.options, json!({"image": "alpine"}));
        assert_eq!((created.created_at, created.last_used_at), (at(5), at(5)));
        assert_eq!(created.description, None);
        assert_eq!(
            store.workspace_current(&owner).await.unwrap().unwrap().id,
            id("ws-1")
        );

        let activated = store
            .workspace_activate(&id("ws-1"), "alpine 3.20", "/work", "files persist")
            .await
            .unwrap();
        assert_eq!(activated.state, WorkspaceState::Active);
        assert_eq!(activated.description.as_deref(), Some("alpine 3.20"));
        assert_eq!(activated.working_directory.as_deref(), Some("/work"));
        assert_eq!(activated.persistence.as_deref(), Some("files persist"));
        assert_eq!(activated.owner, owner);

        let err = store
            .workspace_activate(&id("ws-1"), "x", "/", "y")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        let err = store
            .workspace_activate(&id("nope"), "x", "/", "y")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }

    #[tokio::test]
    async fn an_owner_holds_one_workspace_until_it_is_deleted() {
        let (_dir, store) = store().await;
        let owner = conversation("c1");
        active(&store, "ws-1", owner.clone(), 0).await;

        let err = store
            .workspace_insert(new("ws-2", owner.clone(), 1))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Conflict(m) if m.contains("ws-1")),
            "{err}"
        );
        // Other owners are unaffected, and so is another agent in the same conversation.
        store
            .workspace_insert(new("ws-3", conversation("c2"), 1))
            .await
            .unwrap();
        store
            .workspace_insert(new("ws-4", job(7), 1))
            .await
            .unwrap();
        let err = store
            .workspace_insert(new("ws-5", job(7), 2))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        let other_agent = WorkspaceOwner::Conversation {
            agent: AgentId::new("bunter").unwrap(),
            channel: "whatsapp".into(),
            conversation: ConversationId::new("c1").unwrap(),
        };
        store
            .workspace_insert(new("ws-6", other_agent, 1))
            .await
            .unwrap();

        // A taken id is refused even for a free owner.
        let err = store
            .workspace_insert(new("ws-1", conversation("c9"), 3))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");

        // Deleting frees the slot at once, though the plugin hasn't confirmed.
        let deleting = store
            .workspace_begin_delete_for(&owner, DeleteReason::ModelDelete, at(10))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deleting.state, WorkspaceState::Deleting);
        assert_eq!(deleting.deleted_at, Some(at(10)));
        assert_eq!(deleting.delete_reason, Some(DeleteReason::ModelDelete));
        assert!(store.workspace_current(&owner).await.unwrap().is_none());
        store
            .workspace_insert(new("ws-7", owner.clone(), 11))
            .await
            .unwrap();
        // Nothing left to delete the second time.
        assert!(
            store
                .workspace_begin_delete(&id("ws-1"), DeleteReason::Inactive, at(12))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_last_deleted_workspace_is_the_newest_and_failed_creations_dont_count() {
        let (_dir, store) = store().await;
        let owner = conversation("c1");
        assert!(
            store
                .workspace_last_deleted(&owner)
                .await
                .unwrap()
                .is_none()
        );

        active(&store, "ws-1", owner.clone(), 0).await;
        store
            .workspace_begin_delete(&id("ws-1"), DeleteReason::Inactive, at(10))
            .await
            .unwrap();
        active(&store, "ws-2", owner.clone(), 20).await;
        store
            .workspace_begin_delete(&id("ws-2"), DeleteReason::UserReset, at(30))
            .await
            .unwrap();
        assert!(store.workspace_finish_delete(&id("ws-2")).await.unwrap());
        store
            .workspace_insert(new("ws-3", owner.clone(), 40))
            .await
            .unwrap();
        store
            .workspace_begin_delete(&id("ws-3"), DeleteReason::CreationFailed, at(41))
            .await
            .unwrap();

        let last = store.workspace_last_deleted(&owner).await.unwrap().unwrap();
        assert_eq!(last.id, id("ws-2"));
        assert_eq!(last.state, WorkspaceState::Deleted);
        assert_eq!(last.delete_reason, Some(DeleteReason::UserReset));
        assert_eq!(last.options, json!({"image": "alpine"}));
        assert!(
            store
                .workspace_last_deleted(&conversation("c2"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn finishing_a_delete_keeps_when_and_why_and_gone_workspaces_record_a_reason() {
        let (_dir, store) = store().await;
        active(&store, "ws-1", conversation("c1"), 0).await;
        active(&store, "ws-2", conversation("c2"), 0).await;

        assert!(
            !store.workspace_finish_delete(&id("ws-1")).await.unwrap(),
            "an active workspace isn't being deleted"
        );
        store
            .workspace_begin_delete(&id("ws-1"), DeleteReason::Inactive, at(5))
            .await
            .unwrap();
        assert!(store.workspace_finish_delete(&id("ws-1")).await.unwrap());
        let done = store.workspace(&id("ws-1")).await.unwrap().unwrap();
        assert_eq!(done.state, WorkspaceState::Deleted);
        assert_eq!(done.deleted_at, Some(at(5)));
        assert_eq!(done.delete_reason, Some(DeleteReason::Inactive));

        assert!(
            store
                .workspace_mark_gone(&id("ws-2"), DeleteReason::RemovedOutsideGiles, at(6))
                .await
                .unwrap()
        );
        assert!(
            !store
                .workspace_mark_gone(&id("ws-2"), DeleteReason::RemovedOutsideGiles, at(7))
                .await
                .unwrap()
        );
        let gone = store.workspace(&id("ws-2")).await.unwrap().unwrap();
        assert_eq!(gone.state, WorkspaceState::Deleted);
        assert_eq!(gone.deleted_at, Some(at(6)));
        assert!(store.workspace(&id("nope")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn touching_moves_the_last_use_forward_only_for_active_workspaces() {
        let (_dir, store) = store().await;
        active(&store, "ws-1", conversation("c1"), 0).await;
        store.workspace_touch(&id("ws-1"), at(50)).await.unwrap();
        store.workspace_touch(&id("ws-1"), at(20)).await.unwrap();
        let held = store.workspace(&id("ws-1")).await.unwrap().unwrap();
        assert_eq!(held.last_used_at, at(50));

        store
            .workspace_begin_delete(&id("ws-1"), DeleteReason::Inactive, at(60))
            .await
            .unwrap();
        store.workspace_touch(&id("ws-1"), at(70)).await.unwrap();
        let held = store.workspace(&id("ws-1")).await.unwrap().unwrap();
        assert_eq!(held.last_used_at, at(50));
    }

    #[tokio::test]
    async fn stale_creating_rows_are_given_up_and_deleted_rows_are_pruned_by_instance() {
        let (_dir, store) = store().await;
        store
            .workspace_insert(new("old", conversation("c1"), 0))
            .await
            .unwrap();
        store
            .workspace_insert(new("fresh", conversation("c2"), 100))
            .await
            .unwrap();
        active(&store, "live", conversation("c3"), 0).await;
        assert_eq!(
            store
                .workspaces_fail_creating(at(50), at(200))
                .await
                .unwrap(),
            1
        );
        let old = store.workspace(&id("old")).await.unwrap().unwrap();
        assert_eq!(old.state, WorkspaceState::Deleting);
        assert_eq!(old.delete_reason, Some(DeleteReason::CreationFailed));
        assert_eq!(old.deleted_at, Some(at(200)));
        assert_eq!(
            store.workspace(&id("fresh")).await.unwrap().unwrap().state,
            WorkspaceState::Creating
        );
        // The owner can create again at once.
        store
            .workspace_insert(new("again", conversation("c1"), 201))
            .await
            .unwrap();

        store.workspace_finish_delete(&id("old")).await.unwrap();
        // Another instance's deleted row, and a `deleting` one, are kept.
        store
            .write(|conn| {
                conn.execute(
                    "INSERT INTO workspaces (id, environment, job_session, options, state,
                         created_at, last_used_at, deleted_at, delete_reason)
                     VALUES ('elsewhere', 'other', 1, '{}', 'deleted', 0, 0, 0, 'inactive')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(store.workspaces_prune("box", at(200)).await.unwrap(), 0);
        assert_eq!(store.workspaces_prune("box", at(201)).await.unwrap(), 1);
        assert!(store.workspace(&id("old")).await.unwrap().is_none());
        assert!(store.workspace(&id("elsewhere")).await.unwrap().is_some());
        let ids: Vec<_> = store
            .workspaces_of("box")
            .await
            .unwrap()
            .into_iter()
            .map(|w| w.id.to_string())
            .collect();
        assert_eq!(ids, ["live", "fresh", "again"]);
    }

    #[tokio::test]
    async fn job_run_workspaces_are_all_ended_at_once() {
        let (_dir, store) = store().await;
        active(&store, "job-1", job(1), 0).await;
        active(&store, "job-2", job(2), 0).await;
        active(&store, "chat", conversation("c1"), 0).await;
        store
            .workspace_insert(new("job-3", job(3), 0))
            .await
            .unwrap();
        assert_eq!(store.workspaces_end_job_runs(at(9)).await.unwrap(), 2);
        for ended in ["job-1", "job-2"] {
            let row = store.workspace(&id(ended)).await.unwrap().unwrap();
            assert_eq!(row.state, WorkspaceState::Deleting);
            assert_eq!(row.delete_reason, Some(DeleteReason::JobRunEnded));
            assert_eq!(row.deleted_at, Some(at(9)));
        }
        // A conversation's workspace stays, and a job's `creating` row is `fail_creating`'s.
        assert_eq!(
            store.workspace(&id("chat")).await.unwrap().unwrap().state,
            WorkspaceState::Active
        );
        assert_eq!(
            store.workspace(&id("job-3")).await.unwrap().unwrap().state,
            WorkspaceState::Creating
        );
    }

    #[tokio::test]
    async fn conversation_activity_is_the_newest_message_of_any_session_of_the_key() {
        let (_dir, store) = store().await;
        let owner = conversation("c1");
        assert_eq!(store.workspace_owner_activity(&owner).await.unwrap(), None);
        assert_eq!(
            store.workspace_owner_activity(&job(1)).await.unwrap(),
            None,
            "a job has no conversation"
        );

        let (first, _) = store
            .open_session(key("c1"), SessionKind::Direct, None, at(10))
            .await
            .unwrap();
        // No messages yet: the session's start.
        assert_eq!(
            store.workspace_owner_activity(&owner).await.unwrap(),
            Some(at(10))
        );
        let first = first.id;
        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO messages (session_id, seq, role, content, meta, created_at)
                     VALUES (?1, 1, 'user', '[]', '{}', ?2), (?1, 2, 'assistant', '[]', '{}', ?3)",
                    params![first, to_millis(at(20)), to_millis(at(30))],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            store.workspace_owner_activity(&owner).await.unwrap(),
            Some(at(30))
        );
        // `/new` ended the session and started another; the old one's messages still count
        // until the new session has something later.
        store.end_session(first, at(40)).await.unwrap();
        assert_eq!(
            store.workspace_owner_activity(&owner).await.unwrap(),
            Some(at(30))
        );
        store
            .open_session(key("c1"), SessionKind::Direct, None, at(45))
            .await
            .unwrap();
        assert_eq!(
            store.workspace_owner_activity(&owner).await.unwrap(),
            Some(at(45))
        );
        // Another conversation is another owner.
        assert_eq!(
            store
                .workspace_owner_activity(&conversation("c2"))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn the_schema_refuses_an_owner_in_both_forms_or_neither_and_a_missing_reason() {
        let (_dir, store) = store().await;
        let insert = |columns: &'static str, values: &'static str| {
            let store = store.clone();
            async move {
                store
                    .write(move |conn| {
                        conn.execute(
                            &format!(
                                "INSERT INTO workspaces (id, environment, options, created_at,
                                     last_used_at, {columns})
                                 VALUES ('w', 'box', '{{}}', 0, 0, {values})"
                            ),
                            [],
                        )?;
                        Ok(())
                    })
                    .await
            }
        };
        let none = insert("state", "'creating'").await.unwrap_err();
        assert!(matches!(none, Error::Database(_)), "{none}");
        let both = insert(
            "state, agent, channel, conversation, job_session",
            "'creating', 'a', 'c', 'x', 1",
        )
        .await
        .unwrap_err();
        assert!(matches!(both, Error::Database(_)), "{both}");
        let partial = insert("state, agent, job_session", "'creating', 'a', NULL")
            .await
            .unwrap_err();
        assert!(matches!(partial, Error::Database(_)), "{partial}");
        let no_reason = insert("state, job_session, deleted_at", "'deleting', 1, 0")
            .await
            .unwrap_err();
        assert!(matches!(no_reason, Error::Database(_)), "{no_reason}");
        let unreported = insert("state, job_session", "'active', 1")
            .await
            .unwrap_err();
        assert!(matches!(unreported, Error::Database(_)), "{unreported}");
        insert("state, job_session", "'creating', 1").await.unwrap();
    }
}
