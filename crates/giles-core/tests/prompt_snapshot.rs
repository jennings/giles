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
use giles_core::paths::DataRoot;
use giles_core::registry::Registry;
use giles_store::{Session, SessionId, SessionKind};
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
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        let home = Self { dir, root };
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
        for name in ["load_skill", "memory", "memory_search"] {
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
    let (channel, conversation) = match kind {
        SessionKind::Job => (None, None),
        _ => (
            Some("web".to_owned()),
            Some(ConversationId::new("owner").unwrap()),
        ),
    };
    Session {
        id: SessionId::new(1),
        agent: AgentId::new(agent).unwrap(),
        kind,
        channel,
        conversation,
        user: Some(owner()),
        started_at: Timestamp::UNIX_EPOCH,
        ended_at: None,
        last_message_at: None,
    }
}

async fn assemble(home: &Home, agent: &str, kind: SessionKind) -> SessionPrompt {
    let registry = home.registry();
    let config = registry.config().clone();
    let agent_config = &config.agents[&AgentId::new(agent).unwrap()];
    prompt::assemble(&registry, agent_config, &session(agent, kind))
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
/// one broken), memory files with front-matter, `USER.md`, and an allowlist that picks some
/// toolset and core tools.
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
        "agents/jeeves/MEMORY.md",
        "---\nreviewed: 2026-10-01\n---\nThe home server is `pantry`.\n§\nBackups run at 02:00.\n",
    );
    home.write(
        "users/owner/USER.md",
        "Stephen. Prefers metric units.\n§\nLives in Seattle.\n",
    );
    home.write(
        "agents/jeeves/users/owner/MEMORY.md",
        "Likes tea, not coffee.\n",
    );

    let prompt = assemble(&home, "jeeves", SessionKind::Direct).await;
    assert_snapshot("jeeves-direct", &render(&prompt));
}

/// An agent with nothing but models: no files, no skills, no tools. `system` still shows agent
/// and user memory as empty, with their caps.
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

/// An isolated job run with every tool allowed and a person-written memory file over the cap.
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
    home.write(
        "agents/scout/MEMORY.md",
        "The garden sensor reports soil moisture every hour.\n",
    );
    let prompt = assemble(&home, "scout", SessionKind::Job).await;
    assert_snapshot("scout-job", &render(&prompt));
}

/// Assembling twice from the same files gives the same prefix, byte for byte.
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
}
