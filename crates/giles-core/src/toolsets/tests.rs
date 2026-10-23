//! The toolset lifecycle with fake servers and paused time: boot, the tool cache, idle stop and
//! relaunch, crash restarts with backoff, a degraded remote server, `list_changed`, and shutdown.

use std::collections::{BTreeMap, VecDeque};
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use futures::{FutureExt as _, StreamExt as _};
use giles_types::{
    AgentId, Block, CallContext, Error, Result, Skill, SkillSummary, ToolCall, ToolCallEvent,
    ToolCallHandle, ToolSpec, Toolset,
};
use serde_json::json;
use tokio::sync::{Semaphore, oneshot};

use super::cache::{CacheKey, ToolCache};
use super::host::ToolsetHost;
use super::lifecycle::{Connection, Connector};
use crate::config::{Config, InstanceName, LoadOptions, ToolsetInstance};
use crate::paths::DataRoot;
use crate::registry::Registry;

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: String::new(),
        input_schema: json!({"type": "object"}),
    }
}

fn name(instance: &str) -> InstanceName {
    InstanceName::new(instance).unwrap()
}

/// One connection to a fake server.
struct FakeServer {
    id: usize,
    tools: Vec<ToolSpec>,
    changes: Mutex<Option<futures::channel::mpsc::UnboundedReceiver<Vec<ToolSpec>>>>,
    live: Arc<AtomicUsize>,
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl Toolset for FakeServer {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(self.tools.clone())
    }

    fn tool_changes(&self) -> Option<futures::stream::BoxStream<'static, Vec<ToolSpec>>> {
        let changes = self.changes.lock().unwrap().take()?;
        Some(changes.boxed())
    }

    async fn list_skills(&self) -> Result<Vec<SkillSummary>> {
        Ok(Vec::new())
    }

    async fn get_skill(&self, name: &str) -> Result<Skill> {
        Err(Error::NotFound(name.to_owned()))
    }

    /// `slow` never answers; anything else answers `<tool> by connection <id>` at once.
    async fn call(&self, call: ToolCall, _ctx: CallContext) -> Result<ToolCallHandle> {
        let (answers, _) = tokio::sync::mpsc::channel(1);
        let events = if call.name == "slow" {
            futures::stream::pending().boxed()
        } else {
            let text = format!("{} by connection {}", call.name, self.id);
            futures::stream::iter([Ok(ToolCallEvent::Result {
                content: vec![Block::Text { text }],
                is_error: false,
            })])
            .boxed()
        };
        Ok(ToolCallHandle { events, answers })
    }
}

/// What the fake connector does for one instance.
struct Script {
    source: String,
    version: String,
    tools: Vec<ToolSpec>,
    /// Errors for the next connects, in order.
    failures: VecDeque<Error>,
    /// When set, each connect waits for a permit.
    gate: Option<Arc<Semaphore>>,
    connects: usize,
    /// Per connection: closing it (a crash, with this status) and its `list_changed` sender.
    crash: Vec<Option<oneshot::Sender<Option<ExitStatus>>>>,
    changes: Vec<UnboundedSender<Vec<ToolSpec>>>,
    live: Arc<AtomicUsize>,
}

impl Script {
    fn new(tools: &[&str]) -> Self {
        Self {
            source: "sha256:v1".to_owned(),
            version: "1.0".to_owned(),
            tools: tools.iter().map(|t| tool(t)).collect(),
            failures: VecDeque::new(),
            gate: None,
            connects: 0,
            crash: Vec::new(),
            changes: Vec::new(),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[derive(Default)]
struct FakeConnector {
    scripts: Mutex<BTreeMap<String, Script>>,
}

impl FakeConnector {
    fn with(scripts: impl IntoIterator<Item = (&'static str, Script)>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(
                scripts
                    .into_iter()
                    .map(|(name, script)| (name.to_owned(), script))
                    .collect(),
            ),
        })
    }

    fn script<T>(&self, instance: &str, f: impl FnOnce(&mut Script) -> T) -> T {
        f(self.scripts.lock().unwrap().get_mut(instance).unwrap())
    }

    fn connects(&self, instance: &str) -> usize {
        self.script(instance, |s| s.connects)
    }

    fn live(&self, instance: &str) -> usize {
        self.script(instance, |s| s.live.load(Ordering::SeqCst))
    }

    /// Ends the latest connection, as a crash with exit status `code`.
    fn crash(&self, instance: &str, code: i32) {
        self.script(instance, |s| {
            let crash = s.crash.last_mut().unwrap().take().unwrap();
            crash.send(Some(ExitStatus::from_raw(code << 8))).unwrap();
        });
    }

    /// Sends `notifications/tools/list_changed` with `tools` on the latest connection.
    fn change_tools(&self, instance: &str, tools: &[&str]) {
        self.script(instance, |s| {
            let list = tools.iter().map(|t| tool(t)).collect();
            s.changes.last().unwrap().unbounded_send(list).unwrap();
        });
    }
}

#[async_trait]
impl Connector for FakeConnector {
    async fn source(&self, instance: &ToolsetInstance) -> Result<String> {
        Ok(self.script(instance.name.as_str(), |s| s.source.clone()))
    }

