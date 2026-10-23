//! Output delivery (PLAN section 6.9): what of the model's responses reaches the channel.
//!
//! [`OutputSink`] is the seam. The agent loop hands it every streamed event of a response and
//! then the stored message. [`MessageOutput`] sends each assistant message's text when the
//! message is complete. TASKS.md 2.8 replaces it with sinks that stream deltas (`deltas`
//! channels) or flush paragraphs outside code fences (`blocks` channels). Reasoning and tool
//! blocks never reach a channel.

use async_trait::async_trait;
use giles_types::{Block, CompletionEvent, Message};
use tracing::warn;

use crate::sessions::Reply;

/// Receives one turn's responses as they stream.
#[async_trait]
pub trait OutputSink: Send {
    /// One streamed event of the response being received. The default ignores it.
    async fn event(&mut self, event: &CompletionEvent) {
        let _ = event;
    }

    /// The response is complete and stored as `message`.
    async fn message_end(&mut self, message: &Message);

    /// The response being received was cut off (cancellation or a provider error) and is not
    /// stored. The default does nothing; a sink that sent part of it may want to say so.
    async fn message_aborted(&mut self) {}
}

/// The text of `message` as the channel gets it: its `Text` blocks, trimmed, joined by blank
/// lines. Empty when it has none.
pub fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Sends each complete assistant message's text in one piece (nothing for a message without
/// text). A failed send is logged; the message is stored either way.
#[derive(Debug)]
pub struct MessageOutput {
    reply: Option<Reply>,
}

impl MessageOutput {
    /// Output into `reply`'s conversation; `None` (an isolated job run) sends nothing.
    pub fn new(reply: Option<Reply>) -> Self {
        Self { reply }
    }
}

#[async_trait]
impl OutputSink for MessageOutput {
    async fn message_end(&mut self, message: &Message) {
        let Some(reply) = &self.reply else {
            return;
        };
        let text = message_text(message);
        if text.is_empty() {
            return;
        }
        if let Err(error) = reply.send(text).await {
            warn!(instance = reply.instance(), %error, "cannot send the reply");
        }
    }
}

#[cfg(test)]
mod tests {
    use giles_types::{MessageMeta, Role, Timestamp};
    use serde_json::json;

    use super::*;

    #[test]
    fn only_text_blocks_reach_the_channel() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                Block::Reasoning {
                    text: Some("secret thoughts".to_owned()),
                    opaque: None,
                },
                Block::text("  Let me check.\n"),
                Block::ToolUse {
                    id: "t1".to_owned(),
                    name: "github__list".to_owned(),
                    input: json!({}),
                },
                Block::text(" "),
                Block::text("Done."),
            ],
            meta: MessageMeta::new(Timestamp::UNIX_EPOCH),
        };
        assert_eq!(message_text(&message), "Let me check.\n\nDone.");
    }
}
