# Instance rename

Instance names are permanent ids (PLAN section 7.2). `giles_core::rename` renames one,
`rename_instance`, which `giles instance rename <old> <new>` calls, and refuses a configuration that
looks like a rename done in `giles.toml` alone, `check_unused_instance_data`, which `Runtime::start`
calls. This page covers what a name keys, the order of the steps, and what a failure leaves behind.

## What a name keys

An audit of every place the implementation stores or derives something from an instance name. A
rename must move each of them, and the startup check treats each as data.

| Where                                        | What it is                                     | Rename does                    |
| -------------------------------------------- | ---------------------------------------------- | ------------------------------ |
| `giles.toml` `[<kind>.<name>]`               | The instance table and its sub-tables          | Renames the table in place     |
| `routes.channel`                             | A stored route's channel instance              | Updates in the transaction     |
| `giles.toml` `[[routes]]` `channel`          | A route of the old layout (imported once, then ignored) | Rewrites it (channels) |
| `agent.toml` `models`, `compaction_model`, `[[jobs]]` `model` | `<provider instance>/<model id>` | Rewrites the prefix (providers) |
| `agent.toml` `tools`, `ask`                  | Globs over `<toolset instance>__<tool>`        | Rewrites a leading `<name>__` (toolsets) |
| `sessions.channel`                           | Session key (agent, channel, conversation)     | Updates in the transaction     |
| `inbox.instance`                             | Dedupe key (instance, event id)                | Updates in the transaction     |
| `outbox.instance`                            | The channel a proactive send routes to         | Updates in the transaction     |
| `usage.provider`                             | The provider instance of a request             | Updates in the transaction     |
| `messages.meta` `$.provider`                 | The provider that wrote an assistant message   | Updates in the transaction     |
| `data/plugins/<name>/`                       | The plugin's only writable directory           | Moves (`rename(2)`)            |
| `agents/<agent>/groups/<name>/`              | Per-group `MEMORY.md` files of the old memory design | Moves                   |
| `agents/<agent>/memory.db`, `memories.owner_instance` | The channel instance of a group that owns a memory (also a shared memory's author) | Updates in the hook, one transaction per database |
| `cache/describe/<name>.json`, `cache/toolsets/<name>.json` | Discovery caches                 | Moves if present; they are disposable |
| `run/pids/<name>.pid`                        | The launched plugin's pid, killed at the next start | Moves if present, so a plugin that outlived the core is still found |

Routes are the exception to "a rename must move each of them, and the startup check treats each as
data": a route is a setting, not data. The rename moves `routes.channel` (in the same transaction,
keeping the order; `RenamedRows::routes` counts them), and a name that only routes use still counts
as existing for `old`, so a table renamed by hand can be finished. But `instance_rows`, which the
startup check reads, leaves routes out, and a route to `new` isn't a conflict. The reason is the
removed-instance case: a route to a removed instance is kept (the routes page flags it), and if
routes counted as data, adding any instance would make the check take the removed name for the old
name of the new instance and refuse to start until it is retired. The `[[routes]]` of `giles.toml`
are rewritten too, because Giles reads them exactly once: a rename before the first start with
routes in the database would otherwise import the old name, and the rewrite keeps a leftover block
consistent after that (`RenameReport::legacy_routes`).

Things that look like they carry a name and don't:

- `inbox.payload`, `outbox.payload`, `jobs.spec`, and `job_runs` hold no instance name.
- Identities (`irc:libera:account:me`) use the namespace the channel reports, not the instance name
  (PLAN section 3.2), so users' identity links and `[owner] identities` are untouched.
- `run/sandbox/<name>.json` is rewritten at every launch; it is left as it is and goes stale.

Not rewritten, by design:

- Tool names inside stored transcripts. A toolset's tool calls are stored as `<old>__<tool>`;
  history is a record, and providers accept tool names in history that are no longer offered.
- Globs that match a toolset's tools by its old name without starting with `<old>__` (`github*`,
  `git*`, `*hub__*`). A pattern can't be rewritten safely, and leaving it would silently stop it
  matching (an `ask` entry would stop asking), so the rename refuses and lists them: the operator
  rewrites them (for example to `github__*`) and runs the command again. A glob counts if it matches
  some `<old>__<tool>` but not `<new>__<tool>` (`agent_loop::glob::matches` on a few probe tool
  names); `*`, `memory*`, and `*__create_issue` mean the same under both names and are fine.
- The `sandbox` grants of other instances, which can't name another instance's directory anyway.

