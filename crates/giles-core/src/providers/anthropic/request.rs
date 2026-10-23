//! `CompletionRequest` to a Messages API request body.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use giles_types::{
    Block, CacheRetention, CompletionRequest, Error, GenerationParams, MediaRef, Message, Opaque,
    Result, Role, ToolSpec,
};
use serde_json::{Map, Value, json};

use super::API;
use super::config::AnthropicConfig;
use super::models::{FALLBACK_MAX_TOKENS, Family, MEDIA_TYPES, ThinkingMode, family_of};
use crate::providers::{MediaSource, wire};

/// The first `anthropic-version` that rejects `temperature`. Versions are dates, so they compare
/// as strings.
const NO_SAMPLING_PARAMS_VERSION: &str = "2026-10-21";

/// Builds the streaming request body for `req`, reading media through `media`.
pub(crate) async fn body(
    req: CompletionRequest,
    config: &AnthropicConfig,
    media: &dyn MediaSource,
) -> Result<Value> {
    let CompletionRequest {
        model,
        system,
        messages,
        tools,
        params,
    } = req;
    let family = family_of(&model);
    let cache_control = cache_control(params.cache_retention);

    let mut body = Map::new();
    body.insert(
        "max_tokens".into(),
        params
            .max_tokens
            .or(config.max_tokens)
            .or(family.map(|f| f.max_output_tokens))
            .unwrap_or(FALLBACK_MAX_TOKENS)
            .into(),
    );
    body.insert("model".into(), model.into());
    body.insert("stream".into(), true.into());
    // Automatic caching: the API puts a breakpoint on the last cacheable block and moves it as
    // the conversation grows. With the same TTL as the system breakpoint, it never conflicts.
    body.insert("cache_control".into(), cache_control.clone());
    if let Some(system) = system_json(system, cache_control)? {
        body.insert("system".into(), system);
    }
    body.insert("messages".into(), messages_json(messages, media).await?);
    if !tools.is_empty() {
        body.insert("tools".into(), tools.into_iter().map(tool_json).collect());
    }
    if let Some(thinking) = thinking_json(&params, family) {
        body.insert("thinking".into(), thinking);
    }
    // Sent only where both the API version and the model accept it; elsewhere the request
    // samples at the default instead of failing (unknown models count as new, which reject it).
    if let Some(temperature) = params.temperature
        && config.anthropic_version.as_str() < NO_SAMPLING_PARAMS_VERSION
        && family.is_some_and(|f| f.sampling_params)
    {
        body.insert("temperature".into(), wire::temperature(temperature)?);
    }
    Ok(Value::Object(body))
}

/// `{"type": "ephemeral"}` (5 minutes) or with `"ttl": "1h"` for [`CacheRetention::Long`].
fn cache_control(retention: CacheRetention) -> Value {
    match retention {
        CacheRetention::Long => json!({"type": "ephemeral", "ttl": "1h"}),
        _ => json!({"type": "ephemeral"}),
    }
}

/// The system text blocks with a cache breakpoint on the last one, or `None` if there is no
/// text. Only text is allowed in `system`.
fn system_json(system: Vec<Block>, cache_control: Value) -> Result<Option<Value>> {
    let mut blocks = Vec::with_capacity(system.len());
    for block in system {
        match block {
            Block::Text { text } if text.is_empty() => {}
            Block::Text { text } => blocks.push(json!({"type": "text", "text": text})),
            other => {
                return Err(Error::InvalidRequest(format!(
                    "the system prompt can only contain text, not {:?}",
                    other.kind()
                )));
            }
        }
    }
    let Some(Value::Object(last)) = blocks.last_mut() else {
        return Ok(None);
    };
    last.insert("cache_control".into(), cache_control);
    Ok(Some(Value::Array(blocks)))
}

