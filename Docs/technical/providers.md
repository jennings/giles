# Providers

The built-in providers live in `giles_core::providers`, each behind a Cargo feature of `giles-core`
that the `giles` crate forwards under the same name (PLAN section 4.2). Both implement
`giles_types::Provider` (see [Message Model and Traits](message-model.md)) by translating the
message model to and from the provider's wire format; the core never sees wire formats.

| Feature              | Module                 | API                                    |
| -------------------- | ---------------------- | -------------------------------------- |
| `provider-anthropic` | `providers::anthropic` | Anthropic Messages API                 |
| `provider-openai`    | `providers::openai`    | OpenAI-compatible Chat Completions API |

Both are on by default. The shared modules below are compiled when either is, so `giles-core` builds
with no provider, either one, or both (`--no-default-features --features provider-openai`).

## Shared plumbing

### HTTP

`providers::http` holds what every HTTP provider needs:

- `client(&HttpOptions)` builds a `reqwest` client from `net::tls_client_builder()`, the core's
  ungated outbound HTTP setup (also used by the MCP client): rustls trusting only the Mozilla roots
  bundled by `webpki-root-certs` (`tls_certs_only`), never the OS trust store. The binary behaves
  the same on every host and container, and never consults the macOS trust daemon. Redirects are
  not followed, so an API key header can't be forwarded to another host. `HttpOptions` has a
  connect timeout (default 10 s) and a read timeout (default 5 min) that applies to each read, so
  a stream stays open while data keeps coming but a silent connection fails.
