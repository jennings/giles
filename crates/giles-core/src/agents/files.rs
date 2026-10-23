//! The files of an agent's directory, as its page shows them (PLAN section 11.3): `agent.toml`,
//! `SOUL.md`, `MEMORY.md`, per-user and per-group memory, job prompt files, skills, and anything
//! else in it.
//!
//! [`list_files`] walks the directory without following links: a link is listed, never followed,
//! and every file is opened with `O_NOFOLLOW` and checked to be inside the agent's directory once
//! open, so a link or a directory swapped in during the walk can't show a file from elsewhere
//! (like `load_skill`'s resource reads). Names starting with `.` are skipped, as config loading
//! and skill discovery skip them. Caps keep the page small: [`MAX_FILES`] entries,
//! [`MAX_FILE_BYTES`] shown per file, [`MAX_TOTAL_BYTES`] shown in all, [`MAX_DEPTH`] levels.
//! Contents that aren't UTF-8 text are listed with their size, not shown.
//!
//! Which files a viewer sees is decided in one place, [`FileKind::visible_to`]: for now the owner
//! (the only user, and the only one who can open the agent pages) sees everything.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use giles_types::UserId;

use crate::config::{AgentConfig, JobPrompt};

/// The most entries (files, links, and other non-directories) listed.
pub const MAX_FILES: usize = 200;

/// The most bytes of one file shown; a longer file is cut off there.
pub const MAX_FILE_BYTES: u64 = 64 * 1024;

/// The most bytes of all files shown together; later files are listed without their contents.
pub const MAX_TOTAL_BYTES: u64 = 1024 * 1024;

/// How deep below the agent's directory the walk goes (`groups/<instance>/<conversation>/` is 3).
pub const MAX_DEPTH: usize = 8;

/// What a file is to the agent (PLAN section 11.3).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FileKind {
    /// `agent.toml`.
    Config,
    /// `SOUL.md`.
    Soul,
    /// `MEMORY.md`: the agent-wide memory.
    AgentMemory,
    /// `users/<user>/MEMORY.md`: what the agent remembers about `user`.
    UserMemory {
        /// The user.
        user: String,
    },
    /// `groups/<instance>/<conversation>/MEMORY.md`: what the agent learned in a group.
    GroupMemory {
        /// The channel instance.
        instance: String,
        /// The conversation id, decoded from its percent-encoded directory name.
        conversation: String,
    },
    /// The `prompt_file` of config job `job`.
    JobPrompt {
        /// The job's id.
        job: String,
    },
    /// A file of the agent-local skill `skill` (`skills/<skill>/...`).
    Skill {
        /// The skill's directory name.
        skill: String,
    },
    /// Anything else.
    Other,
}

impl FileKind {
    /// Of the file at `path`, relative to the agent's directory and split into its components,
    /// with `prompts` the agent's job prompt files as `(job id, relative path)`.
    pub fn of(path: &[&str], prompts: &[(String, PathBuf)]) -> Self {
        if let Some((job, _)) = prompts
            .iter()
            .find(|(_, file)| file.iter().eq(path.iter().map(std::ffi::OsStr::new)))
        {
            return Self::JobPrompt { job: job.clone() };
        }
        match path {
            ["agent.toml"] => Self::Config,
            ["SOUL.md"] => Self::Soul,
            ["MEMORY.md"] => Self::AgentMemory,
            ["users", user, "MEMORY.md"] => Self::UserMemory {
                user: (*user).to_owned(),
            },
            ["groups", instance, conversation, "MEMORY.md"] => Self::GroupMemory {
                instance: (*instance).to_owned(),
                conversation: percent_decode(conversation),
            },
            ["skills", skill, _, ..] => Self::Skill {
                skill: (*skill).to_owned(),
            },
            _ => Self::Other,
        }
    }

