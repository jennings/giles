//! Building registries from configurations in temporary data roots, with fake components.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::channel::mpsc;
use futures::stream::{BoxStream, StreamExt};
use giles_types::{
    CallContext, ChannelEvent, ChannelInfo, CompletionEvent, CompletionRequest, Error, EventId,
    MediaRef, MessageId, ModelInfo, OutboundMessage, OutputMode, Result, Skill, SkillSummary,
    ToolCall, ToolCallHandle, ToolSpec,
};
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::config::LoadOptions;
use crate::skills::SkillSource;

const MINIMAL: &str = "[owner]\nidentities = [\"web:owner\"]\n";

struct Home {
    dir: TempDir,
    root: DataRoot,
}

impl Home {
    fn new(giles_toml: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        let home = Self { dir, root };
        home.write("giles.toml", &format!("{MINIMAL}{giles_toml}"));
        home
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn write(&self, relative: &str, contents: &str) -> &Self {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
        self
    }

    fn agent(&self, name: &str, agent_toml: &str) -> &Self {
        self.write(&format!("agents/{name}/agent.toml"), agent_toml)
    }

    fn config(&self) -> Arc<Config> {
        let config = Config::load_with(
            &self.root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        Arc::new(config)
    }

    /// [`Registry::from_config`] with a media source that has no blobs.
    fn build(&self) -> Result<Registry, RegistryError> {
        Registry::from_config(self.root.clone(), self.config(), Arc::new(NoMedia))
    }

    /// An empty registry for this config.
    fn empty(&self) -> Registry {
        Registry::new(self.root.clone(), self.config())
    }
}

struct NoMedia;

#[async_trait]
impl MediaSource for NoMedia {
    async fn read(&self, media: &MediaRef) -> Result<Bytes> {
        Err(Error::NotFound(media.id.to_string()))
    }
}

struct FakeProvider;

#[async_trait]
impl Provider for FakeProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    async fn complete(
        &self,
        _req: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<CompletionEvent>>> {
        Err(Error::unavailable("fake"))
    }
}

struct FakeChannel(ChannelInfo);

impl FakeChannel {
    fn new() -> Self {
        Self(ChannelInfo {
            identity_namespace: "irc:example".into(),
            output: OutputMode::Blocks,
            can_initiate: false,
        })
    }
}

#[async_trait]
impl Channel for FakeChannel {
    fn info(&self) -> &ChannelInfo {
        &self.0
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        Ok(futures::stream::empty().boxed())
    }

    async fn ack(&self, _event: &EventId) -> Result<()> {
        Ok(())
    }

    async fn send(&self, _msg: OutboundMessage) -> Result<MessageId> {
        Err(Error::unavailable("fake"))
    }
}

/// A toolset whose `list_tools` answer is fixed and whose changes come from a channel. Records
/// which of `tool_changes` and `list_tools` were called, in order.
struct FakeToolset {
    tools: Result<Vec<ToolSpec>, String>,
    changes: Mutex<Option<mpsc::UnboundedReceiver<Vec<ToolSpec>>>>,
    calls: Mutex<Vec<&'static str>>,
}

impl FakeToolset {
    fn new(tools: &[&str]) -> Self {
        Self {
            tools: Ok(tools.iter().map(|n| spec(n)).collect()),
            changes: Mutex::new(None),
            calls: Mutex::default(),
        }
    }

    fn failing() -> Self {
        Self {
            tools: Err("server unreachable".into()),
            changes: Mutex::new(None),
            calls: Mutex::default(),
        }
    }

    fn with_changes(mut self) -> (Self, mpsc::UnboundedSender<Vec<ToolSpec>>) {
        let (tx, rx) = mpsc::unbounded();
        self.changes = Mutex::new(Some(rx));
        (self, tx)
    }
}

#[async_trait]
impl Toolset for FakeToolset {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        self.calls.lock().unwrap().push("list_tools");
        self.tools.clone().map_err(Error::unavailable)
    }

    fn tool_changes(&self) -> Option<BoxStream<'static, Vec<ToolSpec>>> {
        self.calls.lock().unwrap().push("tool_changes");
        self.changes.lock().unwrap().take().map(StreamExt::boxed)
    }

    async fn list_skills(&self) -> Result<Vec<SkillSummary>> {
        Ok(Vec::new())
    }

    async fn get_skill(&self, name: &str) -> Result<Skill> {
        Err(Error::NotFound(name.to_owned()))
    }

