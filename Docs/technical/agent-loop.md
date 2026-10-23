# Agent Loop

`giles_core::agent_loop` runs turns ([Plan](../PLAN.md) §6.3, §6.7): it assembles each session's
prompt prefix and, per turn, calls the provider, runs the requested tools, and repeats until the
model stops asking for tools. The session code that hands it turns is in
[Sessions and Turns](sessions.md); the files it reads are in
[Skills and Memory Files](skills-and-memory.md).

| Item                          | What it is                                                         |
| ----------------------------- | ------------------------------------------------------------------ |
| `AgentLoop`                   | The `TurnRunner`: `new(registry, store, clock, dispatcher)`, `from_runtime` |
| `AgentLoop::session_prompt`   | The session's cached prefix, assembled at its first turn           |
| `AgentLoop::rebuild_prompt`   | A fresh `system` for the session, same tools (compaction)          |
| `ToolDispatcher`              | Runs one message's calls: `dispatch(ToolBatch) -> stream of (index, ToolOutput)` |
| `PolicyDispatcher`            | The daemon's dispatcher: call-time checks, parallel runs, deadlines |
| `CallPolicy`                  | A call's `parallel` and `deadline`, from its toolset or the core tool list |
| `OutputSink`                  | Receives a turn's streamed events and finished messages            |
| `MessageOutput`               | Sends each finished message's text through `Turn::reply`           |
| `history::user_message`       | An inbound message's stored form, with its time prefix             |
| `history::request_messages`   | A request's messages from `Store::load_history`                    |
| `prompt::SessionPrompt`       | A session's `system` blocks and its `Arc<SessionTools>`            |
| `prompt::SessionTools`        | The registry's tool snapshot filtered by the `tools` allowlist     |
| `prompt::assemble`            | Tools plus a fresh `system`, at session start                      |
| `prompt::build_system`        | A fresh `system` only (compaction, TASKS.md 2.10)                  |
| `prompt::render_system`       | The pure renderer: `SystemParts` to text                           |
| `glob::matches(_any)`         | Tool-name globs (`*`, `?`) for `tools` and `ask`                   |

## The Prompt Prefix

Anthropic caches the prefix in the order tools, `system`, messages, and a change at one level
invalidates everything after it (§6.7). So both tools and `system` are built once per session and
reused byte for byte by every request of every turn.

### Tools

`SessionTools::from_snapshot(registry.tools().snapshot(), agent.tools)` keeps the snapshot's tools
whose offered name (`<instance>__<tool>` or a bare core tool) matches one of the agent's `tools`
globs. In v1 there are no roles, so the allowlist alone decides (§6.4); call-time enforcement is
the tool dispatcher's job. The snapshot is sorted by name, and filtering keeps the order. A tool a
toolset later drops stays in the list; calling it gets `tool unavailable`.

Globs: `*` matches any run of characters (none included), `?` exactly one, anything else itself,
case-sensitively, over the whole name. `memory*` matches `memory` and `memory_search`. An empty
allowlist offers no tools.

### `system`

One text block (none at all when every part is empty), with these parts separated by blank lines:

1. `SOUL.md` from the agent's directory: a leading byte-order mark and blank lines and trailing
   whitespace removed, `\r\n` normalized. Missing or blank: omitted. Unreadable or not UTF-8: the
   turn fails, so a broken file is noticed rather than silently dropped.
2. `# Skills`, when the agent has any and the session offers `load_skill` (without it the model
   can't read a skill, and listing them would only invite failing calls): one line of instructions
   (load a fitting skill with `load_skill`), then `` - `<name>`: <description> `` per skill, sorted
   by name, with each description's whitespace runs collapsed to single spaces. Discovery warnings
   are logged and the broken skill is left out.
3. `# Memory`: an introduction (entries are separated by `§` lines; this is a snapshot from the
   start of the conversation), then one `## <title>` section per file, each with a one-line
   description and the file's entries joined by `§` lines (front-matter left out), or `(empty)`:
   - `Agent memory [<usage>]`: `agents/<agent>/MEMORY.md`.
   - In a direct session, and in a job session that belongs to a user, `User profile`
     (`users/<user>/USER.md`, no usage header: people write it) and `User memory [<usage>]`
     (`agents/<agent>/users/<user>/MEMORY.md`). Group sessions would load the group's memory
     instead and never a participant's (§6.7); v1 has no groups.

The usage header is `memory::usage_header` with the agent's `memory_cap`, e.g.
`[67% — 1,474/2,200]`. Files the model can't write (USER.md) have none. The current time never
appears in `system`.

The snapshot tests in `crates/giles-core/tests/prompt_snapshot.rs` render `system` and the tool
list (names, descriptions, schemas) for three agents, from real files in a temporary data root, and
compare them with `crates/giles-core/tests/snapshots/prompt-*.txt`: a full agent in a direct
session, an agent with no files or tools, and a job session with every tool allowed and an over-cap
memory file. A change to the cached prefix therefore changes a file in review. Rerun with
`GILES_UPDATE_SNAPSHOTS=1` to accept one.

### Caching the Prefix

`AgentLoop` keeps each session's `SessionPrompt` in memory, keyed by session key (agent, channel
instance, conversation) with the session id, so only the newest session of a conversation is kept
and the map is bounded by the number of conversations. A session without a key (an isolated job
run, one turn long) isn't cached. A turn takes the prefix once at its start and uses it for every
request it makes.

