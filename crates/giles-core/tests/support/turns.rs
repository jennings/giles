//! A daemon wired the way `giles serve` wires it (`Runtime::start`, `AgentLoop::from_runtime`,
//! `Runtime::start_sessions`), with a scripted provider, a fake channel, and fake toolsets whose
//! calls the test controls. For the turn-control and approval tests, on paused Tokio time.
#![allow(dead_code)] // Each test file uses a part of it.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc as fmpsc;
use futures::stream::{self, BoxStream, StreamExt};
use giles_core::agent_loop::AgentLoop;
use giles_core::approvals::ApprovalBoard;
use giles_core::config::{Config, InstanceName, LoadOptions};
use giles_core::paths::DataRoot;
use giles_core::runtime::Runtime;
use giles_core::sessions::Sessions;
use giles_store::Entry;
use giles_types::{
    Block, CallContext, Channel, ChannelEvent, ChannelInfo, CompletionEvent, CompletionRequest,
    ConversationId, ConversationKind, ConversationRef, Error, EventId, InboundMessage, MessageId,
    ModelInfo, NativeId, OutboundMessage, OutputMode, Provider, Result, Role, Skill, SkillSummary,
    StopReason, ToolCall, ToolCallEvent, ToolCallHandle, ToolSpec, Toolset, Usage,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

/// How long a test waits for something to happen before it fails.
pub const WAIT: Duration = Duration::from_secs(10);

/// Each request takes the next script and streams it; an unscripted request fails.
#[derive(Default)]
pub struct FakeProvider {
    scripts: Mutex<VecDeque<Vec<CompletionEvent>>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl FakeProvider {
    /// Scripts the next response.
    pub fn push(&self, events: Vec<CompletionEvent>) {
        self.scripts.lock().unwrap().push_back(events);
    }

    /// Every request so far.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    async fn complete(
        &self,
        req: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<CompletionEvent>>> {
        self.requests.lock().unwrap().push(req);
        match self.scripts.lock().unwrap().pop_front() {
            Some(events) => Ok(stream::iter(events.into_iter().map(Ok)).boxed()),
            None => Err(Error::unavailable("unscripted")),
        }
    }
}

/// A whole response of `blocks`.
pub fn respond(blocks: Vec<Block>, stop: StopReason) -> Vec<CompletionEvent> {
    let mut events = Vec::new();
    for (index, block) in (0..).zip(blocks) {
        events.push(CompletionEvent::BlockStart {
            index,
            kind: block.kind(),
            tool: None,
        });
        events.push(CompletionEvent::BlockStop { index, block });
    }
    events.push(CompletionEvent::Usage(Usage {
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }));
    events.push(CompletionEvent::Stop(stop));
    events
}

/// A response asking for `calls` (id, tool name, input).
pub fn calls(calls: &[(&str, &str, Value)]) -> Vec<CompletionEvent> {
    let blocks = calls
        .iter()
        .map(|(id, name, input)| Block::ToolUse {
            id: (*id).to_owned(),
            name: (*name).to_owned(),
            input: input.clone(),
        })
        .collect();
    respond(blocks, StopReason::ToolUse)
}

/// A text response that ends the turn.
pub fn text(text: &str) -> Vec<CompletionEvent> {
    respond(vec![Block::text(text)], StopReason::EndTurn)
}

/// Logs `end <id>` when answered, `drop <id>` when dropped unanswered (cancelled).
struct Running {
    log: mpsc::UnboundedSender<String>,
    id: String,
    answered: bool,
}

impl Drop for Running {
    fn drop(&mut self) {
        let what = if self.answered { "end" } else { "drop" };
        let _ = self.log.send(format!("{what} {}", self.id));
    }
}

/// Tools `block` (answers once the test releases its call), `quick` (answers at once), and
/// `confirm` (asks for approval mid-call, then reports the answer). Every call logs `start
/// <id>`, then `end <id>` or `drop <id>`.
pub struct FakeToolset {
    log: mpsc::UnboundedSender<String>,
    releases: Mutex<HashMap<String, Arc<Notify>>>,
}

impl FakeToolset {
    /// Lets the `block` call `id` answer.
    pub fn release(&self, id: &str) {
        self.releases
            .lock()
            .unwrap()
            .get(id)
            .unwrap_or_else(|| panic!("no call {id}"))
            .notify_one();
    }
}

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: format!("The {name} tool."),
        input_schema: json!({"type": "object"}),
    }
}

