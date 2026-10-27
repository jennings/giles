//! Editing an agent while the core runs (PLAN section 11.2): the web UI's edit form
//! changes `agent.toml` and `SOUL.md` in place, and the edited agent replaces the old one in the
//! running configuration ([`LiveConfig`](crate::config::LiveConfig)), so the next session uses it.
//! A session that is running keeps the tools and `system` it started with (PLAN section 6.3).
//!
//! The form manages `name`, `models`, `tools`, `ask`, and `timezone`, and `SOUL.md`. Everything
//! else in `agent.toml` is the owner's and stays as written: [`edited_toml`] changes only those
//! keys in the text, with `toml_edit`, so comments, `[[jobs]]`, `busy_mode`, `memory_cap`,
//! `compaction_model`, and any key a later version adds survive, and a key whose value the form
//! doesn't change is not even rewritten. A key whose field was emptied is removed, as a new agent
//! has no key for an empty field.
//!
//! [`AgentManager::edit`], under the manager's lock:
//!
//! 1. Reads both files and refuses the edit ([`EditError::Stale`]) if they aren't the ones the form
//!    was rendered from ([`AgentForm::version`], a hash of both), so a change the owner made by
//!    hand meanwhile is never overwritten silently.
//! 2. Builds the new `agent.toml` text and validates it with
//!    [`Config::validate_agent`](crate::config::Config::validate_agent), as creation and startup do.
//!    An invalid result is refused and nothing is written.
//! 3. Writes the files that changed: each as a temporary file in the agent's directory (mode of
//!    the file it replaces), fsynced; reads the files once more (a change since step 1 is
//!    [`EditError::Stale`] too); renames `SOUL.md`, then `agent.toml`, into place; and fsyncs the
//!    directory. If the second rename fails, the first is undone, so the owner has both files as
//!    they were or both as edited.
//! 4. Replaces the agent in the running configuration.

use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use giles_types::AgentId;
use sha2::{Digest, Sha256};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use super::create::MAX_SOUL_BYTES;
use super::manager::AgentManager;
use crate::config::{AgentConfig, ConfigError};

/// The biggest `agent.toml` or `SOUL.md` the edit form takes on. A bigger file is the owner's to
/// edit by hand: the form would show a page of it.
pub const MAX_EDIT_FILE_BYTES: u64 = 1024 * 1024;

const AGENT_TOML: &str = "agent.toml";
const SOUL: &str = "SOUL.md";

/// What the edit form shows for an agent: the managed settings as `agent.toml` has them, and
/// `SOUL.md`, read from the files (which may differ from the running configuration if the owner
/// edited them by hand).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentForm {
    /// The agent's id: its directory name. Not editable.
    pub id: String,
    /// `name`, empty if unset.
    pub name: String,
    /// `models`.
    pub models: Vec<String>,
    /// `tools`.
    pub tools: Vec<String>,
    /// `ask`.
    pub ask: Vec<String>,
    /// `timezone`, empty if unset.
    pub timezone: String,
    /// `SOUL.md` with `\n` line ends; empty if there is none.
    pub soul: String,
    /// A hash of the two files as they were read. The form posts it back, and the edit is
    /// refused if the files changed meanwhile.
    pub version: String,
}

/// What the edit form posts: the managed settings and `SOUL.md` as the owner wants them, and the
/// [`AgentForm::version`] the form was rendered from. Lists are what the owner typed, split
/// ([`split_list`](super::split_list)); every value is checked like a hand-written `agent.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentEdit {
    /// The agent's id.
    pub id: String,
    /// The display name; empty for none.
    pub name: String,
    /// `<provider>/<model>`, in order.
    pub models: Vec<String>,
    /// Tool globs the agent may call; empty for none.
    pub tools: Vec<String>,
    /// Tool globs that need approval; empty for none.
    pub ask: Vec<String>,
    /// An IANA time zone; empty for none.
    pub timezone: String,
    /// `SOUL.md`.
    pub soul: String,
    /// [`AgentForm::version`] of the form.
    pub version: String,
}

