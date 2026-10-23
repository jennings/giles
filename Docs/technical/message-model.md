# Message Model and Component Traits

The `giles-types` crate holds the domain types and the three component traits the core uses:
`Provider`, `Channel`, and `Toolset` (PLAN sections 4.3, 4.4, and 5). Built-in components implement
the traits directly. The gRPC adapters (`Remote*`) and the plugin SDK convert between these types
and the `prost`-generated ones at the boundary, so generated types never appear in the core.

Conventions:

- Every type is `Debug`; data types are `Clone` and `PartialEq` too.
- Every public enum is `#[non_exhaustive]`, so adding a variant isn't a breaking change for plugin
  code.
- Ids are validated newtypes. String ids (`EventId`, `MessageId`, `NativeId`, `ConversationId`,
  `UserId`, `AgentId`, `ApprovalId`) reject the empty string. `MediaId` is the SHA-256 of a blob
  as 64 lowercase hex digits. Validation runs in every constructor and on deserialization.
- Times are `giles_types::Timestamp` (a re-export of `jiff::Timestamp`), serialized as RFC 3339.
- Errors are one `Error` enum with a `Result<T>` alias. `Error::Unavailable` (rate limit,
  overload, 5xx, timeout, recipient offline) is the only retryable variant; `InvalidRequest`,
  `NotFound`, `InvalidId`, and `Other` are not.

## Messages

A transcript is a list of `Message`s:

```rust
pub struct Message { pub role: Role, pub content: Vec<Block>, pub meta: MessageMeta }
pub enum Role { User, Assistant }

pub enum Block {
    Text { text: String },
    Image { media: MediaRef },
    Audio { media: MediaRef },
    Document { media: MediaRef, name: Option<String> },
    ToolUse { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: Vec<Block>, is_error: bool },
    Reasoning { text: Option<String>, opaque: Option<Opaque> },
    Opaque(Opaque),
}

pub struct Opaque { pub api: String, pub data: serde_json::Value }

pub struct MessageMeta {
    pub provider: Option<String>, // provider instance; assistant messages only
    pub model: Option<String>,    // model id; assistant messages only
    pub usage: Option<Usage>,     // assistant messages only
    pub at: Timestamp,
    pub sender: Option<UserId>,   // user messages
}

pub struct MediaRef {
    pub id: MediaId, pub media_type: String, pub size: u64, pub name: Option<String>,
}
pub struct Usage {
    pub input_tokens: u64, pub output_tokens: u64,
    pub cache_read_tokens: u64, pub cache_write_tokens: u64,
}
pub enum StopReason { EndTurn, ToolUse, MaxTokens, Refusal }
```

There is no system role: the system prompt is assembled at session start and passed separately in
`CompletionRequest::system`.

`Opaque` holds provider data the core keeps but never interprets, such as Anthropic thinking
signatures (inside `Reasoning`) and server-side tool blocks (as `Block::Opaque`). Its `api` names
the API family that produced it; the data is sent back only to a provider of the same family.

