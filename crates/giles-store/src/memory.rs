//! `MemoryStore`: the per-agent memory database (`data/agents/<agent>/memory.db`).
//!
//! A memory is one short piece of text the agent keeps, with an integer id the model names it by.
//! The database is a separate file from `giles.db`, opened through the same [`Store`] machinery
//! (one writer, a pool of readers, the configured journal mode) but with its own migrations and
//! `user_version`. [`MemoryStore`] wraps that handle and exposes only the memory methods.
//!
//! # Visibility
//!
//! Every method takes a [`MemoryViewer`]: the session's owner scope and whether its requester may
//! write shared memories.
//!
//! - A private memory (`shared = false`) is visible only to viewers with the same
//!   [`MemoryOwner`]: one user's sessions, or one group conversation. Another user, another
//!   group, and a user's DM versus a group they're in are all different owners.
//! - A shared memory is visible to every viewer.
//! - A viewer with [`MemoryOwner::None`] (a config job with no requester) sees shared memories
//!   only. Since it could never read it back, it can't write a private memory either.
//! - A memory the viewer can't see is reported as [`Error::UnknownMemory`], the same error as an
//!   id that doesn't exist.
//! - Creating a shared memory, marking a memory shared, and changing or forgetting a shared
//!   memory need [`MemoryViewer::can_write_shared`]; without it the error is
//!   [`Error::SharedWriteForbidden`]. A shared memory keeps its creator as owner. Un-sharing one
//!   makes it private to the viewer who un-shared it.
//!
//! # Limits
//!
//! Enforced inside the write transaction, so concurrent writers can't break them:
//!
//! - Content is trimmed, must not be empty, and has at most [`MAX_MEMORY_CHARS`] characters
//!   (characters, not bytes): [`Error::InvalidRequest`] or [`Error::MemoryTooLong`].
//! - The characters of the core memories in one scope must stay within the `core_cap` the caller
//!   passes. A scope is the shared memories, or one owner's private memories (a user, or a group
//!   conversation). An add or edit that would take a scope over the cap fails with
//!   [`Error::CoreCapExceeded`], carrying the scope's current core memories. Marking a core memory
//!   shared moves it into the shared scope, and un-sharing moves it into the viewer's own scope,
//!   so those are checked there. A change that doesn't grow a scope's total is always allowed, so
//!   a scope that is over a lowered cap can still shrink.
//!
//! # Search
//!
//! [`MemoryStore::recall`] runs FTS5 over the visible memories, core ones included. The query is
//! plain words, each quoted and OR-ed, as for transcript search; a query with no words returns
//! nothing.

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use giles_types::{ConversationId, Timestamp, UserId};
use rusqlite::{Connection, Row, params};

use crate::migrate::MEMORY_MIGRATIONS;
use crate::search::fts_query;
use crate::sessions::SessionId;
use crate::time::to_millis;
use crate::{Error, OpenOptions, Result, Store, get_time, parse_id, query_all, query_opt, row_id};

/// The longest a memory's content may be, in characters.
pub const MAX_MEMORY_CHARS: usize = 1000;

/// Directories the store creates get this mode.
const PRIVATE_DIR_MODE: u32 = 0o700;
/// The database file is created with this mode (its `-wal` and `-shm` files follow it).
const PRIVATE_FILE_MODE: u32 = 0o600;

row_id!(
    /// A row in `memories`: the id the model names a memory by.
    MemoryId
);

/// Who a memory belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MemoryOwner {
    /// A user, by internal id: that user's direct and job sessions.
    User(UserId),
    /// One group conversation.
    Group {
        /// The channel instance.
        instance: String,
        /// The conversation on it.
        conversation: ConversationId,
    },
    /// Nobody: a config job with no requester.
    None,
}

impl MemoryOwner {
    /// The columns this owner is stored in: kind, user, instance, conversation.
    fn columns(&self) -> (&'static str, Option<&str>, Option<&str>, Option<&str>) {
        match self {
            Self::User(user) => ("user", Some(user.as_str()), None, None),
            Self::Group {
                instance,
                conversation,
            } => (
                "group",
                None,
                Some(instance.as_str()),
                Some(conversation.as_str()),
            ),
            Self::None => ("none", None, None, None),
        }
    }
}

/// A set of memories that share one core cap.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MemoryScope {
    /// The shared memories.
    Shared,
    /// One owner's private memories.
    Private(MemoryOwner),
}