/// Why an agent wasn't edited. Nothing was written (or what was written was put back).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum EditError {
    /// No such agent is configured.
    #[error("there is no agent `{0}`")]
    NotFound(String),
    /// `agent.toml` or `SOUL.md` isn't what the form was rendered from: it changed on disk.
    #[error(
        "agent.toml or SOUL.md changed on disk after this form was opened (edited by hand?). \
         Nothing was saved. Reload the form to see the current values, then make your changes again."
    )]
    Stale,
    /// `SOUL.md` is longer than [`MAX_SOUL_BYTES`].
    #[error("SOUL.md is too long: at most {} KiB", MAX_SOUL_BYTES / 1024)]
    SoulTooLong,
    /// The edited `agent.toml` wouldn't load, as it wouldn't at startup.
    #[error("{0}")]
    Invalid(Box<ConfigError>),
    /// The files can't be edited with the form; the message says why and what to do.
    #[error("{0}")]
    Unsupported(String),
    /// Reading or writing the files failed.
    #[error("cannot write {path}: {source}")]
    Io {
        /// What was being read or written.
        path: String,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// The blocking task reading or writing the files failed.
    #[error("editing the agent failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl EditError {
    /// The problem to show next to the form: the key and reason of an invalid configuration (the
    /// file path and position are the server's business), else the whole message.
    pub fn form_message(&self) -> String {
        match self {
            Self::Invalid(error) if error.key.to_string().is_empty() => error.reason.clone(),
            Self::Invalid(error) => format!("{}: {}", error.key, error.reason),
            other => other.to_string(),
        }
    }

    /// Whether the owner can fix it in the form (else it's the server's problem).
    pub fn is_invalid_input(&self) -> bool {
        !matches!(self, Self::Io { .. } | Self::Task(_))
    }
}

/// `agent.toml` and `SOUL.md` as read from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiskFiles {
    toml: String,
    /// `None`: there is no `SOUL.md`.
    soul: Option<String>,
}

impl DiskFiles {
    /// A hash of both files, as the form's version: each file's name, whether it exists, its length,
    /// and its bytes, so no two different pairs share one.
    fn version(&self) -> String {
        let mut hasher = Sha256::new();
        for (name, contents) in [
            (AGENT_TOML, Some(self.toml.as_str())),
            (SOUL, self.soul.as_deref()),
        ] {
            hasher.update(name.as_bytes());
            match contents {
                Some(text) => {
                    hasher.update([1]);
                    hasher.update((text.len() as u64).to_le_bytes());
                    hasher.update(text.as_bytes());
                }
                None => hasher.update([0]),
            }
        }
        hasher
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            })
    }
}

/// The settings the form manages, read from the text of an `agent.toml`.
struct Managed {
    name: String,
    models: Vec<String>,
    tools: Vec<String>,
    ask: Vec<String>,
    timezone: String,
}

fn parse_toml(source: &str) -> Result<DocumentMut, EditError> {
    source.parse().map_err(|error: toml_edit::TomlError| {
        EditError::Unsupported(format!(
            "agent.toml isn't valid TOML, so the form can't edit it; fix it by hand: {}",
            error.message()
        ))
    })
}

