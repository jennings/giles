# Sessions and Turns

`giles_core::sessions` takes every channel event from the stream to a turn ([Plan](../PLAN.md) §4.5,
§6.2, §6.3, §8.5): classify, store, ack, then act. It owns the per-session turn queue, `/new`,
`/stop`, `idle_reset`, startup recovery, dedupe-key pruning, and the latest status of each channel.
The agent loop plugs in as a `TurnRunner`; the decision before anything is stored is
[Identity and Routing](identity-and-routing.md); the tables are in [Storage](storage.md).

| Item                     | What it is                                                              |
| ------------------------ | ----------------------------------------------------------------------- |
| `Sessions`               | The manager: `start`, `attach_channel`, `stop`, `is_running`, `hold`, `wait_idle`, `open_direct`, `channel_status(es)`, `closed` |
| `SessionHold`            | Exclusive use of a session between its turns: `session`, `cancel`; released on drop |
| `SessionsOptions`        | Store, `Router`, `AgentPolicies`, `ChannelLookup`, `TurnRunner`, `Approvals`, `Clock` |
| `AgentPolicies`          | An agent's `AgentPolicy` (`busy_mode`, `idle_reset`), looked up when needed; implemented by `LiveConfig` (agents created at runtime included) and by a `HashMap` for tests |
| `TurnRunner`             | `async fn run_turn(&self, turn: Turn) -> TurnOutcome`; the agent loop implements it |
| `Turn`                   | `session`, `requester`, `messages: Vec<TurnMessage>`, `busy_mode`, `cancel`, `steering`, `reply`, `job` |
| `JobTurn`                | A job run's `prompt` message, `model`, and approval target (the scheduler builds it) |
| `Steering`               | The requester's messages during a turn: `changed`, `take` (`detached` for tests) |
| `Reply`                  | The session's conversation: `instance`, `conversation`, `channel`, `send` |
| `Approvals`              | `is_pending` (for `classify`) and `answer`; `approvals::ApprovalBoard` in the daemon, `NoPendingApprovals` for tests |
| `ChannelLookup`          | Instance name to the registered channel; implemented by `Registry`      |
| `ChannelStatusRecord`    | A channel's latest `ChannelStatus` and when it arrived                  |

## Wiring

`giles serve` calls `Runtime::start_sessions(runner, approvals, shutdown)` after `Runtime::start`
and binding the HTTP listener: it's the last startup step that can fail, since recovery changes
state and resumed turns start running. It builds the `Router` from `[owner]` and the routes
`Runtime::start` loaded (`Runtime::routes`), passes the registry's `LiveConfig` as the
`AgentPolicies` (so an agent created in the web UI gets its policy without a restart; a session of
an agent no longer configured gets the defaults), runs `Sessions::start` (recovery, then resume,
then the prune task), and attaches every channel registered so far. The plugin host then registers
each plugin channel as it comes up and attaches it with
`Sessions::attach_channel(instance, component, channel)` ([Plugin Host](plugin-host.md)); attaching
an instance again (a restarted plugin) stops the earlier pump after its current event.

After the HTTP server has drained at shutdown, `serve` awaits `Sessions::closed()`, which waits for
every task the manager spawned (pumps, drivers, turns, pruning) to stop.

`serve` passes `AgentLoop::from_runtime(&runtime, board)` as the runner and the same
`approvals::ApprovalBoard` as `approvals` ([Agent Loop](agent-loop.md), [Approvals](approvals.md)).

## The Inbound Pipeline

One pump task per attached channel consumes `Channel::events()` and handles events one at a time:

| Event                                     | Stored                                  | Then                         |
| ----------------------------------------- | --------------------------------------- | ---------------------------- |
| `Status`                                  | No; latest per instance kept in memory  | Ack                          |
| Message, `Inbound::Drop`                  | No (§1.1: groups store nothing)         | Ack                          |
| Message, `Inbound::Message(to)`           | `queued` row in the session's queue     | Ack, then wake the session   |
| Message, `Inbound::Command { to, .. }`    | `done` row, only as a dedupe key        | Ack, then act on the command |

- **Store, then ack.** `inbox_insert` deduplicates on (instance, event id) in the same statement;
  the ack goes out only after it returns. A duplicate (`None`) is acked and otherwise ignored, so a
  redelivered message never queues twice and a redelivered `/new` or `yes` never acts twice.
- **A database error** leaves the event unacked and drops the stream; the pump reopens it after the
  backoff and the channel resends the event (§8.5). A failed ack is only logged: the redelivery it
  causes is deduplicated.
- **Reopening.** When `events()` fails, or the stream fails or ends, the pump reopens it after 1 s,
  doubling to at most 60 s; a stream that delivered an event resets the delay. Resending unacked
  events is the channel's job.
- **Shutdown** stops a pump between events, never inside one.
- **Payload.** The row holds the whole `ChannelEvent` as JSON; the turn reads its message back from
  it.

Commands act after their ack, so a crash between the two loses the command but never repeats it:

- `/new` ends the key's open session (`end_session`); the next message opens a fresh one. A turn
  already running, and messages already queued, finish in the old session.
- `/stop` cancels `Turn::cancel` of the turn running for the key, in whichever session it runs. With
  nothing running it's logged and ignored. Making the cancellation reach toolsets, and filling
  unanswered tool calls with `cancelled`, is the agent loop's job.
