//! Paragraph splitting, and `ChannelOutput` against a fake channel and live-output hook.

use std::sync::Mutex;

use futures::stream::{self, BoxStream, StreamExt};
use giles_store::{Session, SessionId, SessionKind};
use giles_types::{
    AgentId, ChannelEvent, ChannelInfo, EventId, MessageMeta, OutboundMessage, Role, Timestamp,
};
use serde_json::json;

use super::*;
use crate::config::BusyMode;
use crate::sessions::{ChannelLookup, Steering};

fn split(text: &str) -> Vec<String> {
    let mut paragraphs = Paragraphs::default();
    let mut out = paragraphs.push(text);
    out.extend(paragraphs.finish());
    out
}

fn split_by_char(text: &str) -> Vec<String> {
    let mut paragraphs = Paragraphs::default();
    let mut out = Vec::new();
    let mut buf = [0; 4];
    for c in text.chars() {
        out.extend(paragraphs.push(c.encode_utf8(&mut buf)));
    }
    out.extend(paragraphs.finish());
    out
}

#[test]
fn paragraphs_split_outside_fences_lists_and_indentation() {
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "paragraphs",
            "One.\n\nTwo.\n\n\nThree.",
            &["One.", "Two.", "Three."],
        ),
        (
            "surrounding whitespace",
            "\n\n  \nOne.  \n \t\n\nTwo.\n\n",
            &["One.", "Two."],
        ),
        (
            "soft line breaks",
            "One\ntwo\n\nThree",
            &["One\ntwo", "Three"],
        ),
        ("nothing", " \n\n\t", &[]),
        (
            "fence keeps its blank lines",
            "Look:\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\nDone.",
            &["Look:", "```rust\nfn a() {}\n\nfn b() {}\n```", "Done."],
        ),
        (
            "tilde fence holds a backtick fence",
            "~~~\n```\n\n```\n~~~\n\nAfter",
            &["~~~\n```\n\n```\n~~~", "After"],
        ),
        (
            "longer fence holds a shorter one",
            "````md\n```\n\nx\n```\n````\n\nAfter",
            &["````md\n```\n\nx\n```\n````", "After"],
        ),
        (
            "a closing fence has nothing after it",
            "```\n``` not closed\n\nstill code\n```\n\nAfter",
            &["```\n``` not closed\n\nstill code\n```", "After"],
        ),
        (
            "backticks in the info string are inline code",
            "```a``` b\n\nc",
            &["```a``` b", "c"],
        ),
        ("two backticks aren't a fence", "``\n\nc", &["``", "c"]),
        (
            "a closing fence indented four columns more doesn't close",
            "```\na\n    ```\n\nb\n```\n\nAfter",
            &["```\na\n    ```\n\nb\n```", "After"],
        ),
        (
            "tilde info string may have backticks",
            "~~~ `x`\na\n\nb\n~~~\n\nc",
            &["~~~ `x`\na\n\nb\n~~~", "c"],
        ),
        (
            "an unclosed fence runs to the end",
            "Code:\n\n```\na\n\nb",
            &["Code:", "```\na\n\nb"],
        ),
        (
            "fence in a list item",
            "- a\n  ```\n  x\n\n  y\n  ```\n\nEnd",
            &["- a\n  ```\n  x\n\n  y\n  ```", "End"],
        ),
        (
            "indented code",
            "Code:\n\n    a\n\n    b\n\nAfter",
            &["Code:\n\n    a\n\n    b", "After"],
        ),
        (
            "loose list",
            "- a\n\n- b\n\n* c\n\nAfter",
            &["- a\n\n- b\n\n* c", "After"],
        ),
        (
            "ordered list with a continuation paragraph",
            "1. a\n\n   more\n\n2) b\n\nAfter",
            &["1. a\n\n   more\n\n2) b", "After"],
        ),
        (
            "a paragraph between lists",
            "- a\n\nText\n\n- b",
            &["- a", "Text", "- b"],
        ),
        (
            "not list markers",
            "- a\n\n-b\n\n1. c\n\n1.5 d\n\n1. e\n\n1234567890. f",
            &["- a", "-b", "1. c", "1.5 d", "1. e", "1234567890. f"],
        ),
        ("empty list item", "- a\n\n-\n\nb", &["- a\n\n-", "b"]),
        (
            "a heading stays with what follows",
            "# Title\n\nBody.\n\n## Next\n\nMore.",
            &["# Title\n\nBody.", "## Next\n\nMore."],
        ),
        ("not a heading", "#tag\n\nText", &["#tag", "Text"]),
        ("crlf", "One.\r\n\r\nTwo.\r\n", &["One.", "Two."]),
        (
            "crlf fence",
            "```\r\na\r\n\r\nb\r\n```\r\n\r\nAfter\r\n",
            &["```\r\na\r\n\r\nb\r\n```", "After"],
        ),
        (
            "crlf list",
            "- a\r\n\r\n- b\r\n\r\nAfter",
            &["- a\r\n\r\n- b", "After"],
        ),
        ("non-ascii", "Ünïcödé ✓\n\n— done", &["Ünïcödé ✓", "— done"]),
    ];
    for (name, text, expected) in cases {
        assert_eq!(split(text), *expected, "{name}: whole");
        assert_eq!(split_by_char(text), *expected, "{name}: by char");
    }
}

