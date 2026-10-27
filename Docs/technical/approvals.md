# Approvals

`giles_core::approvals` makes calls to the tools an agent's `ask` globs name, and questions a tool
asks mid-call (MCP elicitation), wait for the requester's answer ([Plan](../PLAN.md) §6.4, §7.10).
The agent loop asks through it ([Agent Loop](agent-loop.md)); the session manager hands it the
`yes`/`no`/`yes!` replies ([Sessions and Turns](sessions.md)), which the router recognizes only
while it says an approval is pending ([Identity and Routing](identity-and-routing.md)).

| Item                    | What it is                                                              |
| ----------------------- | ----------------------------------------------------------------------- |
| `ApprovalBoard`         | Every pending approval, in memory; implements `sessions::Approvals`     |
| `TurnApprovals`         | One turn's side: `approve_call`, `answer_question`, `has_pending`, `deny_pending` |
| `ApprovalTarget`        | Who answers: session key, user, the `Reply` the prompt goes to, and for jobs a `PromptOutbox` |
| `PromptOutbox`          | Sends a job turn's prompts through the outbox (`send`) and withdraws them (`withdraw`) |
| `ApprovalPresenter`     | Shows a prompt (`present`) and its outcome (`resolved`)                 |
| `ChannelPresenter`      | The daemon's: web chats get buttons, other channels the text (`TextPresenter`) |
| `ApprovalOutcome`       | `Allowed`, `AllowedForSession`, `Denied`, `Expired`                     |
| `call_prompt`, `question_prompt`, `shown`, `escape`, `code_span` | The injection-safe prompt text |
| `not_approved`          | The result of a call a new message denied                               |

## Wiring

`giles serve` builds one `ApprovalBoard::new(ChannelPresenter::new(registry))` and passes it to both
`AgentLoop::from_runtime(&runtime, board)` and `Runtime::start_sessions(runner, board, shutdown)`,
so the replies the session manager receives reach the approvals the agent loop waits on. Each turn
gets a `TurnApprovals::for_turn(board, agent, turn)`: the agent's `ask` globs and
`approval_timeout`, answered by the turn's requester in the session's conversation.

## What Waits

When a call starts, after the dispatcher's call-time checks (an unavailable tool is reported as
such, not asked about), `approve_call(tool, input)`:

- runs it at once unless `tool` matches an `ask` glob (the same globs as `tools`), or `yes!` allowed
  that tool earlier in this session;
- denies it with `denied (unattended)` if nobody attends the turn: a job run without `ask_to`, a job
  session's turn, or one without a requester or conversation;
- otherwise registers a pending approval, shows the prompt, and waits for the answer.

The wait sits before the call's deadline (`tokio::time::timeout`), so it doesn't use the deadline
up; once approved, the call-time checks run again (the tool may have gone meanwhile) and the call
starts. A barrier call waiting for approval holds up the calls after it, as it would while running.
A denial is the call's error result:

| How it ended                            | Result                                                   |
| --------------------------------------- | -------------------------------------------------------- |
| `yes`, `yes!`                           | The call runs                                            |
| `no`                                    | `denied`                                                 |
| No answer within `approval_timeout`     | `denied (the approval request expired)`                  |
| A new message from the requester        | `Not approved. The user said: <text>` (first 300 characters) |
| A job turn                              | `denied (unattended)`                                    |
| The prompt couldn't be sent             | `denied (the approval request could not be sent: <error>)` |
| `/stop`, shutdown                       | The wait is dropped; the slot gets `cancelled`           |

A restart forgets every pending approval: nothing is stored, and startup recovery fills the call's
slot with `interrupted`.

Jobs (§6.11): a job run (`Turn::job`) is answered only by the target the scheduler puts in its
`JobTurn::approvals`, in a job session or a DM alike; without one every `ask` is
`denied (unattended)`. With `ask_to = "deliver_to"` the target is the delivery user in their DM
(session key and `Reply` of that conversation), with a `PromptOutbox`: the prompt is queued through
it for the user's default identity instead of being sent into a conversation, expiring after
`approval_timeout`, and when the approval ends (answered, expired, denied, cancelled) the board
calls `withdraw` instead of telling the presenter, so a prompt still waiting in the outbox is
removed. Replies come through the normal path, from the DM. v1 has no roles, so the delivery user
may approve any tool.

## Prompts

`call_prompt(tool, input_json, code)`:

```text
Allow `shell__exec` (`{"command":"rm -rf ./build"}`)? Reply `yes 4821` or `no 4821`.
```

`question_prompt(tool, message, code)`, for elicitation:

```text
The tool `ci__deploy` asks: `Deploy to production?` Reply `yes 4821` or `no 4821`.
```

The tool name, the input (compact JSON), and the question come from the model or a toolset, so
nothing in them may change what the prompt says (`shown`):

- **Escaped** (`escape`): `\n`, `\r`, and `\t` as those two characters; every other control
  character (C0, DEL, C1), U+2028 and U+2029, the bidirectional formatting characters (U+061C,
  U+200E, U+200F, U+202A–U+202E, U+2066–U+2069), and the invisible ones (soft hyphen U+00AD,
  U+200B–U+200D, U+2060–U+2064, U+FEFF, tag characters U+E0000–U+E007F) as `\uXXXX` (`\u{XXXXX}`
  beyond U+FFFF). The text is therefore one line that displays in its own order, and nothing in it
  is invisible. Other Unicode is shown as itself.