/// The messages, with consecutive messages of the same role merged into one (each stored user
/// message is its own `Message`, PLAN section 5) and messages left empty by dropped blocks
/// skipped.
async fn messages_json(messages: Vec<Message>, media: &dyn MediaSource) -> Result<Value> {
    let mut turns: Vec<(Role, Vec<Value>)> = Vec::new();
    for message in messages {
        let mut content = Vec::with_capacity(message.content.len());
        for block in message.content {
            if let Some(block) = block_json(block, media, content.last()).await? {
                content.push(block);
            }
        }
        if content.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((role, turn)) if *role == message.role => turn.extend(content),
            _ => turns.push((message.role, content)),
        }
    }
    turns
        .into_iter()
        .map(|(role, content)| {
            let role = match role {
                Role::User => "user",
                Role::Assistant => "assistant",
                other => {
                    return Err(Error::InvalidRequest(format!("unsupported role {other:?}")));
                }
            };
            Ok(json!({"role": role, "content": content}))
        })
        .collect()
}

/// One message content block, or `None` for a block that isn't sent: empty text, reasoning
/// without an Anthropic signature, `Opaque` data of another API family, and the reference of
/// unsupported media when `previous` (the part before it) already is that reference.
async fn block_json(
    block: Block,
    media: &dyn MediaSource,
    previous: Option<&Value>,
) -> Result<Option<Value>> {
    Ok(match block {
        Block::ToolUse { id, name, input } => Some(json!({
            "type": "tool_use", "id": id, "name": name, "input": input,
        })),
        Block::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let mut result = Map::new();
            result.insert("type".into(), "tool_result".into());
            result.insert("tool_use_id".into(), tool_use_id.into());
            let mut blocks = Vec::with_capacity(content.len());
            for block in content {
                if let Some(block) = content_json(block, media, blocks.last()).await? {
                    blocks.push(block);
                }
            }
            if !blocks.is_empty() {
                result.insert("content".into(), blocks.into());
            }
            if is_error {
                result.insert("is_error".into(), true.into());
            }
            Some(Value::Object(result))
        }
        Block::Reasoning {
            text,
            opaque: Some(Opaque { api, data }),
        } if api == API => data
            .get("signature")
            .and_then(Value::as_str)
            .map(|signature| {
                json!({
                    "type": "thinking",
                    "thinking": text.unwrap_or_default(),
                    "signature": signature,
                })
            }),
        Block::Reasoning { .. } => None,
        Block::Opaque(Opaque { api, data }) if api == API => {
            if !data.is_object() {
                return Err(Error::InvalidRequest(
                    "an anthropic opaque block must be a JSON object".into(),
                ));
            }
            Some(data)
        }
        Block::Opaque(_) => None,
        other => content_json(other, media, previous).await?,
    })
}

/// A block that may appear anywhere, including inside a tool result: text and media. `None` for
/// empty text and for the reference of unsupported media that `previous` already is.
async fn content_json(
    block: Block,
    media: &dyn MediaSource,
    previous: Option<&Value>,
) -> Result<Option<Value>> {
    match block {
        Block::Text { text } if text.is_empty() => Ok(None),
        Block::Text { text } => Ok(Some(json!({"type": "text", "text": text}))),
        Block::Image { media: media_ref } | Block::Audio { media: media_ref } => {
            media_json(media_ref, None, media, previous).await
        }
        Block::Document {
            media: media_ref,
            name,
        } => media_json(media_ref, name, media, previous).await,
        other => Err(Error::InvalidRequest(format!(
            "a tool result can only contain text and media, not {:?}",
            other.kind()
        ))),
    }
}

/// An image or PDF as a base64 block, by media type whatever the block variant. Any other type
/// becomes its text reference `[media:<id> <name> <type>]`, so an unsupported attachment can't
/// make the request fail (PLAN section 5), unless `previous` already is that reference (the agent
/// loop stores one before each media block): then `None`.
async fn media_json(
    media_ref: MediaRef,
    title: Option<String>,
    media: &dyn MediaSource,
    previous: Option<&Value>,
) -> Result<Option<Value>> {
    let essence = wire::media_essence(&media_ref.media_type);
    if !MEDIA_TYPES.contains(&essence.as_str()) {
        let reference = crate::media::reference(&media_ref);
        let repeated = previous.is_some_and(|previous| {
            previous["type"] == "text" && previous["text"].as_str() == Some(reference.as_str())
        });
        return Ok((!repeated).then(|| json!({"type": "text", "text": reference})));
    }
    let data = BASE64.encode(media.read(&media_ref).await?);
    let source = json!({"type": "base64", "media_type": essence, "data": data});
    if essence == "application/pdf" {
        let mut document = json!({"type": "document", "source": source});
        if let Some(title) = title {
            document["title"] = title.into();
        }
        Ok(Some(document))
    } else {
        Ok(Some(json!({"type": "image", "source": source})))
    }
}