`Usage::input_tokens` counts only input that was neither read from nor written to the prompt
cache. Providers whose API reports a total (OpenAI's `prompt_tokens` includes cached tokens)
subtract the cache counts, so the four fields add up to the request's tokens.

A `MediaRef` points into the blob store; bytes never travel inside messages. It carries the media
type, size, and file name, so the core can render an attachment's text reference and check it
against the model's accepted media types without reading the store.

### JSON representation

The `messages` table stores content blocks as JSON, so the serde form of `Block` is a stored
format. The rules:

- Enums are internally tagged by a `"type"` field in `snake_case`. `Block::Opaque` places the
  `Opaque` fields next to the tag.
- `Option` fields are omitted when `None` and read as `None` when missing. Every other field is
  always written and required.
- Unknown fields are ignored, so a field added later must be optional or have a default.
- An unknown `"type"` fails to deserialize. Skipping a block could drop a `ToolUse` or
  `ToolResult` and break the transcript invariant, so an old binary refuses newer data instead.
  Variants are only added, never renamed or removed.
- Ids serialize as plain strings and times as RFC 3339 strings.
- `Opaque::data` round-trips as JSON values. Object key order isn't preserved, which no provider
  relies on.

Examples, one block per line:

```text
{"type": "text", "text": "hello"}
{"type": "image", "media": {"id": "a3f9…", "media_type": "image/png", "size": 2048}}
{"type": "document", "media": {"id": "a3f9…", "media_type": "application/pdf", "size": 9,
 "name": "q3.pdf"}, "name": "Q3 report"}
{"type": "tool_use", "id": "toolu_1", "name": "github__create_issue", "input": {"title": "Bug"}}
{"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "ok"}],
 "is_error": false}
{"type": "reasoning", "text": "…", "opaque": {"api": "anthropic", "data": {"signature": "…"}}}
{"type": "opaque", "api": "anthropic", "data": {"type": "server_tool_use", "…": "…"}}
```

A whole message:

```json
{
  "role": "assistant",
  "content": [{"type": "text", "text": "hi"}],
  "meta": {
    "provider": "anthropic",
    "model": "claude-sonnet-4-5",
    "usage": {"input_tokens": 10, "output_tokens": 20, "cache_read_tokens": 300,
              "cache_write_tokens": 4},
    "at": "2026-10-22T12:00:01.5Z"
  }
}
```

Channel events (stored in `inbox`) and outbound messages (stored in `outbox`) follow the same
rules. `SendTarget` is the one externally tagged enum: `{"conversation": "#giles"}` or
`{"recipient": "account:stephen"}`.

## Provider

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    async fn list_models(&self) -> Result<Vec<ModelInfo>>;
    async fn complete(&self, req: CompletionRequest)
        -> Result<BoxStream<'static, Result<CompletionEvent>>>;
}
```

`CompletionRequest` has `model`, `system: Vec<Block>`, `messages: Vec<Message>`, `tools:
Vec<ToolSpec>` (prefixed names), and `params: GenerationParams` (`max_tokens`, `temperature`,
`reasoning_budget`, all optional, and `cache_retention`: `Short` by default, `Long` for
human-paced sessions).

The stream reports each content block as `BlockStart { index, kind, tool }` (with the tool use id
and name for a `ToolUse`), then `TextDelta`, `ToolInputDelta`, or `ReasoningDelta` events for that
index, then `BlockStop { index, block }` with the complete block. The core stores the blocks from
`BlockStop`, so `Opaque` data never has to be rebuilt from deltas. `Usage` events carry running
totals for the request; the last one wins. `Stop(StopReason)` ends the stream. Blocks never
overlap: a provider closes the open block before the next `BlockStart`, even if its API interleaves
them, and each `BlockStart` index is one more than the last, starting at 0.

`ModelInfo` declares the model's `context_window`, `max_output_tokens`, whether it does
`reasoning`, and the `media_types` it accepts as input blocks. `ModelInfo::accepts` matches a media
type against that list, ignoring ASCII case and parameters, with no wildcards.

`ToolSpec` is `name`, `description`, and `input_schema` (a JSON Schema object). It carries no
parallel or read-only flags: parallelism is operator config per toolset instance, and MCP
`readOnlyHint` is untrusted.

## Channel

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    fn info(&self) -> &ChannelInfo;
    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>>;
    async fn ack(&self, event: &EventId) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<MessageId>;
    fn deltas(&self) -> Option<&dyn DeltaSink> { None }
}

pub trait DeltaSink: Send + Sync {
    fn start(&self, conversation: &ConversationId) -> Result<Box<dyn DeltaWriter>>;
}

#[async_trait]
pub trait DeltaWriter: Send {
    fn push(&mut self, text: &str);
    async fn finish(self: Box<Self>) -> Result<MessageId>;
}
```

