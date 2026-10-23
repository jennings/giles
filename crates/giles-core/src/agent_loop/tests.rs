//! Turns against a scripted fake provider, a fake toolset and channel, and a temporary store and
//! data root.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use giles_store::{Entry, InboxId, Session, SessionKey, SessionKind, Store};
use giles_types::{
    AgentId, Block, BlockKind, CacheRetention, CallContext, Channel, ChannelEvent, ChannelInfo,
    CompletionEvent, CompletionRequest, ConversationId, ConversationKind, ConversationRef, Error,
    EventId, InboundMessage, MediaRef, Message, MessageId, ModelInfo, NativeId, OutboundMessage,
    OutputMode, Provider, Result, Role, Skill, SkillSummary, StopReason, Timestamp, ToolCall,
    ToolCallEvent, ToolCallHandle, ToolSpec, Toolset, Usage, UserId,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::approvals::ApprovalBoard;
use crate::config::{BusyMode, Config, InstanceName, LoadOptions};
use crate::paths::DataRoot;
use crate::sessions::{Reply, Steering};

const AGENT: &str = "jeeves";
const SECRET: &str = "sk-very-secret-value";

fn now() -> Timestamp {
    "2026-10-22T21:00:00Z".parse().unwrap()
}

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        now()
    }
}

/// Advances one second on every reading, so stored rows are ordered in time.
#[derive(Default)]
struct TickingClock(std::sync::atomic::AtomicI64);

impl Clock for TickingClock {
    fn now(&self) -> Timestamp {
        let tick = self.0.fetch_add(1, Ordering::SeqCst);
        now() + jiff::SignedDuration::from_secs(tick)
    }
}

/// What the fake provider does for one request.
enum Script {
    /// `complete` succeeds and the stream yields these, then ends.
    Events(Vec<Result<CompletionEvent>>),
    /// The stream yields these, then never yields again.
    Hang(Vec<CompletionEvent>),
    /// `complete` fails.
    Refuse(Error),
}

#[derive(Default)]
struct FakeProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<CompletionRequest>>,
    requested: Notify,
    /// What `list_models` answers.
    models: Mutex<Vec<ModelInfo>>,
    /// How many times `list_models` was called.
    listed: AtomicUsize,
    /// Whether `list_models` fails.
    listing_fails: AtomicBool,
}

impl FakeProvider {
    fn push(&self, script: Script) {
        self.scripts.lock().unwrap().push_back(script);
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        self.listed.fetch_add(1, Ordering::SeqCst);
        if self.listing_fails.load(Ordering::SeqCst) {
            return Err(Error::unavailable("the model list is down"));
        }
        Ok(self.models.lock().unwrap().clone())
    }

    async fn complete(
        &self,
        req: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<CompletionEvent>>> {
        self.requests.lock().unwrap().push(req);
        self.requested.notify_one();
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("an unscripted request");
        match script {
            Script::Events(events) => Ok(stream::iter(events).boxed()),
            Script::Hang(events) => Ok(stream::iter(events.into_iter().map(Ok))
                .chain(stream::pending())
                .boxed()),
            Script::Refuse(error) => Err(error),
        }
    }
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 3,
        cache_write_tokens: 0,
    }
}

/// The events of one streamed block.
fn block_events(index: u32, block: Block) -> Vec<CompletionEvent> {
    let (kind, tool, delta) = match &block {
        Block::Text { text } => (
            BlockKind::Text,
            None,
            Some(CompletionEvent::TextDelta {
                index,
                text: text.clone(),
            }),
        ),
        Block::ToolUse { id, name, input } => (
            BlockKind::ToolUse,
            Some((id.clone(), name.clone())),
            Some(CompletionEvent::ToolInputDelta {
                index,
                partial_json: input.to_string(),
            }),
        ),
        other => (other.kind(), None, None),
    };
    let mut events = vec![CompletionEvent::BlockStart { index, kind, tool }];
    events.extend(delta);
    events.push(CompletionEvent::BlockStop { index, block });
    events
}

/// A whole response of `blocks`.
fn response(blocks: Vec<Block>, stop: StopReason) -> Script {
    let mut events = vec![CompletionEvent::Usage(usage(10, 0))];
    for (index, block) in blocks.into_iter().enumerate() {
        events.extend(block_events(u32::try_from(index).unwrap(), block));
    }
    events.push(CompletionEvent::Usage(usage(10, 5)));
    events.push(CompletionEvent::Stop(stop));
    Script::Events(events.into_iter().map(Ok).collect())
}

fn text(text: &str) -> Script {
    response(vec![Block::text(text)], StopReason::EndTurn)
}

fn tool_use(id: &str, name: &str, input: Value) -> Block {
    Block::ToolUse {
        id: id.to_owned(),
        name: name.to_owned(),
        input,
    }
}

/// Sets a flag when dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Tools `list_issues` (answers), `fail` (refuses the call), `slow` (never answers; records
/// when its call is dropped), and `gone`.
#[derive(Default)]
struct FakeToolset {
    calls: Mutex<Vec<(ToolCall, CallContext)>>,
    slow_started: Notify,
    slow_dropped: Arc<AtomicBool>,
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
        Ok(["fail", "gone", "list_issues", "slow"]
            .map(tool_spec)
            .to_vec())
    }

    async fn list_skills(&self) -> Result<Vec<SkillSummary>> {
        Ok(Vec::new())
    }

    async fn get_skill(&self, name: &str) -> Result<Skill> {
        Err(Error::NotFound(name.to_owned()))
    }

    async fn call(&self, call: ToolCall, ctx: CallContext) -> Result<ToolCallHandle> {
        self.calls.lock().unwrap().push((call.clone(), ctx));
        let (answers, _) = tokio::sync::mpsc::channel(1);
        let events = match call.name.as_str() {
            "fail" => return Err(Error::unavailable("the server is down")),
            "slow" => {
                self.slow_started.notify_one();
                let guard = DropFlag(self.slow_dropped.clone());
                stream::once(async move {
                    let _guard = guard;
                    std::future::pending::<Result<ToolCallEvent>>().await
                })
                .boxed()
            }
            // Answers with its input, and each media input as an image.
            _ => {
                let mut content = vec![Block::text(format!(
                    "{} ran with {}",
                    call.name, call.input
                ))];
                content.extend(call.media.iter().map(|media| Block::Image {
                    media: media.clone(),
                }));
                stream::iter([
                    Ok(ToolCallEvent::Progress {
                        progress: 1.0,
                        total: None,
                        message: None,
                    }),
                    Ok(ToolCallEvent::Result {
                        content,
                        is_error: false,
                    }),
                ])
                .boxed()
            }
        };
        Ok(ToolCallHandle { events, answers })
    }
}

struct FakeChannel {
    info: ChannelInfo,
    sent: mpsc::UnboundedSender<String>,
}

#[async_trait]
impl Channel for FakeChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        Ok(stream::pending().boxed())
    }

    async fn ack(&self, _event: &EventId) -> Result<()> {
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        self.sent.send(msg.markdown).unwrap();
        Ok(MessageId::new("sent").unwrap())
    }
}

struct Harness {
    dir: TempDir,
    registry: Arc<Registry>,
    store: Store,
    provider: Arc<FakeProvider>,
    toolset: Arc<FakeToolset>,
    sent: mpsc::UnboundedReceiver<String>,
    session: Session,
    next_event: u32,
}

impl Harness {
    async fn new() -> Self {
        Self::with_agent(
            "models = [\"anthropic/test-model\"]\ntools = [\"github__*\", \"memory\"]\n",
        )
        .await
    }