- An approval reply goes to `Approvals::answer`. `classify` only returns one while `is_pending` says
  it's an approval reply, so with nothing waiting `yes` and `no` stay ordinary messages.

## Sessions and idle reset

A message's session is the open session for its key (agent, channel instance, conversation), created
on demand as a `direct` session owned by the sender. Before that, if the agent sets `idle_reset`,
the open session is ended when it has been quiet for at least that long:
`now - (last_message_at or started_at) >= idle_reset`. A session with a running turn or queued
messages is never reset, however long the turn takes. `last_message_at` comes from the `messages`
table, which the agent loop writes.

## Turns

At most one turn runs per session. Waking a session that isn't live spawns a driver task that loops:

1. Read the session's `queued` rows. If there are none, retire, unless a wake arrived since the read
   (the `dirty` flag), in which case read again. No wake is lost.
2. Pick the batch: the sender of the oldest queued row is the requester, and the batch is every
   queued row from that sender, in order. Others' rows never join it; they start a later turn
   (§6.3).
3. `inbox_start` the batch, build the `Turn`, and await `TurnRunner::run_turn` in its own task (a
   panicking runner is logged like a failed turn).
4. `inbox_finish` every row the turn owns, whatever the `TurnOutcome`, then loop.

Messages that arrive during a turn are `queued` like any other, so a restart loses nothing. In both
busy modes a message from the requester wakes `Steering::changed`, and `Steering::take` marks the
requester's queued rows `started`, adds them to the turn (they're finished with it), and returns
their messages. The agent loop decides when to take, from `Turn::busy_mode`:

- `steer`: at every tool-batch boundary.
- `queue`: never, except while an approval is pending: then a requester message denies it and the
  model sees it (§6.4), which holds in `steer` mode too.

A message the turn doesn't take stays queued and starts the next turn. A message that arrives after
the driver read the queue but before the turn is installed wakes `changed` when it is; a wake-up
only says `take` may have something. After a turn ends, `take` on its `Steering` returns nothing:
the driver closes it under the same lock `take` holds, so no row is started after the turn's rows
are finished.

`Turn::reply` sends into the session's conversation through whatever channel is registered for the
instance at that moment; `Reply::channel()` exposes its `info().output` for streaming decisions.

At shutdown every turn's `cancel` fires (it's a child of the shutdown token). A turn that still
returns `Completed` has its rows finished; otherwise the driver stops without finishing them, so
they're still `started` at the next start and recovery tells the requester. `Sessions::closed()`
returns once every runner has returned.

## Holds

The scheduler runs a `dm` job as its own turn in the user's DM session, and appends a job's reply to
the DM between turns (§6.11). It does so through a hold:

- `Sessions::hold(session)` queues the caller in the session's driver (starting one if needed) with
  the time it asked. Before each turn the driver compares the oldest holder with the oldest queued
  row: rows received up to the moment the holder asked go first, and a turn's batch never takes rows
  received after a waiting holder asked; then the holder gets a `SessionHold`. Holders never merge
  with messages.
- While a hold is held, no turn starts in the session, `is_running` is true, and `/stop` in its
  conversation and shutdown cancel `SessionHold::cancel` (the holder must stop its work when it
  fires). Messages arriving meanwhile are queued as usual and never steer into the holder's work.
  Dropping the hold gives the session back to the driver.
- `hold` returns `None` at shutdown or if the session can't be loaded; a caller that stops waiting
  just drops the future, and the driver skips it.
- `wait_idle(session)` waits until the session has no driver: no turn, no hold, nothing queued (a
  job's `idle_only`), or until shutdown.
- `open_direct(key, user)` opens a DM session the way an inbound message from `user` would,
  `idle_reset` included.

## Startup Recovery

`Sessions::start` runs before any channel is attached:

1. `fill_all_empty_slots("interrupted")`: every `ToolUse` without a result gets one.
2. `inbox_interrupt()`: rows still `started` become `unknown`. Nothing is replayed, so a tool with
   side effects never runs twice.
3. For each session with such rows, once, send "Interrupted by a restart; resend if needed." into
   its conversation (each send waits at most 10 s). Plugin channels aren't registered this early, so
   a notice for an instance with no channel, or one whose send fails, is kept in memory by instance
   and sent when that instance is attached (`attach_channel`); one refused again waits for the next
   attach.
4. `inbox_sessions_with_queued()`: wake each session that still has queued rows, oldest first.

## Pruning

`done` and `unknown` rows are dedupe keys only. A task deletes those received more than 30 days ago
(`DEDUPE_RETENTION`) at startup and then hourly (`PRUNE_INTERVAL`). `queued` and `started` rows are
never pruned.

## Channel Status

The latest `ChannelEvent::Status` of each instance is kept in memory with the time it arrived, for
`giles status` and the admin pages: `Sessions::channel_status(instance)` and
`Sessions::channel_statuses()`. Nothing is stored; a restarted channel reports its state again.

## Time and Tests

Every timestamp comes from the `Clock` in `SessionsOptions` (`SystemClock` in the daemon). The tests
in `sessions/tests.rs` run on paused Tokio time with a clock derived from it, a fake channel that
snapshots the inbox at each ack, and a fake runner whose turns the test ends. Store calls run on the
blocking pool, which holds paused time, so waiting a millisecond means every task has run as far as
it can.
