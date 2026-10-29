//! Instance names are permanent ids (PLAN section 7.2): sessions, the inbox, memory paths, and the
//! plugin's data directory are keyed by them. This module renames one ([`rename_instance`], behind
//! `giles channel rename`, `giles provider rename`, and `giles toolset rename <old> <new>`).
//!
//! Docs/technical/instance-rename.md describes what a name keys, the order of the steps, and what
//! a failure leaves behind.

mod dirs;
mod edit;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use giles_store::{
    BoxError, InstanceRows, MemoryStore, OpenOptions, RenamedRows, Store, StoredToolset,
};
use giles_types::Timestamp;

use crate::admin::InstanceLock;
use crate::config::{Kind, is_valid_name};
use crate::paths::DataRoot;
use crate::toolsets::RESERVED_TOOLSET_NAME;

pub use edit::GlobChange;
pub(crate) use edit::{ToolsetGlobs, rewrite_toolset_globs};

/// What [`rename_instance`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameReport {
    /// The instance's kind.
    pub kind: Kind,
    /// The database rows that moved to the new name.
    pub rows: RenamedRows,
    /// Whether the `[<kind>.<old>]` table of `giles.toml` was renamed. `false` when the operator
    /// had already renamed it by hand.
    pub config_table: bool,
    /// Whether the instance is a toolset stored in the database: its row was renamed (or
    /// already was, by a run that stopped early), and `giles.toml` has no table to rename.
    pub stored_toolset: bool,
    /// How many `[[routes]]` of `giles.toml` named the old channel instance. Giles imports those
    /// once and then ignores them; they are rewritten so a file not yet imported (the first start
    /// after an upgrade) imports the new name, and a leftover block stays consistent. The routes
    /// Giles stores moved with the database rows ([`RenamedRows::routes`]).
    pub legacy_routes: usize,
    /// The `agent.toml` files rewritten, with the number of references changed in each.
    pub agent_files: Vec<(PathBuf, usize)>,
    /// The directories and files moved, `(from, to)`.
    pub moved: Vec<(PathBuf, PathBuf)>,
    /// The agents' memory databases (`agents/<agent>/memory.db`) whose group owners moved to the
    /// new instance name, with the number of memories changed in each. Databases with nothing
    /// under the old name, including every one an interrupted run already did, aren't listed.
    pub memory_dbs: Vec<(PathBuf, usize)>,
}