/// The session a memory call runs for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryViewer {
    /// The session's owner scope: the private memories it sees and writes.
    pub owner: MemoryOwner,
    /// Whether the session's requester may write shared memories: true for the agent owner's
    /// turns and for config jobs.
    pub can_write_shared: bool,
}

impl MemoryViewer {
    /// A viewer with `owner` as its scope.
    pub fn new(owner: MemoryOwner, can_write_shared: bool) -> Self {
        Self {
            owner,
            can_write_shared,
        }
    }

    /// Whether `memory` is visible to this viewer.
    fn sees(&self, memory: &Memory) -> bool {
        memory.shared || (self.owner != MemoryOwner::None && memory.owner == self.owner)
    }
}

/// A stored memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Memory {
    /// Its id.
    pub id: MemoryId,
    /// What it says, at most [`MAX_MEMORY_CHARS`] characters.
    pub content: String,
    /// Who it belongs to.
    pub owner: MemoryOwner,
    /// Whether every session can see it, not only the owner's.
    pub shared: bool,
    /// Whether it is loaded into the prompt.
    pub core: bool,
    /// When it was added.
    pub created_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
    /// The session (in `giles.db`) it was written in. Only recorded, never checked.
    pub source_session: Option<SessionId>,
}

impl Memory {
    /// The scope whose core cap this memory counts against.
    pub fn scope(&self) -> MemoryScope {
        scope_of(self.shared, &self.owner)
    }

    /// The content's length in characters.
    pub fn chars(&self) -> usize {
        self.content.chars().count()
    }
}

/// A memory to add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMemory {
    /// What it says.
    pub content: String,
    /// Whether it is loaded into the prompt.
    pub core: bool,
    /// Whether every session can see it. Needs [`MemoryViewer::can_write_shared`].
    pub shared: bool,
    /// The session it is written in.
    pub source_session: Option<SessionId>,
}

/// Changes to a memory; fields left `None` stay as they are.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryEdit {
    /// The new content, replacing the whole old content.
    pub content: Option<String>,
    /// Whether the memory is core.
    pub core: Option<bool>,
    /// Whether the memory is shared.
    pub shared: Option<bool>,
}

/// One scope's core memories and their total size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoreScope {
    /// The core memories, oldest id first.
    pub memories: Vec<Memory>,
    /// The total characters of their content.
    pub chars: usize,
}

impl CoreScope {
    fn new(memories: Vec<Memory>) -> Self {
        let chars = memories.iter().map(Memory::chars).sum();
        Self { memories, chars }
    }
}

/// The core memories a session sees (what its prompt loads).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoreMemories {
    /// The shared scope, listed first.
    pub shared: CoreScope,
    /// The viewer's own scope: its user's, or its group's. Empty for [`MemoryOwner::None`].
    pub own: CoreScope,
}

/// Why a write was rejected for going over the core cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreOverflow {
    /// The scope that would go over.
    pub scope: MemoryScope,
    /// That scope's core memories as they are now (the write changed nothing), oldest id first.
    pub memories: Vec<Memory>,
    /// Their total characters now.
    pub used: usize,
    /// The cap.
    pub cap: usize,
    /// How many characters over the cap the rejected write would have been.
    pub overflow: usize,
}

/// The per-agent memory database. Cheap to clone; all clones share the connections.
///
/// See the module docs for visibility and limits. Every method runs on Tokio's blocking pool; a
/// write is one `IMMEDIATE` transaction on the single writer connection.
#[derive(Clone, Debug)]
pub struct MemoryStore {
    store: Store,
}

impl MemoryStore {
    /// Opens (creating if needed) the memory database at `path` and migrates it to
    /// [`MEMORY_SCHEMA_VERSION`](crate::MEMORY_SCHEMA_VERSION).
    ///
    /// Missing parent directories are created with mode 0700 and a missing file with mode 0600
    /// (an existing file or directory keeps its mode). `options` apply as for
    /// [`Store::open_with`], including the journal mode.
    pub async fn open(path: impl Into<PathBuf>, options: OpenOptions) -> Result<Self> {
        let path = path.into();
        let create = path.clone();
        tokio::task::spawn_blocking(move || create_private_file(&create)).await??;
        let store = Store::open_migrating(path, options, MEMORY_MIGRATIONS).await?;
        Ok(Self { store })
    }

    /// The database file.
    pub fn path(&self) -> &Path {
        self.store.path()
    }

