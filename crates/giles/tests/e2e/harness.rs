//! The harness: a throwaway data root, the real `giles serve` binary running in it, the echo
//! channel plugin it launches, and helpers to drive and inspect both.
//!
//! Everything is private to one test: a temporary data root (so `GILES_HOME`, `HOME`, and
//! `TMPDIR` never point at the user's), loopback listeners on port 0, and Unix sockets under
//! the root. Waiting is polling with a deadline; nothing sleeps for a fixed time.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::provider::FakeProvider;

/// The most any wait takes before the test fails. Generous: CI hosts are slow and shared.
pub const WAIT: Duration = Duration::from_secs(30);

/// The agent every message is routed to.
pub const AGENT: &str = "jeeves";

/// The echo channel instance's name, which is also its identity namespace.
pub const CHANNEL: &str = "echo";

/// The conversation the tests talk in (the one `Home::say` injects into).
pub const CONVERSATION: &str = "dm";

/// The longest to wait for something the outbox delivers: its retry delay grows to a minute
/// (`BACKOFF_MAX`), plus slack.
pub const OUTBOX_WAIT: Duration = Duration::from_secs(90);

/// Polls `check` every 20 ms until it returns `Some`, failing the test after [`WAIT`].
pub fn wait_until<T>(what: &str, check: impl FnMut() -> Option<T>) -> T {
    wait_until_within(WAIT, what, check)
}

