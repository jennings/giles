//! Creating an agent while the core runs (PLAN section 11.2): the web UI's form
//! becomes `agents/<agent>/agent.toml` and `SOUL.md`, and the agent joins the running
//! configuration ([`LiveConfig`](crate::config::LiveConfig)), so it can be used at once. Nothing
//! else about the configuration changes; reload stays deferred (D28).
//!
//! [`AgentManager::create`]:
//!
//! 1. Builds `agent.toml` with the TOML serializer ([`agent_toml`]) and validates it with
//!    [`Config::validate_agent`], the checks startup loading runs on every agent.
//! 2. Refuses a reserved id ([`RESERVED_AGENT_IDS`]) and one taken by a configured agent, ignoring
//!    ASCII case (on a case-insensitive file system the two would share a directory).
//! 3. Writes the files into a staging directory, `agents/.<agent>.creating/` (created exclusively;
//!    config loading, the agent list, and the file viewer skip dot names): each file as a temporary
//!    file, fsynced, renamed into place (`SOUL.md`, then `agent.toml`); fsyncs it, renames it to
//!    `agents/<agent>/` (refused if that exists and isn't empty), and fsyncs `agents/`. So the
//!    agent's directory appears whole or not at all. On any failure the staging directory is
//!    removed; one left by a crash is removed at the next start ([`AgentManager::new`]).
//! 4. Adds the agent to the [`LiveConfig`](crate::config::LiveConfig).
//!
//! Creations run one at a time (a lock around all four steps), so of two with the same id one
//! wins and the other is told the id is taken.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;

use serde::Serialize;
use tracing::{debug, info};

use super::manager::AgentManager;
use crate::config::{AgentConfig, ConfigError};

/// Agent ids the web UI won't create: `new` is the address of the form itself
/// (`/admin/agents/new`), so an agent with that id couldn't have a page.
pub const RESERVED_AGENT_IDS: &[&str] = &["new"];

/// The longest `SOUL.md` the form accepts, in bytes.
pub const MAX_SOUL_BYTES: usize = 64 * 1024;

/// The text the new-agent form starts `SOUL.md` with, and what an agent created from a post that
/// has no `soul` field gets.
pub const DEFAULT_SOUL: &str = "This is your information. Update it as you learn new things about yourself.\n\nYou are a helpful personal assistant.\n";

/// The id the form derives from an agent's name: lowercased, each space turned into `-`, and every
/// other character that isn't `a`-`z`, `0`-`9`, or `-` removed. So the result always has the
/// characters an id may have (though it can be empty, or `new`, which creation refuses). Nothing
/// is trimmed or collapsed, and ids are ASCII: letters beyond it (`é`) are removed like any
/// symbol, and so is whitespace other than a plain space.
///
/// `assets/agent-form.js` does the same in the browser as the owner types; keep the two equal
/// (the table in this module's tests is for running through the script by hand).
pub fn agent_id_from_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            'a'..='z' | '0'..='9' | '-' => Some(c),
            _ => None,
        })
        .collect()
}

/// What the form asks for. Lists hold what the owner typed, split ([`split_list`]); every value is
/// checked by [`Config::validate_agent`](crate::config::Config::validate_agent) like a
/// hand-written `agent.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewAgent {
    /// The agent's id: its directory name.
    pub id: String,
    /// The display name; empty for none.
    pub name: String,
    /// `<provider>/<model>`, in order.
    pub models: Vec<String>,
    /// Tool globs the agent may call.
    pub tools: Vec<String>,
    /// Tool globs that need approval.
    pub ask: Vec<String>,
    /// An IANA time zone; empty for none.
    pub timezone: String,
    /// `SOUL.md`.
    pub soul: String,
}