/// A rename didn't happen.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RenameError {
    /// A name isn't `[a-zA-Z0-9-]+`.
    #[error("invalid instance name {name:?}: use only letters, digits, and `-`")]
    InvalidName {
        /// The name.
        name: String,
    },
    /// The names are the same, or differ only in case.
    #[error(
        "{old:?} and {new:?} are the same name: instance names are compared ignoring case, \
         because a case-insensitive file system would give both one data directory"
    )]
    SameName {
        /// The old name.
        old: String,
        /// The new name.
        new: String,
    },
    /// The old name isn't configured as the kind and has no data of it.
    #[error(
        "no {kind} named {old:?}: it is not configured in giles.toml and has no data (no \
         database rows or routes, no directory under data/{kind}/ or agents/*/groups/)"
    )]
    UnknownInstance {
        /// The kind being renamed.
        kind: Kind,
        /// The old name.
        old: String,
    },
    /// Both names are configured.
    #[error(
        "both [{old_kind}.{old}] and [{new_kind}.{new}] are in giles.toml: remove or rename one \
         of them first; renaming never merges two instances"
    )]
    BothConfigured {
        /// The old name.
        old: String,
        /// Its kind.
        old_kind: Kind,
        /// The new name.
        new: String,
        /// Its kind.
        new_kind: Kind,
    },
    /// The old name has data, but neither name is configured.
    #[error(
        "the {kind} {old:?} has data but is not configured, and neither is {new:?}: add it to \
         giles.toml under the new name (`[{kind}.{new}]`, with the settings it had) and run this \
         command again to move the data"
    )]
    NotConfigured {
        /// The kind being renamed.
        kind: Kind,
        /// The old name.
        old: String,
        /// The new name.
        new: String,
    },
    /// The old name is an instance of another kind (a toolset stored in the database counts).
    #[error(
        "{old:?} is a {actual}, not a {requested}: run `giles {actual} rename {old} {new}` \
         instead"
    )]
    WrongKind {
        /// The old name.
        old: String,
        /// The new name.
        new: String,
        /// The kind the command renames.
        requested: Kind,
        /// The kind `old` is.
        actual: Kind,
    },
    /// The new name differs only in case from another configured instance.
    #[error(
        "{new:?} differs only in case from the configured instance {existing:?}; instance names \
         are unique ignoring case"
    )]
    CaseCollision {
        /// The new name.
        new: String,
        /// The configured instance.
        existing: String,
    },
    /// A toolset stored in the database has the new name, which giles.toml would gain.
    #[error(
        "{new:?} is the name of the toolset {existing:?} stored in the database; instance names \
         are unique across kinds, ignoring case. Remove that toolset or choose another name"
    )]
    StoredToolset {
        /// The new name.
        new: String,
        /// The stored toolset's name.
        existing: String,
    },
    /// A stored toolset can't have the new name: the web UI and the admin RPCs refuse it too.
    #[error("a stored toolset can't be named {new:?}: {reason}")]
    StoredToolsetName {
        /// The new name.
        new: String,
        /// Why.
        reason: String,
    },
    /// The new name is configured, as some kind, so it can't take the data of the old one.
    #[error(
        "cannot rename the {kind} {old:?} to {new:?}: giles.toml configures [{new_kind}.{new}], \
         and instance names are unique across kinds, so remove or rename that table first; \
         renaming never merges two instances"
    )]
    NewIsConfigured {
        /// The kind being renamed.
        kind: Kind,
        /// The old name.
        old: String,
        /// The new name.
        new: String,
        /// The kind of the table that has it.
        new_kind: Kind,
    },
    /// The new name already has data.
    #[error(
        "{new:?} already has data ({what}); renaming never merges two instances' data. Choose \
         another name, or move that data away first"
    )]
    NewHasData {
        /// The new name.
        new: String,
        /// Where the data is.
        what: String,
    },
    /// A file couldn't be read, or the configuration couldn't be edited.
    #[error("cannot rename in {}: {reason}", .path.display())]
    Edit {
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// The data root couldn't be read.
    #[error("cannot read {}: {source}", .path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The database failed before any step was taken.
    #[error("database error: {0}")]
    Store(#[source] giles_store::Error),
    /// A step failed. Everything done before it was undone, except what `undo_errors` lists.
    #[error("{}", failed_message(.step, .error, .undo_errors))]
    Failed {
        /// The step that failed.
        step: String,
        /// Why.
        error: String,
        /// Changes that couldn't be undone.
        undo_errors: Vec<String>,
    },
}

fn failed_message(step: &str, error: &str, undo_errors: &[String]) -> String {
    if undo_errors.is_empty() {
        format!("{step}: {error}. Nothing was changed.")
    } else {
        format!(
            "{step}: {error}. Undoing the earlier steps failed too, so the rename is partly \
             done; fix these by hand, then run the command again: {}",
            undo_errors.join("; ")
        )
    }
}

/// Renames the `kind` instance `old` to `new`: the database rows, the directories, and the
/// instance's table and references in the configuration. The caller proves the core is stopped by
/// holding `lock`, the data root's single-instance lock.
///
/// `old` is the name in `giles.toml` (`[<kind>.<old>]`), or, if the operator already renamed the
/// table by hand, the name the data of that kind still has; `new` must not have any data of that
/// kind. A toolset stored in the database (made in the web UI) is renamed too, when `kind` is
/// [`Kind::Toolset`] and `giles.toml` doesn't configure `old`: its row is renamed instead of a
/// table, `old` may differ from the stored name in case, and `new` may differ from it in case
/// only. The database rows move in one transaction, and the file changes happen inside it before
/// it commits, ending, for a channel, with the group owners in each agent's memory database
/// (`agents/<agent>/memory.db`, if it has one). A failure at any step undoes the earlier ones and
/// changes nothing.
///
/// Only `kind`'s rows, directories and references are looked at or changed. A name that is
/// configured as another kind is refused ([`RenameError::WrongKind`]); instance names are still
/// unique across kinds.
///
/// # Errors
///
/// A [`RenameError`]; every error but [`RenameError::Failed`] with `undo_errors` leaves the data
/// root as it was.
pub async fn rename_instance(
    root: &DataRoot,
    store: &Store,
    _lock: &InstanceLock,
    kind: Kind,
    old: &str,
    new: &str,
) -> Result<RenameReport, RenameError> {
    for name in [old, new] {
        if !is_valid_name(name) {
            return Err(RenameError::InvalidName {
                name: name.to_owned(),
            });
        }
    }
    if old == new {
        return Err(RenameError::SameName {
            old: old.to_owned(),
            new: new.to_owned(),
        });
    }

    let config_path = root.config_file();
    let config_text = fs::read_to_string(&config_path).map_err(|source| RenameError::Io {
        path: config_path.clone(),
        source,
    })?;
    let doc = edit::parse(&config_text).map_err(|e| RenameError::Edit {
        path: config_path.clone(),
        reason: e.to_string(),
    })?;
    let configured = edit::configured(&doc);
    // Whether `giles.toml` configures `name` as an instance of `kind`, and as an instance of any
    // kind (names are unique across kinds).
    let configured_as = |name: &str| configured.contains(&(kind, name.to_owned()));
    let configured_kind = |name: &str| edit::kind_of(&configured, name);
    // A name that differs from `old` in case only is the same name, which is reported below.
    if let Some((_, existing)) = configured.iter().find(|(_, name)| {
        name.eq_ignore_ascii_case(new) && name != new && !name.eq_ignore_ascii_case(old)
    }) {
        return Err(RenameError::CaseCollision {
            new: new.to_owned(),
            existing: existing.clone(),
        });
    }

    let stored = store.toolsets().await.map_err(RenameError::Store)?;
    let rows = store
        .instance_rows(kind)
        .await
        .map_err(RenameError::Store)?;
    let dirs = dirs::scan_dirs(root).map_err(|(path, source)| RenameError::Io { path, source })?;
    let kind_dirs = dirs::of_kind(&dirs, kind);
    // Routes name a channel instance without being data, but a name only they use is still worth
    // renaming.
    let routes = if kind == Kind::Channel {
        store.instance_routes().await.map_err(RenameError::Store)?
    } else {
        BTreeMap::new()
    };
    let has_data = |name: &str| {
        rows.contains_key(name) || kind_dirs.contains_key(name) || routes.contains_key(name)
    };

    // The stored toolsets are instances too. The row is renamed before the agents' files are
    // rewritten, so a run that stopped in between left the row with `new` and the agents' globs
    // with `old`; running the command again finishes it. The names are compared exactly, and a
    // rename that only changes the case can't be told from a finished one, so it isn't resumed.
    let stored_resumed = kind == Kind::Toolset
        && configured_kind(old).is_none()
        && configured_kind(new).is_none()
        && !old.eq_ignore_ascii_case(new)
        && stored.iter().any(|t| t.name == new)
        && !stored
            .iter()
            .any(|t| t.name != new && t.name.eq_ignore_ascii_case(old));
    // Unless `giles.toml` configures `old`, a stored toolset of that name (ignoring case) is the
    // one to rename.
    let stored_old = if kind == Kind::Toolset && configured_kind(old).is_none() && !stored_resumed {
        stored.iter().find(|t| t.name.eq_ignore_ascii_case(old))
    } else {
        None
    };
    let stored_toolset = stored_old.is_some() || stored_resumed;
    // `old` isn't a `kind` instance, so say what it is. Not when the operator renamed the table
    // by hand: `old` is then a name this kind has data under, whatever else uses it now.
    let old_configured = configured_as(old);
    let data_left = !old_configured && configured_as(new) && has_data(old);
    if !old_configured && !data_left && !stored_toolset {
        let actual = configured_kind(old).or_else(|| {
            stored
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case(old))
                .then_some(Kind::Toolset)
        });
        if let Some(actual) = actual {
            return Err(RenameError::WrongKind {
                old: old.to_owned(),
                new: new.to_owned(),
                requested: kind,
                actual,
            });
        }
    }
    // From here on the name as stored.
    let old = stored_old.map_or(old, |row| row.name.as_str());
    // Names that differ only in case are the same name, but a stored toolset can change its case.
    if old == new || (old.eq_ignore_ascii_case(new) && !stored_toolset) {
        return Err(RenameError::SameName {
            old: old.to_owned(),
            new: new.to_owned(),
        });
    }
    if stored_old.is_some()
        && let Some(new_kind) = configured_kind(new)
    {
        return Err(RenameError::NewIsConfigured {
            kind,
            old: old.to_owned(),
            new: new.to_owned(),
            new_kind,
        });
    }
    // The rules a stored toolset's name has anywhere else. A resumed rename already passed them.
    if stored_old.is_some() && new.eq_ignore_ascii_case(RESERVED_TOOLSET_NAME) {
        return Err(RenameError::StoredToolsetName {
            new: new.to_owned(),
            reason: format!(
                "it is the address of the web form for a new toolset ({RESERVED_TOOLSET_NAME:?} \
                 in any case)"
            ),
        });
    }
    // A name a stored toolset has can't be given to a table of `giles.toml` (which would take the
    // name from it at the next start), or to another stored toolset.
    if configured_kind(new).is_none()
        && !stored_resumed
        && let Some(existing) = stored.iter().find(|t| {
            t.name.eq_ignore_ascii_case(new) && stored_old.is_none_or(|row| row.name != t.name)
        })
    {
        return Err(RenameError::StoredToolset {
            new: new.to_owned(),
            existing: existing.name.clone(),
        });
    }

    if !stored_toolset {
        if old_configured {
            if let Some(new_kind) = configured_kind(new) {
                return Err(RenameError::BothConfigured {
                    old: old.to_owned(),
                    old_kind: kind,
                    new: new.to_owned(),
                    new_kind,
                });
            }
        } else if !has_data(old) {
            return Err(RenameError::UnknownInstance {
                kind,
                old: old.to_owned(),
            });
        } else if !configured_as(new) {
            return Err(match configured_kind(new) {
                Some(new_kind) => RenameError::NewIsConfigured {
                    kind,
                    old: old.to_owned(),
                    new: new.to_owned(),
                    new_kind,
                },
                None => RenameError::NotConfigured {
                    kind,
                    old: old.to_owned(),
                    new: new.to_owned(),
                },
            });
        }
    }

    // Only a channel's groups own memories.
    let memory_dbs = if kind == Kind::Channel {
        memory_databases(root)?
    } else {
        Vec::new()
    };

    // `new` must have no data. The one exception is a rename that stopped part-way and is run
    // again: the database still has rows under `old`, and some directories already moved.
    if rows.contains_key(new) {
        return Err(RenameError::NewHasData {
            new: new.to_owned(),
            what: describe_rows(rows.get(new).copied().unwrap_or_default()),
        });
    }
    // An interrupted run rewrote `giles.toml` before it moved any directory, so only then are
    // directories already at their new path ours. While `old` is still configured as `kind`, a
    // directory named `new` belongs to something else. Rows and directories are per kind, so
    // `old` being configured as another kind doesn't matter.
    let resuming = rows.contains_key(old) && !configured_as(old);
    if let (Some(found), false) = (kind_dirs.get(new), resuming) {
        return Err(RenameError::NewHasData {
            new: new.to_owned(),
            what: describe_dirs(found),
        });
    }

    let plan = Plan::build(
        root,
        old,
        new,
        kind,
        (!stored_toolset).then_some(config_text.as_str()),
        &kind_dirs,
    )?;
    // A stored toolset that already has `new` is only a rename to finish if something is left
    // under `old`: data, or agents that still name it. Otherwise `old` is no instance.
    if stored_resumed && !has_data(old) && plan.files.is_empty() && plan.moves.is_empty() {
        return Err(RenameError::UnknownInstance {
            kind,
            old: old.to_owned(),
        });
    }
    // The row goes first, as `giles.toml` is rewritten before the agents' files and directories.
    if let Some(row) = stored_old {
        rename_stored_toolset(store, row, new).await?;
    }
    let journal = Journal::default();
    let memory = MemoryJournal::default();
    let hook = {
        let (plan, journal, memory) = (plan.clone(), journal.clone(), memory.clone());
        let (handle, old, new) = (
            tokio::runtime::Handle::current(),
            old.to_owned(),
            new.to_owned(),
        );
        // The hook runs on the blocking pool, where the memory databases (async) can be driven
        // with the runtime's handle.
        move |_: &RenamedRows| -> Result<(), BoxError> {
            let fail = |step: String, error: String| {
                let mut undo_errors = handle.block_on(memory.undo(&old, &new));
                undo_errors.extend(journal.undo());
                Box::new(Failure {
                    step,
                    error,
                    undo_errors,
                }) as BoxError
            };
            plan.apply(&journal)
                .map_err(|(step, error)| fail(step, error))?;
            // The memory databases come last: each commits by itself, so they are the one step
            // that can only be taken back by writing again.
            handle
                .block_on(memory.apply(&memory_dbs, &old, &new))
                .map_err(|(step, error)| fail(step, error))
        }
    };
    let mut result = match store.rename_instance(kind, old, new, hook).await {
        Ok(rows) => Ok(RenameReport {
            kind,
            rows,
            config_table: plan.config_table,
            stored_toolset,
            legacy_routes: plan.routes,
            agent_files: plan.agent_files.clone(),
            moved: plan
                .moves
                .iter()
                .map(|m| (m.from.clone(), m.to.clone()))
                .collect(),
            memory_dbs: memory.changed(),
        }),
        Err(giles_store::Error::Hook(error)) => match error.downcast::<Failure>() {
            Ok(failure) => Err(RenameError::Failed {
                step: failure.step,
                error: failure.error,
                undo_errors: failure.undo_errors,
            }),
            Err(other) => Err(RenameError::Failed {
                step: "renaming".to_owned(),
                error: other.to_string(),
                undo_errors: undo_all(&memory, &journal, old, new).await,
            }),
        },
        Err(giles_store::Error::Conflict(what)) => Err(RenameError::NewHasData {
            new: new.to_owned(),
            what,
        }),
        // A failure of the commit itself comes after the hook did its work.
        Err(error) => {
            let touched = !journal.is_untouched() || !memory.is_untouched();
            Err(store_failure(
                error,
                touched,
                undo_all(&memory, &journal, old, new).await,
            ))
        }
    };
    if let (Some(row), Err(error)) = (stored_old, &mut result)
        && let Err(e) = store.toolset_restore(new, row).await
    {
        let note = format!("put the stored toolset {:?} back: {e}", row.name);
        match error {
            RenameError::Failed { undo_errors, .. } => undo_errors.push(note),
            other => {
                *other = RenameError::Failed {
                    step: "renaming the database rows".to_owned(),
                    error: other.to_string(),
                    undo_errors: vec![note],
                };
            }
        }
    }
    result
}

