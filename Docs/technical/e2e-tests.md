# End-to-End Tests

`crates/giles/tests/e2e/` runs the whole daemon, scripted from outside. Each test
starts the real `giles serve` binary on a temporary data root, with the echo channel plugin
(`plugins/giles-channel-echo`, see [Plugin SDK](plugin-sdk.md)) and a fake model provider. The
tests play the owner: they put messages into the echo plugin and read back what the daemon sent.
Nothing in the product is test-aware: there is no test feature, injectable provider, or clock.

| File          | What it holds                                                                      |
| ------------- | ---------------------------------------------------------------------------------- |
| `provider.rs` | `FakeProvider`: the scriptable model server                                        |
| `harness.rs`  | `Home` (data root, echo plugin, database), `Daemon` (the process and its log)     |
| `chat.rs`     | Message to reply, tool call with approval (allowed and denied), `/stop`            |
| `recovery.rs` | Killing the daemon mid-turn and starting it again                                  |
| `jobs.rs`     | A configured job delivered through the outbox, also across a restart               |

The suite is one test target, `cargo test -p giles --test e2e`, and part of `cargo test
--workspace`. It takes a few seconds: the tests run in parallel, each with its own daemon, data
root, and provider.

## The fake provider

Provider plugins aren't supported yet, and there is no way to put a different `Provider` into
`giles serve`, so the fake is a server instead of a component: `FakeProvider` is a loopback HTTP
server speaking the OpenAI Chat Completions streaming protocol, and the data root's `giles.toml`
configures the built-in `openai` provider with `base_url` pointing at it
([Providers](providers.md)). The daemon's real provider code, SSE parsing, and tool call handling
run; only the model is scripted.

A test queues replies with `script([...])`, one per `POST /v1/chat/completions`, and the provider
records each request body, which the test reads with `requests()` or `wait_requests(n)`:

| `Reply`            | What the server streams                                                       |
| ------------------ | ----------------------------------------------------------------------------- |
| `Reply::text(..)`  | A text answer, then `stop`                                                    |
| `Reply::call(..)`  | One tool call (id, name, JSON input, streamed in pieces), then `tool_calls`   |
| `Reply::Hold`      | The start of a response, then silence until the client hangs up               |

`Hold` is the turn in flight for `/stop` and for a crash. The server notices the daemon dropping
the request (`wait_hang_ups(n)`), which is how a test knows a cancellation reached the provider
call, not just the session. A request with nothing queued gets a 400 and is counted, and each test
ends with `assert_no_surprises()`: no unscripted request, no queued reply left over. So a test
can't pass by accident through a turn it didn't plan, and scripts say exactly what the model does.
`GET /v1/models` lists `test-model`, the model every agent uses (`fake/test-model`).

## The harness

- **Data root.** `Home::new(provider, agent_extra)` creates a temporary root (mode 0700, resolved
  through symbolic links, since the macOS sandbox compares real paths) with `giles.toml` (an
  owner `echo:owner`, the fake provider, the echo channel, one catch-all route to `jeeves`),
  `agents/jeeves/agent.toml` (`agent_extra` is appended: `tools`, `ask`, `[[jobs]]`), and the echo
  plugin symlinked into `plugins/`. `HOME` and `TMPDIR` point into the temporary directory, the
  HTTP server listens on `127.0.0.1:0`, and the daemon's sockets are under the root (or `TMPDIR`
  when the path is long, as in production). Nothing touches the real `~/.giles`.
- **The plugin binary** is `giles-channel-echo` next to the `giles` binary in `target/`. Once per
  test run the harness runs `cargo build -p giles-channel-echo` for that target directory and
  profile. It does nothing when `cargo test --workspace` has just built the plugin (same artifact),
  and builds or rebuilds it when only `-p giles` is tested, so a stale plugin never runs.
- **Sandbox.** The echo plugin runs sandboxed wherever `sandbox::check_host()` says the host can
  (macOS, Linux with Landlock); otherwise the config adds `unsandboxed = true`, so the same tests
  run in a container without Landlock.
