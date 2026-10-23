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
