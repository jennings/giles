//! Output delivery (PLAN section 6.9): what of the model's responses reaches the channel, and
//! when. Design: `Docs/technical/agent-loop.md`, "Output".
//!
//! [`OutputSink`] is the seam: the agent loop hands it every streamed event of a response and
//! then the stored message. [`output_for`] picks the sink for a turn: [`ChannelOutput`] for a
//! turn with a conversation, nothing for an isolated job run. Only `Text` blocks reach a
//! channel; reasoning, tool, and opaque blocks never do.
//!
//! - `blocks` channels get whole chunks through [`Reply::send`], split at paragraph boundaries
//!   outside code fences as the text streams, and the rest at the end of the message.
//! - `deltas` channels with a live-output hook ([`Channel::deltas`], a [`DeltaSink`]) get one live
//!   message per text block, fed with the model's text deltas. A `deltas` channel without the
//!   hook gets `blocks` output.

use std::mem;
use std::sync::Arc;

use async_trait::async_trait;
use giles_types::{
    Block, BlockKind, Channel, CompletionEvent, ConversationId, DeltaSink, DeltaWriter, Error,
    Message, OutputMode, Result,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::sessions::{Reply, Turn};

/// Receives one turn's responses as they stream.
#[async_trait]
pub trait OutputSink: Send {
    /// One streamed event of the response being received. The default ignores it.
    async fn event(&mut self, event: &CompletionEvent) {
        let _ = event;
    }

    /// The response is complete and stored as `message`. Every event of it went to
    /// [`event`](Self::event) first.
    async fn message_end(&mut self, message: &Message);

    /// The response being received was cut off (cancellation or a provider error) and is not
    /// stored. The default does nothing. Dropping the sink mid-response means the same.
    async fn message_aborted(&mut self) {}
}

/// The live-output hook of `channel`, if it has one.
fn delta_sink(channel: &dyn Channel) -> Option<&dyn DeltaSink> {
    channel.deltas()
}

/// A `deltas` channel's hook, kept for the turn: the channel registered when the turn started.
struct ChannelDeltas(Arc<dyn Channel>);

impl DeltaSink for ChannelDeltas {
    fn start(&self, conversation: &ConversationId) -> Result<Box<dyn DeltaWriter>> {
        delta_sink(&*self.0)
            .ok_or_else(|| Error::unavailable("the channel has no live output"))?
            .start(conversation)
    }
}

/// Live output for `channel`: `Some` only for a `deltas` channel with a hook.
fn live_output(channel: Arc<dyn Channel>) -> Option<Arc<dyn DeltaSink>> {
    if channel.info().output != OutputMode::Deltas {
        return None;
    }
    delta_sink(&*channel)?;
    Some(Arc::new(ChannelDeltas(channel)))
}

/// Where `turn`'s responses go: [`ChannelOutput`] into `turn.reply`, live if its channel is
/// registered now and streams; nothing for a turn without a conversation (an isolated job run).
pub fn output_for(turn: &Turn) -> Box<dyn OutputSink> {
    let Some(reply) = turn.reply.clone() else {
        return Box::new(Discard);
    };
    let live = reply.channel().and_then(live_output);
    Box::new(ChannelOutput::new(reply, turn.cancel.clone(), live))
}

/// Sends nothing.
pub(super) struct Discard;

#[async_trait]
impl OutputSink for Discard {
    async fn message_end(&mut self, _message: &Message) {}
}

/// Sends a turn's assistant text into its conversation, `blocks` or `deltas` (module docs).
///
/// Each text block is its own Markdown text: its leading whitespace is dropped, and in `blocks`
/// mode what's left of it is sent at its `BlockStop`, so a chunk never spans text blocks. The
/// `BlockStop` text is authoritative: text the deltas missed is added then. Live messages are
/// finished at the end of the message. Sends stop once the turn is cancelled; a failed send is
/// logged. An aborted response sends nothing more: in `blocks` mode the unsent text is dropped
/// (chunks already sent stay sent), in `deltas` mode its live messages are dropped unfinished,
/// which the channel shows as cut off.
pub struct ChannelOutput {
    reply: Reply,
    cancel: CancellationToken,
    live: Option<Arc<dyn DeltaSink>>,
    /// The current text block's text going out as `blocks`.
    paragraphs: Paragraphs,
    /// The text block being streamed.
    block: Option<TextBlock>,
    /// The message's complete live messages, finished at its end.
    done: Vec<Box<dyn DeltaWriter>>,
}

/// A text block being streamed.
struct TextBlock {
    index: u32,
    /// Every delta, as received.
    received: String,
    route: Route,
}

/// Where a text block's text goes.
enum Route {
    /// Only whitespace so far: not decided yet.
    Undecided,
    Blocks,
    Live(Box<dyn DeltaWriter>),
}

impl ChannelOutput {
    /// Output into `reply`'s conversation, streamed to `live` if `Some`. Sending stops once
    /// `cancel` is cancelled.
    pub fn new(reply: Reply, cancel: CancellationToken, live: Option<Arc<dyn DeltaSink>>) -> Self {
        Self {
            reply,
            cancel,
            live,
            paragraphs: Paragraphs::default(),
            block: None,
            done: Vec::new(),
        }
    }

    /// Hands `text` of the current block on.
    async fn text(&mut self, text: &str) {
        let Some(block) = &mut self.block else {
            return;
        };
        block.received.push_str(text);
        let mut text = text;
        if matches!(block.route, Route::Undecided) {
            text = text.trim_start();
            if text.is_empty() {
                return;
            }
            block.route = match &self.live {
                None => Route::Blocks,
                Some(live) => match live.start(self.reply.conversation()) {
                    Ok(writer) => Route::Live(writer),
                    Err(error) => {
                        warn!(instance = self.reply.instance(), %error, "cannot start live output");
                        Route::Blocks
                    }
                },
            };
        }
        let chunks = match &mut block.route {
            Route::Undecided => return,
            Route::Live(writer) => return writer.push(text),
            Route::Blocks => self.paragraphs.push(text),
        };
        for chunk in chunks {
            send(&self.reply, &self.cancel, chunk).await;
        }
    }
}

/// Sends `chunk` through `reply` unless `cancel` is cancelled first; a failure is logged.
async fn send(reply: &Reply, cancel: &CancellationToken, chunk: String) {
    if cancel.is_cancelled() {
        return;
    }
    tokio::select! {
        biased;
        () = cancel.cancelled() => {}
        sent = reply.send(chunk) => if let Err(error) = sent {
            warn!(instance = reply.instance(), %error, "cannot send the reply");
        },
    }
}

#[async_trait]
impl OutputSink for ChannelOutput {
    async fn event(&mut self, event: &CompletionEvent) {
        match event {
            CompletionEvent::BlockStart {
                index,
                kind: BlockKind::Text,
                ..
            } => {
                self.block = Some(TextBlock {
                    index: *index,
                    received: String::new(),
                    route: Route::Undecided,
                });
            }
            CompletionEvent::BlockStart { .. } => self.block = None,
            CompletionEvent::TextDelta { index, text } => {
                if self
                    .block
                    .as_ref()
                    .is_some_and(|block| block.index == *index)
                {
                    self.text(text).await;
                }
            }
            CompletionEvent::BlockStop { index, block } => {
                let Some(current) = self.block.as_ref().filter(|b| b.index == *index) else {
                    return;
                };
                if let Block::Text { text } = block {
                    match text.strip_prefix(current.received.as_str()) {
                        Some("") => {}
                        Some(missing) => self.text(missing).await,
                        None => warn!(
                            instance = self.reply.instance(),
                            "a text block's deltas don't match its text; sent as streamed"
                        ),
                    }
                }
                match self.block.take().map(|block| block.route) {
                    Some(Route::Live(writer)) => self.done.push(writer),
                    Some(Route::Blocks) => {
                        for chunk in self.paragraphs.finish() {
                            send(&self.reply, &self.cancel, chunk).await;
                        }
                    }
                    Some(Route::Undecided) | None => {}
                }
            }
            _ => {}
        }
    }

    async fn message_end(&mut self, _message: &Message) {
        // Every text block ended at its `BlockStop`, before `Stop`.
        self.block = None;
        self.paragraphs = Paragraphs::default();
        for writer in mem::take(&mut self.done) {
            if let Err(error) = writer.finish().await {
                warn!(instance = self.reply.instance(), %error, "cannot finish live output");
            }
        }
    }

    async fn message_aborted(&mut self) {
        self.block = None;
        self.done.clear();
        self.paragraphs = Paragraphs::default();
    }
}

/// Splits one streamed Markdown text into chunks at paragraph boundaries outside code fences.
///
/// A chunk ends at a run of blank lines (whitespace only, `\n` or `\r\n`) outside a fence, as
/// soon as the next line's first characters show a new top-level block starts there. So it
/// doesn't end:
///
/// - inside a fenced code block (backticks or tildes, three or more, closed by a run of the
///   same character at least as long; a backtick fence's info string has no backticks; an
///   unclosed fence runs to the end), whatever its indentation;
/// - before an indented line: indented code, a list item's continuation, or a nested block;
/// - between two list items, so a loose list stays in one chunk;
/// - after a heading, which stays with what follows it.
///
/// Chunks are sent without their trailing whitespace and the blank lines between them.
/// Whatever is left is a chunk at [`finish`](Self::finish).
#[derive(Debug, Default)]
struct Paragraphs {
    /// The incomplete last line.
    partial: String,
    /// The unsent chunk's complete lines; starts at a non-blank line.
    chunk: String,
    /// Blank lines after `chunk`, outside a fence.
    gap: String,
    fence: Option<Fence>,
    /// The last unindented line outside a fence is a list item.
    list: bool,
    /// The last non-blank line is an ATX heading.
    heading: bool,
}

impl Paragraphs {
    /// Adds `text` and returns the chunks it completes, in order.
    fn push(&mut self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(end) = rest.find('\n') {
            let (line, tail) = rest.split_at(end + 1);
            if self.partial.is_empty() {
                self.line(line, &mut out);
            } else {
                self.partial.push_str(line);
                let line = mem::take(&mut self.partial);
                self.line(&line, &mut out);
            }
            rest = tail;
        }
        self.partial.push_str(rest);
        if !self.gap.is_empty()
            && !self.partial.is_empty()
            && let Some(ends) = self.ends_before(&self.partial, false)
        {
            self.end_gap(ends, &mut out);
        }
        out
    }

    /// Whether the chunk ends before the line `start` begins, after a gap; `None` while that
    /// can't be told yet. `complete`: `start` is the whole (non-blank) line.
    fn ends_before(&self, start: &str, complete: bool) -> Option<bool> {
        if start.starts_with([' ', '\t', '\r']) {
            // Indented, or perhaps a blank line.
            return complete.then_some(false);
        }
        if self.heading {
            return Some(false);
        }
        if !self.list {
            return Some(true);
        }
        list_marker(start, complete).map(|item| !item)
    }

    /// Ends the gap after the chunk: sends the chunk or keeps the gap in it.
    fn end_gap(&mut self, ends: bool, out: &mut Vec<String>) {
        if ends {
            let chunk = mem::take(&mut self.chunk);
            out.push(chunk.trim_end().to_owned());
        } else {
            self.chunk.push_str(&self.gap);
        }
        self.gap.clear();
    }

    /// Ends the text: returns the chunks left, in order, and starts over.
    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let partial = mem::take(&mut self.partial);
        if !partial.is_empty() {
            self.line(&partial, &mut out);
        }
        let last = mem::take(&mut self.chunk);
        let last = last.trim_end();
        if !last.is_empty() {
            out.push(last.to_owned());
        }
        *self = Self::default();
        out
    }

    /// One line, with its line ending (none for the last line at the end).
    fn line(&mut self, line: &str, out: &mut Vec<String>) {
        let content = line.trim_end_matches(['\n', '\r']);
        if let Some(fence) = &self.fence {
            if fence.closed_by(content) {
                self.fence = None;
            }
            self.chunk.push_str(line);
            return;
        }
        if content.trim().is_empty() {
            if !self.chunk.is_empty() {
                self.gap.push_str(line);
            }
            return;
        }
        let indented = content.starts_with([' ', '\t']);
        if !self.gap.is_empty() {
            let ends = self.ends_before(content, true).unwrap_or(true);
            self.end_gap(ends, out);
        }
        if !indented {
            self.list = is_list_item(content);
        }
        self.heading = is_heading(content);
        self.fence = Fence::open(content);
        self.chunk.push_str(line);
    }
}