- `status_error(status, retry_after, message)` classifies a failed response: 408, 429, and 5xx
  (including Anthropic's 529 "overloaded") become `Error::Unavailable` with the server's retry
  hint, which the core treats as retryable and a reason to fall back to the next model (PLAN
  section 6.10); 404 becomes `Error::NotFound`; other 4xx become `Error::InvalidRequest`.
- `retry_after(headers)` reads `Retry-After` in its delay-seconds form, the form model APIs use.
- `transport_error` maps connect, TLS, timeout, and reset failures to `Error::Unavailable`, with
  the error's source chain in the message. `body_stream` applies it to a streamed body, and
  `error_body` reads at most 64 KiB of an error response.
- `ApiKey` wraps an API key so `Debug` shows `ApiKey(<redacted>)`; it has no `Display`, and
  `expose()` gives the key for the header, which providers mark sensitive (hidden from `Debug`,
  kept out of HTTP/2 header compression tables).
- `base_url(text)` validates a `base_url` config value: an `http` or `https` URL without
  credentials, query, or fragment, returned without its trailing slash. Plain `http` is allowed
  for local servers and proxies.

The `reqwest` feature set is `rustls` (aws-lc-rs), `http2`, and `stream`, without `system-proxy`.
Proxies from `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` still apply.

### Server-sent events

Both v1 APIs stream with server-sent events. `providers::sse` is a small parser rather than a
dependency: the format is a page of the HTML standard, and owning it lets the parser enforce a size
limit and be tested against every chunk boundary.

- `SseParser::push(&[u8]) -> Result<Vec<SseEvent>, SseError>` is incremental. It accepts CRLF,
  LF, and CR line endings (also split across chunks), comments, multi-line `data`, `id`, a leading
  byte order mark, and drops an event with no `data` and an unfinished event at the end of the
  stream. Each event, and any single line, is limited to 16 MiB (`SseError::TooLarge`).
- `SseEvent { event, data, id }`; `event` is `message` when the stream didn't name it.
- `events(body)` turns a byte stream into a stream of events. Its error type only needs
  `From<SseError>`, which `giles_types::Error` has (as `Error::Other`), so a provider writes
  `sse::events(http::body_stream(response))`. Body errors pass through, and the first error ends
  the stream.

### Wire helpers

`providers::wire` holds what the wire mappings share:

- `EventMapper` is a provider's stream state machine: `event(&SseEvent)` maps one event to
  `CompletionEvent`s or an error, `is_finished()` says `Stop` was emitted, and `end()` is called
  when the body ends before that, returning the final events or the error for a cut-off response.
  `completion_events(events, mapper)` drives it: the stream ends after `Stop` without reading the
  rest of the body, or after the first error.
- `media_essence(type)` is a media type without parameters, lowercased, for matching. The text an
  attachment becomes when the model doesn't receive it as content, `[media:<id> <name> <type>]`
  (PLAN section 5), is `giles_core::media::reference(&MediaRef)`, shared with the agent loop.
- `temperature(f32)` is a temperature as its shortest decimal (`0.7`, not `0.699999988079071`);
  NaN and infinity are `Error::InvalidRequest`.

## Anthropic

`providers::anthropic` speaks the [Messages API](https://platform.claude.com/docs/en/api/messages)
(`POST /v1/messages` with `stream: true`). Its API family name, `Opaque::api`, is `anthropic`
(`anthropic::API`).

```rust
let config = AnthropicConfig::from_json(&instance_config_json)?;
let provider = AnthropicProvider::new(config, media_source)?; // impl giles_types::Provider
```

`AnthropicProvider::new(AnthropicConfig, Arc<dyn MediaSource>)` builds the HTTP client and sends
nothing. Every request carries `x-api-key`, `anthropic-version`, and, when `beta` is set,
`anthropic-beta` (comma-separated).

### Configuration

`AnthropicConfig::from_json` validates the instance's `config` table, as JSON with secret references
already resolved, and returns `Error::InvalidRequest` naming the bad key (never the API key). The
keys are in [Providers](../user/providers.md); `api_key` is an `http::ApiKey`. The connect and read
timeouts (`HttpOptions`) are fields of the struct, not config keys.

### Media

Request bodies carry images and PDFs inline as base64. The provider reads them through a
`providers::MediaSource` (`async fn read(&self, &MediaRef) -> Result<Bytes>`), the blob store in
the daemon. It returns only bytes, because the media type is already in the `MediaRef`. The model's
accepted types are `image/jpeg`, `image/png`, `image/gif`, `image/webp`, and `application/pdf`
(`anthropic::MEDIA_TYPES`). An `Image`, `Audio`, or `Document` block whose type isn't one of them is
sent as its text reference, `[media:<id> <name> <type>]`, so a stray unsupported attachment never
makes every later request fail (PLAN section 5). The core is expected to leave out media the model
doesn't accept, by `ModelInfo::accepts`, when it builds the turn (2.4); this is the backstop.

### Request mapping

| Giles                                          | Messages API                                                                               |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `system` (text blocks only)                    | `system` text blocks, the last with `cache_control`; omitted when empty                    |
| consecutive messages of the same role          | one message; messages left empty by dropped blocks are skipped first                       |
| `Text`                                         | `text`; empty text is dropped (the API rejects it)                                         |
| `Image`/`Document`/`Audio` of an accepted type | `image` or `document` (by media type) with a base64 source; `Document.name` is `title`     |
| media of another type                          | `text` with the reference                                                                  |
| `ToolUse`                                      | `tool_use`                                                                                 |
| `ToolResult`                                   | `tool_result`; `content` omitted when empty, `is_error: true` only when set                |
| `Reasoning` with `opaque` `{api: "anthropic"}` | `thinking` with `thinking: text` (empty when `None`) and the stored `signature`            |
| other `Reasoning`                              | dropped: the API rejects thinking without its signature                                    |
| `Opaque` with `api: "anthropic"`               | `data`, unchanged (`redacted_thinking`, `server_tool_use`, `web_search_tool_result`, ...)  |
| `Opaque` of another API                        | dropped (PLAN section 5)                                                                   |
| `ToolSpec`                                     | `tools` entry with the name unchanged; `description` omitted when empty                    |
| `max_tokens`                                   | request, else config `max_tokens`, else the model family's maximum, else 16,384            |
| `reasoning_budget: None`                       | `{type: "disabled"}` where the model accepts it, else no `thinking` field                  |
| `reasoning_budget: Some(n)`                    | `{type: "enabled", budget_tokens: n}` on Claude 4.5 and earlier, else `{type: "adaptive"}` |
| `temperature`                                  | sent only before API version `2026-10-21`, and only to Claude 4.6 and earlier              |

A tool result may hold only text and media; any other block in it is `Error::InvalidRequest`, as is
a non-text system block. Merging consecutive messages turns the core's one-message-per-inbound
event into the alternating turns the API expects; the API would merge them too, but doing it here
keeps the request explicit.

Thinking has changed across model generations: Claude 4.5 and earlier take a token budget, 4.6
deprecates it, and 4.7 and later reject it and take `adaptive` thinking, with the depth set by
`effort` (not mapped). A `None` budget means "reasoning off where the API allows"
(`GenerationParams`), so it sends `{type: "disabled"}` to the families that accept it: Claude 4.5
through 4.8, Opus 5, Sonnet 5, and Haiku 5.5, where thinking is otherwise off by default or on by
default (5.x) but can be turned off. Opus 5.5, Sonnet 5.5, Fable, and Mythos reject `disabled`
(thinking is always on), and unknown models may too, so they get no `thinking` field and the
model's default. The family of a model id comes from a small built-in table (`models.rs`), with
specific rows such as `claude-opus-5-5` ahead of their generation's (`claude-opus-5`); unknown ids
are treated as new models (adaptive thinking, no `disabled`, no sampling parameters).

`temperature` is sent only when both sides accept it. API version `2026-10-21` removed
`temperature`, `top_p`, and `top_k` for every model, and Claude 4.7 and later reject a non-default
`temperature` on every API version. So with the default `anthropic_version` (`2023-06-01`) it goes
only to Claude 4.6 and 4.5 models; elsewhere the request samples at the default rather than failing
with an error that model fallback wouldn't retry.

### Prompt caching

PLAN section 6.7 lays out the prompt so the prefix (tools, then system, then messages) is stable.
The request sets both:

- automatic caching: a top-level `cache_control`, so the API puts a breakpoint on the last
  cacheable block and moves it forward as the conversation grows;
- an explicit breakpoint on the last `system` block, so the tools and system prompt stay cached
  even when the conversation's breakpoint misses (more than 20 blocks since the last write).

Both use the same TTL: 1 hour for `CacheRetention::Long` (human-paced sessions; `"ttl": "1h"`) and
the API's 5-minute default for `Short` (job runs). The 1-hour TTL needs no beta header. Using one
TTL avoids the API's rule that longer TTLs must come before shorter ones, and its 400 error when
the automatic breakpoint lands on a block that has an explicit `cache_control` with a different
TTL. Two of the four breakpoint slots are used.

### Stream mapping

`StreamMapper` turns the response's server-sent events into `CompletionEvent`s one event at a time.
It checks the `CompletionEvent` contract instead of trusting the stream: a block starting while
another is open, a delta or stop for a block that isn't open, a delta of the wrong type, malformed
JSON, and `message_stop` with a block open or without a stop reason are all `Error::Other`. The
API's block indexes are matched as given, and Giles' indexes are renumbered from 0.

| Messages API event                      | `CompletionEvent`                                                |
| --------------------------------------- | ---------------------------------------------------------------- |
| `message_start`                         | `Usage`                                                          |
| `content_block_start` `text`            | `BlockStart { kind: Text }`                                      |
| `content_block_start` `tool_use`        | `BlockStart { kind: ToolUse, tool: Some((id, name)) }`           |
| `content_block_start` `thinking`        | `BlockStart { kind: Reasoning }`                                 |
| `content_block_start` of any other type | `BlockStart { kind: Opaque }`                                    |
| `text_delta`                            | `TextDelta` (empty deltas are dropped)                           |
| `input_json_delta` on `tool_use`        | `ToolInputDelta`; the JSON is accumulated and parsed at the stop |
| `thinking_delta`                        | `ReasoningDelta`                                                 |
| `signature_delta`                       | none; the signature goes into the block                          |
| `input_json_delta` on an opaque block   | none; parsed at the stop into the block's `input`                |
| `citations_delta`, unknown deltas       | none                                                             |
| `content_block_stop`                    | `BlockStop` with the complete block                              |
| `message_delta`                         | `Usage`; its `stop_reason` is kept                               |
| `message_stop`                          | `Stop`; the stream ends                                          |
| `ping`, unknown event types             | none                                                             |
| `error`                                 | an `Err` item, mapped by error type; the stream ends             |

The blocks in `BlockStop`:

- `text` becomes `Text`. Citations are dropped; the cited text is kept, and the API accepts the
  text back without them.
- `tool_use` becomes `ToolUse`, with the input from the accumulated JSON, or from the start event
  if no deltas came. Input cut off by `max_tokens` isn't valid JSON, so it becomes `{}` (logged)
  and the block can still be stored and sent back; the core answers it with `truncated`.
- `thinking` becomes `Reasoning { text, opaque }`: `text` is `None` when empty (the
  `display: "omitted"` default on newer models), and `opaque` is `{api: "anthropic", data:
  {"signature": ...}}`. The request mapping rebuilds the `thinking` block from both.
- Everything else (`redacted_thinking`, `server_tool_use`, `web_search_tool_result`, and types added
  later) becomes `Opaque { api: "anthropic", data }` holding the whole block as received, sent back
  unchanged.

Usage: Anthropic's `input_tokens` already excludes cache reads and writes, so the mapping is
direct: `input_tokens`, `output_tokens`, `cache_read_input_tokens` to `cache_read_tokens`, and
`cache_creation_input_tokens` to `cache_write_tokens`. `message_start` sets the totals and each
`message_delta` replaces the fields it carries (its counts are cumulative; server tools can update
the input counts at the end). Every `Usage` event carries the running totals, so the last one wins.

Stop reasons: `end_turn` is `EndTurn`, `tool_use` is `ToolUse`, `max_tokens` and
`model_context_window_exceeded` are `MaxTokens`, and `refusal` is `Refusal`. `stop_sequence` and
`pause_turn` are `EndTurn`: Giles sends no stop sequences and configures no server tools, so neither
should occur; if one does, the turn ends with what was produced. An unknown reason is logged and
ends the turn.

Mid-stream `error` events map by error type: `overloaded_error`, `api_error`, `rate_limit_error`,
and `timeout_error` are `Error::Unavailable` (retryable); `not_found_error` is `Error::NotFound`;
`invalid_request_error`, `authentication_error`, `permission_error`, `billing_error`, and
`request_too_large` are `Error::InvalidRequest`; others are `Error::Other`.

On Claude Fable 5.1, Opus 5.5, Sonnet 5.5, and Haiku 5.5, a replayed thinking signature is bound
to the system prompt, tools, and messages before it: for accounts created from 2026-08-31, a
replayed block whose earlier history changed is rejected with a 400. Transcripts are append-only
within a session, so this only matters where the core rewrites history: after compaction (PLAN
section 6.8), the kept messages' `Reasoning` blocks follow a new system prompt and summary, so
compaction has to drop them from the history it sends.

### Errors

`complete` returns an error before the stream starts when the request can't be built (a media read
fails, a non-text system block), the connection fails, or the API answers with an error status. Once
the response is an event stream, failures arrive as an `Err` item that ends the stream.

| Failure                                  | Error                                            |
| ---------------------------------------- | ------------------------------------------------ |
| connect, TLS, timeout, reset             | `Unavailable`                                    |
| HTTP 408, 429, 5xx (529 is "overloaded") | `Unavailable` with `retry_after` from the header |
| HTTP 404                                 | `NotFound`                                       |
| other HTTP 4xx                           | `InvalidRequest`                                 |
| a success that isn't `text/event-stream` | `Other`                                          |
| `error` event                            | by error type (see Stream mapping)               |
| the body ends before `message_stop`      | `Unavailable`                                    |
| a malformed or out-of-order event        | `Other`                                          |

Error messages are `anthropic <status> <error type>: <message>` from the API's error body (or the
first 64 KiB of a body that isn't the API's JSON). The core retries `Unavailable` on the next model
(PLAN section 6.10).

