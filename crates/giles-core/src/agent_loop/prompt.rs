//! Prompt assembly (PLAN sections 6.4, 6.5, 6.6, 6.7): the tools and `system` a session sends
//! with every request.
//!
//! Both are built once, when the session's first turn starts, and kept for the session's life,
//! so the prompt prefix (tools, then system) stays byte-identical and provider prompt caching
//! hits. Only compaction (TASKS.md 2.10) rebuilds `system`, with a fresh memory snapshot; the
//! tools never change within a session. The current time never goes in `system`: it travels in
//! each user message's time prefix ([`super::history`]).
//!
//! `system` is one text block with up to three parts, in this order:
//!
//! 1. `SOUL.md`, as written (omitted when missing or empty).
//! 2. `# Skills`: each skill's name and description, sorted by name (omitted without skills, and
//!    when the session doesn't offer `load_skill`, the only way to read one).
//! 3. `# Memory`: agent-wide `MEMORY.md`, then, in a direct (or job) session with a user, that
//!    user's `USER.md` and the agent's per-user `MEMORY.md`. The files the model writes carry
//!    the usage header `[67% — 1,474/2,200]`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use giles_store::{Session, SessionKind};
use giles_types::{Block, SkillSummary, ToolSpec, UserId};

use super::glob;
use crate::config::AgentConfig;
use crate::memory::{self, MemoryError};
use crate::paths::{DataRoot, PathsError};
use crate::registry::{IndexedTool, Registry, RegistryError, ToolSnapshot};

/// The core tool that reads a skill's instructions (PLAN section 6.6).
pub const LOAD_SKILL: &str = "load_skill";

/// The agent's identity file in `agents/<agent>/` (PLAN section 11.3).
pub const SOUL_FILE: &str = "SOUL.md";

/// What a memory section shows when its file is missing or empty.
pub const EMPTY_MEMORY: &str = "(empty)";

/// The prompt prefix of one session: its tools and its `system`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPrompt {
    /// The `system` blocks: one text block (empty when there is nothing to say).
    pub system: Vec<Block>,
    /// The tools offered, fixed for the session.
    pub tools: Arc<SessionTools>,
}

impl SessionPrompt {
    /// The text of `system`.
    pub fn system_text(&self) -> &str {
        match self.system.first() {
            Some(Block::Text { text }) => text,
            _ => "",
        }
    }
}

/// The tools a session offers: the registry's snapshot at session start, filtered by the agent's
/// `tools` allowlist globs (PLAN section 6.4: in v1, with no roles, the allowlist alone decides),
/// sorted by name. A tool that a toolset later drops stays listed; calling it gets "tool
/// unavailable".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionTools {
    tools: Vec<IndexedTool>,
}

impl SessionTools {
    /// The tools of `snapshot` (already sorted) that match one of `allowlist`.
    pub fn from_snapshot<S: AsRef<str>>(snapshot: &ToolSnapshot, allowlist: &[S]) -> Self {
        Self {
            tools: snapshot
                .tools()
                .iter()
                .filter(|tool| glob::matches_any(allowlist, &tool.spec.name))
                .cloned()
                .collect(),
        }
    }

    /// Every tool, sorted by name.
    pub fn tools(&self) -> &[IndexedTool] {
        &self.tools
    }

    /// The tool offered as `name`, if the session offers it.
    pub fn get(&self, name: &str) -> Option<&IndexedTool> {
        self.tools
            .binary_search_by(|tool| tool.spec.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.tools[i])
    }

    /// The specs, as a request carries them.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec.clone()).collect()
    }

    /// The number of tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the session offers no tools.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

/// Assembling a prompt failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PromptError {
    /// `SOUL.md` exists but can't be read (or isn't UTF-8).
    #[error("cannot read {}: {error}", .path.display())]
    Soul {
        /// The file.
        path: PathBuf,
        /// Why.
        error: io::Error,
    },
    /// A memory file exists but can't be read.
    #[error(transparent)]
    Memory(#[from] MemoryError),
    /// A memory file's path couldn't be built from the agent or user name.
    #[error(transparent)]
    Paths(#[from] PathsError),
    /// Skill discovery failed.
    #[error(transparent)]
    Skills(#[from] RegistryError),
}

/// One memory file as the prompt shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySection {
    /// The heading, e.g. `Agent memory`.
    pub title: &'static str,
    /// One line saying what the file is.
    pub intro: &'static str,
    /// The usage header, for files the model writes.
    pub usage: Option<String>,
    /// The entries joined by `§` lines, without front-matter.
    pub body: String,
}

/// Everything `system` is rendered from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemParts {
    /// `SOUL.md`, trimmed; `None` when missing or empty.
    pub soul: Option<String>,
    /// The skills, sorted by name. Empty when the session doesn't offer [`LOAD_SKILL`].
    pub skills: Vec<SkillSummary>,
    /// The memory snapshot, in prompt order.
    pub memory: Vec<MemorySection>,
}