The prefix isn't stored. After a restart the cache is empty, and a session's first turn assembles
it again from the files and tools of that moment. If nothing changed, the bytes are the same and
the provider's cache (one hour for interactive sessions) still hits. If memory files, skills, or
`SOUL.md` changed while the session was running, or the toolsets aren't all registered yet, that
first request after the restart misses and writes a new cache entry; the session then keeps the new
prefix. Storing the prefix would avoid that one miss but freeze tools the registry may no longer
serve, so v1 recomputes.

`rebuild_prompt(session, agent)` re-reads the files into a new `system`, keeps the cached tools
(tools are fixed for the session's life), and replaces the cache entry. Compaction (TASKS.md 2.10)
calls it after writing a summary; the turn then has to use the returned prompt.

## Messages

Each inbound message is stored as a `User` message whose single text block starts with a time
prefix, `[Thu 2026-10-22 14:03 PDT]` (`%a %Y-%m-%d %H:%M %Z`), then a space and the text (just the
prefix when the text is empty). The time is the channel's `sent_at`, else when the core received
it, shown in `[owner] timezone`, else the agent's `timezone`, else the system's. The prefix is
part of the stored text, so every later request renders the message identically and `memory_search`
snippets carry the date. `meta.at` is when the core received it (it drives `idle_reset`), and
`meta.sender` the requester.

A request's messages come from `Store::load_history`, reloaded before each request: the newest
summary, if any, as a leading `User` message (`Summary of the earlier conversation:` and the text),
then every later message. Filled slots are `User` messages with one `ToolResult`. An empty slot
can't normally reach a request (a turn fills its slots before the next request, and startup
recovery fills leftovers); if one does, it's sent as an `interrupted` error result, logged, so the
request stays valid.

## Turns

`run_turn`:

1. Fills any slot an earlier turn left empty (a crash, or a failed fill) with `interrupted`, so
   this turn's synthetic results (`store.fill_empty_slots` fills the whole session) can't relabel
   it; looks up the session's agent in the configuration; stores the turn's messages in one
   `append_messages`; gets the session's prefix; resolves the model (`Registry::agent_model`, the
   first in `models`).
