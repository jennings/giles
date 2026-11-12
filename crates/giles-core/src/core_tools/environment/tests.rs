//! The workspace tools through [`CoreTool::call`], against an in-process environment
//! ([`Fake`]) and a real store. The agent `jeeves` may use the environment `box`, `other` may
//! use `far` (which is never connected), and `plain` has no environments.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use giles_store::{DeleteReason, SessionId, SessionKind, Store};
use giles_types::{
    AgentId, Block, CallContext, ConversationId, Environment as _, EnvironmentInfo, ExecEvent,
    ExecExit, Timestamp,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use super::bash::{self, BashTool};
use super::files::{self, EditTool, ReadTool, WriteTool};
use super::lifecycle::{
    EnvironmentCreateTool, EnvironmentDeleteTool, EnvironmentPluginsTool, EnvironmentResetTool,
    EnvironmentStatusTool,
};
use super::search::{self, FindTool, GrepTool};
use super::*;
use crate::agent_loop::{CallPolicy, PARALLEL_SAFE_CORE_TOOLS, SessionTools};
use crate::config::{Config, InstanceName, LoadOptions};
use crate::core_tools::{CoreTool, TurnContext};
use crate::paths::DataRoot;
use crate::registry::ENVIRONMENT_TOOLS;
use crate::workspaces::LeaseSlot;
use crate::workspaces::fake::{Fake, TestClock};

const OPTIONS: &str = r#"{"image":"alpine"}"#;

struct Setup {
    _dir: TempDir,
    registry: Arc<Registry>,
    workspaces: Workspaces,
    fake: Arc<Fake>,
    env: Env,
}

fn name(name: &str) -> InstanceName {
    InstanceName::new(name).unwrap()
}

impl Setup {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        let write = |relative: &str, contents: &str| {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        write(
            "giles.toml",
            "[owner]\nidentities = [\"fake:owner\"]\n\
             [provider.anthropic]\nconfig = { api_key = \"sk-test\" }\n\
             [environment.box]\ncomponent = \"docker\"\naddress = \"http://127.0.0.1:9000\"\n\
             description = \"Disposable boxes.\"\n\
             [environment.far]\ncomponent = \"docker\"\naddress = \"http://127.0.0.1:9001\"\n\
             description = \"Never connected.\"\n",
        );
        let agent = |extra: &str| format!("models = [\"anthropic/test-model\"]\n{extra}");
        write(
            "agents/jeeves/agent.toml",
            &agent("tools = [\"*\"]\nenvironments = [\"box\"]\n"),
        );
        write(
            "agents/other/agent.toml",
            &agent("tools = [\"*\"]\nenvironments = [\"far\"]\n"),
        );
        write("agents/plain/agent.toml", &agent("tools = [\"*\"]\n"));
        let config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let registry = Arc::new(Registry::new(root.clone(), Arc::new(config)));
        let store = Store::open(root.database_file()).await.unwrap();
        let clock = TestClock::new();
        let fake = Fake::new(clock.clone());
        let workspaces = Workspaces::new(store, registry.clone(), clock);
        registry
            .register_environment(
                &name("box"),
                fake.clone(),
                EnvironmentInfo {
                    options_schema: json!({"type": "object", "properties": {"image": {"type": "string"}}}),
                },
            )
            .unwrap();
        let env = Env::new(workspaces.clone(), &registry);
        Self {
            _dir: dir,
            registry,
            workspaces,
            fake,
            env,
        }
    }

    fn tool<T: CoreTool + 'static>(&self, make: fn(Env) -> T) -> T {
        make(self.env.clone())
    }

    /// A turn of `agent` in the conversation `chat`.
    fn turn(&self, agent: &str, chat: &str) -> Turn {
        let session = giles_store::Session {
            id: SessionId::new(1),
            agent: AgentId::new(agent).unwrap(),
            kind: SessionKind::Direct,
            channel: Some("fake".to_owned()),
            conversation: Some(ConversationId::new(chat).unwrap()),
            user: None,
            started_at: Timestamp::UNIX_EPOCH,
            ended_at: None,
            last_message_at: None,
        };
        Turn::new(session)
    }

    /// A turn of `jeeves` in `chat`, which already has a workspace in `box`.
    async fn with_workspace(&self) -> (Turn, giles_types::WorkspaceId) {
        let turn = self.turn("jeeves", "chat");
        let created = turn
            .run(
                &self.tool(EnvironmentCreateTool),
                json!({"plugin": "box", "options": {"image": "alpine"}}),
            )
            .await;
        ok(&created);
        let id = self
            .workspaces
            .current(&turn.owner())
            .await
            .unwrap()
            .unwrap()
            .id;
        (turn, id)
    }

    fn started_with(&self, events: Vec<ExecEvent>) {
        self.fake.script_exec(events);
    }
}

/// One turn: its context, shared by its calls (so they share a lease).
#[derive(Clone)]
struct Turn(Arc<TurnContext>);

impl Turn {
    fn new(session: giles_store::Session) -> Self {
        Self(Arc::new(TurnContext {
            session,
            reply: None,
            skills: Arc::default(),
            job: false,
            lease: LeaseSlot::default(),
        }))
    }

    fn owner(&self) -> WorkspaceOwner {
        WorkspaceOwner::of_session(&self.0.session)
    }

    async fn run(&self, tool: &dyn CoreTool, input: Value) -> ToolOutput {
        tool.call(CoreCall {
            id: "t1".to_owned(),
            input,
            context: CallContext {
                agent: self.0.session.agent.clone(),
                requester: None,
                conversation: None,
            },
            turn: self.0.clone(),
            media: Arc::default(),
        })
        .await
    }
}

fn text(output: &ToolOutput) -> &str {
    match output.content.as_slice() {
        [Block::Text { text }] => text,
        other => panic!("not one text block: {other:?}"),
    }
}

fn ok(output: &ToolOutput) -> &str {
    assert!(!output.is_error, "error: {}", text(output));
    text(output)
}

fn err(output: &ToolOutput) -> &str {
    assert!(output.is_error, "not an error: {}", text(output));
    text(output)
}