    /// Writes a consistent snapshot of the database to `dest` with `VACUUM INTO`, while the
    /// store keeps serving. See [`Store::vacuum_into`], which this is.
    pub async fn vacuum_into(&self, dest: PathBuf) -> Result<()> {
        self.store.vacuum_into(dest).await
    }

    /// Adds a memory and returns it.
    ///
    /// # Errors
    ///
    /// [`Error::SharedWriteForbidden`] for a shared memory from a viewer that can't write them;
    /// [`Error::InvalidRequest`] for empty content, or a private memory from a viewer with
    /// [`MemoryOwner::None`]; [`Error::MemoryTooLong`]; [`Error::CoreCapExceeded`] when a core
    /// memory doesn't fit in its scope within `core_cap`.
    pub async fn add(
        &self,
        viewer: &MemoryViewer,
        new: NewMemory,
        core_cap: usize,
        now: Timestamp,
    ) -> Result<Memory> {
        let viewer = viewer.clone();
        self.store
            .write(move |conn| add_memory(conn, &viewer, new, core_cap, now))
            .await
    }

    /// Changes a memory's content and flags, sets `updated_at` to `now`, and returns it.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownMemory`] if the viewer can't see `id`; [`Error::SharedWriteForbidden`] if
    /// the memory is shared, or the edit shares it, and the viewer can't write shared memories;
    /// [`Error::InvalidRequest`] for an edit that changes nothing, empty content, or an edit that
    /// would leave a memory private with no owner; [`Error::MemoryTooLong`];
    /// [`Error::CoreCapExceeded`].
    pub async fn edit(
        &self,
        viewer: &MemoryViewer,
        id: MemoryId,
        edit: MemoryEdit,
        core_cap: usize,
        now: Timestamp,
    ) -> Result<Memory> {
        let viewer = viewer.clone();
        self.store
            .write(move |conn| edit_memory(conn, &viewer, id, edit, core_cap, now))
            .await
    }

    /// Deletes a memory and returns what it was.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownMemory`] if the viewer can't see `id`; [`Error::SharedWriteForbidden`] for
    /// a shared memory the viewer can't write.
    pub async fn forget(&self, viewer: &MemoryViewer, id: MemoryId) -> Result<Memory> {
        let viewer = viewer.clone();
        self.store
            .write(move |conn| {
                let memory = visible(conn, &viewer, id)?;
                if memory.shared && !viewer.can_write_shared {
                    return Err(Error::SharedWriteForbidden);
                }
                conn.execute("DELETE FROM memories WHERE id = ?1", [id])?;
                Ok(memory)
            })
            .await
    }

    /// Returns a memory.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownMemory`] if it doesn't exist or the viewer can't see it.
    pub async fn get(&self, viewer: &MemoryViewer, id: MemoryId) -> Result<Memory> {
        let viewer = viewer.clone();
        self.store
            .read(move |conn| visible(conn, &viewer, id))
            .await
    }

    /// The core memories the viewer sees, shared first and then its own, each scope with its
    /// total characters.
    pub async fn core_memories(&self, viewer: &MemoryViewer) -> Result<CoreMemories> {
        let owner = viewer.owner.clone();
        self.store
            .read(move |conn| {
                let shared = core_in_scope(conn, &MemoryScope::Shared)?;
                let own = match owner {
                    MemoryOwner::None => Vec::new(),
                    owner => core_in_scope(conn, &MemoryScope::Private(owner))?,
                };
                Ok(CoreMemories {
                    shared: CoreScope::new(shared),
                    own: CoreScope::new(own),
                })
            })
            .await
    }

    /// Full-text search over the memories the viewer sees, core ones included: at most `limit`
    /// (and at least one), best matches first by bm25, the most recently updated first among
    /// equal matches. `query` is plain words, each quoted and OR-ed, so FTS5 syntax in it is
    /// matched as text; a query with no words returns nothing. English stemming applies, and
    /// diacritics are ignored.
    pub async fn recall(
        &self,
        viewer: &MemoryViewer,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Memory>> {
        let Some(query) = fts_query(query) else {
            return Ok(Vec::new());
        };
        let owner = viewer.owner.clone();
        self.store
            .read(move |conn| {
                let (kind, user, instance, conversation) = owner.columns();
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT {COLUMNS}
                     FROM memory_search
                     JOIN memories m ON m.id = memory_search.rowid
                     WHERE memory_search MATCH :query
                       AND (m.shared = 1
                            OR (:kind <> 'none'
                                AND m.owner_kind = :kind
                                AND m.owner_user IS :user
                                AND m.owner_instance IS :instance
                                AND m.owner_conversation IS :conversation))
                     ORDER BY memory_search.rank, m.updated_at DESC, m.id DESC
                     LIMIT :limit"
                ))?;
                let params = rusqlite::named_params! {
                    ":query": query,
                    ":kind": kind,
                    ":user": user,
                    ":instance": instance,
                    ":conversation": conversation,
                    ":limit": limit.max(1),
                };
                query_all(&mut stmt, params, memory_from_row)
            })
            .await
    }
}

