//! The store's error type.

use std::error::Error as StdError;

/// A boxed error from a caller-supplied hook.
pub type BoxError = Box<dyn StdError + Send + Sync>;

/// A store operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// SQLite reported an error.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// A file operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON column didn't hold what it should, or a value couldn't be serialized.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// The database was written by a newer Giles: its schema version is ahead of this binary's.
    #[error("database schema version {found} is newer than this binary supports ({supported})")]
    SchemaTooNew {
        /// The database's `user_version`.
        found: i64,
        /// The newest version this binary knows.
        supported: i64,
    },
    /// The hook run before migrations failed, so no migration was applied.
    #[error("pre-migration hook failed: {0}")]
    BeforeMigrate(#[source] BoxError),
    /// SQLite would not switch the database to the requested journal mode, e.g. WAL on a file
    /// system without shared-memory support, or leaving WAL while another process has the
    /// database open.
    #[error("cannot set the database's journal mode to {requested}: {reason}")]
    JournalMode {
        /// The mode that was asked for.
        requested: crate::JournalMode,
        /// What SQLite said.
        reason: String,
    },
    /// A hook the caller passed to a transactional method failed, so the transaction was rolled
    /// back and nothing changed.
    #[error("{0}")]
    Hook(#[source] BoxError),
    /// The referenced row doesn't exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The row exists but isn't in a state that allows the operation, e.g. filling a tool-result
    /// slot twice or starting an inbox row that isn't queued.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The caller passed a value the store can't hold, e.g. a count above `i64::MAX`.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// A stored value is malformed: an unknown enum string, an invalid id, a bad timestamp.
    #[error("corrupt row: {0}")]
    Corrupt(String),
    /// A blob is larger than the caller's limit. Nothing was stored.
    #[error("blob exceeds the {limit}-byte limit")]
    TooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// A blob's metadata exists but its file is missing or has the wrong size. Storing the same
    /// content again repairs it.
    #[error("blob {id} is damaged: {reason}")]
    BlobDamaged {
        /// The blob.
        id: giles_types::MediaId,
        /// What's wrong.
        reason: String,
    },
    /// A blocking task panicked or was cancelled by the runtime shutting down.
    #[error("blocking task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// The memory doesn't exist or isn't visible to the session. The two cases are
    /// indistinguishable on purpose, so ids of other scopes' memories leak nothing.
    #[error("unknown memory {0}")]
    UnknownMemory(crate::MemoryId),
    /// The session isn't allowed to create, mark, change, or delete a shared memory: only the
    /// owner's turns and config jobs are.
    #[error("only the owner and config jobs can write shared memories")]
    SharedWriteForbidden,
    /// The memory was changed after the caller loaded it: an edit that named the `updated_at` it
    /// was based on found a different one. Nothing changed; the caller reloads and decides again.
    #[error("memory {0} changed since it was loaded")]
    StaleMemory(crate::MemoryId),
    /// A toolset with that name exists already. Names are compared without regard to ASCII case.
    #[error("a toolset with that name already exists")]
    ToolsetExists,
    /// There is no toolset with that name.
    #[error("no such toolset")]
    NoSuchToolset,
    /// The toolset was changed after the caller loaded it: an update that named the `updated_at`
    /// it was based on found a different one. Nothing changed; the caller reloads and decides
    /// again.
    #[error("the toolset changed since it was loaded")]
    StaleToolset,
    /// A memory's content is longer than the limit.
    #[error("memory content is {chars} characters; the limit is {max}")]
    MemoryTooLong {
        /// The content's length in characters.
        chars: usize,
        /// The limit in characters.
        max: usize,
    },
    /// The write would take a scope's core memories over the cap. Nothing changed.
    #[error(
        "core memories would exceed the {}-character cap by {} characters",
        .0.cap, .0.overflow
    )]
    CoreCapExceeded(Box<crate::CoreOverflow>),
}

/// `Result` with the store's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub(crate) fn corrupt(what: impl Into<String>) -> Self {
        Self::Corrupt(what.into())
    }

    pub(crate) fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    pub(crate) fn conflict(what: impl Into<String>) -> Self {
        Self::Conflict(what.into())
    }

    pub(crate) fn invalid(what: impl Into<String>) -> Self {
        Self::InvalidRequest(what.into())
    }
}