fn tool_json(tool: ToolSpec) -> Value {
    let mut json = Map::new();
    json.insert("name".into(), tool.name.into());
    if !tool.description.is_empty() {
        json.insert("description".into(), tool.description.into());
    }
    json.insert("input_schema".into(), tool.input_schema);
    Value::Object(json)
}

/// Without a reasoning budget: `disabled` on models that accept it, else no `thinking` field
/// (thinking stays on where it can't be turned off, and unknown models get their default). With
/// one: a token budget on models that take budgets, adaptive thinking on the others (the budget
/// only turns reasoning on there).
fn thinking_json(params: &GenerationParams, family: Option<&Family>) -> Option<Value> {
    let Some(budget) = params.reasoning_budget else {
        return family
            .is_some_and(|f| f.can_disable_thinking)
            .then(|| json!({"type": "disabled"}));
    };
    Some(match family.map(|f| f.thinking) {
        Some(ThinkingMode::Budget) => json!({"type": "enabled", "budget_tokens": budget}),
        Some(ThinkingMode::Adaptive) | None => json!({"type": "adaptive"}),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use bytes::Bytes;
    use giles_types::{MediaId, MessageMeta, Timestamp};

    use super::*;
    use crate::providers::http::ApiKey;

    /// Media bytes by id; reading an unknown id is `NotFound`.
    #[derive(Default)]
    struct Media(HashMap<MediaId, Bytes>);

    #[async_trait]
    impl MediaSource for Media {
        async fn read(&self, media: &MediaRef) -> Result<Bytes> {
            self.0
                .get(&media.id)
                .cloned()
                .ok_or_else(|| Error::NotFound(media.id.to_string()))
        }
    }

    fn media_ref(n: u8, media_type: &str, name: Option<&str>) -> MediaRef {
        MediaRef {
            id: MediaId::from_digest([n; 32]),
            media_type: media_type.into(),
            size: 3,
            name: name.map(Into::into),
        }
    }

    fn message(role: Role, content: Vec<Block>) -> Message {
        Message {
            role,
            content,
            meta: MessageMeta::new(Timestamp::UNIX_EPOCH),
        }
    }

    fn request(model: &str, messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest {
            model: model.into(),
            system: vec![],
            messages,
            tools: vec![],
            params: GenerationParams::default(),
        }
    }

    fn config() -> AnthropicConfig {
        AnthropicConfig::new(ApiKey::new("k"))
    }

    async fn build(req: CompletionRequest) -> Value {
        body(req, &config(), &Media::default()).await.unwrap()
    }

    fn hi() -> Vec<Message> {
        vec![message(Role::User, vec![Block::text("hi")])]
    }

    #[tokio::test]
    async fn minimal_request() {
        assert_eq!(
            build(request("claude-opus-5-5", hi())).await,
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 128_000,
                "stream": true,
                "cache_control": {"type": "ephemeral"},
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            })
        );
    }

    #[tokio::test]
    async fn system_breakpoint_and_automatic_caching_share_the_ttl() {
        let mut req = request("claude-sonnet-4-6", hi());
        req.system = vec![Block::text("soul"), Block::text(""), Block::text("memory")];
        req.params.cache_retention = CacheRetention::Long;
        let body = build(req).await;
        let one_hour = json!({"type": "ephemeral", "ttl": "1h"});
        assert_eq!(body["cache_control"], one_hour);
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": "soul"},
                {"type": "text", "text": "memory", "cache_control": one_hour},
            ])
        );

        let mut req = request("claude-sonnet-4-6", hi());
        req.system = vec![Block::text("soul")];
        let body = build(req).await;
        let five_minutes = json!({"type": "ephemeral"});
        assert_eq!(body["cache_control"], five_minutes);
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "soul", "cache_control": five_minutes}])
        );
    }

    #[tokio::test]
    async fn empty_system_is_omitted_and_non_text_system_is_rejected() {
        let mut req = request("m", hi());
        req.system = vec![Block::text("")];
        assert!(build(req).await.get("system").is_none());

        let mut req = request("m", hi());
        req.system = vec![Block::Image {
            media: media_ref(1, "image/png", None),
        }];
        let error = body(req, &config(), &Media::default()).await.unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(m) if m.contains("system")));
    }

    #[tokio::test]
    async fn consecutive_messages_of_one_role_are_merged() {
        let messages = vec![
            message(Role::User, vec![Block::text("one")]),
            message(Role::User, vec![Block::text("two")]),
            message(Role::Assistant, vec![Block::text("reply")]),
            message(Role::User, vec![Block::text("")]),
            message(Role::Assistant, vec![Block::text("more")]),
            message(Role::User, vec![Block::text("three")]),
        ];
        assert_eq!(
            build(request("m", messages)).await["messages"],
            json!([
                {"role": "user", "content": [
                    {"type": "text", "text": "one"}, {"type": "text", "text": "two"},
                ]},
                // The empty user message between them is skipped, so these two merge.
                {"role": "assistant", "content": [
                    {"type": "text", "text": "reply"}, {"type": "text", "text": "more"},
                ]},
                {"role": "user", "content": [{"type": "text", "text": "three"}]},
            ])
        );
    }

    #[tokio::test]
    async fn tool_use_and_results_with_media() {
        let png = media_ref(1, "image/PNG", Some("chart.png"));
        let pdf = media_ref(2, "application/pdf", Some("q3.pdf"));
        let ogg = media_ref(3, "audio/ogg; codecs=opus", Some("voice.ogg"));
        let csv = media_ref(4, "text/csv", None);
        let media = Media(HashMap::from([
            (png.id.clone(), Bytes::from_static(b"png")),
            (pdf.id.clone(), Bytes::from_static(b"pdf")),
        ]));
        let messages = vec![
            message(
                Role::Assistant,
                vec![Block::ToolUse {
                    id: "toolu_1".into(),
                    name: "files__read".into(),
                    input: json!({"path": "/x"}),
                }],
            ),
            message(
                Role::User,
                vec![Block::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: vec![
                        Block::text("here"),
                        Block::Image { media: png },
                        Block::Document {
                            media: pdf,
                            name: Some("Q3 report".into()),
                        },
                        Block::Audio { media: ogg.clone() },
                    ],
                    is_error: false,
                }],
            ),
            message(
                Role::User,
                vec![
                    Block::Document {
                        media: csv.clone(),
                        name: None,
                    },
                    Block::ToolResult {
                        tool_use_id: "toolu_2".into(),
                        content: vec![],
                        is_error: true,
                    },
                ],
            ),
        ];
        let body = body(request("m", messages), &config(), &media)
            .await
            .unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "files__read",
                     "input": {"path": "/x"}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "text", "text": "here"},
                        {"type": "image", "source": {
                            "type": "base64", "media_type": "image/png", "data": "cG5n"}},
                        {"type": "document", "title": "Q3 report", "source": {
                            "type": "base64", "media_type": "application/pdf", "data": "cGRm"}},
                        {"type": "text", "text": format!(
                            "[media:{} voice.ogg audio/ogg; codecs=opus]", ogg.id)},
                    ]},
                    {"type": "text", "text": format!("[media:{} text/csv]", csv.id)},
                    {"type": "tool_result", "tool_use_id": "toolu_2", "is_error": true},
                ]},
            ])
        );
    }

    #[tokio::test]
    async fn media_read_errors_propagate() {
        let messages = vec![message(
            Role::User,
            vec![Block::Image {
                media: media_ref(9, "image/png", None),
            }],
        )];
        let error = body(request("m", messages), &config(), &Media::default())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)));
    }

    #[tokio::test]
    async fn a_stored_reference_is_not_repeated_for_unsupported_media() {
        let ogg = media_ref(1, "audio/ogg", Some("voice.ogg"));
        let csv = media_ref(2, "text/csv", None);
        let reference = crate::media::reference(&ogg);
        let messages = vec![message(
            Role::User,
            vec![
                Block::text("listen"),
                Block::text(reference.clone()),
                Block::Audio { media: ogg.clone() },
                // Without a stored reference, the provider's own stands in.
                Block::ToolResult {
                    tool_use_id: "t".into(),
                    content: vec![
                        Block::text(reference.clone()),
                        Block::Audio { media: ogg },
                        Block::Document {
                            media: csv.clone(),
                            name: None,
                        },
                    ],
                    is_error: false,
                },
            ],
        )];
        let body = build(request("m", messages)).await;
        let content = body["messages"][0]["content"].as_array().unwrap();
        let texts: Vec<&str> = content.iter().filter_map(|b| b["text"].as_str()).collect();
        assert_eq!(texts, ["listen", reference.as_str()]);
        let result: Vec<&str> = content[2]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap())
            .collect();
        assert_eq!(result, [reference.clone(), crate::media::reference(&csv)]);
    }

    #[tokio::test]
    async fn non_content_blocks_in_tool_results_are_rejected() {
        let messages = vec![message(
            Role::User,
            vec![Block::ToolResult {
                tool_use_id: "t".into(),
                content: vec![Block::ToolUse {
                    id: "x".into(),
                    name: "y".into(),
                    input: json!({}),
                }],
                is_error: false,
            }],
        )];
        let error = body(request("m", messages), &config(), &Media::default())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(m) if m.contains("tool result")));
    }

    #[tokio::test]
    async fn thinking_signatures_and_anthropic_opaque_blocks_pass_back_unchanged() {
        let anthropic = |data: Value| Opaque {
            api: "anthropic".into(),
            data,
        };
        let server_tool_use = json!({
            "type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
            "input": {"query": "weather"},
        });
        let redacted = json!({"type": "redacted_thinking", "data": "EmwKAhgB"});
        let messages = vec![
            message(Role::User, vec![Block::text("q")]),
            message(
                Role::Assistant,
                vec![
                    Block::Reasoning {
                        text: Some("Let me think.".into()),
                        opaque: Some(anthropic(json!({"signature": "sig-1"}))),
                    },
                    // Omitted display: no text, only the signature.
                    Block::Reasoning {
                        text: None,
                        opaque: Some(anthropic(json!({"signature": "sig-2"}))),
                    },
                    Block::Opaque(anthropic(redacted.clone())),
                    Block::Opaque(anthropic(server_tool_use.clone())),
                    // Not sent: another API family's data, and reasoning with no signature.
                    Block::Opaque(Opaque {
                        api: "openai".into(),
                        data: json!({"type": "reasoning"}),
                    }),
                    Block::Reasoning {
                        text: Some("unsigned".into()),
                        opaque: None,
                    },
                    Block::Reasoning {
                        text: Some("foreign".into()),
                        opaque: Some(Opaque {
                            api: "openai".into(),
                            data: json!({"signature": "x"}),
                        }),
                    },
                    Block::text("answer"),
                ],
            ),
        ];
        assert_eq!(
            build(request("m", messages)).await["messages"][1],
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Let me think.", "signature": "sig-1"},
                {"type": "thinking", "thinking": "", "signature": "sig-2"},
                redacted,
                server_tool_use,
                {"type": "text", "text": "answer"},
            ]})
        );
    }

    #[tokio::test]
    async fn assistant_message_of_only_foreign_data_is_dropped() {
        let messages = vec![
            message(Role::User, vec![Block::text("a")]),
            message(
                Role::Assistant,
                vec![Block::Opaque(Opaque {
                    api: "openai".into(),
                    data: json!({}),
                })],
            ),
            message(Role::User, vec![Block::text("b")]),
        ];
        assert_eq!(
            build(request("m", messages)).await["messages"],
            json!([{"role": "user", "content": [
                {"type": "text", "text": "a"}, {"type": "text", "text": "b"},
            ]}])
        );
    }

    #[tokio::test]
    async fn tools_keep_their_names() {
        let mut req = request("m", hi());
        req.tools = vec![
            ToolSpec {
                name: "github__create_issue".into(),
                description: "Create an issue".into(),
                input_schema: json!({"type": "object"}),
            },
            ToolSpec {
                name: "memory".into(),
                description: String::new(),
                input_schema: json!({"type": "object", "properties": {}}),
            },
        ];
        assert_eq!(
            build(req).await["tools"],
            json!([
                {"name": "github__create_issue", "description": "Create an issue",
                 "input_schema": {"type": "object"}},
                {"name": "memory", "input_schema": {"type": "object", "properties": {}}},
            ])
        );
    }

    #[tokio::test]
    async fn max_tokens_precedence() {
        let mut req = request("claude-haiku-4-5", hi());
        assert_eq!(build(req.clone()).await["max_tokens"], 64_000);
        assert_eq!(
            build(request("future-model", hi())).await["max_tokens"],
            16_384
        );

        let mut config = config();
        config.max_tokens = Some(4096);
        let media = Media::default();
        let body_with = |req| body(req, &config, &media);
        assert_eq!(body_with(req.clone()).await.unwrap()["max_tokens"], 4096);
        req.params.max_tokens = Some(100);
        assert_eq!(body_with(req).await.unwrap()["max_tokens"], 100);
    }

    #[tokio::test]
    async fn thinking_with_a_budget_by_model_family() {
        let mut req = request("claude-sonnet-4-5-20250929", hi());
        req.params.reasoning_budget = Some(2048);
        assert_eq!(
            build(req.clone()).await["thinking"],
            json!({"type": "enabled", "budget_tokens": 2048})
        );
        for model in ["claude-opus-4-7", "claude-opus-5-5", "claude-next-1"] {
            req.model = model.into();
            assert_eq!(
                build(req.clone()).await["thinking"],
                json!({"type": "adaptive"}),
                "{model}"
            );
        }
    }

    #[tokio::test]
    async fn no_budget_disables_thinking_where_the_model_allows() {
        // Accept `disabled`: thinking is turned off explicitly, also where it's on by default.
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-haiku-5-5",
            "claude-opus-4-7",
            "claude-sonnet-4-6",
            "claude-haiku-4-5-20251001",
        ] {
            assert_eq!(
                build(request(model, hi())).await["thinking"],
                json!({"type": "disabled"}),
                "{model}"
            );
        }
        // Reject `disabled` (thinking is always on), or unknown: no field, the model's default.
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-fable-5-1",
            "claude-mythos-5",
            "claude-next-1",
        ] {
            let body = build(request(model, hi())).await;
            assert!(body.get("thinking").is_none(), "{model}");
        }
    }

    #[tokio::test]
    async fn temperature_only_where_version_and_model_accept_it() {
        let with_temperature = |model: &str| {
            let mut req = request(model, hi());
            req.params.temperature = Some(0.7);
            req
        };
        for model in ["claude-sonnet-4-6", "claude-haiku-4-5-20251001"] {
            assert_eq!(
                build(with_temperature(model)).await["temperature"],
                json!(0.7),
                "{model}"
            );
        }
        // Claude 4.7 and later reject a non-default temperature on every API version; unknown
        // models are treated as new.
        for model in [
            "claude-opus-4-7",
            "claude-opus-5-5",
            "claude-haiku-5-5",
            "claude-next-1",
        ] {
            let body = build(with_temperature(model)).await;
            assert!(body.get("temperature").is_none(), "{model}");
        }

        // API version 2026-10-21 removed the parameter for every model.
        let mut config = config();
        config.anthropic_version = "2026-10-21".into();
        let req = with_temperature("claude-sonnet-4-6");
        let body = body(req, &config, &Media::default()).await.unwrap();
        assert!(body.get("temperature").is_none());

        let mut req = with_temperature("claude-sonnet-4-6");
        req.params.temperature = Some(f32::NAN);
        let error = body_error(req).await;
        assert!(matches!(error, Error::InvalidRequest(m) if m.contains("temperature")));
    }

    async fn body_error(req: CompletionRequest) -> Error {
        body(req, &config(), &Media::default()).await.unwrap_err()
    }
}