/// Creates the file's missing parent directories (mode 0700) and the file itself (mode 0600),
/// leaving an existing file alone.
fn create_private_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIR_MODE)
            .create(parent)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .open(path)?;
    Ok(())
}

/// The columns of `memories` (as `m`), in the order [`memory_from_row`] reads them.
const COLUMNS: &str = "m.id, m.content, m.owner_kind, m.owner_user, m.owner_instance, \
                       m.owner_conversation, m.shared, m.core, m.created_at, m.updated_at, \
                       m.source_session";

fn memory_from_row(row: &Row<'_>) -> Result<Memory> {
    let kind: String = row.get(2)?;
    let user: Option<String> = row.get(3)?;
    let instance: Option<String> = row.get(4)?;
    let conversation: Option<String> = row.get(5)?;
    let owner = match (kind.as_str(), user, instance, conversation) {
        ("user", Some(user), None, None) => MemoryOwner::User(parse_id(user, UserId::new)?),
        ("group", None, Some(instance), Some(conversation)) => MemoryOwner::Group {
            instance,
            conversation: parse_id(conversation, ConversationId::new)?,
        },
        ("none", None, None, None) => MemoryOwner::None,
        _ => return Err(Error::corrupt(format!("memory owner of kind {kind:?}"))),
    };
    Ok(Memory {
        id: row.get(0)?,
        content: row.get(1)?,
        owner,
        shared: row.get(6)?,
        core: row.get(7)?,
        created_at: get_time(row, 8)?,
        updated_at: get_time(row, 9)?,
        source_session: row.get(10)?,
    })
}

fn scope_of(shared: bool, owner: &MemoryOwner) -> MemoryScope {
    if shared {
        MemoryScope::Shared
    } else {
        MemoryScope::Private(owner.clone())
    }
}

fn fetch(conn: &Connection, id: MemoryId) -> Result<Option<Memory>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {COLUMNS} FROM memories m WHERE m.id = ?1"))?;
    query_opt(&mut stmt, [id], memory_from_row)
}

/// The memory, if it exists and `viewer` sees it; [`Error::UnknownMemory`] either way it doesn't.
fn visible(conn: &Connection, viewer: &MemoryViewer, id: MemoryId) -> Result<Memory> {
    fetch(conn, id)?
        .filter(|memory| viewer.sees(memory))
        .ok_or(Error::UnknownMemory(id))
}

/// Trims `content` and checks it against the limits.
fn check_content(content: &str) -> Result<String> {
    let content = content.trim();
    if content.is_empty() {
        return Err(Error::invalid("memory content is empty"));
    }
    let chars = content.chars().count();
    if chars > MAX_MEMORY_CHARS {
        return Err(Error::MemoryTooLong {
            chars,
            max: MAX_MEMORY_CHARS,
        });
    }
    Ok(content.to_owned())
}

/// The core memories of `scope`, oldest id first.
fn core_in_scope(conn: &Connection, scope: &MemoryScope) -> Result<Vec<Memory>> {
    match scope {
        MemoryScope::Shared => {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM memories m
                 WHERE m.shared = 1 AND m.core = 1
                 ORDER BY m.id"
            ))?;
            query_all(&mut stmt, params![], memory_from_row)
        }
        MemoryScope::Private(owner) => {
            let (kind, user, instance, conversation) = owner.columns();
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM memories m
                 WHERE m.shared = 0 AND m.core = 1
                   AND m.owner_kind = ?1
                   AND m.owner_user IS ?2
                   AND m.owner_instance IS ?3
                   AND m.owner_conversation IS ?4
                 ORDER BY m.id"
            ))?;
            query_all(
                &mut stmt,
                params![kind, user, instance, conversation],
                memory_from_row,
            )
        }
    }
}