/// The managed settings of `source`; a key that isn't a string (or list of strings) shows empty,
/// and the edit replaces it.
fn managed_of(source: &str) -> Result<Managed, EditError> {
    let doc = parse_toml(source)?;
    let string = |key: &str| {
        doc.get(key)
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let list = |key: &str| -> Vec<String> {
        doc.get(key)
            .and_then(Item::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(Managed {
        name: string("name"),
        models: list("models"),
        tools: list("tools"),
        ask: list("ask"),
        timezone: string("timezone"),
    })
}

/// `source` with the managed keys set as `edit` says and nothing else touched: a key whose value
/// is already what `edit` wants is left exactly as written; a changed one gets its new value
/// keeping its position, key, and the comment after it; a missing one is added after the others
/// of the file's top level; an emptied one (`name`, `tools`, `ask`, `timezone`) is removed.
/// `models` is always written, even empty, so validation reports the empty list.
///
/// # Errors
///
/// [`EditError::Unsupported`] if `source` isn't valid TOML.
pub fn edited_toml(source: &str, edit: &AgentEdit) -> Result<String, EditError> {
    // `toml_edit` turns every `\r\n` into `\n` as it reads, so a file with CRLF line ends is edited
    // as LF and written back with CRLF: the rest of the file keeps its line ends.
    let crlf = source.contains("\r\n");
    let lf;
    let text = if crlf {
        lf = source.replace("\r\n", "\n");
        lf.as_str()
    } else {
        source
    };
    let mut doc = parse_toml(text)?;
    let root = doc.as_table_mut();
    // `models` first: the top level always has a key left for the comment at its top to move to.
    let changed = [
        set_list(root, "models", &edit.models, true),
        set_string(root, "name", edit.name.trim()),
        set_list(root, "tools", &edit.tools, false),
        set_list(root, "ask", &edit.ask, false),
        set_string(root, "timezone", edit.timezone.trim()),
    ];
    // Untouched, the text stays as it is: serializing the document would, for one, add the final
    // newline a hand-written file may lack.
    Ok(match (changed.contains(&true), crlf) {
        (false, _) => source.to_owned(),
        (true, false) => doc.to_string(),
        (true, true) => doc.to_string().replace('\n', "\r\n"),
    })
}

/// Sets `key` to the string `value`, or removes it if `value` is empty. Whether it changed
/// anything.
fn set_string(table: &mut Table, key: &str, value: &str) -> bool {
    let current = table.get(key).and_then(Item::as_str);
    if value.is_empty() {
        remove_key(table, key)
    } else if current != Some(value) {
        replace(table, key, Value::from(value));
        true
    } else {
        false
    }
}

/// Sets `key` to the list `values`, or removes it if `values` is empty (unless `keep_empty`).
/// Whether it changed anything.
fn set_list(table: &mut Table, key: &str, values: &[String], keep_empty: bool) -> bool {
    let same = table
        .get(key)
        .and_then(Item::as_array)
        .is_some_and(|array| {
            array.len() == values.len()
                && array
                    .iter()
                    .zip(values)
                    .all(|(have, want)| have.as_str() == Some(want))
        });
    if same {
        return false;
    }
    if values.is_empty() && !keep_empty {
        return remove_key(table, key);
    }
    let array: Array = values.iter().map(String::as_str).collect();
    replace(table, key, Value::Array(array));
    true
}

/// Removes `key`, with the comment on its line and the comment lines right above it, except for
/// what must not be lost with it: at the top of the file the comments above the first key
/// (the file's header), and anywhere the comments above a blank line that separates them from
/// the key. Those go to the next key. Whether there was a key to remove.
fn remove_key(table: &mut Table, key: &str) -> bool {
    let keys: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
    let Some(index) = keys.iter().position(|k| k == key) else {
        return false;
    };
    let prefix = table
        .key(key)
        .and_then(|k| k.leaf_decor().prefix())
        .and_then(|raw| raw.as_str())
        .unwrap_or_default()
        .to_owned();
    let kept = if index == 0 {
        // The first key, which takes the file's header comment with it unless it's moved.
        prefix.as_str()
    } else {
        // Up to and including the last blank line: not the key's own comment. (`toml_edit` reads
        // line ends as `\n`, see `edited_toml`.)
        prefix.rfind("\n\n").map_or("", |end| &prefix[..end + 2])
    };
    table.remove(key);
    if kept.trim().is_empty() {
        return true;
    }
    if let Some(mut next) = keys.get(index + 1).and_then(|next| table.key_mut(next)) {
        let decor = next.leaf_decor_mut();
        let own = decor
            .prefix()
            .and_then(|raw| raw.as_str())
            .unwrap_or_default();
        decor.set_prefix(format!("{kept}{own}"));
    }
    true
}

/// Gives `key` the value `new`, keeping the white space and comment around the old value.
fn replace(table: &mut Table, key: &str, mut new: Value) {
    if let Some(item) = table.get_mut(key) {
        if let Some(old) = item.as_value() {
            *new.decor_mut() = old.decor().clone();
        }
        *item = Item::Value(new);
    } else {
        table.insert(key, Item::Value(new));
    }
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> EditError {
    let path = path.display().to_string();
    move |source| EditError::Io { path, source }
}

/// Reads `dir/name` for the form: `None` if it doesn't exist. Links, other than regular files,
/// huge files, and text that isn't UTF-8 are for the owner to edit by hand.
fn read_text(dir: &Path, name: &str) -> Result<Option<String>, EditError> {
    let path = dir.join(name);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(&path)(error)),
    };
    let by_hand = |why: String| EditError::Unsupported(format!("{name} {why}; edit it by hand"));
    if meta.file_type().is_symlink() {
        return Err(by_hand(
            "is a symbolic link, which the form doesn't write through".into(),
        ));
    }
    if !meta.is_file() {
        return Err(by_hand("isn't a regular file".into()));
    }
    if meta.len() > MAX_EDIT_FILE_BYTES {
        return Err(by_hand(format!(
            "is {} KiB, too big for the form",
            meta.len() / 1024
        )));
    }
    let bytes = fs::read(&path).map_err(io_error(&path))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| by_hand("isn't valid UTF-8".into()))
}

fn read_files(dir: &Path) -> Result<DiskFiles, EditError> {
    let toml = read_text(dir, AGENT_TOML)?
        .ok_or_else(|| EditError::Unsupported(format!("{} has no {AGENT_TOML}", dir.display())))?;
    Ok(DiskFiles {
        toml,
        soul: read_text(dir, SOUL)?,
    })
}

/// A file to replace: its new contents written to `temp`, to be renamed to `dir/name`.
struct Staged {
    name: &'static str,
    temp: PathBuf,
    /// The file as it was, to put back if a later rename fails; `None`: there was none.
    before: Option<String>,
}

/// Writes `contents` to `temp` (a new file), with `mode`, and fsyncs it.
fn write_temp(temp: &Path, contents: &str, mode: u32) -> io::Result<()> {
    // A leftover of a crash. Edits run one at a time, so it's nobody's.
    let _ = fs::remove_file(temp);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)?;
    file.write_all(contents.as_bytes())?;
    // `mode` of the file being replaced, whatever the umask makes of it.
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    file.sync_all()
}

