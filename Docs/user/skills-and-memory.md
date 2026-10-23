# Skills and Memory Files

An agent's knowledge lives in Markdown files under `GILES_HOME` that you can read and edit: skills
teach it how to do things, and memory files hold facts it should keep in mind.

The agent loop that puts these files in front of the model is not built yet; this page describes
the file formats Giles reads.

## Skills

A skill is a directory holding a `SKILL.md` file and any resource files it refers to, in the
[Agent Skills format](https://agentskills.io/specification), so skills written for other agents
work unchanged. Each skill's name and description are shown to the agent in every conversation; the
agent reads the rest of `SKILL.md` only when it decides the skill is relevant.

Put skills in either place:

- `skills/<name>/SKILL.md` for skills every agent can use.
- `agents/<agent>/skills/<name>/SKILL.md` for one agent's skills. A skill here replaces a global
  skill with the same name for this agent.

`SKILL.md` starts with YAML front-matter between two `---` lines, followed by the instructions in
Markdown:

```markdown
---
name: weekly-review
description: Prepare the weekly review. Use when asked for a weekly summary or review.
---

# Weekly Review

1. List what was finished this week.
2. ...
```

- `name` is required: lowercase letters, digits, and hyphens, at most 64 characters, not starting
  or ending with a hyphen and without two hyphens in a row. It must be the same as the directory's
  name.
- `description` is required, at most 1,024 characters. Say what the skill does and when to use it:
  this is all the agent sees until it loads the skill.
- Other fields from the format (`license`, `metadata`, `allowed-tools`, ...) are allowed and
  ignored.

A skill with a mistake is skipped, and Giles logs a warning naming the file and the problem; the
agent's other skills keep working. Skills are read when a conversation starts, so edits need no
restart. Giles never runs scripts that a skill contains.

## Memory Files

Memory is plain Markdown, one file per scope:

| File                                    | What it holds                                 | Who writes it        |
| --------------------------------------- | --------------------------------------------- | -------------------- |
| `agents/<agent>/MEMORY.md`              | What the agent remembers in general           | The agent and you    |
| `agents/<agent>/users/<user>/MEMORY.md` | What the agent remembers about one person     | The agent and you    |
| `users/<user>/USER.md`                  | A person's profile, shown in their DMs only   | People only          |

A memory file holds entries separated by a line containing only `§`:

```markdown
---
# Optional front-matter. No fields are required; Giles keeps it as written.
reviewed: 2026-10-01
---
Stephen prefers short answers and metric units.
§
The home server is a Raspberry Pi 5 named `pantry`.
It runs the backups at 02:00.
§
Weekly review happens on Fridays.
```

- An entry can span several lines and paragraphs. A `§` inside a line is ordinary text.
- Front-matter (between two `---` lines at the top) is optional and kept exactly as you wrote it
  when the agent updates the file. It isn't counted toward the size limit.
- A missing file is the same as an empty one; Giles creates it, and its directories, on the first
  write.

The agent edits memory one batch at a time: it adds entries, patches a piece of text inside one
entry, or removes the entry containing some text. A batch either applies completely or not at all.
When the agent rewrites a file, it tidies the layout: one `§` line between entries, no blank lines
around them, `\n` line endings. The text of each entry is kept.

Each memory file the agent writes is limited to `memory_cap` characters (default 2,200, set per
agent in `agent.toml`). The agent sees how full it is in a header such as `[67% — 1,474/2,200]`. A
change that would go over the limit is refused, and the agent is shown the current entries so it
can merge or drop some. Giles never cuts text you wrote: if you make a file longer than the limit,
it stays that way, and the agent's changes to it fail until it (or you) brings the file back under
the limit.

You can edit memory files at any time. Each conversation reads memory when it starts, so your
changes and the agent's show up in conversations that start afterward.
