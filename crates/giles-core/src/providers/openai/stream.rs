//! Chat Completions stream chunks to [`CompletionEvent`]s.

use std::collections::HashSet;

use giles_types::{Block, BlockKind, CompletionEvent, Error, Opaque, Result, StopReason, Usage};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::API;
use crate::providers::http;
use crate::providers::sse::SseEvent;
use crate::providers::wire::EventMapper;

/// The `data` of the event that ends a stream.
const DONE: &str = "[DONE]";

/// Turns the chunks of one streamed response into [`CompletionEvent`]s, one SSE event at a time.
///
/// Chat Completions has no block events: a chunk's delta may carry reasoning, text, and tool call
/// fragments, and a server may interleave them. The mapper opens a block when a different kind of
/// content arrives, closing the open one first, so blocks never overlap and indexes count from 0.
/// Only choice 0 is read (Giles never asks for more).
#[derive(Debug, Default)]
pub(crate) struct StreamMapper {
    next_index: u32,
    open: Option<OpenBlock>,
    /// The API indexes of tool calls already started, so a fragment of a closed call is caught.
    tool_indexes: HashSet<u64>,
    finish_reason: Option<String>,
    saw_tool_call: bool,
    saw_refusal: bool,
    finished: bool,
}

#[derive(Debug)]
struct OpenBlock {
    index: u32,
    state: BlockState,
}

#[derive(Debug)]
enum BlockState {
    Text(String),
    Reasoning {
        text: String,
        /// The delta field it streamed in, where it goes back in later requests.
        field: &'static str,
    },
    ToolUse {
        /// The API's tool call index, when the server sends one.
        api_index: Option<u64>,
        id: String,
        name: String,
        json: String,
    },
}

#[derive(Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    index: u64,
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    refusal: Option<String>,
    /// OpenRouter, Ollama, and newer vLLM.
    #[serde(default)]
    reasoning: Option<String>,
    /// DeepSeek and older vLLM.
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    /// A JSON text fragment; some servers send the whole input as an object instead.
    #[serde(default)]
    arguments: Option<Value>,
}

/// Token counts. `prompt_tokens` includes cached tokens.
#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptDetails>,
    /// DeepSeek's cache hits, outside `prompt_tokens_details`.
    #[serde(default)]
    prompt_cache_hit_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
    /// OpenRouter's (and OpenAI's on newer models) cache writes.
    #[serde(default)]
    cache_write_tokens: Option<u64>,
}

/// An `error` object, in an error response body or a chunk. Some servers send a bare string.
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum ApiError {
    Object {
        #[serde(default)]
        message: Option<String>,
        #[serde(default, rename = "type")]
        kind: Option<String>,
        #[serde(default)]
        code: Option<Value>,
    },
    Text(String),
}

impl ApiError {
    /// The error's code or type, if it has a textual one: `rate_limit_exceeded`,
    /// `invalid_request_error`. OpenAI's string `code` is more specific than its `type`.
    pub(crate) fn kind(&self) -> Option<&str> {
        let Self::Object { kind, code, .. } = self else {
            return None;
        };
        match code {
            Some(Value::String(code)) if code.parse::<u16>().is_err() => Some(code),
            _ => kind.as_deref(),
        }
    }

