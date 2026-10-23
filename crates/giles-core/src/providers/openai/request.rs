//! `CompletionRequest` to a Chat Completions request body.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use giles_types::{
    Block, CacheRetention, CompletionRequest, Error, MediaRef, Message, Opaque, Result, Role,
    ToolSpec,
};
use serde_json::{Map, Value, json};

use super::API;
use super::config::{Dialect, OpenAiConfig};
use crate::providers::{MediaSource, wire};

/// The assistant message fields reasoning text is sent back in, by the field it streamed in.
/// DeepSeek requires its `reasoning_content` back during tool use; Ollama and OpenRouter read
/// `reasoning`.
pub(crate) const REASONING_FIELDS: [&str; 2] = ["reasoning", "reasoning_content"];

/// Builds the streaming request body for `req`, reading media through `media`.
pub(crate) async fn body(
    req: CompletionRequest,
    config: &OpenAiConfig,
    media: &dyn MediaSource,
) -> Result<Value> {
    let CompletionRequest {
        model,
        system,
        messages,
        tools,
        params,
    } = req;
    let settings = config.settings(&model);
    let accepted = config.media_types(&model);
    let reasoning_model = settings.reasoning.unwrap_or(false);
    let dialect = config.dialect;

    let mut wire_messages = Vec::with_capacity(messages.len() + 1);
    if let Some(system) = system_json(system)? {
        wire_messages.push(system);
    }
    let media = Media {
        source: media,
        accepted: &accepted,
    };
    // OpenAI never streams reasoning in Chat Completions and may reject the field; reasoning from
    // another instance of this component (after a fallback) isn't sent there.
    let send_reasoning = dialect != Dialect::OpenAi;
    messages_json(messages, &media, send_reasoning, &mut wire_messages).await?;

    let mut body = Map::new();
    body.insert("model".into(), model.into());
    body.insert("stream".into(), true.into());
    body.insert("stream_options".into(), json!({"include_usage": true}));
    body.insert("messages".into(), wire_messages.into());
    if !tools.is_empty() {
        body.insert(
            "tools".into(),
            tools.into_iter().map(tool_spec_json).collect(),
        );
    }
    if let Some(max_tokens) = params.max_tokens.or(settings.max_tokens) {
        // OpenAI deprecated `max_tokens` and its reasoning models reject it; other servers may
        // not know the newer name.
        let field = match dialect {
            Dialect::OpenAi => "max_completion_tokens",
            _ => "max_tokens",
        };
        body.insert(field.into(), max_tokens.into());
    }
    // OpenAI's reasoning models accept only the default temperature.
    if let Some(temperature) = params.temperature
        && !(dialect == Dialect::OpenAi && reasoning_model)
    {
        body.insert("temperature".into(), wire::temperature(temperature)?);
    }
    if let Some(budget) = params.reasoning_budget {
        match dialect {
            // OpenRouter takes a token budget and ignores it for models that don't reason.
            Dialect::OpenRouter => {
                body.insert("reasoning".into(), json!({"max_tokens": budget}));
            }
            // Elsewhere an unknown parameter can fail the request, so only declared reasoning
            // models get it.
            _ if reasoning_model => {
                body.insert("reasoning_effort".into(), effort(budget).into());
            }
            _ => {}
        }
    }
    if dialect == Dialect::OpenRouter {
        // Automatic prompt caching for the models that need it asked for (Anthropic's); OpenRouter
        // leaves it out for the others, which cache on their own.
        let cache_control = match params.cache_retention {
            CacheRetention::Long => json!({"type": "ephemeral", "ttl": "1h"}),
            _ => json!({"type": "ephemeral"}),
        };
        body.insert("cache_control".into(), cache_control);
    }
    Ok(Value::Object(body))
}

/// A reasoning token budget as a `reasoning_effort` level: up to 2,048 tokens is `low`, up to
/// 8,192 is `medium`, more is `high`.
pub(crate) fn effort(budget: u32) -> &'static str {
    match budget {
        0..=2048 => "low",
        2049..=8192 => "medium",
        _ => "high",
    }
}

/// The system text blocks as one `system` message, joined by blank lines, or `None` if there is
/// no text. Only text is allowed in `system`.
fn system_json(system: Vec<Block>) -> Result<Option<Value>> {
    let mut texts = Vec::with_capacity(system.len());
    for block in system {
        match block {
            Block::Text { text } if text.is_empty() => {}
            Block::Text { text } => texts.push(text),
            other => {
                return Err(Error::InvalidRequest(format!(
                    "the system prompt can only contain text, not {:?}",
                    other.kind()
                )));
            }
        }
    }
    Ok((!texts.is_empty()).then(|| json!({"role": "system", "content": texts.join("\n\n")})))
}

