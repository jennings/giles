//! Memory files (PLAN sections 6.7 and 11.3): Markdown with optional YAML front-matter and entries
//! separated by a line holding only `§`.
//!
//! [`MemoryDoc`] is the pure part: parsing, rendering, character counts, and applying a batch of
//! [`MemoryOp`]s under the cap. [`read`] and [`apply`] add the file I/O: a missing file is empty
//! memory, and writes hold a per-file lock and replace the file atomically (write to a temporary
//! file in the same directory, fsync, rename, fsync the directory), so concurrent batches from
//! several sessions never lose each other's writes.
//!
//! The cap is the agent's `memory_cap` ([`AgentConfig::memory_cap`](crate::config::AgentConfig),
//! default [`DEFAULT_MEMORY_CAP`](crate::config::DEFAULT_MEMORY_CAP)), in characters.

use std::collections::HashMap;
use std::fs::{self, DirBuilder, File};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::front_matter;
use crate::paths::{self, DataRoot, PathsError};

/// The content of the line that separates entries.
pub const ENTRY_SEPARATOR: &str = "§";

/// What joins two entries when a memory file is rewritten, and what each join counts toward the
/// cap.
const JOIN: &str = "\n§\n";

/// Mode of directories created for memory files, like the rest of the data root.
const PRIVATE_DIR_MODE: u32 = 0o700;

/// `agents/<agent>/MEMORY.md`: agent-wide memory.
///
/// # Errors
///
/// [`PathsError::InvalidName`] if `agent` is not a valid name.
pub fn agent_memory_file(root: &DataRoot, agent: &str) -> Result<PathBuf, PathsError> {
    Ok(root.agent_dir(agent)?.join("MEMORY.md"))
}

/// `agents/<agent>/users/<user>/MEMORY.md`: what this agent remembers about this user.
///
/// # Errors
///
/// [`PathsError::InvalidName`] if `agent` or `user` is not a valid name.
pub fn user_memory_file(root: &DataRoot, agent: &str, user: &str) -> Result<PathBuf, PathsError> {
    if !paths::is_valid_name(user) {
        return Err(PathsError::InvalidName {
            name: user.to_owned(),
        });
    }
    Ok(root
        .agent_dir(agent)?
        .join("users")
        .join(user)
        .join("MEMORY.md"))
}

/// `users/<user>/USER.md`: the user's profile. People write it; the model only reads it.
///
/// # Errors
///
/// [`PathsError::InvalidName`] if `user` is not a valid name.
pub fn user_profile_file(root: &DataRoot, user: &str) -> Result<PathBuf, PathsError> {
    Ok(root.user_dir(user)?.join("USER.md"))
}

/// One operation in a `memory` tool batch.
///
/// Serialized with an `op` tag: `{"op": "patch", "old_text": "...", "content": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[non_exhaustive]
pub enum MemoryOp {
    /// Appends a new entry.
    Add {
        /// The entry's text.
        content: String,
    },
    /// Replaces the one place `old_text` occurs, which lies inside a single entry, with `content`.
    /// An entry left empty is removed.
    Patch {
        /// Text that occurs exactly once in the memory.
        old_text: String,
        /// The replacement; may be empty.
        content: String,
    },
    /// Removes the one entry that contains `old_text`.
    Remove {
        /// Text that exactly one entry contains.
        old_text: String,
    },
}

impl MemoryOp {
    /// The operation's name: `add`, `patch`, or `remove`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Add { .. } => "add",
            Self::Patch { .. } => "patch",
            Self::Remove { .. } => "remove",
        }
    }
}

/// Why a batch was rejected. Nothing is written when a batch fails. `index` is the operation's
/// position in the batch, counting from 0 like the tool's `operations` array.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BatchError {
    /// The new entry is empty or only whitespace.
    #[error("operations[{index}] ({op}): content is empty")]
    EmptyContent {
        /// The operation's position.
        index: usize,
        /// The operation's name.
        op: &'static str,
    },
    /// `old_text` is empty.
    #[error("operations[{index}] ({op}): old_text is empty")]
    EmptyOldText {
        /// The operation's position.
        index: usize,
        /// The operation's name.
        op: &'static str,
    },
    /// The resulting entry would contain a line holding only `§`, which would split it.
    #[error(
        "operations[{index}] ({op}): an entry can't contain a line holding only `§`; that line \
         separates entries (use one `add` per entry)"
    )]
    ContainsSeparator {
        /// The operation's position.
        index: usize,
        /// The operation's name.
        op: &'static str,
    },
    /// No entry contains `old_text`.
    #[error("operations[{index}] ({op}): no entry contains old_text")]
    NoMatch {
        /// The operation's position.
        index: usize,
        /// The operation's name.
        op: &'static str,
    },
    /// `old_text` matches more than once: `count` places for `patch` (overlapping occurrences
    /// count), `count` entries for `remove`.
    #[error(
        "operations[{index}] ({op}): old_text matches {count} {}; include more of the text so it \
         matches exactly one",
        if *.op == "remove" { "entries" } else { "places" }
    )]
    Ambiguous {
        /// The operation's position.
        index: usize,
        /// The operation's name.
        op: &'static str,
        /// How many matches.
        count: usize,
    },
    /// The batch's result is over the cap. Carries the memory as it is (unchanged) so the model can
    /// consolidate in its next batch.
    #[error(
        "the batch would bring memory to {}/{} characters, {} over the cap; nothing was changed. \
         Make room by merging, shortening, or removing entries, in one batch",
        group_digits(*.cap + *.overflow),
        group_digits(*.cap),
        group_digits(*.overflow)
    )]
    OverCap {
        /// The current entries, unchanged.
        entries: Vec<String>,
        /// The current size in characters ([`MemoryDoc::chars`]).
        used: usize,
        /// The cap.
        cap: usize,
        /// How many characters the result is over the cap.
        overflow: usize,
    },
}

