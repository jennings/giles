# Skills and Memory Files

Skills and memory are Markdown files that people write and the agent reads ([Plan](../PLAN.md) §6.6,
§6.7, §11.3). This page covers how the core parses and discovers them. The user-facing format is in
[Skills and Memory Files](../user/skills-and-memory.md).

## Front-Matter

`SKILL.md` and memory files may start with YAML front-matter. The private module
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

`SkillSource::prefix` is for toolset skill directories (§7.10, task 2.21): with `Some(instance)`,
each skill from that source is named `<instance>__<skill>`. Name validation and the directory-name
check apply to the bare name.

## Memory Files

`giles_core::memory` reads and updates memory files: agent-wide `agents/<agent>/MEMORY.md`, per-user
`agents/<agent>/users/<user>/MEMORY.md`, and the person-written profile `users/<user>/USER.md`.

| Item                                  | Purpose                                                          |
| ------------------------------------- | ---------------------------------------------------------------- |
| `agent_memory_file(root, agent)`      | `agents/<agent>/MEMORY.md`                                       |
| `user_memory_file(root, agent, user)` | `agents/<agent>/users/<user>/MEMORY.md`                          |
| `user_profile_file(root, user)`       | `users/<user>/USER.md` (read with `read`, never `apply`)         |
| `MemoryDoc`                           | A parsed file: `parse`, `entries`, `body`, `chars`, `apply`      |
| `MemoryOp`                            | `Add { content }`, `Patch { old_text, content }`, `Remove { .. }` |
| `BatchError`                          | Why a batch was rejected, with the operation's index             |
| `read(path).await`                    | The file as a `MemoryDoc`; a missing file is empty               |
| `apply(path, ops, cap).await`         | Applies a batch under the file lock and returns the result       |
| `usage_header(used, cap)`             | `[67% — 1,474/2,200]`                                            |

The `cap` argument is the agent's `memory_cap` (`config::AgentConfig::memory_cap`, a `u32` that
config validates as positive; its default, 2,200, is `config::DEFAULT_MEMORY_CAP`). Callers convert
it with `as usize`.

### Format

- Front-matter, if present, is kept verbatim (byte for byte, including comments, unknown fields, and
  `\r\n`) and written back unchanged. The only exception: if the closing `---` was the file's last
  line without a line ending and entries follow, a `\n` is added.
- The rest is split at lines holding only `§` (surrounding spaces and tabs allowed). A `§` inside a
  line is ordinary text. Line endings are normalized to `\n`; each entry loses leading blank lines
  and trailing whitespace; empty entries are dropped. A file without front-matter loses a leading
  byte-order mark.
- `MemoryDoc::render` writes the front-matter, the entries joined by `\n§\n`, and a final newline.
  So the first model write normalizes the layout of a person-written body (blank lines around
  separators, `\r\n`); the text of each entry is kept.
- Rendering always round-trips: `parse(render(doc))` has the same entries. Without front-matter, a
  body whose first entry starts with a `---` line and has a later `---` line (a thematic break or a
  setext heading underline) would read back as front-matter, swallowing entries out of the prompt
  and the cap; a body starting with a byte-order mark would lose it. In those cases the file starts
  with a `§` line, which parses as an empty entry and is dropped. A test renders and re-parses every
  combination of up to three tricky entries, with and without front-matter.
- `body()` is the entries joined by `\n§\n`, without front-matter: what the prompt shows. `chars()`
  is its length in Unicode scalar values, computed without building it. Front-matter doesn't count
  toward the cap.

### Batches

`MemoryDoc::apply(ops, cap)` is pure. Operations run in order on a working copy, each seeing the
previous ones' result, and the first failure rejects the whole batch:

- `add` appends `content` as a new entry (trimmed like a parsed entry). Empty content is rejected.
- `patch` replaces the only occurrence of `old_text` with `content`. Occurrences are counted across
  all entries, overlapping ones included (`aa` occurs twice in `aaa`), so the span is unique and
  always inside one entry; it can't cross a separator. An entry left empty is removed, which makes
  `patch` with empty `content` a way to delete part of an entry.
- `remove` removes the only entry containing `old_text`. Several occurrences inside that one entry
  are fine; two matching entries are ambiguous.
- Matching is exact and case-sensitive. `old_text` and `content` have `\r\n` normalized to `\n` to
  match the parsed file. Empty `old_text` is rejected.
- No entry may end up containing a line that holds only `§`: it would silently split the entry.
- Errors name the operation as `operations[<index>] (<op>)`, counting from 0 like the tool's JSON
  array, and say how to fix the call (for example, include more text to make a match unique).

The cap is checked once, on the final result: a batch may pass through an over-cap state. When the
result is over, `BatchError::OverCap` carries the current (unchanged) entries, their size, the cap,
and the overflow, so the tool result can show the model what to consolidate. Text is never
truncated. A file that people pushed over the cap therefore accepts only batches that end at or
under it. A batch that shrinks the file but leaves it over the cap still fails: the plan says an
over-cap file makes model writes fail until someone consolidates, and the overflow in the error
tells the model exactly how much one batch must free.

`usage_header` rounds the percentage down, so 100% means exactly at the cap; an over-cap file shows
more than 100%. Numbers use commas between thousands.

`MemoryOp` deserializes from `{"op": "add" | "patch" | "remove", ...}`; the `memory` tool (task
2.11) owns the JSON schema the model sees and the scope rules (who may write which file). `apply`
never refuses a path by itself, so the tool must not pass `USER.md`.

### Writing

- `read` takes no lock: writes replace the file by rename, so a reader sees the old or the new file.
  A missing file is empty memory; a file that exists but isn't UTF-8 is an error.
- `apply` resolves the path to a canonical lock key (the longest existing ancestor is
  canonicalized and the rest appended, so `a/../a/MEMORY.md` and a symlinked directory share a
  key), then takes that file's `tokio::sync::Mutex` from a process-wide map. Map entries nobody
  holds are dropped on the next lookup. The lock is per process; v1 has one daemon.
- Under the lock it reads, applies, and writes only if the result differs. The write creates parent
  directories (mode 0700), writes a `tempfile` named `.MEMORY.md.<random>.tmp` in the same directory
  with the old file's permissions (0600 for a new file), fsyncs it, renames it over the target, and
  fsyncs the directory. A failed write leaves the old file and removes the temporary file.
- If the path is a symbolic link, its target is written, so the link survives. This holds for a
  dangling link too: the link itself is followed (`read_link`, a relative target taken from the
  link's canonical directory with `.` and `..` resolved lexically) before the ancestors are
  canonicalized, so the first write creates the target.
- The file I/O runs in `spawn_blocking`, and the lock guard moves into that closure: if the caller's
  future is dropped mid-write, the write still finishes before the next batch can start.

A test runs 48 concurrent batches against one file through three different path spellings on a
multi-threaded runtime and checks that every entry lands; without the lock it loses most of them.