    async fn connect(&self, instance: &ToolsetInstance) -> Result<Connection> {
        let gate = self.script(instance.name.as_str(), |s| s.gate.clone());
        if let Some(gate) = gate {
            gate.acquire().await.unwrap().forget();
        }
        self.script(instance.name.as_str(), |s| {
            s.connects += 1;
            if let Some(error) = s.failures.pop_front() {
                return Err(error);
            }
            let (crash, crashed) = oneshot::channel();
            let (changes, changed) = unbounded();
            s.crash.push(Some(crash));
            s.changes.push(changes);
            s.live.fetch_add(1, Ordering::SeqCst);
            let server = FakeServer {
                id: s.connects,
                tools: s.tools.clone(),
                changes: Mutex::new(Some(changed)),
                live: s.live.clone(),
            };
            Ok(Connection {
                toolset: Arc::new(server),
                source: s.source.clone(),
                server_version: Some(s.version.clone()),
                // A dropped sender is a connection that never ends by itself.
                closed: async move { crashed.await.unwrap_or(None) }.boxed(),
            })
        })
    }
}

/// A data root whose `giles.toml` declares `toolsets`, with a registry and a cache.
struct Setup {
    _dir: tempfile::TempDir,
    root: DataRoot,
    registry: Arc<Registry>,
}

impl Setup {
    fn new(toolsets: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        std::fs::write(
            root.config_file(),
            format!("[owner]\nidentities = [\"web:owner\"]\n{toolsets}"),
        )
        .unwrap();
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
        Self {
            _dir: dir,
            root,
            registry,
        }
    }

    fn cache(&self) -> ToolCache {
        ToolCache::new(self.root.cache_dir().join("toolsets"))
    }

    fn instance(&self, instance: &str) -> ToolsetInstance {
        self.registry.config().toolsets[instance].clone()
    }

    async fn host(&self, connector: &Arc<FakeConnector>) -> ToolsetHost {
        ToolsetHost::start_with(connector.clone(), &self.registry, self.cache()).await
    }

    /// The tools new sessions get, once background updates have settled.
    async fn offered(&self) -> Vec<String> {
        settle().await;
        self.registry
            .tools()
            .snapshot()
            .tools()
            .iter()
            .map(|t| t.spec.name.clone())
            .collect()
    }

    /// Calls `tool` on `instance` the way the dispatcher does.
    async fn call(&self, instance: &str, tool: &str) -> Result<ToolCallHandle> {
        let call = ToolCall {
            id: format!("toolu_{tool}"),
            name: tool.to_owned(),
            input: json!({}),
            media: Vec::new(),
        };
        let ctx = CallContext {
            agent: AgentId::new("agent").unwrap(),
            requester: None,
            conversation: None,
        };
        self.registry
            .toolset(instance)
            .unwrap()
            .call(call, ctx)
            .await
    }

