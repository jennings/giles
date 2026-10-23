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
