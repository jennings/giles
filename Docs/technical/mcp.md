# MCP Client

Toolsets are MCP servers (PLAN section 7.10). The core's MCP client is `giles_core::mcp`, built on
`rmcp` 3.7 (the official Rust SDK, client features only). One `McpToolset` is one connection to one
server and implements the `giles_types::Toolset` trait, so the rest of the core never sees MCP.

## Connecting

| Constructor                                                            | Transport                                       |
| ---------------------------------------------------------------------- | ----------------------------------------------- |
| `McpToolset::connect_stdio(instance, cmd, secrets, trampoline, media)` | Spawns a `tokio::process::Command`, stdio pipes |
| `McpToolset::connect_http(instance, &server, blobs)`                   | Streamable HTTP, optional bearer token          |
| `McpToolset::connect_io(instance, rd, wr, media)`                      | An open byte stream pair (in-process servers)   |

`media` (`McpMedia::new(blobs)`) and `blobs` name the blob store that keeps the media the server's
tools return (see result content below).

Every constructor returns once the server has answered `initialize` and the client has sent
`notifications/initialized`: the handshake is the readiness signal. A server gets `INIT_TIMEOUT` (30
s) to answer. The client introduces itself as `giles` with the crate version and asks for protocol
version 2025-11-25 (`ProtocolVersion::LATEST_WITH_INITIALIZE`, the newest one that still has
`initialize`). Its only client capability is form-mode elicitation (`"elicitation": {"form": {}}`):
no roots, no sampling, no URL-mode elicitation.

A server that answers with an older version is accepted, not refused: PLAN section 8.6's 2025-06-18
is the version the core targets, not a floor. Such a server just lacks the newer features; before
2025-06-18 there is no elicitation, no structured tool output (`structuredContent`), and no resource
links, and 2024-11-05 also has no audio content or progress messages.

`instance` is the `[toolset.<instance>]` name. It appears in every error message and as the field of
the `mcp` tracing span around the connection. Tool names stay bare; the registry adds the
`<instance>__` prefix (PLAN section 6.5).

**Secrets.** Each connection has the instance's secret values: a stdio server's `env` values (passed
to `connect_stdio`) or a remote server's bearer token. They are replaced with `<redacted>` by
`config::scrub` in everything the client logs or returns: each stderr line, and every error message,
since server error messages and HTTP error bodies can echo a credential back.

**Closing.** `McpToolset::closed()` resolves once the connection has ended (the server hung up or
exited, or the transport failed). A transport wrapper reports the end when the server's stream ends
or `rmcp`'s service loop drops it. For a stdio server it returns the exit status, waiting up to 2 s
for it, so the [toolset host](toolsets.md#lifecycle) can report it (a sandbox trampoline failure
exits with 125, 126, or 127).

### Stdio

`connect_stdio` takes a prepared `Command` and the secrets to scrub, and replaces the command's
stdin, stdout, and stderr with pipes. `DirectCommand { program, args, env, cwd }` builds the direct,
unsandboxed command (`command()`: a cleared environment plus the daemon's `PATH` and the configured
`env`, whose values are `config::Secret`s) and its secrets (`secrets()`). The toolset launcher
builds its own `Command` instead, running the server through the sandbox trampoline
(`giles sandbox-exec <spec> -- <command>`), and passes it to the same constructor with the
instance's `env` values ([Toolsets](toolsets.md)).

- **Stderr** is read line by line, scrubbed, and logged at `info` with target
  `giles_core::mcp::stderr` inside the `mcp{instance=...}` span, so
  `GILES_LOG=giles_core::mcp::stderr=warn` silences chatty servers. Lines are cut at 4 KiB (marked
  `…`); a cut line that ends in the first bytes of a secret has that end redacted too. Invalid UTF-8
  is replaced.
- **The child** runs as the leader of its own process group and is owned by a supervisor task. If
  the server exits by itself the task logs and publishes the exit status (`closed()` returns it);
  when the `McpToolset` is dropped it kills the server right away, without waiting for it to notice
  its stdin closing. Either way the whole process group gets `SIGKILL` before the server is reaped
  (while its pid still reserves the group id), so children a server started die with it, as for
  plugins ([Plugin Host](plugin-host.md)). `kill_on_drop` backs this up for the server itself if the
  runtime is already gone.
- Failing to spawn, the server exiting during the handshake, and no answer in time are all
  `Error::Unavailable`. If the server exited, the message ends with its status, e.g.
  `(server process: exit status: 125)`. The last argument, `trampoline`, says the command runs the
  sandbox trampoline: then an exit that `sandbox::is_trampoline_failure` recognizes (125, or 71 from
  macOS `sandbox-exec`) is `Error::InvalidRequest` with the same message, since the trampoline
  failed before running the server and retrying won't help.

