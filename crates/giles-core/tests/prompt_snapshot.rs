//! Snapshot tests of the cached prompt prefix (PLAN sections 6.7, 14.1): the rendered `system`
//! and tool list for representative agents, from real files in a temporary data root. Any change
//! to the prefix changes a file under `tests/snapshots/`, so it shows up in review.
//!
//! To accept a change, run with `GILES_UPDATE_SNAPSHOTS=1` and review the diff.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use giles_core::agent_loop::prompt::{self, SessionPrompt};
use giles_core::config::{Config, InstanceName, LoadOptions};
use giles_core::memory_dbs::MemoryDbs;
use giles_core::paths::DataRoot;
use giles_core::registry::Registry;
use giles_store::{
    JournalMode, MemoryOwner, MemoryViewer, NewMemory, Session, SessionId, SessionKind,
};
use giles_types::{AgentId, ConversationId, Timestamp, ToolSpec, UserId};
use serde_json::json;
use tempfile::TempDir;

const GILES_TOML: &str = r#"
[owner]
identities = ["web:owner"]

[provider.anthropic]
config = { api_key = "sk-test" }

[toolset.github]
url = "https://example.com/github"

[toolset.fs]
url = "https://example.com/fs"
"#;

struct Home {
    dir: TempDir,
    root: DataRoot,
    memory_dbs: MemoryDbs,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        let memory_dbs = MemoryDbs::new(root.clone(), JournalMode::Wal);
        let home = Self {
            dir,
            root,
            memory_dbs,
        };
        home.write("giles.toml", GILES_TOML);
        home
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.dir.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn skill(&self, dir: &str, name: &str, description: &str) {
        self.write(
            &format!("{dir}/{name}/SKILL.md"),
            &format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n"),
        );
    }

    /// Adds a memory of `agent` as `viewer`, with room to spare under the cap (the prompt's cap
    /// is the agent's `memory_cap`, which a test may set lower than what it adds).
    async fn remember(
        &self,
        agent: &str,
        viewer: &MemoryViewer,
        content: &str,
        core: bool,
        shared: bool,
    ) {
        let store = self
            .memory_dbs
            .get(&AgentId::new(agent).unwrap())
            .await
            .unwrap();
        let new = NewMemory {
            content: content.to_owned(),
            core,
            shared,
            source_session: None,
        };
        store
            .add(viewer, new, 100_000, Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
    }

    fn registry(&self) -> Registry {
        let config = Config::load_with(
            &self.root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let registry = Registry::new(self.root.clone(), Arc::new(config));
        let tools = registry.tools();
        for name in [
            "load_skill",
            "memory_add",
            "memory_edit",
            "memory_forget",
            "memory_recall",
            "memory_search",
        ] {
            tools
                .set_core_tool(spec(name, &format!("The core tool {name}.")))
                .unwrap();
        }
        let github = InstanceName::new("github").unwrap();
        let errors = tools.set_toolset_tools(
            &github,
            vec![
                spec("list_issues", "List the issues of a repository."),
                spec("create_issue", "Create an issue."),
            ],
        );
        assert!(errors.is_empty());
        let fs = InstanceName::new("fs").unwrap();
        assert!(
            tools
                .set_toolset_tools(&fs, vec![spec("read_file", "Read a file.")])
                .is_empty()
        );
        registry
    }
}

fn spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": { "text": { "type": "string" } },
            "required": ["text"],
        }),
    }
}

fn owner() -> UserId {
    UserId::new("owner").unwrap()
}

fn session(agent: &str, kind: SessionKind) -> Session {
    let conversation = match kind {
        SessionKind::Job => "",
        _ => "owner",
    };
    session_of(agent, kind, Some(owner()), conversation)
}

/// A session of `kind` for `user` (none in a group), in `conversation` of the `web` channel (a
/// job session has none).
fn session_of(agent: &str, kind: SessionKind, user: Option<UserId>, conversation: &str) -> Session {
    let (channel, conversation) = match kind {
        SessionKind::Job => (None, None),
        _ => (
            Some("web".to_owned()),
            Some(ConversationId::new(conversation).unwrap()),
        ),
    };
    Session {
        id: SessionId::new(1),
        agent: AgentId::new(agent).unwrap(),
        kind,
        channel,
        conversation,
        user,
        started_at: Timestamp::UNIX_EPOCH,
        ended_at: None,
        last_message_at: None,
    }
}

