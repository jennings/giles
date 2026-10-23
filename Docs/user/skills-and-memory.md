# Skills and Memory Files

An agent's knowledge lives in Markdown files under `GILES_HOME` that you can read and edit:
`SOUL.md` says who the agent is, skills teach it how to do things, and memory files hold facts it
should keep in mind.

## What the Agent Sees

When a conversation starts, Giles gives the model, in this order:

1. `agents/<agent>/SOUL.md`: who the agent is and how it should behave, exactly as you wrote it.
   It's optional; write it in plain Markdown.
2. The name and description of each skill the agent can use (below), if its `tools` allow
   `load_skill`, the tool it reads a skill with.
3. Its memory: `agents/<agent>/MEMORY.md`, then, in your direct conversations, your profile
   `users/owner/USER.md` and what the agent remembers about you,
   `agents/<agent>/users/owner/MEMORY.md`. Each file the agent writes is shown with how full it
   is.

This stays the same for the whole conversation, which lets the model provider cache it and makes
replies faster and cheaper. So edits to these files show up in conversations that start afterward:
after `/new`, an idle reset, or a restart of Giles. The current time isn't part of it; instead,
each of your messages reaches the agent with the time you sent it, in your `[owner] timezone`, for
example `[Thu 2026-10-22 14:03 PDT] How was the weather?`.

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

The agent edits memory with its `memory` tool, one batch at a time: it adds entries, patches a
piece of text inside one entry, or removes the entry containing some text. A batch either applies
completely or not at all. When the agent rewrites a file, it tidies the layout: one `§` line
between entries, no blank lines around them, `\n` line endings. The text of each entry is kept.

Who may write which file:

- `agents/<agent>/MEMORY.md` is loaded into every conversation the agent has, so it's written only
  in your conversations and in jobs (both the jobs in `agent.toml` and the ones the agent schedules
  for you).
- `agents/<agent>/users/owner/MEMORY.md` is written only in your direct conversations and in jobs
  that report to you.
- `USER.md` is never written by the agent.

Each memory file the agent writes is limited to `memory_cap` characters (default 2,200, set per
agent in `agent.toml`). The agent sees how full it is in a header such as `[67% — 1,474/2,200]`. A
change that would go over the limit is refused, and the agent is shown the current entries so it
can merge or drop some. Giles never cuts text you wrote: if you make a file longer than the limit,
it stays that way, and the agent's changes to it fail until it (or you) brings the file back under
the limit.

You can edit memory files at any time. Each conversation reads memory when it starts, so your
changes and the agent's show up in conversations that start afterward. Within a conversation, the
agent sees its own changes in the `memory` tool's result.

## Searching Past Conversations

The `memory_search` tool lets the agent look up something said before that isn't in its memory.
It searches the text of your direct conversations with that agent, on every channel (not job runs,
not other agents' conversations, not memory files), and returns short excerpts, best matches first,
with when each message was sent. Matching ignores word endings (`running` finds `run`), case, and
accents.

Both tools are offered only if the agent's `tools` allow them, for example `tools = ["memory*"]`.