/// Renders `system` from its parts. Pure, so the same files always give the same bytes.
pub fn render_system(parts: &SystemParts) -> String {
    let mut sections: Vec<String> = Vec::new();
    if let Some(soul) = &parts.soul {
        sections.push(soul.clone());
    }
    if !parts.skills.is_empty() {
        let mut skills = String::from(
            "# Skills\n\nA skill holds instructions for one kind of task. When a skill fits the \
             task, load its instructions with the `load_skill` tool before you start.\n",
        );
        for skill in &parts.skills {
            // Descriptions may span lines; one list item each keeps the list readable.
            let description = skill.description.split_whitespace().collect::<Vec<_>>();
            skills.push_str(&format!("\n- `{}`: {}", skill.name, description.join(" ")));
        }
        sections.push(skills);
    }
    if !parts.memory.is_empty() {
        sections.push(
            "# Memory\n\nMemory files hold what you keep across conversations, as entries \
             separated by lines holding only `§`. This is a snapshot taken when the conversation \
             started: changes made since then show up in later conversations."
                .to_owned(),
        );
        for section in &parts.memory {
            let heading = match &section.usage {
                Some(usage) => format!("## {} {usage}", section.title),
                None => format!("## {}", section.title),
            };
            let body = if section.body.is_empty() {
                EMPTY_MEMORY
            } else {
                &section.body
            };
            sections.push(format!("{heading}\n\n{}\n\n{body}", section.intro));
        }
    }
    sections.join("\n\n")
}

/// The user whose memory a session loads: the session's user in a direct or job session. Group
/// sessions load no user's memory (PLAN section 6.7), and v1 has none.
pub fn memory_user(session: &Session) -> Option<&UserId> {
    match session.kind {
        SessionKind::Direct | SessionKind::Job => session.user.as_ref(),
        _ => None,
    }
}

/// Reads `SOUL.md`, the agent's skills, and the memory snapshot for `session`. The skills are
/// left out unless `tools` (the session's) include [`LOAD_SKILL`]: without it the model can't
/// read a skill, so listing them would only invite failing calls.
///
/// # Errors
///
/// A file that exists but can't be read, or skill discovery failing. Missing files are empty.
pub async fn load_parts(
    registry: &Registry,
    agent: &AgentConfig,
    session: &Session,
    tools: &SessionTools,
) -> Result<SystemParts, PromptError> {
    let root = registry.root();
    let soul = read_soul(&agent.dir.join(SOUL_FILE)).await?;
    let skills = if tools.get(LOAD_SKILL).is_some() {
        registry.agent_skills(&agent.id).await?.summaries()
    } else {
        Vec::new()
    };
    let memory = load_memory(root, agent, memory_user(session)).await?;
    Ok(SystemParts {
        soul,
        skills,
        memory,
    })
}

/// The memory snapshot: agent-wide memory, then `user`'s profile and per-user memory.
async fn load_memory(
    root: &DataRoot,
    agent: &AgentConfig,
    user: Option<&UserId>,
) -> Result<Vec<MemorySection>, PromptError> {
    let cap = agent.memory_cap as usize;
    let doc = memory::read(&memory::agent_memory_file(root, agent.id.as_str())?).await?;
    let mut sections = vec![MemorySection {
        title: "Agent memory",
        intro: "What you remember in general, shared by all your conversations.",
        usage: Some(doc.usage_header(cap)),
        body: doc.body(),
    }];
    if let Some(user) = user {
        let profile = memory::read(&memory::user_profile_file(root, user.as_str())?).await?;
        sections.push(MemorySection {
            title: "User profile",
            intro: "The user's own description of themselves. Only the user edits it.",
            usage: None,
            body: profile.body(),
        });
        let path = memory::user_memory_file(root, agent.id.as_str(), user.as_str())?;
        let doc = memory::read(&path).await?;
        sections.push(MemorySection {
            title: "User memory",
            intro: "What you remember about this user.",
            usage: Some(doc.usage_header(cap)),
            body: doc.body(),
        });
    }
    Ok(sections)
}

