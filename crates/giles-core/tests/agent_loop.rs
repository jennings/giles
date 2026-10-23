//! The agent loop end to end, wired the way `giles serve` wires it: `Runtime::start`, fakes
//! registered for the provider, channel, and toolset, `AgentLoop::from_runtime`, and
//! `Runtime::start_sessions`. Messages go in through the channel's event stream and replies come
//! out of its `send`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc as fmpsc;
use futures::stream::{self, BoxStream, StreamExt};
use giles_core::agent_loop::AgentLoop;
use giles_core::commands::NoPendingApprovals;
use giles_core::config::{Config, InstanceName, LoadOptions};
use giles_core::paths::DataRoot;
use giles_core::runtime::Runtime;
use giles_store::Entry;
use giles_types::{
    Block, BlockKind, CallContext, Channel, ChannelEvent, ChannelInfo, CompletionEvent,
    CompletionRequest, ConversationId, ConversationKind, ConversationRef, Error, EventId,
    InboundMessage, MessageId, ModelInfo, NativeId, OutboundMessage, OutputMode, Provider, Result,
    Role, Skill, SkillSummary, StopReason, ToolCall, ToolCallEvent, ToolCallHandle, ToolSpec,
    Toolset, Usage,
};
use serde_json::json;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);

/// Each request takes the next script: `Some(events)` streams them, `None` hangs after
/// `BlockStart` until the request is dropped.
#[derive(Default)]
struct FakeProvider {
    scripts: Mutex<VecDeque<Option<Vec<CompletionEvent>>>>,
    requests: Mutex<Vec<CompletionRequest>>,
    requested: Notify,
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
        match self.scripts.lock().unwrap().pop_front() {
            Some(Some(events)) => Ok(stream::iter(events.into_iter().map(Ok)).boxed()),
            Some(None) => Ok(stream::iter([Ok(CompletionEvent::BlockStart {
                index: 0,
                kind: BlockKind::Text,
                tool: None,
            })])
            .chain(stream::pending())
            .boxed()),
            None => Err(Error::unavailable("unscripted")),
        }
    }
}

fn respond(blocks: Vec<Block>, stop: StopReason) -> Option<Vec<CompletionEvent>> {
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
    Some(events)
}

struct FakeToolset;

#[async_trait]
impl Toolset for FakeToolset {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(vec![ToolSpec {
            name: "weather".to_owned(),
            description: "Today's weather.".to_owned(),
            input_schema: json!({"type": "object"}),
        }])
    }

    async fn list_skills(&self) -> Result<Vec<SkillSummary>> {
        Ok(Vec::new())
    }

    async fn get_skill(&self, name: &str) -> Result<Skill> {
        Err(Error::NotFound(name.to_owned()))
    }

    async fn call(&self, call: ToolCall, _ctx: CallContext) -> Result<ToolCallHandle> {
        let (answers, _) = tokio::sync::mpsc::channel(1);
        let text = format!("{}: drizzle, 12 °C", call.name);
        Ok(ToolCallHandle {
            events: stream::iter([Ok(ToolCallEvent::Result {
                content: vec![Block::text(text)],
                is_error: false,
            })])
            .boxed(),
            answers,
        })
    }
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
        self.sent.send(msg.markdown).unwrap();
        Ok(MessageId::new("sent").unwrap())
    }
}

fn message(id: &str, text: &str) -> Result<ChannelEvent> {
    Ok(ChannelEvent::Message {
        id: EventId::new(id).unwrap(),
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
    })
}