/// Why an agent wasn't created. Nothing was written (or what was written was removed).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CreateError {
    /// The id is reserved ([`RESERVED_AGENT_IDS`]).
    #[error("`{0}` is reserved; choose another id")]
    Reserved(String),
    /// An agent with this id (ignoring case) is configured, or its directory exists.
    #[error("there is already an agent `{0}`")]
    Exists(String),
    /// `SOUL.md` is longer than [`MAX_SOUL_BYTES`].
    #[error("SOUL.md is too long: at most {} KiB", MAX_SOUL_BYTES / 1024)]
    SoulTooLong,
    /// The agent's configuration is invalid, as it would be at startup.
    #[error("{0}")]
    Invalid(Box<ConfigError>),
    /// Writing the files failed.
    #[error("cannot write {path}: {source}")]
    Io {
        /// What was being written.
        path: String,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// The blocking task writing the files failed.
    #[error("writing the agent failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl CreateError {
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

/// Splits a list as typed in the form: at commas and whitespace, empty parts dropped. Config
/// values in these lists (models, tool globs) never contain whitespace, and in practice no comma.
pub fn split_list(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `agent.toml` for `agent`, from the TOML serializer: unset values are left out, so the
/// defaults apply as in a hand-written file. Every value is a string or a list of strings, which
/// the serializer always accepts.
pub fn agent_toml(agent: &NewAgent) -> String {
    #[derive(Serialize)]
    struct AgentToml<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<&'a str>,
        models: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        tools: &'a [String],
        #[serde(skip_serializing_if = "<[String]>::is_empty")]
        ask: &'a [String],
        #[serde(skip_serializing_if = "Option::is_none")]
        timezone: Option<&'a str>,
    }
    fn present(s: &str) -> Option<&str> {
        Some(s.trim()).filter(|s| !s.is_empty())
    }
    toml::to_string(&AgentToml {
        name: present(&agent.name),
        models: &agent.models,
        tools: &agent.tools,
        ask: &agent.ask,
        timezone: present(&agent.timezone),
    })
    .expect("strings and lists of strings always serialize")
}

impl AgentManager {
    /// Creates `agent` (see the [module docs](self)) and returns its configuration, already part
    /// of the running configuration.
    ///
    /// # Errors
    ///
    /// A [`CreateError`]; nothing is left on disk and the configuration is unchanged.
    pub async fn create(&self, agent: &NewAgent) -> Result<AgentConfig, CreateError> {
        let _one_at_a_time = self.lock.lock().await;
        if agent.soul.len() > MAX_SOUL_BYTES {
            return Err(CreateError::SoulTooLong);
        }
        let source = agent_toml(agent);
        let root = self.registry.root();
        let config = self.registry.config();
        let validated = config
            .validate_agent(root, &agent.id, &source)
            .map_err(|e| CreateError::Invalid(Box::new(e)))?;
        if RESERVED_AGENT_IDS
            .iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(&agent.id))
        {
            return Err(CreateError::Reserved(agent.id.clone()));
        }
        if let Some(taken) = config
            .agents
            .keys()
            .find(|id| id.as_str().eq_ignore_ascii_case(&agent.id))
        {
            return Err(CreateError::Exists(taken.to_string()));
        }
        let agents_dir = root.agents_dir();
        let id = agent.id.clone();
        let soul = agent.soul.clone();
        tokio::task::spawn_blocking(move || {
            write_agent(
                &agents_dir,
                &id,
                &[("SOUL.md", &soul), ("agent.toml", &source)],
            )
        })
        .await??;
        if let Err(taken) = self.registry.live_config().add_agent(validated.clone()) {
            // Another creator over the same configuration took the id, ignoring case, while this
            // one wrote (on a case-sensitive file system `Butler` and `butler` are two
            // directories). The directory is this call's own: remove it, or the next start would
            // load both.
            let _ = fs::remove_dir_all(&validated.dir);
            return Err(CreateError::Exists(taken.0.to_string()));
        }
        info!(agent = %validated.id, dir = %validated.dir.display(), "agent created");
        Ok(validated)
    }
}

/// The staging directory of agent `id` while it's being written: `agents/.<id>.creating/`. Its
/// name starts with `.`, so config loading, the agent list, and the file viewer skip it.
fn staging_name(id: &str) -> String {
    format!(".{id}{STAGING_SUFFIX}")
}

/// The suffix of staging directories ([`staging_name`]).
const STAGING_SUFFIX: &str = ".creating";