/// The mode of the file `path`, or 0600 for one that doesn't exist.
fn mode_of(path: &Path) -> u32 {
    fs::metadata(path).map_or(0o600, |meta| meta.permissions().mode() & 0o7777)
}

/// Replaces `SOUL.md` and `agent.toml` in `dir` with `soul` and `toml` where they're `Some`, if
/// the files are still `before`. All or none: see the module docs.
fn write_edit(
    dir: &Path,
    before: &DiskFiles,
    toml: Option<&str>,
    soul: Option<&str>,
) -> Result<(), EditError> {
    commit(dir, before, toml, soul, |from, to| fs::rename(from, to))
}

/// [`write_edit`] with the rename that puts a file in place as a parameter, for tests to fail.
fn commit(
    dir: &Path,
    before: &DiskFiles,
    toml: Option<&str>,
    soul: Option<&str>,
    rename: impl Fn(&Path, &Path) -> io::Result<()>,
) -> Result<(), EditError> {
    let mut staged = Vec::new();
    let result = (|| {
        for (name, contents, was) in [
            (SOUL, soul, before.soul.as_deref()),
            (AGENT_TOML, toml, Some(before.toml.as_str())),
        ] {
            let Some(contents) = contents else { continue };
            let temp = dir.join(format!(".{name}.edit.tmp"));
            write_temp(&temp, contents, mode_of(&dir.join(name))).map_err(io_error(&temp))?;
            staged.push(Staged {
                name,
                temp,
                before: was.map(str::to_owned),
            });
        }
        // The files may have changed since the check; this narrows the window to the renames.
        if read_files(dir)? != *before {
            return Err(EditError::Stale);
        }
        let mut done: Vec<&Staged> = Vec::new();
        for file in &staged {
            let target = dir.join(file.name);
            if let Err(error) = rename(&file.temp, &target) {
                for put in done.iter().rev() {
                    restore(dir, put, &rename);
                }
                return Err(io_error(&target)(error));
            }
            done.push(file);
        }
        // The files are in place; a directory that can't be fsynced is worth a log line, not an
        // error that would say nothing changed.
        if let Err(error) = File::open(dir).and_then(|d| d.sync_all()) {
            tracing::warn!(dir = %dir.display(), %error, "cannot fsync an agent's directory after an edit");
        }
        Ok(())
    })();
    for file in &staged {
        let _ = fs::remove_file(&file.temp);
    }
    result
}

/// Puts back the file `put` replaced: its old contents, or no file if there was none. Best
/// effort: this runs because a rename failed, and that error is the one reported.
fn restore(dir: &Path, put: &Staged, rename: &impl Fn(&Path, &Path) -> io::Result<()>) {
    let target = dir.join(put.name);
    match &put.before {
        Some(old) => {
            let temp = dir.join(format!(".{}.undo.tmp", put.name));
            let restored =
                write_temp(&temp, old, mode_of(&target)).and_then(|()| rename(&temp, &target));
            if restored.is_err() {
                let _ = fs::remove_file(&temp);
            }
        }
        None => {
            let _ = fs::remove_file(&target);
        }
    }
}