fn group_session(agent: &str, conversation: &str) -> Session {
    session_of(agent, SessionKind::Group, None, conversation)
}

/// The memory viewer of `user`'s own turns: the agent owner may write shared memories.
fn viewer(user: &UserId) -> MemoryViewer {
    MemoryViewer::new(MemoryOwner::User(user.clone()), *user == owner())
}

/// The memory viewer of the group conversation `conversation` of the `web` channel.
fn group_viewer(conversation: &str) -> MemoryViewer {
    MemoryViewer::new(
        MemoryOwner::Group {
            instance: "web".to_owned(),
            conversation: ConversationId::new(conversation).unwrap(),
        },
        false,
    )
}

async fn assemble(home: &Home, agent: &str, kind: SessionKind) -> SessionPrompt {
    assemble_session(home, agent, &session(agent, kind)).await
}

async fn assemble_session(home: &Home, agent: &str, session: &Session) -> SessionPrompt {
    let registry = home.registry();
    let config = registry.config();
    let agent_config = &config.agents[&AgentId::new(agent).unwrap()];
    prompt::assemble(&registry, &home.memory_dbs, agent_config, session)
        .await
        .unwrap()
}

/// The snapshot text: `system`, then each tool with its description and schema.
fn render(prompt: &SessionPrompt) -> String {
    let mut out = format!(
        "=== system ({} block) ===\n{}\n=== tools ===\n",
        prompt.system.len(),
        prompt.system_text()
    );
    for tool in prompt.tools.tools() {
        let spec = &tool.spec;
        writeln!(
            out,
            "--- {}\n{}\n{}",
            spec.name,
            spec.description,
            serde_json::to_string(&spec.input_schema).unwrap()
        )
        .unwrap();
    }
    out
}

fn snapshot_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(format!("prompt-{name}.txt"))
}

fn assert_snapshot(name: &str, actual: &str) {
    let path = snapshot_path(name);
    if std::env::var_os("GILES_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}; run with GILES_UPDATE_SNAPSHOTS=1",
            path.display()
        )
    });
    assert!(
        expected == actual,
        "the prompt prefix changed ({}); if intended, rerun with GILES_UPDATE_SNAPSHOTS=1 and \
         review the diff.\n--- expected\n{expected}\n--- actual\n{actual}",
        path.display()
    );
}

/// An agent with everything: `SOUL.md`, global and agent skills (one overriding a global one,
/// one broken), core memories in the shared and the user's scope (and others that must stay out
/// of the prompt), `USER.md`, and an allowlist that picks some toolset and core tools.
#[tokio::test]
async fn full_agent_in_a_direct_session() {
    let home = Home::new();
    home.write(
        "agents/jeeves/agent.toml",
        "name = \"Jeeves\"\nmodels = [\"anthropic/claude-sonnet-4-5\"]\n\
         tools = [\"github__*\", \"memory*\", \"load_skill\"]\nmemory_cap = 1000\n",
    );
    home.write(
        "agents/jeeves/SOUL.md",
        "# Jeeves\n\nYou are Jeeves, a discreet and capable valet.\n\nKeep answers short.\n",
    );
    home.skill("skills", "weekly-review", "Prepare the weekly review.");
    home.skill(
        "skills",
        "travel",
        "Plan trips.\n  Use for flights,\n  hotels and trains.",
    );
    home.skill(
        "agents/jeeves/skills",
        "travel",
        "Plan trips the Jeeves way.",
    );
    home.skill(
        "agents/jeeves/skills",
        "allotment",
        "Look after the allotment.",
    );
    home.write("agents/jeeves/skills/broken/SKILL.md", "no front-matter");
    home.write(
        "users/owner/USER.md",
        "Stephen. Prefers metric units.\nLives in Seattle.\n",
    );
    let owner = viewer(&owner());
    home.remember("jeeves", &owner, "The home server is `pantry`.", true, true)
        .await;
    home.remember("jeeves", &owner, "Backups run at 02:00.", true, true)
        .await;
    home.remember(
        "jeeves",
        &owner,
        "The old server was `larder`.",
        false,
        true,
    )
    .await;
    home.remember("jeeves", &owner, "Likes tea, not coffee.", true, false)
        .await;
    home.remember("jeeves", &owner, "Takes the 07:45 train.", false, false)
        .await;
    let guest = viewer(&UserId::new("guest").unwrap());
    home.remember(
        "jeeves",
        &guest,
        "The guest's private core memory.",
        true,
        false,
    )
    .await;

    let prompt = assemble(&home, "jeeves", SessionKind::Direct).await;
    assert_snapshot("jeeves-direct", &render(&prompt));
    // The skills listed are the ones kept for `load_skill`, the agent's winning over a global.
    let names: Vec<_> = prompt.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["allotment", "travel", "weekly-review"]);
    assert_eq!(
        prompt.skills.get("travel").unwrap().description,
        "Plan trips the Jeeves way."
    );
}