### Models

`list_models` reads every page of `GET /v1/models?limit=1000`, following `after_id` while
`has_more`. The API reports `max_input_tokens`, `max_tokens`, and `capabilities` per model; each
becomes `ModelInfo`:

- `context_window` and `max_output_tokens` from `max_input_tokens` and `max_tokens`, else from the
  built-in family table, else unknown.
- `media_types`: the image types if `capabilities.image_input` is supported, plus `application/pdf`
  if `capabilities.pdf_input` is; without capabilities, all five.
- `reasoning` from `capabilities.thinking`, else whether the family is in the table.

The family table (`models.rs`) lists the current and legacy model families with their context
window, maximum output, and whether they take thinking budgets or adaptive thinking. It's the
fallback for gateways or API versions that leave fields out, and it chooses the default
`max_tokens` and the `thinking` form for a request without calling `list_models`. Prefixes match
whole id segments: `claude-opus-4-5` matches `claude-opus-4-5-20251101` but not
`claude-opus-4-50`.

### Tests

Unit tests next to each module cover the pure parts: SSE parsing at every chunk boundary, config
validation, request bodies, the stream state machine, and model mapping. `tests/anthropic.rs` runs
the provider against a local HTTP server on 127.0.0.1 (`tests/support/`, shared by the provider
tests) that records requests and replays hand-written responses in the documented wire format
(`tests/fixtures/anthropic/*.sse`, `models_page*.json`) in 7-byte chunks. It checks exact request
bodies and headers, exact event sequences, that the stored blocks of a response reproduce the API's
blocks when sent back, and the error mapping. Nothing calls the real API.

