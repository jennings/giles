//! Pure mappings between `rmcp` types and `giles-types`: tool specs, tool results, progress, and
//! errors.

use giles_types::{Block, Error, ToolCallEvent, ToolSpec};
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorCode, ErrorData, ProgressNotificationParam,
    ResourceContents, Tool,
};
use rmcp::service::{ClientInitializeError, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpError;

use crate::config::{Secret, scrub};

/// Where an error message comes from: the instance it names, and the secrets it must not show.
/// Server error messages, transport errors (an HTTP error carries the response body), and stderr
/// can all echo a credential back.
#[derive(Debug)]
pub(crate) struct Origin {
    pub(crate) instance: String,
    pub(crate) secrets: Vec<Secret>,
}

impl Origin {
    /// `MCP server <instance>: <text>`, with the secrets scrubbed.
    pub(crate) fn message(&self, text: impl std::fmt::Display) -> String {
        self.scrub(&format!("MCP server {}: {text}", self.instance))
    }

    /// `text` with the secrets replaced by `<redacted>`.
    pub(crate) fn scrub(&self, text: &str) -> String {
        scrub(text, &self.secrets.iter().collect::<Vec<_>>())
    }
}

/// A tool as the model sees it: name, description (the title if the server gave no description),
/// and input schema.
pub(crate) fn tool_spec(tool: Tool) -> ToolSpec {
    let description = match (tool.description, tool.title) {
        (Some(description), _) => description.into_owned(),
        (None, Some(title)) => title,
        (None, None) => String::new(),
    };
    ToolSpec {
        name: tool.name.into_owned(),
        description,
        input_schema: serde_json::Value::Object(std::sync::Arc::unwrap_or_clone(tool.input_schema)),
    }
}

/// The tools of a whole `tools/list`, sorted by name so the prompt prefix stays stable (PLAN
/// section 6.5).
pub(crate) fn tool_specs(tools: Vec<Tool>) -> Vec<ToolSpec> {
    let mut specs: Vec<ToolSpec> = tools.into_iter().map(tool_spec).collect();
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

/// A `tools/call` result as a [`ToolCallEvent::Result`].
///
/// Text content becomes [`Block::Text`], and so does the text of an embedded text resource. Images,
/// audio, and blob resources are replaced with a text placeholder saying what was dropped: storing
/// them as blobs is task 2.21. A resource link becomes a text line with its URI. If the result has
/// no text at all but has `structuredContent`, that JSON is the text.
pub(crate) fn tool_result(result: CallToolResult) -> ToolCallEvent {
    let has_text = result
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text(text) if !text.text.is_empty()));
    let mut content: Vec<Block> = result.content.into_iter().map(content_block).collect();
    if !has_text && let Some(structured) = result.structured_content {
        content.push(Block::Text {
            text: structured.to_string(),
        });
    }
    ToolCallEvent::Result {
        content,
        is_error: result.is_error.unwrap_or(false),
    }
}

fn content_block(block: ContentBlock) -> Block {
    let text = match block {
        ContentBlock::Text(text) => text.text,
        ContentBlock::Image(image) => dropped("image", &image.mime_type, decoded_len(&image.data)),
        ContentBlock::Audio(audio) => dropped("audio", &audio.mime_type, decoded_len(&audio.data)),
        ContentBlock::Resource(resource) => match resource.resource {
            ResourceContents::TextResourceContents { text, .. } => text,
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => format!(
                "[resource {uri} ({}, {} bytes) dropped: Giles doesn't take media from MCP tools \
                 yet]",
                mime_type.as_deref().unwrap_or("application/octet-stream"),
                decoded_len(&blob),
            ),
            _ => "[unsupported MCP resource content dropped]".to_owned(),
        },
        ContentBlock::ResourceLink(link) => format!("[resource link: {}]", link.uri),
        // Content kinds newer than this mapping: say so rather than lose them silently.
        other => format!(
            "[unsupported MCP content dropped: {}]",
            serde_json::to_value(&other)
                .ok()
                .and_then(|value| value
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned())
        ),
    };
    Block::Text { text }
}