/// Fails with [`Error::CoreCapExceeded`] if putting a memory of `chars` characters into `scope`
/// as a core memory would take the scope over `cap` and grow it.
///
/// `id` is the memory itself when it already exists, so a core memory already in the scope is
/// replaced instead of counted twice. Shrinking a scope that is already over the cap (the cap was
/// lowered) is allowed.
fn enforce_core_cap(
    conn: &Connection,
    scope: MemoryScope,
    id: Option<MemoryId>,
    chars: usize,
    cap: usize,
) -> Result<()> {
    let current = core_in_scope(conn, &scope)?;
    let used: usize = current.iter().map(Memory::chars).sum();
    let others: usize = current
        .iter()
        .filter(|memory| Some(memory.id) != id)
        .map(Memory::chars)
        .sum();
    let after = others + chars;
    if after > cap && after > used {
        return Err(Error::CoreCapExceeded(Box::new(CoreOverflow {
            scope,
            memories: current,
            used,
            cap,
            overflow: after - cap,
        })));
    }
    Ok(())
}

fn add_memory(
    conn: &Connection,
    viewer: &MemoryViewer,
    new: NewMemory,
    core_cap: usize,
    now: Timestamp,
) -> Result<Memory> {
    if new.shared && !viewer.can_write_shared {
        return Err(Error::SharedWriteForbidden);
    }
    if !new.shared && viewer.owner == MemoryOwner::None {
        return Err(Error::invalid(
            "a memory with no owner must be shared: nothing could read it back otherwise",
        ));
    }
    let content = check_content(&new.content)?;
    if new.core {
        let scope = scope_of(new.shared, &viewer.owner);
        enforce_core_cap(conn, scope, None, content.chars().count(), core_cap)?;
    }
    let (kind, user, instance, conversation) = viewer.owner.columns();
    let at = to_millis(now);
    conn.execute(
        "INSERT INTO memories
             (content, owner_kind, owner_user, owner_instance, owner_conversation, shared, core,
              created_at, updated_at, source_session)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)",
        params![
            content,
            kind,
            user,
            instance,
            conversation,
            new.shared,
            new.core,
            at,
            new.source_session,
        ],
    )?;
    let id = MemoryId::new(conn.last_insert_rowid());
    fetch(conn, id)?.ok_or_else(|| Error::corrupt(format!("memory {id} vanished after insert")))
}

fn edit_memory(
    conn: &Connection,
    viewer: &MemoryViewer,
    id: MemoryId,
    edit: MemoryEdit,
    core_cap: usize,
    now: Timestamp,
) -> Result<Memory> {
    let memory = visible(conn, viewer, id)?;
    if memory.shared && !viewer.can_write_shared {
        return Err(Error::SharedWriteForbidden);
    }
    let shared = edit.shared.unwrap_or(memory.shared);
    if shared && !memory.shared && !viewer.can_write_shared {
        return Err(Error::SharedWriteForbidden);
    }
    if edit.content.is_none() && edit.core.is_none() && edit.shared.is_none() {
        return Err(Error::invalid("the edit changes nothing"));
    }
    let content = match &edit.content {
        Some(content) => check_content(content)?,
        None => memory.content.clone(),
    };
    let core = edit.core.unwrap_or(memory.core);
    // Un-sharing makes the memory private to whoever un-shared it, so it can never end up
    // private to nobody.
    let owner = if memory.shared && !shared {
        viewer.owner.clone()
    } else {
        memory.owner.clone()
    };
    if !shared && owner == MemoryOwner::None {
        return Err(Error::invalid(
            "a memory with no owner must be shared: nothing could read it back otherwise",
        ));
    }
    if core {
        enforce_core_cap(
            conn,
            scope_of(shared, &owner),
            Some(id),
            content.chars().count(),
            core_cap,
        )?;
    }
    let (kind, user, instance, conversation) = owner.columns();
    conn.execute(
        "UPDATE memories
         SET content = ?2, owner_kind = ?3, owner_user = ?4, owner_instance = ?5,
             owner_conversation = ?6, shared = ?7, core = ?8, updated_at = ?9
         WHERE id = ?1",
        params![
            id,
            content,
            kind,
            user,
            instance,
            conversation,
            shared,
            core,
            to_millis(now),
        ],
    )?;
    fetch(conn, id)?.ok_or_else(|| Error::corrupt(format!("memory {id} vanished after update")))
}

#[cfg(test)]
mod tests;