### Streamable HTTP

`HttpServer { url, bearer, connect_timeout }` describes a remote server: an `http` or `https`
`url::Url` (any other scheme is `InvalidRequest`), an optional bearer token (a `config::Secret`),
and a connect timeout (10 s by default). The deprecated HTTP+SSE transport isn't supported (PLAN
section 7.10).

- **Auth.** The token is sent as `Authorization: Bearer <token>` on every request; `reqwest` marks
  the header sensitive so its own logging hides it, and the token is scrubbed from error messages
  (an HTTP error body can echo it). A 401 or 403 during the handshake, with or without
  `WWW-Authenticate`, is `InvalidRequest` ("rejected the credentials"), not `Unavailable`: retrying
  the same token won't help. OAuth is not supported.
- **HTTP client.** `net::tls_client_builder()`, shared with the built-in providers: rustls trusting
  only the bundled Mozilla roots (`webpki-root-certs`), no native-tls or OpenSSL, and no redirects,
  so the token can't be forwarded to another host. MCP adds the connect timeout and no read timeout:
  the server's event stream can be quiet for a long time.
- Sessions, the `GET` event stream, and session re-creation after an expired session (HTTP 404) are
  `rmcp`'s defaults.

## Mapping MCP to the Toolset Trait

| MCP                                | `Toolset`                                                    |
| ---------------------------------- | ------------------------------------------------------------ |
| `tools/list`, every page           | `list_tools()`: bare `ToolSpec`s, sorted by name             |
| `notifications/tools/list_changed` | `tool_changes()`: the whole new list, re-read on each change |
| `tools/call`                       | `call()`: a `ToolCallHandle`                                 |
| `notifications/progress`           | `ToolCallEvent::Progress`                                    |
| `CallToolResult`                   | `ToolCallEvent::Result { content, is_error }`                |
| `notifications/cancelled`          | Sent when the handle's `events` stream is dropped            |
| `elicitation/create` (form mode)   | `ToolCallEvent::ApprovalRequest` of the call that asked; the answer on `answers` |

**Tool specs.** `name`, `description` (the `title` if there is no description, else empty), and
`inputSchema` as is. `annotations` are ignored, as PLAN section 7.10 says. `list_tools` follows
`nextCursor` for at most 100 pages and fails beyond that, so a server that loops can't hang the
core.