fn dropped(kind: &str, mime_type: &str, bytes: u64) -> String {
    format!(
        "[{kind} ({mime_type}, {bytes} bytes) dropped: Giles doesn't take media from MCP tools yet]"
    )
}

/// The decoded size of base64 `data`, without decoding it. Whitespace isn't expected (MCP sends
/// standard base64 without line breaks); padding is accounted for.
fn decoded_len(data: &str) -> u64 {
    let data = data.trim_end_matches('=');
    (data.len() as u64 * 3) / 4
}

/// A progress notification as a [`ToolCallEvent::Progress`], or `None` if it doesn't advance past
/// `last` (the progress of the last event forwarded). MCP requires progress to increase with each
/// notification; `rmcp` dispatches notifications on separate tasks, so they can arrive out of
/// order, and a stale one is dropped rather than forwarded backwards.
pub(crate) fn progress_event(
    param: ProgressNotificationParam,
    last: &mut Option<f64>,
) -> Option<ToolCallEvent> {
    if !param.progress.is_finite() || last.is_some_and(|last| param.progress <= last) {
        return None;
    }
    *last = Some(param.progress);
    Some(ToolCallEvent::Progress {
        progress: param.progress,
        total: param.total.filter(|total| total.is_finite()),
        message: param.message,
    })
}

/// Maps an `rmcp` request failure. The connection being gone (the server exited, the transport
/// failed) is [`Error::Unavailable`]; a JSON-RPC error is [`Error::InvalidRequest`] for the
/// request-shape codes and [`Error::Other`] otherwise, with the server's message.
pub(crate) fn service_error(origin: &Origin, error: ServiceError) -> Error {
    match error {
        ServiceError::McpError(error) => rpc_error(origin, error),
        ServiceError::TransportClosed => {
            Error::unavailable(origin.message("the connection is closed"))
        }
        ServiceError::TransportSend(error) => {
            Error::unavailable(origin.message(format_args!("sending failed: {error}")))
        }
        ServiceError::Timeout { timeout } => {
            Error::unavailable(origin.message(format_args!("no response within {timeout:?}")))
        }
        other => Error::other(origin.message(other)),
    }
}

/// Maps a JSON-RPC error response.
pub(crate) fn rpc_error(origin: &Origin, error: ErrorData) -> Error {
    let message = origin.message(&error.message);
    match error.code {
        ErrorCode::INVALID_REQUEST | ErrorCode::METHOD_NOT_FOUND | ErrorCode::INVALID_PARAMS => {
            Error::InvalidRequest(message)
        }
        _ => Error::other(message),
    }
}

/// Maps a failed `initialize` handshake. Rejected credentials (HTTP 401 or 403) and an unsupported
/// protocol version are [`Error::InvalidRequest`] (retrying won't help); the server being
/// unreachable or closing the connection is [`Error::Unavailable`].
pub(crate) fn init_error(origin: &Origin, error: ClientInitializeError) -> Error {
    if credentials_rejected(&error) {
        return Error::InvalidRequest(origin.scrub(&format!(
            "MCP server {} rejected the credentials: {error}",
            origin.instance
        )));
    }
    match error {
        ClientInitializeError::JsonRpcError(error) => rpc_error(origin, error),
        ClientInitializeError::ConnectionClosed(_)
        | ClientInitializeError::TransportError { .. }
        | ClientInitializeError::Cancelled => {
            Error::unavailable(origin.message(format_args!("initialization failed: {error}")))
        }
        ClientInitializeError::NoCompatibleProtocolVersion { .. } => {
            Error::InvalidRequest(origin.message(error))
        }
        other => Error::other(origin.message(format_args!("initialization failed: {other}"))),
    }
}