    /// Whether `viewer` may see a file of this kind. The one place that decides it: v1 has one
    /// user, the owner, who is the only one who can open an agent's page and sees every file.
    /// With several users (PLAN section 3.1), a user sees only their own
    /// [`UserMemory`](Self::UserMemory) of the agent; that check goes here.
    pub fn visible_to(&self, viewer: &UserId) -> bool {
        let _ = viewer;
        true
    }

    /// How the page describes the file, if it's one Giles knows.
    pub fn label(&self) -> Option<String> {
        Some(match self {
            Self::Config => "Configuration".to_owned(),
            Self::Soul => "Who the agent is".to_owned(),
            Self::AgentMemory => "Agent-wide memory".to_owned(),
            Self::UserMemory { user } => format!("Memory about user {user}"),
            Self::GroupMemory {
                instance,
                conversation,
            } => format!("Memory from group {conversation} on {instance}"),
            Self::JobPrompt { job } => format!("Prompt of job {job}"),
            Self::Skill { skill } => format!("Skill {skill}"),
            Self::Other => return None,
        })
    }

    /// Where the page lists it: the agent's own files first, then memory, prompts, skills, and
    /// the rest.
    fn rank(&self) -> u8 {
        match self {
            Self::Config => 0,
            Self::Soul => 1,
            Self::AgentMemory => 2,
            Self::UserMemory { .. } => 3,
            Self::GroupMemory { .. } => 4,
            Self::JobPrompt { .. } => 5,
            Self::Skill { .. } => 6,
            Self::Other => 7,
        }
    }
}

/// What the page shows of a file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Content {
    /// UTF-8 text: the first [`MAX_FILE_BYTES`] at most (`truncated` if there was more).
    Text {
        /// The text shown.
        text: String,
        /// The file's size in bytes.
        size: u64,
        /// Whether the file is longer than `text`.
        truncated: bool,
    },
    /// Not UTF-8 text (or with NUL bytes).
    Binary {
        /// The file's size in bytes.
        size: u64,
    },
    /// Not shown: the page already shows [`MAX_TOTAL_BYTES`].
    NotShown {
        /// The file's size in bytes.
        size: u64,
    },
    /// A symbolic link; never followed.
    Link,
    /// A FIFO, socket, or device, or a file that turned into something else (or moved outside
    /// the agent's directory) while it was being opened.
    Special,
    /// It couldn't be read.
    Unreadable(String),
}

/// One entry of an agent's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFile {
    /// Its path relative to the agent's directory, with `/` between components.
    pub path: String,
    /// What it is to the agent.
    pub kind: FileKind,
    /// What the page shows of it.
    pub content: Content,
}

/// An agent's files, as [`list_files`] found them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentFiles {
    /// The entries, ordered by kind, then path.
    pub files: Vec<AgentFile>,
    /// Whether there were more than [`MAX_FILES`] entries, or directories deeper than
    /// [`MAX_DEPTH`], that aren't listed.
    pub truncated: bool,
}

/// Lists the files in `agent`'s directory that `viewer` may see ([`FileKind::visible_to`]), with
/// their contents within the caps. Blocking: run it on a blocking thread.
///
/// # Errors
///
/// An I/O error if the agent's directory itself can't be resolved or listed. Problems with one
/// file are that file's [`Content::Unreadable`].
pub fn list_files(agent: &AgentConfig, viewer: &UserId) -> io::Result<AgentFiles> {
    let root = fs::canonicalize(&agent.dir)?;
    let prompts: Vec<(String, PathBuf)> = agent
        .jobs
        .iter()
        .filter_map(|job| match &job.prompt {
            JobPrompt::File(path) => path
                .strip_prefix(&agent.dir)
                .ok()
                .map(|relative| (job.id.clone(), relative.to_path_buf())),
            _ => None,
        })
        .collect();
    let mut walk = Walk {
        root: &root,
        prompts: &prompts,
        viewer,
        found: Vec::new(),
        truncated: false,
    };
    walk.dir(&root, &mut Vec::new())?;
    let Walk {
        mut found,
        truncated,
        ..
    } = walk;
    found.sort_by(|a, b| (a.kind.rank(), &a.relative).cmp(&(b.kind.rank(), &b.relative)));
    let mut shown = 0;
    let files = found
        .into_iter()
        .map(|entry| {
            let content = match entry.what {
                Found::Link => Content::Link,
                Found::Special => Content::Special,
                Found::File(size) if shown >= MAX_TOTAL_BYTES => Content::NotShown { size },
                Found::File(_) => {
                    let content = read_file(&root, &entry.path);
                    if let Content::Text { text, .. } = &content {
                        shown += text.len() as u64;
                    }
                    content
                }
            };
            AgentFile {
                path: entry.relative,
                kind: entry.kind,
                content,
            }
        })
        .collect();
    Ok(AgentFiles { files, truncated })
}

