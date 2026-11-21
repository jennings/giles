# Scheduler

`doozers_core::scheduler` runs jobs ([Plan](../PLAN.md) §6.4, §6.11): config jobs (`[[jobs]]` in
`agent.toml`) and runtime jobs (rows in the `jobs` table, [Storage](storage.md)).

| Item                       | What it is                                                           |
| -------------------------- | -------------------------------------------------------------------- |
| `Scheduler`                | `start(SchedulerOptions, shutdown)`, `closed()`                       |
| `SchedulerOptions`         | Store, `Registry`, `Sessions`, `TurnRunner`, `JobDelivery`, `JobChanges`, `Clock` |
| `JobDelivery`              | Where output goes (the outbox): `enqueue(Delivery) -> OutboxId`, `remove(id)` |
| `Delivery`, `DeliveryKind` | A send into the job's conversation, or else to the user's default identity: `JobReply` or `ApprovalPrompt`, optional expiry |
| `JobChanges`               | `notify()` after creating, changing, or deleting runtime jobs; `Runtime::jobs` holds one |
| `JobDef`                   | A config or runtime job in one shape; `label()` is `` `id` `` or `#3` |
| `RuntimeSpec`              | A runtime job's stored `spec`: `cron` or `at`, `prompt`, `session`, `conversation`, `disabled`, `changed_at` |
| `JobConversation`          | The conversation a runtime job was scheduled in: `channel` (instance) and `id` |
| `When`                     | `Cron` or one-shot `At`; `next_after`, `first_fire`                   |
| `job_reply`, `JobReply`    | What a run said: `NoReply`, `Empty`, or the text                      |

## Wiring

`doozers serve` calls `Runtime::start_scheduler(sessions, runner, shutdown)` after the outbox and
the session manager have started (approval prompts may only be queued once the outbox's sender has
dropped the previous process's), with the same runner (the agent loop). Its `JobDelivery` is
`OutboxDelivery`: an `OutboxItem` into the delivery's conversation (`Destination::Conversation`), or
else `OutboxItem::to_user` for the delivery's user, with `JobReply` or `ApprovalPrompt` and the
delivery's expiry; `remove` withdraws a prompt. At shutdown `serve` waits for `Scheduler::closed()`
before `Sessions::closed()`.

## Timing

Each job has a task. Its time zone is its user's (a runtime job's creator; a config job's is the
owner), else the agent's, else the system's. At start, `first_fire(since, now)` with `since` the
later of the job's `job_runs` time and `JobDef::since` (a runtime job's creation time, or its spec's
`changed_at`, the last edit or switch back on): if a time fell between `since` and `now` (the core
was down), the missed times are coalesced into one run now; otherwise the next match after `now`. So
an edited or re-enabled job never makes up the times before its change. The task sleeps in steps of
at most `MAX_SLEEP` (60 s), reading the clock again, so a jumping wall clock delays a job by at most
that. After each run the next time is the first match after the clock, so times that pass during a
long run are skipped. A time outside `active_hours` is skipped (not recorded). A one-shot runtime
job is deleted after its time, run or not, unless its creator isn't an active user (below).

