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

/// One item of a `tools/call` result: a block as it is, or media still to be stored
/// ([`super::media::store_result`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Part {
    /// Text, or text standing in for content Giles doesn't take.
    Block(Block),
    /// Image, audio, or blob resource content.
    Media(IncomingMedia),
}

/// Media in a tool result, as the server sent it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IncomingMedia {
    /// What MCP called it, for messages: `image`, `audio`, or `resource <uri>`.
    pub(crate) what: String,
    /// The media type ([`media_type`]).
    pub(crate) media_type: String,
    /// The file name: the last segment of a resource's URI.
    pub(crate) name: Option<String>,
    /// The content, base64.
    pub(crate) data: String,
}

impl IncomingMedia {
    /// The decoded size of the content, without decoding it. Whitespace isn't expected (MCP
    /// sends standard base64 without line breaks); padding is accounted for.
    pub(crate) fn decoded_len(&self) -> u64 {
        let data = self.data.trim_end_matches('=');
        (data.len() as u64 * 3) / 4
    }

    /// The text that takes the media's place when it can't be stored, e.g. `[image (image/png,
    /// 5120 bytes) dropped: <why>]`.
    pub(crate) fn dropped(&self, why: impl std::fmt::Display) -> Block {
        Block::text(format!(
            "[{} ({}, {} bytes) dropped: {why}]",
            self.what,
            self.media_type,
            self.decoded_len()
        ))
    }
}

/// A `tools/call` result, before its media is stored.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolResult {
    /// The content, in order.
    pub(crate) parts: Vec<Part>,
    /// Whether the tool failed by its own account (`isError`, default false).
    pub(crate) is_error: bool,
}

/// A `tools/call` result's content.
///
/// Text content becomes [`Block::Text`], and so does the text of an embedded text resource.
/// Images, audio, and blob resources become [`Part::Media`]. A resource link becomes a text line
/// with its URI. If the result has no text at all but has `structuredContent`, that JSON is the
/// text.
pub(crate) fn tool_result(result: CallToolResult) -> ToolResult {
    let has_text = result
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Text(text) if !text.text.is_empty()));
    let mut parts: Vec<Part> = result.content.into_iter().map(content_part).collect();
    if !has_text && let Some(structured) = result.structured_content {
        parts.push(Part::Block(Block::Text {
            text: structured.to_string(),
        }));
    }
    ToolResult {
        parts,
        is_error: result.is_error.unwrap_or(false),
    }
}