/// An entry the walk found, before its contents are read.
struct Entry {
    path: PathBuf,
    relative: String,
    kind: FileKind,
    what: Found,
}

enum Found {
    /// A regular file of this size (as listed).
    File(u64),
    Link,
    Special,
}

struct Walk<'a> {
    root: &'a Path,
    prompts: &'a [(String, PathBuf)],
    viewer: &'a UserId,
    found: Vec<Entry>,
    truncated: bool,
}

impl Walk<'_> {
    /// Lists `dir` (at `parts` below the root) in name order, descending into directories.
    fn dir(&mut self, dir: &Path, parts: &mut Vec<String>) -> io::Result<()> {
        let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            // Enough found: stop walking, rather than list and sort every directory below.
            if self.found.len() >= MAX_FILES {
                self.truncated = true;
                return Ok(());
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            // `file_type` doesn't follow a link: a link is listed as one.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            parts.push(name);
            if file_type.is_dir() {
                if parts.len() > MAX_DEPTH {
                    self.truncated = true;
                } else {
                    // A directory that can't be listed is left out.
                    let _ = self.dir(&entry.path(), parts);
                }
            } else {
                self.add(entry, &file_type, parts);
            }
            parts.pop();
        }
        Ok(())
    }

    fn add(&mut self, entry: fs::DirEntry, file_type: &fs::FileType, parts: &[String]) {
        let borrowed: Vec<&str> = parts.iter().map(String::as_str).collect();
        let kind = FileKind::of(&borrowed, self.prompts);
        if !kind.visible_to(self.viewer) {
            return;
        }
        if self.found.len() >= MAX_FILES {
            self.truncated = true;
            return;
        }
        let what = if file_type.is_symlink() {
            Found::Link
        } else if file_type.is_file() {
            Found::File(entry.metadata().map_or(0, |m| m.len()))
        } else {
            Found::Special
        };
        self.found.push(Entry {
            path: self.root.join(parts.iter().collect::<PathBuf>()),
            relative: parts.join("/"),
            kind,
            what,
        });
    }
}

/// Reads up to [`MAX_FILE_BYTES`] of the regular file `path` inside `root` (canonical) without
/// following a link at the last component (`O_NOFOLLOW`; `O_NONBLOCK` so a FIFO swapped in can't
/// block), then checks the file actually opened is inside `root`: a directory above it swapped
/// for a link would still be followed by the open.
fn read_file(root: &Path, path: &Path) -> Content {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return Content::Link,
        Err(error) => return Content::Unreadable(error.to_string()),
    };
    match crate::skills::fd_path(&file) {
        Ok(actual) if actual.starts_with(root) && actual != root => {}
        Ok(_) => return Content::Special,
        Err(error) => return Content::Unreadable(error.to_string()),
    }
    read_open(file)
}

fn read_open(file: File) -> Content {
    let meta = match file.metadata() {
        Ok(meta) if meta.is_file() => meta,
        Ok(_) => return Content::Special,
        Err(error) => return Content::Unreadable(error.to_string()),
    };
    let mut bytes =
        Vec::with_capacity(usize::try_from(meta.len().min(MAX_FILE_BYTES)).unwrap_or(0));
    // One byte past the cap shows that there is more.
    if let Err(error) = file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes) {
        return Content::Unreadable(error.to_string());
    }
    let truncated = bytes.len() as u64 > MAX_FILE_BYTES;
    bytes.truncate(usize::try_from(MAX_FILE_BYTES).unwrap_or(usize::MAX));
    let size = meta.len().max(bytes.len() as u64);
    match text(bytes, truncated) {
        Some(text) => Content::Text {
            text,
            size,
            truncated,
        },
        None => Content::Binary { size },
    }
}