/// Media sent as content: where the bytes come from and which types the model accepts.
struct Media<'a> {
    source: &'a dyn MediaSource,
    accepted: &'a [String],
}

impl Media<'_> {
    /// The content part for `media_ref` if the model accepts its type, else `None` (the caller
    /// sends its text reference).
    async fn part(&self, media_ref: &MediaRef, name: Option<&str>) -> Result<Option<Value>> {
        let essence = wire::media_essence(&media_ref.media_type);
        if !self.accepted.contains(&essence) {
            return Ok(None);
        }
        let audio_format = match essence.as_str() {
            "audio/wav" => Some("wav"),
            "audio/mpeg" => Some("mp3"),
            _ => None,
        };
        let is_image = essence.starts_with("image/");
        if audio_format.is_none() && !is_image && essence != "application/pdf" {
            return Ok(None);
        }
        let data = BASE64.encode(self.source.read(media_ref).await?);
        Ok(Some(if let Some(format) = audio_format {
            json!({"type": "input_audio", "input_audio": {"data": data, "format": format}})
        } else if is_image {
            json!({"type": "image_url", "image_url": {"url": format!("data:{essence};base64,{data}")}})
        } else {
            let filename = name
                .or(media_ref.name.as_deref())
                .map_or_else(|| format!("{}.pdf", media_ref.id), str::to_owned);
            json!({"type": "file", "file": {
                "filename": filename,
                "file_data": format!("data:application/pdf;base64,{data}"),
            }})
        }))
    }
}

fn text_part(text: impl Into<String>) -> Value {
    json!({"type": "text", "text": text.into()})
}

/// What a run of consecutive user messages leaves for after its `tool` messages: the media moved
/// out of tool results, and the user messages themselves. Chat Completions requires the `tool`
/// messages answering an assistant's `tool_calls` to come right after it.
#[derive(Default)]
struct UserRun {
    relocated: Vec<Value>,
    users: Vec<Value>,
}

impl UserRun {
    fn flush(&mut self, out: &mut Vec<Value>) {
        if !self.relocated.is_empty() {
            let content = std::mem::take(&mut self.relocated);
            out.push(json!({"role": "user", "content": content}));
        }
        out.append(&mut self.users);
    }
}

/// The messages, in order, except that within a run of consecutive user messages every tool
/// result comes first as a `tool` message (one per result), then a `user` message with the media
/// moved out of them, then the run's other content, one `user` message per stored message.
/// Reasoning goes back only if `send_reasoning`.
async fn messages_json(
    messages: Vec<Message>,
    media: &Media<'_>,
    send_reasoning: bool,
    out: &mut Vec<Value>,
) -> Result<()> {
    let mut run = UserRun::default();
    for message in messages {
        match message.role {
            Role::User => user_json(message.content, media, out, &mut run).await?,
            Role::Assistant => {
                run.flush(out);
                if let Some(message) = assistant_json(message.content, send_reasoning)? {
                    out.push(message);
                }
            }
            other => {
                return Err(Error::InvalidRequest(format!("unsupported role {other:?}")));
            }
        }
    }
    run.flush(out);
    Ok(())
}

/// One user message: tool results go to `out` as `tool` messages, the rest to `run.users`.
/// Content is a string when it's a single text, else an array of parts.
async fn user_json(
    content: Vec<Block>,
    media: &Media<'_>,
    out: &mut Vec<Value>,
    run: &mut UserRun,
) -> Result<()> {
    let mut parts = Vec::with_capacity(content.len());
    for block in content {
        match block {
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => out.push(tool_json(tool_use_id, content, is_error, media, run).await?),
            Block::Text { text } if text.is_empty() => {}
            Block::Text { text } => parts.push(text_part(text)),
            Block::Image { media: media_ref } | Block::Audio { media: media_ref } => {
                parts.push(media_or_reference(&media_ref, None, media).await?);
            }
            Block::Document {
                media: media_ref,
                name,
            } => parts.push(media_or_reference(&media_ref, name.as_deref(), media).await?),
            // Reasoning and provider data belong to assistant messages.
            Block::Reasoning { .. } | Block::Opaque(_) => {}
            other => {
                return Err(Error::InvalidRequest(format!(
                    "a user message can't contain {:?}",
                    other.kind()
                )));
            }
        }
    }
    let content = match <[Value; 1]>::try_from(parts) {
        Ok([part]) if part["type"] == "text" => part["text"].clone(),
        Ok([part]) => Value::Array(vec![part]),
        Err(parts) if parts.is_empty() => return Ok(()),
        Err(parts) => Value::Array(parts),
    };
    run.users.push(json!({"role": "user", "content": content}));
    Ok(())
}