fn exit(code: i32) -> ExecEvent {
    ExecEvent::Exit(ExecExit {
        code,
        timed_out: false,
        dropped_bytes: 0,
    })
}

fn out(text: &str) -> ExecEvent {
    ExecEvent::Stdout(text.as_bytes().to_vec())
}

fn errs(text: &str) -> ExecEvent {
    ExecEvent::Stderr(text.as_bytes().to_vec())
}

// -- Offering and the allowlist --

#[tokio::test]
async fn environment_tools_are_offered_only_to_agents_with_an_environment() {
    let setup = Setup::new().await;
    for spec in all_specs(&setup) {
        setup.registry.tools().set_core_tool(spec).unwrap();
    }
    let config = setup.registry.config();
    let snapshot = setup.registry.tools().snapshot();
    let offered = |agent: &str| -> Vec<String> {
        SessionTools::for_agent(&snapshot, &config.agents[&AgentId::new(agent).unwrap()])
            .tools()
            .iter()
            .map(|tool| tool.spec.name.clone())
            .collect()
    };
    let jeeves = offered("jeeves");
    for name in ENVIRONMENT_TOOLS {
        assert!(jeeves.iter().any(|offered| offered == name), "{name}");
    }
    // `tools = ["*"]` allows every core tool, but `plain` has no environment.
    assert_eq!(offered("plain"), Vec::<String>::new());
    // The agent's own `tools` globs still apply on top.
    let only_read = SessionTools::from_snapshot(&snapshot, &["read"]);
    assert_eq!(only_read.len(), 1);
}

fn all_specs(setup: &Setup) -> Vec<giles_types::ToolSpec> {
    vec![
        setup.tool(EnvironmentPluginsTool).spec(),
        setup.tool(EnvironmentCreateTool).spec(),
        setup.tool(EnvironmentStatusTool).spec(),
        setup.tool(EnvironmentResetTool).spec(),
        setup.tool(EnvironmentDeleteTool).spec(),
        setup.tool(BashTool).spec(),
        setup.tool(ReadTool).spec(),
        setup.tool(WriteTool).spec(),
        setup.tool(EditTool).spec(),
        setup.tool(FindTool).spec(),
        setup.tool(GrepTool).spec(),
    ]
}

#[tokio::test]
async fn specs_are_the_environment_tools_with_closed_schemas() {
    let setup = Setup::new().await;
    let mut names: Vec<String> = Vec::new();
    for spec in all_specs(&setup) {
        assert!(
            crate::registry::is_environment_tool(&spec.name),
            "{}",
            spec.name
        );
        assert_eq!(spec.input_schema["type"], json!("object"), "{}", spec.name);
        assert_eq!(
            spec.input_schema["additionalProperties"],
            json!(false),
            "{}",
            spec.name
        );
        for (property, definition) in spec.input_schema["properties"].as_object().unwrap() {
            let description = definition["description"].as_str().unwrap_or_default();
            assert!(!description.is_empty(), "{}.{property}", spec.name);
        }
        names.push(spec.name);
    }
    names.sort();
    assert_eq!(names, ENVIRONMENT_TOOLS);
}

#[test]
fn only_the_reading_tools_are_parallel_safe() {
    for name in ENVIRONMENT_TOOLS {
        let reads = matches!(
            name,
            "read" | "find" | "grep" | "environment_status" | "environment_plugins"
        );
        assert_eq!(PARALLEL_SAFE_CORE_TOOLS.contains(&name), reads, "{name}");
    }
}

#[tokio::test]
async fn bash_gets_a_deadline_above_its_longest_timeout() {
    let setup = Setup::new().await;
    for spec in all_specs(&setup) {
        setup.registry.tools().set_core_tool(spec).unwrap();
    }
    let snapshot = setup.registry.tools().snapshot();
    let tools = SessionTools::from_snapshot(&snapshot, &["*"]);
    let config = setup.registry.config();
    let bash = CallPolicy::of(&tools, &config, "bash");
    assert!(bash.deadline > bash::MAX_TIMEOUT);
    assert!(!bash.parallel);
    let read = CallPolicy::of(&tools, &config, "read");
    assert!(read.parallel);
    assert!(read.deadline < bash.deadline);
}

#[tokio::test]
async fn a_tool_refuses_an_environment_the_agent_may_not_use() {
    let setup = Setup::new().await;
    // `other` may use only `far`.
    let turn = setup.turn("other", "chat");
    let create = turn
        .run(&setup.tool(EnvironmentCreateTool), json!({"plugin": "box"}))
        .await;
    let message = err(&create);
    assert!(
        message.contains("`box` is not one you may use"),
        "{message}"
    );
    assert!(message.contains("`far`"), "{message}");

    // A workspace in `box` that belongs to `other` (the agent's allowlist changed since):
    // every tool refuses it.
    setup
        .workspaces
        .create(&turn.owner(), "box", json!({}))
        .await
        .unwrap();
    for (tool, input) in [
        (
            Box::new(setup.tool(BashTool)) as Box<dyn CoreTool>,
            json!({"command": "ls"}),
        ),
        (Box::new(setup.tool(ReadTool)), json!({"path": "a"})),
        (Box::new(setup.tool(EnvironmentStatusTool)), json!({})),
        (Box::new(setup.tool(EnvironmentDeleteTool)), json!({})),
        (Box::new(setup.tool(EnvironmentResetTool)), json!({})),
    ] {
        let spec = tool.spec();
        let output = turn.run(tool.as_ref(), input).await;
        let message = err(&output);
        assert!(
            message.contains("`box` is not one you may use"),
            "{}: {message}",
            spec.name
        );
    }
    assert!(
        setup
            .workspaces
            .current(&turn.owner())
            .await
            .unwrap()
            .is_some()
    );
    assert!(setup.fake.execs().is_empty());
}

#[tokio::test]
async fn an_agent_without_environments_has_nothing_to_call() {
    let setup = Setup::new().await;
    let turn = setup.turn("plain", "chat");
    let output = turn
        .run(&setup.tool(BashTool), json!({"command": "ls"}))
        .await;
    assert!(err(&output).contains("has no environments"));
    let output = turn
        .run(&setup.tool(EnvironmentPluginsTool), json!({}))
        .await;
    assert!(err(&output).contains("has no environments"));
}

