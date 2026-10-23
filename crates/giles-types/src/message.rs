//! The message model (PLAN section 5).
//!
//! Giles has its own content-block model, patterned on Anthropic's. Providers translate it to and
//! from their wire formats; the core never sees those.
//!
//! # Serialization
//!
//! Transcripts store [`Block`]s as JSON in SQLite, so their serde form is a stable format:
//!
//! - Enums are internally tagged with a `"type"` field in `snake_case`: `{"type": "text", "text":
//!   "hi"}`. [`Block::Opaque`] puts the [`Opaque`] fields next to the tag: `{"type": "opaque",
//!   "api": "anthropic", "data": {...}}`.
//! - `Option` fields are omitted when `None` and default to `None` when missing. Every other field
//!   is always written and required when read.
//! - Unknown fields are ignored, so a field added later must be optional or have a default.
//! - An unknown `"type"` is an error rather than being skipped: dropping a block could break the
//!   transcript invariant (every `ToolUse` has a `ToolResult`). Variants are only ever added, never
//!   renamed or removed.

use serde::{Deserialize, Serialize};

use crate::id::{MediaId, UserId};
use jiff::Timestamp;

/// One message in a session transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who wrote it.
    pub role: Role,
    /// The content, in order.
    pub content: Vec<Block>,
    /// When and from whom; which model produced it.
    pub meta: MessageMeta,
}

/// The author of a [`Message`]. The system prompt is assembled per turn and never stored, so it
/// has no role here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Role {
    /// A person (or a job prompt, or tool results returned to the model).
    User,
    /// The model.
    Assistant,
}

/// One content block of a [`Message`]. See the [module docs](self) for the JSON form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Block {
    /// Markdown text.
    Text {
        /// The text.
        text: String,
    },
    /// An image attachment.
    Image {
        /// The image in the blob store.
        media: MediaRef,
    },
    /// An audio attachment.
    Audio {
        /// The audio in the blob store.
        media: MediaRef,
    },
    /// A document attachment, e.g. a PDF.
    Document {
        /// The document in the blob store.
        media: MediaRef,
        /// The title given to the model, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// The model asks to call a tool.
    ToolUse {
        /// The provider-assigned call id; the matching [`Block::ToolResult`] refers to it.
        id: String,
        /// The tool name, with its instance prefix (PLAN section 6.5).
        name: String,
        /// The tool input, a JSON object.
        input: serde_json::Value,
    },
    /// The result of a [`Block::ToolUse`].
    ToolResult {
        /// The `id` of the `ToolUse` this answers.
        tool_use_id: String,
        /// The result content: text, images, audio, documents.
        content: Vec<Block>,
        /// Whether the call failed. Synthetic results (`cancelled`, `denied`, ...) are errors.
        is_error: bool,
    },
    /// The model's reasoning. Either part may be missing: some providers return only a summary,
    /// some only encrypted data.
    Reasoning {
        /// Readable reasoning text or summary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// Provider data that must go back unchanged, e.g. an Anthropic thinking signature.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opaque: Option<Opaque>,
    },
    /// A provider-private block (server-side tool blocks, citations) that must be sent back
    /// unchanged. The core stores it and never interprets it.
    Opaque(Opaque),
}

impl Block {
    /// A [`Block::Text`].
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// The kind of this block.
    pub fn kind(&self) -> BlockKind {
        match self {
            Self::Text { .. } => BlockKind::Text,
            Self::Image { .. } => BlockKind::Image,
            Self::Audio { .. } => BlockKind::Audio,
            Self::Document { .. } => BlockKind::Document,
            Self::ToolUse { .. } => BlockKind::ToolUse,
            Self::ToolResult { .. } => BlockKind::ToolResult,
            Self::Reasoning { .. } => BlockKind::Reasoning,
            Self::Opaque(_) => BlockKind::Opaque,
        }
    }
}

/// The variant of a [`Block`], without its content. Serialized like the block's `"type"` tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BlockKind {
    /// [`Block::Text`].
    Text,
    /// [`Block::Image`].
    Image,
    /// [`Block::Audio`].
    Audio,
    /// [`Block::Document`].
    Document,
    /// [`Block::ToolUse`].
    ToolUse,
    /// [`Block::ToolResult`].
    ToolResult,
    /// [`Block::Reasoning`].
    Reasoning,
    /// [`Block::Opaque`].
    Opaque,
}

/// Provider data the core stores but never interprets, e.g. Anthropic thinking signatures.
///
/// It's sent only to a provider of the same API family (`api`); after a fallback to another
/// family it's left out of the history sent, and the stored transcript keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Opaque {
    /// The API family that produced the data, e.g. `anthropic`.
    pub api: String,
    /// The data, as the provider returned it. Stored as JSON, so object key order isn't kept.
    pub data: serde_json::Value,
}

/// Metadata of a [`Message`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageMeta {
    /// The provider instance that produced an assistant message. `None` on user messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model id that produced an assistant message. `None` on user messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Token usage of the request that produced an assistant message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// When the message was sent (user) or completed (assistant).
    pub at: Timestamp,
    /// The user who sent a user message. Rendered as a `[sender]` prefix in group sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<UserId>,
}