async fn next_sent(sent: &mut mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(WAIT, sent.recv())
        .await
        .expect("a reply")
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_are_answered_through_the_serve_wiring() {
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
         [[routes]]\nagent = \"jeeves\"\n",
    );
    write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/test-model\"]\ntools = [\"met__*\"]\n",
    );
    write("agents/jeeves/SOUL.md", "You are Jeeves.");
    write("users/owner/USER.md", "Stephen.");
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

    let provider = Arc::new(FakeProvider::default());
    let name = |n: &str| InstanceName::new(n).unwrap();
    runtime
        .registry
        .register_provider(&name("anthropic"), provider.clone())
        .unwrap();
    runtime
        .registry
        .register_toolset(&name("met"), Arc::new(FakeToolset))
        .await
        .unwrap();
    let (events, events_rx) = fmpsc::unbounded();
    let (sent_tx, mut sent) = mpsc::unbounded_channel();
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
    let sessions = runtime
        .start_sessions(
            Arc::new(AgentLoop::from_runtime(&runtime)),
            Arc::new(NoPendingApprovals),
            shutdown.clone(),
        )
        .await
        .unwrap();

    // A turn with a tool round trip.
    {
        let mut scripts = provider.scripts.lock().unwrap();
        scripts.push_back(respond(
            vec![Block::ToolUse {
                id: "t1".to_owned(),
                name: "met__weather".to_owned(),
                input: json!({}),
            }],
            StopReason::ToolUse,
        ));
        scripts.push_back(respond(
            vec![Block::text("Drizzle, 12 °C. Take an umbrella.")],
            StopReason::EndTurn,
        ));
    }
    events.unbounded_send(message("e1", "Weather?")).unwrap();
    assert_eq!(
        next_sent(&mut sent).await,
        "Drizzle, 12 °C. Take an umbrella."
    );

    // `/stop` cancels a turn whose response is still streaming.
    provider.scripts.lock().unwrap().push_back(None);
    events
        .unbounded_send(message("e2", "Tell me a story"))
        .unwrap();
    // The third request is the hanging one.
    while provider.requests.lock().unwrap().len() < 3 {
        tokio::time::timeout(WAIT, provider.requested.notified())
            .await
            .unwrap();
    }
    events.unbounded_send(message("e3", "/stop")).unwrap();

    // The next message starts a new turn in the same session.
    provider.scripts.lock().unwrap().push_back(respond(
        vec![Block::text("Of course.")],
        StopReason::EndTurn,
    ));
    events.unbounded_send(message("e4", "Never mind")).unwrap();
    assert_eq!(next_sent(&mut sent).await, "Of course.");

    shutdown.cancel();
    sessions.closed().await;

    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    // The same prefix for every request of the session.
    for request in &requests[1..] {
        assert_eq!(request.system, requests[0].system);
        assert_eq!(request.tools, requests[0].tools);
    }
    let Block::Text { text: system } = &requests[0].system[0] else {
        panic!("system is not text");
    };
    assert!(system.starts_with("You are Jeeves."));
    assert!(system.contains("Stephen."));
    let tools: Vec<_> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(tools, ["met__weather"]);

    // The last request saw the whole transcript, with London time prefixes (BST).
    let last = &requests[3];
    let texts: Vec<String> = last
        .messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|b| match b {
                    Block::Text { text } => text.clone(),
                    Block::ToolUse { name, .. } => format!("<use {name}>"),
                    Block::ToolResult { content, .. } => format!("<result {content:?}>"),
                    other => format!("<{:?}>", other.kind()),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    assert_eq!(
        texts,
        [
            "[Thu 2026-10-22 17:30 BST] Weather?".to_owned(),
            "<use met__weather>".to_owned(),
            format!("<result {:?}>", [Block::text("weather: drizzle, 12 °C")]),
            "Drizzle, 12 °C. Take an umbrella.".to_owned(),
            "[Thu 2026-10-22 17:30 BST] Tell me a story".to_owned(),
            "[Thu 2026-10-22 17:30 BST] Never mind".to_owned(),
        ]
    );
    assert_eq!(last.messages[4].role, Role::User);

    // One usage row per request, the cancelled one included.
    let usage = runtime.store.usage_by_agent(None).await.unwrap();
    assert_eq!(usage[0].requests, 4);

    // Nothing is left unanswered in the transcript.
    let session = runtime.store.open_sessions().await.unwrap().remove(0);
    let history = runtime.store.load_history(session.id).await.unwrap();
    assert!(
        history
            .messages
            .iter()
            .all(|m| matches!(m.entry, Entry::Message(_)))
    );
}