// -- environment_plugins --

#[tokio::test]
async fn plugins_lists_the_agents_environments_as_they_are_now() {
    let setup = Setup::new().await;
    let turn = setup.turn("jeeves", "chat");
    let tool = setup.tool(EnvironmentPluginsTool);
    let listed = ok(&turn.run(&tool, json!({})).await).to_owned();
    assert!(listed.contains("## `box`"), "{listed}");
    assert!(listed.contains("Disposable boxes."), "{listed}");
    assert!(listed.contains("Reachable: yes"), "{listed}");
    assert!(listed.contains("\"image\""), "{listed}");
    // Not `far`, which the agent may not use.
    assert!(!listed.contains("far"), "{listed}");

    // The list is read at call time: the plugin goes away.
    setup.registry.unregister_environment(&name("box")).unwrap();
    let listed = ok(&turn.run(&tool, json!({})).await).to_owned();
    assert!(listed.contains("Reachable: no"), "{listed}");
    // Its schema is kept from the last connection.
    assert!(listed.contains("\"image\""), "{listed}");

    let other = setup.turn("other", "chat");
    let listed = ok(&other.run(&tool, json!({})).await).to_owned();
    assert!(listed.contains("## `far`"), "{listed}");
    assert!(listed.contains("Reachable: no"), "{listed}");
    assert!(
        listed.contains("not known until it first connects"),
        "{listed}"
    );
}

// -- create, status, reset, delete --

#[tokio::test]
async fn a_workspace_tool_without_a_workspace_says_how_to_make_one() {
    let setup = Setup::new().await;
    let turn = setup.turn("jeeves", "chat");
    let output = turn
        .run(&setup.tool(BashTool), json!({"command": "ls"}))
        .await;
    let message = err(&output);
    assert!(message.contains("no environment yet"), "{message}");
    assert!(message.contains("- `box`: Disposable boxes."), "{message}");
    assert!(message.contains("environment_plugins"), "{message}");
    assert!(message.contains("environment_create"), "{message}");
    assert!(!message.contains("far"), "{message}");
    assert!(setup.fake.execs().is_empty());
}

#[tokio::test]
async fn an_operator_deleting_the_workspace_mid_turn_is_reported_on_the_next_call() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    setup.started_with(vec![out("hi\n"), exit(0)]);
    // The turn is using the workspace: its first call took the lease.
    ok(&turn
        .run(&setup.tool(BashTool), json!({"command": "echo hi"}))
        .await);
    assert!(setup.workspaces.in_use(&id));

    setup.workspaces.delete_by_id(&id).await.unwrap();
    assert!(!setup.fake.has(&id));

    let output = turn
        .run(&setup.tool(BashTool), json!({"command": "echo hi"}))
        .await;
    let message = err(&output);
    assert!(message.contains("was deleted at "), "{message}");
    assert!(message.contains("an operator deleted it"), "{message}");
    assert!(message.contains("environment_create"), "{message}");
}

#[tokio::test]
async fn after_a_deletion_the_error_says_when_why_and_what_it_was() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let deleted = turn
        .run(&setup.tool(EnvironmentDeleteTool), json!({}))
        .await;
    assert!(ok(&deleted).contains("everything in it is gone"));
    assert!(!setup.fake.has(&id));

    let output = turn
        .run(&setup.tool(ReadTool), json!({"path": "notes.txt"}))
        .await;
    let message = err(&output);
    assert!(message.contains("was deleted at "), "{message}");
    assert!(message.contains("you deleted it"), "{message}");
    assert!(message.contains("`box`"), "{message}");
    assert!(message.contains(OPTIONS), "{message}");
    assert!(message.contains("environment_create"), "{message}");

    // Nothing is created automatically, and creating again works.
    assert!(
        setup
            .workspaces
            .current(&turn.owner())
            .await
            .unwrap()
            .is_none()
    );
    let created = turn
        .run(
            &setup.tool(EnvironmentCreateTool),
            json!({"plugin": "box", "options": {"image": "alpine"}}),
        )
        .await;
    ok(&created);

    // Another conversation of the agent never had one.
    let other_chat = setup.turn("jeeves", "elsewhere");
    let output = other_chat
        .run(&setup.tool(ReadTool), json!({"path": "notes.txt"}))
        .await;
    assert!(err(&output).contains("no environment yet"));
}

#[tokio::test]
async fn the_reason_a_workspace_went_is_told() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    setup
        .workspaces
        .delete(&turn.owner(), DeleteReason::UserReset)
        .await
        .unwrap();
    let output = turn
        .run(&setup.tool(BashTool), json!({"command": "ls"}))
        .await;
    assert!(
        err(&output).contains("the user reset it"),
        "{}",
        text(&output)
    );
}

#[tokio::test]
async fn create_makes_one_workspace_per_conversation() {
    let setup = Setup::new().await;
    let turn = setup.turn("jeeves", "chat");
    let tool = setup.tool(EnvironmentCreateTool);
    let created = turn
        .run(
            &tool,
            json!({"plugin": "box", "options": {"image": "alpine"}}),
        )
        .await;
    let created = ok(&created);
    assert!(created.contains("Environment: `box`"), "{created}");
    assert!(created.contains(OPTIONS), "{created}");
    assert!(created.contains("Working directory: /work"), "{created}");
    assert!(
        created.contains("What persists: files persist"),
        "{created}"
    );

    let again = turn.run(&tool, json!({"plugin": "box"})).await;
    let message = err(&again);
    assert!(message.contains("already has a workspace"), "{message}");
    assert!(message.contains("`box`"), "{message}");
    assert!(message.contains(OPTIONS), "{message}");
    assert_eq!(setup.fake.held().len(), 1);

    // Another conversation has its own.
    let elsewhere = setup.turn("jeeves", "elsewhere");
    ok(&elsewhere.run(&tool, json!({"plugin": "box"})).await);
    assert_eq!(setup.fake.held().len(), 2);
}

