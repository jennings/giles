# Skills and Memory

An agent's knowledge has three parts:

- `SOUL.md` says who the agent is.
- Skills teach it how to do things.
- Memories hold facts it should keep in mind.

You write `SOUL.md` and skills as Markdown files under `GILES_HOME`. The agent keeps its memories
itself, in a database.

## What the agent sees

When a conversation starts, the agent gets, in this order:

1. `agents/<agent>/SOUL.md`, if you wrote one. It says who the agent is and how it should behave, in
   plain Markdown.
2. The name and description of each [skill](#skills), if its `tools` allow `load_skill`.
3. Its core [memories](#memory). In your direct conversations, it also gets your profile
   `users/owner/USER.md`, which you write and the agent never changes.

This stays the same for the whole conversation. Edits to `SOUL.md` or `USER.md`, and new memories,
show up in the next conversation, which starts after `/new`, an idle reset, or a restart of Giles.
Compaction also refreshes the memories.

Each of your messages reaches the agent with the time you sent it, in your `owner.timezone`.

## Skills

A skill is a directory holding a `SKILL.md` file and any files it refers to. Skills use the
[Agent Skills format](https://agentskills.io/specification), so skills written for other agents work
unchanged.

The agent sees each skill's name and description in every conversation. It reads the rest of
`SKILL.md` only when it decides the skill is relevant.

Put a skill in one of these places:

- `skills/<name>/SKILL.md` for a skill every agent can use.
- `agents/<agent>/skills/<name>/SKILL.md` for one agent's skill. It replaces a global skill with the
  same name for that agent.

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

- `name` is required and must be the same as the directory's name. It has at most 64 characters, all
  lowercase letters, digits, and hyphens. It can't start or end with a hyphen or have two hyphens in
  a row.
- `description` is required, at most 1,024 characters. Say what the skill does and when to use it.
  It's all the agent sees until it loads the skill.

Giles reads skills when a conversation starts, so edits need no restart. A skill with a mistake is
skipped, and Giles logs a warning naming the file and the problem.

When the agent loads a skill, it sees a list of the skill's other files and can read any of them. It
can then hand a file to a tool or send it to you. It can't read files outside the skill's directory.

Giles never runs scripts that a skill contains.

## Memory

A memory is a short piece of text holding one fact, preference, or instruction, such as "Stephen
prefers metric units." The agent adds, changes, and forgets its memories itself. They aren't files
you edit. To see or change them yourself, use [the agent's memory page](web.md#memory).

### Turning the tools on

The agent gets the memory tools that its [`tools` list](configuration.md#agentsagentagenttoml)
allows:

| Tool            | What the agent does with it                                         |
| --------------- | ------------------------------------------------------------------- |
| `memory_add`    | Remembers something new                                             |
| `memory_edit`   | Replaces a memory's text, or changes whether it's core or shared    |
| `memory_forget` | Deletes a memory for good                                           |
| `memory_recall` | Searches its memories for words                                     |
| `memory_search` | Searches your [past conversations](#searching-past-conversations)   |

`tools = ["memory_*"]` allows all five. To allow fewer, list the ones you want. For example,
`["memory_recall"]` lets the agent look things up but not change what it knows. To approve each call
yourself, add the tool to `ask`, such as `ask = ["memory_forget"]`.

Without memory tools, the agent still sees its core memories, but it can't change them.

### Core and other memories

The agent decides whether a memory is core when it adds it, and can change that later.

- Core memories are in the agent's prompt, so it knows them from the first message of every
  conversation that can see them. Make a memory core only if the agent should always know it.
- Other memories stay out of the prompt. The agent finds them with `memory_recall`, which searches
  every memory the conversation can see, core ones included.

Only core memories take up space in the prompt, so an agent can remember far more than fits there.

### Private and shared memories

A memory is private by default. It belongs to the conversation it was written in:

- What the agent learns in your direct conversations, and in the jobs that report to you, is private
  to you.
- What it learns in a group chat is private to that group.

A group's memories never show up in your direct conversations. What you told the agent privately
never shows up in a group.

A shared memory is visible in every conversation the agent has, including other people's and
groups'. So only you can write one. The agent can create, share, change, or forget a shared memory
only in these places:

- Your direct conversations.
- Your own messages in a group.
- Jobs, both those in `agent.toml` and those the agent schedules for you.

Someone else's message in a group can't write shared memories. The agent can still keep private
memories about that group.

A job from `agent.toml` has no user. It sees only the shared memories, and what it remembers is
shared too.

### Limits

One memory holds at most 1,000 characters. The agent splits longer notes into several memories.

`memory_cap` in `agent.toml` (default 2,200) limits the total characters of the core memories in
each scope. The shared memories are one scope, and each user and each group has its own. Memories
that aren't core don't count, and an agent can keep any number of them.

A change that would take a scope's core memories over the cap is refused, and nothing changes. The
agent is shown that scope's core memories, so it can shorten or forget some, or make some not core.
A memory too long for the cap can only be saved as a non-core memory.

Lowering `memory_cap` never deletes anything. A scope that's already over it keeps its memories, but
changes that would make it longer are refused until it's under again.

### Where memories are stored

Each agent's memories are in `agents/<agent>/memory.db`, a SQLite database.
[`giles backup`](backups.md) includes it, and it moves with the agent's directory when you
[remove the agent](web.md#removing-an-agent) in the web UI.

Don't open or copy it while Giles runs, for the same reasons as `giles.db`.

### Older MEMORY.md files

Earlier versions kept memory in Markdown files:

- `agents/<agent>/MEMORY.md`
- `agents/<agent>/users/<user>/MEMORY.md`
- a `MEMORY.md` for each group

Giles no longer reads them and doesn't migrate them, so the agent starts with no memories. To keep
something from them, tell the agent to remember it. You can delete the files.

## Searching past conversations

The `memory_search` tool lets the agent look up something said before that it didn't keep as a
memory. It searches your direct conversations with that agent, on every channel. It doesn't search
job runs, other agents' conversations, or memories.

It returns short excerpts, best matches first, with when each message was sent.