    async fn call(&self, _call: ToolCall, _ctx: CallContext) -> Result<ToolCallHandle> {
        Err(Error::InvalidRequest("fake".into()))
    }
}

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: String::new(),
        input_schema: json!({"type": "object"}),
    }
}

fn name(name: &str) -> InstanceName {
    InstanceName::new(name).unwrap()
}

fn tool_names(registry: &Registry) -> Vec<String> {
    registry
        .tools()
        .snapshot()
        .specs()
        .map(|s| s.name.clone())
        .collect()
}

/// The error of `result`, whose `Ok` type needn't be `Debug`.
#[track_caller]
fn expect_err<T>(result: Result<T, RegistryError>) -> RegistryError {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(e) => e,
    }
}

/// Asserts `err` is a config error in `file` at `key` (found at `line`:`column`) whose reason
/// contains `reason`.
#[track_caller]
fn assert_config_error(
    err: &RegistryError,
    file: &Path,
    (line, column): (usize, usize),
    key: &str,
    reason: &str,
) {
    let RegistryError::Config(config) = err else {
        panic!("expected a config error, got {err}");
    };
    assert_eq!(config.file, file, "{err}");
    let position = config.position.as_ref().expect("the error has a position");
    assert_eq!((position.line, position.column), (line, column), "{err}");
    assert!(
        err.to_string()
            .starts_with(&format!("{}:{line}:{column}: {key}: ", file.display())),
        "{err}"
    );
    assert_eq!(config.key.to_string(), key, "{err}");
    assert!(config.reason.contains(reason), "{err}");
}

#[cfg(feature = "provider-anthropic")]
#[test]
fn builds_the_anthropic_provider_and_resolves_agent_models() {
    let home = Home::new(
        "[provider.anthropic]\nconfig = { api_key = \"sk-test\" }\n\
         [provider.cheap]\ncomponent = \"anthropic\"\nconfig = { api_key = \"sk-test\" }\n",
    );
    home.agent(
        "jeeves",
        "models = [\"anthropic/claude-sonnet-4-5\", \"cheap/claude-haiku-4-5\"]\n",
    )
    .agent(
        "bunter",
        "models = [\"cheap/claude-haiku-4-5\"]\ncompaction_model = \"anthropic/claude-x\"\n",
    );
    let registry = home.build().unwrap();
    let providers: Vec<_> = registry.providers().into_iter().map(|(n, _)| n).collect();
    assert_eq!(providers, [name("anthropic"), name("cheap")]);

    let config = registry.config().clone();
    let jeeves = &config.agents[&AgentId::new("jeeves").unwrap()];
    let model = registry.agent_model(jeeves).unwrap();
    assert_eq!(model.instance, name("anthropic"));
    assert_eq!(model.model, "claude-sonnet-4-5");
    assert!(Arc::ptr_eq(
        &model.provider,
        &registry.provider("anthropic").unwrap()
    ));
    // No compaction_model: the first model.
    assert_eq!(
        registry.compaction_model(jeeves).unwrap().model,
        "claude-sonnet-4-5"
    );

    let bunter = &config.agents[&AgentId::new("bunter").unwrap()];
    let compaction = registry.compaction_model(bunter).unwrap();
    assert_eq!(compaction.instance, name("anthropic"));
    assert_eq!(compaction.model, "claude-x");

    let err = expect_err(registry.resolve_model(&"other/m".parse().unwrap()));
    assert!(
        matches!(&err, RegistryError::UnknownProvider(n) if n.as_str() == "other"),
        "{err}"
    );
}

#[cfg(feature = "provider-anthropic")]
#[test]
fn a_rejected_provider_config_names_the_key_without_the_secret() {
    let home = Home::new(
        "[provider.anthropic]\nconfig = { api_key = \"sk-secret-value\", max_token = 5 }\n",
    );
    let err = home.build().unwrap_err();
    assert_config_error(
        &err,
        &home.path("giles.toml"),
        (4, 1),
        "provider.anthropic.config",
        "max_token",
    );
    assert!(!err.to_string().contains("sk-secret-value"), "{err}");
}

