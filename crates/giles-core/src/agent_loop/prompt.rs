//! Prompt assembly (PLAN sections 6.4, 6.5, 6.6, 6.7): the tools and `system` a session sends
//! with every request.
//!
//! Both are built once, when the session's first turn starts, and kept for the session's life,
//! so the prompt prefix (tools, then system) stays byte-identical and provider prompt caching
//! hits. Only compaction rebuilds `system`, with a fresh memory snapshot; the
//! tools never change within a session. The current time never goes in `system`: it travels in
//! each user message's time prefix ([`super::history`]).
//!
//! `system` is one text block with up to three parts, in this order:
//!
//! 1. `SOUL.md`, as written (omitted when missing or empty).
//! 2. `# Skills`: each skill's name and description, sorted by name (omitted without skills, and
//!    when the session doesn't offer `load_skill`, the only way to read one).
//! 3. `# Memory`: the core memories visible in the session, each with its id (`#12 (core): ...`,
//!    as [`memory::line`] renders it), read from the agent's memory database. Non-core memories
//!    are never loaded: the model finds them with `memory_recall`, which the intro mentions only
//!    when the session offers that tool. The sections, in order:
//!    - the shared memories (visible in every session), with the usage header
//!      `[67% — 1,474/2,200]` only when the session's user may write them, and left out when
//!      there are none and they can't be written;
//!    - the session's own memories, always with the usage header: the user's in a direct or job
//!      session, the group's in a group session;
//!    - in a direct or job session with a user, that user's `USER.md` profile (written by
//!      people; front-matter dropped).
//!
//!    What the snapshot shows comes from [`memory::prompt_viewer`], one viewer per session. The
//!    prompt is built per session, not per turn, so it can't tell a config job's turn (no
//!    requester, whose tools see shared memories only) from the user's: a config job's session
//!    still shows its session user's own memories. A session with no user and no group shows
//!    shared memories only.
//!
//!    The snapshot is taken when the session's first turn starts and again after compaction.
//!    Memories written meanwhile show in tool results, not in the prompt.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use giles_store::{CoreScope, MemoryOwner, Session, SessionKind};
use giles_types::{Block, SkillSummary, ToolSpec, UserId};

use super::glob;
use crate::config::AgentConfig;
use crate::front_matter;
use crate::memory;
use crate::memory_dbs::MemoryDbs;
use crate::paths::PathsError;
use crate::registry::{IndexedTool, Registry, RegistryError, ToolSnapshot};
use crate::skills::SkillSet;

/// The core tool that reads a skill's instructions (PLAN section 6.6).
pub const LOAD_SKILL: &str = "load_skill";

/// The agent's identity file in `agents/<agent>/` (PLAN section 11.3).
pub const SOUL_FILE: &str = "SOUL.md";

/// What a memory section shows when it holds nothing.
pub const EMPTY_MEMORY: &str = "(empty)";

/// The core tool that searches every memory visible in the session (PLAN section 6.7).
pub const MEMORY_RECALL: &str = "memory_recall";

/// The heading of the shared memories' section.
pub const SHARED_MEMORY_TITLE: &str = "Shared memory";

/// The heading of the section with a user's own memories.
pub const USER_MEMORY_TITLE: &str = "User memory";

/// The heading of the section with a group's own memories.
pub const GROUP_MEMORY_TITLE: &str = "Group memory";

/// The heading of the user's profile (`USER.md`).
pub const USER_PROFILE_TITLE: &str = "User profile";

