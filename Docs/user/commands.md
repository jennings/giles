# Chat commands

Giles handles these messages itself, and they never reach the agent.

| Message                    | Effect                                                               |
| -------------------------- | -------------------------------------------------------------------- |
| `/new`                     | Start a fresh session in this conversation. The old history stays stored. |
| `/stop`                    | Stop the agent's current turn, including any tool calls it's running |
| `yes <code>`               | Allow the tool call the agent asked approval for                     |
| `yes! <code>`              | Allow it, and allow that tool for the rest of the session            |
| `no <code>`                | Deny it                                                              |

A command must be the whole message. Spaces around it and capitalization don't matter, so `/New` and
`Yes` work. Anything else goes to the agent, including:

- other `/words`
- a command with more text after it, such as `/new please`
- any message with an attachment

Commands work only from [the owner](configuration.md#owner), in conversations a
[route](configuration.md#routes) sends to an agent.

## Starting over and stopping

If the agent is still answering when you send `/new`, that answer finishes in the old session. So do
any messages you sent before `/new`. Only later messages start the fresh session.

`/stop` stops only the current turn. Messages still waiting for the agent are answered next.

## Approving tool calls

An approval prompt looks like this:

> Allow `shell__exec` (`{"command":"rm -rf ./build"}`)? Reply `yes 4821` or `no 4821`.

A tool can also ask a question while it runs:

> The tool `ci__deploy` asks: `Deploy to production?` Reply `yes 1093` or `no 1093`.

For a question, `yes!` counts as `yes`. In [the web chat](web.md), the prompt has buttons that send
these replies for you.

You can leave out the code when only one request is waiting. If more than one is waiting, include
it. When no request is waiting, `yes` and `no` are ordinary messages to the agent.

These end a waiting request as if you'd answered `no`:

- you don't answer within the agent's [`approval_timeout`](configuration.md), 10 minutes by default
- you send any other message, which the agent then sees
- you send `/stop`

Waiting requests don't survive a restart of Giles. If a request has ended and you still want the
tool to run, ask the agent again.

In scheduled jobs nobody is there to answer, so tools that need approval are denied unless the job
sets [`ask_to`](configuration.md#jobs).