#[test]
fn a_chunk_goes_out_as_soon_as_the_next_block_starts() {
    let mut paragraphs = Paragraphs::default();
    assert!(paragraphs.push("One.\n\n").is_empty());
    assert_eq!(paragraphs.push("T"), ["One."]);

    // After a list item, the next line's marker decides.
    let mut paragraphs = Paragraphs::default();
    assert!(paragraphs.push("- a\n\n").is_empty());
    assert!(paragraphs.push("-").is_empty());
    assert!(paragraphs.push(" b\n\n1").is_empty());
    assert_eq!(paragraphs.push("x"), ["- a\n\n- b"]);

    // An indented start may still be a blank line, or indented content.
    let mut paragraphs = Paragraphs::default();
    assert!(paragraphs.push("One.\n\n  ").is_empty());
    assert_eq!(paragraphs.push("\nTwo"), ["One."]);
    let mut paragraphs = Paragraphs::default();
    assert!(paragraphs.push("One.\n\n  ").is_empty());
    assert!(paragraphs.push("x\n").is_empty());
    assert_eq!(paragraphs.push("\nY"), ["One.\n\n  x"]);
}

// --- ChannelOutput ---

/// What reached the fake channel, in order.
#[derive(Default)]
struct Log(Mutex<Vec<String>>);

impl Log {
    fn add(&self, entry: impl Into<String>) {
        self.0.lock().unwrap().push(entry.into());
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

struct FakeChannel {
    info: ChannelInfo,
    log: Arc<Log>,
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
        self.log.add(format!("send {}", msg.markdown));
        Ok(MessageId::new("sent").unwrap())
    }
}

struct Lookup(Arc<FakeChannel>);

impl ChannelLookup for Lookup {
    fn channel(&self, instance: &str) -> Option<Arc<dyn Channel>> {
        (instance == "fake").then(|| self.0.clone() as Arc<dyn Channel>)
    }
}

/// A live-output hook; `start` fails if `refuse`.
struct FakeLive {
    log: Arc<Log>,
    refuse: bool,
}

impl DeltaSink for FakeLive {
    fn start(&self, conversation: &ConversationId) -> Result<Box<dyn DeltaWriter>> {
        if self.refuse {
            return Err(Error::unavailable("no live output today"));
        }
        self.log.add(format!("start {conversation}"));
        Ok(Box::new(FakeWriter {
            log: self.log.clone(),
            finished: false,
        }))
    }
}

struct FakeWriter {
    log: Arc<Log>,
    finished: bool,
}

#[async_trait]
impl DeltaWriter for FakeWriter {
    fn push(&mut self, text: &str) {
        self.log.add(format!("push {text}"));
    }

    async fn finish(mut self: Box<Self>) -> Result<MessageId> {
        self.finished = true;
        self.log.add("finish");
        Ok(MessageId::new("live").unwrap())
    }
}

impl Drop for FakeWriter {
    fn drop(&mut self) {
        if !self.finished {
            self.log.add("cut off");
        }
    }
}

struct Fixture {
    log: Arc<Log>,
    reply: Reply,
    cancel: CancellationToken,
}

impl Fixture {
    fn new(output: OutputMode) -> Self {
        let log = Arc::new(Log::default());
        let channel = Arc::new(FakeChannel {
            info: ChannelInfo {
                identity_namespace: "fake".to_owned(),
                output,
                can_initiate: false,
            },
            log: log.clone(),
        });
        let reply = Reply::for_session(&session(), Arc::new(Lookup(channel))).unwrap();
        Self {
            log,
            reply,
            cancel: CancellationToken::new(),
        }
    }

    fn blocks(&self) -> ChannelOutput {
        ChannelOutput::new(self.reply.clone(), self.cancel.clone(), None)
    }