    /// An HTTP status given as the error's `code` (OpenRouter), if any.
    fn status(&self) -> Option<StatusCode> {
        let Self::Object {
            code: Some(code), ..
        } = self
        else {
            return None;
        };
        let code = match code {
            Value::Number(code) => code.as_u64().and_then(|code| u16::try_from(code).ok()),
            Value::String(code) => code.parse().ok(),
            _ => None,
        }?;
        StatusCode::from_u16(code)
            .ok()
            .filter(|status| status.is_client_error() || status.is_server_error())
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Object { message, .. } => message.as_deref().unwrap_or_default(),
            Self::Text(message) => message,
        }
    }

    /// The error a mid-stream `error` object stands for: by its HTTP status code if it has one
    /// (OpenRouter), else by its code or type. Transient ones are [`Error::Unavailable`].
    fn into_error(self) -> Error {
        let status = self.status();
        let kind = self
            .kind()
            .map(str::to_owned)
            .or_else(|| status.map(|status| status.as_u16().to_string()));
        let message = match &kind {
            Some(kind) => format!("openai {kind}: {}", self.message()),
            None => format!("openai: {}", self.message()),
        };
        if let Some(status) = status {
            return http::status_error(status, None, message);
        }
        match kind.as_deref().unwrap_or_default() {
            "server_error"
            | "api_error"
            | "overloaded_error"
            | "rate_limit_error"
            | "rate_limit_exceeded"
            | "timeout"
            | "service_unavailable" => Error::unavailable(message),
            "not_found_error" | "model_not_found" => Error::NotFound(message),
            "invalid_request_error"
            | "context_length_exceeded"
            | "authentication_error"
            | "permission_error"
            | "insufficient_quota" => Error::InvalidRequest(message),
            _ => Error::other(message),
        }
    }
}