/// An open fenced code block.
#[derive(Debug, Clone, Copy)]
struct Fence {
    marker: char,
    len: usize,
    indent: usize,
}

impl Fence {
    /// The fence `line` opens, if it's an opening fence at any indentation.
    fn open(line: &str) -> Option<Self> {
        let indent = indentation(line);
        let rest = line.trim_start_matches([' ', '\t']);
        let marker = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let len = rest.chars().take_while(|&c| c == marker).count();
        if len < 3 {
            return None;
        }
        // `len` is a count of one-byte characters.
        let info = &rest[len..];
        if marker == '`' && info.contains('`') {
            return None;
        }
        Some(Self {
            marker,
            len,
            indent,
        })
    }

    /// Whether `line` closes this fence: up to three columns more indented than the opening,
    /// at least as many of its character, then only whitespace.
    fn closed_by(&self, line: &str) -> bool {
        if indentation(line) > self.indent + 3 {
            return false;
        }
        let rest = line.trim_start_matches([' ', '\t']);
        let len = rest.chars().take_while(|&c| c == self.marker).count();
        len >= self.len && rest[len..].trim().is_empty()
    }
}

/// Leading whitespace in columns, a tab counting four.
fn indentation(line: &str) -> usize {
    line.chars()
        .map_while(|c| match c {
            ' ' => Some(1),
            '\t' => Some(4),
            _ => None,
        })
        .sum()
}