A run is counted from the moment its time comes, per agent (`Scheduler::agent_busy`, which keeps an
agent from being removed) and per creator (`Scheduler::user_busy`, which keeps a user from being
forgotten, an isolated run's turn included). Once it is counted, a run of a runtime job whose
creator is no longer an active user is skipped, so a user who is disabled for removal starts no new
run, and one that began before is seen. A recurring job misses that time and runs at its next one,
so it resumes when the user is enabled again. If Doozers restarts before that next time, the job's
last run is older than the times it missed, so the start-up rule above runs it once for them. A
one-shot job's time doesn't come again, and deleting it would make disabling a user destroy their
reminders, so its task cancels its own token and ends, and the job stays in the store. The next read
of the runtime jobs starts a task for it, which runs it at once if its creator is active by then, as
a job that was due while Doozers was down. Every change to the users starts that read
(`UserList::subscribe`, which the scheduler watches next to `JobChanges`), so enabling a user, by
any path, starts their waiting jobs, and a change that leaves the user disabled starts the task,
which skips and ends again.

Config jobs follow the running configuration ([Registry](registry.md#the-running-configuration)). At
start, and whenever `LiveConfig` changes, `reconcile_config_jobs` compares each agent's `[[jobs]]`
with the ones its tasks were started for: an agent that is gone, or whose jobs differ, has its tasks
stopped (one token per agent, so they stop together), and one with jobs and no tasks gets them
started. An edit that leaves `[[jobs]]` alone keeps the tasks and their place in the schedule. Each
run reads the agent's configuration again when its time comes (the time zone, the model and tools,
`ask`), and a task whose agent has been removed stops, so a removed agent's job can't fire, and one
created later under the same id runs only the jobs its own `agent.toml` has. A task that started
from `first_fire` follows the rule above, so a restarted one can run once for times it missed.

Runtime jobs: a task reads `jobs` at start and on every `JobChanges::notify` and change of the
users, starting tasks for new rows and stopping the tasks of deleted ones. It keeps the spec each
task started from: a row whose spec changed (an edit) has its task stopped and a new one started
from the new spec, and a spec with `disabled = true` gets no task, so a disabled job keeps its row
and never runs. Stopping a job's task (deleting, editing, or disabling the job, or shutdown) ends
its run's waits before the turn (for an idle DM, for the DM's hold); a turn already started runs to
its end and its reply is delivered, so a job that cancels itself in its last run still reports. Only
`/stop` in the DM (a `dm` run) and shutdown cut a started turn off. A spec it can't parse is logged
and skipped.

## One run

1. **The DM.** The job's user is a runtime job's creator, else the owner. A runtime job with a
   `conversation` whose channel instance is still configured has that conversation as its DM (its
   open session, if any, for the excerpt and `idle_only`), and its reply goes there. If another user
   holds that conversation (an open session of someone else's: an IRC nickname's conversation id can
   change hands), it isn't the job's DM: the job is treated as if the conversation were gone, with a
   warning in the log, so its transcript, workspace, and SSH keys are never the job's, and nothing
   is queued into it. Any other job (a config job, a runtime job made on the web jobs page, or one
   whose instance is gone or whose conversation is held by another user, which is logged) uses the
   user's DM on their default identity: the direct session of that user with the agent, open or
   ended, with the newest activity on a channel instance that can own the identity, one whose
   registered channel's namespace and `:` start the identity, or, not registered yet, whose
   component is its first segment (`Store::latest_direct_session`). Its reply goes to the default
   identity.
2. **`idle_only`**: waits until the DM has no running turn, hold, or queued message
   (`Sessions::wait_idle`). Then `active_hours` is checked again: a run whose wait ended outside
   them is skipped and not recorded.
3. Records the run's start in `job_runs`, then reads the prompt (`prompt_file` at each run).
4. **Approvals.** Without `ask_to`, `ask` tools are denied as unattended. With
   `ask_to = "deliver_to"` and a DM, the target is the user in that DM, with prompts queued through
   `JobDelivery` as `ApprovalPrompt` to where its reply would go (prefixed `Scheduled job <label>:`,
   expiring after `approval_timeout`) and removed when the approval resolves first; replies come
   from the DM ([Approvals](approvals.md)). Without a DM there is nobody to answer, so they're
   denied.
5. **The turn**, a `Turn` with `job: Some(JobTurn)` ([Agent loop](agent-loop.md)). Its prompt is a
   user message: the time prefix,
   `Scheduled job <label>. Your reply is sent to the user; reply exactly NO_REPLY to send nothing.`,
   for `dm_excerpt = N` the text of the DM's last N messages (`**User:**`/`**You:**`, tool blocks
   left out), then the prompt.
   - `isolated`: a fresh job session owned by the user (their core memories are in its prompt),
     ended after the run. Its [environment workspace](environments.md#job-runs), if the run made
     one, is deleted when the run ends, before the session ends; a `dm` run's workspace is the DM's
     and stays. A config job's turn has no requester, so its memory tools see and write shared
     memories only ([Skills and Memory](skills-and-memory.md#who-sees-what)); a runtime job runs as
     its creator, with the creator's memories.
   - `dm`: the DM's open session (`Sessions::open_direct`, `idle_reset` applied), held
     (`Sessions::hold`) so the run is its own turn after the turns ahead of it; `/stop` there
     cancels it. With no DM yet it runs like an isolated job. If another user takes the conversation
     between the check in step 1 and the open, `open_direct` refuses (`Error::Conflict`): the run is
     skipped, with an `info` line in the log, and never runs in the holder's session with their
     workspace and SSH keys. An isolated run's reply that was delivered isn't appended to that
     holder's transcript either (also logged at `info`).
6. **The reply** is read back from the transcript (`job_reply`): `NoReply` when the last assistant
   message's text is exactly `NO_REPLY`, else the text of every assistant message of the run.
   - `NO_REPLY`: nothing is sent; an isolated run's session is deleted, a `dm` run's messages are
     cut from the DM (`truncate_messages`).
   - Text: queued as a `JobReply` (with the job's conversation, if the DM is one), and for an
     isolated run appended to the DM as an assistant message (under a hold, between turns); a `dm`
     run's reply is in the DM already. A web chat shows the queued reply on its open pages, and its
     transcript shows the stored one.
   - A cancelled or failed turn delivers nothing (logged).

## The `schedule` tool

`core_tools::ScheduleTool` (registered by `core_tools::register`, not parallel-safe) is how the
agent manages runtime jobs for the turn's requester. Input (strict: `deny_unknown_fields`, each
action refuses the keys of the others): `action` = `create` | `list` | `cancel`.

- `create`: `prompt` and exactly one of `cron` (five fields), `at` (RFC 3339 with an offset, or
  `YYYY-MM-DDTHH:MM` in the user's time zone) or `in` (a delay, `parse_duration`), plus optional
  `session` (`isolated` default, `dm`). `in` becomes `at`; a past time or a cron pattern with no
  future match is refused. It stores a `RuntimeSpec` with the requester as creator and the call's
  conversation (`CallContext::conversation`) as `conversation`, calls `JobChanges::notify()`, and
  answers e.g. "Scheduled job #2: every `0 9 * * 1-5`, next Fri 2026-10-23 09:00 BST."
- `list`: the requester's jobs with this agent (number, schedule and next run, session, prompt cut
  to 100 characters; a disabled job is marked `disabled:` and has no next run), then the agent's
  config jobs as read-only.
- `cancel`: `id`; deletes the job only if it's this agent's and the requester created it, then
  notifies the scheduler, whose task for it stops: a run waiting for the DM is dropped, but a run
  whose turn has started (the job cancelling itself) finishes and delivers its reply.

Limits: a turn without a requester (a config job's run) can only list. A job run (the `TurnContext`
has `job` set) can list and cancel but not create, so no job can spawn more jobs. A user has at most
`MAX_RUNTIME_JOBS_PER_USER` (20) runtime jobs with one agent; `create` beyond that answers "too many
scheduled jobs (at most 20); cancel one first". Times are shown in the same zone messages use (the
requester's, else the agent's, else the system's).

## Tests

`scheduler/schedule.rs`: time zones and DST, firing strictly after, one-shots, coalescing, active
hours. `scheduler/run.rs`: replies and `NO_REPLY`, excerpts, identity ownership. `scheduler/job.rs`:
runtime specs. `scheduler/tests.rs`, on paused time with a real store and session manager, a fake
runner and delivery: firing in the owner's zone, active hours, coalescing at startup, `NO_REPLY` in
both session kinds, delivery with DM append and excerpt, model override and prompt files read per
run, `dm` jobs queued behind a running turn and falling back without a DM, `idle_only`, `ask_to`
targets and prompt withdrawal, runtime jobs (creator as requester, cancel, one-shot deletion, a
missed one-shot at startup, an edit moving a job, a disabled job not running and making up nothing
when switched back on, the jobs of a disabled creator waiting (a one-shot kept, a recurring one
skipping times, an unrelated change starting nothing) and running again when the users change back,
`isolated` and `dm` runs in the conversation they were scheduled in, and the default identity when
that conversation's instance is gone).