impl AgentManager {
    /// `id`'s configured agent. Ids compare exactly: the form's address is the directory name.
    fn configured(&self, id: &str) -> Result<AgentConfig, EditError> {
        AgentId::new(id)
            .ok()
            .and_then(|agent| self.registry.config().agents.get(&agent).cloned())
            .ok_or_else(|| EditError::NotFound(id.to_owned()))
    }

    /// What the edit form shows for agent `id`, read from its files.
    ///
    /// # Errors
    ///
    /// [`EditError::NotFound`] for an agent that isn't configured; [`EditError::Unsupported`] if
    /// the files can't be edited with the form (not TOML, a link, not UTF-8, too big);
    /// [`EditError::Io`].
    pub async fn edit_form(&self, id: &str) -> Result<AgentForm, EditError> {
        let _one_at_a_time = self.lock.lock().await;
        let agent = self.configured(id)?;
        let dir = agent.dir.clone();
        let files = tokio::task::spawn_blocking(move || read_files(&dir)).await??;
        let managed = managed_of(&files.toml)?;
        Ok(AgentForm {
            id: agent.id.to_string(),
            name: managed.name,
            models: managed.models,
            tools: managed.tools,
            ask: managed.ask,
            timezone: managed.timezone,
            soul: files
                .soul
                .as_deref()
                .unwrap_or_default()
                .replace("\r\n", "\n"),
            version: files.version(),
        })
    }

    /// Edits the agent (see the [module docs](self)) and returns its configuration, which now
    /// replaces the old one in the running configuration.
    ///
    /// # Errors
    ///
    /// An [`EditError`]; the files and the configuration are as they were.
    pub async fn edit(&self, edit: &AgentEdit) -> Result<AgentConfig, EditError> {
        let _one_at_a_time = self.lock.lock().await;
        let agent = self.configured(&edit.id)?;
        let dir = agent.dir.clone();
        let files = {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || read_files(&dir)).await??
        };
        if files.version() != edit.version {
            return Err(EditError::Stale);
        }
        let old_soul = files
            .soul
            .as_deref()
            .unwrap_or_default()
            .replace("\r\n", "\n");
        let soul_changed = edit.soul != old_soul;
        if soul_changed && edit.soul.len() > MAX_SOUL_BYTES {
            return Err(EditError::SoulTooLong);
        }
        let toml = edited_toml(&files.toml, edit)?;
        let validated = self
            .registry
            .config()
            .validate_agent(self.registry.root(), agent.id.as_str(), &toml)
            .map_err(|e| EditError::Invalid(Box::new(e)))?;
        let write_toml = (toml != files.toml).then_some(toml);
        let write_soul = soul_changed.then(|| edit.soul.clone());
        if write_toml.is_some() || write_soul.is_some() {
            tokio::task::spawn_blocking(move || {
                write_edit(&dir, &files, write_toml.as_deref(), write_soul.as_deref())
            })
            .await??;
        }
        self.registry
            .live_config()
            .replace_agent(validated.clone())
            .map_err(|missing| EditError::NotFound(missing.0.to_string()))?;
        tracing::info!(agent = %validated.id, "agent edited");
        Ok(validated)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    /// An `agent.toml` a person wrote: comments, a multi-line list, keys the form doesn't manage,
    /// and jobs.
    const HAND_WRITTEN: &str = r#"# Jeeves, the household agent.
name = "Jeeves"   # shown in chats
models = [
    "anthropic/claude-sonnet-4-5",   # first choice
    "ollama/qwen3:8b",
]
compaction_model = "ollama/qwen3:8b"
tools = ["memory_add", "github__*"]
max_iterations = 50
busy_mode = "queue"
memory_cap = 4000
idle_reset = "6h"
timezone = "Europe/London"   # the agent's own zone

[[jobs]]
id = "morning"   # runs daily
cron = "0 8 * * *"
prompt = "Plan my day."

[[jobs]]
id = "digest"
cron = "0 18 * * 5"
prompt = "Summarize the week."
"#;

    fn edit_of(source: &str) -> AgentEdit {
        let managed = managed_of(source).unwrap();
        AgentEdit {
            id: "jeeves".into(),
            name: managed.name,
            models: managed.models,
            tools: managed.tools,
            ask: managed.ask,
            timezone: managed.timezone,
            soul: String::new(),
            version: String::new(),
        }
    }