/// `bytes` as text, if they are UTF-8 without NUL bytes. When the file was cut off, a character
/// split by the cut is dropped.
fn text(mut bytes: Vec<u8>, truncated: bool) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    match std::str::from_utf8(&bytes) {
        Ok(_) => {}
        // An incomplete sequence at the very end (`error_len` is `None`) is the cut's doing.
        Err(error) if truncated && error.error_len().is_none() => {
            bytes.truncate(error.valid_up_to());
        }
        Err(_) => return None,
    }
    String::from_utf8(bytes).ok()
}

/// Decodes `%XX` escapes (PLAN section 11.3: conversation ids are percent-encoded in paths, `#giles`
/// as `%23giles`). A `%` not followed by two hex digits stays as it is; bytes that aren't UTF-8
/// become U+FFFD.
pub fn percent_decode(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|&b| hex(b)),
                bytes.get(i + 2).and_then(|&b| hex(b)),
            )
        {
            out.push(u8::try_from(hi * 16 + lo).expect("two hex digits fit a byte"));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_follow_the_agent_layout() {
        let prompts = [("morning".to_owned(), PathBuf::from("prompts/morning.md"))];
        let kind = |path: &str| {
            let parts: Vec<&str> = path.split('/').collect();
            FileKind::of(&parts, &prompts)
        };
        assert_eq!(kind("agent.toml"), FileKind::Config);
        assert_eq!(kind("SOUL.md"), FileKind::Soul);
        assert_eq!(kind("MEMORY.md"), FileKind::AgentMemory);
        assert_eq!(
            kind("users/owner/MEMORY.md"),
            FileKind::UserMemory {
                user: "owner".into()
            }
        );
        assert_eq!(
            kind("groups/libera/%23giles/MEMORY.md"),
            FileKind::GroupMemory {
                instance: "libera".into(),
                conversation: "#giles".into()
            }
        );
        assert_eq!(
            kind("prompts/morning.md"),
            FileKind::JobPrompt {
                job: "morning".into()
            }
        );
        assert_eq!(
            kind("skills/greet/SKILL.md"),
            FileKind::Skill {
                skill: "greet".into()
            }
        );
        for other in [
            "notes.md",
            "users/owner/notes.md",
            "users/MEMORY.md",
            "skills/README.md",
            "sub/agent.toml",
        ] {
            assert_eq!(kind(other), FileKind::Other, "{other}");
        }
        assert_eq!(
            kind("groups/libera/%23giles/MEMORY.md").label().unwrap(),
            "Memory from group #giles on libera"
        );
        assert_eq!(FileKind::Other.label(), None);
        // v1: the owner sees everything.
        assert!(
            FileKind::UserMemory {
                user: "someone".into()
            }
            .visible_to(&crate::identity::owner())
        );
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("%23giles"), "#giles");
        assert_eq!(percent_decode("a%2Fb%2fc"), "a/b/c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
        assert_eq!(percent_decode("%C3%A9t%C3%A9"), "été");
        assert_eq!(percent_decode("%FF"), "\u{fffd}");
    }

    #[test]
    fn text_is_utf8_without_nul_and_a_cut_character_is_dropped() {
        assert_eq!(text(b"hello".to_vec(), false).as_deref(), Some("hello"));
        assert_eq!(text(b"a\0b".to_vec(), false), None);
        assert_eq!(text(vec![0xff, b'a'], false), None);
        // "é" is two bytes; cut after the first.
        assert_eq!(text(vec![b'a', 0xc3], true).as_deref(), Some("a"));
        assert_eq!(text(vec![b'a', 0xc3], false), None);
    }
}