    fn live(&self, refuse: bool) -> ChannelOutput {
        let live = FakeLive {
            log: self.log.clone(),
            refuse,
        };
        ChannelOutput::new(
            self.reply.clone(),
            self.cancel.clone(),
            Some(Arc::new(live)),
        )
    }
}

fn session() -> Session {
    Session {
        id: SessionId::new(1),
        agent: AgentId::new("jeeves").unwrap(),
        kind: SessionKind::Direct,
        channel: Some("fake".to_owned()),
        conversation: Some(ConversationId::new("c1").unwrap()),
        user: None,
        started_at: Timestamp::UNIX_EPOCH,
        ended_at: None,
        last_message_at: None,
    }
}

/// A streamed text block: its deltas, then `BlockStop` with their concatenation.
fn text_block(index: u32, deltas: &[&str]) -> Vec<CompletionEvent> {
    let mut events = vec![CompletionEvent::BlockStart {
        index,
        kind: BlockKind::Text,
        tool: None,
    }];
    events.extend(deltas.iter().map(|text| CompletionEvent::TextDelta {
        index,
        text: (*text).to_owned(),
    }));
    events.push(CompletionEvent::BlockStop {
        index,
        block: Block::text(deltas.concat()),
    });
    events
}

fn reasoning_block(index: u32, text: &str) -> Vec<CompletionEvent> {
    vec![
        CompletionEvent::BlockStart {
            index,
            kind: BlockKind::Reasoning,
            tool: None,
        },
        CompletionEvent::ReasoningDelta {
            index,
            text: text.to_owned(),
        },
        CompletionEvent::BlockStop {
            index,
            block: Block::Reasoning {
                text: Some(text.to_owned()),
                opaque: None,
            },
        },
    ]
}

fn tool_block(index: u32, input: &str) -> Vec<CompletionEvent> {
    vec![
        CompletionEvent::BlockStart {
            index,
            kind: BlockKind::ToolUse,
            tool: Some(("t1".to_owned(), "github__list".to_owned())),
        },
        CompletionEvent::ToolInputDelta {
            index,
            partial_json: input.to_owned(),
        },
        CompletionEvent::BlockStop {
            index,
            block: Block::ToolUse {
                id: "t1".to_owned(),
                name: "github__list".to_owned(),
                input: json!({ "query": input }),
            },
        },
    ]
}

fn message() -> Message {
    // The sink doesn't read the stored message.
    Message {
        role: Role::Assistant,
        content: Vec::new(),
        meta: MessageMeta::new(Timestamp::UNIX_EPOCH),
    }
}

async fn feed(output: &mut dyn OutputSink, events: &[CompletionEvent]) {
    for event in events {
        output.event(event).await;
    }
}

/// A message with reasoning, text, a tool call, and whitespace-only text.
fn mixed_message() -> Vec<CompletionEvent> {
    let mut events = reasoning_block(0, "secret thoughts");
    events.extend(text_block(1, &["  Let me ", "check.\n"]));
    events.extend(tool_block(2, "hidden input"));
    events.extend(text_block(3, &[" \n"]));
    events.extend(text_block(4, &["Done."]));
    events.push(CompletionEvent::Stop(giles_types::StopReason::ToolUse));
    events
}

#[tokio::test]
async fn blocks_go_out_in_order_as_paragraphs_complete() {
    let f = Fixture::new(OutputMode::Blocks);
    let mut output = f.blocks();
    let start = CompletionEvent::BlockStart {
        index: 0,
        kind: BlockKind::Text,
        tool: None,
    };
    let delta = |text: &str| CompletionEvent::TextDelta {
        index: 0,
        text: text.to_owned(),
    };
    feed(&mut output, &[start, delta("One.\n"), delta("\nTw")]).await;
    assert_eq!(f.log.take(), ["send One."]);
    feed(
        &mut output,
        &[delta("o.\n\n```\na\n\nb\n```\n"), delta("\nThree.")],
    )
    .await;
    assert_eq!(f.log.take(), ["send Two.", "send ```\na\n\nb\n```"]);
    let text = "One.\n\nTwo.\n\n```\na\n\nb\n```\n\nThree.";
    feed(
        &mut output,
        &[CompletionEvent::BlockStop {
            index: 0,
            block: Block::text(text),
        }],
    )
    .await;
    assert_eq!(f.log.take(), ["send Three."]);
    output.message_end(&message()).await;
    assert!(f.log.take().is_empty());
}

#[tokio::test]
async fn only_text_reaches_a_blocks_channel() {
    let f = Fixture::new(OutputMode::Blocks);
    let mut output = f.blocks();
    feed(&mut output, &mixed_message()).await;
    output.message_end(&message()).await;
    assert_eq!(f.log.take(), ["send Let me check.", "send Done."]);
}

#[tokio::test]
async fn only_text_reaches_live_output() {
    let f = Fixture::new(OutputMode::Deltas);
    let mut output = f.live(false);
    feed(&mut output, &mixed_message()).await;
    // Live while streaming; nothing for the whitespace-only block.
    assert_eq!(
        f.log.take(),
        [
            "start c1",
            "push Let me ",
            "push check.\n",
            "start c1",
            "push Done."
        ]
    );
    output.message_end(&message()).await;
    assert_eq!(f.log.take(), ["finish", "finish"]);
}

#[tokio::test]
async fn the_block_stop_text_fills_in_missed_deltas() {
    let f = Fixture::new(OutputMode::Blocks);
    let mut output = f.blocks();
    let mut events = text_block(0, &["Hel"]);
    events[2] = CompletionEvent::BlockStop {
        index: 0,
        block: Block::text("Hello.\n\nWorld."),
    };
    feed(&mut output, &events).await;
    assert_eq!(f.log.take(), ["send Hello.", "send World."]);

    // Deltas that don't match the block are sent as streamed.
    let mut events = text_block(1, &["abc"]);
    events[2] = CompletionEvent::BlockStop {
        index: 1,
        block: Block::text("xyz"),
    };
    feed(&mut output, &events).await;
    assert_eq!(f.log.take(), ["send abc"]);

    // Live output too.
    let mut output = f.live(false);
    let mut events = text_block(0, &["Hel"]);
    events[2] = CompletionEvent::BlockStop {
        index: 0,
        block: Block::text("Hello."),
    };
    feed(&mut output, &events).await;
    output.message_end(&message()).await;
    assert_eq!(f.log.take(), ["start c1", "push Hel", "push lo.", "finish"]);
}

#[tokio::test]
async fn an_aborted_message_sends_nothing_more_to_a_blocks_channel() {
    let f = Fixture::new(OutputMode::Blocks);
    let mut output = f.blocks();
    // "Two." is complete but not sent yet: the next block hasn't started.
    let mut events = text_block(0, &["One.\n\nTwo.\n\n"]);
    events.pop();
    feed(&mut output, &events).await;
    output.message_aborted().await;
    assert_eq!(f.log.take(), ["send One."]);

    // The next response starts clean.
    feed(&mut output, &text_block(0, &["Next."])).await;
    output.message_end(&message()).await;
    assert_eq!(f.log.take(), ["send Next."]);
}

#[tokio::test]
async fn an_aborted_message_cuts_live_output_off() {
    let f = Fixture::new(OutputMode::Deltas);
    let mut output = f.live(false);
    let mut events = text_block(0, &["Done block."]);
    let mut streaming = text_block(1, &["Half"]);
    streaming.pop();
    events.extend(streaming);
    feed(&mut output, &events).await;
    output.message_aborted().await;
    let log = f.log.take();
    assert_eq!(
        log[..4],
        ["start c1", "push Done block.", "start c1", "push Half"]
    );
    // Both live messages are dropped unfinished.
    assert_eq!(log[4..], ["cut off", "cut off"]);

    // Dropping the sink mid-response is an abort too.
    let mut output = f.live(false);
    feed(&mut output, &text_block(0, &["Gone."])).await;
    drop(output);
    assert_eq!(f.log.take(), ["start c1", "push Gone.", "cut off"]);
}

#[tokio::test]
async fn a_cancelled_turn_sends_nothing() {
    let f = Fixture::new(OutputMode::Blocks);
    let mut output = f.blocks();
    f.cancel.cancel();
    feed(&mut output, &text_block(0, &["One.\n\nTwo."])).await;
    output.message_end(&message()).await;
    assert!(f.log.take().is_empty());
}

#[tokio::test]
async fn live_output_that_cannot_start_falls_back_to_blocks() {
    let f = Fixture::new(OutputMode::Deltas);
    let mut output = f.live(true);
    feed(&mut output, &text_block(0, &["One.\n\n", "Two."])).await;
    output.message_end(&message()).await;
    assert_eq!(f.log.take(), ["send One.", "send Two."]);
}

fn turn(reply: Option<Reply>) -> Turn {
    Turn {
        session: session(),
        requester: None,
        messages: Vec::new(),
        busy_mode: BusyMode::Queue,
        cancel: CancellationToken::new(),
        steering: Steering::detached(),
        reply,
    }
}

#[tokio::test]
async fn a_deltas_channel_without_a_live_hook_gets_blocks() {
    let f = Fixture::new(OutputMode::Deltas);
    let mut output = output_for(&turn(Some(f.reply.clone())));
    feed(output.as_mut(), &text_block(0, &["One.\n\n", "Two."])).await;
    output.message_end(&message()).await;
    assert_eq!(f.log.take(), ["send One.", "send Two."]);
}

#[tokio::test]
async fn a_turn_without_a_conversation_sends_nothing() {
    let mut output = output_for(&turn(None));
    feed(output.as_mut(), &text_block(0, &["Report."])).await;
    output.message_end(&message()).await;
}