/// Errors from reading or updating a memory file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MemoryError {
    /// A filesystem operation failed (including a file that isn't UTF-8).
    #[error("cannot {action} {}: {error}", .path.display())]
    Io {
        /// What was being done, e.g. "read".
        action: &'static str,
        /// The path it was done to.
        path: PathBuf,
        /// The underlying error.
        error: io::Error,
    },
    /// The batch was rejected; the file is unchanged.
    #[error(transparent)]
    Batch(#[from] BatchError),
}

/// A parsed memory file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryDoc {
    front_matter: Option<String>,
    entries: Vec<String>,
}

impl MemoryDoc {
    /// Parses a memory file.
    ///
    /// The front-matter, if any, is kept verbatim. The rest is split at lines holding only `§`
    /// (surrounding spaces and tabs allowed). Line endings are normalized to `\n`; each entry loses
    /// its leading blank lines and trailing whitespace, and empty entries are dropped.
    pub fn parse(text: &str) -> Self {
        let (front_matter, body) = match front_matter::split(text) {
            Some(split) => (Some(split.raw.to_owned()), split.body),
            None => (None, text.strip_prefix('\u{feff}').unwrap_or(text)),
        };
        let body = body.replace("\r\n", "\n");
        let mut entries = Vec::new();
        let mut current = String::new();
        for line in body.split_inclusive('\n') {
            if is_separator(line) {
                push_entry(&mut entries, &current);
                current.clear();
            } else {
                current.push_str(line);
            }
        }
        push_entry(&mut entries, &current);
        Self {
            front_matter,
            entries,
        }
    }

    /// The front-matter block, verbatim, including its `---` lines.
    pub fn front_matter(&self) -> Option<&str> {
        self.front_matter.as_deref()
    }

    /// The entries, in file order.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// The entries joined by `\n§\n`: the memory as the model sees it, without front-matter.
    pub fn body(&self) -> String {
        self.entries.join(JOIN)
    }

    /// The size that counts toward the cap: characters (Unicode scalar values) of [`Self::body`].
    pub fn chars(&self) -> usize {
        let separators = self.entries.len().saturating_sub(1) * JOIN.chars().count();
        self.entries
            .iter()
            .map(|e| e.chars().count())
            .sum::<usize>()
            + separators
    }

    /// The usage header for the prompt, e.g. `[67% — 1,474/2,200]` ([`usage_header`]).
    pub fn usage_header(&self, cap: usize) -> String {
        usage_header(self.chars(), cap)
    }

    /// The file contents: the front-matter verbatim (plus a line ending if it had none and entries
    /// follow), then the body and a final newline.
    ///
    /// Without front-matter, a body that would read back as front-matter (its first entry starts
    /// with a `---` line and a later line is `---`, e.g. a thematic break or a setext heading) or
    /// would lose a leading byte-order mark is preceded by a `§` line, which parses as an empty
    /// entry and is dropped. So `parse(render(doc))` always gives back the same entries.
    pub fn render(&self) -> String {
        let mut out = self.front_matter.clone().unwrap_or_default();
        if !self.entries.is_empty() {
            let body = self.body();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            } else if front_matter::split(&body).is_some() || body.starts_with('\u{feff}') {
                out.push_str(ENTRY_SEPARATOR);
                out.push('\n');
            }
            out.push_str(&body);
            out.push('\n');
        }
        out
    }

    /// Applies `ops` in order, each seeing the previous ones' result, and returns the new memory.
    ///
    /// The batch is all or nothing: the first failing operation rejects it. The cap is checked on
    /// the final result only, so a batch may pass through an over-cap state, and a memory already
    /// over the cap (people's text is never truncated) accepts only batches that end at or under
    /// it.
    ///
    /// # Errors
    ///
    /// The first [`BatchError`].
    pub fn apply(&self, ops: &[MemoryOp], cap: usize) -> Result<Self, BatchError> {
        let mut entries = self.entries.clone();
        for (index, op) in ops.iter().enumerate() {
            let name = op.name();
            match op {
                MemoryOp::Add { content } => {
                    let content = normalize(content);
                    let entry = trim_entry(&content);
                    if entry.is_empty() {
                        return Err(BatchError::EmptyContent { index, op: name });
                    }
                    check_no_separator(entry, index, name)?;
                    entries.push(entry.to_owned());
                }
                MemoryOp::Patch { old_text, content } => {
                    let old_text = non_empty_old_text(old_text, index, name)?;
                    let (at, start) = unique_span(&entries, &old_text, index, name)?;
                    let entry = &entries[at];
                    let patched = format!(
                        "{}{}{}",
                        &entry[..start],
                        normalize(content),
                        &entry[start + old_text.len()..]
                    );
                    let patched = trim_entry(&patched);
                    check_no_separator(patched, index, name)?;
                    if patched.is_empty() {
                        entries.remove(at);
                    } else {
                        entries[at] = patched.to_owned();
                    }
                }
                MemoryOp::Remove { old_text } => {
                    let old_text = non_empty_old_text(old_text, index, name)?;
                    let mut matching = entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.contains(old_text.as_str()));
                    let Some((at, _)) = matching.next() else {
                        return Err(BatchError::NoMatch { index, op: name });
                    };
                    let others = matching.count();
                    if others > 0 {
                        return Err(BatchError::Ambiguous {
                            index,
                            op: name,
                            count: others + 1,
                        });
                    }
                    entries.remove(at);
                }
            }
        }

        let result = Self {
            front_matter: self.front_matter.clone(),
            entries,
        };
        let size = result.chars();
        if size > cap {
            return Err(BatchError::OverCap {
                entries: self.entries.clone(),
                used: self.chars(),
                cap,
                overflow: size - cap,
            });
        }
        Ok(result)
    }
}

