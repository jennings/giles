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

Approval prompts look like "Allow `shell__exec` (`rm -rf ./build`)? Reply `yes 4821` or `no 4821`."
You can leave out the code (`yes`) when only one request is waiting; otherwise include it. `yes` and
`no` count as approval replies only while a request is waiting for you in that conversation;
otherwise they're ordinary messages to the agent. Any other message while a request waits denies it,
and the agent sees your message.

Anything else is passed to the agent, including other `/words`, a command with more text after it
(`/new please`), and any message with an attachment.

Commands work only in conversations a [route](configuration.md#routes) sends to an agent, and only
from the owner (see [`[owner]`](configuration.md#owner)).