#[cfg(feature = "provider-openai")]
#[test]
fn builds_the_openai_provider() {
    let home = Home::new(
        "[provider.ollama]\ncomponent = \"openai\"\n\
         config = { base_url = \"http://127.0.0.1:11434/v1\" }\n\
         [provider.openai]\nconfig = { api_key = \"sk-test\" }\n",
    );
    home.agent("jeeves", "models = [\"ollama/qwen3:8b\"]\n");
    let registry = home.build().unwrap();
    let providers: Vec<_> = registry.providers().into_iter().map(|(n, _)| n).collect();
    assert_eq!(providers, [name("ollama"), name("openai")]);
    let config = registry.config().clone();
    let model = registry
        .agent_model(&config.agents[&AgentId::new("jeeves").unwrap()])
        .unwrap();
    assert_eq!(
        (model.instance.as_str(), model.model.as_str()),
        ("ollama", "qwen3:8b")
    );

    let home = Home::new(
        "[provider.openai]\nconfig = { api_key = \"sk-secret-value\", dialect = \"bogus\" }\n",
    );
    let err = home.build().unwrap_err();
    assert_config_error(
        &err,
        &home.path("giles.toml"),
        (4, 1),
        "provider.openai.config",
        "dialect",
    );
    assert!(!err.to_string().contains("sk-secret-value"), "{err}");
}

#[test]
fn unimplemented_built_ins_are_config_errors() {
    let home = Home::new("[channel.web]\n");
    let err = home.build().unwrap_err();
    assert_config_error(
        &err,
        &home.path("giles.toml"),
        // No `component` key: the position of the instance's table name.
        (3, 10),
        "channel.web.component",
        "`web` channel is not implemented yet (TASKS.md 2.22)",
    );
}

#[test]
fn plugin_instances_stay_pending_until_registered() {
    let home = Home::new(
        "[channel.libera]\ncomponent = \"irc\"\n\
         [channel.mail]\ncomponent = \"email\"\naddress = \"https://mail.example.com\"\n\
         [toolset.github]\nurl = \"https://example.com/mcp\"\n",
    );
    let registry = home.build().unwrap();
    let pending: Vec<_> = registry
        .pending()
        .into_iter()
        .map(|p| (p.kind, p.name.to_string(), p.reason))
        .collect();
    assert_eq!(pending.len(), 3, "{pending:?}");
    assert_eq!(pending[0].0, Kind::Channel);
    assert_eq!(pending[0].1, "libera");
    assert!(pending[0].2.contains("2.16"), "{pending:?}");
    assert_eq!(pending[1].1, "mail");
    assert!(pending[1].2.contains("2.18"), "{pending:?}");
    assert_eq!(pending[2].0, Kind::Toolset);
    assert!(pending[2].2.contains("2.20"), "{pending:?}");

    let old = registry
        .register_channel(&name("libera"), Arc::new(FakeChannel::new()))
        .unwrap();
    assert!(old.is_none());
    assert_eq!(
        registry
            .channel("libera")
            .unwrap()
            .info()
            .identity_namespace,
        "irc:example"
    );
    let still: Vec<_> = registry.pending().into_iter().map(|p| p.name).collect();
    assert_eq!(still, [name("mail"), name("github")]);
    // Registering again replaces and returns the earlier object.
    let old = registry
        .register_channel(&name("libera"), Arc::new(FakeChannel::new()))
        .unwrap();
    assert!(old.is_some());
}

#[tokio::test]
async fn registering_needs_a_configured_instance_of_that_kind() {
    let home = Home::new("[channel.libera]\ncomponent = \"irc\"\n");
    let registry = home.build().unwrap();
    let err = expect_err(registry.register_channel(&name("oftc"), Arc::new(FakeChannel::new())));
    assert!(
        matches!(&err, RegistryError::NotConfigured { kind: Kind::Channel, name: n } if n.as_str() == "oftc"),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "there is no configured channel instance `oftc`"
    );
    let err = expect_err(
        registry
            .register_toolset(&name("libera"), Arc::new(FakeToolset::new(&[])))
            .await,
    );
    assert!(
        matches!(
            err,
            RegistryError::NotConfigured {
                kind: Kind::Toolset,
                ..
            }
        ),
        "{err}"
    );
    let err = expect_err(registry.register_provider(&name("libera"), Arc::new(FakeProvider)));
    assert!(
        matches!(
            err,
            RegistryError::NotConfigured {
                kind: Kind::Provider,
                ..
            }
        ),
        "{err}"
    );
    assert!(registry.toolsets().is_empty());
}