/// Whether `line` (with its line ending) separates entries.
fn is_separator(line: &str) -> bool {
    line.trim_matches([' ', '\t', '\r', '\n']) == ENTRY_SEPARATOR
}

fn push_entry(entries: &mut Vec<String>, text: &str) {
    let entry = trim_entry(text);
    if !entry.is_empty() {
        entries.push(entry.to_owned());
    }
}

/// `text` without leading whitespace-only lines and trailing whitespace. Leading spaces on the first
/// non-blank line are kept (they may start an indented code block).
fn trim_entry(text: &str) -> &str {
    let text = text.trim_end();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        if !line.trim().is_empty() {
            break;
        }
        start += line.len();
    }
    &text[start..]
}

/// Model-supplied text with `\r\n` line endings normalized, to match the parsed file.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn non_empty_old_text(
    old_text: &str,
    index: usize,
    op: &'static str,
) -> Result<String, BatchError> {
    if old_text.is_empty() {
        return Err(BatchError::EmptyOldText { index, op });
    }
    Ok(normalize(old_text))
}

fn check_no_separator(entry: &str, index: usize, op: &'static str) -> Result<(), BatchError> {
    if entry.split('\n').any(is_separator) {
        return Err(BatchError::ContainsSeparator { index, op });
    }
    Ok(())
}

/// The entry and byte offset of the only occurrence of `needle`. Overlapping occurrences count
/// separately: `aa` occurs twice in `aaa`.
fn unique_span(
    entries: &[String],
    needle: &str,
    index: usize,
    op: &'static str,
) -> Result<(usize, usize), BatchError> {
    let mut found = None;
    let mut count = 0;
    for (at, entry) in entries.iter().enumerate() {
        let mut from = 0;
        while let Some(offset) = entry[from..].find(needle) {
            let start = from + offset;
            count += 1;
            found.get_or_insert((at, start));
            from = start + entry[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
    match (found, count) {
        (Some(span), 1) => Ok(span),
        (None, _) => Err(BatchError::NoMatch { index, op }),
        (Some(_), count) => Err(BatchError::Ambiguous { index, op, count }),
    }
}

/// The usage header shown above a memory file in the prompt: `[67% — 1,474/2,200]`.
///
/// The percentage is rounded down, so 100% means at the cap; an over-cap file shows more than
/// 100%. With a cap of 0, empty memory is 0% and anything else 100%.
pub fn usage_header(used: usize, cap: usize) -> String {
    let percent = match cap {
        0 if used == 0 => 0,
        0 => 100,
        cap => used.saturating_mul(100) / cap,
    };
    format!(
        "[{percent}% — {}/{}]",
        group_digits(used),
        group_digits(cap)
    )
}

/// `n` with commas between groups of three digits: `1,474`.
fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// One lock per memory file in this process, keyed by canonical path. Entries nobody holds are
/// dropped on the next lookup.
static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

fn file_lock(key: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = LOCKS.lock().unwrap_or_else(PoisonError::into_inner);
    locks.retain(|_, lock| Arc::strong_count(lock) > 1);
    Arc::clone(locks.entry(key.to_owned()).or_default())
}

/// Reads a memory file (or `USER.md`). A missing file is empty memory.
///
/// Reads take no lock: writes replace the file by rename, so a reader sees the old or the new
/// version, never a mix.
///
/// # Errors
///
/// [`MemoryError::Io`] if the file exists but can't be read or isn't UTF-8.
pub async fn read(path: &Path) -> Result<MemoryDoc, MemoryError> {
    let path = path.to_owned();
    blocking(move || read_doc(&path)).await
}

/// Applies a batch to the memory file at `path` under the per-file lock and returns the new
/// memory. The file is written only if the batch changes it; parent directories are created as
/// needed. If `path` is a symbolic link, its target is updated.
///
/// The write runs to completion even if the returned future is dropped, and the lock is held until
/// it finishes.
///
/// # Errors
///
/// [`MemoryError::Batch`] if the batch is rejected ([`MemoryDoc::apply`]); [`MemoryError::Io`] if
/// reading or writing fails. Either way the file is unchanged.
pub async fn apply(path: &Path, ops: Vec<MemoryOp>, cap: usize) -> Result<MemoryDoc, MemoryError> {
    let path = path.to_owned();
    let key = blocking(move || {
        resolve(&path).map_err(|error| MemoryError::Io {
            action: "resolve",
            path,
            error,
        })
    })
    .await?;
    let guard = file_lock(&key).lock_owned().await;
    blocking(move || {
        let _guard = guard;
        let doc = read_doc(&key)?;
        let updated = doc.apply(&ops, cap)?;
        if updated != doc {
            write_atomic(&key, updated.render().as_bytes())?;
        }
        Ok(updated)
    })
    .await
}

/// Runs blocking file I/O off the async threads.
async fn blocking<T, F>(f: F) -> Result<T, MemoryError>
where
    F: FnOnce() -> Result<T, MemoryError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => match e.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(e) => Err(MemoryError::Io {
                action: "access",
                path: PathBuf::new(),
                error: io::Error::other(e),
            }),
        },
    }
}