/// Renames the stored toolset `row` to `new`, and puts it back if that fails part-way.
async fn rename_stored_toolset(
    store: &Store,
    row: &StoredToolset,
    new: &str,
) -> Result<(), RenameError> {
    match store.toolset_rename(&row.name, new, Timestamp::now()).await {
        Ok(_) => Ok(()),
        Err(error) => {
            let undo = store.toolset_restore(&row.name, row).await;
            Err(store_failure(
                error,
                false,
                undo.err()
                    .map(|e| format!("put the stored toolset {:?} back: {e}", row.name))
                    .into_iter()
                    .collect(),
            ))
        }
    }
}

/// The error for a database failure that isn't a refusal: [`RenameError::Store`] if no step had
/// been taken, else a failed step (the commit), with what couldn't be undone.
fn store_failure(
    error: giles_store::Error,
    touched: bool,
    undo_errors: Vec<String>,
) -> RenameError {
    if touched || !undo_errors.is_empty() {
        RenameError::Failed {
            step: "committing the database".to_owned(),
            error: error.to_string(),
            undo_errors,
        }
    } else {
        RenameError::Store(error)
    }
}

fn describe_rows(rows: InstanceRows) -> String {
    format!(
        "{} sessions, {} inbox rows, {} outbox rows, {} usage rows in the database",
        rows.sessions, rows.inbox, rows.outbox, rows.usage
    )
}

