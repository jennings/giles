//! Instance names are permanent ids (PLAN section 7.2): sessions, the inbox, memory paths, and the
//! plugin's data directory are keyed by them. This module renames one ([`rename_instance`], behind
//! `giles instance rename <old> <new>`).
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

use giles_store::{BoxError, InstanceRows, RenamedRows, Store};

use crate::admin::InstanceLock;
use crate::config::{Kind, is_valid_name};
use crate::paths::DataRoot;

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
    /// How many `[[routes]]` of `giles.toml` named the old channel instance.
    pub routes: usize,
    /// The `agent.toml` files rewritten, with the number of references changed in each.
    pub agent_files: Vec<(PathBuf, usize)>,
    /// The directories and files moved, `(from, to)`.
    pub moved: Vec<(PathBuf, PathBuf)>,
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
    /// Neither name is configured and the old one has no data.
    #[error(
        "no instance named {old:?}: it is not configured in giles.toml and has no data (no \
         database rows, no directory under data/plugins/ or agents/*/groups/)"
    )]
    UnknownInstance {
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
    /// The old name has data, but neither name is configured, so the kind is unknown.
    #[error(
        "{old:?} has data but is not configured, and neither is {new:?}: add the instance to \
         giles.toml under the new name (`[<kind>.{new}]`, with the settings it had) and run this \
         command again to move the data"
    )]
    NotConfigured {
        /// The old name.
        old: String,
        /// The new name.
        new: String,
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

/// Renames the instance `old` to `new`: the database rows, the directories, and the instance's
/// table and references in the configuration. The caller proves the core is stopped by holding
/// `lock`, the data root's single-instance lock.
///
/// `old` is the name in `giles.toml` (`[<kind>.<old>]`), or, if the operator already renamed the
/// table by hand, the name the data still has; `new` must not have any data. The database rows
/// move in one transaction, and the file changes happen inside it before it commits. A failure
/// at any step undoes the earlier ones and changes nothing.
///
/// # Errors
///
/// A [`RenameError`]; every error but [`RenameError::Failed`] with `undo_errors` leaves the data
/// root as it was.
pub async fn rename_instance(
    root: &DataRoot,
    store: &Store,
    _lock: &InstanceLock,
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
    if old.eq_ignore_ascii_case(new) {
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
    if let Some(existing) = configured
        .keys()
        .find(|name| name.eq_ignore_ascii_case(new) && name.as_str() != new)
    {
        return Err(RenameError::CaseCollision {
            new: new.to_owned(),
            existing: existing.clone(),
        });
    }

    let rows = store.instance_rows().await.map_err(RenameError::Store)?;
    let dirs = dirs::scan_dirs(root).map_err(|(path, source)| RenameError::Io { path, source })?;
    let has_data = |name: &str| rows.contains_key(name) || dirs.contains_key(name);

    let kind = match (configured.get(old), configured.get(new)) {
        (Some(&old_kind), Some(&new_kind)) => {
            return Err(RenameError::BothConfigured {
                old: old.to_owned(),
                old_kind,
                new: new.to_owned(),
                new_kind,
            });
        }
        (Some(&kind), None) => kind,
        (None, Some(&kind)) if has_data(old) => kind,
        (None, _) if !has_data(old) => {
            return Err(RenameError::UnknownInstance {
                old: old.to_owned(),
            });
        }
        (None, _) => {
            return Err(RenameError::NotConfigured {
                old: old.to_owned(),
                new: new.to_owned(),
            });
        }
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
    // directories already at their new path ours. While `old` is still configured, a directory
    // named `new` belongs to something else.
    let resuming = rows.contains_key(old) && !configured.contains_key(old);
    if let (Some(found), false) = (dirs.get(new), resuming) {
        return Err(RenameError::NewHasData {
            new: new.to_owned(),
            what: describe_dirs(found),
        });
    }

    let plan = Plan::build(root, old, new, kind, &config_text, &dirs)?;
    let journal = Journal::default();
    let hook = {
        let (plan, journal) = (plan.clone(), journal.clone());
        move |_: &RenamedRows| -> Result<(), BoxError> {
            plan.apply(&journal).map_err(|(step, error)| {
                let undo_errors = journal.undo();
                Box::new(Failure {
                    step,
                    error,
                    undo_errors,
                }) as BoxError
            })
        }
    };
    match store.rename_instance(old, new, hook).await {
        Ok(rows) => Ok(RenameReport {
            kind,
            rows,
            config_table: plan.config_table,
            routes: plan.routes,
            agent_files: plan.agent_files.clone(),
            moved: plan
                .moves
                .iter()
                .map(|m| (m.from.clone(), m.to.clone()))
                .collect(),
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
                undo_errors: journal.undo(),
            }),
        },
        Err(giles_store::Error::Conflict(what)) => Err(RenameError::NewHasData {
            new: new.to_owned(),
            what,
        }),
        // A failure of the commit itself comes after the hook did its work.
        Err(error) => {
            let touched = !journal.is_untouched();
            Err(store_failure(error, touched, journal.undo()))
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
        config_text: &str,
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

        // The configuration last: it is what the startup check reads.
        let giles = edit::rename_in_giles_toml(config_text, kind, old, new)
            .map_err(|e| edit_error(&config_path, e))?;
        let reparsed = edit::parse(&giles.text).map_err(|e| edit_error(&config_path, e))?;
        let after = edit::configured(&reparsed);
        if after.get(new) != Some(&kind) || after.contains_key(old) {
            return Err(RenameError::Edit {
                path: config_path,
                reason: format!(
                    "could not rename [{kind}.{old}] to [{kind}.{new}] without changing \
                     anything else: it is written in a form this command doesn't edit; rename \
                     it by hand, then run the command again to move the data"
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

        let mut moves = Vec::new();
        for dir in dirs.get(old).into_iter().flatten() {
            let to = dir.path.with_file_name(new);
            if to.symlink_metadata().is_ok() {
                return Err(RenameError::NewHasData {
                    new: new.to_owned(),
                    what: describe_dirs(&[dirs::DataDir {
                        path: to,
                        group: dir.group,
                    }]),
                });
            }
            moves.push(Move {
                from: dir.path.clone(),
                to,
            });
        }
        // Disposable files named after the instance: moved if present, never a conflict.
        let soft = [
            (
                root.cache_dir()
                    .join("describe")
                    .join(format!("{old}.json")),
                root.cache_dir()
                    .join("describe")
                    .join(format!("{new}.json")),
            ),
            (
                root.cache_dir()
                    .join("toolsets")
                    .join(format!("{old}.json")),
                root.cache_dir()
                    .join("toolsets")
                    .join(format!("{new}.json")),
            ),
        ];
        let pids = (root.pid_file(old).ok(), root.pid_file(new).ok());
        let soft = soft.into_iter().chain(match pids {
            (Some(from), Some(to)) => Some((from, to)),
            _ => None,
        });
        for (from, to) in soft {
            if from.symlink_metadata().is_ok() && to.symlink_metadata().is_err() {
                moves.push(Move { from, to });
            }
        }

        Ok(Self {
            agent_files,
            files,
            moves,
            config_table: giles.table_renamed,
            routes: giles.routes,
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
