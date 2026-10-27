# Skills and Memory

Skills are Markdown files that people write and the agent reads; memory is a per-agent database the
agent writes through tools ([Plan](../PLAN.md) §6.6, §6.7, §11.3). This page covers how the core
parses and discovers skills and how memory is built on the memory database. The user-facing format
is in [Skills and Memory](../user/skills-and-memory.md).

## Front-Matter

`SKILL.md` and the user profile `USER.md` may start with YAML front-matter. The private module
`giles_core::front_matter` only finds the block's boundaries:

- The first line (after an optional UTF-8 byte-order mark) is `---`, and the block ends at the next
  line that is exactly `---`. Delimiter lines may end in `\n` or `\r\n` and carry trailing spaces or
  tabs. Without both lines there is no front-matter and the whole file is body.
- The part up to the closing line, opening `---` included, is itself a YAML stream (`---` starts a
  document), so skills parse that slice and the parser's line numbers match the file's.

## Skills

`giles_core::skills` implements the [Agent Skills format](https://agentskills.io/specification). A
skill is a directory with `SKILL.md` and optional resource files.

| Item                           | Purpose                                                            |
| ------------------------------ | ------------------------------------------------------------------ |
| `parse_skill(dir, text)`       | Parses the contents of `dir/SKILL.md` into a `giles_types::Skill`  |
| `load_skill(dir)`              | Reads and parses `dir/SKILL.md`                                    |
| `validate_skill_name(name)`    | The specification's name rules                                     |
| `SkillSource { dir, prefix }`  | A directory whose subdirectories are skills                        |
| `agent_sources(root, agent)`   | `<root>/skills/` then `agents/<agent>/skills/`                     |
| `discover(sources)`            | `Discovery { skills: SkillSet, warnings: Vec<SkillError> }`        |
| `SkillSet`                     | `get(name)`, `iter()` and `summaries()` sorted by name, `len()`    |

### Parsing

- The front-matter must be a YAML mapping with string `name` and `description`. Other fields
  (`license`, `compatibility`, `metadata`, `allowed-tools`, anything else) are accepted and ignored.
  Duplicate keys are an error.
- `name` follows the specification: 1 to 64 characters, only lowercase alphanumeric characters and
  hyphens (a character is lowercase when lowercasing leaves it unchanged, so `café` and `数据` are
  valid), no leading, trailing, or consecutive hyphens. Such a name never contains `__`, so a local
  skill can't look like a toolset skill (`<instance>__<skill>`).
- `name` must equal the skill directory's name exactly (no Unicode normalization). The specification
  requires it, and the directory name is what an agent skill overrides a global one by, so a
  mismatch would make overrides surprising.
- `description` is trimmed and must be 1 to 1,024 characters. Line breaks inside it are kept; prompt
  assembly decides how to lay it out.
- The body is everything after the closing `---` line, with `\r\n` normalized to `\n`, leading blank
  lines removed, and trailing whitespace trimmed. Leading spaces on the first line are kept (they
  may be an indented code block).
- Every `SkillError` names the file or directory at fault. YAML errors are one line, with line and
  column counted from the top of `SKILL.md`.

The YAML parser is `serde-saphyr` (pure Rust, panic-free, with a default budget against alias
bombs), built with only its `deserialize` feature. `serde_yaml` is deprecated, and its forks wrap
`unsafe-libyaml`.

### Discovery

`discover` walks an ordered list of `SkillSource`s. Later sources override earlier ones by skill
name, so for local skills `agent_sources` lists the global directory first. In each source:

- Every subdirectory is a skill, except hidden ones (names starting with `.`). Symbolic links are
  followed; plain files are ignored. A missing source directory holds no skills.
- Subdirectories are read in name order, so warnings come out in a stable order.
- A skill that fails to load (no `SKILL.md`, bad front-matter, a dangling symlink, a non-UTF-8
  directory name) is skipped and its `SkillError` goes into `warnings` for the caller to log. A
  skipped skill overrides nothing: a broken agent skill leaves the global one of the same name in
  place.

The result is a `SkillSet` keyed by name in a `BTreeMap`, so listings are sorted and the prompt
prefix stays stable for caching (§6.5). Discovery reads every `SKILL.md` in full (bodies are small),
so `load_skill(name)` can be answered from the set built at session start. All of this is blocking
file I/O; async callers run it on a blocking thread.

`SkillSource::prefix` is for toolset skill directories (§7.10): with `Some(instance)`, each skill
from that source is named `<instance>__<skill>`. Name validation and the directory-name check apply
to the bare name.

### Resource Files

The files next to `SKILL.md` are a skill's resources; plugins never read `GILES_HOME`, so the core
hands them over as media (§6.6). `list_resources(dir)` lists them and `read_resource(dir, path)`
reads one, both blocking:

- Listed: regular files under the skill's directory, as `/`-separated relative paths in sorted
  order, except the top-level `SKILL.md`, hidden names (starting with `.`) at any level, non-UTF-8
  names, and anything more than `MAX_RESOURCE_DEPTH` (8) directories down. A symbolic link is listed
  when it resolves to a regular file inside the skill's directory; links to directories aren't
  followed. At most `MAX_RESOURCE_FILES` (200), with `truncated` set past that.
- Readable: only paths the listing could show. A path that is empty, absolute, or has an empty,
  `..`, or hidden component (`.` included) is refused before touching the disk. The rest is joined
  to the skill's canonical directory and canonicalized with every link followed; the result must be
  a regular file strictly inside that directory, so neither `..` nor a link can reach other files.
  Files over `MAX_RESOURCE_BYTES` (10 MiB) are refused.
- The read can't be raced: anything but a regular file (a FIFO would block the open) is refused
  before opening; the canonical path is opened with `O_NOFOLLOW | O_NONBLOCK`, so a link or FIFO
  swapped in for the file since fails; and the path of the descriptor actually opened (Linux
  `/proc/self/fd/N`, macOS `fcntl(F_GETPATH)`) must still be inside the skill's directory, which
  catches a directory above it swapped for a link.
- Errors (`ResourceError`) name the path as requested, never where the skill lives.

Config loading also keeps plugins out of skill directories: a toolset's `skills` directory may not
overlap the plugins' data directories, and no `write` or `unix` grant may overlap one (the same rule
as for the directories of binaries Giles runs). Local skills live under `GILES_HOME`, which grants
can't overlap anyway. `giles_core::media::media_type_for(name, bytes)` gives a file's media type
from its extension, else `text/plain` for UTF-8 text, else `application/octet-stream`.