#[tokio::test]
async fn registered_toolsets_feed_the_tool_index() {
    let home = Home::new(
        "[toolset.github]\nurl = \"https://example.com/mcp\"\n\
         [toolset.fs]\nurl = \"https://example.com/fs\"\n\
         [toolset.down]\nurl = \"https://example.com/down\"\n",
    );
    let registry = home.build().unwrap();
    registry.tools().set_core_tool(spec("memory")).unwrap();
    let long = "x".repeat(60);
    registry
        .register_toolset(
            &name("github"),
            Arc::new(FakeToolset::new(&[
                "search",
                "create_issue",
                &long,
                "bad name",
            ])),
        )
        .await
        .unwrap();
    let (fs, changes) = FakeToolset::new(&["read"]).with_changes();
    let fs = Arc::new(fs);
    registry
        .register_toolset(&name("fs"), fs.clone())
        .await
        .unwrap();
    // Subscribed before listing, so no change between the two can be missed.
    assert_eq!(*fs.calls.lock().unwrap(), ["tool_changes", "list_tools"]);
    registry
        .register_toolset(&name("down"), Arc::new(FakeToolset::failing()))
        .await
        .unwrap();
    assert!(registry.toolset("down").is_some());
    assert!(registry.pending().is_empty());

    let session = registry.tools().snapshot();
    assert_eq!(
        tool_names(&registry),
        [
            "fs__read",
            "github__create_issue",
            "github__search",
            "memory"
        ]
    );
    assert_eq!(
        session.get("github__search").unwrap().target,
        ToolTarget::Toolset {
            instance: name("github"),
            tool: "search".into()
        }
    );

    changes
        .unbounded_send(vec![spec("read"), spec("write")])
        .unwrap();
    for _ in 0..500 {
        if tool_names(&registry).contains(&"fs__write".to_owned()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        tool_names(&registry),
        [
            "fs__read",
            "fs__write",
            "github__create_issue",
            "github__search",
            "memory"
        ]
    );
    // The session's snapshot is unchanged.
    assert_eq!(session.len(), 4);
    assert!(session.get("fs__write").is_none());

    // A replacement object's list replaces the old one; the old change stream no longer counts.
    registry
        .register_toolset(&name("fs"), Arc::new(FakeToolset::new(&["stat"])))
        .await
        .unwrap();
    // Whether or not the aborted watcher has dropped its receiver yet, this must not apply.
    let _ = changes.unbounded_send(vec![spec("stale")]);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        tool_names(&registry),
        [
            "fs__stat",
            "github__create_issue",
            "github__search",
            "memory"
        ]
    );
}

#[tokio::test]
async fn agent_skills_combine_toolset_global_and_agent_sources() {
    let tools_dir = tempfile::tempdir().unwrap();
    let skill = |dir: &Path, name: &str, description: &str| {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nBody of {name}.\n"),
        )
        .unwrap();
    };
    skill(tools_dir.path(), "review", "Review a pull request.");
    skill(tools_dir.path(), "pdf", "The toolset's PDF skill.");
    let home = Home::new(&format!(
        "[provider.anthropic]\nconfig = {{ api_key = \"k\" }}\n\
         [toolset.github]\nurl = \"https://example.com/mcp\"\nskills = [{:?}]\n",
        tools_dir.path().display().to_string()
    ));
    home.agent("jeeves", "models = [\"anthropic/a\"]\n");
    skill(&home.path("skills"), "pdf", "Global PDF skill.");
    skill(&home.path("skills"), "notes", "Global notes skill.");
    skill(
        &home.path("agents/jeeves/skills"),
        "pdf",
        "Jeeves' PDF skill.",
    );
    let registry = home.empty();
    let jeeves = AgentId::new("jeeves").unwrap();

    let sources = registry.skill_sources(&jeeves).unwrap();
    assert_eq!(
        sources,
        [
            SkillSource {
                dir: tools_dir.path().to_owned(),
                prefix: Some("github".into())
            },
            SkillSource::local(home.root.skills_dir()),
            SkillSource::local(home.root.agent_dir("jeeves").unwrap().join("skills")),
        ]
    );

    let skills = registry.agent_skills(&jeeves).await.unwrap();
    let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["github__pdf", "github__review", "notes", "pdf"]);
    assert_eq!(skills.get("pdf").unwrap().description, "Jeeves' PDF skill.");
    assert_eq!(
        skills.get("github__pdf").unwrap().description,
        "The toolset's PDF skill."
    );
}