async fn media_or_reference(
    media_ref: &MediaRef,
    name: Option<&str>,
    media: &Media<'_>,
) -> Result<Value> {
    Ok(match media.part(media_ref, name).await? {
        Some(part) => part,
        None => text_part(crate::media::reference(media_ref)),
    })
}

/// A tool result as a `tool` message. Its text is the result's text blocks, joined by blank
/// lines, with each media block as its reference; `tool` messages hold only text, so media the
/// model accepts is moved to `run.relocated`, after its reference. An error result is prefixed
/// with `Error: ` (the API has no error flag).
async fn tool_json(
    tool_use_id: String,
    content: Vec<Block>,
    is_error: bool,
    media: &Media<'_>,
    run: &mut UserRun,
) -> Result<Value> {
    let mut texts = Vec::with_capacity(content.len());
    for block in content {
        let (media_ref, name) = match block {
            Block::Text { text } if text.is_empty() => continue,
            Block::Text { text } => {
                texts.push(text);
                continue;
            }
            Block::Image { media } | Block::Audio { media } => (media, None),
            Block::Document { media, name } => (media, name),
            other => {
                return Err(Error::InvalidRequest(format!(
                    "a tool result can only contain text and media, not {:?}",
                    other.kind()
                )));
            }
        };
        let reference = crate::media::reference(&media_ref);
        if let Some(part) = media.part(&media_ref, name.as_deref()).await? {
            run.relocated.push(text_part(reference.clone()));
            run.relocated.push(part);
        }
        texts.push(reference);
    }
    let text = texts.join("\n\n");
    let text = match (is_error, text.is_empty()) {
        (false, _) => text,
        (true, true) => "Error".to_owned(),
        (true, false) => format!("Error: {text}"),
    };
    Ok(json!({"role": "tool", "tool_call_id": tool_use_id, "content": text}))
}

/// An assistant message: text blocks joined by blank lines (media as references), tool uses as
/// `tool_calls`, and, if `send_reasoning`, reasoning this provider produced back in the field it
/// came in. `None` when there is neither text nor a tool call.
fn assistant_json(content: Vec<Block>, send_reasoning: bool) -> Result<Option<Value>> {
    let mut texts = Vec::new();
    let mut tool_calls = Vec::new();
    let mut reasoning: Map<String, Value> = Map::new();
    for block in content {
        match block {
            Block::Text { text } if text.is_empty() => {}
            Block::Text { text } => texts.push(text),
            Block::Image { media } | Block::Audio { media } | Block::Document { media, .. } => {
                texts.push(crate::media::reference(&media));
            }
            Block::ToolUse { id, name, input } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": input.to_string()},
            })),
            Block::Reasoning {
                text: Some(text),
                opaque: Some(Opaque { api, data }),
            } if send_reasoning && api == API => {
                let field = data.get("field").and_then(Value::as_str);
                if let Some(field) = field.filter(|field| REASONING_FIELDS.contains(field)) {
                    let entry = reasoning
                        .entry(field)
                        .or_insert_with(|| Value::String(String::new()));
                    if let Value::String(all) = entry {
                        all.push_str(&text);
                    }
                }
            }
            // Reasoning of other APIs, and provider data: never sent (PLAN section 5).
            Block::Reasoning { .. } | Block::Opaque(_) => {}
            other => {
                return Err(Error::InvalidRequest(format!(
                    "an assistant message can't contain {:?}",
                    other.kind()
                )));
            }
        }
    }
    if texts.is_empty() && tool_calls.is_empty() {
        return Ok(None);
    }
    let mut message = Map::new();
    message.insert("role".into(), "assistant".into());
    message.insert(
        "content".into(),
        if texts.is_empty() {
            Value::Null
        } else {
            texts.join("\n\n").into()
        },
    );
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), tool_calls.into());
    }
    message.extend(reasoning);
    Ok(Some(Value::Object(message)))
}