**List changes.** `tool_changes()` is `Some` only if the server declared `tools.listChanged` at
initialization. Each notification bumps a counter; the stream re-reads the full list and yields it.
Notifications that arrive while a list is being read coalesce into one more read. A failed re-read
is logged and skipped, except a lost connection, which ends the stream; dropping the toolset also
ends it. The [toolset host](toolsets.md#tool-list-cache) caches the lists (PLAN section 7.9).

**Calls.** `call` sends `tools/call` with the input object as `arguments` (`null` means none; any
other non-object is `InvalidRequest`) and returns as soon as the request is queued. A task per call
then forwards events:

- Progress: `rmcp` puts a fresh progress token on every request. Notifications for the token become
  `Progress` events. `rmcp` handles each notification on its own task, so they can arrive out of
  order; one that doesn't increase the progress is dropped, which keeps the trait's promise that
  progress increases. Notifications that arrive before the call has subscribed to its token (it
  learns the token only once the request is queued) are held in a small ring and handed over. A
  consumer more than 64 events behind misses newer progress, never the result.
- Result: the last event, then the stream ends. Progress already received when the result arrives
  goes first.
- A JSON-RPC error response or a lost connection is the last item instead, as an `Err`.

**Cancellation.** Dropping `events` before the result makes the call's task send
`notifications/cancelled` with the request id and the reason "the caller stopped waiting". `rmcp`
then drops any late response. The server cancels the handler (an `rmcp` server cancels the request's
cancellation token).

**Result content.** Text content becomes `Block::Text`. So does the text of an embedded text
resource; a resource link becomes `[resource link: <uri>]`. Images, audio, and blob resources go to
the blob store (`mcp/media.rs`): the base64 is decoded (padding optional) and stored with its media
type, and the item becomes the media block for that type (`media::block`: `Image`, `Audio`, or
`Document` named after the last segment of a resource's URI). When the agent loop stores the result,
each media block gets its `[media:<id> <name> <type>]` reference, so the model can pass it on
(`send_attachment`, `{"$media": "<id>"}`), and a model that doesn't take the type sees only the
reference. The media type is the server's, lowercased and without parameters, and
`application/octet-stream` unless it is a plain `type/subtype` (at most 127 characters), since
channels and providers act on it. An item over `MAX_RESULT_MEDIA_BYTES` (50 MiB decoded, the same as
`DEFAULT_MAX_BLOB` for plugins), with invalid base64, or that can't be stored (logged) is replaced
with a text placeholder, e.g. `[image (image/png, 5120 bytes) dropped: invalid base64]`. If the
result has no text content but has `structuredContent`, that JSON is the text. `isError` (default
false) becomes `is_error`.

**Elicitation** (`mcp/elicitation.rs`, PLAN sections 6.4 and 7.10). A server may ask a question
while a call runs. The client hands it to the call it is for, whose task emits
`ToolCallEvent::ApprovalRequest { id: "elicitation-<n>", message, schema }` (`schema` is the
`requestedSchema` as JSON); the agent loop asks the turn's requester through the channel
([Approvals](approvals.md)) and sends the `ApprovalAnswer` back on `answers`. The answer becomes the
`ElicitResult` (MCP 2025-06-18):

| Answer                                             | `ElicitResult`                         |
| -------------------------------------------------- | -------------------------------------- |
| `Approve { content: None }` (v1's `yes`)            | `accept` with `content: {}`            |
| `Approve { content: Some(object) }`                | `accept` with the object as `content`  |
| `Approve { content: Some(non-object) }`            | `cancel` (not a valid answer)          |
| `Deny` (`no`, expired, superseded by a message, unattended) | `decline`                     |
| `answers` dropped, now or before the question      | `decline`                              |
| The call ends first (result, deadline, cancellation), or the server cancels the question | `cancel` |

A v1 answer is only yes or no, so `yes` sends an empty object: that satisfies a schema with no
required fields (a confirmation), and `rmcp`'s typed `elicit` treats `accept` without content as an
error. A question whose `requestedSchema.required` names any field can't be answered that way, so it
is declined at once (logged), without asking anyone. URL-mode questions (not declared) are declined
too.

**Which call asked.** MCP has no field linking a question to a request. Over streamable HTTP a
question arrives on the response stream of the `tools/call` that asked it, which `rmcp` records in
the request's extensions (`InboundStreamOrigin::OutboundRequest(id)`); the question goes to that
call. A question on HTTP's standalone stream (`InboundStreamOrigin::Unassociated`) is unrelated to
any running request, as the transport spec says, and is declined. Over stdio and in-process pipes
(no extension) nothing tells calls apart, so the question goes to the only call running, and is
declined (logged) when there is none or there are several: guessing could show one person another
conversation's question and let them answer it. Every call registers in the connection's
`CallRouter` before its request goes out and records the request id once it's sent; a question
carrying the id of a call still being sent waits for it. Answers have no timeout of their own: the
approval's `approval_timeout` denies an unanswered one, and the call's deadline (which an
elicitation wait counts against, unlike an `ask` approval) ends the call, which cancels the
question.

**Skills.** MCP has no skills, so a bare `McpToolset`'s `list_skills` is empty and `get_skill` is
`NotFound`. A toolset's `skills = [...]` directories come from its configuration: the toolset host's
`ManagedToolset` serves them ([Toolsets](toolsets.md#lifecycle)) and the registry indexes them as
`<instance>__<skill>` for the agent.

**Media inputs** (PLAN sections 5 and 7.10). The dispatcher resolves each `{"$media": "<id>"}` in a
tool input against the session's media and puts the media in `ToolCall::media`
([Agent Loop](agent-loop.md)). For a connection made with `McpMedia::files_in(dir)` (the toolset
launcher passes the stdio server's `TMPDIR`, `data/plugins/<instance>/tmp/`), `call` copies each
blob into `dir` and replaces every reference object with the copy's absolute path, as a string,
before sending `tools/call`; an id missing from `ToolCall::media` is `InvalidRequest`
(`unknown media <id>`), and so is an input that is a reference itself. The copies (`mcp/files.rs`):

- are named `giles-media-<16 random hex>-<name>`: the media's name reduced to its last path
  component and ASCII letters, digits, `.`, `-`, `_` (others become `_`), at most 100 bytes keeping
  a short extension, `media` without a name, and the media type's usual extension added when the
  name has none (servers often go by it);
- are created mode 0600 with `openat(O_CREAT | O_EXCL | O_NOFOLLOW)` through one descriptor of the
  directory, itself opened with `O_NOFOLLOW`. The server can write its `TMPDIR` and the data
  directory around it, so it could put a link where the core is about to write; this way a link (in
  place of the directory or of a file) only makes the copy fail, and nothing is written outside the
  directory;
- are removed (`unlinkat` through the same descriptor, which never follows a link) when the call's
  task ends: with the result, an error, or cancellation. A failed copy removes the ones already
  made. Copies a crash left behind are removed when the server next starts (`connect_stdio` sweeps
  `giles-media-*` entries before spawning it), since the core keeps no list across restarts.

The blob reader checks the content's hash as it copies, so a damaged blob fails the call instead of
reaching the server. Other connections (remote servers, and `McpMedia::new` alone) take no media: a
call with `ToolCall::media` is `InvalidRequest` (`remote toolsets take no media inputs`), as PLAN
section 7.10 says for v1. Without media the input is sent unchanged.

## Errors

| Failure                                                  | `giles_types::Error`               |
| -------------------------------------------------------- | ---------------------------------- |
| Connection closed, transport failure, server exited      | `Unavailable` (retryable)          |
| HTTP 401 or 403 during `initialize`                      | `InvalidRequest`                   |
| JSON-RPC `-32600`, `-32601`, `-32602` (e.g. unknown tool) | `InvalidRequest`, server's message |
| Any other JSON-RPC error                                 | `Other`, server's message          |
| No common protocol version                               | `InvalidRequest`                   |
| Sandbox trampoline failed (stdio with `trampoline`, exit 125, or 71 on macOS) | `InvalidRequest` |

Messages start with `MCP server <instance>:` and have the instance's secrets scrubbed. A tool that
fails by its own account (`isError: true`) is a normal `Result` event, not an error.

## Tests

`crates/giles-core/tests/support/mcp_server.rs` is a small `rmcp` server (paginated tools,
`list_changed`, gated progress, a tool that waits to be cancelled, a tool that asks a question by
elicitation and reports the answer, media results, a tool that reads the file at a path, exit). It
runs:

- in process over a duplex pipe (`tests/mcp.rs`), with a relay task in between whose abort stands in
  for a crash; it also checks that an image, audio, and a PDF resource come back as media blocks
  whose blobs hold the bytes the server sent, that a question is an approval request whose answers
  reach the server as `accept` with content, `accept` with `{}`, and `decline`, that a question
  whose schema requires a field is declined without asking, that a question is declined once
  `answers` is dropped, and that with two calls running on the pipe a question is declined without
  asking anyone;
- as a real stdio child (`tests/mcp_stdio.rs`): the test binary re-executes itself with
  `GILES_MCP_TEST_SERVER` set and serves MCP instead of running tests. Libtest would print to
  stdout, so this target has `harness = false` and its `main` runs the tests. It checks stderr
  logging with an echoed `env` secret redacted, cancellation reaching the child, a crash and the
  exit status from `closed()`, the exit status in a failed handshake's message (a child that exits
  with 125: `Unavailable` for a plain command, `InvalidRequest` when it stands for the sandbox
  trampoline, while another status stays `Unavailable`), that dropping the toolset kills a server
  that ignores stdin closing, and that the server's own child dies with it, whether the toolset is
  dropped or the server crashes.
- over streamable HTTP (`tests/mcp_http.rs`): `rmcp`'s `StreamableHttpService` in `axum` on
  `127.0.0.1`, behind a middleware that requires the bearer token. It checks listing, progress, and
  cancellation with the right token; rejection of a wrong, missing, or forbidden one (the 403 body
  echoes the token, which must come back scrubbed); an unreachable server; that a call with media is
  refused; and that with two calls running, a question reaches the one whose response stream it came
  on.

`mcp/files.rs` unit tests cover the copy (contents, mode 0600, gone on drop, nothing left after a
missing blob), a link in place of the directory (refused, nothing written where it points), the
sweep (only `giles-media-*` entries, a link removed rather than followed), and safe file names. The
[toolsets tests](toolsets.md#tests) hand a file to a real server, sandboxed and not.
`mcp/elicitation.rs` unit tests cover routing (by stream, the lone call without stream information,
nobody on the standalone stream, waiting for a call being sent, a call ending while a question
waits), the answer mapping, and the waiting questions. `tests/approvals.rs` runs the test server as
a toolset of a whole daemon: its question reaches the owner through the channel, `yes` and `no` come
back as `accept` (`{}`) and `decline`, and a question left unanswered ends with the call's deadline,
its prompt then cancelled.
