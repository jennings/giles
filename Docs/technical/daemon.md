# The `giles` Binary

`crates/giles` holds the command-line interface and the daemon's process lifecycle. Everything else
lives in library crates.

## Command Line

`clap` (derive). `cli::Cli` has the global options and a `cli::Command` enum with one variant per
subcommand, each carrying its own `clap::Args` struct (`Serve(ServeArgs)`,
`SandboxExec(SandboxExecArgs)`, ...). `status`, `usage`, `backup`, and `admin login-link` call the
running daemon's admin service ([Admin Service](admin.md)) through `admin_client::call`, which
builds a current-thread Tokio runtime and words connection and token errors for the operator.
`instance rename` is the opposite: it works on the data root with the core stopped, taking the same
single-instance lock `serve` takes ([Instance Rename](instance-rename.md)). Later subcommands
(`pair`) are added as variants with their own argument structs and modules, dispatched from `main`.

`giles sandbox-exec <SPEC> -- <COMMAND>...` is the internal plugin sandbox trampoline (see
[Plugin Sandbox](sandbox.md)); `--home` is accepted there but unused. Its failures exit 125 to 127
like `env(1)` instead of 1 (see [Exit Status](sandbox.md#exit-status)).

`--home <DIR>` is global, so it is accepted before or after the subcommand. It is not tied to
`GILES_HOME` through `clap`; `giles_core::paths::resolve_root` applies the precedence (flag, then
`GILES_HOME`, then `$HOME/.giles`) so it can be tested as a pure function (see
[Data Root](data-root.md)).

`main` is synchronous. Only `serve` builds a multi-threaded Tokio runtime, and the admin client
commands a current-thread one, so subcommands that must not start threads, such as the
`sandbox-exec` trampoline, don't get one.

Errors are `anyhow` in the binary and `thiserror` in libraries; a failed subcommand prints
`Error: ...` with its causes and exits with status 1.

## Logging

One `tracing-subscriber` `fmt` subscriber, installed in `main` before any subcommand runs. It writes
to stderr, with ANSI colors only when stderr is a terminal. The filter is `GILES_LOG`, else
`RUST_LOG`, else `info`; an invalid filter fails startup rather than being ignored.

## `giles serve` Lifecycle

1. Resolve the data root (`DataRoot::from_env`) and create its layout (`create_layout`), then take
   the single-instance lock on `run/giles.lock` (`giles_core::admin::InstanceLock`,
   [Admin Service](admin.md#one-core-per-data-root)). A second `giles serve` on the same data root
   fails here.
2. Load and validate the configuration (`giles_core::config::Config::load`,
   [Configuration](config.md)). Any error ends the process here, before the runtime exists.
3. Build the runtime.
4. Install the SIGINT and SIGTERM handlers (`shutdown::cancel_on_signal`). This happens before
   anything is logged, so a signal sent after the `giles started` line always takes the graceful
   path instead of killing the process.
5. Check that the host can sandbox plugins (`giles_core::sandbox::check_host`, see
   [Plugin Sandbox](sandbox.md#host-check)) and log the result; a failure is a warning, and the
   plugin host refuses sandboxed instances with it. Find the `giles` binary itself, the sandbox
   trampoline (`std::env::current_exe`).
6. Start the core runtime (`giles_core::runtime::Runtime::start`, [Registry](registry.md)): open the
   database, check that no unused instance name still has data while a configured instance has none
   ([Instance Rename](instance-rename.md#startup-check)), then open the blob store and build the
   registry from the configuration. Any error ends the process. Then register every toolset
   (`ToolsetHost::start`, [Toolsets](toolsets.md)), each offering its cached tools, with its first
   start running in the background, and log what is registered (`registry ready`) and each instance
   still pending.
7. Bind the HTTP listener on `core.http_listen` (a busy address fails startup here). Then write a
   fresh admin token and bind the admin socket (`AdminListener::bind`, removing one a crash left).
8. Start the outbox's sender (`Runtime::start_outbox`, [Outbox](outbox.md)): drop the approval
   prompts the previous process left, then deliver proactive sends as their channels come up. Start
   the sessions (`Runtime::start_sessions`, [Sessions and Turns](sessions.md)): recover from an
   earlier crash or shutdown (interrupted turns), resume queued messages, and consume the events of
   every registered channel. Turns run on the agent loop (`AgentLoop::from_runtime`,
   [Agent Loop](agent-loop.md)). This is the last step that can fail: recovery changes state and
   resumed turns start running, so nothing after it may abort startup. Then start the HTTP server on
   its own task ([HTTP Server](http.md#lifecycle)) and the admin service on another
   ([Admin Service](admin.md)), and start supervising every plugin channel (`PluginHost::start`,
   [Plugin Host](plugin-host.md#supervision)), which waits for each one's first launch attempt,
   while also waiting for every toolset's first start (`ToolsetHost::started`); both waits end if a
   shutdown signal arrives first. Each plugin is registered with the registry and attached to the
   sessions as it comes up; one that fails is logged and relaunched with backoff, or once its binary
   changes. A toolset that fails to start is logged and retried with backoff
   ([Toolsets](toolsets.md#lifecycle)). The admin service reports plugin supervision through
   `admin::PluginControl`. The scheduler and the blob sweep (`Runtime::start_blob_sweep`,
   `giles_core::blobs::BlobSweep`) start just before them. The sweep waits 60 seconds, so recovery
   and the first turns finish first, then runs `BlobStore::sweep` ([Storage](storage.md#sweep))
   every six hours, with the 48-hour grace period (constants `SWEEP_FIRST_DELAY` and
   `SWEEP_INTERVAL`; the interval is measured from the end of one sweep to the start of the next).
   Each run logs one `info` line (`blob sweep removed unreferenced blobs`, with `blobs`,
   `orphan_files`, and `bytes`) when it removed something, a `debug` line when it didn't, and a
   `warn` when it failed, in which case it removed nothing and the next run is on schedule.
9. Log `giles started` with the root, admin socket path, HTTP URL, and instance and agent counts,
   plus a warning if the sockets use the `$TMPDIR` fallback.
10. Wait until the shutdown token is cancelled, then await the HTTP server (it drains open requests
    for up to 5 s), the sessions (`Sessions::closed`: running turns are cancelled, channel pumps
    stop between events), the outbox's sender (`Outbox::closed`), and the blob sweep
    (`BlobSweep::closed`; a sweep that is running finishes first), then stop the plugins and their
    supervision (`PluginHost::stop_all`: `Shutdown`, then `SIGTERM`, then `SIGKILL` to each plugin's
    process group) and the toolsets (`ToolsetHost::stop`: stdio servers' process groups killed,
    remote connections closed), await the admin server (it drains for up to 5 s and removes its
    socket and token file), log `giles stopped`, release the lock, and exit with status 0. If the
    HTTP server stops on its own first, cancel the token, stop the plugins, the toolsets, and the
    admin server, and exit with an error.

The shutdown signal is a `tokio_util::sync::CancellationToken`. The first SIGINT or SIGTERM cancels
it. Subsystems added later take a clone (or a `child_token()`), stop when it is cancelled, and are
awaited by `serve` before it returns.

The signal handlers stay installed for the life of the process, so the default "die on signal" no
longer applies. To keep a hung shutdown interruptible without SIGKILL, any SIGINT or SIGTERM that
arrives after the token is cancelled logs a warning and calls `std::process::exit(130)` without
waiting for subsystems.