#[tokio::test]
async fn create_reports_what_the_environment_refuses_and_what_is_unreachable() {
    let setup = Setup::new().await;
    let turn = setup.turn("jeeves", "chat");
    let tool = setup.tool(EnvironmentCreateTool);
    setup.fake.fail_next_create(
        giles_types::Error::InvalidRequest("the image `evil` is not allowed".to_owned()),
        false,
    );
    let refused = turn
        .run(
            &tool,
            json!({"plugin": "box", "options": {"image": "evil"}}),
        )
        .await;
    assert_eq!(err(&refused), "the image `evil` is not allowed");
    // The slot is free again.
    ok(&turn.run(&tool, json!({"plugin": "box"})).await);

    let bad = setup.turn("jeeves", "bad");
    let output = bad
        .run(&tool, json!({"plugin": "box", "options": "alpine"}))
        .await;
    assert!(err(&output).contains("invalid") || err(&output).contains("JSON object"));

    setup.registry.unregister_environment(&name("box")).unwrap();
    let down = setup.turn("jeeves", "down");
    let output = down.run(&tool, json!({"plugin": "box"})).await;
    assert!(err(&output).contains("not reachable"));
}

#[tokio::test]
async fn status_shows_the_workspace_and_what_became_of_the_last_one() {
    let setup = Setup::new().await;
    let turn = setup.turn("jeeves", "chat");
    let tool = setup.tool(EnvironmentStatusTool);
    let none = turn.run(&tool, json!({})).await;
    assert!(err(&none).contains("no environment yet"));

    let created = turn
        .run(
            &setup.tool(EnvironmentCreateTool),
            json!({"plugin": "box", "options": {"image": "alpine"}}),
        )
        .await;
    ok(&created);
    let status = ok(&turn.run(&tool, json!({})).await).to_owned();
    assert!(status.contains("Environment: `box`"), "{status}");
    assert!(status.contains(OPTIONS), "{status}");
    assert!(status.contains("State: running"), "{status}");
    assert!(status.contains("Created: "), "{status}");
    assert!(status.contains("Last used: "), "{status}");

    let id = setup
        .workspaces
        .current(&turn.owner())
        .await
        .unwrap()
        .unwrap()
        .id;
    setup.fake.stop_workspace(&id).await.unwrap();
    let status = ok(&turn.run(&tool, json!({})).await).to_owned();
    assert!(status.contains("State: stopped"), "{status}");
    // Asking doesn't start it.
    assert_eq!(setup.fake.running(&id), Some(false));
}

#[tokio::test]
async fn reset_replaces_the_workspace_with_the_same_environment_and_options() {
    let setup = Setup::new().await;
    let (turn, old) = setup.with_workspace().await;
    setup.fake.put_file(&old, "notes.txt", b"hello");
    let reset = turn.run(&setup.tool(EnvironmentResetTool), json!({})).await;
    let reset = ok(&reset).to_owned();
    assert!(reset.contains("Deleted the previous workspace"), "{reset}");
    assert!(reset.contains(OPTIONS), "{reset}");
    assert!(reset.contains("everything in it is gone"), "{reset}");
    assert!(reset.contains("Created a new workspace"), "{reset}");

    let new = setup
        .workspaces
        .current(&turn.owner())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(new.id, old);
    assert_eq!(new.environment, "box");
    assert_eq!(new.options, serde_json::from_str::<Value>(OPTIONS).unwrap());
    assert!(!setup.fake.has(&old));
    assert!(setup.fake.has(&new.id));

    // The old files are gone.
    let output = turn
        .run(&setup.tool(ReadTool), json!({"path": "notes.txt"}))
        .await;
    assert!(err(&output).contains("no such file"));
    // The previous workspace was deleted by the model.
    let last = setup
        .workspaces
        .last_deleted(&turn.owner())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.delete_reason, Some(DeleteReason::ModelReset));
}

#[tokio::test]
async fn reset_takes_a_new_plugin_and_options() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    let reset = turn
        .run(
            &setup.tool(EnvironmentResetTool),
            json!({"options": {"image": "debian"}}),
        )
        .await;
    ok(&reset);
    let current = setup
        .workspaces
        .current(&turn.owner())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.options, json!({"image": "debian"}));

    // A plugin the agent may not use is refused before anything is deleted.
    let before = current.id.clone();
    let other = turn
        .run(&setup.tool(EnvironmentResetTool), json!({"plugin": "far"}))
        .await;
    assert!(err(&other).contains("`far` is not one you may use"));
    let current = setup
        .workspaces
        .current(&turn.owner())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.id, before);
}

#[tokio::test]
async fn reset_checks_before_it_deletes() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let tool = setup.tool(EnvironmentResetTool);
    let bad = turn.run(&tool, json!({"options": "debian"})).await;
    assert!(err(&bad).contains("JSON object"));
    setup.registry.unregister_environment(&name("box")).unwrap();
    let down = turn.run(&tool, json!({})).await;
    assert!(err(&down).contains("not reachable"));
    assert!(setup.fake.has(&id));
    assert_eq!(
        setup
            .workspaces
            .current(&turn.owner())
            .await
            .unwrap()
            .unwrap()
            .id,
        id
    );
}

