# Chat Commands

A few messages are handled by Giles itself and never reach the agent. Each must be the whole
message; surrounding spaces and capitalization don't matter (`/New` and `Yes` work).

| Message                    | Effect                                                               |
| -------------------------- | -------------------------------------------------------------------- |
| `/new`                     | End this conversation's session; your next message starts a fresh one. The old history stays stored. |
| `/stop`                    | Stop the agent's current turn, including any tool calls it's running |
| `yes <code>`               | Allow the tool call the agent asked approval for                     |
| `yes! <code>`              | Allow it, and allow that tool for the rest of the session            |
| `no <code>`                | Deny it                                                              |

If the agent is still answering when you send `/new`, that answer (and any messages you sent before
`/new`) finishes in the old session; only later messages start the fresh one. `/stop` when the agent
isn't doing anything has no effect. It stops only the current turn: messages still waiting for the
agent are answered next.

Approval prompts look like "Allow `shell__exec` (`{"command":"rm -rf ./build"}`)? Reply `yes 4821`
or `no 4821`." The prompt shows the tool's input as one line: line breaks and other control
characters appear escaped (`\n`), and long input is cut, followed by its size and the start of its
SHA-256 hash. A tool that asks a question while it runs looks like "The tool `ci__deploy` asks:
`Deploy to production?` Reply `yes 1093` or `no 1093`."; there `yes!` counts as `yes`. In the web
chat the prompt has Allow, Allow for this session, and Deny buttons, which send those replies for
you, and it changes to show how the request ended.

You can leave out the code (`yes`) when only one request is waiting; otherwise include it (Giles
says so and decides nothing). `yes` and `no` count as approval replies only while a request is
waiting for you in that conversation, or when their code names one that ended in the last day;
otherwise they're ordinary messages to the agent. A reply to a request that already ended tells you
how it ended: unanswered requests are denied after the agent's `approval_timeout` (10 minutes by
default, see [Configuration](configuration.md)), and `/stop` cancels them; ask the agent again if
you still want it. Any other message while a request waits denies it, and the agent sees your
message. Waiting requests don't survive a restart of Giles.

In scheduled jobs nobody is there to answer, so tools that need approval are denied unless the job
sets [`ask_to`](configuration.md#jobs).

Anything else is passed to the agent, including other `/words`, a command with more text after it
(`/new please`), and any message with an attachment.

Commands work only in conversations a [route](configuration.md#routes) sends to an agent, and only
from the owner (see [`[owner]`](configuration.md#owner)).