impl StreamMapper {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn chunk(&mut self, chunk: Chunk) -> Result<Vec<CompletionEvent>> {
        if let Some(error) = chunk.error {
            return Err(error.into_error());
        }
        let mut events = Vec::new();
        for choice in chunk.choices.into_iter().filter(|c| c.index == 0) {
            let delta = choice.delta.unwrap_or_default();
            let reasoning = match (delta.reasoning, delta.reasoning_content) {
                (Some(text), _) if !text.is_empty() => Some((text, "reasoning")),
                (_, Some(text)) if !text.is_empty() => Some((text, "reasoning_content")),
                _ => None,
            };
            if let Some((text, field)) = reasoning {
                self.reasoning(text, field, &mut events);
            }
            if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                self.text(text, &mut events);
            }
            if let Some(text) = delta.refusal.filter(|text| !text.is_empty()) {
                self.saw_refusal = true;
                self.text(text, &mut events);
            }
            for call in delta.tool_calls.unwrap_or_default() {
                self.tool_call(call, &mut events)?;
            }
            if let Some(reason) = choice.finish_reason {
                if reason == "error" {
                    return Err(Error::unavailable(
                        "openai: the response stopped with finish_reason \"error\"",
                    ));
                }
                events.extend(self.close());
                self.finish_reason = Some(reason);
            }
        }
        if let Some(usage) = chunk.usage {
            events.push(CompletionEvent::Usage(usage_of(usage)));
        }
        Ok(events)
    }

    fn reasoning(&mut self, new: String, field: &'static str, events: &mut Vec<CompletionEvent>) {
        if !matches!(
            self.open,
            Some(OpenBlock {
                state: BlockState::Reasoning { .. },
                ..
            })
        ) {
            let state = BlockState::Reasoning {
                text: String::new(),
                field,
            };
            self.start(BlockKind::Reasoning, None, state, events);
        }
        if let Some(OpenBlock {
            index,
            state: BlockState::Reasoning { text, .. },
        }) = &mut self.open
        {
            text.push_str(&new);
            events.push(CompletionEvent::ReasoningDelta {
                index: *index,
                text: new,
            });
        }
    }

    fn text(&mut self, new: String, events: &mut Vec<CompletionEvent>) {
        if !matches!(
            self.open,
            Some(OpenBlock {
                state: BlockState::Text(_),
                ..
            })
        ) {
            let state = BlockState::Text(String::new());
            self.start(BlockKind::Text, None, state, events);
        }
        if let Some(OpenBlock {
            index,
            state: BlockState::Text(text),
        }) = &mut self.open
        {
            text.push_str(&new);
            events.push(CompletionEvent::TextDelta {
                index: *index,
                text: new,
            });
        }
    }

    /// A tool call fragment. It continues the open tool call if it carries no id (or the open
    /// call's id) and its index matches or is absent. Otherwise it starts a new call, which must
    /// have an id and a name: a new id always starts a call, since Ollama sends index 0 for every
    /// call.
    fn tool_call(&mut self, call: ToolCallDelta, events: &mut Vec<CompletionEvent>) -> Result<()> {
        let function = call.function;
        let (name, arguments) = match function {
            Some(FunctionDelta { name, arguments }) => (name, arguments),
            None => (None, None),
        };
        let arguments = match arguments {
            Some(Value::String(text)) => text,
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
        let new_id = call.id.filter(|id| !id.is_empty());
        let continues = match &self.open {
            Some(OpenBlock {
                state: BlockState::ToolUse { api_index, id, .. },
                ..
            }) => {
                new_id.as_ref().is_none_or(|new| new == id)
                    && (call.index.is_none() || *api_index == call.index)
            }
            _ => false,
        };
        if !continues {
            let Some(id) = new_id else {
                return Err(match call.index {
                    Some(index) if self.tool_indexes.contains(&index) => protocol(format!(
                        "tool call {index} continued after another block started"
                    )),
                    _ => protocol("a tool call started without an id and a name"),
                });
            };
            let Some(name) = name.filter(|name| !name.is_empty()) else {
                return Err(protocol("a tool call started without an id and a name"));
            };
            if let Some(index) = call.index {
                self.tool_indexes.insert(index);
            }
            self.saw_tool_call = true;
            let state = BlockState::ToolUse {
                api_index: call.index,
                id: id.clone(),
                name: name.clone(),
                json: String::new(),
            };
            self.start(BlockKind::ToolUse, Some((id, name)), state, events);
        }
        if let Some(OpenBlock {
            index,
            state: BlockState::ToolUse { json, .. },
        }) = &mut self.open
            && !arguments.is_empty()
        {
            json.push_str(&arguments);
            events.push(CompletionEvent::ToolInputDelta {
                index: *index,
                partial_json: arguments,
            });
        }
        Ok(())
    }

    /// Closes the open block and starts a new one.
    fn start(
        &mut self,
        kind: BlockKind,
        tool: Option<(String, String)>,
        state: BlockState,
        events: &mut Vec<CompletionEvent>,
    ) {
        events.extend(self.close());
        let index = self.next_index;
        self.next_index += 1;
        self.open = Some(OpenBlock { index, state });
        events.push(CompletionEvent::BlockStart { index, kind, tool });
    }

    /// `BlockStop` for the open block, if any.
    fn close(&mut self) -> Option<CompletionEvent> {
        let OpenBlock { index, state } = self.open.take()?;
        let block = match state {
            BlockState::Text(text) => Block::Text { text },
            BlockState::Reasoning { text, field } => Block::Reasoning {
                text: Some(text),
                opaque: Some(Opaque {
                    api: API.to_owned(),
                    data: json!({ "field": field }),
                }),
            },
            BlockState::ToolUse { id, name, json, .. } => Block::ToolUse {
                id,
                name,
                input: parse_input(&json),
            },
        };
        Some(CompletionEvent::BlockStop { index, block })
    }

    /// The final events: the open block's stop, then `Stop`. Without a finish reason the
    /// response was cut off.
    fn stop(&mut self) -> Result<Vec<CompletionEvent>> {
        let Some(reason) = self.finish_reason.take() else {
            return Err(ended_early());
        };
        let reason = stop_reason(&reason, self.saw_tool_call, self.saw_refusal);
        self.finished = true;
        let close = self.close();
        Ok(close
            .into_iter()
            .chain([CompletionEvent::Stop(reason)])
            .collect())
    }
}

impl EventMapper for StreamMapper {
    /// Maps one chunk, or `[DONE]`. An `error` object, a malformed chunk, or an impossible
    /// sequence of tool call fragments is an `Err` that ends the response.
    fn event(&mut self, event: &SseEvent) -> Result<Vec<CompletionEvent>> {
        if self.finished {
            return Ok(Vec::new());
        }
        if event.data.trim() == DONE {
            return self.stop();
        }
        let chunk = serde_json::from_str(&event.data)
            .map_err(|error| protocol(format!("malformed chunk: {error}")))?;
        self.chunk(chunk)
    }

