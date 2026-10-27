# Skills and Memory

An agent's knowledge has three parts. `SOUL.md` says who the agent is, skills teach it how to do
things, and memories hold facts it should keep in mind. You write the first two as Markdown files
under `GILES_HOME`; the agent keeps its memories itself, in a database.

## What the Agent Sees

When a conversation starts, Giles gives the model, in this order:

1. `agents/<agent>/SOUL.md`: who the agent is and how it should behave, exactly as you wrote it.
   It's optional; write it in plain Markdown.
2. The name and description of each skill the agent can use (below), if its `tools` allow
   `load_skill`, the tool it reads a skill with.
3. Its core memories ([Memory](#memory)): the shared ones, then the ones for this conversation,
   and, in your direct conversations, your profile `users/owner/USER.md`. Each scope the agent can
   write is shown with how full it is.

This stays the same for the whole conversation, which lets the model provider cache it and makes
replies faster and cheaper. So edits to `SOUL.md` or `USER.md` and new memories show up in
conversations that start afterward: after `/new`, an idle reset, or a restart of Giles (compaction
refreshes the memories too). The current time isn't part of it; instead, each of your messages
reaches the agent with the time you sent it, in your `[owner] timezone`, for example
`[Thu 2026-10-22 14:03 PDT] How was the weather?`.

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

When the agent loads a skill it also sees the list of the skill's other files (hidden files left
out) and can read any of them. A file it reads is shown to it and stored like an attachment, so
it can hand the file to a tool or send it to you (`send_attachment`). Only files inside the
skill's directory can be read: `..` and links that lead elsewhere are refused.

## Memory

An agent remembers by keeping memories: short pieces of text, each one a fact, preference, or
instruction, such as "Stephen prefers metric units." They aren't files you edit. The agent keeps
them in its own database, `agents/<agent>/memory.db`, and manages them with four tools:

| Tool            | What the agent does with it                                                    |
| --------------- | ------------------------------------------------------------------------------ |
| `memory_add`    | Remembers something new: `content`, and optionally `core` and `shared`         |
| `memory_edit`   | Changes a memory, named by its id: new `content`, and/or `core`, and/or `shared` |
| `memory_forget` | Deletes a memory by its id                                                     |
| `memory_recall` | Searches the memories it can see, `query` words and an optional `limit`        |

Every memory has an id, a number the agent uses to name it when it edits or forgets it. A memory
holds at most 1,000 characters, so one memory is one fact; the agent splits up longer notes.
`memory_edit` replaces a memory's whole text. `memory_forget` deletes it for good: no earlier
version is kept.

### Core and Other Memories

A memory is either *core* or not, which the agent decides when it adds it and can change later
with `memory_edit`:

- Core memories are loaded into the agent's prompt, so it knows them from the first message of
  every conversation that can see them. Make a memory core only if the agent should always know it.
- Other memories stay out of the prompt. The agent finds them with `memory_recall`, which searches
  every memory the conversation can see, core ones included. It matches any of the words you give,
  ranks the memories with more (and rarer) matching words first and the most recently changed first
  among equals, and ignores word endings (`running` finds `run`), case, and accents. It returns 10
  memories unless the agent asks for another `limit`, at most 50, each with its id, whether it's
  core and shared, when it was created and last changed (in your `[owner] timezone`), and its text.

So an agent can remember far more than fits in its prompt. Only the core memories take up space
there, and `memory_cap` limits them: see [Limits](#limits).

### Private and Shared Memories

Each memory is also private or shared:

- A **private** memory (the default) belongs to the conversation it was written in, and only that
  conversation's sessions see it. What the agent learns in your direct conversations, and in the
  jobs that report to you, is private to you; what it learns in a group chat is private to that
  group. A group's memories never show up in your direct conversations, and what you told the agent
  privately never shows up in a group.
- A **shared** memory is visible in every conversation the agent has: yours, other people's, and
  groups'.

Because a shared memory reaches every conversation, only you can create one: the agent may create
a shared memory, mark one as shared, or change or forget one only in your conversations (a direct
conversation, or you writing in a group) and in jobs. These are both the jobs in `agent.toml` and
the ones the agent schedules for you. Someone else's message in a group can't, though the agent
can still keep private memories about that group. If the agent tries anyway, the tool says that
shared memories can be written only in the owner's turns and in jobs.

A memory the conversation can't see is reported as "no memory #N", the same as an id that doesn't
exist, so one conversation can't learn about another's memories.

A job from `agent.toml` has no user, so it sees only the shared memories, and what it remembers is
shared too. (Its prompt may still list your private core memories, because Giles builds the
prompt per session and can't tell such a job from one you scheduled through the agent; the job's
memory tools can't read or change them, and `memory_recall` finds shared memories only.)

### What the Agent Sees in Its Prompt

In the prompt, memory comes after `SOUL.md` and the skills, under `# Memory`, in this order:

1. `Shared memory`: the core memories that are shared, if there are any or if this conversation
   may write them (yours do).
2. `User memory` in your direct conversations and jobs, or `Group memory` in a group: the core
   memories private to that user or group.
3. `User profile`, in your direct conversations only: your `users/<user>/USER.md`, which you write
   and the agent never changes. It isn't a memory and isn't part of `memory_cap`.

Each core memory is a line with its id and flags, for example:

```
## Shared memory [4% — 49/1,000]

What you remember in general, shared by all your conversations.

#1 (core, shared): The home server is `pantry`.
#2 (core, shared): Backups run at 02:00.
```

The bracketed usage header after a section's title says how full that scope's core memories are,
in characters against `memory_cap`: here 49 of 1,000, rounded down to 4%. A section the agent
can't write has no header. A section with no memories says `(empty)`. If the agent's `tools` allow
`memory_recall`, the prompt also tells it that memories that aren't core are found with that
tool.

As with `SOUL.md`, the prompt is a snapshot: Giles takes it when a conversation starts and again
when the conversation is compacted ([Configuration](configuration.md#agentsagentagenttoml)), so
it stays the same in between and the model provider can cache it. A memory the agent adds during
a conversation doesn't show in the prompt until then; the agent sees what it changed in the
tool's result, which shows the memory with its id and, for core memories, the scope's usage. A
memory added today is in the prompt of conversations that start afterward: after `/new`, an idle
reset, or a restart of Giles.

### Limits

- **One memory** holds at most 1,000 characters. A longer text is refused and the agent is told
  the limit.
- **`memory_cap`** (default 2,200, set per agent in `agent.toml`) caps the total characters of the
  *core* memories in each scope: the shared ones, each user's, and each group's. Memories that
  aren't core don't count against it, and there is no limit on how many of them an agent keeps.
  `memory_add` or `memory_edit` that would take a scope's core memories over the cap is refused and
  changes nothing; the error lists that scope's core memories and how far over the cap the change
  was, so the agent can shorten or forget some, or make some not core, and try again. A memory too
  long for the cap can be saved only as a non-core memory. Lowering `memory_cap` never deletes
  anything: a scope that is already over it stays as it is, and only changes that don't make it
  longer are allowed until it's under again.

### Turning the Tools On

The agent gets the tools its `tools` list allows, like any other tool
([Configuration](configuration.md#agentsagentagenttoml)). `tools = ["memory_*"]` turns on all four
memory tools, and `memory_search`, described below; list the names you want to give less, such as
`["memory_recall"]` for an agent that may look things up but not change what it knows. A tool in
`ask` needs your approval before each call (`ask = ["memory_forget"]`, for example). With no
memory tools allowed the agent still sees its core memories in the prompt; it just can't change
them.

### Where Memories Are Stored

Memories are in `agents/<agent>/memory.db`, one SQLite database for each agent, created the first
time the agent needs it. It's included in `giles backup` ([Backups](backups.md)) and moves with
the agent's directory when you remove the agent in the web UI ([Web UI](web.md#removing-an-agent)).
Don't open or copy it while Giles runs, for the reasons that apply to `giles.db`. The web UI's
agent page lists it with its size, not its contents. Viewing and editing memories in the web UI
is planned but not built yet.

### Older MEMORY.md Files

Earlier versions kept memory in Markdown files: `agents/<agent>/MEMORY.md`,
`agents/<agent>/users/<user>/MEMORY.md`, and a `MEMORY.md` for each group. Giles no longer reads or
writes them, and nothing is migrated: files that exist stay on disk, ignored, and the agent starts
with no memories. To keep something from them, tell the agent to remember it, or delete the files.
`SOUL.md`, `USER.md`, and skills work as before.

## Searching Past Conversations

The `memory_search` tool lets the agent look up something said before that it didn't keep as a
memory. It searches the text of your direct conversations with that agent, on every channel (not
job runs, not other agents' conversations, not memories; `memory_recall` searches those), and
returns short excerpts, best matches first, with when each message was sent. Matching ignores word
endings (`running` finds `run`), case, and accents.

Like the memory tools, it's offered only if the agent's `tools` allow it, for example
`tools = ["memory_*"]`.