fn describe_dirs(dirs: &[dirs::DataDir]) -> String {
    let paths: Vec<_> = dirs.iter().map(|d| d.path.display().to_string()).collect();
    format!("directories {}", paths.join(", "))
}

/// A step failed inside the database transaction; carried through the store's hook error.
#[derive(Debug)]
struct Failure {
    step: String,
    error: String,
    undo_errors: Vec<String>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.error)
    }
}

impl std::error::Error for Failure {}

/// A file whose text changes.
#[derive(Debug, Clone)]
struct StagedFile {
    path: PathBuf,
    original: String,
    text: String,
    /// References changed in it (for the report).
    changes: usize,
}

/// A directory or file that moves.
#[derive(Debug, Clone)]
struct Move {
    from: PathBuf,
    to: PathBuf,
}

/// Everything a rename will do to the files, decided before anything changes.
#[derive(Debug, Clone)]
struct Plan {
    /// `agent.toml` files first, `giles.toml` last.
    files: Vec<StagedFile>,
    moves: Vec<Move>,
    /// The agent files in `files`, with the references changed in each.
    agent_files: Vec<(PathBuf, usize)>,
    config_table: bool,
    routes: usize,
}

impl Plan {
    fn build(
        root: &DataRoot,
        old: &str,
        new: &str,
        kind: Kind,
        config_text: Option<&str>,
        dirs: &BTreeMap<String, Vec<dirs::DataDir>>,
    ) -> Result<Self, RenameError> {
        let config_path = root.config_file();
        let edit_error = |path: &Path, e: edit::EditError| RenameError::Edit {
            path: path.to_owned(),
            reason: e.to_string(),
        };
        let mut files = Vec::new();

        // Agents that name the instance.
        let agents = root.agents_dir();
        for (_, dir) in sorted_dirs(&agents)? {
            let path = dir.join("agent.toml");
            let original = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(source) => return Err(RenameError::Io { path, source }),
            };
            if let Some((text, changes)) = edit::rename_in_agent_toml(&original, kind, old, new)
                .map_err(|e| edit_error(&path, e))?
            {
                files.push(StagedFile {
                    path,
                    original,
                    text,
                    changes,
                });
            }
        }