#[tokio::test]
async fn reset_says_what_happened_when_creating_fails() {
    let setup = Setup::new().await;
    let (turn, old) = setup.with_workspace().await;
    setup.fake.fail_next_create(
        giles_types::Error::InvalidRequest("no space left".to_owned()),
        false,
    );
    let reset = turn.run(&setup.tool(EnvironmentResetTool), json!({})).await;
    let message = err(&reset);
    assert!(
        message.contains("Deleted the previous workspace"),
        "{message}"
    );
    assert!(message.contains("no space left"), "{message}");
    assert!(message.contains("environment_create"), "{message}");
    assert!(!setup.fake.has(&old));
    assert!(
        setup
            .workspaces
            .current(&turn.owner())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn reset_with_no_workspace_needs_a_plugin() {
    let setup = Setup::new().await;
    let turn = setup.turn("jeeves", "chat");
    let tool = setup.tool(EnvironmentResetTool);
    let none = turn.run(&tool, json!({})).await;
    assert!(err(&none).contains("no environment yet"));
    let created = turn.run(&tool, json!({"plugin": "box"})).await;
    let created = ok(&created);
    assert!(created.contains("no workspace to discard"), "{created}");
    assert!(
        setup
            .workspaces
            .current(&turn.owner())
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn delete_with_no_workspace_is_guidance() {
    let setup = Setup::new().await;
    let turn = setup.turn("jeeves", "chat");
    let output = turn
        .run(&setup.tool(EnvironmentDeleteTool), json!({}))
        .await;
    assert!(err(&output).contains("no environment yet"));
}

#[tokio::test]
async fn reset_and_delete_say_when_to_go_ahead_and_when_to_ask() {
    let setup = Setup::new().await;
    for spec in [
        setup.tool(EnvironmentResetTool).spec(),
        setup.tool(EnvironmentDeleteTool).spec(),
    ] {
        assert!(
            spec.description.contains("Go ahead if the user clearly"),
            "{}",
            spec.name
        );
        assert!(spec.description.contains("ask them first"), "{}", spec.name);
    }
}

// -- Lifecycle notices --

#[tokio::test]
async fn a_lifecycle_notice_leads_the_next_result() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    setup.fake.put_file(&id, "a.txt", b"one\n");
    // Idle: the sweep stopped it.
    setup.fake.stop_workspace(&id).await.unwrap();
    let read = turn
        .run(&setup.tool(ReadTool), json!({"path": "a.txt"}))
        .await;
    let read = ok(&read);
    let (notice, rest) = read.split_once("\n\n").unwrap();
    assert!(
        notice.contains("stopped and has been started again"),
        "{notice}"
    );
    assert!(rest.contains("one"), "{rest}");

    // The notice comes only once.
    let read = turn
        .run(&setup.tool(ReadTool), json!({"path": "a.txt"}))
        .await;
    assert!(!ok(&read).contains("started again"));

    // Recreated, and the call then fails: the notice still leads the error.
    setup.fake.stop_workspace(&id).await.unwrap();
    setup.fake.recreate_next_start();
    let read = turn
        .run(&setup.tool(ReadTool), json!({"path": "gone.txt"}))
        .await;
    let message = err(&read);
    assert!(
        message.starts_with("The workspace's environment was gone and has been recreated"),
        "{message}"
    );
    assert!(
        message.contains("Installed packages are gone."),
        "{message}"
    );
    assert!(message.ends_with("no such file: gone.txt"), "{message}");

    // It leads a bash result too.
    setup.fake.stop_workspace(&id).await.unwrap();
    setup.started_with(vec![out("hi\n"), exit(0)]);
    let ran = turn
        .run(&setup.tool(BashTool), json!({"command": "echo hi"}))
        .await;
    let ran = ok(&ran);
    assert!(ran.starts_with("The workspace was stopped"), "{ran}");
    assert!(ran.contains("Exit code: 0"), "{ran}");
}

#[tokio::test]
async fn a_workspace_the_environment_lost_is_reported_and_can_be_replaced() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    setup.fake.remove(&id);
    let output = turn.run(&setup.tool(ReadTool), json!({"path": "a"})).await;
    assert!(
        err(&output).contains("no longer exists"),
        "{}",
        text(&output)
    );
    let output = turn.run(&setup.tool(ReadTool), json!({"path": "a"})).await;
    assert!(
        err(&output).contains("removed outside Giles"),
        "{}",
        text(&output)
    );
    ok(&turn
        .run(&setup.tool(EnvironmentCreateTool), json!({"plugin": "box"}))
        .await);
}

#[tokio::test]
async fn a_job_run_has_its_own_workspace() {
    let setup = Setup::new().await;
    let session = giles_store::Session {
        id: SessionId::new(7),
        agent: AgentId::new("jeeves").unwrap(),
        kind: SessionKind::Job,
        channel: None,
        conversation: None,
        user: None,
        started_at: Timestamp::UNIX_EPOCH,
        ended_at: None,
        last_message_at: None,
    };
    let turn = Turn::new(session);
    let none = turn.run(&setup.tool(ReadTool), json!({"path": "a"})).await;
    assert!(err(&none).contains("no environment yet"));
    ok(&turn
        .run(&setup.tool(EnvironmentCreateTool), json!({"plugin": "box"}))
        .await);
    let id = setup
        .workspaces
        .current(&turn.owner())
        .await
        .unwrap()
        .unwrap()
        .id;
    setup.fake.put_file(&id, "a", b"x");
    ok(&turn.run(&setup.tool(ReadTool), json!({"path": "a"})).await);
    assert!(matches!(turn.owner(), WorkspaceOwner::Job { .. }));
}

// -- bash --

#[tokio::test]
async fn bash_shows_exit_code_stdout_and_stderr() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    setup.started_with(vec![
        out("hello\n"),
        errs("warning\n"),
        out("bye\n"),
        exit(0),
    ]);
    let ran = turn
        .run(&setup.tool(BashTool), json!({"command": "echo hello"}))
        .await;
    assert_eq!(
        ok(&ran),
        "Exit code: 0\n\nstdout:\nhello\nbye\n\nstderr:\nwarning\n"
    );

    let request = setup.fake.execs().pop().unwrap();
    assert_eq!(request.command, "echo hello");
    assert_eq!(request.timeout, bash::DEFAULT_TIMEOUT);
    assert_eq!(request.timeout, Duration::from_secs(120));
    assert_eq!(request.output_limit, bash::EXEC_OUTPUT_LIMIT);

    setup.started_with(vec![exit(0)]);
    let quiet = turn
        .run(&setup.tool(BashTool), json!({"command": "true"}))
        .await;
    assert_eq!(ok(&quiet), "Exit code: 0\n(no output)\n");
}

#[tokio::test]
async fn bash_fails_on_a_nonzero_exit() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    setup.started_with(vec![errs("no such file\n"), exit(2)]);
    let ran = turn
        .run(&setup.tool(BashTool), json!({"command": "cat nope"}))
        .await;
    let message = err(&ran);
    assert!(message.starts_with("Exit code: 2\n"), "{message}");
    assert!(message.contains("stderr:\nno such file"), "{message}");
}

