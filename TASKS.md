# Giles - Tasks

The v1 implementation plan, derived from `Docs/IDEA.md` and `Docs/PLAN.md`. `§` refers to a
`PLAN.md` section.

How to read it:

- Every task needs Phase 0. Beyond that, a task needs only what its **Needs** line lists, and may
  start as soon as those are checked off. Tasks with no unmet needs can be built in parallel.
- Phases group tasks that become available at about the same time. A task may start before the
  rest of its phase is done, and some later-phase tasks (e.g. the channel plugins) open up early.
- Within Phase 2, tracks are independent of each other; tasks inside a track mostly build on the
  one before.
- Each task lands as one or more reviewable commits that build, pass tests, and update the user and
  technical docs (`AGENTS.md`).

Deferred work (multiple users, group chats, model fallback, config reload, provider plugins,
webhooks, passkeys) is not listed; see `IDEA.md` "Out of Scope for v1" and §1.1.

## Build Decisions

Answered by the owner before the build started:

- License: `MIT OR Apache-2.0` for every crate.
- Repository: public at `github.com/jennings/giles`; `main` is pushed after each landed task and CI
  runs there.
- Linux verification on the macOS development machine: an Apple `container` Linux container (arm64
  Debian bookworm, kernel 6.12). `buf` is installed locally.
- 1.13 WhatsApp spike: an agent builds the spike and its test plan; the owner does the live device
  linking. 1.13 stays open, and 3.6 waits for it, until then.
- The draft decision records (`Docs/DECISIONS.md`, `Docs/decisions/`) stay off `main` for now.

## Phase 0: Project Setup

Sequential, except 0.2 and 0.3, which can run in parallel after 0.1.

- [x] **0.1 Cargo workspace** (§13.1, §14.1). Skeleton crates `giles` (bin), `giles-core`,
  `giles-types`, `giles-proto`, `giles-plugin-sdk`, `giles-store`. Edition 2024,
  `rust-toolchain.toml` pinned to a stable release, `Cargo.lock` committed, shared workspace
  dependencies and lints, `.gitignore`.
- [x] **0.2 CI.** `cargo fmt --check`, `cargo clippy` (warnings denied), `cargo test --workspace`,
  `rumdl` Markdown lint, `mdbook build`. (`buf` checks are added in 1.4.)
- [x] **0.3 mdBook skeleton.** `Docs/book.toml` and `SUMMARY.md` with a User Guide and a Technical
  section; `IDEA.md` and `PLAN.md` linked as design background.
- [x] **0.4 `giles` binary skeleton** (§11.1). `clap` CLI with `serve`, `tracing` subscriber,
  `anyhow`, `GILES_HOME` resolution (default `~/.giles`), creation of the data root layout, the
  `$TMPDIR` fallback for over-long socket paths, clean shutdown on SIGINT/SIGTERM.
- [x] **0.5 `giles-types`** (§4.3, §4.4, §5). The shared contract everything else builds on:
  - Message model: `Message`, `Role`, `Block`, `Opaque`, `MessageMeta`, `MediaRef`, `Usage`,
    `StopReason`, with serde (blocks are stored as JSON).
  - Provider types: `CompletionRequest`, `CompletionEvent`, `ToolSpec`, `GenerationParams`,
    `ModelInfo`.
  - Channel types: `ChannelInfo` (output mode, `can_initiate`), `ChannelEvent`, `InboundMessage`
    (with `sender_authenticated` and `sender_aliases`), `OutboundMessage`, `ConversationRef` (with
    kind), `EventId`, `MessageId`.
  - Toolset types: `ToolCall`, `CallContext`, `ToolCallHandle`, `ToolCallEvent`,
    `ApprovalAnswer`, `SkillSummary`, `Skill`.
  - Traits `Provider`, `Channel`, `Toolset` via `async_trait`; public enums `#[non_exhaustive]`;
    `thiserror` errors.
- [x] **0.6 Publish the book to GitHub Pages** (added by the owner). Move `mdbook build` (and the
  Markdown lint) out of `ci.yml` into its own GitHub Actions workflow that builds `Docs/` and, on
  pushes to `main`, deploys it to GitHub Pages; pull requests only build it. **Needs:** 0.2, 0.3.

## Phase 1: Foundations

All parallel. Each task needs only Phase 0.