/// The prompt prefix of one session: its tools and its `system`, and the skills `system` lists.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionPrompt {
    /// The `system` blocks: one text block (empty when there is nothing to say).
    pub system: Vec<Block>,
    /// The tools offered, fixed for the session.
    pub tools: Arc<SessionTools>,
    /// The skills `system` lists, read when it was built: `load_skill` answers from these, so
    /// the model can load exactly the skills it was shown. Empty unless the session offers
    /// [`LOAD_SKILL`].
    pub skills: Arc<SkillSet>,
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
    /// `USER.md` exists but can't be read.
    #[error("cannot read {}: {error}", .path.display())]
    Profile {
        /// The file.
        path: PathBuf,
        /// Why.
        error: io::Error,
    },
    /// The agent's memory database failed.
    #[error("memory database: {0}")]
    Memory(#[from] giles_store::Error),
    /// The user's profile path couldn't be built from the user name.
    #[error(transparent)]
    Paths(#[from] PathsError),
    /// Skill discovery failed.
    #[error(transparent)]
    Skills(#[from] RegistryError),
}

/// One memory section as the prompt shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemorySection {
    /// The heading, e.g. `Shared memory`.
    pub title: &'static str,
    /// One line saying what the section is.
    pub intro: &'static str,
    /// The usage header, for scopes the model can write.
    pub usage: Option<String>,
    /// The memories, one per line ([`memory::line`]), or the profile's text.
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
    /// Whether the session offers [`MEMORY_RECALL`]: the memory intro then points to it.
    pub recall: bool,
}

/// Renders `system` from its parts. Pure, so the same parts always give the same bytes.
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
        let recall = if parts.recall {
            " Memories that aren't core aren't loaded here: find them with the `memory_recall` tool."
        } else {
            ""
        };
        sections.push(format!(
            "# Memory\n\nThis is what you keep across conversations: your core memories, each \
             with its id.{recall} It is a snapshot taken when the conversation started: changes \
             made since then show up in later conversations."
        ));
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

/// The skills a session lists: the agent's skills if `tools` (the session's) include
/// [`LOAD_SKILL`], else none. Without it the model can't read a skill, so listing them would only
/// invite failing calls.
///
/// # Errors
///
/// Skill discovery failing.
pub async fn session_skills(
    registry: &Registry,
    agent: &AgentConfig,
    tools: &SessionTools,
) -> Result<SkillSet, PromptError> {
    Ok(if tools.get(LOAD_SKILL).is_some() {
        registry.agent_skills(&agent.id).await?
    } else {
        SkillSet::default()
    })
}

/// Reads `SOUL.md` and the memory snapshot for `session`, with `skills` (the session's,
/// [`session_skills`]) and `tools` (the session's, which decide whether the memory intro mentions
/// [`MEMORY_RECALL`]).
///
/// # Errors
///
/// A file that exists but can't be read, or the agent's memory database failing. Missing files
/// are empty.
pub async fn load_parts(
    registry: &Registry,
    memory_dbs: &MemoryDbs,
    agent: &AgentConfig,
    session: &Session,
    skills: &SkillSet,
    tools: &SessionTools,
) -> Result<SystemParts, PromptError> {
    let soul = read_soul(&agent.dir.join(SOUL_FILE)).await?;
    let memory = load_memory(registry, memory_dbs, agent, session).await?;
    Ok(SystemParts {
        soul,
        skills: skills.summaries(),
        memory,
        recall: tools.get(MEMORY_RECALL).is_some(),
    })
}

/// The memory snapshot: the shared core memories, then the session's own, then the user's
/// profile. See the module docs for what each section shows and when.
async fn load_memory(
    registry: &Registry,
    memory_dbs: &MemoryDbs,
    agent: &AgentConfig,
    session: &Session,
) -> Result<Vec<MemorySection>, PromptError> {
    let cap = agent.memory_cap as usize;
    let viewer = memory::prompt_viewer(session);
    let core = memory_dbs
        .get(&agent.id)
        .await?
        .core_memories(&viewer)
        .await?;

    let mut sections = Vec::new();
    if viewer.can_write_shared || !core.shared.memories.is_empty() {
        sections.push(scope_section(
            SHARED_MEMORY_TITLE,
            "What you remember in general, shared by all your conversations.",
            &core.shared,
            viewer.can_write_shared.then_some(cap),
        ));
    }
    let own = match &viewer.owner {
        MemoryOwner::User(_) => Some((USER_MEMORY_TITLE, "What you remember about this user.")),
        MemoryOwner::Group { .. } => Some((GROUP_MEMORY_TITLE, "What you remember in this group.")),
        MemoryOwner::None => None,
    };
    if let Some((title, intro)) = own {
        sections.push(scope_section(title, intro, &core.own, Some(cap)));
    }
    if let Some(user) = memory_user(session) {
        let path = memory::user_profile_file(registry.root(), user.as_str())?;
        sections.push(MemorySection {
            title: USER_PROFILE_TITLE,
            intro: "The user's own description of themselves. Only the user edits it.",
            usage: None,
            body: read_profile(&path).await?.unwrap_or_default(),
        });
    }
    Ok(sections)
}

/// A section listing one scope's core memories, oldest id first, under the usage header against
/// `cap` if the model can write the scope.
fn scope_section(
    title: &'static str,
    intro: &'static str,
    scope: &CoreScope,
    cap: Option<usize>,
) -> MemorySection {
    let lines: Vec<String> = scope.memories.iter().map(memory::line).collect();
    MemorySection {
        title,
        intro,
        usage: cap.map(|cap| memory::usage_header(scope.chars, cap)),
        body: lines.join("\n"),
    }
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

/// `USER.md` without front-matter, a byte-order mark, and surrounding whitespace; `None` if
/// missing or blank.
async fn read_profile(path: &Path) -> Result<Option<String>, PromptError> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => {
            let body = front_matter::split(&text).map_or(text.as_str(), |split| split.body);
            let body = body.strip_prefix('\u{feff}').unwrap_or(body).trim();
            Ok((!body.is_empty()).then(|| body.replace("\r\n", "\n")))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(PromptError::Profile {
            path: path.to_owned(),
            error,
        }),
    }
}

/// `system` for `session`, freshly read, listing `skills` (the session's, [`session_skills`]) and
/// offering `tools`.
///
/// # Errors
///
/// As [`load_parts`].
pub async fn build_system(
    registry: &Registry,
    memory_dbs: &MemoryDbs,
    agent: &AgentConfig,
    session: &Session,
    skills: &SkillSet,
    tools: &SessionTools,
) -> Result<Vec<Block>, PromptError> {
    let parts = load_parts(registry, memory_dbs, agent, session, skills, tools).await?;
    let text = render_system(&parts);
    Ok(if text.is_empty() {
        Vec::new()
    } else {
        vec![Block::text(text)]
    })
}

/// The prompt prefix for `session` offering `tools`, with its skills and `system` freshly read:
/// at session start, and after compaction with the session's tools.
///
/// # Errors
///
/// As [`session_skills`] and [`load_parts`].
pub async fn build(
    registry: &Registry,
    memory_dbs: &MemoryDbs,
    agent: &AgentConfig,
    session: &Session,
    tools: Arc<SessionTools>,
) -> Result<SessionPrompt, PromptError> {
    let skills = session_skills(registry, agent, &tools).await?;
    Ok(SessionPrompt {
        system: build_system(registry, memory_dbs, agent, session, &skills, &tools).await?,
        tools,
        skills: Arc::new(skills),
    })
}

/// The whole prompt prefix for a session starting now: the registry's current tools filtered by
/// the agent's allowlist, and [`build`].
///
/// # Errors
///
/// As [`build`].
pub async fn assemble(
    registry: &Registry,
    memory_dbs: &MemoryDbs,
    agent: &AgentConfig,
    session: &Session,
) -> Result<SessionPrompt, PromptError> {
    let tools = SessionTools::from_snapshot(&registry.tools().snapshot(), &agent.tools);
    build(registry, memory_dbs, agent, session, Arc::new(tools)).await
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
                title: SHARED_MEMORY_TITLE,
                intro: "General.",
                usage: Some("[0% — 0/2,200]".to_owned()),
                body: String::new(),
            }],
            recall: false,
        };
        let text = render_system(&parts);
        let soul = text.find("You are Jeeves.").unwrap();
        let skills = text.find("# Skills").unwrap();
        let memory = text.find("# Memory").unwrap();
        assert!(soul < skills && skills < memory);
        assert!(text.contains("\n- `a`: First.\n- `b`: Two lines.\n\n# Memory"));
        assert!(text.ends_with("## Shared memory [0% — 0/2,200]\n\nGeneral.\n\n(empty)"));
    }

    #[test]
    fn the_memory_intro_mentions_recall_only_when_the_session_offers_it() {
        let mut parts = SystemParts {
            memory: vec![MemorySection {
                title: USER_MEMORY_TITLE,
                intro: "Mine.",
                usage: None,
                body: "#1 (core): Tea.".to_owned(),
            }],
            ..SystemParts::default()
        };
        assert!(!render_system(&parts).contains("memory_recall"));
        parts.recall = true;
        let text = render_system(&parts);
        assert!(text.contains("find them with the `memory_recall` tool"));
        assert!(text.ends_with("## User memory\n\nMine.\n\n#1 (core): Tea."));
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
            recall: true,
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

    #[tokio::test]
    async fn profile_drops_front_matter_and_surrounding_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("USER.md");
        assert_eq!(read_profile(&path).await.unwrap(), None);
        std::fs::write(
            &path,
            "---\nreviewed: 2026-10-01\n---\r\n\r\nStephen.\r\nLives in Seattle.\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            read_profile(&path).await.unwrap().as_deref(),
            Some("Stephen.\nLives in Seattle.")
        );
        std::fs::write(&path, "---\nonly: front-matter\n---\n").unwrap();
        assert_eq!(read_profile(&path).await.unwrap(), None);
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert!(matches!(
            read_profile(&path).await,
            Err(PromptError::Profile { .. })
        ));
    }
}