#[tokio::test]
async fn bash_takes_a_timeout_up_to_ten_minutes() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    setup.started_with(vec![exit(0)]);
    let tool = setup.tool(BashTool);
    ok(&turn
        .run(&tool, json!({"command": "sleep 1", "timeout": 600}))
        .await);
    assert_eq!(
        setup.fake.execs().pop().unwrap().timeout,
        Duration::from_secs(600)
    );
    for bad in [0, 601, 3600] {
        let output = turn
            .run(&tool, json!({"command": "x", "timeout": bad}))
            .await;
        assert!(err(&output).contains("between 1 and 600"), "{bad}");
    }
    assert_eq!(setup.fake.execs().len(), 1);
    let empty = turn.run(&tool, json!({"command": "  "})).await;
    assert!(err(&empty).contains("command is empty"));
}

#[tokio::test]
async fn bash_reports_a_command_the_environment_timed_out() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    setup.started_with(vec![
        out("working\n"),
        ExecEvent::Exit(ExecExit {
            code: 137,
            timed_out: true,
            dropped_bytes: 0,
        }),
    ]);
    let ran = turn
        .run(
            &setup.tool(BashTool),
            json!({"command": "sleep 100", "timeout": 5}),
        )
        .await;
    let message = err(&ran);
    assert!(message.starts_with("Exit code: 137\n"), "{message}");
    assert!(
        message.contains("Timed out: the command was killed after 5 seconds."),
        "{message}"
    );
    assert!(message.contains("working"), "{message}");
}

#[tokio::test]
async fn bash_keeps_the_head_and_the_tail_of_long_output() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    // 100 pieces of 1,000 bytes: `a` lines to start with, then `b`, and an end marker.
    let mut events = Vec::new();
    for n in 0..100 {
        let fill = if n < 50 { 'a' } else { 'b' };
        events.push(out(&format!("{}\n", fill.to_string().repeat(999))));
    }
    events.push(out("THE END\n"));
    events.push(exit(0));
    setup.started_with(events);
    let ran = turn
        .run(&setup.tool(BashTool), json!({"command": "yes"}))
        .await;
    let ran = ok(&ran);
    let total = 100 * 1000 + "THE END\n".len();
    let dropped = total - bash::HEAD_BYTES - bash::TAIL_BYTES;
    assert!(
        ran.contains(&format!("[... {dropped} bytes of output left out ...]")),
        "{}",
        &ran[..200.min(ran.len())]
    );
    assert!(
        ran.ends_with("THE END\n"),
        "{}",
        &ran[ran.len().saturating_sub(100)..]
    );
    assert!(ran.contains("stdout:\naaaa"));
    assert!(
        ran.len() < bash::HEAD_BYTES + bash::TAIL_BYTES + 300,
        "{}",
        ran.len()
    );
    // Head and tail are both there, the middle is not.
    assert!(ran.contains("bbbb"));
    let kept_a = ran.matches('a').count();
    assert!(kept_a >= bash::HEAD_BYTES - 100, "{kept_a}");
}

#[tokio::test]
async fn bash_counts_what_each_stream_dropped_and_what_the_environment_dropped() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    let big = "x".repeat(bash::HEAD_BYTES + bash::TAIL_BYTES);
    setup.started_with(vec![
        out(&big),
        errs("oops\n"),
        out(&big),
        ExecEvent::Exit(ExecExit {
            code: 0,
            timed_out: false,
            dropped_bytes: 4096,
        }),
    ]);
    let ran = turn
        .run(&setup.tool(BashTool), json!({"command": "yes"}))
        .await;
    let ran = ok(&ran);
    assert!(ran.contains("bytes of output left out"), "{ran}");
    assert!(
        ran.contains("[The environment dropped 4096 more bytes of output"),
        "{ran}"
    );
    // The small stderr write is in the middle: dropped, which its section says.
    assert!(
        ran.contains("stderr:\n[... 5 bytes of output left out ...]"),
        "{ran}"
    );
}

#[tokio::test]
async fn bash_fails_when_the_stream_ends_without_an_exit() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    setup.started_with(vec![out("partial\n")]);
    let ran = turn
        .run(&setup.tool(BashTool), json!({"command": "x"}))
        .await;
    let message = err(&ran);
    assert!(message.contains("without an exit status"), "{message}");
}

#[tokio::test]
async fn cancelling_a_bash_call_drops_the_exec_stream() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    let dropped = setup.fake.hang_exec();
    let tool = setup.tool(BashTool);
    // `/stop` drops the dispatcher's stream, which drops the call's future.
    let outcome = tokio::time::timeout(
        Duration::from_millis(200),
        turn.run(&tool, json!({"command": "sleep 1000"})),
    )
    .await;
    assert!(outcome.is_err(), "the call should still be running");
    assert_eq!(setup.fake.execs().len(), 1);
    assert!(
        dropped.load(Ordering::SeqCst),
        "the exec stream was not dropped"
    );
}

#[tokio::test]
async fn aborting_a_running_bash_task_drops_the_exec_stream() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    let dropped = setup.fake.hang_exec();
    let tool = setup.tool(BashTool);
    let task = tokio::spawn(async move { turn.run(&tool, json!({"command": "sleep 1000"})).await });
    while setup.fake.execs().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(!dropped.load(Ordering::SeqCst));
    task.abort();
    let _ = task.await;
    assert!(dropped.load(Ordering::SeqCst));
}

// -- read --

