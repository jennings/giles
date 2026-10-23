# MCP Client

Toolsets are MCP servers (PLAN section 7.10). The core's MCP client is `giles_core::mcp`, built on
`rmcp` 3.7 (the official Rust SDK, client features only). One `McpToolset` is one connection to one
server and implements the `giles_types::Toolset` trait, so the rest of the core never sees MCP.

## Connecting

| Constructor                                         | Transport                                       |
| --------------------------------------------------- | ----------------------------------------------- |
| `McpToolset::connect_stdio(instance, cmd, secrets)` | Spawns a `tokio::process::Command`, stdio pipes |
| `McpToolset::connect_http(instance, &server)`       | Streamable HTTP, optional bearer token          |
| `McpToolset::connect_io(instance, rd, wr)`          | An open byte stream pair (in-process servers)   |

Every constructor returns once the server has answered `initialize` and the client has sent
`notifications/initialized`: the handshake is the readiness signal. A server gets `INIT_TIMEOUT`
(30 s) to answer. The client introduces itself as `giles` with the crate version and asks for
protocol version 2025-11-25 (`ProtocolVersion::LATEST_WITH_INITIALIZE`, the newest one that still
has `initialize`). It declares no client capabilities: no roots, no sampling, no elicitation.

A server that answers with an older version is accepted, not refused: PLAN section 8.6's 2025-06-18
is the version the core targets, not a floor. Such a server just lacks the newer features; before
2025-06-18 there is no elicitation, no structured tool output (`structuredContent`), and no
resource links, and 2024-11-05 also has no audio content or progress messages.

`instance` is the `[toolset.<instance>]` name. It appears in every error message and as the field
of the `mcp` tracing span around the connection. Tool names stay bare; the registry adds the
`<instance>__` prefix (PLAN section 6.5).

**Secrets.** Each connection has the instance's secret values: a stdio server's `env` values (passed
to `connect_stdio`) or a remote server's bearer token. They are replaced with `<redacted>`
by `config::scrub` in everything the client logs or returns: each stderr line, and every error
message, since server error messages and HTTP error bodies can echo a credential back.

**Closing.** `McpToolset::closed()` resolves once the connection has ended (the server hung up or
exited, or the transport failed). A transport wrapper reports the end when the server's stream ends
or `rmcp`'s service loop drops it. For a stdio server it returns the exit status, waiting up to 2 s
for it, so task 2.20's supervision can tell a trampoline failure (exit 125, 126, 127) from a crash.

### Stdio

`connect_stdio` takes a prepared `Command` and the secrets to scrub, and replaces the command's
stdin, stdout, and stderr with pipes. `DirectCommand { program, args, env, cwd }` builds the
direct, unsandboxed command (`command()`: a cleared environment plus the daemon's `PATH` and the
configured `env`, whose values are `config::Secret`s) and its secrets (`secrets()`). Task 2.19
builds its own `Command` instead, running the server through the sandbox trampoline
(`giles sandbox-exec <spec> -- <command>`), and passes it to the same constructor with the
instance's `env` values.

- **Stderr** is read line by line, scrubbed, and logged at `info` with target
  `giles_core::mcp::stderr` inside the `mcp{instance=...}` span, so
  `GILES_LOG=giles_core::mcp::stderr=warn` silences chatty servers. Lines are cut at 4 KiB (marked
  `…`); a cut line that ends in the first bytes of a secret has that end redacted too. Invalid
  UTF-8 is replaced.
- **The child** is owned by a supervisor task. If the server exits by itself the task logs and
  publishes the exit status (`closed()` returns it); when the `McpToolset` is dropped it kills and
  reaps the child right away, without waiting for the server to notice its stdin closing.
  `kill_on_drop` backs this up if the runtime is already gone. Only the direct child is killed; a
  server that forks its own children relies on them exiting when their pipes close.
- Failing to spawn, the server exiting during the handshake, and no answer in time are all
  `Error::Unavailable`. If the server exited, the message ends with its status, e.g.
  `(server process: exit status: 125)`.

### Streamable HTTP

`HttpServer { url, bearer, connect_timeout }` describes a remote server: an `http` or `https`
`url::Url` (any other scheme is `InvalidRequest`), an optional bearer token (a `config::Secret`),
and a connect timeout (10 s by default). The deprecated HTTP+SSE transport isn't supported (PLAN
section 7.10).

- **Auth.** The token is sent as `Authorization: Bearer <token>` on every request; `reqwest` marks
  the header sensitive so its own logging hides it, and the token is scrubbed from error messages
  (an HTTP error body can echo it). A 401 or 403 during the handshake, with or without
  `WWW-Authenticate`, is `InvalidRequest` ("rejected the credentials"), not `Unavailable`:
  retrying the same token won't help. OAuth is not supported.
- **HTTP client.** `net::tls_client_builder()`, shared with the built-in providers: rustls trusting
  only the bundled Mozilla roots (`webpki-root-certs`), no native-tls or OpenSSL, and no redirects,
  so the token can't be forwarded to another host. MCP adds the connect timeout and no read
  timeout: the server's event stream can be quiet for a long time.