    /// Whether `[DONE]` arrived and `Stop` was emitted. Later events are ignored.
    fn is_finished(&self) -> bool {
        self.finished
    }

    /// A body that ends without `[DONE]` is complete if a finish reason arrived (some servers
    /// just close the stream); otherwise it was cut off, a retryable error.
    fn end(&mut self) -> Result<Vec<CompletionEvent>> {
        self.stop()
    }
}

/// The usage as Giles counts it: `prompt_tokens` includes cache reads and writes, which
/// `Usage::input_tokens` excludes.
fn usage_of(usage: WireUsage) -> Usage {
    let details = usage.prompt_tokens_details.unwrap_or_default();
    let cache_read = details
        .cached_tokens
        .or(usage.prompt_cache_hit_tokens)
        .unwrap_or(0);
    let cache_write = details.cache_write_tokens.unwrap_or(0);
    Usage {
        input_tokens: usage
            .prompt_tokens
            .unwrap_or(0)
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output_tokens: usage.completion_tokens.unwrap_or(0),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    }
}

/// `stop` ends the turn, unless the response called tools (some servers report `stop` with tool
/// calls) or refused; `tool_calls` (and the legacy `function_call`) is `ToolUse`, `length` is
/// `MaxTokens`, and `content_filter` is `Refusal`. Unknown reasons end the turn, or stop for
/// tools if there were tool calls.
fn stop_reason(reason: &str, saw_tool_call: bool, saw_refusal: bool) -> StopReason {
    match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ if saw_tool_call => StopReason::ToolUse,
        "stop" if saw_refusal => StopReason::Refusal,
        "stop" => StopReason::EndTurn,
        other => {
            tracing::warn!(
                finish_reason = other,
                "unknown OpenAI finish_reason, ending the turn"
            );
            StopReason::EndTurn
        }
    }
}

/// Parses accumulated tool arguments. No arguments is `{}`. Arguments cut off by the output limit
/// aren't valid JSON; they become `{}` so the block can still be stored and sent back.
fn parse_input(json: &str) -> Value {
    if json.trim().is_empty() {
        return Value::Object(Map::new());
    }
    serde_json::from_str(json).unwrap_or_else(|error| {
        tracing::warn!(%error, "unparseable OpenAI tool arguments, using {{}}");
        Value::Object(Map::new())
    })
}

/// The error for a response that ended without a finish reason: the connection or the upstream
/// model was cut off, so it's worth retrying.
fn ended_early() -> Error {
    Error::unavailable("openai: the response stream ended before a finish_reason")
}