- [x] **1.1 Config loading** (§1.1, §7.7, §11.2, §12.4).
  - `giles.toml`: `[core]`, `[owner]` (identities, default), `[provider.*]`, `[channel.*]`,
    `[toolset.*]` with the core-owned instance keys, `[[routes]]`.
  - `agents/<agent>/agent.toml`: models, `compaction_model`, `tools`, `ask`, iteration caps,
    `busy_mode`, `memory_cap`, `approval_timeout`, `timezone`, `idle_reset`, `[[jobs]]`.
  - Instance names `[a-zA-Z0-9-]+`; `component` resolution (built-in, else
    `giles-<kind>-<component>`); `<provider>/<model>` parsing; durations.
  - Secrets: `{ secret = "..." }` from `secrets.toml` and `{ env = "..." }`; refuse to start if
    `secrets.toml` is readable by group or others; values at secret paths redacted in errors and
    logs.
  - Sandbox grants parsed; `read`/`write` grants overlapping `GILES_HOME` (other than the
    instance's data dir) or plugin dirs rejected.
  - Instance config hash taken before secret resolution (for the cache, §7.9).
- [x] **1.2 SQLite store** (§11.4). `giles-store`: `rusqlite` (bundled) on a blocking pool, WAL,
  migrations embedded in the binary. v1 tables `sessions`, `messages`, `summaries`, `inbox`,
  `outbox`, `jobs`, `job_runs`, `usage`, `web_sessions`, and the FTS5 `search` index kept by
  triggers on `messages`. Typed query APIs, tested against temporary databases.
- [x] **1.3 Blob store** (§11.5). Content-addressed files under `data/blobs/sha256/`: streaming put
  with a size limit, get, media type and name metadata, `MediaRef` ids. (The sweep is 4.2.)
- [x] **1.4 Protobuf contracts** (§8). `.proto` sources under `proto/giles/<package>/v1/`:
  `types.v1` (mirroring §5, `BlobRef`), `plugin.v1` (base service, blobs), `channel.v1`,
  `admin.v1`, and the `provider.v1` messages `DescribeResponse` references (`ProviderService` is
  deferred). `giles-proto` `build.rs` with `tonic-build`, using `PROTOC` or a vendored `protoc`.
  `buf.yaml`; `buf lint` and `buf breaking` added to CI.
- [x] **1.5 Spike: gRPC over a socket pair** (§7.4). Confirm the `[INFERENCE]`: `tonic` serving on
  one pre-connected stream (`serve_with_incoming` with a one-item stream) and a client over a
  one-shot connector, with 16 MiB message limits and bearer-token metadata. Record the result in
  the technical docs; revise §7.4 if it fails.
- [x] **1.6 Sandbox trampoline** (§12.1, §12.4). `giles sandbox-exec <spec> -- <cmd>`: spec format
  (grants, data dir, network, Unix-socket grant, `min_abi`), scrubbed environment with the §12.4
  defaults, fds passed through, then `exec`. Platform backends behind an internal trait
  (implemented in 2.13 and 2.14). Startup host check that reports one clear error.
- [x] **1.7 CommonMark renderers** (§9, §10). In `giles-plugin-sdk` on `pulldown-cmark`: sanitized
  HTML with remote images dropped (web, email), plain text (email), IRC formatting codes with
  line-limit splitting, WhatsApp markup. Table-driven tests.
- [x] **1.8 Anthropic provider** (§4.2, §5, §6.7). Feature `provider-anthropic`: streaming
  Messages API to `CompletionEvent`s, tool use, thinking blocks and signatures kept as
  `Reasoning`/`Opaque` and sent back unchanged, server-side blocks as `Opaque`, automatic caching
  plus a breakpoint at the end of `system` (1 h TTL, 5 min for jobs), usage with cache tokens,
  declared media types, consecutive `User` messages merged, config validation, `list_models`.
  Tests against recorded responses.
- [x] **1.9 OpenAI-compatible provider** (§4.2, §5). Feature `provider-openai`: streaming Chat
  Completions with `base_url` (OpenAI, OpenRouter, Ollama), tool calls, tool-result images moved
  into a following `user` message, `Opaque` of other API families not sent, usage including
  cached tokens where reported, declared media types, config validation, `list_models`. Tests
  against recorded responses.
- [x] **1.10 Skills** (§6.6). Parse Agent Skills `SKILL.md` (front-matter `name` and
  `description`, body); discover from `<root>/skills/` and `agents/<agent>/skills/`, agent skills
  overriding global ones; sorted listing. (Toolset skill dirs come in 2.21.)
- [x] **1.11 Memory files** (§6.7, §11.3). Optional front-matter preserved on rewrite;
  `§`-separated entries; batches of `add`, `patch{old_text, content}`, `remove{old_text}` with
  unique-match rules; cap checked on the batch result, with the current entries and overflow
  returned on failure; usage header (`[67% — 1,474/2,200]`); per-file lock and
  write-to-temp-then-rename.
- [x] **1.12 MCP client** (§7.10, §8.6). A `Toolset` over `rmcp`: stdio (spawned directly for now)
  and streamable HTTP with bearer auth; `tools/list` to bare `ToolSpec`s; `list_changed` to
  `tool_changes()`; `tools/call` to a `ToolCallHandle` with progress events;
  `notifications/cancelled` when a call is dropped; text result content to blocks. Tested against
  a small `rmcp` test server.
- [ ] **1.13 Spike: WhatsApp library** (§10). Link `whatsapp-rust` as a device (QR and pair code),
  send and receive a DM, survive a restart with the persisted session. Decide between
  `whatsapp-rust` and the `whatsmeow` fallback before 3.6; record the result.
  - Spike built (`spikes/whatsapp/`, on `whatsapp-rust` 0.7.0); findings, recommendation, and the
    owner's live test plan and results table are in `Docs/technical/whatsapp-spike.md`. Open until
    the owner has run it.
- [x] **1.14 HTTP server skeleton** (§9). `axum` on `http_listen`, `askama` layout, vendored `htmx`
  and its SSE extension embedded in the binary, `Content-Security-Policy: img-src 'self' data:`
  and related headers.

## Phase 2: Core Runtime and Plugin Plumbing

The five tracks run in parallel.

### Track A: Message Path

- [x] **2.1 Registry and startup wiring** (§6.5, §13.3). Built-in components constructed from
  config; instance name to trait object; prefixed tool index (`<instance>__<tool>`, 64-character
  limit, reserved core tool names, sorted) and skill index. Providers plug in as 1.8 and 1.9 land.
  **Needs:** 1.1.
- [x] **2.2 Identity, routing, core commands** (§1.1, §3.2, §3.3, §6.1). Drop (and log)
  unauthenticated senders, identities not in `[owner]`, and group conversations; match the sender
  or any alias; enforce the identity-namespace prefix; ordered routes, first match wins; intercept
  `/new`, `/stop`, and approval replies after routing, before the agent. **Needs:** 1.1.
- [x] **2.3 Inbox and sessions** (§6.2, §6.3, §8.5). Store each event in `inbox` with
  (instance, event id) dedupe, then ack; prune dedupe keys after 30 days; session key (agent,
  instance, conversation); at most one turn per session with an inbox-backed queue; consecutive
  messages from one person form one turn; `/new`; `idle_reset`; startup recovery (empty result
  slots get `interrupted`, running rows become `unknown`, the requester is told). **Needs:** 1.2,
  2.2.
- [x] **2.4 Agent loop** (§6.3, §6.7, §11.7, §14.1). Prompt assembly at session start: sorted tools;
  `system` = `SOUL.md`, skill list, memory snapshot (agent `MEMORY.md`, the owner's `USER.md` and
  per-user `MEMORY.md`); per-message time prefix in the owner's timezone. Stream the provider;
  store each assistant message with its N result slots in one transaction; loop until no tool use;
  `Stop(MaxTokens)` gives `truncated`; one `usage` row per request. Snapshot test of the rendered
  `system` and tool list. **Needs:** 2.1, 2.3, 1.10, 1.11, and 1.8 or 1.9.
- [x] **2.5 Tool dispatch and policy** (§6.3, §6.4). Tool list fixed per session from the agent's
  allowlist globs and enforced at call time; "tool unavailable" for tools no longer served; calls
  in model order, consecutive parallel-safe calls (read-only core tools, `parallel = true`
  toolsets) up to 8 at once, others as barriers; per-toolset `deadline`; synthetic results;
  iteration caps (`max_iterations`, `max_iterations_jobs` default 25) with a final no-tools
  request. **Needs:** 2.4.
- [x] **2.6 Turn control** (§6.3). `/stop` cancels the turn and the cancellation reaches toolsets
  and plugins; `busy_mode = "steer"` (requester's new messages appended at tool-batch boundaries,
  unstarted calls get "Skipped to read a new message") and `"queue"`. **Needs:** 2.5.
- [x] **2.7 Approvals** (§6.4). `ask` globs; prompt with a short code and injection-safe rendering
  of the input (fence longer than its longest backtick run, control characters escaped, long input
  cut with size and SHA-256 prefix); `yes`/`no`/`yes!` replies consumed by the core; in-memory
  pending approvals expiring after `approval_timeout`; a non-approval message from the requester
  denies with "Not approved. The user said: ..."; MCP elicitation through the same path.
  **Needs:** 2.5.
- [x] **2.8 Output delivery** (§6.9). `deltas` channels get streamed text; `blocks` channels get
  chunks flushed at paragraph boundaries outside code fences and at message end; reasoning and tool
  blocks never reach channels. **Needs:** 2.4. Parallel with 2.5–2.7.
- [x] **2.9 Media in turns** (§5). Inbound attachments become `Image`/`Audio`/`Document` blocks with
  `[media:<id> <name> <type>]` references; media types a provider doesn't accept reach the model
  only as the reference. **Needs:** 2.4, 1.3.
- [x] **2.10 Compaction** (§6.8). Near the model's context limit, summarize older history with
  `compaction_model` into `summaries`; the prompt uses the summary plus recent messages; `system`
  is rebuilt with a fresh memory snapshot. **Needs:** 2.4.

### Track B: Core Tools

- [x] **2.11 `memory` and `memory_search`** (§6.7). `memory(scope, operations[])` on 1.11 for the
  `agent` and owner `user` scopes; agent-wide writes only in owner turns and config jobs;
  `memory_search` over the owner's DM transcripts with this agent (FTS5). **Needs:** 2.5, 1.11.
- [x] **2.12 `load_skill` and `send_attachment`** (§5, §6.6). **Needs:** 2.5, 2.9.

### Track C: Plugin Plumbing

- [x] **2.13 Sandbox: Linux** (§12.2). Landlock with the newest ABI the kernel offers, effective ABI
  logged, `min_abi` enforced; seccomp allowlist based on Docker's default profile (`ENOSYS` for
  unlisted syscalls, `EPERM` for the denied list and ungranted socket families, native arch only,
  `socketpair(AF_UNIX)` allowed). **Needs:** 1.6.
