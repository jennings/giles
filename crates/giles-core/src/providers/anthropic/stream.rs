//! Messages API stream events to [`CompletionEvent`]s.

use giles_types::{Block, BlockKind, CompletionEvent, Error, Opaque, Result, StopReason, Usage};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::API;
use crate::providers::sse::SseEvent;
use crate::providers::wire::EventMapper;

/// Turns the events of one streamed response into [`CompletionEvent`]s, one SSE event at a time.
///
/// It enforces the `CompletionEvent` contract rather than trusting the stream: blocks may not
/// overlap and deltas must belong to the open block (else [`Error::Other`]), indexes are
/// renumbered from 0, and `Stop` comes only from `message_stop`.
#[derive(Debug, Default)]
pub(crate) struct StreamMapper {
    next_index: u32,
    open: Option<OpenBlock>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    finished: bool,
}

#[derive(Debug)]
struct OpenBlock {
    /// The API's index, which deltas and the stop event refer to.
    api_index: u64,
    /// Our index.
    index: u32,
    state: BlockState,
}

#[derive(Debug)]
enum BlockState {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        /// The input from `content_block_start`, used if no input deltas arrive.
        input: Value,
        json: String,
    },
    Thinking {
        text: String,
        signature: String,
    },
    /// Any other block (server tool use and results, redacted thinking, blocks added later),
    /// kept whole. `json` collects `input_json_delta`s for blocks that stream an `input`.
    Opaque {
        block: Map<String, Value>,
        json: String,
    },
}

#[derive(Deserialize)]
struct MessageStart {
    message: StartMessage,
}

#[derive(Deserialize)]
struct StartMessage {
    #[serde(default)]
    usage: Option<WireUsage>,
}

/// Usage fields; a missing or null field leaves the running total unchanged.
#[derive(Deserialize, Default)]
struct WireUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct BlockStart {
    index: u64,
    content_block: Map<String, Value>,
}

#[derive(Deserialize)]
struct BlockDelta {
    index: u64,
    delta: Map<String, Value>,
}

#[derive(Deserialize)]
struct BlockStop {
    index: u64,
}

#[derive(Deserialize)]
struct MessageDelta {
    delta: MessageDeltaBody,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct MessageDeltaBody {
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct ErrorEvent {
    error: ApiError,
}

/// The `error` object of an error event or an error response body.
#[derive(Deserialize)]
pub(crate) struct ApiError {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub message: String,
}

impl StreamMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }
}

impl EventMapper for StreamMapper {
    /// Maps one SSE event. An `error` event, or a malformed or out-of-order event, is an `Err`
    /// that ends the response.
    fn event(&mut self, event: &SseEvent) -> Result<Vec<CompletionEvent>> {
        if self.finished {
            return Ok(Vec::new());
        }
        match event.event.as_str() {
            "message_start" => {
                let start: MessageStart = parse(event)?;
                self.usage = Usage::default();
                self.add_usage(start.message.usage);
                Ok(vec![CompletionEvent::Usage(self.usage)])
            }
            "content_block_start" => self.block_start(parse(event)?),
            "content_block_delta" => self.block_delta(parse(event)?),
            "content_block_stop" => self.block_stop(parse(event)?).map(|e| vec![e]),
            "message_delta" => {
                let delta: MessageDelta = parse(event)?;
                if let Some(reason) = delta.delta.stop_reason {
                    self.stop_reason = Some(stop_reason(&reason));
                }
                self.add_usage(delta.usage);
                Ok(vec![CompletionEvent::Usage(self.usage)])
            }
            "message_stop" => {
                if self.open.is_some() {
                    return Err(protocol("message_stop with a content block still open"));
                }
                let Some(reason) = self.stop_reason else {
                    return Err(protocol("message_stop without a stop_reason"));
                };
                self.finished = true;
                Ok(vec![CompletionEvent::Stop(reason)])
            }
            "error" => {
                let ErrorEvent { error } = parse(event)?;
                Err(api_error(&error.kind, error.message))
            }
            // `ping`, and event types added later (the API may add them, so they're skipped).
            _ => Ok(Vec::new()),
        }
    }

    /// Whether `message_stop` arrived and `Stop` was emitted. Later events are ignored.
    fn is_finished(&self) -> bool {
        self.finished
    }

    /// A body that ends before `message_stop` was cut off: a retryable error.
    fn end(&mut self) -> Result<Vec<CompletionEvent>> {
        Err(ended_early())
    }
}