    async fn with_agent(agent_toml: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        let write = |relative: &str, contents: &str| {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        write(
            "giles.toml",
            "[owner]\nidentities = [\"fake:owner\"]\ntimezone = \"America/Los_Angeles\"\n\
             [provider.anthropic]\nconfig = { api_key = { secret = \"anthropic\" } }\n\
             [provider.ollama]\ncomponent = \"openai\"\n\
             config = { base_url = \"http://127.0.0.1:11434/v1\", \
             models.\"qwen3:8b\" = { context_window = 2000 } }\n\
             [toolset.github]\nurl = \"https://example.com/mcp\"\n\
             [channel.fake]\n",
        );
        write("secrets.toml", &format!("anthropic = \"{SECRET}\"\n"));
        std::fs::set_permissions(
            dir.path().join("secrets.toml"),
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )
        .unwrap();
        write(&format!("agents/{AGENT}/agent.toml"), agent_toml);
        write(&format!("agents/{AGENT}/SOUL.md"), "You are Jeeves.");
        write(&format!("agents/{AGENT}/MEMORY.md"), "Tea at four.");
        let config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let registry = Arc::new(Registry::new(root, Arc::new(config)));
        let provider = Arc::new(FakeProvider::default());
        registry
            .register_provider(&name("anthropic"), provider.clone())
            .unwrap();
        let toolset = Arc::new(FakeToolset::default());
        registry
            .register_toolset(&name("github"), toolset.clone())
            .await
            .unwrap();
        registry.tools().set_core_tool(tool_spec("memory")).unwrap();
        let (sent_tx, sent) = mpsc::unbounded_channel();
        registry
            .register_channel(
                &name("fake"),
                Arc::new(FakeChannel {
                    info: ChannelInfo {
                        identity_namespace: "fake".to_owned(),
                        output: OutputMode::Blocks,
                        can_initiate: false,
                    },
                    sent: sent_tx,
                }),
            )
            .unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let (session, _) = store
            .open_session(
                SessionKey {
                    agent: AgentId::new(AGENT).unwrap(),
                    channel: "fake".to_owned(),
                    conversation: ConversationId::new("owner").unwrap(),
                },
                SessionKind::Direct,
                Some(owner()),
                now(),
            )
            .await
            .unwrap();
        Self {
            dir,
            registry,
            store,
            provider,
            toolset,
            sent,
            session,
            next_event: 0,
        }
    }

    /// An agent loop over this harness (a new one simulates a restart).
    fn agent_loop(&self) -> AgentLoop {
        self.agent_loop_with(Arc::new(PolicyDispatcher::new(self.registry.clone())))
    }

    fn agent_loop_with(&self, dispatcher: Arc<dyn ToolDispatcher>) -> AgentLoop {
        AgentLoop::new(
            self.registry.clone(),
            self.store.clone(),
            Arc::new(FixedClock),
            dispatcher,
            Arc::new(ApprovalBoard::default()),
        )
    }

    /// An agent loop whose clock advances a second per reading.
    fn ticking_agent_loop(&self) -> AgentLoop {
        AgentLoop::new(
            self.registry.clone(),
            self.store.clone(),
            Arc::new(TickingClock::default()),
            Arc::new(PolicyDispatcher::new(self.registry.clone())),
            Arc::new(ApprovalBoard::default()),
        )
    }

    fn turn(&mut self, texts: &[&str]) -> (Turn, CancellationToken) {
        let cancel = CancellationToken::new();
        let messages = texts
            .iter()
            .map(|text| {
                self.next_event += 1;
                TurnMessage {
                    inbox: InboxId::new(i64::from(self.next_event)),
                    event: EventId::new(format!("e{}", self.next_event)).unwrap(),
                    received_at: now(),
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
                        text: (*text).to_owned(),
                        attachments: Vec::new(),
                        message_id: None,
                        reply_to: None,
                        sent_at: Some("2026-10-22T20:58:00Z".parse().unwrap()),
                    },
                }
            })
            .collect();
        let turn = Turn {
            session: self.session.clone(),
            requester: Some(owner()),
            messages,
            busy_mode: BusyMode::Steer,
            cancel: cancel.clone(),
            steering: Steering::detached(),
            reply: Reply::for_session(&self.session, self.registry.clone()),
            job: None,
        };
        (turn, cancel)
    }

    /// The session's transcript: role and content of each row, `None` for an empty slot.
    async fn transcript(&self) -> Vec<Option<(Role, Vec<Block>)>> {
        self.store
            .load_history(self.session.id)
            .await
            .unwrap()
            .messages
            .into_iter()
            .map(|m| match m.entry {
                Entry::Message(Message { role, content, .. }) => Some((role, content)),
                Entry::EmptySlot { .. } => None,
            })
            .collect()
    }

    fn sent(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(text) = self.sent.try_recv() {
            out.push(text);
        }
        out
    }

    async fn usage_requests(&self) -> u64 {
        self.store
            .usage_by_agent(None)
            .await
            .unwrap()
            .first()
            .map_or(0, |u| u.requests)
    }
}

fn name(name: &str) -> InstanceName {
    InstanceName::new(name).unwrap()
}

fn owner() -> UserId {
    UserId::new("owner").unwrap()
}

const PREFIX: &str = "[Thu 2026-10-22 13:58 PDT]";

fn user(text: &str) -> Option<(Role, Vec<Block>)> {
    Some((Role::User, vec![Block::text(format!("{PREFIX} {text}"))]))
}

fn assistant(content: Vec<Block>) -> Option<(Role, Vec<Block>)> {
    Some((Role::Assistant, content))
}

fn result(id: &str, text: &str, is_error: bool) -> Option<(Role, Vec<Block>)> {
    Some((
        Role::User,
        vec![Block::ToolResult {
            tool_use_id: id.to_owned(),
            content: vec![Block::text(text)],
            is_error,
        }],
    ))
}

#[tokio::test]
async fn a_text_turn_stores_both_messages_replies_and_records_usage() {
    let mut h = Harness::new().await;
    h.provider.push(text("Good afternoon."));
    let (turn, _) = h.turn(&["Hello", "Are you there?"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    assert_eq!(
        h.transcript().await,
        [
            user("Hello"),
            user("Are you there?"),
            assistant(vec![Block::text("Good afternoon.")]),
        ]
    );
    assert_eq!(h.sent(), ["Good afternoon."]);

    let requests = h.provider.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.model, "test-model");
    assert_eq!(request.params.cache_retention, CacheRetention::Long);
    let system = match request.system.as_slice() {
        [Block::Text { text }] => text.clone(),
        other => panic!("unexpected system {other:?}"),
    };
    assert!(system.starts_with("You are Jeeves."));
    assert!(system.contains("Tea at four."));
    assert!(!system.contains("2026"), "the time never goes in system");
    let tools: Vec<_> = request.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        tools,
        [
            "github__fail",
            "github__gone",
            "github__list_issues",
            "github__slow",
            "memory"
        ]
    );
    assert_eq!(request.messages.len(), 2);
    assert_eq!(
        request.messages[0].content,
        [Block::text(format!("{PREFIX} Hello"))]
    );

    let rows = h.store.usage_by_agent(None).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent.as_str(), AGENT);
    assert_eq!(rows[0].requests, 1);
    assert_eq!(rows[0].fallbacks, 0);
    assert_eq!(rows[0].tokens, usage_totals(1));

    // The stored assistant message says where it came from.
    let history = h.store.load_history(h.session.id).await.unwrap();
    let Entry::Message(stored) = &history.messages[2].entry else {
        panic!("not a message");
    };
    assert_eq!(stored.meta.provider.as_deref(), Some("anthropic"));
    assert_eq!(stored.meta.model.as_deref(), Some("test-model"));
    assert_eq!(stored.meta.usage, Some(usage(10, 5)));
    let Entry::Message(first) = &history.messages[0].entry else {
        panic!("not a message");
    };
    assert_eq!(first.meta.sender, Some(owner()));
}

/// The usage of `requests` scripted responses.
fn usage_totals(requests: u64) -> Usage {
    Usage {
        input_tokens: 10 * requests,
        output_tokens: 5 * requests,
        cache_read_tokens: 3 * requests,
        cache_write_tokens: 0,
    }
}