`Store::instance_rows`, `Store::instance_routes` and `Store::rename_instance` (storage.md, "Instance
Names") do the database half.

## Editing config with comments intact

`rename::edit` uses `toml_edit` so the files stay as the operator wrote them. It removes every entry
of the `[provider]`, `[channel]`, or `[toolset]` table and puts them back, the renamed one under a
new key that copies the old key's whitespace decoration, so the order of entries, the header's
comments and its sub-tables (which keep their own document positions) are all unchanged. String
values are replaced with their surrounding whitespace and comments kept. Inline and dotted spellings
work as well. After editing, the module parses the result and checks that the new name is configured
with the same kind and the old one is gone; if not, it refuses and changes nothing, so a form it
cannot edit is never half-edited.

An instance with no `component` key runs the component named like the instance (`[channel.web]` is
the built-in web channel, `[toolset.github]` without `command` or `url` runs
`giles-toolset-github`), so a plain rename would change what runs, and for a channel also the
identity namespace, which must start with the component's name. When the renamed table lacks
`component` and the instance runs a component (providers and channels always; a toolset only without
`url` and `command`, where `component` is an error), the edit adds `component = "<old>"`. The check
after editing compares the component the old and the new table resolve to and refuses if they
differ.

## The steps

`rename_instance(root, store, lock, old, new)` takes the data root's `InstanceLock` as an argument:
the caller proves the core is stopped by holding the same `flock` `giles serve` holds, so no admin
socket probing is needed and a core that starts mid-rename is refused instead.

1. **Validate, read-only.** Both names must be `[a-zA-Z0-9-]+`, different ignoring case, and `new`
   must not differ only in case from another configured instance. `giles.toml` and every
   `agent.toml` are read and parsed, the database's names are listed (`instance_rows`), and the data
   directories are scanned.
   - If both names are configured the rename is refused: instances are never merged.
   - If only `old` is configured, that table is renamed.
   - If only `new` is configured, the operator already renamed the table (this is what the startup
     check asks for) and the data under `old` is moved to match. `old` must have data.
   - If neither is configured, it is refused: with no data it is an unknown instance, and with data
     the kind can't be known, so the message says to configure `new` first.
   - `new` must have no data: no database rows, no `data/plugins/<new>`, no `groups/<new>` (routes
     that name `new` don't count). The memory databases aren't checked: nothing in their schema is
     unique over the owner, and an interrupted run (below) legitimately leaves some of them already
     renamed. The exception is an interrupted rename run again (below): `giles.toml` already names
     `new`, so `old` is no longer configured, and rows still exist under `old`. While `old` is still
     configured, a directory called `new` belongs to something else and is refused.
2. **Stage.** Every file's new text is computed in memory: the `agent.toml` files that name the
   instance, then `giles.toml`; and the list of directory and file moves is computed. The memory
   databases to update are listed: every `agents/<agent>/memory.db` that exists. An agent without
   one is skipped (the rename never creates a database), and a removed agent's database is in
   `trash/agents/`, outside `agents/`, so it is not touched, like the agent's directories and
   `agent.toml` there. Any parse error or conflict ends the command here with nothing changed.
3. **One database transaction** (`Store::rename_instance`): update every row, then run a hook, then
   commit. The hook does the file system work, in this order, recording each completed step in a
   journal:
   1. rewrite the `agent.toml` files (a temporary file in the same directory renamed over the
      original, with the original's permissions),
   2. rewrite `giles.toml` the same way,
   3. move the directories, then the disposable files, with `rename(2)`,
   4. in each agent's memory database, in agent-name order, run
      `MemoryStore::rename_instance(old, new)`:
      `UPDATE memories SET owner_instance = new WHERE owner_kind = 'group' AND owner_instance = old`,
      one write transaction per database, the file opened with `OpenOptions::keep_journal_mode` (the
      core is stopped, and the rename must not undo the daemon's journal mode). The hook runs on the
      blocking pool and drives these async calls with the runtime's handle. A database that changed
      is recorded in a second journal with its row count (`RenameReport::memory_dbs`).

   If any step fails, the journal is undone newest first (the memory databases first: the same
   update with the names swapped in each database that changed; then files rewritten from the text
   read in step 1, and moves reversed) and the error returns, which rolls the transaction back. If
   the commit itself fails after the hook, the caller undoes the journal too.
4. The report lists what changed, for the command to print.

Why this order. The database transaction is the only step that can't be undone by hand-editing, so
it is the one that is held open until everything else worked: every failure the code can observe
leaves the data root exactly as it was. `giles.toml` is rewritten before the directories, so a crash
between them leaves the startup check's situation (config renamed, data not) and not the reverse.
The memory databases come last, after `giles.toml`, for the same reason and one more: each of them
commits on its own, so a step after them would have to rename them back with another write. Renaming
the owners is also idempotent (rows already under `new` don't match), which makes it safe to repeat
after a crash.

## Crashes

A failure the process can see is undone. If the process is killed (power loss, `kill -9`) during
step 3, some files or directories may have changed, and some memory databases may already name
`new`, while `giles.db` has not committed. The journals exist only in memory, so recovery is to run
the same command again:

- The database still has rows under `old`, so a directory that is already at its new path (and no
  longer at the old one) is treated as moved. A directory at both paths is a conflict, and the
  command refuses and says which.
- `giles.toml` already naming `new` is the "only `new` is configured" case above.
- Rewriting an `agent.toml` is idempotent: only references to `old` are changed.
- A memory database that was already updated has no group owner under `old` any more, so the rerun
  changes nothing in it and lists only the databases it did update (`RenameReport::memory_dbs`); the
  others are finished. In a database whose commit never happened nothing changed, since a database
  is one transaction.

The window is the few system calls of step 3, and the startup check usually notices the unfinished
state if the daemon is started in between (`giles.toml` renamed, rows still under `old`). It cannot
once any moved directory is already at its new path, because the instance then has a data directory
and no longer looks new: the daemon would start, with the old rows stranded. Run the command again
before the next start. After the daemon has written rows under the new name the command refuses (the
new name has data), and merging by hand is the only way back.

## Startup check

`Runtime::start` calls `check_unused_instance_data(root, config, store)` right after the database
opens, before anything else uses the data, and fails with `StartError::InstanceData`. It refuses the
one situation that looks like a rename done in `giles.toml` alone, because starting would leave the
old name's sessions, inbox, and directories orphaned and begin a new, empty instance.

Definitions:

- A name **has data** if it appears in `sessions.channel`, `inbox.instance`, `outbox.instance`, or
  `usage.provider` (not `routes.channel`, see above), or is a directory `data/plugins/<name>` or
  `agents/*/groups/<name>`. Only directories with a valid name count (`.DS_Store` doesn't). An agent
  removed in the web UI lives in `trash/agents/`, outside `agents/`, so its directories count for
  nothing here, and a rename neither reads nor rewrites its `agent.toml` or its `memory.db`. The
  memories in a restored agent's database keep the old instance name, so the renamed instance's
  groups don't see them. Memory databases are not data to this check.
- A configured instance is **fresh** if its name has no data.
- A name is **unused** if it has data, is not configured, and is not in `[core] retired_instances`.
- An unused name **matches** a fresh instance if the kinds fit. Rows of `sessions`, `inbox`, or
  `outbox` can only belong to a channel; `usage` rows to a provider; a `groups` directory to a
  channel; a `data/plugins` directory to a channel or a toolset that the core launches (a plugin
  channel, or a stdio toolset). Built-in and remote instances, and v1 providers, never have such a
  directory, so a leftover one is never taken for their old name. A remote toolset (`url`) gets
  neither rows nor a directory, so nothing ever matches it.

The check fails if some unused name matches a fresh instance. The message names each such pair and
shows `giles instance rename <old> <new>` with the first candidate.

False positives and how each resolves:

| Situation                                                     | Result   | Resolution                           |
| ------------------------------------------------------------- | -------- | ------------------------------------ |
| A new instance is added; nothing has been removed             | Starts   |                                      |
| An instance is removed; nothing is added                      | Starts   | Its data stays unused                |
| An instance is removed, an unrelated one of the same kind is added | Refused | List the old name in `retired_instances` |
| An instance is removed and a different kind is added          | Starts when no kind fits | |
| A built-in or remote channel, or a provider, has never produced data and a removed instance of that kind had data | Refused | `retired_instances`, or use the instance once |
| A remote toolset is configured and a stdio toolset's directory is left over | Starts |  |
| `retired_instances` names a configured instance               | Config error | Remove one of them               |

`retired_instances` (`[core]` in `giles.toml`) is the explicit "I know" list: names there are never
unused. It keeps the data, so the name can be un-retired and configured again later. It doesn't
delete anything: the rows and directories of a retired name stay until the operator removes them
(there is no command for that in v1).

The check is conservative about what it reads: it doesn't count messages (every assistant message
with a provider has a `usage` row) and doesn't look at the cache or pid files.