## OpenAI-compatible

`providers::openai` speaks the
[Chat Completions API](https://platform.openai.com/docs/api-reference/chat) (`POST
<base_url>/chat/completions` with `stream: true`), which OpenAI, OpenRouter, Ollama, vLLM,
llama.cpp, and most gateways serve. Its API family name, `Opaque::api`, is `openai`
(`openai::API`).

```rust
let config = OpenAiConfig::from_json(&instance_config_json)?;
let provider = OpenAiProvider::new(config, media_source)?; // impl giles_types::Provider
```

`OpenAiProvider::new(OpenAiConfig, Arc<dyn MediaSource>)` builds the HTTP client and sends nothing.
Requests carry `Authorization: Bearer <api_key>` when there is a key (marked sensitive) and the
configured extra headers.

### Configuration

`OpenAiConfig::from_json` validates the instance's `config` table like the Anthropic one; the keys
are in [Providers](../user/providers.md). The struct:

- `api_key: Option<http::ApiKey>`: optional, since local servers need none.
- `base_url`: the API root including its version path (`https://api.openai.com/v1`,
  `https://openrouter.ai/api/v1`, `http://localhost:11434/v1`); paths `/chat/completions` and
  `/models` are appended.
- `dialect: Dialect` (`OpenAi`, `OpenRouter`, `Generic`): from the `base_url` host
  (`api.openai.com`, `openrouter.ai`, anything else) unless the `dialect` key sets it, e.g. for a
  proxy in front of OpenAI. It picks the few request fields that differ (below).