/// [`wait_until`] with a limit other than [`WAIT`].
pub fn wait_until_within<T>(
    limit: Duration,
    what: &str,
    mut check: impl FnMut() -> Option<T>,
) -> T {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A data root for one daemon, and the echo plugin's files inside it.
pub struct Home {
    dir: tempfile::TempDir,
    provider_url: String,
    agent_toml: String,
    next_event: AtomicUsize,
}

impl Home {
    /// A data root whose `jeeves` agent uses `provider`'s model, with `agent_extra` appended to
    /// its `agent.toml` (`tools`, `ask`, `[[jobs]]`, ...), and an echo channel with no config.
    pub fn new(provider: &FakeProvider, agent_extra: &str) -> Self {
        let home = Self {
            dir: tempfile::Builder::new().prefix("g").tempdir().unwrap(),
            provider_url: provider.base_url(),
            agent_toml: format!(
                "name = \"Jeeves\"\nmodels = [\"fake/{}\"]\n{agent_extra}\n",
                crate::provider::MODEL
            ),
            next_event: AtomicUsize::new(1),
        };
        fs::DirBuilder::new()
            .mode(0o700)
            .create(home.root())
            .unwrap();
        fs::create_dir_all(home.root().join("agents").join(AGENT)).unwrap();
        fs::create_dir(home.root().join("plugins")).unwrap();
        symlink(
            &*ECHO_BINARY,
            home.root().join("plugins/giles-channel-echo"),
        )
        .unwrap();
        fs::create_dir(home.user()).unwrap();
        fs::create_dir(home.tmp()).unwrap();
        home.configure_echo("");
        home
    }

    /// The temporary directory, resolved: the sandbox compares real paths (macOS `/var` is a link
    /// to `/private/var`).
    fn base(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// The data root.
    pub fn root(&self) -> PathBuf {
        self.base().join("root")
    }

    fn user(&self) -> PathBuf {
        self.base().join("user")
    }

    fn tmp(&self) -> PathBuf {
        self.base().join("tmp")
    }

    /// Rewrites `giles.toml` and `agent.toml` with `echo_config` (TOML keys of the echo
    /// channel's `config` table, e.g. `unavailable = ["owner"]`). Takes effect at the next start.
    pub fn configure_echo(&self, echo_config: &str) {
        // Plugins run in the sandbox wherever this host has one.
        let unsandboxed = if giles_core::sandbox::check_host().is_ok() {
            ""
        } else {
            "unsandboxed = true\n"
        };
        fs::write(
            self.root().join("giles.toml"),
            format!(
                "[core]\nhttp_listen = \"127.0.0.1:0\"\n\n\
                 [owner]\nidentities = [\"{CHANNEL}:owner\"]\n\n\
                 [provider.fake]\ncomponent = \"openai\"\n\
                 config = {{ base_url = \"{}\" }}\n\n\
                 [channel.{CHANNEL}]\ncomponent = \"echo\"\n{unsandboxed}\n\
                 [channel.{CHANNEL}.config]\n{echo_config}\n\n\
                 [[routes]]\nagent = \"{AGENT}\"\n",
                self.provider_url
            ),
        )
        .unwrap();
        fs::write(
            self.root().join("agents").join(AGENT).join("agent.toml"),
            &self.agent_toml,
        )
        .unwrap();
    }

    fn echo_dir(&self) -> PathBuf {
        self.root().join("data/channel").join(CHANNEL)
    }

    /// The agent's directory.
    pub fn agent_dir(&self) -> PathBuf {
        self.root().join("agents").join(AGENT)
    }

    /// The owner sends `text` in the test conversation, through the echo plugin.
    pub fn say(&self, text: &str) {
        let inject = self.echo_dir().join("inject");
        // The plugin creates `inject/` when it is configured.
        wait_until("the echo plugin's inject directory", || {
            inject.is_dir().then_some(())
        });
        let id = format!("e{}", self.next_event.fetch_add(1, Ordering::SeqCst));
        let event = json!({
            "event": {
                "type": "message",
                "id": id,
                "message": {
                    "conversation": { "id": CONVERSATION, "kind": "direct" },
                    "sender": "owner",
                    "sender_aliases": [],
                    "sender_authenticated": true,
                    "text": text,
                    "attachments": [],
                },
            },
        });
        // Written elsewhere and renamed in, so the plugin never reads half a file.
        let staged = self.echo_dir().join(format!("{id}.staged"));
        fs::write(&staged, serde_json::to_vec(&event).unwrap()).unwrap();
        fs::rename(&staged, inject.join(format!("{id}.json"))).unwrap();
    }

    /// Every message the daemon has sent through the echo plugin, oldest first.
    pub fn sent(&self) -> Vec<Value> {
        let Ok(text) = fs::read_to_string(self.echo_dir().join("sent.jsonl")) else {
            return Vec::new();
        };
        // A line being appended right now doesn't parse yet; it counts once it's whole.
        text.lines()
            .map_while(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The markdown of every message sent so far.
    pub fn texts(&self) -> Vec<String> {
        self.sent()
            .iter()
            .map(|sent| sent["message"]["markdown"].as_str().unwrap().to_owned())
            .collect()
    }

    /// Waits for a sent message whose text contains `needle`; returns the whole record
    /// (`{"message_id", "message": {"target", "markdown", ..}, ..}`).
    pub fn wait_sent(&self, needle: &str) -> Value {
        self.wait_sent_after(0, needle)
    }

    /// [`Home::wait_sent`], looking only at the messages after the first `skip`.
    pub fn wait_sent_after(&self, skip: usize, needle: &str) -> Value {
        self.wait_sent_within(WAIT, skip, needle)
    }

    /// [`Home::wait_sent_after`] with a limit other than [`WAIT`].
    pub fn wait_sent_within(&self, limit: Duration, skip: usize, needle: &str) -> Value {
        wait_until_within(limit, &format!("a message containing {needle:?}"), || {
            self.sent().into_iter().skip(skip).find(|sent| {
                sent["message"]["markdown"]
                    .as_str()
                    .is_some_and(|text| text.contains(needle))
            })
        })
    }

    /// Waits for the approval prompt for a call to `tool` among the messages after the first
    /// `skip`, and returns its code.
    pub fn wait_approval_code(&self, skip: usize, tool: &str) -> String {
        let prompt = self.wait_sent_after(skip, &format!("Allow `{tool}`"));
        let text = prompt["message"]["markdown"].as_str().unwrap();
        let code = text
            .split("Reply `yes ")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .unwrap_or_else(|| panic!("no code in the prompt {text:?}"));
        assert!(code.chars().all(|c| c.is_ascii_digit()), "{code:?}");
        code.to_owned()
    }

    /// The ids of the events the daemon has acknowledged to the plugin, oldest first.
    pub fn acked(&self) -> Vec<String> {
        let Ok(text) = fs::read_to_string(self.echo_dir().join("acks.jsonl")) else {
            return Vec::new();
        };
        text.lines()
            .map_while(|line| serde_json::from_str::<Value>(line).ok())
            .map(|ack| ack["event_id"].as_str().unwrap().to_owned())
            .collect()
    }

    /// A connection to the daemon's database (read it only: the daemon owns it).
    pub fn db(&self) -> rusqlite::Connection {
        wait_until("the database file", || {
            let path = self.root().join("data/giles.db");
            path.is_file()
                .then(|| rusqlite::Connection::open(path).unwrap())
        })
    }

    /// The pending outbox rows as (attempts so far, last error), oldest first.
    pub fn outbox_rows(&self) -> Vec<(i64, Option<String>)> {
        let db = self.db();
        let mut statement = db
            .prepare("SELECT attempts, last_error FROM outbox ORDER BY id")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// What the agent's memory database holds, as (content, core, shared), oldest first. Empty
    /// when the database doesn't exist yet (it is created the first time something asks for it)
    /// or the daemon hasn't finished creating its tables.
    pub fn memories(&self) -> Vec<(String, bool, bool)> {
        let path = self.agent_dir().join("memory.db");
        if !path.is_file() {
            return Vec::new();
        }
        let db = rusqlite::Connection::open(path).unwrap();
        let Ok(mut statement) =
            db.prepare("SELECT content, core, shared FROM memories ORDER BY id")
        else {
            return Vec::new();
        };
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// The state of every inbox row, oldest first.
    pub fn inbox_states(&self) -> Vec<String> {
        let db = self.db();
        let mut statement = db.prepare("SELECT state FROM inbox ORDER BY id").unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
}

/// The echo plugin binary built next to `giles`, brought up to date once per test run: `cargo
/// build -p giles-channel-echo` does nothing when `cargo test --workspace` has just built it (it
/// resolves to the same artifact), and builds or rebuilds it when only `-p giles` is being tested,
/// so a stale plugin never runs silently. The tests run the build one at a time (this
/// initializer), and Cargo isn't holding its build lock while tests run.
static ECHO_BINARY: LazyLock<PathBuf> = LazyLock::new(|| {
    let giles = Path::new(env!("CARGO_BIN_EXE_giles"));
    let profile_dir = giles.parent().unwrap();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = Command::new(cargo);
    build
        .args(["build", "--locked", "-p", "giles-channel-echo"])
        .arg("--target-dir")
        .arg(profile_dir.parent().unwrap());
    let profile = profile_dir.file_name().unwrap().to_str().unwrap();
    if profile != "debug" {
        build.args(["--profile", profile]);
    }
    let status = build.status().unwrap();
    assert!(status.success(), "cannot build giles-channel-echo");
    let echo = profile_dir.join("giles-channel-echo");
    assert!(echo.is_file(), "no {} after building it", echo.display());
    echo
});

/// What a daemon has written to stderr, and whether it has finished.
#[derive(Default)]
struct LogState {
    lines: Vec<String>,
    /// The pipe closed: the daemon (and anything holding its stderr) is gone.
    ended: bool,
}

/// The log of a running daemon.
#[derive(Default)]
struct Log {
    state: Mutex<LogState>,
    changed: Condvar,
}

impl Log {
    /// Locks the state. A poisoned lock is still usable: the data is only lines of text, and
    /// panicking again (for example in `Drop`, while a test is failing) would abort the process.
    fn lock(&self) -> MutexGuard<'_, LogState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits until `done` is true of the state, the log ends, or `WAIT` passes. The error is the
    /// reason, with the log; the lock is released before the caller panics on it.
    fn wait_for(&self, what: &str, done: impl Fn(&LogState) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + WAIT;
        let mut state = self.lock();
        loop {
            if done(&state) {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let reason = if state.ended {
                "the daemon's output ended"
            } else if left.is_zero() {
                "timed out"
            } else {
                state = self
                    .changed
                    .wait_timeout(state, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
                continue;
            };
            return Err(format!(
                "{reason} waiting for {what}; the log:\n{}",
                state.lines.join("\n")
            ));
        }
    }
}

/// A running `giles serve`. Killed on drop; its log is printed if the test is failing.
pub struct Daemon {
    child: Child,
    log: Arc<Log>,
}

impl Daemon {
    /// Starts `giles serve` on `home` and returns once it logs `giles started`.
    pub fn start(home: &Home) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_giles"));
        command
            .args(["serve", "--home"])
            .arg(home.root())
            .env("HOME", home.user())
            .env("TMPDIR", home.tmp())
            .env("GILES_LOG", "info")
            .env_remove("GILES_HOME")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let stderr = child.stderr.take().unwrap();
        let log = Arc::new(Log::default());
        let sink = log.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                sink.lock().lines.push(line);
                sink.changed.notify_all();
            }
            // The pipe closed: wake waiters so they fail now instead of at the deadline.
            sink.lock().ended = true;
            sink.changed.notify_all();
        });
        let daemon = Self { child, log };
        daemon.wait_for_log("giles started");
        daemon
    }

    /// The log so far.
    pub fn log(&self) -> String {
        self.log.lock().lines.join("\n")
    }

    /// Waits for a log line containing `needle`; fails at once if the daemon's output ended.
    pub fn wait_for_log(&self, needle: &str) {
        let found = self.log.wait_for(&format!("{needle:?}"), |state| {
            state.lines.iter().any(|line| line.contains(needle))
        });
        if let Err(message) = found {
            panic!("{message}");
        }
    }

    /// A crash: SIGKILL, so nothing shuts down in order.
    pub fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    /// SIGTERM, then waits for a clean exit; returns the whole log.
    pub fn stop(mut self) -> String {
        let signalled = Command::new("kill")
            .args(["-s", "TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(signalled.success());
        let deadline = Instant::now() + WAIT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "giles did not stop");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "{status}\n{}", self.log());
        // The reader thread ends with the pipe; wait for the last lines.
        if let Err(message) = self.log.wait_for("the end of the log", |state| state.ended) {
            panic!("{message}");
        }
        let log = self.log();
        assert!(log.contains("giles stopped"), "{log}");
        log
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if thread::panicking() {
            eprintln!("--- giles log ---\n{}", self.log());
        }
    }
}
