//! Turns against a scripted fake provider, a fake toolset and channel, and a temporary store and
//! data root.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use giles_store::{Entry, InboxId, Session, SessionKey, SessionKind, Store};
use giles_types::{
    AgentId, Block, BlockKind, CacheRetention, CallContext, Channel, ChannelEvent, ChannelInfo,
    CompletionEvent, CompletionRequest, ConversationId, ConversationKind, ConversationRef, Error,
    EventId, InboundMessage, Message, MessageId, ModelInfo, NativeId, OutboundMessage, OutputMode,
    Provider, Result, Role, Skill, SkillSummary, StopReason, Timestamp, ToolCall, ToolCallEvent,
    ToolCallHandle, ToolSpec, Toolset, Usage, UserId,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use super::*;
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
        Ok(Vec::new())
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
            _ => stream::iter([
                Ok(ToolCallEvent::Progress {
                    progress: 1.0,
                    total: None,
                    message: None,
                }),
                Ok(ToolCallEvent::Result {
                    content: vec![Block::text(format!(
                        "{} ran with {}",
                        call.name, call.input
                    ))],
                    is_error: false,
                }),
            ])
            .boxed(),
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
    let rebuilt = agent_loop.rebuild_prompt(&h.session, agent).await.unwrap();
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