impl StreamMapper {
    fn add_usage(&mut self, usage: Option<WireUsage>) {
        let usage = usage.unwrap_or_default();
        let total = &mut self.usage;
        let fields = [
            (&mut total.input_tokens, usage.input_tokens),
            (&mut total.output_tokens, usage.output_tokens),
            (
                &mut total.cache_write_tokens,
                usage.cache_creation_input_tokens,
            ),
            (&mut total.cache_read_tokens, usage.cache_read_input_tokens),
        ];
        // Anthropic's counts are cumulative, and `input_tokens` already excludes cache reads
        // and writes, which is what `Usage::input_tokens` means.
        for (field, value) in fields {
            if let Some(value) = value {
                *field = value;
            }
        }
    }

    fn block_start(&mut self, start: BlockStart) -> Result<Vec<CompletionEvent>> {
        if let Some(open) = &self.open {
            return Err(protocol(format!(
                "content block {} started while block {} is open",
                start.index, open.api_index
            )));
        }
        let index = self.next_index;
        let mut block = start.content_block;
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut tool = None;
        // Text in a start event (normally empty) is reported like a first delta.
        let mut first_delta = None;
        let (kind, state) = match kind {
            "text" => {
                let text = take_string(&mut block, "text");
                if !text.is_empty() {
                    first_delta = Some(CompletionEvent::TextDelta {
                        index,
                        text: text.clone(),
                    });
                }
                (BlockKind::Text, BlockState::Text(text))
            }
            "tool_use" => {
                let id = take_string(&mut block, "id");
                let name = take_string(&mut block, "name");
                tool = Some((id.clone(), name.clone()));
                let input = block
                    .remove("input")
                    .unwrap_or_else(|| Value::Object(Map::new()));
                let state = BlockState::ToolUse {
                    id,
                    name,
                    input,
                    json: String::new(),
                };
                (BlockKind::ToolUse, state)
            }
            "thinking" => {
                let text = take_string(&mut block, "thinking");
                if !text.is_empty() {
                    first_delta = Some(CompletionEvent::ReasoningDelta {
                        index,
                        text: text.clone(),
                    });
                }
                let signature = take_string(&mut block, "signature");
                (
                    BlockKind::Reasoning,
                    BlockState::Thinking { text, signature },
                )
            }
            _ => {
                let state = BlockState::Opaque {
                    block,
                    json: String::new(),
                };
                (BlockKind::Opaque, state)
            }
        };
        self.next_index += 1;
        self.open = Some(OpenBlock {
            api_index: start.index,
            index,
            state,
        });
        let start = CompletionEvent::BlockStart { index, kind, tool };
        Ok(std::iter::once(start).chain(first_delta).collect())
    }

    fn block_delta(&mut self, delta: BlockDelta) -> Result<Vec<CompletionEvent>> {
        let open = self.open_block(delta.index, "content_block_delta")?;
        let index = open.index;
        let mut delta = delta.delta;
        let kind = delta
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let event = match (kind, &mut open.state) {
            ("text_delta", BlockState::Text(text)) => {
                let new = take_string(&mut delta, "text");
                text.push_str(&new);
                (!new.is_empty()).then_some(CompletionEvent::TextDelta { index, text: new })
            }
            ("input_json_delta", BlockState::ToolUse { json, .. }) => {
                let partial = take_string(&mut delta, "partial_json");
                json.push_str(&partial);
                (!partial.is_empty()).then_some(CompletionEvent::ToolInputDelta {
                    index,
                    partial_json: partial,
                })
            }
            ("input_json_delta", BlockState::Opaque { json, .. }) => {
                json.push_str(&take_string(&mut delta, "partial_json"));
                None
            }
            ("thinking_delta", BlockState::Thinking { text, .. }) => {
                let new = take_string(&mut delta, "thinking");
                text.push_str(&new);
                (!new.is_empty()).then_some(CompletionEvent::ReasoningDelta { index, text: new })
            }
            ("signature_delta", BlockState::Thinking { signature, .. }) => {
                signature.push_str(&take_string(&mut delta, "signature"));
                None
            }
            // Citations on text aren't kept: the cited text is, and the API accepts text sent
            // back without them.
            ("citations_delta", BlockState::Text(_)) => None,
            ("text_delta" | "input_json_delta" | "thinking_delta" | "signature_delta", state) => {
                return Err(protocol(format!(
                    "{kind} for a {} block",
                    state.wire_name()
                )));
            }
            // Delta types added later.
            _ => None,
        };
        Ok(event.into_iter().collect())
    }