        let agent_files = files.iter().map(|f| (f.path.clone(), f.changes)).collect();

        // The configuration last. A stored toolset has no table in it (`config_text` is `None`):
        // its row is renamed in the database.
        let (mut config_table, mut routes) = (false, 0);
        if let Some(config_text) = config_text {
            let giles = edit::rename_in_giles_toml(config_text, kind, old, new)
                .map_err(|e| edit_error(&config_path, e))?;
            let reparsed = edit::parse(&giles.text).map_err(|e| edit_error(&config_path, e))?;
            let after = edit::configured(&reparsed);
            if !after.contains(&(kind, new.to_owned())) || after.contains(&(kind, old.to_owned())) {
                return Err(RenameError::Edit {
                    path: config_path,
                    reason: format!(
                        "could not rename [{kind}.{old}] to [{kind}.{new}] without changing \
                         anything else: it is written in a form this command doesn't edit; \
                         rename it by hand, then run the command again to move the data"
                    ),
                });
            }
            if giles.text != config_text {
                files.push(StagedFile {
                    path: config_path,
                    original: config_text.to_owned(),
                    text: giles.text,
                    changes: giles.routes,
                });
            }
            (config_table, routes) = (giles.table_renamed, giles.routes);
        }

        let mut moves = Vec::new();
        for dir in dirs.get(old).into_iter().flatten() {
            let to = dir.path.with_file_name(new);
            if to.symlink_metadata().is_ok() {
                return Err(RenameError::NewHasData {
                    new: new.to_owned(),
                    what: describe_dirs(&[dirs::DataDir {
                        path: to,
                        kind: dir.kind,
                    }]),
                });
            }
            moves.push(Move {
                from: dir.path.clone(),
                to,
            });
        }
        // Disposable files named after the instance: moved if present, never a conflict.
        let cache = root.cache_dir();
        let describe = cache.join("describe").join(kind.as_str());
        let mut soft = vec![(
            describe.join(format!("{old}.json")),
            describe.join(format!("{new}.json")),
        )];
        if kind == Kind::Toolset {
            let tools = cache.join("toolsets");
            soft.push((
                tools.join(format!("{old}.json")),
                tools.join(format!("{new}.json")),
            ));
        }
        if let (Ok(from), Ok(to)) = (root.pid_file(kind, old), root.pid_file(kind, new)) {
            soft.push((from, to));
        }
        for (from, to) in soft {
            if from.symlink_metadata().is_ok() && to.symlink_metadata().is_err() {
                moves.push(Move { from, to });
            }
        }