#[tokio::test]
async fn read_numbers_lines_and_pages_with_offset_and_limit() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let content: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    setup.fake.put_file(&id, "src/a.txt", content.as_bytes());
    let tool = setup.tool(ReadTool);

    let all = turn.run(&tool, json!({"path": "src/a.txt"})).await;
    let all = ok(&all);
    assert!(all.starts_with("     1\tline 1\n     2\tline 2\n"), "{all}");
    assert!(all.ends_with("    10\tline 10\n"), "{all}");

    let page = turn
        .run(&tool, json!({"path": "src/a.txt", "offset": 3, "limit": 2}))
        .await;
    assert_eq!(
        ok(&page),
        "     3\tline 3\n     4\tline 4\n\n[Showed lines 3-4. Continue with offset 5.]"
    );

    let past = turn
        .run(&tool, json!({"path": "src/a.txt", "offset": 11}))
        .await;
    assert!(err(&past).contains("past the end"), "{}", text(&past));
    assert!(err(&past).contains("10 lines"));

    let bad = turn
        .run(&tool, json!({"path": "src/a.txt", "offset": 0}))
        .await;
    assert!(err(&bad).contains("count from 1"));
}

#[tokio::test]
async fn read_limits_lines_by_default() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let content: String = (1..=files::READ_DEFAULT_LINES + 5)
        .map(|n| format!("{n}\n"))
        .collect();
    setup.fake.put_file(&id, "long", content.as_bytes());
    let read = turn
        .run(&setup.tool(ReadTool), json!({"path": "long"}))
        .await;
    let read = ok(&read);
    assert!(read.contains(&format!(
        "[Showed lines 1-{}. Continue with offset {}.]",
        files::READ_DEFAULT_LINES,
        files::READ_DEFAULT_LINES + 1
    )));
}

#[tokio::test]
async fn read_caps_bytes_and_says_how_big_the_file_is() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let line = "0123456789".repeat(10) + "\n";
    let content = line.repeat(10_000);
    assert!(content.len() as u64 > files::READ_MAX_BYTES);
    setup.fake.put_file(&id, "big.log", content.as_bytes());
    let read = turn
        .run(
            &setup.tool(ReadTool),
            json!({"path": "big.log", "limit": 100000}),
        )
        .await;
    let read = ok(&read).to_owned();
    assert_eq!(setup.fake.reads(), [files::READ_MAX_BYTES]);
    assert!(
        read.contains("Truncated: the file is 986.3 KiB"),
        "{}",
        &read[read.len() - 300..]
    );
    assert!(read.contains("first 256 KiB"));
    assert!(read.len() < files::READ_MAX_BYTES as usize + 100_000);

    // An offset beyond what was read says so.
    let read = turn
        .run(
            &setup.tool(ReadTool),
            json!({"path": "big.log", "offset": 9000}),
        )
        .await;
    let message = err(&read);
    assert!(
        message.contains("past the part of big.log that could be read"),
        "{message}"
    );
}

#[tokio::test]
async fn read_refuses_binary_files_and_handles_empty_and_missing_ones() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let tool = setup.tool(ReadTool);
    setup.fake.put_file(&id, "bin", b"\x7fELF\0\0\0\x01");
    let binary = turn.run(&tool, json!({"path": "bin"})).await;
    let message = err(&binary);
    assert!(
        message.contains("looks like a binary file (8 bytes)"),
        "{message}"
    );
    assert!(message.contains("bash"), "{message}");

    setup.fake.put_file(&id, "latin", b"caf\xe9\n");
    let latin = turn.run(&tool, json!({"path": "latin"})).await;
    assert!(err(&latin).contains("binary"));

    setup.fake.put_file(&id, "empty", b"");
    let empty = turn.run(&tool, json!({"path": "empty"})).await;
    assert_eq!(ok(&empty), "empty is empty.");

    let missing = turn.run(&tool, json!({"path": "nope"})).await;
    assert_eq!(err(&missing), "no such file: nope");
}

#[tokio::test]
async fn read_drops_a_character_the_byte_cap_cut_in_two() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    // One ASCII byte, then two-byte characters: the cap falls after the first byte of one.
    let mut content = b"a".to_vec();
    content.extend("é".repeat(files::READ_MAX_BYTES as usize).into_bytes());
    setup.fake.put_file(&id, "utf8.txt", &content);
    let read = turn
        .run(&setup.tool(ReadTool), json!({"path": "utf8.txt"}))
        .await;
    let read = ok(&read);
    assert!(read.contains("Truncated"), "{}", &read[read.len() - 200..]);
    assert!(!read.contains('\u{fffd}'));
}

// -- write and edit --

#[tokio::test]
async fn write_replaces_the_whole_file() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let tool = setup.tool(WriteTool);
    let written = turn
        .run(&tool, json!({"path": "src/new.txt", "content": "hello\n"}))
        .await;
    assert_eq!(ok(&written), "Wrote 6 bytes to src/new.txt.");
    assert_eq!(setup.fake.file(&id, "src/new.txt").unwrap(), b"hello\n");
    ok(&turn
        .run(&tool, json!({"path": "src/new.txt", "content": "x"}))
        .await);
    assert_eq!(setup.fake.file(&id, "src/new.txt").unwrap(), b"x");
    let empty = turn.run(&tool, json!({"path": "", "content": "x"})).await;
    assert!(err(&empty).contains("path is empty"));
}

#[tokio::test]
async fn edit_replaces_a_unique_match() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    setup
        .fake
        .put_file(&id, "a.rs", b"fn main() {\n    old();\n}\n");
    let edited = turn
        .run(
            &setup.tool(EditTool),
            json!({"path": "a.rs", "old_string": "old()", "new_string": "new()"}),
        )
        .await;
    assert_eq!(ok(&edited), "Replaced 1 occurrence in a.rs.");
    assert_eq!(
        setup.fake.file(&id, "a.rs").unwrap(),
        b"fn main() {\n    new();\n}\n"
    );
}