/// An agent with nothing but models: no files, no memories, no skills, no tools. `system` still
/// shows the shared and the user's memories as empty, with their caps, and no mention of
/// `memory_recall`.
#[tokio::test]
async fn minimal_agent_in_a_direct_session() {
    let home = Home::new();
    home.write(
        "agents/bare/agent.toml",
        "models = [\"anthropic/claude-haiku-4-5\"]\n",
    );
    let prompt = assemble(&home, "bare", SessionKind::Direct).await;
    assert_snapshot("bare-direct", &render(&prompt));
}

/// An isolated job run with every tool allowed and a core memory over a lowered cap.
#[tokio::test]
async fn wildcard_agent_in_a_job_session() {
    let home = Home::new();
    home.write(
        "agents/scout/agent.toml",
        "models = [\"anthropic/claude-haiku-4-5\"]\ntools = [\"*\"]\nmemory_cap = 40\n",
    );
    home.write(
        "agents/scout/SOUL.md",
        "You check on things and report back.",
    );
    home.remember(
        "scout",
        &viewer(&owner()),
        "The garden sensor reports soil moisture every hour.",
        true,
        true,
    )
    .await;
    let prompt = assemble(&home, "scout", SessionKind::Job).await;
    assert_snapshot("scout-job", &render(&prompt));
}

/// Assembling twice from the same files and memories gives the same prefix, byte for byte.
#[tokio::test]
async fn assembly_is_stable() {
    let home = Home::new();
    home.write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/a\"]\ntools = [\"*\"]\n",
    );
    home.write("agents/jeeves/SOUL.md", "Soul.");
    home.skill("skills", "b-skill", "B.");
    home.skill("skills", "a-skill", "A.");
    home.remember("jeeves", &viewer(&owner()), "Tea at five.", true, false)
        .await;
    home.remember("jeeves", &viewer(&owner()), "Coffee at nine.", true, true)
        .await;
    let first = assemble(&home, "jeeves", SessionKind::Direct).await;
    let second = assemble(&home, "jeeves", SessionKind::Direct).await;
    assert_eq!(first, second);
    let names: Vec<_> = first.tools.tools().iter().map(|t| &t.spec.name).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

/// Without `load_skill` the model can't read a skill, so no skills are listed.
#[tokio::test]
async fn skills_are_listed_only_with_load_skill() {
    let home = Home::new();
    home.write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/a\"]\ntools = [\"github__*\", \"memory*\"]\n",
    );
    home.skill("skills", "weekly-review", "Prepare the weekly review.");
    let prompt = assemble(&home, "jeeves", SessionKind::Direct).await;
    assert!(prompt.tools.get("load_skill").is_none());
    assert!(!prompt.system_text().contains("# Skills"));
    assert!(!prompt.system_text().contains("weekly-review"));
    assert!(prompt.skills.is_empty());
}

