# Providers

The built-in providers live in `giles_core::providers`, each behind a Cargo feature of `giles-core`
that the `giles` crate forwards under the same name (PLAN section 4.2). Both implement
`giles_types::Provider` (see [Message Model and Traits](message-model.md)) by translating the
message model to and from the provider's wire format; the core never sees wire formats.

| Feature              | Module                 | API                    |
| -------------------- | ---------------------- | ---------------------- |
| `provider-anthropic` | `providers::anthropic` | Anthropic Messages API |

## Shared plumbing

### HTTP

`providers::http` holds what every HTTP provider needs:

- `client(&HttpOptions)` builds a `reqwest` client on rustls that trusts only the Mozilla roots
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

## Anthropic

`providers::anthropic` speaks the [Messages API](https://platform.claude.com/docs/en/api/messages)
(`POST /v1/messages` with `stream: true`). Its API family name, `Opaque::api`, is `anthropic`.

### Configuration

`AnthropicConfig::from_json` validates the instance's `config` table, as JSON with secret references
already resolved, and returns `Error::InvalidRequest` naming the bad key (never the API key). The
keys are in [Providers](../user/providers.md). `ApiKey` wraps the key so `Debug` shows
`ApiKey(<redacted>)`. The connect and read timeouts (`HttpOptions`) are fields of the struct, not
config keys.

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

| Messages API event                                | `CompletionEvent`                                                           |
| ------------------------------------------------- | --------------------------------------------------------------------------- |
| `message_start`                                   | `Usage`                                                                     |
| `content_block_start` `text`                      | `BlockStart { kind: Text }`                                                 |
| `content_block_start` `tool_use`                  | `BlockStart { kind: ToolUse, tool: Some((id, name)) }`                      |
| `content_block_start` `thinking`                  | `BlockStart { kind: Reasoning }`                                            |
| `content_block_start` of any other type           | `BlockStart { kind: Opaque }`                                               |
| `text_delta`                                      | `TextDelta` (empty deltas are dropped)                                      |
| `input_json_delta` on `tool_use`                  | `ToolInputDelta`; the JSON is accumulated and parsed at the stop            |
| `thinking_delta`                                  | `ReasoningDelta`                                                            |
| `signature_delta`                                 | none; the signature goes into the block                                     |
| `input_json_delta` on an opaque block             | none; parsed at the stop into the block's `input`                           |
| `citations_delta`, unknown deltas                 | none                                                                        |
| `content_block_stop`                              | `BlockStop` with the complete block                                         |
| `message_delta`                                   | `Usage`; its `stop_reason` is kept                                          |
| `message_stop`                                    | `Stop`; the stream ends                                                     |
| `ping`, unknown event types                       | none                                                                        |
| `error`                                           | an `Err` item, mapped by error type; the stream ends                        |

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