/// Removes the staging directories left in `agents_dir` by a creation that never finished (the
/// process died mid-write). Called when the daemon's manager is built at startup, before any
/// creation runs (the data root's instance lock makes this daemon the only writer), so none is in
/// use. A leftover never breaks startup anyway: config loading skips it like any dot directory.
pub(super) fn remove_stale_staging(agents_dir: &Path) {
    let Ok(entries) = fs::read_dir(agents_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if is_dir && name.starts_with('.') && name.ends_with(STAGING_SUFFIX) {
            let dir = entry.path();
            match fs::remove_dir_all(&dir) {
                Ok(()) => info!(dir = %dir.display(), "removed an unfinished agent creation"),
                Err(error) => {
                    debug!(dir = %dir.display(), %error, "cannot remove an unfinished agent creation");
                }
            }
        }
    }
}

/// Writes `files` (name, contents) as the new agent directory `agents_dir/<id>`: into a staging
/// directory `agents_dir/.<id>.creating/` (created exclusively, mode 0700), in order, each through
/// a temporary file that is fsynced and renamed into place; then fsyncs the staging directory,
/// renames it to `<id>` (which fails if `<id>` exists and isn't empty), and fsyncs `agents_dir`.
/// So `agents/<id>/` appears whole or not at all. On any failure the staging directory is removed.
fn write_agent(agents_dir: &Path, id: &str, files: &[(&str, &str)]) -> Result<(), CreateError> {
    let dir = agents_dir.join(id);
    let staging = agents_dir.join(staging_name(id));
    let io_error = |path: &Path| {
        let path = path.display().to_string();
        move |source| CreateError::Io { path, source }
    };
    if fs::symlink_metadata(&dir).is_ok() {
        return Err(CreateError::Exists(id.to_owned()));
    }
    DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(io_error(&staging))?;
    let written = (|| {
        for (name, contents) in files {
            write_file(&staging, name, contents.as_bytes())
                .map_err(io_error(&staging.join(name)))?;
        }
        File::open(&staging)
            .and_then(|d| d.sync_all())
            .map_err(io_error(&staging))?;
        match fs::rename(&staging, &dir) {
            Ok(()) => {}
            // Something non-empty appeared at `<id>` meanwhile (made by hand): not ours to replace.
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) => {
                return Err(CreateError::Exists(id.to_owned()));
            }
            Err(error) => return Err(io_error(&dir)(error)),
        }
        File::open(agents_dir)
            .and_then(|d| d.sync_all())
            .map_err(io_error(agents_dir))
    })();
    if written.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    written
}