/// The memory intro points to `memory_recall` only when the session offers it.
#[tokio::test]
async fn recall_is_mentioned_only_when_the_session_offers_it() {
    let home = Home::new();
    home.write(
        "agents/with/agent.toml",
        "models = [\"anthropic/a\"]\ntools = [\"memory_recall\"]\n",
    );
    home.write(
        "agents/without/agent.toml",
        "models = [\"anthropic/a\"]\ntools = [\"memory_add\", \"memory_search\"]\n",
    );
    let with = assemble(&home, "with", SessionKind::Direct).await;
    assert!(with.system_text().contains("`memory_recall`"));
    let without = assemble(&home, "without", SessionKind::Direct).await;
    assert!(without.system_text().contains("# Memory"));
    assert!(!without.system_text().contains("memory_recall"));
}

/// Shared core memories come first, then the session's own, each with its id, whatever the ids'
/// order. Memories that aren't core, and other users' and groups' private ones, stay out.
#[tokio::test]
async fn shared_memories_come_before_the_sessions_own_and_only_core_ones_appear() {
    let home = Home::new();
    home.write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/a\"]\ntools = [\"*\"]\n",
    );
    let owner = viewer(&owner());
    let guest = viewer(&UserId::new("guest").unwrap());
    let room = group_viewer("room");
    let other_room = group_viewer("elsewhere");
    // Ids 1 to 8, in this order.
    home.remember("jeeves", &owner, "Owner private core.", true, false)
        .await;
    home.remember("jeeves", &owner, "Shared core.", true, true)
        .await;
    home.remember("jeeves", &owner, "Owner private archive.", false, false)
        .await;
    home.remember("jeeves", &owner, "Shared archive.", false, true)
        .await;
    home.remember("jeeves", &guest, "Guest private core.", true, false)
        .await;
    home.remember("jeeves", &room, "Room private core.", true, false)
        .await;
    home.remember(
        "jeeves",
        &other_room,
        "Elsewhere private core.",
        true,
        false,
    )
    .await;
    home.remember("jeeves", &owner, "Second owner core.", true, false)
        .await;

    let text = assemble(&home, "jeeves", SessionKind::Direct)
        .await
        .system_text()
        .to_owned();
    let shared = text.find("#2 (core, shared): Shared core.").unwrap();
    let own = text.find("#1 (core): Owner private core.").unwrap();
    let second = text.find("#8 (core): Second owner core.").unwrap();
    let user_memory = text.find("## User memory").unwrap();
    assert!(text.find("## Shared memory").unwrap() < shared);
    assert!(shared < user_memory && user_memory < own && own < second);
    // The shared scope has 12 characters, the user's 19 + 18.
    assert!(text.contains("## Shared memory [0% — 12/2,200]"));
    assert!(text.contains("## User memory [1% — 37/2,200]"));
    for hidden in [
        "archive",
        "Guest private core.",
        "Room private core.",
        "Elsewhere private core.",
    ] {
        assert!(!text.contains(hidden), "{hidden}");
    }
}

/// A group session shows the group's core memories, and no user's: no user memory, no profile.
/// Who may write shared memories depends on who speaks, so the shared ones carry no usage header.
#[tokio::test]
async fn a_group_session_shows_the_groups_core_memories_only() {
    let home = Home::new();
    home.write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/a\"]\ntools = [\"*\"]\n",
    );
    home.write("users/owner/USER.md", "Stephen.");
    let owner = viewer(&owner());
    home.remember("jeeves", &owner, "Owner private core.", true, false)
        .await;
    home.remember("jeeves", &owner, "Shared core.", true, true)
        .await;
    home.remember(
        "jeeves",
        &group_viewer("room"),
        "The room likes pizza.",
        true,
        false,
    )
    .await;
    home.remember(
        "jeeves",
        &group_viewer("room"),
        "The room's archive.",
        false,
        false,
    )
    .await;
    home.remember(
        "jeeves",
        &group_viewer("elsewhere"),
        "Elsewhere core.",
        true,
        false,
    )
    .await;

    let text = assemble_session(&home, "jeeves", &group_session("jeeves", "room"))
        .await
        .system_text()
        .to_owned();
    assert!(text.contains("## Shared memory\n\n"), "{text}");
    assert!(text.contains("#2 (core, shared): Shared core."));
    assert!(text.contains("## Group memory [0% — 21/2,200]\n\n"));
    assert!(text.contains("What you remember in this group.\n\n#3 (core): The room likes pizza."));
    for hidden in [
        "Owner private core.",
        "archive",
        "Elsewhere core.",
        "## User memory",
        "## User profile",
        "Stephen.",
    ] {
        assert!(!text.contains(hidden), "{hidden}");
    }
}