#[tokio::test]
async fn edit_says_how_many_matches_there_were() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    setup.fake.put_file(&id, "a.txt", b"x = 1\nx = 1\ny = 2\n");
    let tool = setup.tool(EditTool);

    let none = turn
        .run(
            &tool,
            json!({"path": "a.txt", "old_string": "z = 3", "new_string": "z = 4"}),
        )
        .await;
    assert!(err(&none).contains("0 matches"), "{}", text(&none));

    let many = turn
        .run(
            &tool,
            json!({"path": "a.txt", "old_string": "x = 1", "new_string": "x = 5"}),
        )
        .await;
    let message = err(&many);
    assert!(message.contains("matches 2 places"), "{message}");
    assert!(message.contains("replace_all"), "{message}");
    // Nothing was written.
    assert_eq!(
        setup.fake.file(&id, "a.txt").unwrap(),
        b"x = 1\nx = 1\ny = 2\n"
    );

    let all = turn
        .run(
            &tool,
            json!({"path": "a.txt", "old_string": "x = 1", "new_string": "x = 5", "replace_all": true}),
        )
        .await;
    assert_eq!(ok(&all), "Replaced 2 occurrences in a.txt.");
    assert_eq!(
        setup.fake.file(&id, "a.txt").unwrap(),
        b"x = 5\nx = 5\ny = 2\n"
    );
}

#[tokio::test]
async fn edit_refuses_what_it_cannot_change_safely() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let tool = setup.tool(EditTool);

    let same = turn
        .run(
            &tool,
            json!({"path": "a", "old_string": "x", "new_string": "x"}),
        )
        .await;
    assert!(err(&same).contains("are the same"));
    let empty = turn
        .run(
            &tool,
            json!({"path": "a", "old_string": "", "new_string": "x"}),
        )
        .await;
    assert!(err(&empty).contains("old_string is empty"));

    let missing = turn
        .run(
            &tool,
            json!({"path": "nope", "old_string": "x", "new_string": "y"}),
        )
        .await;
    assert_eq!(err(&missing), "no such file: nope");

    setup.fake.put_file(&id, "bin", b"\0\x01x");
    let binary = turn
        .run(
            &tool,
            json!({"path": "bin", "old_string": "x", "new_string": "y"}),
        )
        .await;
    assert!(err(&binary).contains("not a text file"));

    // A file past the edit limit would be cut short by writing back what was read.
    let big = vec![b'a'; files::EDIT_MAX_BYTES as usize + 1];
    setup.fake.put_file(&id, "big", &big);
    let too_big = turn
        .run(
            &tool,
            json!({"path": "big", "old_string": "a", "new_string": "b"}),
        )
        .await;
    assert!(
        err(&too_big).contains("too large for edit"),
        "{}",
        text(&too_big)
    );
    assert_eq!(setup.fake.file(&id, "big").unwrap().len(), big.len());
}

// -- find and grep --

#[tokio::test]
async fn find_lists_paths_and_says_when_it_cut_them() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let tool = setup.tool(FindTool);
    let none = turn.run(&tool, json!({"pattern": "*.rs"})).await;
    assert_eq!(ok(&none), "No files matched.");

    setup.fake.put_file(&id, "src/a.rs", b"");
    setup.fake.put_file(&id, "src/b.rs", b"");
    let found = turn
        .run(&tool, json!({"pattern": "*.rs", "path": "src"}))
        .await;
    assert_eq!(ok(&found), "src/a.rs\nsrc/b.rs\n");

    for n in 0..search::RESULT_LIMIT + 50 {
        setup.fake.put_file(&id, &format!("many/f{n:04}.txt"), b"");
    }
    let found = turn.run(&tool, json!({"pattern": "many"})).await;
    let found = ok(&found);
    assert_eq!(
        found
            .lines()
            .filter(|line| line.starts_with("many/"))
            .count(),
        200
    );
    assert!(found.contains("Showing the first 200 matches"), "{found}");
}

#[tokio::test]
async fn grep_shows_matching_lines_and_cuts_long_ones() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    let tool = setup.tool(GrepTool);
    let none = turn.run(&tool, json!({"pattern": "needle"})).await;
    assert_eq!(ok(&none), "No lines matched.");

    let long = format!("needle {}", "z".repeat(1000));
    let content = format!("hay\nneedle here\n{long}\n");
    setup.fake.put_file(&id, "a.txt", content.as_bytes());
    let found = turn
        .run(
            &tool,
            json!({"pattern": "needle", "case_insensitive": true}),
        )
        .await;
    let found = ok(&found);
    let mut lines = found.lines();
    assert_eq!(lines.next(), Some("a.txt:2:needle here"));
    let cut = lines.next().unwrap();
    assert!(cut.starts_with("a.txt:3:needle zzz"), "{cut}");
    assert!(cut.ends_with("[line cut at 500 characters]"), "{cut}");
    assert!(cut.len() < 600);

    let many = "needle\n".repeat(search::RESULT_LIMIT as usize + 1);
    setup.fake.put_file(&id, "b.txt", many.as_bytes());
    let found = turn
        .run(&tool, json!({"pattern": "needle", "glob": "*.txt"}))
        .await;
    assert!(ok(&found).contains("Showing the first 200 matching lines"));
}

#[tokio::test]
async fn tools_reject_input_they_do_not_know() {
    let setup = Setup::new().await;
    let (turn, _) = setup.with_workspace().await;
    let output = turn
        .run(&setup.tool(ReadTool), json!({"path": "a", "colour": "red"}))
        .await;
    assert!(err(&output).starts_with("invalid input"));
    let output = turn.run(&setup.tool(BashTool), json!({})).await;
    assert!(err(&output).starts_with("invalid input"));
    let output = turn
        .run(&setup.tool(EnvironmentStatusTool), json!({"x": 1}))
        .await;
    assert!(err(&output).starts_with("invalid input"));
}

#[tokio::test]
async fn parallel_reads_share_the_turns_lease() {
    let setup = Setup::new().await;
    let (turn, id) = setup.with_workspace().await;
    setup.fake.put_file(&id, "a", b"1\n");
    setup.fake.put_file(&id, "b", b"2\n");
    let read = setup.tool(ReadTool);
    let find = setup.tool(FindTool);
    let status = setup.tool(EnvironmentStatusTool);
    let (a, b, c, d) = tokio::join!(
        turn.run(&read, json!({"path": "a"})),
        turn.run(&read, json!({"path": "b"})),
        turn.run(&find, json!({"pattern": "*"})),
        turn.run(&status, json!({})),
    );
    ok(&a);
    ok(&b);
    ok(&c);
    ok(&d);
    assert_eq!(turn.0.lease.held(), Some(id));
}