    fn block_stop(&mut self, stop: BlockStop) -> Result<CompletionEvent> {
        let OpenBlock { index, state, .. } = match self.open.take() {
            Some(open) if open.api_index == stop.index => open,
            other => {
                self.open = other;
                return Err(not_open("content_block_stop", stop.index));
            }
        };
        let block = match state {
            BlockState::Text(text) => Block::Text { text },
            BlockState::ToolUse {
                id,
                name,
                input,
                json,
            } => Block::ToolUse {
                id,
                name,
                input: if json.is_empty() {
                    input
                } else {
                    parse_input(&json)
                },
            },
            BlockState::Thinking { text, signature } => Block::Reasoning {
                text: (!text.is_empty()).then_some(text),
                opaque: (!signature.is_empty()).then(|| Opaque {
                    api: API.to_owned(),
                    data: serde_json::json!({ "signature": signature }),
                }),
            },
            BlockState::Opaque { mut block, json } => {
                if !json.is_empty() {
                    block.insert("input".into(), parse_input(&json));
                }
                Block::Opaque(Opaque {
                    api: API.to_owned(),
                    data: Value::Object(block),
                })
            }
        };
        Ok(CompletionEvent::BlockStop { index, block })
    }

    fn open_block(&mut self, api_index: u64, event: &str) -> Result<&mut OpenBlock> {
        match &mut self.open {
            Some(open) if open.api_index == api_index => Ok(open),
            _ => Err(not_open(event, api_index)),
        }
    }
}

impl BlockState {
    fn wire_name(&self) -> &str {
        match self {
            Self::Text(_) => "text",
            Self::ToolUse { .. } => "tool_use",
            Self::Thinking { .. } => "thinking",
            Self::Opaque { block, .. } => block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("untyped"),
        }
    }
}

/// The error for a response body that ended before `message_stop`: the connection was cut, so
/// it's worth retrying.
fn ended_early() -> Error {
    Error::unavailable("anthropic: the response stream ended before message_stop")
}

/// Maps an Anthropic error type (from an `error` event or an error response) to an error: the
/// transient ones are [`Error::Unavailable`].
pub(crate) fn api_error(kind: &str, message: String) -> Error {
    let message = format!("anthropic {kind}: {message}");
    match kind {
        "overloaded_error" | "api_error" | "rate_limit_error" | "timeout_error" => {
            Error::unavailable(message)
        }
        "not_found_error" => Error::NotFound(message),
        "invalid_request_error"
        | "authentication_error"
        | "permission_error"
        | "billing_error"
        | "request_too_large" => Error::InvalidRequest(message),
        _ => Error::other(message),
    }
}

/// `end_turn`, `stop_sequence` (Giles sends no stop sequences), and `pause_turn` (a server tool
/// loop paused; Giles configures no server tools) end the turn. `model_context_window_exceeded`
/// cut the output short like `max_tokens`. Unknown reasons end the turn.
fn stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        "end_turn" | "stop_sequence" | "pause_turn" => StopReason::EndTurn,
        other => {
            tracing::warn!(
                stop_reason = other,
                "unknown Anthropic stop_reason, ending the turn"
            );
            StopReason::EndTurn
        }
    }
}

/// Parses accumulated tool input. Input cut off by `max_tokens` isn't valid JSON; it becomes `{}`
/// so the block can still be stored and sent back (the core gives it the result `truncated`).
fn parse_input(json: &str) -> Value {
    serde_json::from_str(json).unwrap_or_else(|error| {
        tracing::warn!(%error, "unparseable Anthropic tool input, using {{}}");
        Value::Object(Map::new())
    })
}

fn take_string(map: &mut Map<String, Value>, key: &str) -> String {
    match map.remove(key) {
        Some(Value::String(text)) => text,
        _ => String::new(),
    }
}

fn parse<T: for<'de> Deserialize<'de>>(event: &SseEvent) -> Result<T> {
    serde_json::from_str(&event.data)
        .map_err(|error| protocol(format!("malformed {} event: {error}", event.event)))
}

fn not_open(event: &str, api_index: u64) -> Error {
    protocol(format!("{event} for block {api_index}, which isn't open"))
}