/// Whether the handshake failed on HTTP 401 or 403. `rmcp` classifies these only when the server
/// sends `WWW-Authenticate`; without it, it reports an unexpected response whose message starts
/// with the status (`HTTP 401 Unauthorized: <body>`). `tests/mcp_http.rs` pins both shapes.
fn credentials_rejected(error: &ClientInitializeError) -> bool {
    if error.is_authorization_required() || error.auth_challenge().is_some() {
        return true;
    }
    let ClientInitializeError::TransportError { error, .. } = error else {
        return false;
    };
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error.error.as_ref());
    while let Some(current) = source {
        if let Some(StreamableHttpError::UnexpectedServerResponse(message)) =
            current.downcast_ref::<StreamableHttpError<reqwest::Error>>()
            && (message.starts_with("HTTP 401 ") || message.starts_with("HTTP 403 "))
        {
            return true;
        }
        source = current.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use rmcp::model::{EmbeddedResource, ProgressToken, Resource};
    use serde_json::json;

    use super::*;

    fn text(text: &str) -> Block {
        Block::Text {
            text: text.to_owned(),
        }
    }

    fn result_of(event: ToolCallEvent) -> (Vec<Block>, bool) {
        match event {
            ToolCallEvent::Result { content, is_error } => (content, is_error),
            other => panic!("not a result: {other:?}"),
        }
    }

    #[test]
    fn tool_spec_keeps_name_description_and_schema() {
        let schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        let tool = Tool::new(
            "search",
            "Searches things",
            serde_json::from_value::<rmcp::model::JsonObject>(schema.clone()).unwrap(),
        );
        assert_eq!(
            tool_spec(tool),
            ToolSpec {
                name: "search".to_owned(),
                description: "Searches things".to_owned(),
                input_schema: schema,
            }
        );
    }

    #[test]
    fn tool_spec_falls_back_to_the_title_then_empty() {
        let mut tool = Tool::new("a", "x", rmcp::model::JsonObject::new());
        tool.description = None;
        tool.title = Some("Title A".to_owned());
        assert_eq!(tool_spec(tool.clone()).description, "Title A");
        tool.title = None;
        assert_eq!(tool_spec(tool).description, "");
    }

    #[test]
    fn tool_specs_are_sorted_by_name() {
        let tools = ["b", "c", "a"]
            .map(|name| Tool::new(name, "", rmcp::model::JsonObject::new()))
            .to_vec();
        let names: Vec<String> = tool_specs(tools)
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn text_content_becomes_text_blocks_in_order() {
        let result =
            CallToolResult::success(vec![ContentBlock::text("one"), ContentBlock::text("two")]);
        assert_eq!(
            result_of(tool_result(result)),
            (vec![text("one"), text("two")], false)
        );
    }

    #[test]
    fn is_error_is_carried_and_defaults_to_false() {
        let result = CallToolResult::error(vec![ContentBlock::text("boom")]);
        assert_eq!(result_of(tool_result(result)), (vec![text("boom")], true));
        let mut result = CallToolResult::success(vec![]);
        result.is_error = None;
        assert!(!result_of(tool_result(result)).1);
    }

    #[test]
    fn media_is_replaced_with_a_placeholder_naming_what_was_dropped() {
        // 8 base64 characters with one padding character decode to 5 bytes.
        let result = CallToolResult::success(vec![
            ContentBlock::image("aGVsbG8=", "image/png"),
            ContentBlock::audio("aGVsbG8h", "audio/wav"),
            ContentBlock::resource(ResourceContents::BlobResourceContents {
                uri: "file:///a.pdf".to_owned(),
                mime_type: Some("application/pdf".to_owned()),
                blob: "aGk=".to_owned(),
                meta: None,
            }),
        ]);
        let (content, _) = result_of(tool_result(result));
        let texts: Vec<&str> = content
            .iter()
            .map(|block| match block {
                Block::Text { text } => text.as_str(),
                other => panic!("not text: {other:?}"),
            })
            .collect();
        assert!(
            texts[0].starts_with("[image (image/png, 5 bytes) dropped"),
            "{}",
            texts[0]
        );
        assert!(
            texts[1].starts_with("[audio (audio/wav, 6 bytes) dropped"),
            "{}",
            texts[1]
        );
        assert!(
            texts[2].starts_with("[resource file:///a.pdf (application/pdf, 2 bytes) dropped"),
            "{}",
            texts[2]
        );
    }

    #[test]
    fn text_resources_and_links_become_text() {
        let link = Resource::new("file:///notes.md", "notes");
        let result = CallToolResult::success(vec![
            ContentBlock::Resource(EmbeddedResource::new(ResourceContents::text(
                "the notes",
                "file:///notes.md",
            ))),
            ContentBlock::ResourceLink(link),
        ]);
        assert_eq!(
            result_of(tool_result(result)).0,
            vec![text("the notes"), text("[resource link: file:///notes.md]")]
        );
    }

    #[test]
    fn structured_content_is_the_text_only_when_there_is_no_text() {
        let structured = json!({"temperature": 21});
        let result = CallToolResult::structured(structured.clone());
        // `structured` also adds the JSON as text content, as the spec recommends.
        assert_eq!(
            result_of(tool_result(result)).0,
            vec![text(&structured.to_string())]
        );

        let mut only_structured = CallToolResult::success(vec![]);
        only_structured.structured_content = Some(structured.clone());
        assert_eq!(
            result_of(tool_result(only_structured)).0,
            vec![text(r#"{"temperature":21}"#)]
        );

        let mut with_image = CallToolResult::success(vec![ContentBlock::image("", "image/png")]);
        with_image.structured_content = Some(structured);
        let (content, _) = result_of(tool_result(with_image));
        assert_eq!(content.len(), 2);
        assert_eq!(content[1], text(r#"{"temperature":21}"#));
    }

    fn progress(value: f64) -> ProgressNotificationParam {
        ProgressNotificationParam::new(ProgressToken(rmcp::model::NumberOrString::Number(1)), value)
    }

    #[test]
    fn progress_only_moves_forward() {
        let mut last = None;
        let first = progress(1.0).with_total(4.0).with_message("step 1");
        assert_eq!(
            progress_event(first, &mut last),
            Some(ToolCallEvent::Progress {
                progress: 1.0,
                total: Some(4.0),
                message: Some("step 1".to_owned()),
            })
        );
        assert_eq!(progress_event(progress(1.0), &mut last), None);
        assert_eq!(progress_event(progress(0.5), &mut last), None);
        assert_eq!(progress_event(progress(f64::NAN), &mut last), None);
        assert!(progress_event(progress(2.0), &mut last).is_some());
        assert_eq!(last, Some(2.0));
    }

    #[test]
    fn rpc_errors_map_by_code_and_keep_the_message() {
        let error = rpc_error(&gh(), ErrorData::invalid_params("Unknown tool: nope", None));
        assert!(
            matches!(&error, Error::InvalidRequest(message) if message == "MCP server gh: Unknown tool: nope"),
            "{error:?}"
        );
        let error = rpc_error(
            &gh(),
            ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                Cow::Borrowed("tools/call"),
                None,
            ),
        );
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
        let error = rpc_error(&gh(), ErrorData::internal_error("database down", None));
        assert!(matches!(&error, Error::Other(_)), "{error:?}");
        assert_eq!(error.to_string(), "MCP server gh: database down");
    }

    #[test]
    fn a_closed_connection_is_unavailable() {
        let error = service_error(&gh(), ServiceError::TransportClosed);
        assert!(error.is_retryable(), "{error:?}");
        let error = service_error(
            &gh(),
            ServiceError::McpError(ErrorData::internal_error("x", None)),
        );
        assert!(!error.is_retryable(), "{error:?}");
    }

    fn gh() -> Origin {
        Origin {
            instance: "gh".to_owned(),
            secrets: vec![Secret::new("ghp_secret_token")],
        }
    }

    #[test]
    fn messages_are_scrubbed() {
        let error = rpc_error(
            &gh(),
            ErrorData::invalid_params("bad token ghp_secret_token", None),
        );
        assert_eq!(
            error.to_string(),
            "invalid request: MCP server gh: bad token <redacted>"
        );
        let error = service_error(
            &gh(),
            ServiceError::Cancelled {
                reason: Some("ghp_secret_token".to_owned()),
            },
        );
        assert!(!error.to_string().contains("ghp_secret_token"), "{error}");
    }
}