    fn lines(text: &str) -> Vec<&str> {
        text.lines().collect()
    }

    #[test]
    fn an_edit_that_changes_nothing_leaves_the_file_byte_for_byte() {
        for source in [
            HAND_WRITTEN,
            "models = [\"a/b\"]",
            "models=['a/b']\n",
            "# a header\n\nmodels = [\"a/b\"]\nunknown_future_key = { x = 1 }\n",
        ] {
            let edit = edit_of(source);
            assert_eq!(edited_toml(source, &edit).unwrap(), source, "{source:?}");
        }
    }

    #[test]
    fn only_the_changed_managed_keys_are_rewritten() {
        let mut edit = edit_of(HAND_WRITTEN);
        edit.name = " Jeeves II ".into();
        edit.tools = vec!["memory_add".into(), "memory_search".into()];
        edit.ask = vec!["github__create_*".into()];
        let edited = edited_toml(HAND_WRITTEN, &edit).unwrap();

        let before = lines(HAND_WRITTEN);
        let after = lines(&edited);
        // Replaced in place, keeping the comment after the old value.
        assert!(
            after.contains(&"name = \"Jeeves II\"   # shown in chats"),
            "{edited}"
        );
        assert!(
            after.contains(&"tools = [\"memory_add\", \"memory_search\"]"),
            "{edited}"
        );
        // Everything else is exactly as written: every other line, in order.
        let unchanged: Vec<&str> = before
            .iter()
            .copied()
            .filter(|l| !l.starts_with("name = ") && !l.starts_with("tools = "))
            .collect();
        let rest: Vec<&str> = after
            .iter()
            .copied()
            .filter(|l| {
                !l.starts_with("name = ") && !l.starts_with("tools = ") && !l.starts_with("ask = ")
            })
            .collect();
        assert_eq!(rest, unchanged);

        // And as data: the new values, and the rest of the file's keys as they were.
        let parsed: toml::Table = toml::from_str(&edited).unwrap();
        let original: toml::Table = toml::from_str(HAND_WRITTEN).unwrap();
        assert_eq!(parsed["ask"].as_array().unwrap().len(), 1);
        for key in [
            "models",
            "compaction_model",
            "max_iterations",
            "busy_mode",
            "memory_cap",
            "idle_reset",
            "timezone",
            "jobs",
        ] {
            assert_eq!(parsed[key], original[key], "{key}");
        }
    }

    #[test]
    fn a_new_key_lands_in_the_top_level_not_after_the_jobs() {
        let source =
            "models = [\"a/b\"]\n\n[[jobs]]\nid = \"x\"\ncron = \"0 8 * * *\"\nprompt = \"p\"\n";
        let mut edit = edit_of(source);
        edit.name = "Jeeves".into();
        edit.tools = vec!["memory_add".into()];
        edit.timezone = "Europe/London".into();
        let edited = edited_toml(source, &edit).unwrap();
        let parsed: toml::Table = toml::from_str(&edited).unwrap();
        assert_eq!(parsed["name"].as_str(), Some("Jeeves"), "{edited}");
        assert_eq!(
            parsed["timezone"].as_str(),
            Some("Europe/London"),
            "{edited}"
        );
        assert_eq!(parsed["tools"][0].as_str(), Some("memory_add"), "{edited}");
        let jobs = parsed["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].as_table().unwrap().len(),
            3,
            "the job gained keys: {edited}"
        );
    }