#[async_trait]
impl Toolset for FakeToolset {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(["block", "confirm", "quick"].map(tool_spec).to_vec())
    }

    async fn list_skills(&self) -> Result<Vec<SkillSummary>> {
        Ok(Vec::new())
    }

    async fn get_skill(&self, name: &str) -> Result<Skill> {
        Err(Error::NotFound(name.to_owned()))
    }

    async fn call(&self, call: ToolCall, _ctx: CallContext) -> Result<ToolCallHandle> {
        let release = Arc::new(Notify::new());
        self.releases
            .lock()
            .unwrap()
            .insert(call.id.clone(), release.clone());
        let _ = self.log.send(format!("start {}", call.id));
        let mut running = Running {
            log: self.log.clone(),
            id: call.id.clone(),
            answered: false,
        };
        let (answers, mut answered) = mpsc::channel::<giles_types::ApprovalAnswer>(1);
        let (events_tx, events) = mpsc::unbounded_channel();
        let name = call.name.clone();
        let input = call.input.clone();
        // The call runs as long as its events stream is held.
        let body = async move {
            let text = match name.as_str() {
                "block" => {
                    release.notified().await;
                    format!("{name} ran with {input}")
                }
                "confirm" => {
                    let id = giles_types::ApprovalId::new("q1").unwrap();
                    let _ = events_tx.send(Ok(ToolCallEvent::ApprovalRequest {
                        id: id.clone(),
                        message: "Deploy to production?".to_owned(),
                        schema: None,
                    }));
                    match answered.recv().await {
                        Some(answer) => format!("answer {:?}", answer.decision),
                        None => "no answer".to_owned(),
                    }
                }
                _ => format!("{name} ran with {input}"),
            };
            running.answered = true;
            drop(running);
            let _ = events_tx.send(Ok(ToolCallEvent::Result {
                content: vec![Block::text(text)],
                is_error: false,
            }));
        };
        let events = futures::stream::select(
            stream::once(body).filter_map(|()| async { None }),
            tokio_stream_from(events),
        )
        .boxed();
        Ok(ToolCallHandle { events, answers })
    }
}

/// The receiver as a stream.
fn tokio_stream_from<T: Send + 'static>(
    mut rx: mpsc::UnboundedReceiver<T>,
) -> BoxStream<'static, T> {
    stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed()
}

struct FakeChannel {
    info: ChannelInfo,
    events: Mutex<Option<fmpsc::UnboundedReceiver<Result<ChannelEvent>>>>,
    sent: mpsc::UnboundedSender<String>,
}

#[async_trait]
impl Channel for FakeChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        match self.events.lock().unwrap().take() {
            Some(rx) => Ok(rx.boxed()),
            None => Ok(stream::pending().boxed()),
        }
    }

    async fn ack(&self, _event: &EventId) -> Result<()> {
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        let _ = self.sent.send(msg.markdown);
        Ok(MessageId::new("sent").unwrap())
    }
}

/// The time prefix of every message [`Daemon::say`] sends.
pub const PREFIX: &str = "[Thu 2026-10-22 17:30 BST]";

/// The daemon under test.
pub struct Daemon {
    _dir: TempDir,
    /// The runtime: store, registry.
    pub runtime: Runtime,
    /// The scripted provider.
    pub provider: Arc<FakeProvider>,
    /// The serial toolset `met` (`parallel = false`).
    pub met: Arc<FakeToolset>,
    /// The parallel toolset `par` (`parallel = true`).
    pub par: Arc<FakeToolset>,
    /// The serial toolset `slow`, with a 1 s deadline.
    pub slow: Arc<FakeToolset>,
    /// The approvals the agent loop asks and the session manager answers.
    pub approvals: Arc<ApprovalBoard>,
    tool_log: mpsc::UnboundedReceiver<String>,
    events: fmpsc::UnboundedSender<Result<ChannelEvent>>,
    sent: mpsc::UnboundedReceiver<String>,
    /// The session manager.
    pub sessions: Sessions,
    shutdown: CancellationToken,
    next_event: u32,
}

impl Daemon {
    /// Starts a daemon whose agent `jeeves` has `agent_toml` added to its model and tools.
    pub async fn start(agent_toml: &str) -> Self {
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
            "[owner]\nidentities = [\"fake:owner\"]\ntimezone = \"Europe/London\"\n\
             [provider.anthropic]\nconfig = { api_key = \"sk-test\" }\n\
             [channel.fake]\n\
             [toolset.met]\nurl = \"https://example.com/mcp\"\n\
             [toolset.par]\nurl = \"https://example.com/mcp\"\nparallel = true\n\
             [toolset.slow]\nurl = \"https://example.com/mcp\"\ndeadline = \"1s\"\n\
             [[routes]]\nagent = \"jeeves\"\n",
        );
        write(
            "agents/jeeves/agent.toml",
            &format!(
                "models = [\"anthropic/test-model\"]\n\
                 tools = [\"met__*\", \"par__*\", \"slow__*\"]\n{agent_toml}"
            ),
        );
        let config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let runtime = Runtime::start(root, Arc::new(config)).await.unwrap();