/// A guest's DM shows the shared core memories without a usage header (the guest can't write
/// them) and the guest's own with one. Without shared memories the section is left out.
#[tokio::test]
async fn a_guest_dm_shows_shared_memories_read_only_and_its_own() {
    let home = Home::new();
    home.write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/a\"]\ntools = [\"*\"]\n",
    );
    let guest_id = UserId::new("guest").unwrap();
    let guest = viewer(&guest_id);
    let guest_session = || {
        session_of(
            "jeeves",
            SessionKind::Direct,
            Some(guest_id.clone()),
            "guest",
        )
    };

    let before = assemble_session(&home, "jeeves", &guest_session()).await;
    assert!(!before.system_text().contains("## Shared memory"));
    assert!(
        before
            .system_text()
            .contains("## User memory [0% — 0/2,200]")
    );

    home.remember("jeeves", &viewer(&owner()), "Shared core.", true, true)
        .await;
    home.remember(
        "jeeves",
        &viewer(&owner()),
        "Owner private core.",
        true,
        false,
    )
    .await;
    home.remember("jeeves", &guest, "Guest likes jazz.", true, false)
        .await;
    let text = assemble_session(&home, "jeeves", &guest_session())
        .await
        .system_text()
        .to_owned();
    assert!(text.contains(
        "## Shared memory\n\nWhat you remember in general, shared by all your conversations.\n\n\
         #1 (core, shared): Shared core."
    ));
    assert!(text.contains("## User memory [0% — 17/2,200]"));
    assert!(text.contains("#3 (core): Guest likes jazz."));
    assert!(!text.contains("Owner private core."));
}

/// A session with no user (nobody's private memories) shows the shared core memories only, and
/// no memory section at all when there are none.
#[tokio::test]
async fn a_session_without_a_user_shows_shared_memories_only() {
    let home = Home::new();
    home.write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/a\"]\ntools = [\"*\"]\n",
    );
    let job = || session_of("jeeves", SessionKind::Job, None, "");
    let empty = assemble_session(&home, "jeeves", &job()).await;
    assert_eq!(empty.system_text(), "");

    home.remember("jeeves", &viewer(&owner()), "Shared core.", true, true)
        .await;
    home.remember(
        "jeeves",
        &viewer(&owner()),
        "Owner private core.",
        true,
        false,
    )
    .await;
    let text = assemble_session(&home, "jeeves", &job())
        .await
        .system_text()
        .to_owned();
    assert!(text.contains("## Shared memory\n\n"));
    assert!(text.contains("#1 (core, shared): Shared core."));
    assert!(!text.contains("Owner private core."));
    assert!(!text.contains("## User memory"));
    assert!(!text.contains("## User profile"));
}

/// `memory_cap` caps each scope's core memories: every scope's usage header counts its own core
/// characters against it, and memories that aren't core don't count.
#[tokio::test]
async fn each_scope_counts_its_core_characters_against_memory_cap() {
    let home = Home::new();
    home.write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/a\"]\ntools = [\"*\"]\nmemory_cap = 100\n",
    );
    let owner = viewer(&owner());
    home.remember("jeeves", &owner, &"s".repeat(30), true, true)
        .await;
    home.remember("jeeves", &owner, &"u".repeat(60), true, false)
        .await;
    home.remember("jeeves", &owner, &"n".repeat(200), false, false)
        .await;
    home.remember(
        "jeeves",
        &group_viewer("room"),
        &"g".repeat(45),
        true,
        false,
    )
    .await;

    let dm = assemble(&home, "jeeves", SessionKind::Direct).await;
    assert!(dm.system_text().contains("## Shared memory [30% — 30/100]"));
    assert!(dm.system_text().contains("## User memory [60% — 60/100]"));

    let group = assemble_session(&home, "jeeves", &group_session("jeeves", "room")).await;
    assert!(
        group
            .system_text()
            .contains("## Group memory [45% — 45/100]")
    );
}
