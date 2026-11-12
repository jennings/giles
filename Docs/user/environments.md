# Environments

This page explains what an agent can do in an environment, how to let an agent use one, and how long
its workspace lasts.

An environment is an isolated place where an agent runs commands and works with files, such as a
container. An environment plugin provides it, and you configure the plugin in `giles.toml`. The
[Docker environment](docker-environment.md) is the first one. A workspace is one conversation's
place in an environment. Each conversation has at most one workspace, and the agent decides when to
create it.

## Let an agent use an environment

Open the agent's **Edit** page in the [web interface](web.md#agents), tick the environments it may
use under **Environments**, and tick the tools that work in them under **Tools**: `bash`, `read`,
`write`, `edit`, `find`, `grep`, and the four `environment_*` tools. The **New agent** page has the
same fields. If you tick an environment but none of its tools, or the tools but no environment, the
page tells you.

The form writes the same settings you can put in the agent's `agent.toml` yourself:

```toml
environments = ["docker"]
tools = ["environment_*", "bash", "read", "write", "edit", "find", "grep"]
```

An agent with no `environments` is never offered these tools, whatever its `tools` say. Giles also
checks every call, and refuses an environment the agent doesn't list.

## What the agent can do

| Tool                  | What it does                                                                |
| --------------------- | --------------------------------------------------------------------------- |
| `environment_plugins` | Lists the environments the agent may use, with their options                |
| `environment_create`  | Creates the conversation's workspace in one of them                         |
| `environment_status`  | Shows the workspace: its environment, options, state, age, and disk use     |
| `environment_reset`   | Deletes the workspace and creates a new one, by default with the same options |
| `environment_delete`  | Deletes the workspace                                                       |
| `bash`                | Runs a shell command in the workspace                                       |
| `read`                | Shows a file with line numbers                                              |
| `write`               | Writes a whole file                                                         |
| `edit`                | Replaces exact text in a file                                               |
| `find`                | Finds files by a glob on their path                                         |
| `grep`                | Searches file contents with a regular expression                            |

The agent has no workspace until it creates one. If it tries `bash` or another workspace tool first,
the error lists the environments it may use and tells it to call `environment_create`. Giles never
creates a workspace by itself. A conversation that already has one can't create a second.

If the agent's workspace is deleted, for example by you or after a long time unused, the next
workspace tool call fails and says when and why, and which environment and options the workspace
had. The agent can then create it again, or tell you.

The options for a workspace depend on the environment. For the Docker environment, the model picks
an image from the list you allowed.

## Commands and files

Each `bash` call is a fresh `/bin/sh -c` in the workspace's working directory. A `cd` or an `export`
doesn't carry over to the next call, but files do. A command runs for 2 minutes at most unless the
agent asks for more, up to 10 minutes, and then it is killed. When you send `/stop`, a running
command is killed too.

Giles keeps limits on what the agent sees, the same for every environment:

- `bash` shows the first 10 KiB and the last 20 KiB of a command's output, and says how much it left
  out.
- `read` shows up to 2000 lines of the first 256 KiB of a file, and says what it left out. It
  refuses binary files.
- `edit` changes text only if it matches once, unless the agent asks to replace all matches, and it
  refuses files over 8 MiB.
- `find` and `grep` return at most 200 results, and `grep` cuts lines over 500 characters.

## How long a workspace lasts

A workspace belongs to its conversation. `/new` starts a fresh session and keeps the workspace, so
the agent can carry on with the same files.

A workspace goes away when:

- you reset it with [`/env reset`](commands.md#resetting-the-workspace), or the agent resets or
  deletes it
- it has gone unused, and its conversation has had no messages, for the environment's `delete_after`
- the environment loses it, for example when you remove its container by hand
- it belongs to a scheduled job's run that ended

An `"isolated"` [job](configuration.md#jobs) gets its own workspace for each run, which is deleted
when the run ends. A `"dm"` job uses your DM's workspace.

After `stop_after` without use, the environment stops the workspace and keeps its files. The next
tool call starts it again, and the agent is told that processes it left running have ended. If the
environment had to recreate the workspace, the agent is told what was lost. See
[the environment settings](configuration.md#environments) for `stop_after` and `delete_after`.

## Ask before risky tools

The agent doesn't need your approval for `environment_reset` and `environment_delete`. Their
descriptions tell it to go ahead when you clearly want the files gone, and to ask you first when it
isn't sure. To make Giles ask you every time, add them to the agent's `ask` list. You can do the
same for `bash`:

```toml
ask = ["environment_reset", "environment_delete", "bash"]
```

A call to one of these waits for your [approval](commands.md#approving-tool-calls). In scheduled
jobs nobody is there to answer, so these calls are denied unless the job sets
[`ask_to`](configuration.md#jobs).