- `headers: HeaderMap`: extra headers such as OpenRouter's `HTTP-Referer` and `X-Title`. Values are
  marked sensitive, since some gateways authenticate with a custom header (Azure's `api-key`), so
  `Debug` shows `Sensitive`. `Authorization` (use `api_key`) and headers the provider or the HTTP
  client sets (`Accept`, `Content-Type`, `Content-Length`, `Host`, `Connection`,
  `Transfer-Encoding`) are rejected. Errors name the header, never its value.
- `defaults: ModelSettings` and `models: BTreeMap<String, ModelSettings>`: what the provider needs
  to know about a model that the API doesn't report. `ModelSettings { media_types, context_window,
  max_tokens, reasoning }`, every field optional; `settings(model)` takes the model's own fields
  over the instance's, field by field.

### Media

Compatible servers differ in which media they accept, and `/models` doesn't say, so the config
declares it. `media_types(model)` is the model's declared list, else the instance's, else the
dialect's default: the four common image types (`openai::DEFAULT_IMAGE_TYPES`) for OpenAI and
OpenRouter, nothing for other servers (vision varies by local model, and sending an image to a
model without vision fails every later request). A declared type must be one the API has a content
part for: an image type, `audio/wav`, `audio/mpeg`, or `application/pdf`; anything else is a config
error.

| Media type (declared) | Content part                                                            |
| --------------------- | ----------------------------------------------------------------------- |
| `image/*`             | `image_url` with a `data:<type>;base64,...` URL                         |
| `audio/wav`           | `input_audio` with `format: "wav"`                                      |
| `audio/mpeg`          | `input_audio` with `format: "mp3"`                                      |
| `application/pdf`     | `file` with `filename` (`Document.name`, the media name, or `<id>.pdf`) |
| anything undeclared   | `text` with the reference `[media:<id> <name> <type>]`, no bytes read   |

The part follows the media type, whatever the block variant. As for Anthropic, the core is expected
to filter media by `ModelInfo::accepts`; this is the backstop.

### Request mapping

| Giles                                     | Chat Completions                                                               |
| ----------------------------------------- | ------------------------------------------------------------------------------ |
| `system` (text blocks only)               | one `system` message, the texts joined by blank lines; omitted when empty      |
| `User` message                            | one `user` message; `content` is a string for a single text, else parts        |
| `ToolResult` (in a `User` message)        | one `tool` message per result, in order, with `tool_call_id`                   |
| media in a `ToolResult`                   | its reference in the `tool` text; if declared, moved to a following `user`     |
| `is_error: true`                          | the `tool` text prefixed with `Error:` (the API has no error flag)             |
| `Assistant` message                       | `content` (texts joined by blank lines, `null` without text) and `tool_calls`  |
| `ToolUse`                                 | a `tool_calls` entry, `type: "function"`, `arguments` as compact JSON text     |
| `Reasoning` with `opaque` `{api: openai}` | its text in the stored field (`reasoning`, `reasoning_content`); not to OpenAI |
| other `Reasoning`, any `Opaque`           | dropped (PLAN section 5)                                                       |
| empty text                                | dropped; a message left empty is skipped                                       |
| `ToolSpec`                                | `{type: "function", function: {name, description, parameters}}`                |

Consecutive user messages stay separate: Chat Completions doesn't require alternating roles, and
keeping them apart keeps each stored message's time prefix on its own. Within a run of consecutive
user messages, though, every tool result is sent first, because the `tool` messages answering an
assistant's `tool_calls` must follow it directly. The run becomes its `tool` messages, then one
`user` message with the relocated media (each part preceded by its reference text, so the model can
tie it to the `tool` message that names it), then the run's other content. `tool` messages hold
only text (PLAN section 5), hence the relocation. A `ToolUse` in a user message, a `ToolResult` in
an assistant message, and anything but text and media in a tool result are `Error::InvalidRequest`.

Generation parameters depend on the dialect:

| `GenerationParams`       | OpenAI                                    | OpenRouter                   | Generic                       |
| ------------------------ | ----------------------------------------- | ---------------------------- | ----------------------------- |
| `max_tokens`             | `max_completion_tokens`                   | `max_tokens`                 | `max_tokens`                  |
| `temperature`            | sent, except to declared reasoning models | sent                         | sent                          |
| `reasoning_budget: Some` | `reasoning_effort` (declared reasoning)   | `reasoning: {max_tokens: n}` | `reasoning_effort` (declared) |
| `reasoning_budget: None` | nothing                                   | nothing                      | nothing                       |
| `cache_retention`        | nothing (caching is automatic)            | top-level `cache_control`    | nothing                       |

- `max_tokens` is the request's, else the model's or instance's `max_tokens` setting, else not
  sent (the server's default). OpenAI deprecated `max_tokens` and its reasoning models reject it;
  other servers may not know `max_completion_tokens`.
- A reasoning budget becomes an effort level, `low` up to 2,048 tokens, `medium` up to 8,192, and
  `high` above. It's sent only to models declared `reasoning = true`, because OpenAI rejects
  `reasoning_effort` for models that don't reason and other servers may too. OpenRouter takes a
  token budget directly and ignores it for models that don't reason, so it always gets one. A
  `None` budget sends nothing: there is no value every model accepts for "off" (OpenAI's `none`
  is GPT-5.1 and later only), so the model keeps its default.
- OpenAI's reasoning models accept only the default temperature, so it's left out for models
  declared `reasoning` on the OpenAI dialect.
- On OpenRouter, a top-level `cache_control` (`{type: "ephemeral"}`, with `ttl: "1h"` for
  `CacheRetention::Long`) turns on Anthropic's automatic caching for Claude models; OpenRouter
  applies it only where the upstream provider needs it. OpenAI, DeepSeek, Gemini, and local servers
  cache prefixes on their own.
- The system prompt goes in a `system` message rather than OpenAI's newer `developer` role, which
  other servers may not know; OpenAI treats `system` as `developer` for reasoning models.

### Stream mapping

`StreamMapper` turns the chunks into `CompletionEvent`s. Chat Completions has no block events: a
chunk's `choices[0].delta` may carry reasoning, text, and tool call fragments, and a server may
interleave them. The mapper opens a block when a different kind of content arrives, closing the
open one with `BlockStop` first, so blocks never overlap and indexes count from 0. Within a delta it
takes reasoning, then text, then tool calls. Only choice 0 is read.

| Chunk content                                      | `CompletionEvent`                                            |
| -------------------------------------------------- | ------------------------------------------------------------ |
| `delta.reasoning` or `delta.reasoning_content`     | a `Reasoning` block (opened if needed) and `ReasoningDelta`  |
| `delta.content`, `delta.refusal`                   | a `Text` block (opened if needed) and `TextDelta`            |
| `delta.tool_calls[]` starting a call (new `index`) | `BlockStart { kind: ToolUse, tool: Some((id, name)) }`       |
| `function.arguments` fragments                     | `ToolInputDelta`; accumulated and parsed at the block's stop |
| `finish_reason`                                    | `BlockStop` for the open block; the reason is kept           |
| `usage` (any chunk; normally the last, no choices) | `Usage`                                                      |
| `data: [DONE]`                                     | `Stop`; the stream ends                                      |
| `error` object                                     | an `Err` item; the stream ends                               |
| empty deltas, `role`, `reasoning_details`, others  | none                                                         |

Tool calls are matched by `id` and `index`. A fragment continues the open call when it carries no
id (or the open call's id) and its index matches the open call's or is absent. Any other fragment
starts a call, which must carry `id` and `function.name` (else `Error::Other`). A new id always
starts a call, whatever its index: Ollama sends index 0 for every call of a response, each with
its own id. Several calls in one chunk, whole calls in one fragment (Ollama), and `arguments` sent
as a JSON object rather than text are handled. A fragment without a new id for a call index
already seen, after the call was closed (because other content came between), is `Error::Other`,
since a closed block can't be reopened. Arguments that don't parse (cut off by the output limit)
become `{}`, logged, as with Anthropic; no arguments are `{}`.

Reasoning becomes `Reasoning { text, opaque: {api: "openai", data: {"field": <field>}} }`, the
field being the delta field it streamed in. The request mapping sends the text back in that field
of the assistant message, which DeepSeek requires during tool use (`reasoning_content`) and Ollama
and OpenRouter read (`reasoning`). Every instance of this component shares the `openai` family, so
after a fallback reasoning from one server can reach another; the OpenAI dialect never gets the
field, since OpenAI doesn't stream reasoning in Chat Completions and may reject unknown fields.
OpenRouter's `reasoning_details` (encrypted or signed reasoning of the upstream model) isn't kept.

Stop reasons, applied at `[DONE]`: `tool_calls` (and the legacy `function_call`) is `ToolUse`,
`length` is `MaxTokens`, `content_filter` is `Refusal`, and `stop` is `EndTurn`. A response that
made tool calls stops with `ToolUse` whatever the reason (older Ollama reported `stop`), and one
that streamed `refusal` text with `stop` is `Refusal`. Unknown reasons are logged and end the turn.

Usage: `prompt_tokens` counts all input, so `input_tokens = prompt_tokens - cached - cache writes`
(saturating), `cache_read_tokens` is `prompt_tokens_details.cached_tokens` (DeepSeek:
`prompt_cache_hit_tokens`), `cache_write_tokens` is `prompt_tokens_details.cache_write_tokens`
(OpenRouter, and OpenAI on models that charge for writes), else 0, and `output_tokens` is
`completion_tokens` (reasoning tokens included). The request asks for usage with
`stream_options: {include_usage: true}`; a server that sends it on every chunk just updates the
totals.

Ending: `[DONE]` with a finish reason gives `Stop`. A body that ends after a finish reason but
without `[DONE]` is complete too (not every server sends it). `[DONE]` or the end of the body
without a finish reason is `Error::Unavailable`: the response was cut off.

Mid-stream `error` objects (`{"error": {...}}` in a chunk; some servers send a bare string) map by
an HTTP status given as `code` (OpenRouter's upstream failures, e.g. 502) like an HTTP error,
else by the textual code or type: `server_error`, `api_error`, `overloaded_error`,
`rate_limit_error`, `rate_limit_exceeded`, `timeout`, and `service_unavailable` are `Unavailable`;
`not_found_error` and `model_not_found` are `NotFound`; `invalid_request_error`,
`context_length_exceeded`, `authentication_error`, `permission_error`, and `insufficient_quota`
are `InvalidRequest`; others are `Other`. OpenRouter's `finish_reason: "error"` without an error
object is `Unavailable`.

### Errors

Errors before the stream starts, and HTTP statuses, map as for Anthropic (`http::status_error`:
408, 429, 5xx are `Unavailable` with `Retry-After`; 404 is `NotFound`; other 4xx are
`InvalidRequest`). Messages are `openai <status> <code or type>: <message>` from an
`{"error": {...}}` body (OpenAI's string `code`, such as `rate_limit_exceeded`, is preferred over
its `type`), else the first 64 KiB of the body. Some servers echo the API key in an
authentication error; the provider replaces it with `<redacted>`.

### Models

`list_models` reads `GET <base_url>/models` (one page; the API isn't paginated). OpenAI and
Ollama report only ids, so most of `ModelInfo` comes from the config:

- `context_window`: the configured one, else what the server reports (`context_length` from
  OpenRouter, `max_model_len` from vLLM, `top_provider.context_length`), else unknown.
- `max_output_tokens`: OpenRouter's `top_provider.max_completion_tokens`, else unknown.
- `media_types`: the model's own configured list as is; otherwise `media_types(model)` without the
  types whose modality the server says the model lacks (OpenRouter's
  `architecture.input_modalities`: `image`, `audio`, and `file` for PDFs).
- `reasoning`: configured, else whether the server lists a `reasoning` or `reasoning_effort`
  parameter (OpenRouter's `supported_parameters`), else false.
- `display_name`: OpenRouter's `name`.

### Tests

Unit tests next to each module cover config validation, request bodies (every mapping row,
dialects, relocation, reasoning round trips), the stream state machine (interleaving, parallel and
index-less tool calls, finish reasons, usage math, errors, protocol violations), and model mapping.
`tests/openai.rs` runs the provider against the shared local server with hand-written streams
modeled on OpenAI, OpenRouter (comment keep-alives, `reasoning_details`, cost and cache-write
usage, a mid-stream upstream error), and Ollama (whole tool calls, `reasoning`, no key)
(`tests/fixtures/openai/`). It checks exact request bodies, URLs under each `base_url` shape, and
headers, exact event sequences, that the stored blocks of a response are sent back correctly
(tool calls, relocated tool-result images, reasoning), the HTTP and mid-stream error mapping with
`Retry-After`, key redaction, and the three model lists. Nothing calls a real API.
