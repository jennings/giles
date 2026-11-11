# Environment workspaces

An environment plugin ([Plugin host](plugin-host.md#environment-plugins), PLAN section 8.8) holds
workspaces: places where an agent's `bash`, `read`, `write`, `edit`, `find`, and `grep` tools run.
The plugin makes and destroys them when the core asks; everything about who owns a workspace, when
it is used, and when it is stopped or deleted is the core's. `giles_core::workspaces::Workspaces` is
that part. The runtime holds it (`Runtime::workspaces`), the core tools call it, the scheduler calls
it when an isolated job run ends, and its sweep runs beside the blob sweep. The `workspaces` table
is described in [Storage](storage.md#workspaces).

## Owners and ids

A workspace has one owner, and an owner has at most one workspace:

- A **conversation**: the key of a session, (agent, channel instance, conversation id). The agent is
  part of the key, so changing the route of a chat can't hand one agent's files to another. `/new`
  and an idle reset start a new session with the same key, so the workspace carries over.
- An **isolated job run**: its job session. A `dm` job runs in the DM's session, so it uses the DM's
  workspace and doesn't delete it.

`WorkspaceOwner::of_session(&Session)` derives the owner from a turn's session: a keyed session is a
conversation, a job session is a job. The core assigns the workspace id (`ws-` and 20 random hex
digits) and the plugin labels its resources with it, so reconciliation needs no handle but the id.

## States

| State      | Meaning                                                                          | Holds the owner's slot |
| ---------- | -------------------------------------------------------------------------------- | ---------------------- |
| `creating` | The row exists and the plugin's `CreateWorkspace` is in flight                   | yes                    |
| `active`   | Created; `description`, `working_directory`, and `persistence` came from the plugin | yes                 |
| `deleting` | Deleted as far as the owner can tell, but the plugin hasn't confirmed            | no                     |
| `deleted`  | The plugin confirmed, or it was found gone. Kept to tell the model, then pruned  | no                     |

`deleted_at` and `delete_reason` are set on `deleting` and `deleted` rows. The reasons
(`DeleteReason`): `user_reset`, `model_reset`, `model_delete`, `inactive` (the sweep),
`job_run_ended`, `removed_outside_giles` (reconciliation, or a call that found the workspace
missing), `chat_archived` (reserved for the chat-archive hook), and `creation_failed`. A failed
creation isn't reported to the model: it saw the error.

## Calls

- `create(owner, environment, options)`: the environment must be configured and connected, and the
  owner must have no workspace (`WorkspaceError::AlreadyHasWorkspace` names it). It inserts a
  `creating` row, calls the plugin, and makes the row `active`. The plugin call runs in its own
  task, so a turn that is cancelled halfway doesn't leave a row nobody finishes. If the plugin
  fails, the row becomes `deleting` (`creation_failed`) and the plugin is asked to delete what it
  may have made, so the owner can try again at once; the plugin's `InvalidRequest` (refused options)
  reaches the model as `WorkspaceError::Refused` with its own message.
- `delete(owner, reason)`: frees the owner's slot immediately (`deleting`), then asks the plugin and
  marks the row `deleted`. If the plugin is unreachable or fails, the row stays `deleting` and the
  sweep or the next connection finishes it. Leases don't stop it: the owner's own turn holds one.
- `current(owner)` and `last_deleted(owner)`: what the owner has, and what it had, with when, why,
  environment, and options, so the model can say what happened and create it again.
- `open(owner, slot)`: takes the turn's lease, calls `StartWorkspace`, notes the use
  (`last_used_at`), and returns the record, the environment's client, and a notice for the model
  when the workspace was started or recreated. The core tools call it before each workspace tool
  call. If the plugin says the workspace doesn't exist, the row becomes `deleted`
  (`removed_outside_giles`) and the call fails with `WorkspaceError::Gone`.
- `recover()`: called once at startup. A `creating` row means the previous process died while
  creating, and an `active` job row means its run died with it; both become `deleting`.

## Leases

A turn holds the lease of its workspace from the first `open` until the turn ends. The turn carries
a `LeaseSlot` (`TurnContext::lease`, built per turn and dropped with it); `open` fills it. A lease
is the shared side of a per-workspace `tokio::sync::RwLock`. The sweep takes the exclusive side with
`try_write` before it stops or deletes a workspace, so it skips a leased one without waiting, and an
`open` that arrives while the sweep holds the workspace waits for it. A turn that already holds a
lease on the workspace keeps it and doesn't take the lock again (a waiting writer would block the
second read). When a turn deletes its workspace and creates another, the next `open` replaces the
old lease with the new one. Leases are in memory and end with the process.

## The sweep

`Workspaces::start_sweep` (`Runtime::start_workspace_sweep`) starts two tasks, stopped by the
shutdown token (`WorkspaceSweep::closed`). `serve` stops it before the plugins, because it calls
them, and drops a sweep that is running: every step is idempotent and the next run finishes it.

The periodic task waits 60 seconds (`SWEEP_FIRST_DELAY`), then runs `sweep_once` every five minutes
(`SWEEP_INTERVAL`, measured from the end of one run to the start of the next). For each configured
environment instance it:

1. prunes `deleted` rows deleted more than `delete_after` ago, whether or not the plugin is
   connected;
2. if the plugin is connected, reconciles (below), then looks at every `active` workspace that no
   turn has leased. Its **owner activity** is the later of the workspace's `last_used_at` and, for a
   conversation, the newest message across every session of its key, ended ones included. If the
   owner has been inactive for longer than `delete_after`, the row becomes `deleting` (`inactive`)
   and the plugin deletes the workspace. Otherwise, if `last_used_at` is more than `stop_after` ago
   and the plugin lists the workspace as running, the plugin stops it. A stopped workspace stays
   `active` and `open` starts it again.

While an instance's plugin is away, the sweep only prunes its rows: nothing is stopped or deleted
(the plugin couldn't be told), and the delete of a `deleting` row waits for the connection. An
instance that is no longer in `giles.toml` isn't visited at all, so its workspaces stay as they are
until it is configured again.

A workspace is therefore stopped or deleted at most one interval after its threshold. A job's
workspace has only its own use to go by, which matters only for one that outlived its run. A step
that fails is logged and the sweep goes on; the next run tries again. Each run logs one `info` line
with a `SweepReport` when it changed something, and a `debug` line otherwise.

The sweep and the reconciliations run one at a time (a mutex), so two never act on one workspace.
Unlike the blob sweep, it doesn't need to be held off during a backup ([Backups](backups.md)): a
backup holds the `workspaces` table but not what the plugins hold, so nothing the sweep deletes is a
file a backup is about to archive. After a restore, reconciliation brings the rows and the plugins
back in step.

## Reconciliation

`reconcile` runs when an environment plugin connects (a second task watches
`Registry::environment_changes()` and runs it for each instance whose `connections` count moved, and
once for every instance already connected when the task starts), and at the start of each sweep for
a connected instance. It reads the instance's rows first, then calls `ListWorkspaces`: the order
matters, because a row that was `active` before the plugin was asked, and that the plugin doesn't
list, is gone, while asking first could mistake a workspace created in between for a lost one. Then:

- A workspace the plugin has that no row names, in any state, and that is older than an hour
  (`RECONCILE_GRACE`), is deleted. A create in flight has a row; the grace period covers a create
  whose row isn't visible yet, and anything younger is left until it is old enough.
- A `deleting` row that no turn has leased is deleted in the plugin (which succeeds if there is
  nothing to delete) and becomes `deleted`.
- An `active` row the plugin doesn't list becomes `deleted` (`removed_outside_giles`), so the next
  time the model asks it is told, and its owner can create another. This also covers a restored
  backup and a plugin that lost its state.
- A `creating` row older than the grace period becomes `deleting` (`creation_failed`) before the
  rest runs, so it is cleaned up in the same pass.

A failing `ListWorkspaces` ends the reconciliation of that instance with a warning; nothing is
changed on a guess.

## Job runs

When an isolated job run ends, however it ended, `scheduler::run` deletes the run's workspace
(`job_run_ended`) before it ends the job session, even if the run replied `NO_REPLY` and its session
is about to be deleted. A plugin that can't be reached leaves the row `deleting` for the sweep. A
run killed with the process is settled by `recover` at the next start.

## Renames

`workspaces.environment` is the environment instance and `workspaces.channel` the channel of a
conversation's owner, so `giles environment rename` and `giles channel rename` move them with the
other rows ([Instance rename](instance-rename.md)); a renamed instance keeps its workspaces, in
every state, so a pending cleanup still finds its plugin. There is no agent rename to cover: the
agent in an owner is the agent's id.

## Workspace tools

The core tools are in `core_tools/environment/`: `lifecycle.rs` (`environment_plugins`,
`environment_create`, `environment_status`, `environment_reset`, `environment_delete`), `bash.rs`,
`files.rs` (`read`, `write`, `edit`), and `search.rs` (`find`, `grep`), on a shared `Env` (the
`Workspaces` service and the registry). The plugin provides primitives, and the tools shape what the
model sees, so the limits and messages are the same for every plugin.
[Environments](../user/environments.md) is the user's view.

### Offering and checking

`SessionTools::for_agent` takes the registry's tool snapshot, keeps the tools that match the agent's
`tools` globs, and drops the `ENVIRONMENT_TOOLS` unless the agent's `environments` is not empty.
Tool specs never change during a session. What changes is read at call time: `environment_plugins`
lists the agent's own instances (name, description, whether `Registry::environment` has a client,
and the options schema as of the plugin's last `Configure`). Every call goes through `Env::scope`,
which reads the agent's `environments` from the live configuration, and a workspace whose
environment isn't among them is refused, so a call can't use an environment the agent doesn't list.
For a job run the agent is the job's. `Workspaces::create` doesn't check the allowlist; the tools
do.

`Workspaces` and `Env` hold the registry weakly. The registry owns the core tools, and the tools own
`Workspaces`, so a strong reference back would keep the registry (and the database it shares) alive
after the runtime is dropped: the database's WAL files would never be checkpointed at shutdown.

### Opening a workspace

A workspace tool calls `Env::open`:

```rust
let owner = WorkspaceOwner::of_session(&call.turn.session);
let opened = runtime.workspaces.open(&owner, &call.turn.lease).await?;
// run in `opened.record.id` through `opened.client`; show `opened.notice` first, if any
```

It reads `current` first. With no workspace the error result lists the agent's environments with
their descriptions, points at `environment_plugins` and `environment_create`, and, if the owner had
one, says when and why it was deleted (`last_deleted`: `deleted_at`, `DeleteReason`'s wording) and
the environment and options it had. `Opened::notice` leads the result, error or not.
`WorkspaceError`'s messages are written for the model, so the other failures are returned as
`error.to_string()`; `environment_create` words `AlreadyHasWorkspace` itself, with the current
environment and options.

### Lifecycle tools

- `environment_create {plugin, options?}` checks the allowlist and calls `Workspaces::create`.
- `environment_status` reads `current`, and asks the plugin's `ListWorkspaces` for the run state and
  disk use. It doesn't call `StartWorkspace`, so it doesn't start a stopped workspace.
- `environment_reset {plugin?, options?}` resolves the target from the current workspace (options
  default to the current ones only if the plugin stays the same), checks the allowlist, that the
  environment is reachable, and that the options are an object, all before deleting. Then it calls
  `delete(ModelReset)` and `create`, and says what was discarded. If creating fails, the error
  result says so, and that the conversation now has no workspace.
- `environment_delete` calls `delete(ModelDelete)`.

None needs approval; an operator can put them in the agent's `ask` globs.

### `bash`

`ExecRequest { command, timeout, output_limit }` with the call's `timeout` (default 120 s, at most
600 s, outside that the call is refused) and an `output_limit` of 1 MiB. The core reads the stream
into a `Capture`: the first 10 KiB and the last 20 KiB of the combined output in arrival order. What
falls between is counted per stream, and each stream's section of the result shows
`[... N bytes of output left out ...]` where it was. Bytes the plugin dropped itself
(`ExecExit::dropped_bytes`) are reported at the end. The result gives the exit code, a note if the
environment timed the command out, and the stdout and stderr sections; a non-zero exit or a timeout
is an error result.

The call's future owns the exec stream, so dropping the future (the dispatcher's stream dropped by
`/stop`) drops the stream, which is how the plugin learns to kill the command. The dispatcher's
deadline for `bash` is `BASH_DEADLINE`, 11 minutes (`CallPolicy::of`), above the longest timeout.

### File tools

`read` asks for at most 256 KiB (`max_bytes`) and shows `cat -n` lines (`{:>6}\t{line}`), 2000 by
default, from the 1-based `offset`. It refuses content with a NUL byte in the first 8 KiB or that
isn't UTF-8 (a character cut by the byte cap at the end is dropped, not refused), and says when it
left lines out and the file's full size. `write` sends the whole file. `edit` reads the file (at
most 8 MiB; a larger file is refused, because writing back a partial read would cut it off), counts
non-overlapping matches of `old_string`, refuses zero, and more than one unless `replace_all`, and
writes the result. The read and the write aren't atomic. Paths go to the plugin as given: absolute,
or relative to the workspace's working directory.

`find` and `grep` are protocol calls with a limit of 200. `grep` lines are cut at 500 characters,
and a truncated result says so.

### Parallelism

`read`, `find`, `grep`, `environment_status`, and `environment_plugins` are in
`PARALLEL_SAFE_CORE_TOOLS`. Calls of one turn that open the workspace at once share its lease (a
shared lock; each `open` replaces the `LeaseSlot`'s guard with another read guard), and a call that
finds the workspace stopped starts it, so only one of them gets the notice. `write`, `edit`, `bash`,
and the lifecycle tools run alone.

## Tests

`workspaces/tests.rs` runs the service against an in-process environment (`workspaces/fake.rs`, with
a manual clock) and a real store: ids and one workspace per owner, refused and failed creations,
deleting with the plugin away and cleaning up on reconnect, opening (start notices, a lost
workspace, a plugin that is away, waiting for a sweep), leases against the sweep, stopping after
`stop_after`, deleting after `delete_after` with conversation activity keeping a workspace alive,
pruning, each reconciliation rule and the grace period, startup recovery, the schedule and the
connection watcher. `scheduler/tests.rs` covers the isolated run's deletion and a `dm` job's
workspace staying; `giles-store`'s tests cover the table, and the rename tests cover the rows
moving.

`core_tools/environment/tests.rs` runs each workspace tool against the same in-process environment
(its `exec` streams scripted events, or hangs and records that its stream was dropped, and it keeps
files): the offer and the allowlist gate, the no-workspace and deleted guidance, one workspace per
conversation, notices leading results, reset and delete, `bash` output and timeout handling and
cancellation, `read`'s numbering and caps, `edit`'s match counting, and `find` and `grep` limits.
The end-to-end tests (`crates/giles/tests/e2e/environments.rs`) drive the daemon with the echo
environment plugin: create, write, read, and run, and `/stop` cancelling a running command in the
plugin.