### The `load_skill` and `send_attachment` Tools

Core tools in `giles_core::core_tools::{load_skill, send_attachment}`, registered by
`core_tools::register` with the runtime's blob store.

`load_skill(name, file?)` (`LoadSkillTool`; parallel-safe) answers from `TurnContext::skills`, the
`SkillSet` the session's prompt lists (`SessionPrompt::skills`), so a name the prompt doesn't show
is `unknown skill <name>`:

- Without `file`: the body of `SKILL.md`, then
  `Files of this skill (read one with load_skill and file):` and one `- <path>` line per
  `list_resources` entry (`- (more files not listed)` when truncated). A listing failure is logged
  and leaves the body alone.
- With `file`: `read_resource` (so nothing outside the skill's directory, hidden, or over 10 MiB),
  then the bytes go into the blob store named by the file's name with `media_type_for`'s type. The
  result is `` `<path>` (<type>, <n> bytes): `` and the text, for UTF-8 text without NUL bytes up to
  `MAX_INLINE_TEXT_BYTES` (64 KiB), else `` `<path>` (<type>, <n> bytes) is attached. ``; then the
  file's reference and media block (`media::attachment`). The file is then media of the session like
  any other: the model can pass `{"$media": "<id>"}` to a tool (§6.6, §7.10) or send it.

`send_attachment(media, caption?)` (`SendAttachmentTool`; not parallel-safe) sends one of the
session's media (`CoreCall::media`, every media block of the history, by id) into the turn's
conversation (`TurnContext::reply`) as an `OutboundMessage` with the caption as its Markdown (empty
without one) and the media as its one attachment (`Reply::send_with_attachments`). It answers
`Sent [media:...].`, or an error result: `unknown media <id>` (not in the session's history),
`this session has no conversation to send to` (an isolated job run),
`media <id> is no longer stored` (`BlobStore::stat`), or `sending failed: <error>` (e.g. the channel
isn't registered). The sent attachment isn't stored as a message (that would separate the `ToolUse`
from its result): the transcript has the call and its result, and the web chat's history shows a
call that succeeded as an assistant message with the caption and the media.

## Memory

Memory is a SQLite database per agent that the model reads and writes through tools; no Markdown
file holds a memory. The design is in [Plan](../PLAN.md) §6.7 and §11.3. The database layer (schema,
visibility, limits, all enforced inside the write transaction) is
[Storage](storage.md#memory-database); this section covers what the core builds on it.

| Piece                                                                         | Where                                                              |
| ----------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| `MemoryStore`, `MemoryViewer`, `MemoryOwner`                                  | `giles-store` (`memory.rs`, `memory_migrations/`)                  |
| `MemoryDbs`                                                                   | `giles_core::memory_dbs`: the open databases, by agent             |
| `viewer`, `prompt_viewer`, `line`, `flags`, `usage_header`, `user_profile_file` | `giles_core::memory`                                             |
| `memory_add`, `memory_edit`, `memory_forget`, `memory_recall`                 | `giles_core::core_tools::memory_*`                                 |
| the `# Memory` section                                                        | `giles_core::agent_loop::prompt` ([Agent Loop](agent-loop.md#system)) |

### Data Layout

- `agents/<agent>/memory.db` (`DataRoot::agent_memory_db`, in the agent's directory, not under
  `data/`) holds the agent's memories. It is created on first use with mode 0600, and moves with the
  agent when it is removed or restored, is part of every backup, and is rewritten by an instance
  rename ([Storage](storage.md#memory-database), [Backups](backups.md),
  [Instance Rename](instance-rename.md)).
- `users/<user>/USER.md` is the person-written profile, the only memory-related file left. People
  edit it, and the prompt loads it in a user's direct and job sessions; no tool reads or writes it.
- Nothing reads or writes `MEMORY.md` any more, at any level. Existing files stay on disk and are
  ignored; nothing is migrated. The old `memory` tool, the `§` entry format, the per-file locks, and
  the rename-based file writes were removed with them.

### `MemoryDbs`

`MemoryDbs::new(root, journal_mode)` is built once in `Runtime::start` and shared (the core tools,
the agent loop, the agent manager). `get(agent)` opens `agents/<agent>/memory.db` on first use, in
the journal mode of `[core] journal_mode` as for `giles.db`, and keeps it open; concurrent first
calls open the file once (one `OnceCell` per agent), and a failed open isn't remembered. A database
nothing asks for is never opened, so an agent that never remembered anything has no `memory.db`
until its first `get`. `close(agent)` drops the registry's handle so the writer checkpoints and the
`-wal` and `-shm` files go away (removing an agent calls it before moving the directory). Callers
therefore never keep a `MemoryStore` between calls or turns: each use gets a handle with `get` and
drops it.

### Who Sees What

`giles_core::memory` decides which `MemoryViewer` (the session's owner scope and `can_write_shared`)
a call or the prompt uses; the store applies it. The scopes:

- A direct or job session belongs to a user: its private scope is `MemoryOwner::User`. A runtime job
  runs as its creator, in the creator's session, so it is the creator's turn.
- A group session's private scope is the conversation,
  `MemoryOwner::Group { instance, conversation }`: the channel instance and its conversation id, so
  the same conversation id on another instance is another group. A participant's private memories
  are never in a group, and a group's never in a participant's DM. v1 ignores groups at the router
  ([Identity and Routing](identity-and-routing.md)), so no group session exists yet, but the scope,
  the store, the tools, and the prompt handle it.
- A config job has no requester. Its viewer is `MemoryOwner::None`: shared memories only, and what
  it writes has no owner and is shared.
- Shared memories (`shared = true`) are visible in every session of the agent, whoever wrote them.

`memory::viewer(session, requester)` is for a tool call. With no requester (a config job) it gives
`MemoryOwner::None` with `can_write_shared` true. Otherwise the owner scope is the session's, and
`can_write_shared` is true when the requester is the agent owner (`identity::owner()`). In a direct
or job session, a requester who isn't the session's user gets the error
`not permitted for <requester>: this conversation's private memories belong to <user>`; any
participant may use a group's scope.

`memory::prompt_viewer(session)` is for the prompt snapshot. No requester is known when the prompt
is built (once per session), so it assumes the session's own user: the same owner scope, and
`can_write_shared` true only in a direct or job session of the agent owner. In a group it is false:
who may write shared memories there depends on who speaks, so the prompt treats them as read-only.

**Known difference for config jobs.** The prompt can't tell a config job's session from a runtime
job's, since both are the user's DM or a job session owned by them. A config job's prompt therefore
lists the owner's private core memories, which its tools (viewer `None`) can neither read nor write;
its `memory_recall` finds shared memories only, and what it adds must be shared. The tools are
authoritative; the prompt only shows more.

The rules are enforced in three places:

| Rule                                                                                              | Enforced by                                                           |
| ------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| Private memories are visible only to their owner scope, shared ones to all; an invisible id reads as unknown | Store (`MemoryViewer::sees`, checked before any permission) |
| Creating, sharing, editing, un-sharing, or forgetting a shared memory needs `can_write_shared`    | Store, inside the write transaction (`Error::SharedWriteForbidden`)   |
| A memory with no owner must be shared; un-sharing makes a memory private to the viewer who did it | Store                                                                 |
| At most 1,000 characters per memory; the `memory_cap` limit on each scope's core memories         | Store, inside the write transaction                                   |
| Which viewer a turn is: the requester against the session's user, shared writes for the agent owner only | `memory::viewer`, used by the tools                            |
| What the prompt lists, and whether it shows a usage header for a scope                            | `memory::prompt_viewer` and `prompt::load_memory`                     |

So a guest or a group can't plant a shared memory (shared memories reach every prompt, including the
owner's tool-enabled DMs), and a bug in a tool can't skip the check: the store refuses it.

### The Limits

All checked by `MemoryStore` in the transaction of each write, so concurrent writers can't break
them (the single writer connection also runs writes one at a time):

- Content is trimmed, can't be empty, and has at most `MAX_MEMORY_CHARS` (1,000) characters.
- `memory_cap` (`config::AgentConfig::memory_cap`, a `u32` that config validates as positive,
  default `config::DEFAULT_MEMORY_CAP`, 2,200) caps the total characters of the **core** memories of
  each scope: the shared scope, each user's, each group's. The tools pass it to the store as
  `core_cap`. Memories that aren't core don't count toward it, and have no total limit. A write that
  would take a scope over the cap and grow it fails; a write that doesn't grow it always succeeds,
  so a scope that is over a lowered cap can still shrink. Sharing and un-sharing move a core memory
  between scopes, so the target scope is checked.
- No edit history is kept; `memory_forget` deletes the row.

### Rendering

Tool results and the prompt show a memory the same way (`memory::line`):
`#12 (core, shared): content`. The flags (`memory::flags`) are `core, shared`, `core`, `shared`, or
`private`. `memory::usage_header(used, cap)` is `[67% — 1,474/2,200]`: the percentage rounds down,
so 100% means exactly at the cap and an over-cap scope shows more; numbers use commas between
thousands. With a cap of 0, empty is 0% and anything else 100%.

### The Memory Tools

Four core tools in
`giles_core::core_tools::{memory_add, memory_edit, memory_forget, memory_recall}`, registered by
`core_tools::register` at the end of `Runtime::start` (see [Agent Loop](agent-loop.md#core-tools))
and reserved in `CORE_TOOLS` ([Registry](registry.md)). Each holds the `MemoryDbs`, the live
configuration, and a clock. Their input schemas are closed (`additionalProperties: false`, and
parsing uses `deny_unknown_fields`), and the descriptions tell the model the rules: what core means
and the cap, that a memory is private by default, who may share, and that the prompt is a snapshot.

| Tool            | Input                                                                           | Result                                                                                 |
| --------------- | ------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| `memory_add`    | `content` (required, 1 to 1,000 characters), `core` (default false), `shared` (default false) | `Added #<id> (<flags>): <content>`, then a usage line for the scope if the memory is core |
| `memory_edit`   | `id` (required, at least 1), `content`, `core`, `shared`; at least one of the three | `Updated #<id> (<flags>): <content>`, then a usage line for each core scope the memory left or entered |
| `memory_forget` | `id` (required, at least 1)                                                     | `Forgot #<id> (<flags>): <content>`, then the usage line of its scope if it was core   |
| `memory_recall` | `query` (required), `limit` (1 to 50, default 10)                               | `<n> memories, best first:` (`1 memory` for one) and, per hit after a blank line, the memory's line and `created <time>, updated <time>`; `No matching memories.` for none |

- `memory_edit`'s `content` replaces the whole old content: a memory holds one fact, so there is no
  patching. An empty `content` is refused with a pointer to `memory_forget`.
- The usage line is `<title> <usage_header>`, with the title `Shared core memories`,
  `Your core memories`, or `This group's core memories`.
- Every call builds an `Access` (`core_tools::memory`): the viewer from `memory::viewer`, the
  agent's `memory_cap` and message time zone (times read as in the transcript,
  `Thu 2026-10-22 14:03 BST`), the session (recorded as the memory's `source_session`, never
  checked), and the clock, with a database handle from `MemoryDbs::get`. `memory_edit` reads the
  memory first (`MemoryStore::get`) to know which core scopes the edit touches.
- `memory_recall` is `MemoryStore::recall`: FTS5 over the visible memories, core ones included. The
  query is plain words, each quoted and OR-ed (as for `memory_search`, so FTS5 syntax is matched as
  text), with English stemming and diacritics ignored. Hits are ordered by bm25, then newest update
  first. It only reads, so it's in `PARALLEL_SAFE_CORE_TOOLS` with `load_skill` and `memory_search`;
  `memory_add`, `memory_edit`, and `memory_forget` write and run alone.
- Nothing touches the session's cached prompt: a write shows in its tool result, and reaches the
  prompt at the next session or after compaction ([Agent Loop](agent-loop.md#system)).

**Errors** are error results in words the model can act on:

| Cause                                          | Message                                                                                  |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------- |
| Unknown or invisible id                        | `no memory #<id>` (the same text for both)                                               |
| Shared write without permission                | `shared memories can be created, shared, changed, or forgotten only in the owner's turns and in config jobs; private memories aren't affected` |
| Over 1,000 characters                          | `the memory is <n> characters; the limit is 1000. Shorten it, or split it into several memories` |
| Over the core cap                              | `not saved: <scope> would use <n>/<cap> characters, <overflow> over the cap; nothing was changed. ...`, then the scope's usage line and each of its core memories with its line and length (`(none)` if there are none). It says to shorten, forget, or demote with `core=false`, and that a memory too long for the cap can only be saved without `core` |
| Bad input                                      | `invalid input: ...`: an unknown or missing field, empty content or query, an edit giving none of `content`, `core`, `shared`, an edit that changes nothing, a private memory with no owner |
| Requester isn't the session's user             | `not permitted for <requester>: ...`                                                     |
| The agent isn't configured, or the database fails | ``agent `<agent>` is not configured``, `the memory database is unavailable`, `the memory database failed` (the cause is logged) |

## The `memory_search` Tool

A core tool (§6.7) in `giles_core::core_tools::memory_search`, registered by `core_tools::register`
with the others. It searches transcripts in `giles.db`, not memories, and the memory database
doesn't change it.

`memory_search(query, limit?)` (`MemorySearchTool`, holding the store; parallel-safe):

- Scope (`memory_search::scope`): in a direct or job session with a user,
  `SearchScope::direct(agent, user)` (every direct session of the user with this agent, every
  channel); in a group, `SearchScope::conversation`; otherwise an error.
- `limit` defaults to 10 (`DEFAULT_LIMIT`), at most 50 (`MAX_LIMIT`); a blank query is an error.
  `memory_recall` takes the same limits.
- Each hit is `<time> <user|you> on <channel>: <snippet>`, snippets with `**` around matches and
  newlines flattened. A user message's time is its stored time prefix (when it was sent), split out
  of the snippet with its match markers removed (`split_sent_prefix`: the prefix is indexed, so a
  query word like `22` or `BST` is highlighted inside it); other messages, and snippets cut before
  the prefix, show their stored time in the message time zone. No hits: `No matches.`
