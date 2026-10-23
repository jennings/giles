//! The `send_attachment` core tool (PLAN section 5): sends media the session has (an
//! attachment, a tool's media result, a skill file `load_skill` handed over) to the current
//! conversation, with an optional caption.
//!
//! The media is named by the id in its reference, `[media:<id> ...]`, and must be one of the
//! session's ([`CoreCall::media`]), so a call can't send a blob the session never saw. It goes
//! out through the turn's channel ([`TurnContext::reply`](super::TurnContext)) as an
//! [`OutboundMessage`](giles_types::OutboundMessage) with the attachment. It sends, so it isn't
//! parallel-safe.

use async_trait::async_trait;
use giles_store::BlobStore;
use giles_types::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use super::{CoreCall, CoreTool};
use crate::agent_loop::ToolOutput;
use crate::media;

/// The tool's name.
pub const SEND_ATTACHMENT: &str = "send_attachment";

/// The `send_attachment` tool.
#[derive(Debug, Clone)]
pub struct SendAttachmentTool {
    blobs: BlobStore,
}

impl SendAttachmentTool {
    /// Checks that the media is still in `blobs` before sending it.
    pub fn new(blobs: BlobStore) -> Self {
        Self { blobs }
    }
}

/// The tool's input.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    media: String,
    caption: Option<String>,
}

#[async_trait]
impl CoreTool for SendAttachmentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: SEND_ATTACHMENT.to_owned(),
            description: "Send a file to the current conversation: an attachment, a tool's media \
                          result, or a skill file, by the id in its `[media:<id> ...]` reference, \
                          with an optional Markdown caption."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "media": {
                        "type": "string",
                        "description": "The media id from its `[media:<id> ...]` reference.",
                    },
                    "caption": {"type": "string", "description": "Text sent with it (Markdown)."},
                },
                "required": ["media"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: Input = match serde_json::from_value(call.input) {
            Ok(input) => input,
            Err(error) => return ToolOutput::error(format!("invalid input: {error}")),
        };
        let Some(media) = call.media.get(&input.media) else {
            return ToolOutput::error(format!("unknown media `{}`", input.media));
        };
        let Some(reply) = &call.turn.reply else {
            return ToolOutput::error("this session has no conversation to send to");
        };
        match self.blobs.stat(&media.id).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return ToolOutput::error(format!("media `{}` is no longer stored", media.id));
            }
            Err(error) => {
                warn!(media = %media.id, %error, "cannot look up a blob");
                return ToolOutput::error(format!("cannot look up media `{}`", media.id));
            }
        }
        let caption = input.caption.unwrap_or_default();
        match reply
            .send_with_attachments(caption, vec![media.clone()])
            .await
        {
            Ok(_) => ToolOutput {
                content: vec![giles_types::Block::text(format!(
                    "Sent {}.",
                    media::reference(media)
                ))],
                is_error: false,
            },
            Err(error) => ToolOutput::error(format!("sending failed: {error}")),
        }
    }
}