        Ok(Self {
            agent_files,
            files,
            moves,
            config_table,
            routes,
        })
    }

    /// Does every step, recording each in `journal` as it completes. On failure returns the
    /// step's description and the error; the caller undoes the journal.
    fn apply(&self, journal: &Journal) -> Result<(), (String, String)> {
        for file in &self.files {
            write_atomic(&file.path, &file.text)
                .map_err(|e| (format!("rewriting {}", file.path.display()), e.to_string()))?;
            journal.push(Applied::File {
                path: file.path.clone(),
                original: file.original.clone(),
            });
        }
        for step in &self.moves {
            fs::rename(&step.from, &step.to).map_err(|e| {
                (
                    format!("moving {} to {}", step.from.display(), step.to.display()),
                    e.to_string(),
                )
            })?;
            journal.push(Applied::Move {
                from: step.from.clone(),
                to: step.to.clone(),
            });
        }
        Ok(())
    }
}

/// A completed step, enough to take it back.
#[derive(Debug)]
enum Applied {
    File { path: PathBuf, original: String },
    Move { from: PathBuf, to: PathBuf },
}

/// The steps taken so far, shared between the hook that takes them and the code that undoes them
/// when the commit after the hook fails.
#[derive(Debug, Clone, Default)]
struct Journal(Arc<Mutex<Vec<Applied>>>);