- **Cut**: at most 200 characters of the escaped text (an escape or character is never split). A cut
  text is followed by its full size and the first 12 hex digits of its SHA-256:
  `` `…` … (10,000 bytes, SHA-256 0123456789ab)``, so two long inputs with the same start can be
  told apart.
- **One code span** (`code_span`): delimited by one more backtick than the text's longest backtick
  run, so no run inside can close it, and padded with a space on each side when the text starts or
  ends with a backtick or a space (CommonMark strips exactly one such space, so the text shows
  unchanged). An empty text shows as `(empty)`. Inside a code span nothing is Markdown, HTML, or a
  link, on any channel's renderer.

The prompt goes out through the board's `ApprovalPresenter` (below, Web Buttons); the whole wait,
the send included, is bounded by `approval_timeout`.

## Codes

Each pending approval gets a short numeric code, random (`getrandom`), four digits, unique among the
pending approvals and the closed ones still remembered (six digits once half the four-digit codes
are taken). A closed approval (answered, expired, cancelled, or denied by a message) keeps its code
for 24 hours (`CLOSED_RETENTION`), so a late reply is told what became of it and the code isn't
handed out again meanwhile.

## Replies

The router asks `is_pending(to, code)` for every `yes`, `yes!`, or `no` message (optionally with a
code). It answers true when an approval is pending for `to` (same session key and user), with or
without a code, and when `code` names one of theirs that closed recently. So `yes` and `no` stay
ordinary messages while nothing waits, and every reply while something waits is consumed by the
core, never queued for the agent. `answer(to, reply)` then:

| Reply                                           | Effect                                           |
| ----------------------------------------------- | ------------------------------------------------ |
| With the code of one of `to`'s pending approvals | Decides it                                       |
| Without a code, exactly one pending             | Decides that one                                 |
| Without a code, several pending                 | Nothing decided; told "More than one approval request is waiting for you (`1093`, `4821`). Reply with its code, e.g. `yes 1093`." |
| With a code that closed recently                | Nothing decided; told it expired, was cancelled, was already denied by your message, or was already answered |
| With an unknown code while some are pending     | Nothing decided; told "No approval request `0000` is waiting for you (waiting: ...)." |

`yes!` allows the call, and the tool for the rest of the session: later `approve_call`s for it in
that session (in any turn) run without asking, and the session's other calls of that tool still
waiting are allowed at once too. Replies from another conversation or another user never match.

## New Messages

A message from the requester while one of the turn's approvals waits denies it (§6.4) and becomes
part of the turn. The session code wakes `Turn::steering` for each requester message. In its
tool-batch loop the agent loop only looks at the queued messages (`Steering::peek`; the rows stay
`queued`). It remembers the newest row it has seen, starting with what was queued when the batch
began; a wake-up that finds newer rows while `TurnApprovals::has_pending` says an approval of this
turn waits passes their text to `deny_pending(said)`, which denies every approval of this turn with
`not_approved(said)` (a question is declined). A message that was already queued before, or that
arrived while nothing waited, denies nothing. At the batch boundary the queued requester messages
are taken into the turn (`Steering::take`, every one, in order) and stored after the tool results,
so the model sees both; with `busy_mode = "queue"` this happens only when a message denied an
approval. A turn that `/stop` ends first takes nothing, so the message starts the next turn. In
`steer` mode the batch's unstarted calls are also skipped ([Agent Loop](agent-loop.md), Turn
Control).

## Questions (Elicitation)

A running call's `ToolCallEvent::ApprovalRequest { id, message, schema }` goes to
`answer_question(tool, message)` through the same board: same codes, prompts, replies, expiry, and
denial by a new message. `yes` (and `yes!`, which allows nothing further for a question) answers
`ApprovalDecision::Approve { content: None }` on the handle's `answers`; anything else `Deny`. v1
answers only yes or no, without content: an MCP server gets `accept` with an empty object for `yes`
and `decline` for anything else, and a question whose schema requires a field never reaches the
board, the MCP client declines it at once ([MCP Client](mcp.md#mapping-mcp-to-the-toolset-trait)).
The wait is part of the running call, so its deadline keeps running; an unattended turn declines at
once.

## Web Buttons

`ChannelPresenter` shows a prompt for a conversation on the web channel's instance
(`registry.web_channel()` whose `instance()` is `reply.instance()`) with
`WebChannel::send_approval(chat, code, markdown)`: the prompt with Allow / Allow for this session /
Deny buttons ([Web Channel](web-channel.md)). Every outcome (an answer, a typed reply, expiry, a new
message, `/stop`) retires it with `WebChannel::resolve_approval(chat, code, outcome)`, so the page
shows how it ended. A button posts `yes <code>`, `yes! <code>`, or `no <code>` as the user, which
reaches the board like a typed reply. On every other channel the prompt is the text, sent with
`Reply::send` (`TextPresenter`), and the outcome isn't shown. `ApprovalOutcome` is defined here and
re-exported by the web channel. `resolved` must not block.

## Tests

`approvals/prompt.rs` has the rendering table (backticks, padding, line breaks, control and
bidirectional characters, long input). `approvals/tests.rs` tests the board and `TurnApprovals` on
paused time with a fake channel and a recording presenter, and `ChannelPresenter` with a real web
channel. `crates/giles-core/tests/approvals.rs` runs the whole path through
`Runtime::start_sessions` and the agent loop: `yes`, `no`, `yes!` (and for the other waiting calls),
a bare `yes` with two waiting, expiry and a late reply, denial by a new message in both busy modes,
a question answered both ways, `/stop` while waiting, and the deadline not counting the wait.
