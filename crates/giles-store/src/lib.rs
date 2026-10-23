//! Storage for the Giles core: the SQLite database and files under the data root.
//!
//! [`Store`] is the database (`data/giles.db`, PLAN 11.4): open it once with [`Store::open`],
//! clone the handle freely, and call its typed methods; callers never write SQL. The schema,
//! concurrency model, and migrations are described in `Docs/technical/storage.md`.
//!
//! Conventions shared by every method:
//!
//! - Times are [`Timestamp`]s, stored as Unix milliseconds, so they come back truncated to the
//!   millisecond. Methods that depend on the current time take it as a `now` argument.
//! - Row ids ([`SessionId`] and friends) are opaque newtypes over SQLite rowids.

mod db;
mod error;
mod messages;
mod migrate;
mod search;
mod sessions;
mod time;

pub use db::{OpenOptions, Store};
pub use error::{BoxError, Error, Result};
pub use messages::{
    Appended, Entry, History, Slot, StoredMessage, StoredMessageId, Summary, SummaryId,
};
pub use migrate::{MigrationPlan, SCHEMA_VERSION};
pub use search::{SNIPPET_ELLIPSIS, SNIPPET_END, SNIPPET_START, SearchHit, SearchScope};
pub use sessions::{Session, SessionId, SessionKey, SessionKind};

use giles_types::Timestamp;
use rusqlite::{Params, Row, Statement};

/// Defines a rowid newtype.
macro_rules! row_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(i64);

        impl $name {
            /// Wraps a raw rowid, e.g. one an admin passed on the command line.
            pub const fn new(raw: i64) -> Self {
                Self(raw)
            }

            /// The raw rowid.
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl rusqlite::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                self.0.to_sql()
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(
                value: rusqlite::types::ValueRef<'_>,
            ) -> rusqlite::types::FromSqlResult<Self> {
                i64::column_result(value).map(Self)
            }
        }
    };
}
pub(crate) use row_id;

/// Runs a prepared query and converts every row with `f`.
pub(crate) fn query_all<T>(
    stmt: &mut Statement<'_>,
    params: impl Params,
    mut f: impl FnMut(&Row<'_>) -> Result<T>,
) -> Result<Vec<T>> {
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(f(row)?);
    }
    Ok(out)
}

/// Runs a prepared query and converts its first row, if any, with `f`.
pub(crate) fn query_opt<T>(
    stmt: &mut Statement<'_>,
    params: impl Params,
    f: impl FnOnce(&Row<'_>) -> Result<T>,
) -> Result<Option<T>> {
    let mut rows = stmt.query(params)?;
    rows.next()?.map(f).transpose()
}

/// Parses a stored id column (`AgentId`, `UserId`, ...), mapping a bad value to
/// [`Error::Corrupt`].
pub(crate) fn parse_id<T, E: std::fmt::Display>(
    value: String,
    new: impl FnOnce(String) -> Result<T, E>,
) -> Result<T> {
    new(value).map_err(|e| Error::corrupt(e.to_string()))
}

/// Reads a millisecond timestamp column.
pub(crate) fn get_time(row: &Row<'_>, idx: usize) -> Result<Timestamp> {
    time::from_millis(row.get(idx)?)
}

/// Reads a nullable millisecond timestamp column.
pub(crate) fn get_opt_time(row: &Row<'_>, idx: usize) -> Result<Option<Timestamp>> {
    row.get::<_, Option<i64>>(idx)?
        .map(time::from_millis)
        .transpose()
}