2. Builds a `CompletionRequest`: the model id, the prefix's `system` and tools, the history, and
   `cache_retention` `Long` (`Short` for job sessions, which aren't human-paced). Other generation
   settings are left to the provider.
3. Streams the response. Every event goes to the `OutputSink`; `BlockStop` blocks are collected in
   order, the last `Usage` wins, and `Stop` ends it. One `usage` row is recorded per request once
   its stream has started (provider instance, model, agent, requester, session, tokens, `fallback`
   false), however the stream ends; a request refused before streaming generated nothing and
   records none.
4. Stores the assistant message (`meta` provider, model, usage) with one slot per `ToolUse` in one
   transaction, then hands it to the sink. An empty response stores nothing and ends the turn.
5. No `ToolUse`: the turn is `Completed`. `Stop(MaxTokens)`: every slot gets `truncated`, nothing
   runs, and the turn is `Completed`. Otherwise the calls go to the `ToolDispatcher`, each slot
   filled as its output arrives (a call the dispatcher never answers gets `skipped`).
6. The tool-batch boundary hook runs (below), then back to 2.

Ends other than completion:

- **Cancelled** (`Turn::cancel`: `/stop` or shutdown), checked while waiting for the provider, the
  stream, a delivery, or a tool: a response being streamed is dropped unstored; the dispatcher's
  stream is dropped, which drops each running call's events stream and so cancels it at the
  toolset (MCP `notifications/cancelled`); every empty slot gets `cancelled`. Returns `Cancelled`.
- **Failed**: an unknown agent, no usable model, an unreadable prompt file, a database error, or a
  provider error (refused, an `Err` in the stream, or a stream ending without `Stop`). Empty slots
  get `interrupted`, and the requester gets a notice within 10 s: `Sorry, the model request failed:
  <error>` (cut to 300 characters, the provider instance's secrets scrubbed) or `Sorry, something
  went wrong: <error>`. Returns `Failed` with the same text for the log.
- **Panicked**: `AgentLoop::run_turn` catches a panic of the turn's future and handles it like a
  failure: empty slots get `interrupted`, the notice is `Sorry, something went wrong: the turn
  stopped unexpectedly.`, and it returns `Failed("the turn panicked: <message>")`.

## Tool Dispatch

`ToolDispatcher::dispatch(ToolBatch { calls, tools, context })` returns a stream of `(index,
ToolOutput { content, is_error })`, one per call, in any order. Dropping it must cancel the calls
still running. `context` is the `CallContext`: agent, requester, and the session's channel
conversation.

`PolicyDispatcher` is the daemon's (§6.3, §6.4). It doesn't spawn: the stream drives the calls, so
a call starts only while the agent loop polls it, and dropping the stream drops every running
call's future, its events stream with it, which cancels the call at the toolset (MCP
`notifications/cancelled`). Calls not yet started never start.

### Order and Parallelism

Calls start in model order. Each has a `CallPolicy`:

- A toolset tool takes its instance's `parallel` (default `false`) and `deadline` (default 5 m).
- A core tool is parallel-safe if it only reads (`PARALLEL_SAFE_CORE_TOOLS`: `load_skill`,
  `memory_search`), with the default deadline. MCP's `readOnlyHint` is ignored: it's untrusted
  (§7.10).
- A tool the session doesn't offer runs nothing, so it counts as parallel-safe and never holds up
  another call.

Consecutive parallel-safe calls run together, at most `MAX_PARALLEL_CALLS` (8) at once: when one
finishes the next waiting one starts. Any other call is a barrier: it starts once everything
before it has finished and runs alone, and the calls after it wait for it. So `[p1, p2, s, p3]`
runs `p1` and `p2` together, then `s`, then `p3`.

### Checks at Call Time

When a call starts (not when the batch arrives, so a tool dropped meanwhile is caught), it gets the
error result `tool unavailable` when the session doesn't offer the tool (outside the allowlist,
or no such tool), the registry no longer serves it under that name and target (a toolset dropped
it), it's a core tool (no core tool is implemented yet: 2.11, 2.12), or its toolset isn't
registered. The session's tool list is the agent's allowlist (§6.4); v1 has no roles, so there is
no second check by the requester's role. With roles, a forbidden call would get `not permitted for
<user>` here, before it runs.

Otherwise it calls the toolset with the bare name, the input, and no media, and the first
`ToolCallEvent::Result` is the output; a `call` error, an `Err` event, or a stream that ends
without a result becomes an error result with the error's text. Progress is logged. The handle's
`answers` sender is dropped at once, which denies any approval request (2.7 answers them).

### Deadlines

The deadline covers the whole call, from `Toolset::call` to its result. A call still running at
its deadline is dropped (cancelled at the toolset, as above) and gets the error result `timed out
after <deadline>`, e.g. `timed out after 5m`; the batch goes on.

## Output

`MessageOutput` sends each stored assistant message's `Text` blocks, trimmed and joined by blank
lines, in one `Reply::send` once the message is complete; reasoning, tool, and opaque blocks never
reach the channel, and a message without text sends nothing. A failed send is logged; the message
is stored either way. Isolated job runs have no `Reply` and send nothing.

## Seams for Later Tasks

- **2.6, steering**: `AgentLoop::at_batch_boundary(turn)` runs after every slot of a batch is
  filled and before the next request; the messages it returns are stored after the tool results.
  It returns none, so the requester's new messages stay queued and start the next turn. 2.6 takes
  them there with `turn.steering.take()` in `steer` mode. To skip unstarted calls it hands the
  dispatcher a signal in `ToolBatch` (e.g. from `Steering::changed()`): `Scheduler::start` in
  `dispatch.rs` is the one place a call starts, so once the signal fires it stops starting calls
  and answers each waiting one with `Skipped to read a new message` (still one output per call),
  while running calls finish.
- **2.7, approvals**: the gate goes in the per-call future built in `PolicyDispatcher::dispatch`,
  after the call-time checks and before `tokio::time::timeout`, so waiting for an answer doesn't
  use up the call's deadline; a denial is that call's output. A barrier call waiting for approval
  holds up the calls after it, as it would while running. Mid-call elicitation arrives as
  `ToolCallEvent::ApprovalRequest` in `run_call`, answered on the handle's `answers` sender.
- **2.8, output**: `AgentLoop::output_for(turn)` picks the sink; `OutputSink::event` sees every
  streamed event and `message_aborted` is called when a streamed response is dropped.
- **2.9, media**: inbound attachments into `history::user_message`, `ModelInfo::accepts` filtering,
  `ToolCall::media`.
- **2.10, compaction**: `AgentLoop::rebuild_prompt`, and the summary rendering in
  `history::request_messages`.
- **2.11, 2.12, core tools**: a core-tool arm in the dispatcher in place of `tool unavailable`.