#[tokio::test]
async fn a_tool_round_trip_fills_the_slot_and_asks_again() {
    let mut h = Harness::new().await;
    h.provider.push(response(
        vec![
            Block::text("Let me look."),
            tool_use("t1", "github__list_issues", json!({"repo": "giles"})),
        ],
        StopReason::ToolUse,
    ));
    h.provider.push(text("There are two."));
    let (turn, _) = h.turn(&["Any issues?"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    let ran = "list_issues ran with {\"repo\":\"giles\"}";
    assert_eq!(
        h.transcript().await,
        [
            user("Any issues?"),
            assistant(vec![
                Block::text("Let me look."),
                tool_use("t1", "github__list_issues", json!({"repo": "giles"})),
            ]),
            result("t1", ran, false),
            assistant(vec![Block::text("There are two.")]),
        ]
    );
    // Only text reaches the channel, one piece per assistant message.
    assert_eq!(h.sent(), ["Let me look.", "There are two."]);

    // The toolset got the bare name and the turn's context.
    let calls = h.toolset.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    let (call, ctx) = &calls[0];
    assert_eq!(call.id, "t1");
    assert_eq!(call.name, "list_issues");
    assert_eq!(call.input, json!({"repo": "giles"}));
    assert_eq!(ctx.agent.as_str(), AGENT);
    assert_eq!(ctx.requester, Some(owner()));
    let conversation = ctx.conversation.as_ref().unwrap();
    assert_eq!(conversation.channel, "fake");
    assert_eq!(conversation.conversation.id.as_str(), "owner");

    // The second request carries the result, with the same prefix as the first.
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].system, requests[1].system);
    assert_eq!(requests[0].tools, requests[1].tools);
    assert_eq!(requests[1].messages[..1], requests[0].messages[..]);
    assert_eq!(requests[1].messages.len(), 3);
    assert_eq!(h.usage_requests().await, 2);
}