/// Whether `line` starts a list item: up to three spaces, then `-`, `*`, `+`, or one to nine
/// digits and `.` or `)`, then whitespace or the end of the line.
fn is_list_item(line: &str) -> bool {
    list_marker(line, true) == Some(true)
}

/// [`is_list_item`] for a line that may not be `complete`: `None` while its start can't tell.
fn list_marker(line: &str, complete: bool) -> Option<bool> {
    if indentation(line) > 3 {
        return Some(false);
    }
    let rest = line.trim_start_matches(' ');
    let after = if let Some(after) = rest.strip_prefix(['-', '*', '+']) {
        after
    } else {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=9).contains(&digits) {
            return Some(false);
        }
        match rest[digits..].strip_prefix(['.', ')']) {
            Some(after) => after,
            None if digits == rest.len() && !complete => return None,
            None => return Some(false),
        }
    };
    match after.chars().next() {
        None if complete => Some(true),
        None => None,
        Some(c) => Some(c == ' ' || c == '\t'),
    }
}

/// Whether `line` is an ATX heading: up to three spaces, one to six `#`, then whitespace or the
/// end of the line.
fn is_heading(line: &str) -> bool {
    if indentation(line) > 3 {
        return false;
    }
    let rest = line.trim_start_matches(' ');
    let hashes = rest.bytes().take_while(|&b| b == b'#').count();
    (1..=6).contains(&hashes) && (rest.len() == hashes || rest[hashes..].starts_with([' ', '\t']))
}

#[cfg(test)]
mod tests;
