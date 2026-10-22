//! The [`Provider`] trait: a model API (PLAN sections 4.4, 6.10).

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::message::{Block, BlockKind, Message, StopReason, Usage};

/// A model API, e.g. Anthropic Messages or OpenAI-compatible Chat Completions.
///
/// Built-in providers are constructed from config; the trait has no configuration or lifecycle
/// methods.
#[async_trait]
pub trait Provider: Send + Sync {
    /// The models this provider serves.
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;

    /// Starts a completion. Always streams; callers that want a whole response collect the
    /// stream. See [`CompletionEvent`] for the order of events.
    ///
    /// An error before the stream starts, or as a stream item, that is
    /// [retryable](crate::Error::is_retryable) may be retried on the next model.
    async fn complete(
        &self,
        req: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<CompletionEvent>>>;
}

/// One request to a model.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionRequest {
    /// The model id, without the provider instance prefix.
    pub model: String,
    /// The system prompt, assembled at session start.
    pub system: Vec<Block>,
    /// The conversation history.
    pub messages: Vec<Message>,
    /// The tools offered, names already prefixed (PLAN section 6.5) and sorted.
    pub tools: Vec<ToolSpec>,
    /// Generation settings.
    pub params: GenerationParams,
}

/// One event of a streamed completion.
///
/// A stream reports each content block as `BlockStart`, then deltas for that `index`, then
/// `BlockStop` carrying the complete block. Blocks never overlap: a provider closes the open block
/// with `BlockStop` before the next `BlockStart`, even if its API interleaves them. Indexes start
/// at 0 and each `BlockStart` index is one more than the last. `Usage` may come any number of
/// times and carries running totals for the whole request, so the last one wins. `Stop` comes
/// last.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CompletionEvent {
    /// A content block begins.
    BlockStart {
        /// The block's position in the response.
        index: u32,
        /// What kind of block it is.
        kind: BlockKind,
        /// For a tool use: the call id and the tool name.
        tool: Option<(String, String)>,
    },
    /// More text of a `Text` block.
    TextDelta {
        /// The block's position.
        index: u32,
        /// The new text.
        text: String,
    },
    /// More of a `ToolUse` block's input, as a fragment of its JSON text.
    ToolInputDelta {
        /// The block's position.
        index: u32,
        /// The JSON fragment.
        partial_json: String,
    },
    /// More reasoning text of a `Reasoning` block.
    ReasoningDelta {
        /// The block's position.
        index: u32,
        /// The new text.
        text: String,
    },
    /// A content block is complete.
    BlockStop {
        /// The block's position.
        index: u32,
        /// The complete block, including any `Opaque` data. This, not the deltas, is what gets
        /// stored.
        block: Block,
    },
    /// Token usage so far.
    Usage(Usage),
    /// The response is complete.
    Stop(StopReason),
}

/// A tool offered to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool name: bare from a [`Toolset`](crate::Toolset), prefixed with the instance name in
    /// a [`CompletionRequest`] (PLAN section 6.5).
    pub name: String,
    /// What the tool does, for the model.
    pub description: String,
    /// The JSON Schema of the tool input, an object schema.
    pub input_schema: serde_json::Value,
}

/// Generation settings for one request. `None` leaves the setting to the provider or model
/// default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenerationParams {
    /// The maximum number of output tokens.
    pub max_tokens: Option<u32>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Token budget for reasoning, on models that support it. `None` turns reasoning off where
    /// the API allows.
    pub reasoning_budget: Option<u32>,
    /// How long the provider should keep the prompt prefix cached (PLAN section 6.7).
    pub cache_retention: CacheRetention,
}

/// How long a prompt cache entry should live, for providers that let the caller choose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CacheRetention {
    /// The provider's short default (5 minutes on Anthropic); for job runs.
    #[default]
    Short,
    /// A longer lifetime (1 hour on Anthropic); for human-paced sessions.
    Long,
}

/// A model a provider serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// The model id, as used in [`CompletionRequest::model`].
    pub id: String,
    /// A human-readable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The context window in tokens, if known. Compaction (PLAN section 6.8) works against it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// The maximum output tokens per request, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Media types the model accepts as input blocks, e.g. `image/png`, `application/pdf`.
    /// Anything else reaches the model only as its text reference (PLAN section 5).
    pub media_types: Vec<String>,
    /// Whether the model can produce `Reasoning` blocks.
    pub reasoning: bool,
}

impl ModelInfo {
    /// Whether the model accepts `media_type` as an input block. Matching ignores ASCII case and
    /// parameters (`audio/ogg; codecs=opus` matches `audio/ogg`); there are no wildcards.
    pub fn accepts(&self, media_type: &str) -> bool {
        let Some(wanted) = essence(media_type) else {
            return false;
        };
        self.media_types
            .iter()
            .filter_map(|declared| essence(declared))
            .any(|declared| declared.eq_ignore_ascii_case(wanted))
    }
}

/// The `type/subtype` part of a media type, trimmed, or `None` if it isn't one.
fn essence(media_type: &str) -> Option<&str> {
    let essence = media_type.split(';').next().unwrap_or_default().trim();
    let (ty, subtype) = essence.split_once('/')?;
    let valid = |part: &str| !part.is_empty() && !part.contains(|c: char| c.is_whitespace());
    (valid(ty) && valid(subtype)).then_some(essence)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn model(media_types: &[&str]) -> ModelInfo {
        ModelInfo {
            id: "m".into(),
            display_name: None,
            context_window: Some(200_000),
            max_output_tokens: None,
            media_types: media_types.iter().map(|&t| t.into()).collect(),
            reasoning: false,
        }
    }

    #[test]
    fn accepts_exact_media_types_ignoring_case_and_parameters() {
        let m = model(&["image/png", "Audio/OGG", "application/pdf; version=2"]);
        assert!(m.accepts("image/png"));
        assert!(m.accepts("IMAGE/PNG"));
        assert!(m.accepts("audio/ogg; codecs=opus"));
        assert!(m.accepts(" audio/ogg "));
        assert!(m.accepts("application/pdf"));
        assert!(!m.accepts("image/jpeg"));
        assert!(!m.accepts("image"));
        assert!(!m.accepts("image/"));
        assert!(!m.accepts(""));
    }

    #[test]
    fn declared_wildcards_and_malformed_types_match_nothing() {
        let m = model(&["image/*", "garbage", "/"]);
        assert!(!m.accepts("image/png"));
        assert!(!m.accepts("garbage"));
        assert!(!m.accepts("/"));
        assert!(!model(&[]).accepts("image/png"));
    }

    #[test]
    fn model_info_json() {
        let m = model(&["image/png"]);
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(
            json,
            json!({
                "id": "m",
                "context_window": 200_000,
                "media_types": ["image/png"],
                "reasoning": false,
            })
        );
        assert_eq!(serde_json::from_value::<ModelInfo>(json).unwrap(), m);
    }

    #[test]
    fn tool_spec_json() {
        let spec = ToolSpec {
            name: "create_issue".into(),
            description: "Create an issue".into(),
            input_schema: json!({"type": "object", "properties": {}}),
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(
            json,
            json!({
                "name": "create_issue",
                "description": "Create an issue",
                "input_schema": {"type": "object", "properties": {}},
            })
        );
        assert_eq!(serde_json::from_value::<ToolSpec>(json).unwrap(), spec);
    }
}