- Sessions, the `GET` event stream, and session re-creation after an expired session (HTTP 404)
  are `rmcp`'s defaults.

## Mapping MCP to the Toolset Trait

| MCP                                | `Toolset`                                                    |
| ---------------------------------- | ------------------------------------------------------------ |
| `tools/list`, every page           | `list_tools()`: bare `ToolSpec`s, sorted by name             |
| `notifications/tools/list_changed` | `tool_changes()`: the whole new list, re-read on each change |
| `tools/call`                       | `call()`: a `ToolCallHandle`                                 |
| `notifications/progress`           | `ToolCallEvent::Progress`                                    |
| `CallToolResult`                   | `ToolCallEvent::Result { content, is_error }`                |
| `notifications/cancelled`          | Sent when the handle's `events` stream is dropped            |
| `elicitation/create`               | Declined (task 2.21)                                         |

**Tool specs.** `name`, `description` (the `title` if there is no description, else empty), and
`inputSchema` as is. `annotations` are ignored, as PLAN section 7.10 says. `list_tools` follows
`nextCursor` for at most 100 pages and fails beyond that, so a server that loops can't hang the
core.

**List changes.** `tool_changes()` is `Some` only if the server declared `tools.listChanged` at
initialization. Each notification bumps a counter; the stream re-reads the full list and yields it.
Notifications that arrive while a list is being read coalesce into one more read. A failed re-read
is logged and skipped, except a lost connection, which ends the stream; dropping the toolset also
ends it. Caching the list (PLAN section 7.9) is task 2.20.

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
then drops any late response. The server cancels the handler (an `rmcp` server cancels the
request's cancellation token).

**Result content.** Text content becomes `Block::Text`. So does the text of an embedded text
resource; a resource link becomes `[resource link: <uri>]`. Images, audio, and blob resources are
replaced with a text placeholder naming what was dropped, such as `[image (image/png, 5120 bytes)
dropped: Giles doesn't take media from MCP tools yet]`: storing them in the blob store is task 2.21.
If the result has no text content but has `structuredContent`, that JSON is the text.
`isError` (default false) becomes `is_error`.

**Approvals.** `answers` exists and accepts sends while the call runs, but nothing reads it yet:
the client doesn't declare elicitation, and a server that asks anyway is declined. Mapping
`elicitation/create` to `ToolCallEvent::ApprovalRequest` is task 2.21.

**Skills.** `list_skills` is empty and `get_skill` is `NotFound`: toolset skill directories are
task 2.21.

**Media inputs.** `ToolCall::media` isn't handed to the server yet and `{"$media": ...}` values
are sent unchanged; copying blobs into a stdio server's `TMPDIR` is task 2.21.

## Errors

| Failure                                                  | `giles_types::Error`               |
| -------------------------------------------------------- | ---------------------------------- |
| Connection closed, transport failure, server exited      | `Unavailable` (retryable)          |
| HTTP 401 or 403 during `initialize`                      | `InvalidRequest`                   |
| JSON-RPC `-32600`, `-32601`, `-32602` (e.g. unknown tool) | `InvalidRequest`, server's message |
| Any other JSON-RPC error                                 | `Other`, server's message          |
| No common protocol version                               | `InvalidRequest`                   |

Messages start with `MCP server <instance>:` and have the instance's secrets scrubbed. A tool that
fails by its own account (`isError: true`) is a normal `Result` event, not an error.

## Staged Work

- 2.19: launch stdio servers through the sandbox trampoline (a `Command` passed to
  `connect_stdio`).
- 2.20: start toolsets at boot, restart with backoff, the tool discovery cache, idle stop.
- 2.21: media in and out of tools, toolset skill directories, elicitation as approval requests.

## Tests

`crates/giles-core/tests/support/mcp_server.rs` is a small `rmcp` server (paginated tools,
`list_changed`, gated progress, a tool that waits to be cancelled, elicitation, exit). It runs:

- in process over a duplex pipe (`tests/mcp.rs`), with a relay task in between whose abort stands
  in for a crash;
- as a real stdio child (`tests/mcp_stdio.rs`): the test binary re-executes itself with
  `GILES_MCP_TEST_SERVER` set and serves MCP instead of running tests. Libtest would print to
  stdout, so this target has `harness = false` and its `main` runs the tests. It checks stderr
  logging with an echoed `env` secret redacted, cancellation reaching the child, a crash and the
  exit status from `closed()`, the exit status in a failed handshake's message (a child that exits
  with 125), and that dropping the toolset kills a server that ignores stdin closing.
- over streamable HTTP (`tests/mcp_http.rs`): `rmcp`'s `StreamableHttpService` in `axum` on
  `127.0.0.1`, behind a middleware that requires the bearer token. It checks listing, progress, and
  cancellation with the right token; rejection of a wrong, missing, or forbidden one (the 403 body
  echoes the token, which must come back scrubbed); and an unreachable server.