impl Journal {
    fn push(&self, step: Applied) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(step);
    }

    fn is_untouched(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    /// Takes every recorded step back, newest first, and returns what couldn't be undone.
    fn undo(&self) -> Vec<String> {
        let steps = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
        let mut errors = Vec::new();
        for step in steps.into_iter().rev() {
            match step {
                Applied::File { path, original } => {
                    if let Err(e) = write_atomic(&path, &original) {
                        errors.push(format!("restore {}: {e}", path.display()));
                    }
                }
                Applied::Move { from, to } => {
                    if let Err(e) = fs::rename(&to, &from) {
                        errors.push(format!(
                            "move {} back to {}: {e}",
                            to.display(),
                            from.display()
                        ));
                    }
                }
            }
        }
        errors
    }
}

/// The agents' memory databases a rename updates: every `agents/<agent>/memory.db` that exists.
/// An agent without one is skipped (the rename never creates one), and an agent removed in the
/// web UI lives in `trash/`, outside `agents/`, so its database is left as it is.
fn memory_databases(root: &DataRoot) -> Result<Vec<PathBuf>, RenameError> {
    Ok(sorted_dirs(&root.agents_dir())?
        .into_iter()
        .map(|(_, dir)| dir.join("memory.db"))
        .filter(|path| path.is_file())
        .collect())
}