- **Driving the owner.** `say(text)` writes an event into the plugin's `inject/` directory (staged
  elsewhere and renamed in, as the plugin expects). `sent()`, `wait_sent(needle)`,
  `wait_sent_after(n, needle)`, `wait_approval_code(n, tool)`, and `acked()` read the plugin's
  `sent.jsonl` and `acks.jsonl`. These files live in the plugin's data directory, so they survive
  a daemon restart, and tests that restart count what was sent before to find what is new.
- **Inspecting state.** `db()`, `inbox_states()`, and `outbox_rows()` read the daemon's SQLite
  database (read-only use; the daemon owns it) for facts the channel can't show, such as an inbox
  row being `started` or `unknown`.
- **The daemon.** `Daemon::start(&home)` spawns `giles serve --home <root>` and waits for
  `giles started`; its stderr is collected for `wait_for_log`. `stop()` sends SIGTERM and checks
  for a clean exit, `kill()` is a SIGKILL crash, and dropping a daemon kills it and, when the test
  is failing, prints its log. A killed daemon's plugin sees stdin close and exits; the next start
  reaps one that doesn't ([Plugin Host](plugin-host.md#pid-files-and-orphans)).
- **Waiting.** `wait_until` polls every 20 ms up to 30 s; the fake provider and the log use
  condition variables. Waiting for something the outbox delivers allows 90 s (`OUTBOX_WAIT`): its
  retry delay grows to a minute. A log wait fails at once if the daemon's output has ended, and
  the log's lock is never held while panicking (a poisoned lock would make `Daemon`'s `Drop`
  panic, abort the test binary, and leak daemons). No test sleeps for a fixed time. Where a reply
  is sent before the daemon records that the turn ended (the inbox row becomes `done` just
  after), the test polls for the state instead of reading it once.

## Scenarios

- **Message to reply** (`chat.rs`): the message reaches the model with the agent's prompt, the
  text answer goes back to the conversation it came from, the plugin is acked, and the second
  request carries the whole conversation.
- **Approval** (`chat.rs`): a call matching `ask` stops at a prompt that shows the input and goes to
  the asking conversation, nothing runs and the model isn't asked again until `yes <code>`; then
  the tool runs (`memory_add`: the memory is in the agent's `memory.db`) and the model gets the
  result. `no <code>` and any other message deny the call, the model is told (`Error: denied`,
  `Error: Not approved. The user said: ...`), and the tool never runs.
- **`/stop`** (`chat.rs`): during a model request, the daemon drops the request, the conversation
  works at once, and the cancelled turn left nothing in the history; during a pending approval,
  the call's slot is filled with `cancelled` and a late `yes` doesn't run it.
- **Restart recovery** (`recovery.rs`): SIGKILL with an approval pending, and SIGKILL while the
  model is answering. The inbox row is `started` after the crash; after the restart the owner is
  told `Interrupted by a restart; resend if needed.`, the row is `unknown`, nothing is replayed
  (the provider sees no extra request), the model gets `Error: interrupted` for the call that was
  waiting, and that call never runs.
- **Job delivery** (`jobs.rs`): `cron = "0 0 1 1 *"` with a `job_runs` row 400 days old (written
  with `giles-store` before the daemon starts) makes the scheduler run the job once at startup
  as a missed time ([Scheduler](scheduler.md#timing)), without waiting for a clock. The reply is
  sent to the channel as `{"recipient": "owner"}`, the identity `echo:owner` split at its
  namespace, and the outbox is empty after. In a second test the echo plugin refuses the owner
  (`unavailable`), the row waits in `outbox` with its error, survives the daemon stopping, and
  is delivered once after a restart with the channel working; the job doesn't run again.

## Adding a test

Script the replies first, build a `Home` with the agent's `tools`, `ask`, or jobs, `Daemon::start`
it, drive it with `say` and the `wait_*` helpers, and end with `daemon.stop()` and
`provider.assert_no_surprises()`. Wait for what the daemon does, never for time, and assert
absence only after something later has shown the daemon moved on (a reply to a following message,
not a pause).