fn protocol(message: impl std::fmt::Display) -> Error {
    Error::other(format!("anthropic stream: {message}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sse(event: &str, data: Value) -> SseEvent {
        SseEvent {
            event: event.into(),
            data: data.to_string(),
            id: None,
        }
    }

    fn run(mapper: &mut StreamMapper, events: &[SseEvent]) -> Vec<CompletionEvent> {
        events
            .iter()
            .flat_map(|event| mapper.event(event).unwrap())
            .collect()
    }

    fn start(index: u64, block: Value) -> SseEvent {
        sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": block}),
        )
    }

    fn delta(index: u64, delta: Value) -> SseEvent {
        sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": index, "delta": delta}),
        )
    }

    fn stop(index: u64) -> SseEvent {
        sse(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        )
    }

    fn message_delta(stop_reason: &str, usage: Value) -> SseEvent {
        sse(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": stop_reason,
                   "stop_sequence": null}, "usage": usage}),
        )
    }

    fn message_start(usage: Value) -> SseEvent {
        sse(
            "message_start",
            json!({"type": "message_start", "message": {"id": "msg_1", "type": "message",
                   "role": "assistant", "content": [], "model": "m", "stop_reason": null,
                   "usage": usage}}),
        )
    }

    fn message_stop() -> SseEvent {
        sse("message_stop", json!({"type": "message_stop"}))
    }

    fn usage(input: u64, output: u64, read: u64, write: u64) -> CompletionEvent {
        CompletionEvent::Usage(Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_write_tokens: write,
        })
    }

    #[test]
    fn usage_is_cumulative_and_excludes_cache_from_input() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                message_start(json!({"input_tokens": 12, "output_tokens": 1,
                    "cache_creation_input_tokens": 300, "cache_read_input_tokens": 4000})),
                sse("ping", json!({"type": "ping"})),
                // Only output: the other totals stand.
                message_delta("end_turn", json!({"output_tokens": 57})),
                // Server tools can update input totals at the end; null leaves a total alone.
                message_delta(
                    "end_turn",
                    json!({"input_tokens": 20, "output_tokens": 60,
                           "cache_read_input_tokens": null}),
                ),
                message_stop(),
            ],
        );
        assert_eq!(
            events,
            [
                usage(12, 1, 4000, 300),
                usage(12, 57, 4000, 300),
                usage(20, 60, 4000, 300),
                CompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
        assert!(mapper.is_finished());
        // Nothing after message_stop counts.
        assert_eq!(mapper.event(&message_stop()).unwrap(), []);
    }

    #[test]
    fn message_start_without_usage_counts_zero() {
        let mut mapper = StreamMapper::new();
        assert_eq!(
            run(&mut mapper, &[message_start(Value::Null)]),
            [usage(0, 0, 0, 0)]
        );
    }

    #[test]
    fn text_tool_use_and_index_renumbering() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                // The API's indexes are taken as given for matching but ours restart at 0.
                start(3, json!({"type": "text", "text": ""})),
                delta(3, json!({"type": "text_delta", "text": "Okay"})),
                delta(3, json!({"type": "text_delta", "text": ""})),
                delta(3, json!({"type": "citations_delta", "citation": {}})),
                delta(3, json!({"type": "text_delta", "text": ", checking."})),
                stop(3),
                start(
                    4,
                    json!({"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                                "input": {}}),
                ),
                delta(4, json!({"type": "input_json_delta", "partial_json": ""})),
                delta(
                    4,
                    json!({"type": "input_json_delta", "partial_json": "{\"location\":"}),
                ),
                delta(
                    4,
                    json!({"type": "input_json_delta", "partial_json": " \"SF\"}"}),
                ),
                stop(4),
                start(
                    5,
                    json!({"type": "tool_use", "id": "toolu_2", "name": "now",
                                "input": {}}),
                ),
                stop(5),
                message_delta("tool_use", json!({"output_tokens": 9})),
                message_stop(),
            ],
        );
        let tool = |id: &str, name: &str| Some((id.to_owned(), name.to_owned()));
        assert_eq!(
            events,
            [
                CompletionEvent::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                    tool: None
                },
                CompletionEvent::TextDelta {
                    index: 0,
                    text: "Okay".into()
                },
                CompletionEvent::TextDelta {
                    index: 0,
                    text: ", checking.".into()
                },
                CompletionEvent::BlockStop {
                    index: 0,
                    block: Block::text("Okay, checking.")
                },
                CompletionEvent::BlockStart {
                    index: 1,
                    kind: BlockKind::ToolUse,
                    tool: tool("toolu_1", "get_weather"),
                },
                CompletionEvent::ToolInputDelta {
                    index: 1,
                    partial_json: "{\"location\":".into()
                },
                CompletionEvent::ToolInputDelta {
                    index: 1,
                    partial_json: " \"SF\"}".into()
                },
                CompletionEvent::BlockStop {
                    index: 1,
                    block: Block::ToolUse {
                        id: "toolu_1".into(),
                        name: "get_weather".into(),
                        input: json!({"location": "SF"}),
                    },
                },
                CompletionEvent::BlockStart {
                    index: 2,
                    kind: BlockKind::ToolUse,
                    tool: tool("toolu_2", "now"),
                },
                CompletionEvent::BlockStop {
                    index: 2,
                    block: Block::ToolUse {
                        id: "toolu_2".into(),
                        name: "now".into(),
                        input: json!({}),
                    },
                },
                usage(0, 9, 0, 0),
                CompletionEvent::Stop(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn text_in_a_start_event_is_reported_as_a_delta() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                start(0, json!({"type": "text", "text": "Hel"})),
                delta(0, json!({"type": "text_delta", "text": "lo"})),
                stop(0),
            ],
        );
        assert_eq!(
            events,
            [
                CompletionEvent::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                    tool: None
                },
                CompletionEvent::TextDelta {
                    index: 0,
                    text: "Hel".into()
                },
                CompletionEvent::TextDelta {
                    index: 0,
                    text: "lo".into()
                },
                CompletionEvent::BlockStop {
                    index: 0,
                    block: Block::text("Hello")
                },
            ]
        );
    }

    #[test]
    fn truncated_tool_input_becomes_an_empty_object() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                start(
                    0,
                    json!({"type": "tool_use", "id": "t", "name": "n", "input": {}}),
                ),
                delta(
                    0,
                    json!({"type": "input_json_delta", "partial_json": "{\"a\": \"b"}),
                ),
                stop(0),
                message_delta("max_tokens", json!({"output_tokens": 5})),
                message_stop(),
            ],
        );
        assert_eq!(
            events[2],
            CompletionEvent::BlockStop {
                index: 0,
                block: Block::ToolUse {
                    id: "t".into(),
                    name: "n".into(),
                    input: json!({}),
                },
            }
        );
        assert_eq!(
            events.last(),
            Some(&CompletionEvent::Stop(StopReason::MaxTokens))
        );
    }

    #[test]
    fn thinking_keeps_its_signature_and_omitted_thinking_has_no_text() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                start(
                    0,
                    json!({"type": "thinking", "thinking": "", "signature": ""}),
                ),
                delta(
                    0,
                    json!({"type": "thinking_delta", "thinking": "GCD of 1071"}),
                ),
                delta(
                    0,
                    json!({"type": "thinking_delta", "thinking": " and 462."}),
                ),
                delta(
                    0,
                    json!({"type": "signature_delta", "signature": "EqQBCgIY"}),
                ),
                stop(0),
                // display: "omitted": an empty thinking delta, then the signature.
                start(
                    1,
                    json!({"type": "thinking", "thinking": "", "signature": ""}),
                ),
                delta(1, json!({"type": "thinking_delta", "thinking": ""})),
                delta(1, json!({"type": "signature_delta", "signature": "sig2"})),
                stop(1),
            ],
        );
        let anthropic = |signature: &str| {
            Some(Opaque {
                api: "anthropic".into(),
                data: json!({"signature": signature}),
            })
        };
        assert_eq!(
            events,
            [
                CompletionEvent::BlockStart {
                    index: 0,
                    kind: BlockKind::Reasoning,
                    tool: None
                },
                CompletionEvent::ReasoningDelta {
                    index: 0,
                    text: "GCD of 1071".into()
                },
                CompletionEvent::ReasoningDelta {
                    index: 0,
                    text: " and 462.".into()
                },
                CompletionEvent::BlockStop {
                    index: 0,
                    block: Block::Reasoning {
                        text: Some("GCD of 1071 and 462.".into()),
                        opaque: anthropic("EqQBCgIY"),
                    },
                },
                CompletionEvent::BlockStart {
                    index: 1,
                    kind: BlockKind::Reasoning,
                    tool: None
                },
                CompletionEvent::BlockStop {
                    index: 1,
                    block: Block::Reasoning {
                        text: None,
                        opaque: anthropic("sig2"),
                    },
                },
            ]
        );
    }

    #[test]
    fn server_tool_and_redacted_blocks_are_kept_whole_as_opaque() {
        let result = json!({"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1",
            "content": [{"type": "web_search_result", "title": "Weather", "url": "https://w",
                         "encrypted_content": "Ev0D", "page_age": null}]});
        let redacted = json!({"type": "redacted_thinking", "data": "EmwKAhgB"});
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                start(
                    0,
                    json!({"type": "server_tool_use", "id": "srvtoolu_1",
                                "name": "web_search", "input": {}}),
                ),
                delta(
                    0,
                    json!({"type": "input_json_delta", "partial_json": "{\"query"}),
                ),
                delta(
                    0,
                    json!({"type": "input_json_delta", "partial_json": "\": \"weather\"}"}),
                ),
                stop(0),
                start(1, result.clone()),
                stop(1),
                start(2, redacted.clone()),
                stop(2),
            ],
        );
        let opaque = |index, data| CompletionEvent::BlockStop {
            index,
            block: Block::Opaque(Opaque {
                api: "anthropic".into(),
                data,
            }),
        };
        let start = |index| CompletionEvent::BlockStart {
            index,
            kind: BlockKind::Opaque,
            tool: None,
        };
        assert_eq!(
            events,
            [
                start(0),
                opaque(
                    0,
                    json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                           "input": {"query": "weather"}})
                ),
                start(1),
                opaque(1, result),
                start(2),
                opaque(2, redacted),
            ]
        );
    }

    #[test]
    fn stop_reasons() {
        for (wire, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("stop_sequence", StopReason::EndTurn),
            ("pause_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("model_context_window_exceeded", StopReason::MaxTokens),
            ("refusal", StopReason::Refusal),
            ("something_new", StopReason::EndTurn),
        ] {
            let mut mapper = StreamMapper::new();
            let events = run(
                &mut mapper,
                &[message_delta(wire, json!({})), message_stop()],
            );
            assert_eq!(
                events.last(),
                Some(&CompletionEvent::Stop(expected)),
                "{wire}"
            );
        }
    }

    #[test]
    fn unknown_events_and_deltas_are_skipped() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                sse("something_new", json!({"type": "something_new"})),
                start(0, json!({"type": "text", "text": ""})),
                delta(0, json!({"type": "future_delta", "x": 1})),
                stop(0),
            ],
        );
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn error_event_maps_by_type() {
        let error = |kind: &str| {
            StreamMapper::new()
                .event(&sse(
                    "error",
                    json!({"type": "error", "error": {"type": kind, "message": "Overloaded"}}),
                ))
                .unwrap_err()
        };
        let overloaded = error("overloaded_error");
        assert!(overloaded.is_retryable());
        assert_eq!(
            overloaded.to_string(),
            "unavailable: anthropic overloaded_error: Overloaded"
        );
        assert!(error("rate_limit_error").is_retryable());
        assert!(error("api_error").is_retryable());
        assert!(matches!(
            error("invalid_request_error"),
            Error::InvalidRequest(_)
        ));
        assert!(matches!(error("not_found_error"), Error::NotFound(_)));
        assert!(matches!(error("mystery_error"), Error::Other(_)));
    }

    #[test]
    fn protocol_violations_are_errors() {
        let violations: Vec<Vec<SseEvent>> = vec![
            // Overlapping blocks.
            vec![
                start(0, json!({"type": "text"})),
                start(1, json!({"type": "text"})),
            ],
            // A delta or stop for a block that isn't open.
            vec![delta(0, json!({"type": "text_delta", "text": "x"}))],
            vec![start(0, json!({"type": "text"})), stop(1)],
            // A delta of the wrong type.
            vec![
                start(0, json!({"type": "text"})),
                delta(0, json!({"type": "input_json_delta", "partial_json": "{"})),
            ],
            // Stopping with a block open, or without a stop reason.
            vec![start(0, json!({"type": "text"})), message_stop()],
            vec![message_stop()],
            // Malformed JSON.
            vec![SseEvent {
                event: "content_block_start".into(),
                data: "{".into(),
                id: None,
            }],
        ];
        for events in violations {
            let mut mapper = StreamMapper::new();
            let result: Result<Vec<_>> = events.iter().map(|e| mapper.event(e)).collect();
            let error = result.unwrap_err();
            assert!(matches!(error, Error::Other(_)), "{events:?}: {error:?}");
        }
    }
}