fn read_doc(path: &Path) -> Result<MemoryDoc, MemoryError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(MemoryDoc::parse(&text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(MemoryDoc::default()),
        Err(error) => Err(MemoryError::Io {
            action: "read",
            path: path.to_owned(),
            error,
        }),
    }
}

/// The canonical path of `path`, which need not exist: the longest existing ancestor is
/// canonicalized and the rest appended. Two spellings of one file give the same lock key.
///
/// A symbolic link at `path` is followed even when its target doesn't exist yet, so the first write
/// creates the target and the link survives.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    let path = follow_links(&std::path::absolute(path)?)?;
    let mut existing = path.as_path();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut real) => {
                real.extend(missing.iter().rev());
                return Ok(real);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
                    return Err(e);
                };
                missing.push(name);
                existing = parent;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Follows symbolic links at `path` itself (not its ancestors), up to a limit; a cycle is left for
/// `canonicalize` to report. A relative target is taken relative to the link's canonical parent
/// directory, with `.` and `..` resolved lexically.
fn follow_links(path: &Path) -> io::Result<PathBuf> {
    const MAX_LINKS: usize = 40;
    let mut path = path.to_owned();
    for _ in 0..MAX_LINKS {
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&path)?;
                let parent = match path.parent() {
                    Some(parent) => fs::canonicalize(parent)?,
                    None => PathBuf::from("/"),
                };
                path = normalize_lexically(&parent.join(target));
            }
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(e),
        }
    }
    Ok(path)
}

