# Agent Loop

`giles_core::agent_loop` runs turns ([Plan](../PLAN.md) §6.3, §6.7): it assembles each session's
prompt prefix and, per turn, calls the provider, runs the requested tools, and repeats until the
model stops asking for tools. The session code that hands it turns is in
[Sessions and Turns](sessions.md); the files it reads are in
[Skills and Memory Files](skills-and-memory.md).

| Item                          | What it is                                                         |
| ----------------------------- | ------------------------------------------------------------------ |
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