        let name = |n: &str| InstanceName::new(n).unwrap();
        let provider = Arc::new(FakeProvider::default());
        runtime
            .registry
            .register_provider(&name("anthropic"), provider.clone())
            .unwrap();
        let (log, tool_log) = mpsc::unbounded_channel();
        let toolset = || {
            Arc::new(FakeToolset {
                log: log.clone(),
                releases: Mutex::default(),
            })
        };
        let (met, par, slow) = (toolset(), toolset(), toolset());
        for (instance, toolset) in [("met", &met), ("par", &par), ("slow", &slow)] {
            runtime
                .registry
                .register_toolset(&name(instance), toolset.clone())
                .await
                .unwrap();
        }
        let (events, events_rx) = fmpsc::unbounded();
        let (sent_tx, sent) = mpsc::unbounded_channel();
        runtime
            .registry
            .register_channel(
                &name("fake"),
                Arc::new(FakeChannel {
                    info: ChannelInfo {
                        identity_namespace: "fake".to_owned(),
                        output: OutputMode::Blocks,
                        can_initiate: false,
                    },
                    events: Mutex::new(Some(events_rx)),
                    sent: sent_tx,
                }),
            )
            .unwrap();

        let shutdown = CancellationToken::new();
        let approvals = Arc::new(ApprovalBoard::default());
        let sessions = runtime
            .start_sessions(
                Arc::new(AgentLoop::from_runtime(&runtime, approvals.clone())),
                approvals.clone(),
                shutdown.clone(),
            )
            .await
            .unwrap();
        Self {
            _dir: dir,
            runtime,
            provider,
            met,
            par,
            slow,
            approvals,
            tool_log,
            events,
            sent,
            sessions,
            shutdown,
            next_event: 0,
        }
    }

    /// The owner sends `text` in their direct conversation.
    pub fn say(&mut self, text: &str) {
        self.next_event += 1;
        let event = ChannelEvent::Message {
            id: EventId::new(format!("e{}", self.next_event)).unwrap(),
            message: InboundMessage {
                conversation: ConversationRef {
                    id: ConversationId::new("owner").unwrap(),
                    kind: ConversationKind::Direct,
                    title: None,
                },
                sender: NativeId::new("owner").unwrap(),
                sender_aliases: Vec::new(),
                sender_authenticated: true,
                sender_display_name: None,
                text: text.to_owned(),
                attachments: Vec::new(),
                message_id: None,
                reply_to: None,
                sent_at: Some("2026-10-22T16:30:00Z".parse().unwrap()),
            },
        };
        self.events.unbounded_send(Ok(event)).unwrap();
    }

    /// Lets every task run as far as it can (store calls hold paused time).
    pub async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    /// The next message the channel sent.
    pub async fn next_sent(&mut self) -> String {
        tokio::time::timeout(WAIT, self.sent.recv())
            .await
            .expect("a message sent")
            .unwrap()
    }

    /// Every message the channel sent and the test hasn't read yet.
    pub fn sent(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(text) = self.sent.try_recv() {
            out.push(text);
        }
        out
    }

    /// The next `start`/`end`/`drop` entry of the toolsets' log.
    pub async fn next_tool_event(&mut self) -> String {
        tokio::time::timeout(WAIT, self.tool_log.recv())
            .await
            .expect("a tool event")
            .unwrap()
    }

    /// Every toolset log entry the test hasn't read yet.
    pub fn tool_events(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(entry) = self.tool_log.try_recv() {
            out.push(entry);
        }
        out
    }

    /// Waits until no turn is running in the open session.
    pub async fn idle(&self) {
        tokio::time::timeout(WAIT, async {
            loop {
                Self::settle().await;
                let open = self.runtime.store.open_sessions().await.unwrap();
                if open.iter().all(|s| !self.sessions.is_running(s.id)) {
                    return;
                }
            }
        })
        .await
        .expect("the turn to end");
    }

    /// The open session's transcript, one line per stored message: `user: <text>`,
    /// `assistant: <text or tool uses>`, `result <id>: <text>` (`!` marks an error), or `empty
    /// <id>` for an unfilled slot.
    pub async fn transcript(&self) -> Vec<String> {
        let session = self.runtime.store.open_sessions().await.unwrap().remove(0);
        let history = self.runtime.store.load_history(session.id).await.unwrap();
        history
            .messages
            .into_iter()
            .map(|m| match m.entry {
                Entry::EmptySlot { tool_use_id } => format!("empty {tool_use_id}"),
                Entry::Message(message) => {
                    let parts: Vec<String> = message.content.iter().map(render_block).collect();
                    match (message.role, message.content.first()) {
                        (Role::User, Some(Block::ToolResult { .. })) => parts.join(" "),
                        (Role::User, _) => format!("user: {}", parts.join(" ")),
                        (_, _) => format!("assistant: {}", parts.join(" ")),
                    }
                }
            })
            .collect()
    }

    /// Shuts the daemon down and waits for its tasks.
    pub async fn stop(self) {
        self.shutdown.cancel();
        self.sessions.closed().await;
    }
}

fn render_block(block: &Block) -> String {
    match block {
        Block::Text { text } => text.clone(),
        Block::ToolUse { id, name, .. } => format!("<{name} {id}>"),
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let text: Vec<String> = content.iter().map(render_block).collect();
            let mark = if *is_error { "!" } else { "" };
            format!("result{mark} {tool_use_id}: {}", text.join(" "))
        }
        other => format!("<{:?}>", other.kind()),
    }
}