/// The memory databases whose group owners were renamed, with how many memories changed in
/// each, shared between the hook that renames them and the code that undoes them. Each database
/// commits by itself, so taking one back means renaming the owners back in it.
#[derive(Debug, Clone, Default)]
struct MemoryJournal(Arc<Mutex<Vec<(PathBuf, usize)>>>);

impl MemoryJournal {
    fn push(&self, path: PathBuf, changed: usize) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((path, changed));
    }

    fn is_untouched(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    /// The databases changed so far, with the memories changed in each.
    fn changed(&self) -> Vec<(PathBuf, usize)> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Renames the group owners of `old` to `new` in each database in turn, recording the ones
    /// that changed. A database is opened in the journal mode it has: the core is stopped, and
    /// the rename must not undo the daemon's choice. On failure returns the step's description
    /// and the error; that database is unchanged (one transaction), the caller undoes the rest.
    async fn apply(&self, dbs: &[PathBuf], old: &str, new: &str) -> Result<(), (String, String)> {
        for path in dbs {
            let step = || format!("renaming the group owners in {}", path.display());
            let store = MemoryStore::open(path.clone(), OpenOptions::new().keep_journal_mode())
                .await
                .map_err(|e| (step(), e.to_string()))?;
            let changed = store
                .rename_instance(old, new)
                .await
                .map_err(|e| (step(), e.to_string()))?;
            if changed > 0 {
                self.push(path.clone(), changed);
            }
        }
        Ok(())
    }

    /// Renames the group owners back, newest database first, and returns what couldn't be taken
    /// back.
    async fn undo(&self, old: &str, new: &str) -> Vec<String> {
        let done = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
        let mut errors = Vec::new();
        for (path, _) in done.into_iter().rev() {
            let back = async {
                let store =
                    MemoryStore::open(path.clone(), OpenOptions::new().keep_journal_mode()).await?;
                store.rename_instance(new, old).await
            };
            if let Err(e) = back.await {
                errors.push(format!(
                    "rename the group owners back in {}: {e}",
                    path.display()
                ));
            }
        }
        errors
    }
}

/// Takes every step back: the memory databases, which came last, then the files and moves.
async fn undo_all(memory: &MemoryJournal, journal: &Journal, old: &str, new: &str) -> Vec<String> {
    let mut errors = memory.undo(old, new).await;
    errors.extend(journal.undo());
    errors
}

/// Replaces `path` with `contents` all at once (a temporary file in the same directory, renamed
/// over it), keeping its permissions.
fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(contents.as_bytes())?;
    file.as_file().sync_all()?;
    if let Ok(meta) = fs::metadata(path) {
        file.as_file().set_permissions(meta.permissions())?;
    }
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// The sub-directories of `dir`, by name; none if `dir` doesn't exist.
fn sorted_dirs(dir: &Path) -> Result<Vec<(String, PathBuf)>, RenameError> {
    dirs::list_dirs(dir).map_err(|source| RenameError::Io {
        path: dir.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests;