fn tool_spec_json(tool: ToolSpec) -> Value {
    let mut function = Map::new();
    function.insert("name".into(), tool.name.into());
    if !tool.description.is_empty() {
        function.insert("description".into(), tool.description.into());
    }
    function.insert("parameters".into(), tool.input_schema);
    json!({"type": "function", "function": function})
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use bytes::Bytes;
    use giles_types::{GenerationParams, MediaId, MessageMeta, Timestamp};

    use super::*;
    use crate::providers::openai::config::ModelSettings;

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

    fn config(dialect: Dialect) -> OpenAiConfig {
        let mut config = OpenAiConfig::new(None);
        config.dialect = dialect;
        config
    }

    async fn build(req: CompletionRequest) -> Value {
        body(req, &config(Dialect::OpenAi), &Media::default())
            .await
            .unwrap()
    }

    async fn build_with(req: CompletionRequest, config: &OpenAiConfig) -> Value {
        body(req, config, &Media::default()).await.unwrap()
    }

    async fn build_error(req: CompletionRequest) -> Error {
        body(req, &config(Dialect::OpenAi), &Media::default())
            .await
            .unwrap_err()
    }

    fn hi() -> Vec<Message> {
        vec![message(Role::User, vec![Block::text("hi")])]
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Block {
        Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    fn tool_result(id: &str, content: Vec<Block>, is_error: bool) -> Block {
        Block::ToolResult {
            tool_use_id: id.into(),
            content,
            is_error,
        }
    }

    #[tokio::test]
    async fn minimal_request() {
        assert_eq!(
            build(request("gpt-5.1", hi())).await,
            json!({
                "model": "gpt-5.1",
                "stream": true,
                "stream_options": {"include_usage": true},
                "messages": [{"role": "user", "content": "hi"}],
            })
        );
    }

    #[tokio::test]
    async fn system_blocks_become_one_system_message() {
        let mut req = request("m", hi());
        req.system = vec![Block::text("soul"), Block::text(""), Block::text("memory")];
        assert_eq!(
            build(req).await["messages"],
            json!([
                {"role": "system", "content": "soul\n\nmemory"},
                {"role": "user", "content": "hi"},
            ])
        );

        let mut req = request("m", hi());
        req.system = vec![Block::text("")];
        assert_eq!(build(req).await["messages"].as_array().unwrap().len(), 1);

        let mut req = request("m", hi());
        req.system = vec![Block::Image {
            media: media_ref(1, "image/png", None),
        }];
        assert!(matches!(build_error(req).await, Error::InvalidRequest(m) if m.contains("system")));
    }

    #[tokio::test]
    async fn user_messages_stay_separate_and_use_parts_only_when_needed() {
        let png = media_ref(1, "image/png", None);
        let media = Media(HashMap::from([(
            png.id.clone(),
            Bytes::from_static(b"png"),
        )]));
        let messages = vec![
            message(Role::User, vec![Block::text("[09:00] hi")]),
            message(Role::User, vec![Block::text("")]),
            message(
                Role::User,
                vec![
                    Block::text("[09:01] look"),
                    Block::Image { media: png.clone() },
                ],
            ),
            message(Role::User, vec![Block::text("a"), Block::text("b")]),
            message(Role::User, vec![Block::Image { media: png }]),
        ];
        let body = body(request("m", messages), &config(Dialect::OpenAi), &media)
            .await
            .unwrap();
        let image =
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}});
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "[09:00] hi"},
                {"role": "user", "content": [
                    {"type": "text", "text": "[09:01] look"}, image,
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "a"}, {"type": "text", "text": "b"},
                ]},
                {"role": "user", "content": [image]},
            ])
        );
    }

    #[tokio::test]
    async fn tool_calls_and_results_with_image_relocation() {
        let png = media_ref(1, "image/png", Some("chart.png"));
        let csv = media_ref(2, "text/csv", Some("data.csv"));
        let media = Media(HashMap::from([(
            png.id.clone(),
            Bytes::from_static(b"png"),
        )]));
        let messages = vec![
            message(Role::User, vec![Block::text("chart it")]),
            message(
                Role::Assistant,
                vec![
                    Block::text("Fetching."),
                    tool_use("call_a", "files__read", json!({"path": "/x"})),
                    tool_use("call_b", "chart", json!({})),
                ],
            ),
            message(
                Role::User,
                vec![tool_result(
                    "call_a",
                    vec![Block::Document {
                        media: csv.clone(),
                        name: None,
                    }],
                    false,
                )],
            ),
            // A steering message stored in the same run: it follows the tool messages.
            message(
                Role::User,
                vec![
                    Block::text("make it blue"),
                    tool_result(
                        "call_b",
                        vec![Block::text("done"), Block::Image { media: png.clone() }],
                        false,
                    ),
                ],
            ),
            message(
                Role::Assistant,
                vec![tool_use("call_c", "chart", json!({"color": "blue"}))],
            ),
            message(Role::User, vec![tool_result("call_c", vec![], true)]),
        ];
        let body = body(request("m", messages), &config(Dialect::OpenAi), &media)
            .await
            .unwrap();
        let png_ref = format!("[media:{} chart.png image/png]", png.id);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": "chart it"},
                {"role": "assistant", "content": "Fetching.", "tool_calls": [
                    {"id": "call_a", "type": "function",
                     "function": {"name": "files__read", "arguments": "{\"path\":\"/x\"}"}},
                    {"id": "call_b", "type": "function",
                     "function": {"name": "chart", "arguments": "{}"}},
                ]},
                {"role": "tool", "tool_call_id": "call_a",
                 "content": format!("[media:{} data.csv text/csv]", csv.id)},
                {"role": "tool", "tool_call_id": "call_b",
                 "content": format!("done\n\n{png_ref}")},
                {"role": "user", "content": [
                    {"type": "text", "text": png_ref},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
                ]},
                {"role": "user", "content": "make it blue"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_c", "type": "function",
                     "function": {"name": "chart", "arguments": "{\"color\":\"blue\"}"}},
                ]},
                {"role": "tool", "tool_call_id": "call_c", "content": "Error"},
            ])
        );
    }

    #[tokio::test]
    async fn error_results_are_prefixed() {
        let messages = vec![message(
            Role::User,
            vec![tool_result("t", vec![Block::text("no such file")], true)],
        )];
        assert_eq!(
            build(request("m", messages)).await["messages"][0]["content"],
            "Error: no such file"
        );
    }

    #[tokio::test]
    async fn media_parts_by_type_and_declared_types() {
        let png = media_ref(1, "IMAGE/PNG", None);
        let wav = media_ref(2, "audio/wav", Some("memo.wav"));
        let mp3 = media_ref(3, "audio/mpeg", None);
        let pdf = media_ref(4, "application/pdf", Some("q3.pdf"));
        let unnamed_pdf = media_ref(5, "application/pdf", None);
        let ogg = media_ref(6, "audio/ogg; codecs=opus", Some("voice.ogg"));
        let media = Media(HashMap::from([
            (png.id.clone(), Bytes::from_static(b"png")),
            (wav.id.clone(), Bytes::from_static(b"wav")),
            (mp3.id.clone(), Bytes::from_static(b"mp3")),
            (pdf.id.clone(), Bytes::from_static(b"pdf")),
            (unnamed_pdf.id.clone(), Bytes::from_static(b"pdf")),
        ]));
        let content = vec![
            Block::Image { media: png.clone() },
            Block::Audio { media: wav.clone() },
            Block::Audio { media: mp3.clone() },
            Block::Document {
                media: pdf.clone(),
                name: Some("Q3 report".into()),
            },
            Block::Document {
                media: unnamed_pdf.clone(),
                name: None,
            },
            Block::Audio { media: ogg.clone() },
        ];
        let req = request("m", vec![message(Role::User, content)]);

        let mut config = config(Dialect::Generic);
        config.models.insert(
            "m".into(),
            ModelSettings {
                media_types: Some(
                    ["image/png", "audio/wav", "audio/mpeg", "application/pdf"]
                        .map(String::from)
                        .to_vec(),
                ),
                ..ModelSettings::default()
            },
        );
        let sent = body(req.clone(), &config, &media).await.unwrap();
        assert_eq!(
            sent["messages"][0]["content"],
            json!([
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,cG5n"}},
                {"type": "input_audio", "input_audio": {"data": "d2F2", "format": "wav"}},
                {"type": "input_audio", "input_audio": {"data": "bXAz", "format": "mp3"}},
                {"type": "file", "file": {"filename": "Q3 report",
                                          "file_data": "data:application/pdf;base64,cGRm"}},
                {"type": "file", "file": {"filename": format!("{}.pdf", unnamed_pdf.id),
                                          "file_data": "data:application/pdf;base64,cGRm"}},
                {"type": "text", "text": format!("[media:{} voice.ogg audio/ogg; codecs=opus]",
                                                 ogg.id)},
            ])
        );

        // A generic server with nothing declared gets references only, and reads no bytes.
        let sent = body(
            req,
            &OpenAiConfig {
                models: Default::default(),
                ..config
            },
            &Media::default(),
        )
        .await
        .unwrap();
        let content = sent["messages"][0]["content"].as_array().unwrap();
        assert!(
            content.iter().all(|part| part["type"] == "text"),
            "{content:?}"
        );
        assert_eq!(content[0]["text"], format!("[media:{} IMAGE/PNG]", png.id));
    }

    #[tokio::test]
    async fn media_read_errors_propagate() {
        let messages = vec![message(
            Role::User,
            vec![Block::Image {
                media: media_ref(9, "image/png", None),
            }],
        )];
        assert!(matches!(
            build_error(request("m", messages)).await,
            Error::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn own_reasoning_goes_back_in_its_field_and_foreign_data_is_dropped() {
        let openai = |field: &str| Opaque {
            api: "openai".into(),
            data: json!({"field": field}),
        };
        let messages = vec![
            message(Role::User, vec![Block::text("q")]),
            message(
                Role::Assistant,
                vec![
                    Block::Reasoning {
                        text: Some("First, ".into()),
                        opaque: Some(openai("reasoning_content")),
                    },
                    Block::Reasoning {
                        text: Some("then.".into()),
                        opaque: Some(openai("reasoning_content")),
                    },
                    // Not sent: unknown field, another API's reasoning, unmarked reasoning, and
                    // provider data.
                    Block::Reasoning {
                        text: Some("x".into()),
                        opaque: Some(openai("system")),
                    },
                    Block::Reasoning {
                        text: Some("thinking".into()),
                        opaque: Some(Opaque {
                            api: "anthropic".into(),
                            data: json!({"signature": "sig"}),
                        }),
                    },
                    Block::Reasoning {
                        text: Some("plain".into()),
                        opaque: None,
                    },
                    Block::Opaque(Opaque {
                        api: "anthropic".into(),
                        data: json!({"type": "redacted_thinking", "data": "x"}),
                    }),
                    Block::Opaque(openai("reasoning")),
                    tool_use("call_1", "now", json!({})),
                ],
            ),
            // Only foreign data: dropped entirely.
            message(
                Role::Assistant,
                vec![Block::Reasoning {
                    text: Some("only".into()),
                    opaque: Some(openai("reasoning")),
                }],
            ),
        ];
        let req = request("m", messages);
        for dialect in [Dialect::Generic, Dialect::OpenRouter] {
            assert_eq!(
                build_with(req.clone(), &config(dialect)).await["messages"],
                json!([
                    {"role": "user", "content": "q"},
                    {"role": "assistant", "content": null, "reasoning_content": "First, then.",
                     "tool_calls": [{"id": "call_1", "type": "function",
                                     "function": {"name": "now", "arguments": "{}"}}]},
                ]),
                "{dialect:?}"
            );
        }
        // OpenAI itself never streams reasoning, so reasoning from another instance (after a
        // fallback) isn't sent to it.
        assert_eq!(
            build(req).await["messages"],
            json!([
                {"role": "user", "content": "q"},
                {"role": "assistant", "content": null,
                 "tool_calls": [{"id": "call_1", "type": "function",
                                 "function": {"name": "now", "arguments": "{}"}}]},
            ])
        );
    }

    #[tokio::test]
    async fn misplaced_blocks_are_rejected() {
        let tool_use_in_user = vec![message(Role::User, vec![tool_use("t", "n", json!({}))])];
        let result_in_assistant = vec![message(
            Role::Assistant,
            vec![tool_result("t", vec![], false)],
        )];
        let nested_tool_use = vec![message(
            Role::User,
            vec![tool_result("t", vec![tool_use("x", "y", json!({}))], false)],
        )];
        for (messages, expected) in [
            (tool_use_in_user, "user message"),
            (result_in_assistant, "assistant message"),
            (nested_tool_use, "tool result"),
        ] {
            let error = build_error(request("m", messages)).await;
            assert!(
                matches!(&error, Error::InvalidRequest(m) if m.contains(expected)),
                "{error:?}"
            );
        }
    }

    #[tokio::test]
    async fn tools_are_functions() {
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
                {"type": "function", "function": {"name": "github__create_issue",
                 "description": "Create an issue", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "memory",
                 "parameters": {"type": "object", "properties": {}}}},
            ])
        );
    }

    #[tokio::test]
    async fn max_tokens_field_and_precedence() {
        let mut config = config(Dialect::OpenAi);
        let mut req = request("gpt-5.1", hi());
        assert!(
            build_with(req.clone(), &config)
                .await
                .get("max_completion_tokens")
                .is_none()
        );
        config.defaults.max_tokens = Some(8192);
        assert_eq!(
            build_with(req.clone(), &config).await["max_completion_tokens"],
            8192
        );
        config.models.insert(
            "gpt-5.1".into(),
            ModelSettings {
                max_tokens: Some(4096),
                ..ModelSettings::default()
            },
        );
        assert_eq!(
            build_with(req.clone(), &config).await["max_completion_tokens"],
            4096
        );
        req.params.max_tokens = Some(100);
        let body = build_with(req.clone(), &config).await;
        assert_eq!(body["max_completion_tokens"], 100);
        assert!(body.get("max_tokens").is_none());

        for dialect in [Dialect::OpenRouter, Dialect::Generic] {
            config.dialect = dialect;
            let body = build_with(req.clone(), &config).await;
            assert_eq!(body["max_tokens"], 100, "{dialect:?}");
            assert!(body.get("max_completion_tokens").is_none());
        }
    }

    #[tokio::test]
    async fn reasoning_and_temperature_by_dialect() {
        let mut req = request("m", hi());
        req.params.reasoning_budget = Some(4096);
        req.params.temperature = Some(0.7);

        // Undeclared models get no reasoning parameter, except on OpenRouter.
        for dialect in [Dialect::OpenAi, Dialect::Generic] {
            let body = build_with(req.clone(), &config(dialect)).await;
            assert!(body.get("reasoning_effort").is_none(), "{dialect:?}");
            assert!(body.get("reasoning").is_none());
            assert_eq!(body["temperature"], json!(0.7));
        }
        let body = build_with(req.clone(), &config(Dialect::OpenRouter)).await;
        assert_eq!(body["reasoning"], json!({"max_tokens": 4096}));
        assert_eq!(body["temperature"], json!(0.7));

        // Declared reasoning models get an effort; OpenAI's take no temperature.
        let mut openai = config(Dialect::OpenAi);
        openai.defaults.reasoning = Some(true);
        let body = build_with(req.clone(), &openai).await;
        assert_eq!(body["reasoning_effort"], "medium");
        assert!(body.get("temperature").is_none());
        let mut generic = config(Dialect::Generic);
        generic.defaults.reasoning = Some(true);
        let body = build_with(req.clone(), &generic).await;
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["temperature"], json!(0.7));

        // No budget: nothing, leaving the model's default.
        req.params.reasoning_budget = None;
        let body = build_with(req.clone(), &openai).await;
        assert!(body.get("reasoning_effort").is_none());
        let body = build_with(req.clone(), &config(Dialect::OpenRouter)).await;
        assert!(body.get("reasoning").is_none());

        req.params.temperature = Some(f32::NAN);
        let error = super::body(req, &config(Dialect::Generic), &Media::default())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(m) if m.contains("temperature")));
    }

    #[test]
    fn effort_levels() {
        assert_eq!(effort(1), "low");
        assert_eq!(effort(2048), "low");
        assert_eq!(effort(2049), "medium");
        assert_eq!(effort(8192), "medium");
        assert_eq!(effort(8193), "high");
        assert_eq!(effort(u32::MAX), "high");
    }

    #[tokio::test]
    async fn openrouter_asks_for_automatic_caching() {
        let mut req = request("anthropic/claude-sonnet-4.5", hi());
        let openrouter = config(Dialect::OpenRouter);
        assert_eq!(
            build_with(req.clone(), &openrouter).await["cache_control"],
            json!({"type": "ephemeral"})
        );
        req.params.cache_retention = CacheRetention::Long;
        assert_eq!(
            build_with(req.clone(), &openrouter).await["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        for dialect in [Dialect::OpenAi, Dialect::Generic] {
            assert!(
                build_with(req.clone(), &config(dialect))
                    .await
                    .get("cache_control")
                    .is_none()
            );
        }
    }
}