- [x] **2.14 Sandbox: macOS** (§12.3). Generated deny-by-default Seatbelt profile with no Mach
  services, real paths, writable roots that can't be swapped for links, IP-only network grants
  (plus mDNSResponder's socket for DNS), per-path `unix` grants, applied through `sandbox-exec`.
  **Needs:** 1.6. Parallel with 2.13.
- [x] **2.15 Plugin SDK** (§7.4, §8.1–§8.3, §8.5). `giles-plugin-sdk`: magic-cookie check, token
  from stdin, protocol choice from `GILES_PLUGIN_PROTOCOLS`, gRPC on fd 3, handshake line
  `1|<protocol>|fd|3|grpc`, bearer check, exit on stdin EOF; `Plugin` base service and
  `grpc.health.v1`; plugin-side blob handles; serve a `Channel` as `ChannelService`, with unacked
  events persisted in the data dir and resent when `Events` reopens; 16 MiB limits; `rustls` with
  bundled roots; publishable to crates.io. Includes a test-only echo channel plugin for
  integration tests. **Needs:** 1.4, 1.5.
- [x] **2.16 Plugin host: launch** (§7.3, §7.4, §11.6, §13.4). Discovery in `<root>/plugins/`, then
  `$PATH`; per-instance data dir and `TMPDIR`; socket pair, token, and scrubbed env; launch through
  the trampoline (`unsandboxed = true` bypasses it with a warning); handshake with a 10 s timeout;
  `Describe` with kind and name checks; `Configure` with resolved secrets; identity-namespace
  prefix check; stderr logged under the instance span, stdout drained; trace context in gRPC
  metadata; pid files and orphan cleanup at startup. **Needs:** 1.1, 1.4, 1.5, 1.6. Can be built
  alongside 2.15; end-to-end tests use its echo plugin.
- [x] **2.17 Plugin host: supervision** (§7.5, §7.9). Health check every 30 s; restart with backoff
  from 1 s to 5 min, reset after 10 min healthy; no retry after a protocol mismatch until the
  binary's hash changes; stop with `Shutdown`, then SIGTERM, then SIGKILL; `Describe` cache.
  **Needs:** 2.16.
- [x] **2.18 `RemoteChannel` adapter** (§4.3, §8.3, §8.5). `Channel` over `ChannelService`:
  bidirectional `Events` with acks, unknown event variants acked and ignored, `ChannelStatus`
  recorded for `giles status`, `Send` by `conversation_id` or `recipient`; blob transfer
  (`GetBlob`, store, `ReleaseBlob`; `PutBlob` for outbound) with `max_blob` and header-size limits;
  prost-to-domain conversion by moving fields. **Needs:** 2.16, 1.3.

### Track D: Toolsets (MCP)

- [x] **2.19 Sandboxed stdio toolsets** (§7.10). Resolve `command` with the operator's `PATH` and
  add its install prefix to the read and execute grants; launch through the trampoline with
  network off unless granted. **Needs:** 1.12, 1.6.
- [x] **2.20 Toolset lifecycle and cache** (§7.5, §7.6, §7.9). Start every toolset at boot;
  tool-list cache in `<root>/cache/` keyed on binary SHA-256, server version, and config hash;
  refresh on start and on `list_changed` (new sessions only); `idle_stop` with cached tools still
  offered and relaunch on the next call; an unreachable remote server leaves Giles degraded and
  retrying with backoff. **Needs:** 1.12, 2.1.
- [x] **2.21 MCP media, skills, elicitation** (§6.6, §7.10). `{"$media": "<id>"}` inputs copied
  into a stdio server's `TMPDIR` and replaced by the path; image and audio results stored as blobs;
  toolset `skills = [...]` dirs, named `<instance>__<skill>`; `elicitation/create` surfaced as a
  `ToolCallEvent` approval request. **Needs:** 1.12, 1.3, 1.10.

### Track E: Web and Admin

- [x] **2.22 Web channel** (§9, §10). Built-in `channel-web` on the 1.14 server: chat pages, posted
  messages as `ChannelEvent`s, live output over SSE (`deltas`), HTML from 1.7, approval buttons.
  **Needs:** 1.14, 1.7.
- [x] **2.23 Web login** (§3.4). Session cookie (`HttpOnly`, `SameSite=Lax`, `Secure` over HTTPS)
  backed by `web_sessions`, expiring after 30 days; redemption of one-time login links; the
  trusted-proxy identity header, honored only on `run/proxy.sock` (mode 0600) or a TCP listener
  that also requires a shared-secret header. **Needs:** 2.22, 1.2.
- [x] **2.24 Admin service and CLI** (§8.7, §11.7). `giles.admin.v1` on `run/admin.sock` (mode
  0600), every call checked against `run/admin.token`; v1 RPCs `Status` (config error, sandbox ABI,
  `requested_grants`, channel status), `ListPlugins`, `RestartPlugin`, `ListSessions`,
  `ResetSession`, `CreateLoginLink`, `UsageReport`; CLI `giles status`,
  `giles admin login-link --user`, `giles usage` (cache hit rate per agent). RPCs fill in as their
  subsystems land. **Needs:** 1.4, 1.2. Plugin lifecycle fields and `RestartPlugin` come from 2.17
  (the plugin host implements `admin::PluginControl`; toolsets join with 2.20); `CreateLoginLink`
  uses the web channel (`admin::LoginLinks`).

## Phase 3: v1 Features

The channel plugins (3.4–3.6) open up as soon as 2.15 and 1.7 are done, and are parallel with each
other and with the rest of this phase.

- [x] **3.1 Outbox** (§6.11, §8.5). Proactive sends addressed to an identity via `recipient`,
  retried with backoff until accepted; rows expire (approval prompts with their approval, everything
  else after 24 h) and are logged with instance, recipient, and kind. **Needs:** 2.1, 1.2.
- [x] **3.2 Scheduler** (§6.4, §6.11). `croner`; config jobs from `agent.toml`;
  `session = "isolated"` (fresh context, memory snapshot, `dm_excerpt`) and `"dm"` (its own queued
  turn in the owner's default DM); `idle_only`, `active_hours`, job `model`, `prompt` or
  `prompt_file`; timezone precedence; `NO_REPLY` sends and stores nothing, other replies go through
  the outbox and are appended to the DM session; `ask` tools denied unattended unless
  `ask_to = "deliver_to"`; `job_runs`, with missed runs coalesced at startup. **Needs:** 2.7, 3.1.
- [x] **3.3 `schedule` core tool** (§6.11). Create, list, and cancel runtime jobs in `jobs` for the
  requester; config jobs listed read-only. **Needs:** 3.2.
- [x] **3.4 IRC channel plugin** (§3.3, §8.5, §10). `giles-channel-irc`: pick the library; TLS;
  IRCv3 `account-tag` identities (`account:<name>`) and opt-in `mask:` identities; queries as
  direct conversations, channels reported as group; IRC rendering and splitting; `recipient`
  resolves an account to its current nick, or `UNAVAILABLE`; namespace `irc:<network>`.
  **Needs:** 2.15, 1.7. End-to-end with the core needs 2.18.
- [x] **3.5 Email channel plugin** (§3.3, §8.5, §10). `giles-channel-email`: pick the libraries;
  IMAP IDLE with a `UIDVALIDITY:UID` cursor; sender verified from the topmost
  `Authentication-Results` carrying the configured `authserv-id` (`dmarc=pass` or aligned
  `dkim=pass`); automated mail dropped; thread root as the conversation; SMTP
  `multipart/alternative` with `Auto-Submitted: auto-replied` and threading headers; attachments as
  blobs.
  **Needs:** 2.15, 1.7. End-to-end with the core needs 2.18.
- [ ] **3.6 WhatsApp channel plugin** (§3.3, §10). `giles-channel-whatsapp` on the library chosen in
  1.13: linking reported as `ChannelStatus` `NeedsAuth` (QR or pair code); session store in the data
  dir; phone number identity with the LID as an alias; echoes of its own messages dropped; group
  chats reported as group; WhatsApp markup; media as blobs. **Needs:** 2.15, 1.7, 1.13.
  End-to-end with the core needs 2.18.
- [ ] **3.7 Remote plugins** (§7.6). User-run gRPC plugins over TCP: optional TLS, plain TCP to a
  non-loopback address refused unless `insecure = true`; bearer token or OAuth client credentials;
  degraded with backoff while unreachable; the core only calls `Shutdown` and disconnects.
  **Needs:** 2.18.
- [ ] **3.8 Web admin pages** (§8.5, §8.7). Server-rendered status and plugin pages over the admin
  service, including a QR code for a channel in `NeedsAuth`. **Needs:** 2.23, 2.24.

## Phase 4: Operations and Release

4.1–4.3 are parallel; 4.4 and 4.5 come last.

- [ ] **4.1 Backups** (§11.4). `Backup` RPC and `giles backup <dir>`: `VACUUM INTO` plus a tar of
  the TOML, Markdown, and blobs; the same snapshot written to `backups/` before migrations run.
  **Needs:** 2.24.
- [ ] **4.2 Blob sweep** (§11.5). Periodic removal of blobs that no message, inbox or outbox row,
  or job references, and that are older than 48 h. **Needs:** 1.2, 1.3.
- [ ] **4.3 Instance rename** (§7.2). `giles instance rename <old> <new>` with the core stopped
  (database rows, directories, the `giles.toml` table); at startup, refuse a new instance name
  while an unused old name has data, pointing to the command. **Needs:** 1.1, 1.2.
- [ ] **4.4 Container image** (§14.2). Multi-stage `Dockerfile` and `.dockerignore`: Debian
  bookworm, binaries in `/usr/local/bin/`, non-root `giles` user, `GILES_HOME=/data` volume, port
  8080, `giles serve` entrypoint, BuildKit cache mounts. Verify the sandbox startup check inside the
  container. **Needs:** 3.4, 3.5, 3.6.
- [ ] **4.5 End-to-end tests.** Scripted tests of the whole daemon with a fake provider and the echo
  channel plugin: message to reply, tool call with approval, `/stop`, restart recovery, job
  delivery through the outbox. **Needs:** Phases 2 and 3.