- `deltas()` is the live-output hook of PLAN section 6.9. A channel that streams assistant text
  (in v1 only the built-in web channel) returns `Some` and declares `OutputMode::Deltas`; every
  other channel keeps the default `None` and gets whole chunks through `send`. The agent loop
  (`agent_loop::output`) starts one `DeltaWriter` per assistant text block (at its first
  non-whitespace delta), `push`es each `TextDelta`, and `finish`es the message's writers once the
  response is stored; the message's text is the
  concatenation of the pushed deltas. Dropping a writer unfinished (the turn was stopped or failed)
  leaves what was pushed on the channel, marked as cut off. `send` still delivers whole messages
  on a deltas channel: notices, approval prompts, and any reply the core doesn't stream.
- `ChannelInfo` has the `identity_namespace` (e.g. `whatsapp`, `irc:libera.chat`; the core accepts
  it only if it starts with the channel component's name), `output: OutputMode` (`Deltas` or
  `Blocks`), and `can_initiate`. A plugin reports the namespace in `ConfigureResponse`; the
  `RemoteChannel` adapter puts it here, so the core reads it the same way for every channel.
- `ChannelEvent` is `Message { id, message: InboundMessage }` or `Status { id, status:
  ChannelStatus }`. Every event is acked with its id once the core has stored it.
- `ChannelStatus` is `Connected`, `NeedsAuth { challenge, expires_at }`, or `Error { message }`;
  an `AuthChallenge` is a `QrCode`, a `PairCode`, or a `Url`.
- `InboundMessage` has the `conversation: ConversationRef` (`id`, `kind`: `Direct` or `Group`,
  optional `title`), the canonical `sender: NativeId` with `sender_aliases`,
  `sender_authenticated`, an optional `sender_display_name`, the `text`, `attachments` as
  `MediaRef`s, and optional `message_id`, `reply_to`, and `sent_at`. `message_id` is the message's
  own id on the channel, the one a reply's `reply_to` uses; it isn't always the event id (an email
  event id may be `UIDVALIDITY:UID`).
- `OutboundMessage` has a `target`, CommonMark `markdown`, `attachments`, and an optional
  `reply_to`. The target is `SendTarget::Conversation(ConversationId)` for replies or
  `SendTarget::Recipient(NativeId)` for proactive sends, which needs `can_initiate`.

## Toolset

```rust
#[async_trait]
pub trait Toolset: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>>;
    fn tool_changes(&self) -> Option<BoxStream<'static, Vec<ToolSpec>>> { None }
    async fn list_skills(&self) -> Result<Vec<SkillSummary>>;
    async fn get_skill(&self, name: &str) -> Result<Skill>;
    async fn call(&self, call: ToolCall, ctx: CallContext) -> Result<ToolCallHandle>;
}

pub struct ToolCallHandle {
    pub events: BoxStream<'static, Result<ToolCallEvent>>,
    pub answers: tokio::sync::mpsc::Sender<ApprovalAnswer>,
}
```

Names in this trait are bare; the core adds the instance prefix.

- `ToolCall` has the `ToolUse` `id`, the bare `name`, the JSON `input`, and `media`: the
  `MediaRef`s that `{"$media": "<id>"}` values in the input refer to.
- `CallContext` has the `agent`, the `requester` (`None` for config jobs), and the session's
  `conversation` as a `ChannelConversation` (channel instance and `ConversationRef`; `None` for
  isolated job runs). Core tools use it to scope memory, search, and sends.
- A call's `events` yield `Progress { progress, total, message }` and `ApprovalRequest { id,
  message, schema }` events, then one `Result { content, is_error }`. Each approval request is
  answered with an `ApprovalAnswer { id, decision }`, where the decision is `Approve { content }` or
  `Deny`. Dropping `events` cancels the call. Dropping `answers` only means no more answers will
  come: the call goes on, and pending or later approval requests are denied.
- `SkillSummary` is a skill's `name` and `description`; `Skill` adds the Markdown `body` and the
  skill `dir` holding its resource files.