/// Writes `contents` to `dir/name` atomically: a new temporary file (mode 0600), fsynced, then
/// renamed over the name.
fn write_file(dir: &Path, name: &str, contents: &[u8]) -> io::Result<()> {
    let temp = dir.join(format!(".{name}.tmp"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temp, dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(name, id)`. Also the cases to run through `derive` in `assets/agent-form.js`.
    const ID_FROM_NAME: &[(&str, &str)] = &[
        ("Jeeves", "jeeves"),
        ("The Butler", "the-butler"),
        ("MIXED case 42", "mixed-case-42"),
        ("with  two   spaces", "with--two---spaces"),
        (" padded ", "-padded-"),
        ("Dr. Who? (Time & Space) #1!", "dr-who-time--space-1"),
        ("under_score/slash\\back.dot", "underscoreslashbackdot"),
        ("already-hyphenated--name", "already-hyphenated--name"),
        ("-", "-"),
        ("007", "007"),
        ("tab\tnewline\nnbsp\u{a0}end", "tabnewlinenbspend"),
        ("Café", "caf"),
        ("Ünïcödé", "ncd"),
        ("日本語 agent", "-agent"),
        ("emoji 🙂 face", "emoji--face"),
        // Lowercasing happens first: these two lowercase to ASCII letters (plus a combining dot).
        ("\u{212a}elvin", "kelvin"),
        ("\u{130}stanbul", "istanbul"),
        ("", ""),
        ("!@#$%^&*()", ""),
        ("日本語", ""),
    ];

    #[test]
    fn the_id_is_the_name_lowercased_with_hyphens_for_spaces_and_no_other_symbols() {
        for (name, id) in ID_FROM_NAME {
            assert_eq!(agent_id_from_name(name), *id, "{name:?}");
        }
    }

    #[test]
    fn a_derived_id_has_only_id_characters() {
        for (name, _) in ID_FROM_NAME {
            let id = agent_id_from_name(name);
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{name:?} -> {id:?}"
            );
            if !id.is_empty() {
                assert!(giles_types::AgentId::new(&id).is_ok(), "{id:?}");
            }
        }
    }

    #[test]
    fn the_default_soul_is_two_paragraphs_with_a_trailing_newline() {
        assert_eq!(
            DEFAULT_SOUL,
            "This is your information. Update it as you learn new things about yourself.\n\nYou are a helpful personal assistant.\n"
        );
        assert!(DEFAULT_SOUL.len() <= MAX_SOUL_BYTES);
    }

    #[test]
    fn lists_split_at_commas_and_whitespace() {
        assert_eq!(
            split_list(" memory_add, memory_search\nload_skill ,,github__* "),
            ["memory_add", "memory_search", "load_skill", "github__*"]
        );
        assert!(split_list(" ,\n").is_empty());
    }

    #[test]
    fn agent_toml_comes_from_the_serializer_and_leaves_out_unset_values() {
        let agent = NewAgent {
            id: "bunter".into(),
            name: "Bunter \"the\" Butler\n[x]".into(),
            models: vec!["anthropic/claude".into(), "ollama/qwen3:8b".into()],
            soul: "ignored here".into(),
            ..NewAgent::default()
        };
        let source = agent_toml(&agent);
        let parsed: toml::Table = toml::from_str(&source).unwrap();
        assert_eq!(parsed["name"].as_str(), Some("Bunter \"the\" Butler\n[x]"));
        assert_eq!(parsed["models"].as_array().unwrap().len(), 2, "{source}");
        for key in ["tools", "ask", "timezone", "soul", "id"] {
            assert!(!parsed.contains_key(key), "{key} in {source}");
        }
        let full = agent_toml(&NewAgent {
            tools: vec!["memory_add".into()],
            ask: vec!["github__*".into()],
            timezone: " Europe/London ".into(),
            name: "  ".into(),
            ..agent
        });
        let parsed: toml::Table = toml::from_str(&full).unwrap();
        assert_eq!(parsed["tools"][0].as_str(), Some("memory_add"));
        assert_eq!(parsed["ask"][0].as_str(), Some("github__*"));
        assert_eq!(parsed["timezone"].as_str(), Some("Europe/London"));
        assert!(!parsed.contains_key("name"), "{full}");
    }

    #[test]
    fn the_directory_appears_whole_or_not_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let names = || {
            let mut names: Vec<String> = fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        // A file name that can't be created (it's a directory path): the second write fails,
        // and neither the agent's directory nor the staging directory is left.
        let error =
            write_agent(dir.path(), "bunter", &[("SOUL.md", "hi"), ("no/such", "x")]).unwrap_err();
        assert!(matches!(error, CreateError::Io { .. }), "{error}");
        assert!(names().is_empty(), "{:?}", names());

        // Success: only the agent's directory, with only its files.
        write_agent(
            dir.path(),
            "bunter",
            &[("SOUL.md", "hi"), ("agent.toml", "x")],
        )
        .unwrap();
        assert_eq!(names(), ["bunter"]);
        let mut files: Vec<String> = fs::read_dir(dir.path().join("bunter"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files, ["SOUL.md", "agent.toml"]);

        // An existing directory (or file) is never touched.
        fs::create_dir(dir.path().join("taken")).unwrap();
        fs::write(dir.path().join("taken/keep"), "x").unwrap();
        let error = write_agent(dir.path(), "taken", &[("SOUL.md", "hi")]).unwrap_err();
        assert!(matches!(error, CreateError::Exists(_)), "{error}");
        assert!(dir.path().join("taken/keep").exists());
        assert!(!dir.path().join("taken/SOUL.md").exists());
        assert_eq!(names(), ["bunter", "taken"]);
    }

    #[test]
    fn staging_left_by_a_crash_is_removed_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(staging_name("butler"));
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join(".agent.toml.tmp"), "models = [").unwrap();
        for keep in ["jeeves", ".git", ".notes.creating.bak"] {
            fs::create_dir(dir.path().join(keep)).unwrap();
        }
        fs::write(dir.path().join(".file.creating"), "a file, not a directory").unwrap();
        remove_stale_staging(dir.path());
        assert!(!stale.exists());
        for keep in ["jeeves", ".git", ".notes.creating.bak", ".file.creating"] {
            assert!(dir.path().join(keep).exists(), "{keep}");
        }
    }
}