/// `SOUL.md` without a byte-order mark and trailing whitespace; `None` if missing or blank.
async fn read_soul(path: &Path) -> Result<Option<String>, PromptError> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => {
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text).trim_end();
            let text = text.trim_start_matches(['\n', '\r']);
            Ok((!text.is_empty()).then(|| text.replace("\r\n", "\n")))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(PromptError::Soul {
            path: path.to_owned(),
            error,
        }),
    }
}

/// `system` for `session`, freshly read: at session start and after compaction. `tools` are the
/// session's tools: the skills are listed only if they include [`LOAD_SKILL`].
///
/// # Errors
///
/// As [`load_parts`].
pub async fn build_system(
    registry: &Registry,
    agent: &AgentConfig,
    session: &Session,
    tools: &SessionTools,
) -> Result<Vec<Block>, PromptError> {
    let text = render_system(&load_parts(registry, agent, session, tools).await?);
    Ok(if text.is_empty() {
        Vec::new()
    } else {
        vec![Block::text(text)]
    })
}

/// The whole prompt prefix for a session starting now: the registry's current tools filtered by
/// the agent's allowlist, and a fresh `system`.
///
/// # Errors
///
/// As [`load_parts`].
pub async fn assemble(
    registry: &Registry,
    agent: &AgentConfig,
    session: &Session,
) -> Result<SessionPrompt, PromptError> {
    let tools = SessionTools::from_snapshot(&registry.tools().snapshot(), &agent.tools);
    Ok(SessionPrompt {
        system: build_system(registry, agent, session, &tools).await?,
        tools: Arc::new(tools),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str) -> SkillSummary {
        SkillSummary {
            name: name.to_owned(),
            description: description.to_owned(),
        }
    }

    #[test]
    fn sections_come_in_plan_order_separated_by_blank_lines() {
        let parts = SystemParts {
            soul: Some("You are Jeeves.".to_owned()),
            skills: vec![skill("a", "First."), skill("b", "Two\n  lines.")],
            memory: vec![MemorySection {
                title: "Agent memory",
                intro: "General.",
                usage: Some("[0% — 0/2,200]".to_owned()),
                body: String::new(),
            }],
        };
        let text = render_system(&parts);
        let soul = text.find("You are Jeeves.").unwrap();
        let skills = text.find("# Skills").unwrap();
        let memory = text.find("# Memory").unwrap();
        assert!(soul < skills && skills < memory);
        assert!(text.contains("\n- `a`: First.\n- `b`: Two lines.\n\n# Memory"));
        assert!(text.ends_with("## Agent memory [0% — 0/2,200]\n\nGeneral.\n\n(empty)"));
    }

    #[test]
    fn missing_parts_leave_no_empty_sections() {
        assert_eq!(render_system(&SystemParts::default()), "");
        let only_soul = SystemParts {
            soul: Some("Soul.".to_owned()),
            ..SystemParts::default()
        };
        assert_eq!(render_system(&only_soul), "Soul.");
    }

    #[test]
    fn rendering_is_deterministic() {
        let parts = SystemParts {
            soul: Some("x".to_owned()),
            skills: vec![skill("a", "b")],
            memory: Vec::new(),
        };
        assert_eq!(render_system(&parts), render_system(&parts.clone()));
    }

    #[tokio::test]
    async fn soul_is_trimmed_and_a_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOUL_FILE);
        assert_eq!(read_soul(&path).await.unwrap(), None);
        std::fs::write(
            &path,
            "\u{feff}\r\n\r\nYou are Jeeves.\r\nBe brief.\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            read_soul(&path).await.unwrap().as_deref(),
            Some("You are Jeeves.\nBe brief.")
        );
        std::fs::write(&path, " \n\t\n").unwrap();
        assert_eq!(read_soul(&path).await.unwrap(), None);
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(matches!(
            read_soul(&path).await,
            Err(PromptError::Soul { .. })
        ));
    }
}