    async fn call_text(&self, instance: &str, tool: &str) -> String {
        let mut handle = self.call(instance, tool).await.unwrap();
        match handle.events.next().await {
            Some(Ok(ToolCallEvent::Result { content, .. })) => match &content[..] {
                [Block::Text { text }] => text.clone(),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }
}

/// Lets spawned tasks run without moving the paused clock.
async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// Moves the paused clock forward by `by`, letting timers fire.
async fn advance(by: Duration) {
    settle().await;
    tokio::time::advance(by).await;
    settle().await;
}

const FILES: &str = "\n[toolset.files]\ncommand = \"/nonexistent/files\"\nidle_stop = \"1m\"\n";
const REMOTE: &str = "\n[toolset.remote]\nurl = \"https://mcp.example.com/mcp\"\n";

#[tokio::test(start_paused = true)]
async fn boot_starts_and_registers_every_toolset() {
    let s = Setup::new(&format!("{FILES}{REMOTE}"));
    let connector = FakeConnector::with([
        ("files", Script::new(&["read", "write"])),
        ("remote", Script::new(&["fetch"])),
    ]);
    let host = s.host(&connector).await;
    host.started().await;
    assert_eq!(connector.connects("files"), 1);
    assert_eq!(connector.connects("remote"), 1);
    assert_eq!(
        s.offered().await,
        ["files__read", "files__write", "remote__fetch"]
    );
    assert!(s.registry.pending().is_empty());
    assert_eq!(s.call_text("files", "read").await, "read by connection 1");
    assert_eq!(
        s.call_text("remote", "fetch").await,
        "fetch by connection 1"
    );
    // Each start stored its list with the full key.
    let files = s.instance("files");
    assert_eq!(
        s.cache()
            .lookup(&name("files"), "sha256:v1", &files.config_hash),
        Some(vec![tool("read"), tool("write")])
    );
    host.stop().await;
    assert_eq!(connector.live("files"), 0);
    assert_eq!(connector.live("remote"), 0);
}

#[tokio::test(start_paused = true)]
async fn cached_tools_are_offered_until_the_server_answers() {
    let s = Setup::new(FILES);
    let files = s.instance("files");
    let key = CacheKey {
        source: "sha256:v1".to_owned(),
        config_hash: files.config_hash.to_string(),
        server_version: Some("0.9".to_owned()),
    };
    s.cache().store(&name("files"), &key, &[tool("old")]);
    let gate = Arc::new(Semaphore::new(0));
    let mut script = Script::new(&["new"]);
    script.gate = Some(gate.clone());
    let connector = FakeConnector::with([("files", script)]);
    let host = s.host(&connector).await;
    // A hit: offered from registration on, while the server starts.
    assert_eq!(s.offered().await, ["files__old"]);
    gate.add_permits(1);
    host.started().await;
    // The start refreshed the list and the cache, with the version the server reported.
    assert_eq!(s.offered().await, ["files__new"]);
    let stored: serde_json::Value = serde_json::from_slice(
        &std::fs::read(s.root.cache_dir().join("toolsets/files.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(stored["key"]["server_version"], "1.0");
    assert_eq!(stored["tools"][0]["name"], "new");
    host.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_changed_binary_or_config_misses_the_cache() {
    for (source, config_hash) in [("sha256:v2", None), ("sha256:v1", Some("other"))] {
        let s = Setup::new(FILES);
        let files = s.instance("files");
        let config_hash = config_hash.map_or(files.config_hash.to_string(), str::to_owned);
        let key = CacheKey {
            source: source.to_owned(),
            config_hash,
            server_version: Some("1.0".to_owned()),
        };
        s.cache().store(&name("files"), &key, &[tool("old")]);
        let gate = Arc::new(Semaphore::new(0));
        let mut script = Script::new(&["new"]);
        script.gate = Some(gate.clone());
        let connector = FakeConnector::with([("files", script)]);
        let host = s.host(&connector).await;
        assert!(s.offered().await.is_empty(), "{source}");
        gate.add_permits(1);
        host.started().await;
        assert_eq!(s.offered().await, ["files__new"]);
        host.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn an_idle_server_stops_keeps_its_tools_and_relaunches_on_a_call() {
    let s = Setup::new(FILES);
    let connector = FakeConnector::with([("files", Script::new(&["read", "slow"]))]);
    let host = s.host(&connector).await;
    host.started().await;
    assert_eq!(connector.live("files"), 1);

    // A call in flight keeps it running past `idle_stop`.
    let slow = s.call("files", "slow").await.unwrap();
    advance(Duration::from_secs(5 * 60)).await;
    assert_eq!(connector.live("files"), 1);
    drop(slow);
    advance(Duration::from_secs(59)).await;
    assert_eq!(connector.live("files"), 1);
    advance(Duration::from_secs(2)).await;
    assert_eq!(connector.live("files"), 0);
    assert_eq!(s.offered().await, ["files__read", "files__slow"]);

    // The next call relaunches it.
    assert_eq!(s.call_text("files", "read").await, "read by connection 2");
    assert_eq!(connector.connects("files"), 2);
    assert_eq!(connector.live("files"), 1);
    // A finished call starts the idle clock again.
    advance(Duration::from_secs(61)).await;
    assert_eq!(connector.live("files"), 0);
    host.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_remote_server_never_stops_for_idleness() {
    let s = Setup::new(REMOTE);
    let connector = FakeConnector::with([("remote", Script::new(&["fetch"]))]);
    let host = s.host(&connector).await;
    host.started().await;
    advance(Duration::from_secs(24 * 3600)).await;
    assert_eq!(connector.live("remote"), 1);
    host.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_crashed_server_restarts_with_backoff() {
    // Without `idle_stop`, so the server runs through the ten healthy minutes below.
    let s = Setup::new("\n[toolset.files]\ncommand = \"/nonexistent/files\"\n");
    let connector = FakeConnector::with([("files", Script::new(&["read"]))]);
    let host = s.host(&connector).await;
    host.started().await;

    connector.crash("files", 3);
    settle().await;
    // Down for 1 s: its tools stay offered, and calls fail with when to retry.
    assert_eq!(s.offered().await, ["files__read"]);
    match s.call("files", "read").await {
        Err(Error::Unavailable {
            message,
            retry_after,
        }) => {
            assert!(message.contains("exit status: 3"), "{message}");
            assert_eq!(retry_after, Some(Duration::from_secs(1)));
        }
        other => panic!("{other:?}"),
    }
    advance(Duration::from_secs(1)).await;
    assert_eq!(connector.connects("files"), 2);
    assert_eq!(s.call_text("files", "read").await, "read by connection 2");

    // Crashing again soon doubles the delay.
    connector.crash("files", 1);
    advance(Duration::from_millis(1500)).await;
    assert_eq!(connector.connects("files"), 2);
    advance(Duration::from_millis(500)).await;
    assert_eq!(connector.connects("files"), 3);

    // Ten healthy minutes reset it.
    advance(super::HEALTHY_RESET).await;
    connector.crash("files", 1);
    advance(Duration::from_secs(1)).await;
    assert_eq!(connector.connects("files"), 4);
    host.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_unreachable_remote_server_leaves_giles_degraded_and_retrying() {
    let s = Setup::new(&format!("{FILES}{REMOTE}"));
    let mut remote = Script::new(&["fetch"]);
    remote.failures = (0..3)
        .map(|_| Error::unavailable("connection refused"))
        .collect();
    let connector = FakeConnector::with([("files", Script::new(&["read"])), ("remote", remote)]);
    let host = s.host(&connector).await;
    // Boot finishes with the remote down; everything else works.
    host.started().await;
    assert_eq!(s.offered().await, ["files__read"]);
    assert_eq!(s.call_text("files", "read").await, "read by connection 1");
    assert!(matches!(
        s.call("remote", "fetch").await,
        Err(Error::Unavailable { .. })
    ));

    // Retries after 1, 2, and 4 s; the fourth attempt connects.
    advance(Duration::from_secs(1)).await;
    assert_eq!(connector.connects("remote"), 2);
    advance(Duration::from_secs(2)).await;
    assert_eq!(connector.connects("remote"), 3);
    advance(Duration::from_secs(3)).await;
    assert_eq!(connector.connects("remote"), 3);
    advance(Duration::from_secs(1)).await;
    assert_eq!(connector.connects("remote"), 4);
    assert_eq!(s.offered().await, ["files__read", "remote__fetch"]);

    // Losing it withdraws its tools from new sessions until it's back.
    let before = s.registry.tools().snapshot();
    connector.crash("remote", 0);
    assert_eq!(s.offered().await, ["files__read"]);
    assert_eq!(before.len(), 2, "a running session keeps its tools");
    advance(Duration::from_secs(8)).await;
    assert_eq!(connector.connects("remote"), 5);
    assert_eq!(s.offered().await, ["files__read", "remote__fetch"]);
    host.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_idle_stop_resets_the_restart_backoff() {
    let s = Setup::new(FILES);
    let connector = FakeConnector::with([("files", Script::new(&["read"]))]);
    let host = s.host(&connector).await;
    host.started().await;
    // Two quick crashes: the next delay would be 4 s.
    connector.crash("files", 1);
    advance(Duration::from_secs(1)).await;
    connector.crash("files", 1);
    advance(Duration::from_secs(2)).await;
    assert_eq!(connector.connects("files"), 3);
    // Stopped for idleness, relaunched by a call, then crashing: restarted after 1 s again.
    advance(Duration::from_secs(61)).await;
    assert_eq!(connector.live("files"), 0);
    assert_eq!(s.call_text("files", "read").await, "read by connection 4");
    connector.crash("files", 1);
    advance(Duration::from_secs(1)).await;
    assert_eq!(connector.connects("files"), 5);
    host.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_stdio_server_that_fails_for_good_withdraws_its_cached_tools() {
    let s = Setup::new(FILES);
    let files = s.instance("files");
    let key = CacheKey {
        source: "sha256:v1".to_owned(),
        config_hash: files.config_hash.to_string(),
        server_version: Some("1.0".to_owned()),
    };
    s.cache().store(&name("files"), &key, &[tool("read")]);
    let mut script = Script::new(&["read"]);
    script.gate = Some(Arc::new(Semaphore::new(0)));
    let gate = script.gate.clone().unwrap();
    script.failures.push_back(Error::InvalidRequest(
        "the sandbox trampoline failed (exit status: 125)".to_owned(),
    ));
    let connector = FakeConnector::with([("files", script)]);
    let host = s.host(&connector).await;
    // Offered from the cache while it starts...
    assert_eq!(s.offered().await, ["files__read"]);
    gate.add_permits(1);
    host.started().await;
    // ...but not once it won't be retried: every call would fail.
    assert!(s.offered().await.is_empty());
    advance(Duration::from_secs(3600)).await;
    assert_eq!(connector.connects("files"), 1);
    host.stop().await;
}

#[tokio::test(start_paused = true)]
async fn rejected_credentials_are_not_retried() {
    let s = Setup::new(REMOTE);
    let mut remote = Script::new(&["fetch"]);
    remote
        .failures
        .push_back(Error::InvalidRequest("HTTP 401".to_owned()));
    let connector = FakeConnector::with([("remote", remote)]);
    let host = s.host(&connector).await;
    host.started().await;
    advance(Duration::from_secs(3600)).await;
    assert_eq!(connector.connects("remote"), 1);
    match s.call("remote", "fetch").await {
        Err(Error::Unavailable {
            message,
            retry_after: None,
        }) => assert!(message.contains("HTTP 401"), "{message}"),
        other => panic!("{other:?}"),
    }
    host.stop().await;
}

#[tokio::test]
async fn skill_directories_are_served_bare_and_indexed_with_the_prefix() {
    let skills = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(skills.path()).unwrap();
    let skill = base.join("review");
    std::fs::create_dir_all(skill.join("scripts")).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: review\ndescription: Review a pull request.\n---\nRun `scripts/check.sh`.\n",
    )
    .unwrap();
    std::fs::write(skill.join("scripts/check.sh"), "echo ok\n").unwrap();
    let s = Setup::new(&format!("{REMOTE}skills = [\"{}\"]\n", base.display()));
    let connector = FakeConnector::with([("remote", Script::new(&["fetch"]))]);
    let host = s.host(&connector).await;

    // The toolset serves its skills by bare name, whether or not the server is up.
    let toolset = s.registry.toolset("remote").unwrap();
    assert_eq!(
        toolset.list_skills().await.unwrap(),
        [SkillSummary {
            name: "review".to_owned(),
            description: "Review a pull request.".to_owned(),
        }]
    );
    let review = toolset.get_skill("review").await.unwrap();
    assert_eq!(review.body, "Run `scripts/check.sh`.");
    assert_eq!(review.dir, skill);
    assert!(matches!(
        toolset.get_skill("remote__review").await,
        Err(Error::NotFound(_))
    ));

    // The agent's skill index has it prefixed, with the directory `load_skill` reads files from.
    let agent = AgentId::new("agent").unwrap();
    let indexed = s.registry.agent_skills(&agent).await.unwrap();
    let review = indexed
        .get("remote__review")
        .expect("indexed with the prefix");
    assert_eq!(review.dir, skill);
    let listed = crate::skills::list_resources(&review.dir).unwrap();
    assert_eq!(listed.files, ["scripts/check.sh"]);
    host.stop().await;
}

#[tokio::test(start_paused = true)]
async fn list_changed_reaches_new_sessions_and_the_cache() {
    let s = Setup::new(FILES);
    let connector = FakeConnector::with([("files", Script::new(&["read"]))]);
    let host = s.host(&connector).await;
    host.started().await;
    let session = s.registry.tools().snapshot();
    connector.change_tools("files", &["read", "search"]);
    assert_eq!(s.offered().await, ["files__read", "files__search"]);
    assert_eq!(session.len(), 1, "a running session keeps its snapshot");
    let files = s.instance("files");
    assert_eq!(
        s.cache()
            .lookup(&name("files"), "sha256:v1", &files.config_hash),
        Some(vec![tool("read"), tool("search")])
    );
    host.stop().await;
}