fn protocol(message: impl std::fmt::Display) -> Error {
    Error::other(format!("openai stream: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(data: Value) -> SseEvent {
        SseEvent {
            event: "message".into(),
            data: data.to_string(),
            id: None,
        }
    }

    fn delta(delta: Value) -> SseEvent {
        chunk(
            json!({"id": "chatcmpl-1", "object": "chat.completion.chunk",
                     "choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
        )
    }

    fn finish(reason: &str) -> SseEvent {
        chunk(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]}))
    }

    fn usage_chunk(usage: Value) -> SseEvent {
        chunk(json!({"choices": [], "usage": usage}))
    }

    fn done() -> SseEvent {
        SseEvent {
            event: "message".into(),
            data: DONE.into(),
            id: None,
        }
    }

    fn call(index: u64, id: Option<&str>, name: Option<&str>, arguments: &str) -> Value {
        let mut function = json!({"arguments": arguments});
        if let Some(name) = name {
            function["name"] = name.into();
        }
        let mut call = json!({"index": index, "function": function});
        if let Some(id) = id {
            call["id"] = id.into();
            call["type"] = "function".into();
        }
        call
    }

    fn run(mapper: &mut StreamMapper, events: &[SseEvent]) -> Vec<CompletionEvent> {
        events
            .iter()
            .flat_map(|event| mapper.event(event).unwrap())
            .collect()
    }

    fn start(index: u32, kind: BlockKind) -> CompletionEvent {
        CompletionEvent::BlockStart {
            index,
            kind,
            tool: None,
        }
    }

    fn tool_start(index: u32, id: &str, name: &str) -> CompletionEvent {
        CompletionEvent::BlockStart {
            index,
            kind: BlockKind::ToolUse,
            tool: Some((id.into(), name.into())),
        }
    }

    fn text(index: u32, text: &str) -> CompletionEvent {
        CompletionEvent::TextDelta {
            index,
            text: text.into(),
        }
    }

    fn input(index: u32, partial_json: &str) -> CompletionEvent {
        CompletionEvent::ToolInputDelta {
            index,
            partial_json: partial_json.into(),
        }
    }

    fn stop_block(index: u32, block: Block) -> CompletionEvent {
        CompletionEvent::BlockStop { index, block }
    }

    fn tool_block(id: &str, name: &str, input: Value) -> Block {
        Block::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
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
    fn text_then_usage_with_cached_tokens() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                delta(json!({"role": "assistant", "content": "", "refusal": null})),
                delta(json!({"content": "Hello"})),
                delta(json!({"content": ", Stephen."})),
                finish("stop"),
                usage_chunk(json!({"prompt_tokens": 2006, "completion_tokens": 300,
                    "total_tokens": 2306, "prompt_tokens_details": {"cached_tokens": 1920,
                    "audio_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}})),
                done(),
            ],
        );
        assert_eq!(
            events,
            [
                start(0, BlockKind::Text),
                text(0, "Hello"),
                text(0, ", Stephen."),
                stop_block(0, Block::text("Hello, Stephen.")),
                usage(86, 300, 1920, 0),
                CompletionEvent::Stop(StopReason::EndTurn),
            ]
        );
        assert!(mapper.is_finished());
        assert_eq!(mapper.event(&done()).unwrap(), []);
        assert_eq!(
            mapper.event(&delta(json!({"content": "late"}))).unwrap(),
            []
        );
    }

    #[test]
    fn usage_variants() {
        let usage_of = |json: Value| {
            let mut mapper = StreamMapper::new();
            mapper.event(&usage_chunk(json)).unwrap()
        };
        // No details: everything is uncached input.
        assert_eq!(
            usage_of(json!({"prompt_tokens": 10, "completion_tokens": 2})),
            [usage(10, 2, 0, 0)]
        );
        // OpenRouter reports cache writes, which are also part of prompt_tokens.
        assert_eq!(
            usage_of(json!({"prompt_tokens": 10339, "completion_tokens": 60,
                "prompt_tokens_details": {"cached_tokens": 10000, "cache_write_tokens": 300}})),
            [usage(39, 60, 10000, 300)]
        );
        // DeepSeek reports hits outside the details.
        assert_eq!(
            usage_of(json!({"prompt_tokens": 500, "completion_tokens": 9,
                "prompt_cache_hit_tokens": 384, "prompt_cache_miss_tokens": 116})),
            [usage(116, 9, 384, 0)]
        );
        // Nulls count as zero, and inconsistent counts don't underflow.
        assert_eq!(
            usage_of(json!({"prompt_tokens": 5, "completion_tokens": null,
                "prompt_tokens_details": {"cached_tokens": 9}})),
            [usage(0, 0, 9, 0)]
        );
        // A null usage on an ordinary chunk is nothing.
        assert_eq!(usage_of(Value::Null), []);
    }

    #[test]
    fn interleaved_text_and_parallel_tool_calls() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                delta(json!({"role": "assistant", "content": "Checking both."})),
                delta(json!({"tool_calls": [call(0, Some("call_1"), Some("weather__get"), "")]})),
                delta(json!({"tool_calls": [call(0, None, None, "{\"city\":")]})),
                delta(json!({"tool_calls": [call(0, None, None, " \"Paris\"}")]})),
                // The second call begins while the first is open: the first is closed.
                delta(
                    json!({"tool_calls": [call(1, Some("call_2"), Some("weather__get"),
                                                 "{\"city\": \"Oslo\"}")]}),
                ),
                // Text after a tool call opens a new text block.
                delta(json!({"content": "One moment."})),
                delta(json!({"tool_calls": [call(2, Some("call_3"), Some("time__now"), "")]})),
                finish("tool_calls"),
                usage_chunk(json!({"prompt_tokens": 80, "completion_tokens": 40})),
                done(),
            ],
        );
        assert_eq!(
            events,
            [
                start(0, BlockKind::Text),
                text(0, "Checking both."),
                stop_block(0, Block::text("Checking both.")),
                tool_start(1, "call_1", "weather__get"),
                input(1, "{\"city\":"),
                input(1, " \"Paris\"}"),
                stop_block(
                    1,
                    tool_block("call_1", "weather__get", json!({"city": "Paris"}))
                ),
                tool_start(2, "call_2", "weather__get"),
                input(2, "{\"city\": \"Oslo\"}"),
                stop_block(
                    2,
                    tool_block("call_2", "weather__get", json!({"city": "Oslo"}))
                ),
                start(3, BlockKind::Text),
                text(3, "One moment."),
                stop_block(3, Block::text("One moment.")),
                tool_start(4, "call_3", "time__now"),
                stop_block(4, tool_block("call_3", "time__now", json!({}))),
                usage(80, 40, 0, 0),
                CompletionEvent::Stop(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn several_calls_in_one_chunk_and_calls_without_indexes() {
        // Some servers send whole calls, several per chunk; Ollama once sent no index, and some
        // send the arguments as an object.
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                delta(json!({"tool_calls": [
                    call(0, Some("call_a"), Some("a"), "{}"),
                    call(1, Some("call_b"), Some("b"), "{\"x\":1}"),
                ]})),
                delta(json!({"tool_calls": [
                    {"id": "call_c", "function": {"name": "c", "arguments": {"y": 2}}},
                ]})),
                delta(json!({"tool_calls": [{"function": {"arguments": ""}}]})),
                delta(json!({"tool_calls": [
                    {"id": "call_d", "function": {"name": "d", "arguments": "{\"z\""}},
                ]})),
                // The same id again continues the call.
                delta(json!({"tool_calls": [{"id": "call_d", "function": {"arguments": ":3}"}}]})),
                // Ollama reported `stop` with tool calls before it learned `tool_calls`.
                finish("stop"),
                done(),
            ],
        );
        let blocks: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                CompletionEvent::BlockStop { block, .. } => Some(block.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            blocks,
            [
                tool_block("call_a", "a", json!({})),
                tool_block("call_b", "b", json!({"x": 1})),
                tool_block("call_c", "c", json!({"y": 2})),
                tool_block("call_d", "d", json!({"z": 3})),
            ]
        );
        assert_eq!(
            events.last(),
            Some(&CompletionEvent::Stop(StopReason::ToolUse))
        );
    }

    #[test]
    fn a_new_id_starts_a_call_even_with_the_same_index() {
        // Ollama sends index 0 for every call; each has its own id.
        let tool_blocks = |events: &[SseEvent]| -> Vec<CompletionEvent> {
            run(&mut StreamMapper::new(), events)
                .into_iter()
                .filter(|event| {
                    matches!(
                        event,
                        CompletionEvent::BlockStart { .. } | CompletionEvent::BlockStop { .. }
                    )
                })
                .collect()
        };
        let expected = [
            tool_start(0, "call_a", "files__read"),
            stop_block(
                0,
                tool_block("call_a", "files__read", json!({"path": "hello.py"})),
            ),
            tool_start(1, "call_b", "files__read"),
            stop_block(
                1,
                tool_block("call_b", "files__read", json!({"path": "world.py"})),
            ),
        ];
        let first = call(
            0,
            Some("call_a"),
            Some("files__read"),
            "{\"path\":\"hello.py\"}",
        );
        let second = call(
            0,
            Some("call_b"),
            Some("files__read"),
            "{\"path\":\"world.py\"}",
        );

        // In two chunks.
        let events = tool_blocks(&[
            delta(json!({"tool_calls": [first.clone()]})),
            delta(json!({"tool_calls": [second.clone()]})),
            finish("tool_calls"),
        ]);
        assert_eq!(events, expected);

        // As two entries of one delta.
        let events = tool_blocks(&[
            delta(json!({"tool_calls": [first, second]})),
            finish("tool_calls"),
        ]);
        assert_eq!(events, expected);

        // The same id with the same index continues the call.
        let events = tool_blocks(&[
            delta(json!({"tool_calls": [call(0, Some("call_a"), Some("files__read"), "{\"pa")]})),
            delta(json!({"tool_calls": [call(0, Some("call_a"), None, "th\":\"hello.py\"}")]})),
            finish("tool_calls"),
        ]);
        assert_eq!(events, expected[..2]);
    }

    #[test]
    fn reasoning_keeps_its_field() {
        for field in ["reasoning", "reasoning_content"] {
            let mut mapper = StreamMapper::new();
            let events = run(
                &mut mapper,
                &[
                    delta(json!({"role": "assistant", "content": null, field: ""})),
                    delta(json!({field: "9.11 < 9.8"})),
                    // OpenRouter sends `reasoning_details` alongside; it's not kept.
                    delta(json!({field: ", so 9.8.", "reasoning_details": [{"type": "x"}]})),
                    delta(json!({"content": "9.8 is larger."})),
                    finish("stop"),
                    done(),
                ],
            );
            assert_eq!(
                events,
                [
                    start(0, BlockKind::Reasoning),
                    CompletionEvent::ReasoningDelta {
                        index: 0,
                        text: "9.11 < 9.8".into()
                    },
                    CompletionEvent::ReasoningDelta {
                        index: 0,
                        text: ", so 9.8.".into()
                    },
                    stop_block(
                        0,
                        Block::Reasoning {
                            text: Some("9.11 < 9.8, so 9.8.".into()),
                            opaque: Some(Opaque {
                                api: "openai".into(),
                                data: json!({"field": field}),
                            }),
                        }
                    ),
                    start(1, BlockKind::Text),
                    text(1, "9.8 is larger."),
                    stop_block(1, Block::text("9.8 is larger.")),
                    CompletionEvent::Stop(StopReason::EndTurn),
                ],
                "{field}"
            );
        }
    }

    #[test]
    fn finish_reasons() {
        for (wire, expected) in [
            ("stop", StopReason::EndTurn),
            ("tool_calls", StopReason::ToolUse),
            ("function_call", StopReason::ToolUse),
            ("length", StopReason::MaxTokens),
            ("content_filter", StopReason::Refusal),
            ("something_new", StopReason::EndTurn),
        ] {
            let mut mapper = StreamMapper::new();
            let events = run(&mut mapper, &[finish(wire), done()]);
            assert_eq!(events, [CompletionEvent::Stop(expected)], "{wire}");
        }
        // A refusal streams as text and stops as a refusal.
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                delta(json!({"refusal": "I can't help with that."})),
                finish("stop"),
                done(),
            ],
        );
        assert_eq!(events[1], text(0, "I can't help with that."));
        assert_eq!(
            events.last(),
            Some(&CompletionEvent::Stop(StopReason::Refusal))
        );
    }

    #[test]
    fn truncated_tool_arguments_become_an_empty_object() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                delta(json!({"tool_calls": [call(0, Some("c"), Some("n"), "{\"a\": \"b")]})),
                finish("length"),
                done(),
            ],
        );
        assert_eq!(events[2], stop_block(0, tool_block("c", "n", json!({}))));
        assert_eq!(
            events.last(),
            Some(&CompletionEvent::Stop(StopReason::MaxTokens))
        );
    }

    #[test]
    fn the_end_of_the_body_completes_only_after_a_finish_reason() {
        let mut mapper = StreamMapper::new();
        run(
            &mut mapper,
            &[delta(json!({"content": "Hi"})), finish("stop")],
        );
        assert_eq!(
            mapper.end().unwrap(),
            [CompletionEvent::Stop(StopReason::EndTurn)]
        );

        let mut mapper = StreamMapper::new();
        run(&mut mapper, &[delta(json!({"content": "Half a sen"}))]);
        let error = mapper.end().unwrap_err();
        assert!(error.is_retryable(), "{error:?}");

        let mut mapper = StreamMapper::new();
        run(&mut mapper, &[delta(json!({"content": "Half"}))]);
        let error = mapper.event(&done()).unwrap_err();
        assert!(error.is_retryable(), "{error:?}");
    }

    #[test]
    fn error_objects_map_by_code_or_type() {
        let error = |error: Value| {
            StreamMapper::new()
                .event(&chunk(json!({"error": error})))
                .unwrap_err()
        };
        let server = error(
            json!({"message": "The server had an error while processing your \
            request.", "type": "server_error", "param": null, "code": null}),
        );
        assert!(server.is_retryable());
        assert_eq!(
            server.to_string(),
            "unavailable: openai server_error: The server had an error while processing your \
             request."
        );
        // OpenRouter: an HTTP status as the code, alongside a choice that finished with "error".
        let upstream = StreamMapper::new()
            .event(&chunk(json!({
                "id": "gen-1", "object": "chat.completion.chunk",
                "error": {"code": 502, "message": "Provider disconnected"},
                "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "error"}],
            })))
            .unwrap_err();
        assert!(upstream.is_retryable());
        assert!(
            upstream
                .to_string()
                .contains("openai 502: Provider disconnected")
        );
        assert!(matches!(
            error(json!({"code": 400, "message": "bad"})),
            Error::InvalidRequest(_)
        ));
        assert!(error(json!({"type": "api_error", "message": "model crashed"})).is_retryable());
        assert!(matches!(
            error(json!({"type": "invalid_request_error", "message": "x"})),
            Error::InvalidRequest(_)
        ));
        assert!(matches!(
            error(json!({"type": "not_found_error", "message": "x"})),
            Error::NotFound(_)
        ));
        assert!(matches!(
            error(json!("model is loading")),
            Error::Other(m) if m.to_string() == "openai: model is loading"
        ));
        assert!(matches!(error(json!({"type": "mystery"})), Error::Other(_)));

        // A finish reason of "error" without an error object.
        let error = StreamMapper::new().event(&finish("error")).unwrap_err();
        assert!(error.is_retryable());
    }

    #[test]
    fn protocol_violations_are_errors() {
        let violations: Vec<Vec<SseEvent>> = vec![
            // A tool call without an id or a name.
            vec![delta(
                json!({"tool_calls": [call(0, None, Some("n"), "{}")]}),
            )],
            vec![delta(
                json!({"tool_calls": [call(0, Some("c"), None, "{}")]}),
            )],
            // A fragment of a call that was already closed.
            vec![
                delta(json!({"tool_calls": [call(0, Some("c"), Some("n"), "{")]})),
                delta(json!({"content": "x"})),
                delta(json!({"tool_calls": [call(0, None, None, "}")]})),
            ],
            // Malformed JSON.
            vec![SseEvent {
                event: "message".into(),
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

    #[test]
    fn other_choices_are_ignored() {
        let mut mapper = StreamMapper::new();
        let events = run(
            &mut mapper,
            &[
                chunk(json!({"choices": [{"index": 1, "delta": {"content": "other"}}]})),
                delta(json!({"content": "mine"})),
            ],
        );
        assert_eq!(events, [start(0, BlockKind::Text), text(0, "mine")]);
    }
}