    #[test]
    fn an_emptied_field_removes_its_key_and_models_are_kept_even_empty() {
        let mut edit = edit_of(HAND_WRITTEN);
        edit.name = "  ".into();
        edit.tools.clear();
        edit.timezone.clear();
        edit.models.clear();
        let edited = edited_toml(HAND_WRITTEN, &edit).unwrap();
        let parsed: toml::Table = toml::from_str(&edited).unwrap();
        for key in ["name", "tools", "timezone"] {
            assert!(!parsed.contains_key(key), "{key} in {edited}");
        }
        assert_eq!(parsed["models"].as_array().unwrap().len(), 0, "{edited}");
        // The rest is untouched, including the comments around what stayed.
        assert!(
            edited.contains("# Jeeves, the household agent."),
            "{edited}"
        );
        assert!(
            edited.contains("id = \"morning\"   # runs daily"),
            "{edited}"
        );
        assert_eq!(parsed["jobs"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_removed_key_takes_its_own_comment_but_not_the_files_header_or_a_standalone_one() {
        let source = "# The household agent.\nname = \"Jeeves\"\n\n# Which models.\nmodels = [\"a/b\"]\n\n# Standalone notes.\n\n# The tools it may use.\ntools = [\"memory\"]   # trailing\ntimezone = \"Europe/London\"\n";
        let mut edit = edit_of(source);
        edit.name.clear();
        edit.tools.clear();
        let edited = edited_toml(source, &edit).unwrap();
        // The header moved to the next key; the blank-line-separated notes stayed; the comments
        // that belonged to `tools` went with it.
        assert_eq!(
            edited,
            "# The household agent.\n\n# Which models.\nmodels = [\"a/b\"]\n\n# Standalone notes.\n\ntimezone = \"Europe/London\"\n"
        );
    }

    #[test]
    fn a_file_with_crlf_line_ends_keeps_them_and_the_blank_line_rule_holds() {
        let source = "# The household agent.\nname = \"Jeeves\"\n\n# Which models.\nmodels = [\"a/b\"]\n\n# Standalone notes.\n\n# The tools it may use.\ntools = [\"memory\"]\ntimezone = \"Europe/London\"\n".replace('\n', "\r\n");
        let mut edit = edit_of(&source);
        edit.name.clear();
        edit.tools.clear();
        let edited = edited_toml(&source, &edit).unwrap();
        assert_eq!(
            edited,
            "# The household agent.\n\n# Which models.\nmodels = [\"a/b\"]\n\n# Standalone notes.\n\ntimezone = \"Europe/London\"\n"
                .replace('\n', "\r\n")
        );
        // Unchanged, it is returned as read, CRLF and all.
        assert_eq!(edited_toml(&source, &edit_of(&source)).unwrap(), source);
    }

    #[test]
    fn a_value_of_the_wrong_type_is_replaced_and_toml_that_does_not_parse_is_refused() {
        let source = "name = 7\nmodels = \"a/b\"\ntools = { x = 1 }\n";
        let mut edit = edit_of(source);
        assert_eq!(edit.name, "");
        edit.name = "Jeeves".into();
        edit.models = vec!["a/b".into()];
        edit.tools = vec!["memory_add".into()];
        let edited = edited_toml(source, &edit).unwrap();
        let parsed: toml::Table = toml::from_str(&edited).unwrap();
        assert_eq!(parsed["name"].as_str(), Some("Jeeves"));
        assert_eq!(parsed["models"][0].as_str(), Some("a/b"));
        assert_eq!(parsed["tools"][0].as_str(), Some("memory_add"));

        let error = edited_toml("models = [", &edit).unwrap_err();
        assert!(
            matches!(&error, EditError::Unsupported(m) if m.contains("isn't valid TOML")),
            "{error}"
        );
        assert!(managed_of("name = ").is_err());
    }

    #[test]
    fn the_version_tells_every_pair_of_files_apart() {
        let files = |toml: &str, soul: Option<&str>| DiskFiles {
            toml: toml.into(),
            soul: soul.map(str::to_owned),
        };
        let versions = [
            files("a", Some("bc")).version(),
            files("ab", Some("c")).version(),
            files("abc", Some("")).version(),
            files("abc", None).version(),
            files("a", Some("bc ")).version(),
        ];
        for (i, one) in versions.iter().enumerate() {
            assert_eq!(one.len(), 64);
            for other in &versions[i + 1..] {
                assert_ne!(one, other);
            }
        }
        assert_eq!(files("a", None).version(), files("a", None).version());
    }

    fn agent_dir(toml: &str, soul: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(AGENT_TOML), toml).unwrap();
        if let Some(soul) = soul {
            fs::write(dir.path().join(SOUL), soul).unwrap();
        }
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn both_files_are_replaced_keeping_their_modes_and_leaving_no_temporary_files() {
        let dir = agent_dir("models = [\"a/b\"]\n", Some("old soul"));
        fs::set_permissions(
            dir.path().join(AGENT_TOML),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        let before = read_files(dir.path()).unwrap();
        write_edit(
            dir.path(),
            &before,
            Some("models = [\"c/d\"]\n"),
            Some("new soul"),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(AGENT_TOML)).unwrap(),
            "models = [\"c/d\"]\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(SOUL)).unwrap(),
            "new soul"
        );
        assert_eq!(mode_of(&dir.path().join(AGENT_TOML)), 0o640);
        assert_eq!(names(dir.path()), ["SOUL.md", "agent.toml"]);

        // Only what changed is written: a missing SOUL.md stays missing.
        let dir = agent_dir("models = [\"a/b\"]\n", None);
        let before = read_files(dir.path()).unwrap();
        write_edit(dir.path(), &before, Some("models = [\"c/d\"]\n"), None).unwrap();
        assert_eq!(names(dir.path()), ["agent.toml"]);
    }

    #[test]
    fn if_the_second_rename_fails_the_first_is_undone() {
        for soul_before in [Some("old soul"), None] {
            let dir = agent_dir("models = [\"a/b\"]\n", soul_before);
            let before = read_files(dir.path()).unwrap();
            let calls = Cell::new(0);
            let error = commit(
                dir.path(),
                &before,
                Some("models = [\"c/d\"]\n"),
                Some("new soul"),
                |from, to| {
                    calls.set(calls.get() + 1);
                    // `SOUL.md` goes first and works; the `agent.toml` rename fails.
                    if to.file_name().unwrap() == AGENT_TOML {
                        Err(io::Error::other("disk full"))
                    } else {
                        fs::rename(from, to)
                    }
                },
            )
            .unwrap_err();
            assert!(matches!(error, EditError::Io { .. }), "{error}");
            assert!(calls.get() >= 2);
            assert_eq!(
                fs::read_to_string(dir.path().join(AGENT_TOML)).unwrap(),
                "models = [\"a/b\"]\n"
            );
            match soul_before {
                Some(old) => assert_eq!(fs::read_to_string(dir.path().join(SOUL)).unwrap(), old),
                None => assert!(!dir.path().join(SOUL).exists()),
            }
            let expected: Vec<&str> = if soul_before.is_some() {
                vec!["SOUL.md", "agent.toml"]
            } else {
                vec!["agent.toml"]
            };
            assert_eq!(names(dir.path()), expected, "no temporary files");
        }
    }

    #[test]
    fn a_file_that_changed_since_it_was_read_is_never_replaced() {
        let dir = agent_dir("models = [\"a/b\"]\n", Some("soul"));
        let before = read_files(dir.path()).unwrap();
        fs::write(dir.path().join(SOUL), "edited by hand").unwrap();
        let error =
            write_edit(dir.path(), &before, Some("models = [\"c/d\"]\n"), None).unwrap_err();
        assert!(matches!(error, EditError::Stale), "{error}");
        assert_eq!(
            fs::read_to_string(dir.path().join(AGENT_TOML)).unwrap(),
            "models = [\"a/b\"]\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(SOUL)).unwrap(),
            "edited by hand"
        );
        assert_eq!(names(dir.path()), ["SOUL.md", "agent.toml"]);
    }

    #[test]
    fn files_the_form_cannot_write_through_are_for_the_owner_to_edit_by_hand() {
        let dir = agent_dir("models = [\"a/b\"]\n", None);
        let outside = dir.path().join("elsewhere.md");
        fs::write(&outside, "linked soul").unwrap();
        std::os::unix::fs::symlink(&outside, dir.path().join(SOUL)).unwrap();
        let error = read_files(dir.path()).unwrap_err();
        assert!(
            matches!(&error, EditError::Unsupported(m) if m.contains("SOUL.md is a symbolic link")),
            "{error}"
        );
        fs::remove_file(dir.path().join(SOUL)).unwrap();

        fs::write(dir.path().join(SOUL), [0xff, 0xfe, b'x']).unwrap();
        let error = read_files(dir.path()).unwrap_err();
        assert!(
            matches!(&error, EditError::Unsupported(m) if m.contains("UTF-8")),
            "{error}"
        );

        fs::write(
            dir.path().join(SOUL),
            vec![b'x'; MAX_EDIT_FILE_BYTES as usize + 1],
        )
        .unwrap();
        let error = read_files(dir.path()).unwrap_err();
        assert!(
            matches!(&error, EditError::Unsupported(m) if m.contains("too big")),
            "{error}"
        );
        fs::remove_file(dir.path().join(SOUL)).unwrap();

        fs::remove_file(dir.path().join(AGENT_TOML)).unwrap();
        let error = read_files(dir.path()).unwrap_err();
        assert!(
            matches!(&error, EditError::Unsupported(m) if m.contains("no agent.toml")),
            "{error}"
        );
    }
}