impl MessageMeta {
    /// Metadata with only the time set.
    pub fn new(at: Timestamp) -> Self {
        Self {
            provider: None,
            model: None,
            usage: None,
            at,
            sender: None,
        }
    }
}

/// A reference to an attachment in the core's blob store. Bytes never travel inside messages.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaRef {
    /// The blob's id.
    pub id: MediaId,
    /// The media type, e.g. `image/png` or `audio/ogg; codecs=opus`.
    pub media_type: String,
    /// The size in bytes.
    pub size: u64,
    /// The file name, if the source had one, e.g. `voice.ogg`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Token counts for one provider request. Counts the provider doesn't report are zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens that were neither read from nor written to the prompt cache.
    pub input_tokens: u64,
    /// Output tokens, including reasoning.
    pub output_tokens: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write_tokens: u64,
}

impl Usage {
    /// The prompt cache hit rate: cache-read tokens over all input tokens (uncached input plus
    /// cache reads plus cache writes). `None` without input tokens.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        let input = self.input_tokens + self.cache_read_tokens + self.cache_write_tokens;
        // Token totals stay far below 2^53, where f64 starts losing integers.
        (input > 0).then(|| self.cache_read_tokens as f64 / input as f64)
    }
}

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// The model wants the results of the `ToolUse` blocks it produced.
    ToolUse,
    /// The output hit the token limit. A `ToolUse` cut off here gets the result `truncated`.
    MaxTokens,
    /// The model declined to answer.
    Refusal,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ID: &str = "a3f9000000000000000000000000000000000000000000000000000000000001";

    fn media(name: Option<&str>) -> MediaRef {
        MediaRef {
            id: MediaId::new(ID).unwrap(),
            media_type: "audio/ogg".into(),
            size: 2048,
            name: name.map(Into::into),
        }
    }

    /// Serializes `value`, checks the JSON, and checks it deserializes back to `value`.
    fn assert_json<T>(value: &T, expected: serde_json::Value)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_value(value).unwrap();
        assert_eq!(json, expected);
        let back: T = serde_json::from_value(json).unwrap();
        assert_eq!(&back, value);
    }

    #[test]
    fn cache_hit_rate_is_reads_over_all_input() {
        let usage = Usage {
            input_tokens: 110,
            output_tokens: 90,
            cache_read_tokens: 900,
            cache_write_tokens: 900,
        };
        assert_eq!(usage.cache_hit_rate(), Some(900.0 / 1910.0));
        let no_input = Usage {
            output_tokens: 10,
            ..Usage::default()
        };
        assert_eq!(no_input.cache_hit_rate(), None);
    }

    #[test]
    fn text_block_json() {
        assert_json(
            &Block::text("hello"),
            json!({"type": "text", "text": "hello"}),
        );
    }

    #[test]
    fn media_block_json() {
        let media_json =
            json!({"id": ID, "media_type": "audio/ogg", "size": 2048, "name": "a.ogg"});
        assert_json(
            &Block::Image {
                media: media(Some("a.ogg")),
            },
            json!({"type": "image", "media": media_json}),
        );
        assert_json(
            &Block::Audio {
                media: media(Some("a.ogg")),
            },
            json!({"type": "audio", "media": media_json}),
        );
        assert_json(
            &Block::Document {
                media: media(None),
                name: Some("Report".into()),
            },
            json!({
                "type": "document",
                "media": {"id": ID, "media_type": "audio/ogg", "size": 2048},
                "name": "Report",
            }),
        );
        assert_json(
            &Block::Document {
                media: media(None),
                name: None,
            },
            json!({
                "type": "document",
                "media": {"id": ID, "media_type": "audio/ogg", "size": 2048},
            }),
        );
    }

    #[test]
    fn tool_blocks_json() {
        assert_json(
            &Block::ToolUse {
                id: "toolu_1".into(),
                name: "github__create_issue".into(),
                input: json!({"title": "Bug", "labels": ["a"]}),
            },
            json!({
                "type": "tool_use",
                "id": "toolu_1",
                "name": "github__create_issue",
                "input": {"title": "Bug", "labels": ["a"]},
            }),
        );
        assert_json(
            &Block::ToolResult {
                tool_use_id: "toolu_1".into(),
                content: vec![Block::text("done"), Block::Image { media: media(None) }],
                is_error: false,
            },
            json!({
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": [
                    {"type": "text", "text": "done"},
                    {
                        "type": "image",
                        "media": {"id": ID, "media_type": "audio/ogg", "size": 2048},
                    },
                ],
                "is_error": false,
            }),
        );
    }

    #[test]
    fn reasoning_and_opaque_blocks_json() {
        let opaque = Opaque {
            api: "anthropic".into(),
            data: json!({"signature": "c2ln", "n": 1}),
        };
        assert_json(
            &Block::Reasoning {
                text: Some("thinking".into()),
                opaque: Some(opaque.clone()),
            },
            json!({
                "type": "reasoning",
                "text": "thinking",
                "opaque": {"api": "anthropic", "data": {"signature": "c2ln", "n": 1}},
            }),
        );
        assert_json(
            &Block::Reasoning {
                text: None,
                opaque: None,
            },
            json!({"type": "reasoning"}),
        );
        assert_json(
            &Block::Opaque(opaque),
            json!({"type": "opaque", "api": "anthropic", "data": {"signature": "c2ln", "n": 1}}),
        );
    }

    #[test]
    fn opaque_data_round_trips_exactly() {
        let data = json!({
            "type": "server_tool_use",
            "nested": [{"a": null}, 1.5, -7, u64::MAX, "\u{1f600}"],
            "empty": {},
        });
        let block = Block::Opaque(Opaque {
            api: "anthropic".into(),
            data: data.clone(),
        });
        let text = serde_json::to_string(&block).unwrap();
        let Block::Opaque(back) = serde_json::from_str(&text).unwrap() else {
            panic!("not opaque: {text}");
        };
        assert_eq!(back.data, data);
    }

    #[test]
    fn unknown_block_type_is_an_error() {
        let err = serde_json::from_value::<Block>(json!({"type": "hologram", "x": 1})).unwrap_err();
        assert!(err.to_string().contains("unknown variant"), "{err}");
        assert!(serde_json::from_value::<Block>(json!({"text": "untagged"})).is_err());
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_options_default() {
        let block: Block =
            serde_json::from_value(json!({"type": "text", "text": "hi", "added_later": true}))
                .unwrap();
        assert_eq!(block, Block::text("hi"));
        let block: Block = serde_json::from_value(json!({
            "type": "document",
            "media": {"id": ID, "media_type": "application/pdf", "size": 1},
        }))
        .unwrap();
        assert!(matches!(block, Block::Document { name: None, .. }));
    }

    #[test]
    fn required_fields_are_required() {
        let missing_is_error = json!({"type": "tool_result", "tool_use_id": "t", "content": []});
        assert!(serde_json::from_value::<Block>(missing_is_error).is_err());
        let bad_media_id =
            json!({"type": "image", "media": {"id": "a3f9", "media_type": "image/png", "size": 1}});
        assert!(serde_json::from_value::<Block>(bad_media_id).is_err());
    }

    #[test]
    fn block_kind_matches_the_serde_tag() {
        let blocks = [
            Block::text("t"),
            Block::Image { media: media(None) },
            Block::Audio { media: media(None) },
            Block::Document {
                media: media(None),
                name: None,
            },
            Block::ToolUse {
                id: "i".into(),
                name: "n".into(),
                input: json!({}),
            },
            Block::ToolResult {
                tool_use_id: "i".into(),
                content: vec![],
                is_error: true,
            },
            Block::Reasoning {
                text: None,
                opaque: None,
            },
            Block::Opaque(Opaque {
                api: "a".into(),
                data: json!(null),
            }),
        ];
        for block in blocks {
            let tag = serde_json::to_value(&block).unwrap()["type"].clone();
            assert_eq!(serde_json::to_value(block.kind()).unwrap(), tag);
        }
    }

    #[test]
    fn user_message_json() {
        let message = Message {
            role: Role::User,
            content: vec![
                Block::text("hello"),
                Block::Audio {
                    media: media(Some("voice.ogg")),
                },
            ],
            meta: MessageMeta {
                sender: Some(UserId::new("alice").unwrap()),
                ..MessageMeta::new("2026-10-22T12:00:00Z".parse().unwrap())
            },
        };
        assert_json(
            &message,
            json!({
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello"},
                    {
                        "type": "audio",
                        "media": {
                            "id": ID,
                            "media_type": "audio/ogg",
                            "size": 2048,
                            "name": "voice.ogg",
                        },
                    },
                ],
                "meta": {"at": "2026-10-22T12:00:00Z", "sender": "alice"},
            }),
        );
    }

    #[test]
    fn assistant_message_json() {
        let message = Message {
            role: Role::Assistant,
            content: vec![Block::text("hi")],
            meta: MessageMeta {
                provider: Some("anthropic".into()),
                model: Some("claude-sonnet-4-5".into()),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 20,
                    cache_read_tokens: 300,
                    cache_write_tokens: 4,
                }),
                ..MessageMeta::new("2026-10-22T12:00:01.5Z".parse().unwrap())
            },
        };
        assert_json(
            &message,
            json!({
                "role": "assistant",
                "content": [{"type": "text", "text": "hi"}],
                "meta": {
                    "provider": "anthropic",
                    "model": "claude-sonnet-4-5",
                    "usage": {
                        "input_tokens": 10,
                        "output_tokens": 20,
                        "cache_read_tokens": 300,
                        "cache_write_tokens": 4,
                    },
                    "at": "2026-10-22T12:00:01.5Z",
                },
            }),
        );
    }

    #[test]
    fn stop_reason_json() {
        for (reason, name) in [
            (StopReason::EndTurn, "end_turn"),
            (StopReason::ToolUse, "tool_use"),
            (StopReason::MaxTokens, "max_tokens"),
            (StopReason::Refusal, "refusal"),
        ] {
            assert_json(&reason, json!(name));
        }
    }
}