/// `path` with `.` dropped and `..` removing the previous component, without touching the disk.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Replaces `path` with `contents`: a temporary file in the same directory (keeping an existing
/// file's permissions), fsync, rename over `path`, then fsync the directory.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), MemoryError> {
    let io_error = |action, path: &Path| {
        let path = path.to_owned();
        move |error| MemoryError::Io {
            action,
            path,
            error,
        }
    };
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        let error = io::Error::new(io::ErrorKind::InvalidInput, "not a file path");
        return Err(MemoryError::Io {
            action: "write",
            path: path.to_owned(),
            error,
        });
    };
    DirBuilder::new()
        .recursive(true)
        .mode(PRIVATE_DIR_MODE)
        .create(dir)
        .map_err(io_error("create directory", dir))?;

    let mut prefix = std::ffi::OsString::from(".");
    prefix.push(name);
    prefix.push(".");
    let mut temp = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(io_error("create a temporary file in", dir))?;
    match fs::metadata(path) {
        Ok(meta) => temp
            .as_file()
            .set_permissions(meta.permissions())
            .map_err(io_error("set permissions on", temp.path()))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(MemoryError::Io {
                action: "inspect",
                path: path.to_owned(),
                error,
            });
        }
    }
    temp.write_all(contents)
        .map_err(io_error("write", temp.path()))?;
    temp.as_file()
        .sync_all()
        .map_err(io_error("sync", temp.path()))?;
    temp.persist(path).map_err(|e| MemoryError::Io {
        action: "replace",
        path: path.to_owned(),
        error: e.error,
    })?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_error("sync directory", dir))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// The cap callers pass by default: `AgentConfig::memory_cap` defaults to it.
    const DEFAULT_MEMORY_CAP: usize = crate::config::DEFAULT_MEMORY_CAP as usize;

    fn doc(entries: &[&str]) -> MemoryDoc {
        MemoryDoc {
            front_matter: None,
            entries: entries.iter().map(|e| e.to_string()).collect(),
        }
    }

    fn add(content: &str) -> MemoryOp {
        MemoryOp::Add {
            content: content.into(),
        }
    }

    fn patch(old_text: &str, content: &str) -> MemoryOp {
        MemoryOp::Patch {
            old_text: old_text.into(),
            content: content.into(),
        }
    }

    fn remove(old_text: &str) -> MemoryOp {
        MemoryOp::Remove {
            old_text: old_text.into(),
        }
    }

    #[test]
    fn parses_entries() {
        let cases: &[(&str, &[&str])] = &[
            ("", &[]),
            ("\n\n", &[]),
            ("one fact\n", &["one fact"]),
            ("a\n§\nb\n§\nc", &["a", "b", "c"]),
            ("a\r\n§\r\nb\r\nline 2\r\n", &["a", "b\nline 2"]),
            ("\n\na\n\n  §  \n\n\nb\n\n", &["a", "b"]),
            ("a\n§\n§\n\n§\nb", &["a", "b"]),
            ("§\na\n§", &["a"]),
            ("a § b\n§§\n-§-", &["a § b\n§§\n-§-"]),
            (
                "first\n\nsecond paragraph\n§\n    indented\n",
                &["first\n\nsecond paragraph", "    indented"],
            ),
            ("\u{feff}a\n§\nb", &["a", "b"]),
            ("---\nnot closed\n§\nb", &["---\nnot closed", "b"]),
        ];
        for (text, entries) in cases {
            let parsed = MemoryDoc::parse(text);
            assert_eq!(parsed.entries(), *entries, "{text:?}");
            assert_eq!(parsed.front_matter(), None, "{text:?}");
        }
    }

    #[test]
    fn front_matter_is_kept_verbatim_on_rewrite() {
        let front = "---\r\n# a comment\r\nowner: stephen\r\nunknown: [1, 2]\r\n---\r\n";
        let text = format!("{front}\r\nfact one\r\n§\r\nfact two\r\n");
        let parsed = MemoryDoc::parse(&text);
        assert_eq!(parsed.front_matter(), Some(front));
        assert_eq!(parsed.entries(), ["fact one", "fact two"]);

        let updated = parsed
            .apply(&[add("fact three")], DEFAULT_MEMORY_CAP)
            .unwrap();
        assert_eq!(
            updated.render(),
            format!("{front}fact one\n§\nfact two\n§\nfact three\n")
        );

        // Removing every entry leaves the front-matter alone.
        let emptied = parsed
            .apply(&[remove("one"), remove("two")], DEFAULT_MEMORY_CAP)
            .unwrap();
        assert_eq!(emptied.render(), front);
    }

    #[test]
    fn render_adds_line_ending_after_front_matter_only_when_needed() {
        let parsed = MemoryDoc::parse("\u{feff}---\na: 1\n---");
        assert_eq!(parsed.front_matter(), Some("\u{feff}---\na: 1\n---"));
        assert_eq!(parsed.render(), "\u{feff}---\na: 1\n---");
        let updated = parsed.apply(&[add("x")], 10).unwrap();
        assert_eq!(updated.render(), "\u{feff}---\na: 1\n---\nx\n");
        assert_eq!(MemoryDoc::parse(&updated.render()).entries(), ["x"]);
        assert_eq!(MemoryDoc::default().render(), "");
    }

    #[test]
    fn render_round_trips_entries() {
        // Fragments chosen to trip the parser: delimiter lines, setext headings, a BOM, `§` inside
        // a line, indentation, blank lines.
        let fragments = [
            "a",
            "---",
            "---\nnotes\n---\nmore",
            "Title\n---\nx",
            "--- \n---",
            "\u{feff}x",
            "\u{feff}---\nb\n---",
            "  indented",
            "§ inline",
            "x\n\ny",
            "数据 🍵",
        ];
        let front_matters = [None, Some("---\nk: v\n---\n"), Some("---\r\nk: v\r\n---")];
        let mut docs = vec![Vec::new()];
        for len in 1..=3 {
            let mut next = Vec::new();
            for entries in docs.iter().filter(|d: &&Vec<&str>| d.len() == len - 1) {
                for fragment in fragments {
                    let mut longer = entries.clone();
                    longer.push(fragment);
                    next.push(longer);
                }
            }
            docs.extend(next);
        }
        for entries in &docs {
            for front_matter in front_matters {
                let original = MemoryDoc {
                    front_matter: front_matter.map(str::to_owned),
                    entries: entries.iter().map(|e| e.to_string()).collect(),
                };
                let rendered = original.render();
                let reparsed = MemoryDoc::parse(&rendered);
                assert_eq!(reparsed.entries(), original.entries(), "{rendered:?}");
                assert_eq!(reparsed.render(), rendered, "render is stable");
            }
        }
    }

    #[test]
    fn batches_never_turn_entries_into_front_matter() {
        // Removing the first entry would make the second one start the file.
        let before = doc(&["a", "---\nnotes\n---\nmore"]);
        let after = before.apply(&[remove("a")], 1000).unwrap();
        assert_eq!(after.render(), "§\n---\nnotes\n---\nmore\n");
        let reread = MemoryDoc::parse(&after.render());
        assert_eq!(reread.front_matter(), None);
        assert_eq!(reread.entries(), ["---\nnotes\n---\nmore"]);

        // An unclosed `---` closed by a later setext heading.
        let before = MemoryDoc::parse("---\nnot closed\n§\nb");
        let after = before.apply(&[add("Title\n---\nx")], 1000).unwrap();
        let reread = MemoryDoc::parse(&after.render());
        assert_eq!(reread.front_matter(), None);
        assert_eq!(reread.entries(), ["---\nnot closed", "b", "Title\n---\nx"]);
        assert_eq!(reread.chars(), after.chars());
    }

    #[test]
    fn counts_characters_not_bytes() {
        assert_eq!(doc(&[]).chars(), 0);
        assert_eq!(doc(&["café"]).chars(), 4);
        // Each join (`\n§\n`) counts three characters.
        assert_eq!(doc(&["a", "数据", "🍵"]).chars(), 1 + 2 + 1 + 2 * 3);
        let d = doc(&["a", "数据", "🍵"]);
        assert_eq!(d.chars(), d.body().chars().count());
        // Front-matter doesn't count.
        assert_eq!(
            MemoryDoc::parse("---\nlong: front-matter\n---\nab").chars(),
            2
        );
    }

    #[test]
    fn applies_operations() {
        type Case = (
            &'static [&'static str],
            Vec<MemoryOp>,
            &'static [&'static str],
        );
        let cases: Vec<Case> = vec![
            (&[], vec![add("first")], &["first"]),
            (&["a"], vec![add("\n\n  b\n\n")], &["a", "  b"]),
            (&["a"], vec![add("x\r\ny")], &["a", "x\ny"]),
            (
                &["a"],
                vec![add("inline § is fine")],
                &["a", "inline § is fine"],
            ),
            (
                &["likes tea", "lives in Oslo"],
                vec![patch("tea", "coffee")],
                &["likes coffee", "lives in Oslo"],
            ),
            (
                &["café ☕ daily"],
                vec![patch("☕", "🍵")],
                &["café 🍵 daily"],
            ),
            (
                &["数据 und mehr"],
                vec![patch("und", "and")],
                &["数据 and mehr"],
            ),
            (&["a\nb\nc"], vec![patch("b\r\n", "")], &["a\nc"]),
            (&["keep", "drop me"], vec![patch("drop me", "")], &["keep"]),
            (&["keep", "drop"], vec![patch("drop", "  \n ")], &["keep"]),
            (&["x one", "y one"], vec![remove("x")], &["y one"]),
            (&["tea tea", "coffee"], vec![remove("tea")], &["coffee"]),
            // Later operations see earlier ones' results.
            (
                &["a"],
                vec![add("new fact"), patch("new", "newer")],
                &["a", "newer fact"],
            ),
            (
                &["a", "b"],
                vec![remove("a"), add("a2"), patch("a2", "c")],
                &["b", "c"],
            ),
            (&["a"], vec![], &["a"]),
        ];
        for (before, ops, after) in cases {
            let result = doc(before).apply(&ops, DEFAULT_MEMORY_CAP);
            let entries = result.as_ref().map(MemoryDoc::entries);
            assert_eq!(entries, Ok(doc(after).entries()), "{before:?} {ops:?}");
        }
    }

    #[test]
    fn rejects_operations_naming_their_index() {
        use BatchError::*;
        let entries = &["likes tea", "drinks tea at noon", "aaa", "unique"];
        let cases: Vec<(Vec<MemoryOp>, BatchError)> = vec![
            (
                vec![add("   \n")],
                EmptyContent {
                    index: 0,
                    op: "add",
                },
            ),
            (
                vec![add("a\n§\nb")],
                ContainsSeparator {
                    index: 0,
                    op: "add",
                },
            ),
            (
                vec![add("ok"), add("a\r\n §\r\nb")],
                ContainsSeparator {
                    index: 1,
                    op: "add",
                },
            ),
            (
                vec![patch("", "x")],
                EmptyOldText {
                    index: 0,
                    op: "patch",
                },
            ),
            (
                vec![remove("")],
                EmptyOldText {
                    index: 0,
                    op: "remove",
                },
            ),
            (
                vec![patch("coffee", "x")],
                NoMatch {
                    index: 0,
                    op: "patch",
                },
            ),
            (
                vec![remove("coffee")],
                NoMatch {
                    index: 0,
                    op: "remove",
                },
            ),
            // Case matters.
            (
                vec![patch("Unique", "x")],
                NoMatch {
                    index: 0,
                    op: "patch",
                },
            ),
            // Two entries; and overlapping occurrences in one entry.
            (
                vec![patch("tea", "x")],
                Ambiguous {
                    index: 0,
                    op: "patch",
                    count: 2,
                },
            ),
            (
                vec![patch("aa", "x")],
                Ambiguous {
                    index: 0,
                    op: "patch",
                    count: 2,
                },
            ),
            (
                vec![remove("tea")],
                Ambiguous {
                    index: 0,
                    op: "remove",
                    count: 2,
                },
            ),
            // A span never crosses entries.
            (
                vec![patch("aaa\n§\nunique", "x")],
                NoMatch {
                    index: 0,
                    op: "patch",
                },
            ),
            (
                vec![patch("unique", "a\n§\nb")],
                ContainsSeparator {
                    index: 0,
                    op: "patch",
                },
            ),
            (
                vec![remove("unique"), patch("unique", "x")],
                NoMatch {
                    index: 1,
                    op: "patch",
                },
            ),
            (
                vec![add("x"), add("y"), remove("nope")],
                NoMatch {
                    index: 2,
                    op: "remove",
                },
            ),
        ];
        for (ops, expected) in cases {
            assert_eq!(
                doc(entries).apply(&ops, DEFAULT_MEMORY_CAP),
                Err(expected),
                "{ops:?}"
            );
        }
    }

    #[test]
    fn error_messages_guide_the_model() {
        let err = doc(&["tea", "tea"])
            .apply(&[remove("tea")], 100)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "operations[0] (remove): old_text matches 2 entries; include more of the text so it \
             matches exactly one"
        );
        let err = doc(&["tea tea"])
            .apply(&[patch("tea", "x")], 100)
            .unwrap_err();
        assert!(err.to_string().contains("matches 2 places"), "{err}");
        let err = doc(&["a".repeat(2000).as_str()]).apply(&[add(&"b".repeat(1201))], 2200);
        assert_eq!(
            err.unwrap_err().to_string(),
            "the batch would bring memory to 3,204/2,200 characters, 1,004 over the cap; nothing \
             was changed. Make room by merging, shortening, or removing entries, in one batch"
        );
    }

    #[test]
    fn cap_is_checked_on_the_batch_result() {
        // "ab" + join + "cd" = 7 characters.
        let start = doc(&["ab"]);
        assert_eq!(start.apply(&[add("cd")], 7).unwrap().chars(), 7);
        assert_eq!(
            start.apply(&[add("cd")], 6),
            Err(BatchError::OverCap {
                entries: vec!["ab".into()],
                used: 2,
                cap: 6,
                overflow: 1
            })
        );
        // Characters, not bytes: "é" is two bytes.
        assert!(doc(&[]).apply(&[add(&"é".repeat(5))], 5).is_ok());
        // Passing through an over-cap state within a batch is fine.
        let big = "x".repeat(10);
        assert!(start.apply(&[add(&big), remove("x"), add("cd")], 7).is_ok());
    }

    #[test]
    fn over_cap_memory_accepts_only_batches_that_end_under_the_cap() {
        let human = doc(&["a".repeat(30).as_str(), "b".repeat(30).as_str()]);
        let cap = 40;
        assert_eq!(human.chars(), 63);
        match human.apply(&[add("c")], cap) {
            Err(BatchError::OverCap {
                entries,
                used,
                overflow,
                ..
            }) => {
                assert_eq!(entries, human.entries());
                assert_eq!((used, overflow), (63, 27));
            }
            other => panic!("{other:?}"),
        }
        // Shrinking but still over the cap is rejected; consolidating under it is accepted.
        assert!(matches!(
            human.apply(&[patch(&"a".repeat(30), &"a".repeat(10))], cap),
            Err(BatchError::OverCap { overflow: 3, .. })
        ));
        let consolidated = human
            .apply(&[remove("a"), patch(&"b".repeat(30), "b")], cap)
            .unwrap();
        assert_eq!(consolidated.entries(), ["b"]);
    }

    #[test]
    fn usage_headers() {
        let cases = [
            ((1474, 2200), "[67% — 1,474/2,200]"),
            ((0, 2200), "[0% — 0/2,200]"),
            ((2199, 2200), "[99% — 2,199/2,200]"),
            ((2200, 2200), "[100% — 2,200/2,200]"),
            ((2300, 2200), "[104% — 2,300/2,200]"),
            ((5, 999), "[0% — 5/999]"),
            ((1_234_567, 1_000_000), "[123% — 1,234,567/1,000,000]"),
            ((0, 0), "[0% — 0/0]"),
            ((3, 0), "[100% — 3/0]"),
        ];
        for ((used, cap), header) in cases {
            assert_eq!(usage_header(used, cap), header);
        }
        assert_eq!(doc(&["ab", "cd"]).usage_header(100), "[7% — 7/100]");
    }

    #[test]
    fn operations_deserialize_from_tagged_json() {
        let ops: Vec<MemoryOp> = serde_json::from_str(
            r#"[{"op":"add","content":"a"},{"op":"patch","old_text":"a","content":"b"},
                {"op":"remove","old_text":"b"}]"#,
        )
        .unwrap();
        assert_eq!(ops, [add("a"), patch("a", "b"), remove("b")]);
    }

    #[test]
    fn path_helpers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = DataRoot::new(tmp.path(), tmp.path()).unwrap();
        let agents = root.agents_dir();
        assert_eq!(
            agent_memory_file(&root, "jeeves").unwrap(),
            agents.join("jeeves/MEMORY.md")
        );
        assert_eq!(
            user_memory_file(&root, "jeeves", "stephen").unwrap(),
            agents.join("jeeves/users/stephen/MEMORY.md")
        );
        assert_eq!(
            user_profile_file(&root, "stephen").unwrap(),
            root.users_dir().join("stephen/USER.md")
        );
        assert!(user_memory_file(&root, "jeeves", "../x").is_err());
        assert!(user_memory_file(&root, "..", "stephen").is_err());
        assert!(user_profile_file(&root, "a/b").is_err());
    }

    fn dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn missing_file_is_empty_and_first_write_creates_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("agents/jeeves/users/stephen/MEMORY.md");
        assert_eq!(read(&path).await.unwrap(), MemoryDoc::default());
        assert!(
            !tmp.path().join("agents").exists(),
            "reading must not create directories"
        );

        let updated = apply(&path, vec![add("likes tea")], DEFAULT_MEMORY_CAP)
            .await
            .unwrap();
        assert_eq!(updated.entries(), ["likes tea"]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "likes tea\n");
        assert_eq!(read(&path).await.unwrap(), updated);
        let mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, PRIVATE_DIR_MODE);
        assert_eq!(
            dir_names(path.parent().unwrap()),
            ["MEMORY.md"],
            "no temporary files left"
        );
    }

    #[tokio::test]
    async fn rejected_batch_leaves_the_file_byte_for_byte() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("MEMORY.md");
        let original = "---\nx: 1\n---\n\n  a fact\n\n§\nanother\n";
        fs::write(&path, original).unwrap();

        let err = apply(
            &path,
            vec![add("ok"), remove("missing")],
            DEFAULT_MEMORY_CAP,
        )
        .await;
        assert!(matches!(
            err,
            Err(MemoryError::Batch(BatchError::NoMatch { index: 1, .. }))
        ));
        let err = apply(&path, vec![add(&"x".repeat(50))], 20).await;
        assert!(matches!(
            err,
            Err(MemoryError::Batch(BatchError::OverCap { .. }))
        ));
        // A batch that changes nothing doesn't rewrite the file either.
        apply(&path, vec![], DEFAULT_MEMORY_CAP).await.unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert_eq!(dir_names(tmp.path()), ["MEMORY.md"]);
    }

    #[tokio::test]
    async fn writes_keep_permissions_and_follow_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real.md");
        fs::write(&target, "a\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let link = tmp.path().join("MEMORY.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        apply(&link, vec![add("b")], DEFAULT_MEMORY_CAP)
            .await
            .unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "a\n§\nb\n");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[tokio::test]
    async fn writes_through_dangling_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agents/jeeves");
        fs::create_dir_all(&agent).unwrap();
        let cases = [
            (
                "MEMORY.md",
                "../../store/jeeves.md",
                tmp.path().join("store/jeeves.md"),
            ),
            ("OTHER.md", "sub/other.md", agent.join("sub/other.md")),
        ];
        for (name, target, expected) in cases {
            let link = agent.join(name);
            std::os::unix::fs::symlink(target, &link).unwrap();
            apply(&link, vec![add("fact")], DEFAULT_MEMORY_CAP)
                .await
                .unwrap();
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{name}"
            );
            assert_eq!(fs::read_to_string(&expected).unwrap(), "fact\n", "{name}");
            assert_eq!(fs::read_to_string(&link).unwrap(), "fact\n", "{name}");
        }
    }

    #[tokio::test]
    async fn unreadable_file_is_an_error_naming_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("MEMORY.md");
        fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
        let err = read(&path).await.unwrap_err();
        assert!(
            err.to_string()
                .starts_with(&format!("cannot read {}", path.display())),
            "{err}"
        );
        assert!(apply(&path, vec![add("x")], 100).await.is_err());
        assert_eq!(fs::read(&path).unwrap(), [0xff, 0xfe, b'\n']);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_batches_lose_no_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("agents/jeeves");
        fs::create_dir_all(&dir).unwrap();
        // Different spellings of one file share a lock.
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&dir, &alias).unwrap();
        let spellings = [
            dir.join("MEMORY.md"),
            alias.join("MEMORY.md"),
            dir.join("../jeeves/./MEMORY.md"),
        ];

        let tasks: Vec<_> = (0..48)
            .map(|i| {
                let path = spellings[i % spellings.len()].clone();
                tokio::spawn(async move {
                    let ops = vec![
                        add(&format!("fact {i}")),
                        patch(&format!("fact {i}"), &format!("fact #{i}")),
                    ];
                    apply(&path, ops, 100_000).await.unwrap();
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }

        let doc = read(&dir.join("MEMORY.md")).await.unwrap();
        let mut entries = doc.entries().to_vec();
        entries.sort();
        let mut expected: Vec<_> = (0..48).map(|i| format!("fact #{i}")).collect();
        expected.sort();
        assert_eq!(entries, expected);
        assert_eq!(dir_names(&dir), ["MEMORY.md"]);
    }
}