fn content_part(block: ContentBlock) -> Part {
    let media = |what: String, mime_type: Option<&str>, name, data| {
        Part::Media(IncomingMedia {
            what,
            media_type: media_type(mime_type.unwrap_or_default()),
            name,
            data,
        })
    };
    let text = match block {
        ContentBlock::Text(text) => text.text,
        ContentBlock::Image(image) => {
            return media("image".into(), Some(&image.mime_type), None, image.data);
        }
        ContentBlock::Audio(audio) => {
            return media("audio".into(), Some(&audio.mime_type), None, audio.data);
        }
        ContentBlock::Resource(resource) => match resource.resource {
            ResourceContents::TextResourceContents { text, .. } => text,
            ResourceContents::BlobResourceContents {
                uri,
                mime_type,
                blob,
                ..
            } => {
                let name = resource_name(&uri);
                return media(format!("resource {uri}"), mime_type.as_deref(), name, blob);
            }
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
    Part::Block(Block::Text { text })
}

/// The longest media type kept; a longer one is `application/octet-stream`.
const MAX_MEDIA_TYPE_LEN: usize = 127;

/// The media type a server gave, as the blob store records it: lowercase, without parameters
/// (`audio/ogg; codecs=opus` is `audio/ogg`), and `application/octet-stream` unless it is a
/// plain `type/subtype` of RFC 6838 characters, so a server can't put anything else where
/// channels and providers read a media type.
pub(crate) fn media_type(raw: &str) -> String {
    const FALLBACK: &str = "application/octet-stream";
    let essence = raw.split(';').next().unwrap_or_default().trim();
    let token = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
    };
    match essence.split_once('/') {
        Some((kind, sub)) if essence.len() <= MAX_MEDIA_TYPE_LEN && token(kind) && token(sub) => {
            essence.to_ascii_lowercase()
        }
        _ => FALLBACK.to_owned(),
    }
}

/// The longest file name taken from a resource URI, in characters.
const MAX_NAME_CHARS: usize = 255;

/// A resource's file name: the last path segment of its URI, without query or fragment, if it
/// has one of at most 255 characters.
fn resource_name(uri: &str) -> Option<String> {
    let path = uri.split(['?', '#']).next().unwrap_or_default();
    let last = path.rsplit('/').next().unwrap_or_default();
    let name = if path.contains('/') { last } else { "" };
    (!name.is_empty() && name.chars().count() <= MAX_NAME_CHARS).then(|| name.to_owned())
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

    /// The content with each media part as `<media <what>>`.
    fn result_of(result: ToolResult) -> (Vec<Block>, bool) {
        let content = result
            .parts
            .into_iter()
            .map(|part| match part {
                Part::Block(block) => block,
                Part::Media(media) => text(&format!("<media {}>", media.what)),
            })
            .collect();
        (content, result.is_error)
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
    fn media_is_kept_with_its_type_name_and_data() {
        let result = CallToolResult::success(vec![
            ContentBlock::image("aGVsbG8=", "image/png"),
            ContentBlock::audio("aGVsbG8h", "Audio/OGG; codecs=opus"),
            ContentBlock::resource(ResourceContents::BlobResourceContents {
                uri: "file:///reports/q3.pdf?v=2".to_owned(),
                mime_type: Some("application/pdf".to_owned()),
                blob: "aGk=".to_owned(),
                meta: None,
            }),
            ContentBlock::resource(ResourceContents::BlobResourceContents {
                uri: "urn:x".to_owned(),
                mime_type: None,
                blob: String::new(),
                meta: None,
            }),
        ]);
        let media: Vec<IncomingMedia> = tool_result(result)
            .parts
            .into_iter()
            .map(|part| match part {
                Part::Media(media) => media,
                other => panic!("not media: {other:?}"),
            })
            .collect();
        let incoming =
            |what: &str, media_type: &str, name: Option<&str>, data: &str| IncomingMedia {
                what: what.to_owned(),
                media_type: media_type.to_owned(),
                name: name.map(str::to_owned),
                data: data.to_owned(),
            };
        assert_eq!(
            media,
            [
                incoming("image", "image/png", None, "aGVsbG8="),
                incoming("audio", "audio/ogg", None, "aGVsbG8h"),
                incoming(
                    "resource file:///reports/q3.pdf?v=2",
                    "application/pdf",
                    Some("q3.pdf"),
                    "aGk="
                ),
                incoming("resource urn:x", "application/octet-stream", None, ""),
            ]
        );
        // 8 base64 characters with one padding character decode to 5 bytes.
        assert_eq!(media[0].decoded_len(), 5);
        assert_eq!(media[1].decoded_len(), 6);
        assert_eq!(
            media[0].dropped("too large"),
            text("[image (image/png, 5 bytes) dropped: too large]")
        );
    }

    #[test]
    fn media_types_are_plain_type_slash_subtype_or_octet_stream() {
        assert_eq!(media_type("IMAGE/PNG"), "image/png");
        assert_eq!(media_type(" text/plain ; charset=utf-8"), "text/plain");
        assert_eq!(media_type("image/svg+xml"), "image/svg+xml");
        for bad in [
            "",
            "png",
            "image/",
            "/png",
            "image/png/x",
            "image/p ng",
            "image/png\n[media:x]",
            &format!("image/{}", "x".repeat(130)),
        ] {
            assert_eq!(media_type(bad), "application/octet-stream", "{bad:?}");
        }
    }

    #[test]
    fn resource_names_are_the_last_path_segment() {
        assert_eq!(resource_name("file:///a/b.txt").as_deref(), Some("b.txt"));
        assert_eq!(
            resource_name("https://x/y.png#top").as_deref(),
            Some("y.png")
        );
        assert_eq!(resource_name("file:///dir/"), None);
        assert_eq!(resource_name("urn:isbn:1"), None);
        assert_eq!(resource_name(&format!("file:///{}", "n".repeat(256))), None);
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