#[tokio::test]
async fn several_calls_run_in_order_and_each_gets_a_result() {
    let mut h = Harness::new().await;
    h.provider.push(response(
        vec![
            tool_use("t1", "github__list_issues", json!({"n": 1})),
            tool_use("t2", "not_offered", json!({})),
            tool_use("t3", "memory", json!({})),
            tool_use("t4", "github__fail", json!({})),
            tool_use("t5", "github__list_issues", json!({"n": 2})),
        ],
        StopReason::ToolUse,
    ));
    h.provider.push(text("Done."));
    let (turn, _) = h.turn(&["Go"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    let transcript = h.transcript().await;
    assert_eq!(
        transcript[2..7],
        [
            result("t1", "list_issues ran with {\"n\":1}", false),
            result("t2", "tool unavailable", true),
            // A core tool without an implementation yet.
            result("t3", "tool unavailable", true),
            result("t4", "unavailable: the server is down", true),
            result("t5", "list_issues ran with {\"n\":2}", false),
        ]
    );
    let calls: Vec<String> = h
        .toolset
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(call, _)| call.id.clone())
        .collect();
    assert_eq!(calls, ["t1", "t4", "t5"]);
}

#[tokio::test]
async fn a_tool_the_toolset_dropped_stays_listed_but_is_unavailable() {
    let mut h = Harness::new().await;
    let agent_loop = h.agent_loop();
    h.provider.push(text("Hi."));
    let (turn, _) = h.turn(&["Hi"]);
    agent_loop.run_turn(turn).await;

    // The toolset drops `gone` mid-session.
    h.registry.tools().set_toolset_tools(
        &name("github"),
        vec![
            tool_spec("list_issues"),
            tool_spec("fail"),
            tool_spec("slow"),
        ],
    );
    h.provider.push(response(
        vec![tool_use("t1", "github__gone", json!({}))],
        StopReason::ToolUse,
    ));
    h.provider.push(text("It's gone."));
    let (turn, _) = h.turn(&["Use gone"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);

    let requests = h.provider.requests();
    assert_eq!(requests[0].tools, requests[1].tools);
    assert!(requests[1].tools.iter().any(|t| t.name == "github__gone"));
    assert_eq!(
        h.transcript().await[4],
        result("t1", "tool unavailable", true)
    );
    assert!(h.toolset.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn max_tokens_truncates_every_call_and_ends_the_turn() {
    let mut h = Harness::new().await;
    h.provider.push(response(
        vec![
            Block::text("I'll check both."),
            tool_use("t1", "github__list_issues", json!({})),
            tool_use("t2", "github__list_issues", json!({"cut": true})),
        ],
        StopReason::MaxTokens,
    ));
    let (turn, _) = h.turn(&["Check"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    let transcript = h.transcript().await;
    assert_eq!(
        transcript[2..],
        [
            result("t1", "truncated", true),
            result("t2", "truncated", true)
        ]
    );
    assert_eq!(h.provider.requests().len(), 1);
    assert!(h.toolset.calls.lock().unwrap().is_empty());
    assert_eq!(h.sent(), ["I'll check both."]);
    assert_eq!(h.usage_requests().await, 1);
}

#[tokio::test]
async fn cancelling_mid_stream_stores_nothing_of_the_response() {
    let mut h = Harness::new().await;
    let mut events = vec![CompletionEvent::Usage(usage(10, 0))];
    events.extend(block_events(0, Block::text("Complete block")));
    events.push(CompletionEvent::BlockStart {
        index: 1,
        kind: BlockKind::Text,
        tool: None,
    });
    h.provider.push(Script::Hang(events));
    let (turn, cancel) = h.turn(&["Tell me a long story"]);
    let agent_loop = Arc::new(h.agent_loop());
    let running = tokio::spawn({
        let agent_loop = agent_loop.clone();
        async move { agent_loop.run_turn(turn).await }
    });
    h.provider.requested.notified().await;
    cancel.cancel();
    assert_eq!(running.await.unwrap(), TurnOutcome::Cancelled);

    assert_eq!(h.transcript().await, [user("Tell me a long story")]);
    // The text block that ended went out at its end; the unstarted one sends nothing.
    assert_eq!(h.sent(), ["Complete block"]);
    // The request was made, so it has a usage row.
    assert_eq!(h.usage_requests().await, 1);
}

#[tokio::test]
async fn cancelling_mid_tool_cancels_the_call_and_fills_the_slots() {
    let mut h = Harness::new().await;
    h.provider.push(response(
        vec![
            tool_use("t1", "github__slow", json!({})),
            tool_use("t2", "github__list_issues", json!({})),
        ],
        StopReason::ToolUse,
    ));
    let (turn, cancel) = h.turn(&["Slow, please"]);
    let agent_loop = Arc::new(h.agent_loop());
    let running = tokio::spawn({
        let agent_loop = agent_loop.clone();
        async move { agent_loop.run_turn(turn).await }
    });
    h.toolset.slow_started.notified().await;
    assert!(!h.toolset.slow_dropped.load(Ordering::SeqCst));
    cancel.cancel();
    assert_eq!(running.await.unwrap(), TurnOutcome::Cancelled);

    assert!(h.toolset.slow_dropped.load(Ordering::SeqCst));
    assert_eq!(
        h.transcript().await[2..],
        [
            result("t1", "cancelled", true),
            result("t2", "cancelled", true)
        ]
    );
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(h.toolset.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_refused_request_fails_the_turn_with_a_scrubbed_notice() {
    let mut h = Harness::new().await;
    h.provider
        .push(Script::Refuse(Error::InvalidRequest(format!(
            "bad key {SECRET}"
        ))));
    let (turn, _) = h.turn(&["Hi"]);
    let outcome = h.agent_loop().run_turn(turn).await;
    let TurnOutcome::Failed(reason) = outcome else {
        panic!("not a failure: {outcome:?}");
    };
    assert!(!reason.contains(SECRET), "{reason}");

    let sent = h.sent();
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].starts_with("Sorry, the model request failed: invalid request: bad key "),
        "{}",
        sent[0]
    );
    assert!(!sent[0].contains(SECRET));
    assert_eq!(h.transcript().await, [user("Hi")]);
    // Refused before streaming: nothing was generated, so no usage row.
    assert_eq!(h.usage_requests().await, 0);
}

#[tokio::test]
async fn a_stream_error_fails_the_turn_after_recording_usage() {
    let mut h = Harness::new().await;
    h.provider.push(Script::Events(vec![
        Ok(CompletionEvent::Usage(usage(10, 1))),
        Ok(CompletionEvent::BlockStart {
            index: 0,
            kind: BlockKind::Text,
            tool: None,
        }),
        Err(Error::unavailable("overloaded")),
    ]));
    let (turn, _) = h.turn(&["Hi"]);
    assert!(matches!(
        h.agent_loop().run_turn(turn).await,
        TurnOutcome::Failed(_)
    ));
    assert_eq!(
        h.sent(),
        ["Sorry, the model request failed: unavailable: overloaded"]
    );
    assert_eq!(h.usage_requests().await, 1);

    // A stream that ends without `Stop` is a failure too.
    h.provider
        .push(Script::Events(vec![Ok(CompletionEvent::Usage(usage(
            1, 1,
        )))]));
    let (turn, _) = h.turn(&["Again"]);
    assert!(matches!(
        h.agent_loop().run_turn(turn).await,
        TurnOutcome::Failed(_)
    ));
    assert_eq!(h.usage_requests().await, 2);
}

#[tokio::test]
async fn the_prefix_is_stable_across_turns_and_restarts() {
    let mut h = Harness::new().await;
    let agent_loop = h.agent_loop();
    h.provider.push(text("One."));
    let (turn, _) = h.turn(&["First"]);
    agent_loop.run_turn(turn).await;

    // Memory changes mid-session don't reach this session's prompt.
    std::fs::write(
        h.dir.path().join(format!("agents/{AGENT}/MEMORY.md")),
        "Tea at five.",
    )
    .unwrap();
    h.provider.push(text("Two."));
    let (turn, _) = h.turn(&["Second"]);
    agent_loop.run_turn(turn).await;

    let requests = h.provider.requests();
    assert_eq!(requests[0].system, requests[1].system);
    assert_eq!(requests[0].tools, requests[1].tools);
    // The history only grows: the first request is a prefix of the second.
    assert_eq!(requests[1].messages[..1], requests[0].messages[..]);
    assert_eq!(requests[1].messages[1].content, [Block::text("One.")]);
    assert_eq!(requests[1].messages.len(), 3);

    // A restart (a new loop) reloads the same history and takes a fresh snapshot.
    let restarted = h.agent_loop();
    h.provider.push(text("Three."));
    let (turn, _) = h.turn(&["Third"]);
    restarted.run_turn(turn).await;
    let requests = h.provider.requests();
    assert_eq!(requests[2].messages[..3], requests[1].messages[..]);
    assert_ne!(requests[2].system, requests[1].system);
    let system = |r: &CompletionRequest| match &r.system[0] {
        Block::Text { text } => text.clone(),
        _ => panic!("not text"),
    };
    assert!(system(&requests[2]).contains("Tea at five."));
    assert!(system(&requests[1]).contains("Tea at four."));
}

#[tokio::test]
async fn rebuilding_refreshes_system_and_keeps_the_tools() {
    let h = Harness::new().await;
    let agent_loop = h.agent_loop();
    let config = h.registry.config().clone();
    let agent = &config.agents[&AgentId::new(AGENT).unwrap()];
    let first = agent_loop.session_prompt(&h.session, agent).await.unwrap();
    assert!(Arc::ptr_eq(
        &first,
        &agent_loop.session_prompt(&h.session, agent).await.unwrap()
    ));

    std::fs::write(
        h.dir.path().join(format!("agents/{AGENT}/MEMORY.md")),
        "Tea at five.",
    )
    .unwrap();
    h.registry.tools().remove_toolset(&name("github"));
    let rebuilt = agent_loop
        .rebuild_prompt(&h.session, agent, &first)
        .await
        .unwrap();
    assert!(rebuilt.system_text().contains("Tea at five."));
    assert!(Arc::ptr_eq(&first.tools, &rebuilt.tools));
    // Later turns use the rebuilt prompt.
    assert!(Arc::ptr_eq(
        &rebuilt,
        &agent_loop.session_prompt(&h.session, agent).await.unwrap()
    ));
}

#[tokio::test]
async fn a_turn_for_an_unknown_agent_fails_with_a_notice() {
    let mut h = Harness::new().await;
    let (mut turn, _) = h.turn(&["Hi"]);
    turn.session.agent = AgentId::new("gone").unwrap();
    let outcome = h.agent_loop().run_turn(turn).await;
    assert_eq!(
        outcome,
        TurnOutcome::Failed("agent `gone` is not configured".to_owned())
    );
    assert_eq!(
        h.sent(),
        ["Sorry, something went wrong: agent `gone` is not configured"]
    );
}

#[tokio::test]
async fn the_owner_timezone_falls_back_to_the_agent_timezone() {
    let mut h =
        Harness::with_agent("models = [\"anthropic/test-model\"]\ntimezone = \"Asia/Tokyo\"\n")
            .await;
    let config = h.registry.config().clone();
    let agent = &config.agents[&AgentId::new(AGENT).unwrap()];
    // The owner's zone wins.
    assert_eq!(
        history::message_timezone(&config, agent).iana_name(),
        Some("America/Los_Angeles")
    );
    let mut without_owner = (*config).clone();
    without_owner.owner.timezone = None;
    assert_eq!(
        history::message_timezone(&without_owner, agent).iana_name(),
        Some("Asia/Tokyo")
    );

    // An agent without tools offers none.
    h.provider.push(text("Hi."));
    let (turn, _) = h.turn(&["Hi"]);
    h.agent_loop().run_turn(turn).await;
    assert!(h.provider.requests()[0].tools.is_empty());
}

/// Answers only the first call of each batch (or panics, if `panic`).
struct PartialDispatcher {
    panic: bool,
}

impl ToolDispatcher for PartialDispatcher {
    fn dispatch(&self, _batch: ToolBatch) -> BoxStream<'static, (usize, ToolOutput)> {
        assert!(!self.panic, "dispatcher exploded");
        stream::iter([(
            0,
            ToolOutput {
                content: vec![Block::text("first only")],
                is_error: false,
            },
        )])
        .boxed()
    }
}

fn two_calls() -> Script {
    response(
        vec![
            tool_use("t1", "github__list_issues", json!({})),
            tool_use("t2", "github__list_issues", json!({})),
        ],
        StopReason::ToolUse,
    )
}

#[tokio::test]
async fn calls_the_dispatcher_leaves_unanswered_are_skipped() {
    let mut h = Harness::new().await;
    h.provider.push(two_calls());
    h.provider.push(text("Partly done."));
    let (turn, _) = h.turn(&["Both"]);
    let agent_loop = h.agent_loop_with(Arc::new(PartialDispatcher { panic: false }));
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    assert_eq!(
        h.transcript().await[2..4],
        [
            result("t1", "first only", false),
            result("t2", "skipped", true)
        ]
    );
}

#[tokio::test]
async fn a_panicking_turn_fails_fills_its_slots_and_says_so() {
    let mut h = Harness::new().await;
    h.provider.push(two_calls());
    let (turn, _) = h.turn(&["Both"]);
    let agent_loop = h.agent_loop_with(Arc::new(PartialDispatcher { panic: true }));
    assert_eq!(
        agent_loop.run_turn(turn).await,
        TurnOutcome::Failed("the turn panicked: dispatcher exploded".to_owned())
    );
    assert_eq!(
        h.transcript().await[2..],
        [
            result("t1", "interrupted", true),
            result("t2", "interrupted", true)
        ]
    );
    assert_eq!(h.sent(), [run::PANIC_NOTICE]);
}

#[tokio::test]
async fn slots_an_earlier_turn_left_empty_become_interrupted() {
    let mut h = Harness::new().await;
    // As if a turn had stored a call and then died without filling its slot.
    h.store
        .append_messages(
            h.session.id,
            vec![Message {
                role: Role::Assistant,
                content: vec![tool_use("t0", "github__list_issues", json!({}))],
                meta: giles_types::MessageMeta::new(now()),
            }],
        )
        .await
        .unwrap();
    // This turn's own truncated call must not relabel the old slot.
    h.provider.push(response(
        vec![tool_use("t1", "github__list_issues", json!({}))],
        StopReason::MaxTokens,
    ));
    let (turn, _) = h.turn(&["Hi"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);
    let transcript = h.transcript().await;
    assert_eq!(transcript[1], result("t0", "interrupted", true));
    assert_eq!(transcript[4], result("t1", "truncated", true));
}

#[tokio::test]
async fn a_job_turn_uses_short_retention_sends_nothing_and_is_not_cached() {
    let mut h = Harness::new().await;
    h.session = h
        .store
        .create_job_session(AgentId::new(AGENT).unwrap(), Some(owner()), now())
        .await
        .unwrap();
    let agent_loop = h.agent_loop();
    h.provider.push(text("Report: all quiet."));
    let (turn, _) = h.turn(&["Check the garden"]);
    assert!(turn.reply.is_none());
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);

    let requests = h.provider.requests();
    assert_eq!(requests[0].params.cache_retention, CacheRetention::Short);
    assert!(h.sent().is_empty());
    assert_eq!(
        h.transcript().await[1],
        assistant(vec![Block::text("Report: all quiet.")])
    );

    // Without a session key nothing is cached: each turn reads the files again.
    std::fs::write(
        h.dir.path().join(format!("agents/{AGENT}/MEMORY.md")),
        "Tea at five.",
    )
    .unwrap();
    h.provider.push(text("Still quiet."));
    let (turn, _) = h.turn(&["Again"]);
    agent_loop.run_turn(turn).await;
    let requests = h.provider.requests();
    let system = |r: &CompletionRequest| match &r.system[0] {
        Block::Text { text } => text.clone(),
        _ => panic!("not text"),
    };
    assert!(system(&requests[1]).contains("Tea at five."));
}

#[tokio::test]
async fn a_job_turn_denies_ask_tools_as_unattended() {
    let mut h = Harness::with_agent(
        "models = [\"anthropic/test-model\"]\ntools = [\"github__*\"]\n\
         ask = [\"github__list_issues\"]\n",
    )
    .await;
    h.session = h
        .store
        .create_job_session(AgentId::new(AGENT).unwrap(), Some(owner()), now())
        .await
        .unwrap();
    h.provider.push(one_call(1));
    h.provider.push(text("I wasn't allowed to look."));
    let (turn, _) = h.turn(&["Check the issues"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    assert_eq!(
        h.transcript().await[2],
        result("t1", crate::approvals::UNATTENDED_RESULT, true)
    );
    assert!(toolset_calls(&h).is_empty());
    assert!(h.sent().is_empty());
}

/// A job run in the harness's session: `text` as its prompt, fired at [`now`].
fn job_turn(
    h: &Harness,
    text: &str,
    model: Option<&str>,
    approvals: Option<crate::approvals::ApprovalTarget>,
) -> (Turn, CancellationToken) {
    let tz = jiff::tz::TimeZone::get("America/Los_Angeles").unwrap();
    let cancel = CancellationToken::new();
    let turn = Turn {
        session: h.session.clone(),
        requester: None,
        messages: Vec::new(),
        busy_mode: BusyMode::Steer,
        cancel: cancel.clone(),
        steering: Steering::detached(),
        reply: None,
        job: Some(crate::sessions::JobTurn {
            prompt: history::job_message(text, now(), None, &tz),
            model: model.map(|m| m.parse().unwrap()),
            approvals,
        }),
    };
    (turn, cancel)
}

#[tokio::test]
async fn a_job_run_in_a_dm_stores_its_prompt_and_uses_its_model_and_the_jobs_limits() {
    let mut h = Harness::with_agent(
        "models = [\"anthropic/test-model\"]\ntools = [\"github__*\"]\n\
         max_iterations_jobs = 1\n",
    )
    .await;
    let ollama = Arc::new(FakeProvider::default());
    h.registry
        .register_provider(&name("ollama"), ollama.clone())
        .unwrap();
    ollama.push(one_call(1));
    ollama.push(text("Nothing to report."));
    let (turn, _) = job_turn(&h, "Check in.", Some("ollama/qwen3:8b"), None);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    // The DM session (no job session) still gets the jobs' cap and short retention.
    assert!(h.provider.requests().is_empty());
    let requests = ollama.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].model, "qwen3:8b");
    assert!(
        requests
            .iter()
            .all(|r| r.params.cache_retention == CacheRetention::Short)
    );
    assert!(requests[1].tools.is_empty());
    assert!(toolset_calls(&h).is_empty());
    let transcript = h.transcript().await;
    assert_eq!(
        transcript[0],
        Some((
            Role::User,
            vec![Block::text("[Thu 2026-10-22 14:00 PDT] Check in.")]
        ))
    );
    assert_eq!(transcript[2], result("t1", "iteration limit", true));
    assert_eq!(
        transcript[3],
        assistant(vec![Block::text("Nothing to report.")])
    );
    // The reply is the scheduler's to deliver, not sent into the conversation.
    assert!(h.sent().is_empty());
}

/// Records the prompts a job's approval target queues and the codes withdrawn.
#[derive(Debug, Default)]
struct FakePromptOutbox {
    sent: Mutex<Vec<(String, String, std::time::Duration)>>,
    withdrawn: Mutex<Vec<String>>,
    queued: Notify,
}

#[async_trait]
impl crate::approvals::PromptOutbox for FakePromptOutbox {
    async fn send(
        &self,
        code: &str,
        markdown: &str,
        timeout: std::time::Duration,
    ) -> std::result::Result<(), String> {
        self.sent
            .lock()
            .unwrap()
            .push((code.to_owned(), markdown.to_owned(), timeout));
        self.queued.notify_one();
        Ok(())
    }

    fn withdraw(&self, code: &str) {
        self.withdrawn.lock().unwrap().push(code.to_owned());
    }
}

#[tokio::test]
async fn a_job_run_asks_only_its_approval_target_through_the_outbox() {
    use crate::sessions::Approvals as _;

    let mut h = Harness::with_agent(
        "models = [\"anthropic/test-model\"]\ntools = [\"github__*\"]\n\
         ask = [\"github__list_issues\"]\n",
    )
    .await;
    let board = Arc::new(ApprovalBoard::default());
    let agent_loop = Arc::new(AgentLoop::new(
        h.registry.clone(),
        h.store.clone(),
        Arc::new(FixedClock),
        Arc::new(PolicyDispatcher::new(h.registry.clone())),
        board.clone(),
    ));

    // Without a target, even in a DM: denied as unattended, nothing asked.
    h.provider.push(one_call(1));
    h.provider.push(text("I wasn't allowed."));
    let (turn, _) = job_turn(&h, "Check the issues.", None, None);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    assert_eq!(
        h.transcript().await[2],
        result("t1", crate::approvals::UNATTENDED_RESULT, true)
    );
    assert!(h.sent().is_empty());

    // With `ask_to`: the prompt goes through the outbox, the answer comes from the DM.
    let outbox = Arc::new(FakePromptOutbox::default());
    let key = h.session.key().unwrap();
    let target = crate::approvals::ApprovalTarget {
        key: key.clone(),
        user: owner(),
        reply: Reply::for_session(&h.session, h.registry.clone()).unwrap(),
        outbox: Some(outbox.clone()),
    };
    h.provider.push(one_call(2));
    h.provider.push(text("Listed."));
    let (turn, _) = job_turn(&h, "Check the issues.", None, Some(target));
    let running = tokio::spawn({
        let agent_loop = agent_loop.clone();
        async move { agent_loop.run_turn(turn).await }
    });
    outbox.queued.notified().await;
    let (code, markdown, timeout) = outbox.sent.lock().unwrap()[0].clone();
    assert!(markdown.contains("github__list_issues"), "{markdown}");
    assert_eq!(timeout, crate::config::DEFAULT_APPROVAL_TIMEOUT);
    assert!(h.sent().is_empty(), "the prompt went into the conversation");
    let to = crate::router::Routed {
        session: key,
        user: owner(),
    };
    assert!(board.is_pending(&to, Some(&code)));
    board
        .answer(
            to,
            crate::commands::ApprovalReply {
                answer: crate::commands::Answer::Yes,
                code: Some(code.clone()),
            },
        )
        .await;
    assert_eq!(running.await.unwrap(), TurnOutcome::Completed);
    assert_eq!(toolset_calls(&h), ["t2"]);
    assert_eq!(*outbox.withdrawn.lock().unwrap(), [code]);
}

/// A response asking for one `list_issues` call, `t<n>`.
fn one_call(n: usize) -> Script {
    response(
        vec![tool_use(&format!("t{n}"), "github__list_issues", json!({}))],
        StopReason::ToolUse,
    )
}

/// The ids of the calls the toolset got.
fn toolset_calls(h: &Harness) -> Vec<String> {
    h.toolset
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(call, _)| call.id.clone())
        .collect()
}

#[tokio::test]
async fn an_interactive_turn_has_no_iteration_limit_by_default() {
    let mut h = Harness::new().await;
    for n in 1..=30 {
        h.provider.push(one_call(n));
    }
    h.provider.push(text("Done."));
    let (turn, _) = h.turn(&["Keep going"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    assert_eq!(toolset_calls(&h).len(), 30);
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 31);
    assert!(requests.iter().all(|r| !r.tools.is_empty()));
}

#[tokio::test]
async fn at_max_iterations_the_calls_get_iteration_limit_and_a_last_request_has_no_tools() {
    let mut h = Harness::with_agent(
        "models = [\"anthropic/test-model\"]\ntools = [\"github__*\"]\nmax_iterations = 2\n",
    )
    .await;
    h.provider.push(one_call(1));
    h.provider.push(response(
        vec![
            Block::text("Two more."),
            tool_use("t2", "github__list_issues", json!({})),
            tool_use("t3", "github__list_issues", json!({})),
        ],
        StopReason::ToolUse,
    ));
    h.provider
        .push(text("I listed the issues once and stopped there."));
    let (turn, _) = h.turn(&["List them all"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    // The first request's call ran; the second's didn't.
    assert_eq!(toolset_calls(&h), ["t1"]);
    let transcript = h.transcript().await;
    assert_eq!(
        transcript[4..],
        [
            result("t2", "iteration limit", true),
            result("t3", "iteration limit", true),
            assistant(vec![Block::text(
                "I listed the issues once and stopped there."
            )]),
        ]
    );
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[1].tools.is_empty());
    assert!(requests[2].tools.is_empty());
    // The last request still carries the whole history, unanswered calls included.
    assert_eq!(requests[2].messages.len(), 6);
    assert_eq!(
        h.sent(),
        ["Two more.", "I listed the issues once and stopped there."]
    );
    assert_eq!(h.usage_requests().await, 3);
}

#[tokio::test]
async fn calls_asked_for_in_the_last_request_do_not_run_and_the_turn_ends() {
    let mut h = Harness::with_agent(
        "models = [\"anthropic/test-model\"]\ntools = [\"github__*\"]\nmax_iterations = 1\n",
    )
    .await;
    h.provider.push(one_call(1));
    h.provider.push(response(
        vec![
            Block::text("Stopped before listing."),
            tool_use("t2", "github__list_issues", json!({})),
        ],
        StopReason::ToolUse,
    ));
    let (turn, _) = h.turn(&["List"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    assert!(toolset_calls(&h).is_empty());
    let transcript = h.transcript().await;
    assert_eq!(transcript[2], result("t1", "iteration limit", true));
    assert_eq!(transcript[4], result("t2", "iteration limit", true));
    assert_eq!(h.provider.requests().len(), 2);
    assert_eq!(h.sent(), ["Stopped before listing."]);
}

#[tokio::test]
async fn a_job_turn_stops_at_25_iterations_by_default() {
    let mut h = Harness::new().await;
    h.session = h
        .store
        .create_job_session(AgentId::new(AGENT).unwrap(), Some(owner()), now())
        .await
        .unwrap();
    for n in 1..=25 {
        h.provider.push(one_call(n));
    }
    h.provider.push(text("Stopped at the limit."));
    let (turn, _) = h.turn(&["Check everything"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    let ran = toolset_calls(&h);
    assert_eq!(ran.len(), 24);
    assert_eq!(ran.last().map(String::as_str), Some("t24"));
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 26);
    assert!(requests[..25].iter().all(|r| !r.tools.is_empty()));
    assert!(requests[25].tools.is_empty());
    let transcript = h.transcript().await;
    assert_eq!(
        transcript[transcript.len() - 2..],
        [
            result("t25", "iteration limit", true),
            assistant(vec![Block::text("Stopped at the limit.")]),
        ]
    );
}

#[tokio::test]
async fn a_blocks_channel_gets_a_reply_in_paragraphs_in_order() {
    let mut h = Harness::new().await;
    h.provider.push(response(
        vec![
            Block::text("Checking.\n\nOne moment."),
            tool_use("t1", "github__list_issues", json!({})),
        ],
        StopReason::ToolUse,
    ));
    h.provider.push(text(
        "Found:\n\n```\na\n\nb\n```\n\n- one\n\n- two\n\nThat's all.",
    ));
    let (turn, _) = h.turn(&["Issues?"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);
    assert_eq!(
        h.sent(),
        [
            "Checking.",
            "One moment.",
            "Found:",
            "```\na\n\nb\n```",
            "- one\n\n- two",
            "That's all."
        ]
    );
}

fn media_ref(byte: u8, media_type: &str, name: Option<&str>) -> MediaRef {
    MediaRef {
        id: giles_types::MediaId::from_digest([byte; 32]),
        media_type: media_type.to_owned(),
        size: 3,
        name: name.map(str::to_owned),
    }
}

/// The model the harness's agent uses, accepting `media_types`.
fn test_model(media_types: &[&str]) -> ModelInfo {
    ModelInfo {
        id: "test-model".to_owned(),
        display_name: None,
        context_window: None,
        max_output_tokens: None,
        media_types: media_types.iter().map(|&t| t.to_owned()).collect(),
        reasoning: false,
    }
}

/// `media` as stored: its reference, then its block.
fn attached(media: &MediaRef) -> Vec<Block> {
    crate::media::attachment(media.clone()).to_vec()
}

fn reference(media: &MediaRef) -> Block {
    Block::text(crate::media::reference(media))
}

#[tokio::test]
async fn attachments_reach_the_model_as_content_only_if_it_accepts_their_type() {
    let mut h = Harness::new().await;
    *h.provider.models.lock().unwrap() = vec![
        // Not the agent's model: ignored.
        ModelInfo {
            id: "other-model".to_owned(),
            ..test_model(&["audio/ogg"])
        },
        test_model(&["image/png", "application/pdf"]),
    ];
    let png = media_ref(1, "image/png", Some("cat.png"));
    let jpeg = media_ref(2, "image/jpeg", None);
    let ogg = media_ref(3, "audio/ogg", Some("voice.ogg"));
    let pdf = media_ref(4, "application/pdf", Some("q3.pdf"));
    let csv = media_ref(5, "text/csv", Some("q3.csv"));
    h.provider.push(text("Lovely cat."));
    let (mut turn, _) = h.turn(&["Look"]);
    turn.messages[0].message.attachments = vec![
        png.clone(),
        jpeg.clone(),
        ogg.clone(),
        pdf.clone(),
        csv.clone(),
    ];
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    // Stored: every attachment as its reference and its block.
    let mut stored = vec![Block::text(format!("{PREFIX} Look"))];
    for media in [&png, &jpeg, &ogg, &pdf, &csv] {
        stored.extend(attached(media));
    }
    assert_eq!(h.transcript().await[0], Some((Role::User, stored)));

    // Sent: the accepted ones as content, the others as their references alone.
    let mut sent = vec![Block::text(format!("{PREFIX} Look"))];
    sent.extend(attached(&png));
    sent.push(reference(&jpeg));
    sent.push(reference(&ogg));
    sent.extend(attached(&pdf));
    sent.push(reference(&csv));
    assert_eq!(h.provider.requests()[0].messages[0].content, sent);
    assert_eq!(h.provider.listed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn media_is_filtered_the_same_after_a_restart_and_models_are_listed_once() {
    let mut h = Harness::new().await;
    *h.provider.models.lock().unwrap() = vec![test_model(&["image/png"])];
    let png = media_ref(1, "image/png", None);
    let wav = media_ref(2, "audio/wav", None);
    h.provider.push(text("Noted."));
    h.provider.push(text("Still here."));
    h.provider.push(text("Yes."));
    let agent_loop = h.agent_loop();
    let (mut turn, _) = h.turn(&["Here"]);
    turn.messages[0].message.attachments = vec![png.clone(), wav.clone()];
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    let (turn, _) = h.turn(&["Again?"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    assert_eq!(h.provider.listed.load(Ordering::SeqCst), 1);

    // A new agent loop (a restart) reads the stored media back and filters it the same way.
    let (turn, _) = h.turn(&["Sure?"]);
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);
    assert_eq!(h.provider.listed.load(Ordering::SeqCst), 2);
    let requests = h.provider.requests();
    let mut first = vec![Block::text(format!("{PREFIX} Here"))];
    first.extend(attached(&png));
    first.push(reference(&wav));
    for request in &requests {
        assert_eq!(request.messages[0].content, first);
    }
    assert_eq!(requests[2].messages.len(), 5);
}

#[tokio::test]
async fn without_the_models_info_every_attachment_is_sent_for_the_provider_to_judge() {
    let mut h = Harness::new().await;
    let ogg = media_ref(3, "audio/ogg", None);
    h.provider.push(text("Heard."));
    let (mut turn, _) = h.turn(&[""]);
    turn.messages[0].message.attachments = vec![ogg.clone()];
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);
    let mut sent = vec![Block::text(PREFIX)];
    sent.extend(attached(&ogg));
    assert_eq!(h.provider.requests()[0].messages[0].content, sent);
}

#[tokio::test]
async fn a_failed_model_listing_waits_to_retry_and_media_goes_to_the_provider_to_judge() {
    let mut h = Harness::new().await;
    h.provider.listing_fails.store(true, Ordering::SeqCst);
    let agent_loop = h.agent_loop();
    // Each turn looks the model up (its context window decides compaction); this one fails.
    h.provider.push(text("Hello."));
    let (turn, _) = h.turn(&["Hi"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    assert_eq!(h.provider.listed.load(Ordering::SeqCst), 1);

    // Within the retry window the next turn doesn't list again, and every attachment is sent
    // for the provider to judge.
    let ogg = media_ref(3, "audio/ogg", None);
    h.provider.push(text("Heard."));
    let (mut turn, _) = h.turn(&["Listen"]);
    turn.messages[0].message.attachments = vec![ogg.clone()];
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    let mut sent = vec![Block::text(format!("{PREFIX} Listen"))];
    sent.extend(attached(&ogg));
    assert_eq!(h.provider.requests()[1].messages[2].content, sent);
    assert_eq!(h.provider.listed.load(Ordering::SeqCst), 1);

    // Nor does the one after.
    h.provider.push(text("Still heard."));
    let (turn, _) = h.turn(&["Again"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    assert_eq!(h.provider.listed.load(Ordering::SeqCst), 1);
    assert_eq!(h.provider.requests()[2].messages[2].content, sent);
}

#[tokio::test]
async fn an_unknown_media_input_is_refused_before_asking_for_approval() {
    let mut h = Harness::with_agent(
        "models = [\"anthropic/test-model\"]\ntools = [\"github__*\"]\n\
         ask = [\"github__list_issues\"]\n",
    )
    .await;
    let unknown = media_ref(9, "image/png", None);
    h.provider.push(response(
        vec![tool_use(
            "t1",
            "github__list_issues",
            json!({"file": {"$media": unknown.id.as_str()}}),
        )],
        StopReason::ToolUse,
    ));
    h.provider.push(text("That file is gone."));
    let (turn, _) = h.turn(&["Chart"]);
    // An approval prompt would wait for an answer that never comes.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        h.agent_loop().run_turn(turn),
    )
    .await
    .expect("no approval was asked");
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(
        h.transcript().await[2],
        result("t1", &format!("unknown media `{}`", unknown.id), true)
    );
    assert!(toolset_calls(&h).is_empty());
    assert_eq!(h.sent(), ["That file is gone."]);
}

#[tokio::test]
async fn tools_take_session_media_by_id_and_their_media_results_get_references() {
    let mut h = Harness::new().await;
    *h.provider.models.lock().unwrap() = vec![test_model(&[])];
    let png = media_ref(1, "image/png", Some("chart.png"));
    let unknown = media_ref(9, "image/png", None);
    h.provider.push(response(
        vec![
            tool_use(
                "t1",
                "github__list_issues",
                json!({"file": {"$media": png.id.as_str()}}),
            ),
            tool_use(
                "t2",
                "github__list_issues",
                json!({"file": {"$media": unknown.id.as_str()}}),
            ),
        ],
        StopReason::ToolUse,
    ));
    h.provider.push(text("Done."));
    let (mut turn, _) = h.turn(&["Chart"]);
    turn.messages[0].message.attachments = vec![png.clone()];
    assert_eq!(h.agent_loop().run_turn(turn).await, TurnOutcome::Completed);

    // The toolset got the media the input names; the unknown id never reached it.
    let calls = h.toolset.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.media, std::slice::from_ref(&png));

    let ran = format!(
        "list_issues ran with {}",
        json!({"file": {"$media": png.id.as_str()}})
    );
    let transcript = h.transcript().await;
    assert_eq!(
        transcript[2],
        Some((
            Role::User,
            vec![Block::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: vec![
                    Block::text(ran.clone()),
                    reference(&png),
                    Block::Image { media: png.clone() }
                ],
                is_error: false,
            }]
        ))
    );
    assert_eq!(
        transcript[3],
        result("t2", &format!("unknown media `{}`", unknown.id), true)
    );

    // A model that takes no media gets the result's reference alone.
    let second = &h.provider.requests()[1];
    assert_eq!(
        second.messages[2].content,
        [Block::ToolResult {
            tool_use_id: "t1".to_owned(),
            content: vec![Block::text(ran), reference(&png)],
            is_error: false,
        }]
    );
}

/// An agent whose summaries are written by `anthropic/summarizer`.
const COMPACTING_AGENT: &str = "models = [\"anthropic/test-model\"]\n\
     compaction_model = \"anthropic/summarizer\"\n\
     tools = [\"github__*\", \"memory\"]\n";

/// A harness whose agent's model has a 2,000-token context window: requests estimated above
/// 1,600 tokens are compacted, keeping at most 500 tokens of recent messages.
async fn compacting_harness() -> Harness {
    let h = Harness::with_agent(COMPACTING_AGENT).await;
    *h.provider.models.lock().unwrap() = vec![ModelInfo {
        context_window: Some(2_000),
        ..test_model(&[])
    }];
    h
}

/// A response of `blocks` whose request had `input` input tokens (the anchor of the next
/// estimate: `input` plus 5 output tokens).
fn sized(blocks: Vec<Block>, stop: StopReason, input: u64) -> Script {
    let Script::Events(mut events) = response(blocks, stop) else {
        unreachable!()
    };
    let at = events.len() - 2;
    events[at] = Ok(CompletionEvent::Usage(Usage {
        input_tokens: input,
        output_tokens: 5,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }));
    Script::Events(events)
}

fn request_text(request: &CompletionRequest) -> String {
    match &request.messages[..] {
        [Message { content, .. }] => match &content[..] {
            [Block::Text { text }] => text.clone(),
            other => panic!("not one text block: {other:?}"),
        },
        other => panic!("not one message: {other:?}"),
    }
}

fn summary_message(summary: &str) -> Vec<Block> {
    vec![Block::text(format!(
        "{}\n\n{summary}",
        history::SUMMARY_INTRO
    ))]
}

fn system_text(request: &CompletionRequest) -> &str {
    match &request.system[..] {
        [Block::Text { text }] => text,
        _ => "",
    }
}

#[tokio::test]
async fn a_session_near_the_context_window_continues_from_a_summary() {
    let mut h = compacting_harness().await;
    let agent_loop = h.ticking_agent_loop();
    let first = format!("First {}", "x".repeat(3_000));

    // About 1,300 tokens: no compaction yet. The response reports a 1,700-token prompt.
    h.provider
        .push(sized(vec![Block::text("One.")], StopReason::EndTurn, 1_700));
    let (turn, _) = h.turn(&[&first]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    assert_eq!(h.provider.requests().len(), 1);

    // The next request would be over 1,600 tokens: the first message is summarized (the rest
    // fits in the 500 kept), and `system` gets the memory as it is now.
    std::fs::write(
        h.dir.path().join(format!("agents/{AGENT}/MEMORY.md")),
        "Tea at five.",
    )
    .unwrap();
    h.provider.push(text("The user sent a long first message."));
    h.provider
        .push(sized(vec![Block::text("Two.")], StopReason::EndTurn, 1_700));
    let (turn, _) = h.turn(&["Second"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);

    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3);
    let summarize = &requests[1];
    assert_eq!(summarize.model, "summarizer");
    assert_eq!(summarize.system, [Block::text(compaction::SUMMARY_SYSTEM)]);
    assert!(summarize.tools.is_empty());
    assert_eq!(
        request_text(summarize),
        format!("<conversation>\nUser:\n{PREFIX} {first}\n</conversation>")
    );

    let summary = h.store.load_history(h.session.id).await.unwrap().summary;
    let summary = summary.unwrap();
    assert_eq!((summary.from_seq, summary.through_seq), (1, 1));
    assert_eq!(summary.content, "The user sent a long first message.");

    let next = &requests[2];
    assert_eq!(next.model, "test-model");
    let contents: Vec<_> = next.messages.iter().map(|m| m.content.clone()).collect();
    assert_eq!(
        contents,
        [
            summary_message("The user sent a long first message."),
            vec![Block::text("One.")],
            vec![Block::text(format!("{PREFIX} Second"))],
        ]
    );
    assert!(system_text(&requests[0]).contains("Tea at four."));
    assert!(system_text(next).contains("Tea at five."));
    assert_eq!(next.tools, requests[0].tools);
    // Later requests keep the rebuilt prompt.
    let config = h.registry.config().clone();
    let agent = &config.agents[&AgentId::new(AGENT).unwrap()];
    let cached = agent_loop.session_prompt(&h.session, agent).await.unwrap();
    assert_eq!(cached.system, next.system);

    // The transcript keeps every message; the summary is never sent to the channel.
    assert_eq!(
        h.store
            .recent_messages(h.session.id, 100)
            .await
            .unwrap()
            .len(),
        4
    );
    assert_eq!(h.sent(), ["One.", "Two."]);
    // One usage row per request, the summary's under the compaction model.
    let usage = h.store.usage_by_model(None, None).await.unwrap();
    let rows: Vec<_> = usage
        .iter()
        .map(|u| (u.agent.as_str(), u.model.as_str(), u.requests))
        .collect();
    assert_eq!(rows, [(AGENT, "summarizer", 1), (AGENT, "test-model", 2)]);

    // Near the limit again: the new summary rolls the old one up with the next messages.
    h.provider.push(text("Rolled up."));
    h.provider.push(text("Three."));
    let (turn, _) = h.turn(&["Third"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[3].model, "summarizer");
    assert_eq!(
        request_text(&requests[3]),
        "<previous-summary>\nThe user sent a long first message.\n</previous-summary>\n\n\
         <conversation>\nAssistant:\nOne.\n</conversation>"
    );
    let summary = h.store.load_history(h.session.id).await.unwrap().summary;
    let summary = summary.unwrap();
    assert_eq!((summary.from_seq, summary.through_seq), (1, 2));
    assert_eq!(
        requests[4].messages[0].content,
        summary_message("Rolled up.")
    );
    assert_eq!(requests[4].messages.len(), 4);
}

#[tokio::test]
async fn compaction_after_a_tool_batch_keeps_the_call_with_its_result() {
    let mut h = compacting_harness().await;
    let agent_loop = h.ticking_agent_loop();
    let first = format!("First {}", "x".repeat(3_000));
    let input = json!({"pad": "y".repeat(1_200)});
    h.provider.push(sized(
        vec![tool_use("t1", "github__list_issues", input.clone())],
        StopReason::ToolUse,
        1_700,
    ));
    h.provider.push(text("Summary."));
    h.provider.push(text("Done."));
    let (turn, _) = h.turn(&[&first]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);

    // With the result the next request is over the threshold. Keeping only the result would
    // fit better, but the result stays with its call.
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].model, "summarizer");
    let summary = h.store.load_history(h.session.id).await.unwrap().summary;
    assert_eq!(summary.unwrap().through_seq, 1);
    let contents: Vec<_> = requests[2]
        .messages
        .iter()
        .map(|m| m.content.clone())
        .collect();
    assert_eq!(
        contents,
        [
            summary_message("Summary."),
            vec![tool_use("t1", "github__list_issues", input.clone())],
            result("t1", &format!("list_issues ran with {input}"), false)
                .unwrap()
                .1,
        ]
    );
}

#[tokio::test]
async fn a_failed_summary_is_logged_and_the_turn_goes_on_without_compacting() {
    let mut h = compacting_harness().await;
    let agent_loop = h.ticking_agent_loop();
    let first = format!("First {}", "x".repeat(3_000));
    h.provider
        .push(sized(vec![Block::text("One.")], StopReason::EndTurn, 1_700));
    let (turn, _) = h.turn(&[&first]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);

    // The summary request fails: the turn's requests go out uncompacted, and the turn doesn't
    // try again after its tool batch, though it's still over the threshold.
    h.provider
        .push(Script::Refuse(Error::unavailable("overloaded")));
    h.provider.push(sized(
        vec![tool_use("t1", "github__list_issues", json!({}))],
        StopReason::ToolUse,
        1_700,
    ));
    h.provider
        .push(sized(vec![Block::text("Two.")], StopReason::EndTurn, 1_700));
    let (turn, _) = h.turn(&["Second"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[1].model, "summarizer");
    assert_eq!(requests[2].messages.len(), 3);
    assert_eq!(requests[3].model, "test-model");
    assert_eq!(requests[3].messages.len(), 5);
    let history = h.store.load_history(h.session.id).await.unwrap();
    assert!(history.summary.is_none());
    // The refused request generated nothing and records no usage.
    assert_eq!(h.usage_requests().await, 3);
    assert_eq!(h.sent(), ["One.", "Two."]);

    // The next turn tries again.
    h.provider.push(text("Summary."));
    h.provider.push(text("Three."));
    let (turn, _) = h.turn(&["Third"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    let requests = h.provider.requests();
    assert_eq!(requests[4].model, "summarizer");
    assert_eq!(requests[5].messages[0].content, summary_message("Summary."));
}

#[tokio::test]
async fn without_a_reported_context_window_a_long_session_compacts_at_the_default() {
    let mut h = Harness::with_agent(COMPACTING_AGENT).await;
    let agent_loop = h.ticking_agent_loop();
    h.provider.push(sized(
        vec![Block::text("One.")],
        StopReason::EndTurn,
        compaction::DEFAULT_CONTEXT_WINDOW * 80 / 100,
    ));
    let (turn, _) = h.turn(&["First"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    h.provider.push(text("Summary."));
    h.provider.push(text("Two."));
    let (turn, _) = h.turn(&["Second"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    let requests = h.provider.requests();
    assert_eq!(requests[1].model, "summarizer");
    assert_eq!(requests[2].messages[0].content, summary_message("Summary."));
}

#[cfg(feature = "provider-openai")]
#[tokio::test]
async fn an_unlisted_model_uses_the_context_window_its_provider_config_sets() {
    // `ollama` is an OpenAI-compatible instance whose config gives `qwen3:8b` a 2,000-token
    // window; its model list doesn't have the model.
    let mut h = Harness::with_agent("models = [\"ollama/qwen3:8b\"]\n").await;
    h.registry
        .register_provider(&name("ollama"), h.provider.clone())
        .unwrap();
    let agent_loop = h.ticking_agent_loop();
    h.provider
        .push(sized(vec![Block::text("One.")], StopReason::EndTurn, 1_700));
    let (turn, _) = h.turn(&["First"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    h.provider.push(text("Summary."));
    h.provider.push(text("Two."));
    let (turn, _) = h.turn(&["Second"]);
    assert_eq!(agent_loop.run_turn(turn).await, TurnOutcome::Completed);
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3);
    // No `compaction_model`: the agent's model summarizes.
    assert_eq!(requests[1].model, "qwen3:8b");
    assert_eq!(
        requests[1].system,
        [Block::text(compaction::SUMMARY_SYSTEM)]
    );
    assert_eq!(requests[2].messages[0].content, summary_message("Summary."));
}
