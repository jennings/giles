//! End-to-end tests of `giles serve`: data root resolution and layout, configuration loading, the
//! HTTP listener, and clean shutdown on signals. Every test points `HOME` and `TMPDIR` into a
//! temporary directory.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);

/// A sandbox for one `giles` run: a fake `HOME` and `TMPDIR` under a temporary directory.
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(env.home()).unwrap();
        fs::create_dir(env.tmp()).unwrap();
        env
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn home(&self) -> PathBuf {
        self.path("user")
    }

    fn tmp(&self) -> PathBuf {
        self.path("tmp")
    }

    /// `giles <args>` with `HOME` and `TMPDIR` in the sandbox and no inherited `GILES_HOME`.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_giles"));
        cmd.args(args)
            .env("HOME", self.home())
            .env("TMPDIR", self.tmp())
            .env("GILES_LOG", "info")
            .env_remove("GILES_HOME")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        cmd
    }
}

/// Runs `cmd` (a `giles serve` expected to fail at startup, stderr piped) to its exit, and returns
/// what it printed. A daemon that starts when it shouldn't is killed after [`TIMEOUT`] instead of
/// outliving the test.
fn output_within_timeout(mut cmd: Command) -> Output {
    let mut child = cmd.spawn().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("giles did not exit; it should have failed at startup");
        }
        thread::sleep(Duration::from_millis(20));
    };
    Output {
        status,
        stdout: Vec::new(),
        stderr: reader.join().unwrap(),
    }
}

/// A running `giles serve` whose stderr is collected line by line.
struct Daemon {
    child: Child,
    lines: Receiver<String>,
    log: Vec<String>,
}

impl Daemon {
    fn start(cmd: Command) -> Self {
        let mut daemon = Self::spawn(cmd);
        daemon.wait_for_log("giles started");
        daemon
    }

    /// Starts `giles` without waiting for `giles started`.
    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd.spawn().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            log: Vec::new(),
        }
    }

    fn wait_for_log(&mut self, needle: &str) {
        self.wait_for_line(&[needle]);
    }

    /// Waits for one line of the log that contains every one of `needles`.
    fn wait_for_line(&mut self, needles: &[&str]) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if self
                .log
                .iter()
                .any(|l| needles.iter().all(|needle| l.contains(needle)))
            {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.log.push(line),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    let _ = self.child.kill();
                    panic!(
                        "no line with {needles:?} in the log:\n{}",
                        self.log.join("\n")
                    );
                }
            }
        }
    }

    /// Sends `signal` (e.g. "TERM") and waits for the process to exit.
    fn stop(mut self, signal: &str) -> (ExitStatus, String) {
        let killed = Command::new("kill")
            .args(["-s", signal, &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(killed.success());
        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("giles did not exit after SIG{signal}");
            }
            thread::sleep(Duration::from_millis(20));
        };
        self.log.extend(self.lines.iter());
        (status, self.log.join("\n"))
    }
}

/// A daemon a test abandons (a failed assertion) must not outlive it.
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn assert_layout(root: &Path) {
    for dir in [
        "agents",
        "users",
        "skills",
        "plugins",
        "data",
        "data/blobs",
        "data/channel",
        "data/provider",
        "data/toolset",
        "backups",
        "run",
        "run/pids/channel",
        "run/pids/provider",
        "run/pids/toolset",
        "run/sandbox/channel",
        "run/sandbox/provider",
        "run/sandbox/toolset",
        "cache",
    ] {
        assert!(root.join(dir).is_dir(), "missing {dir}");
    }
    assert_eq!(mode(root), 0o700);
    assert_eq!(mode(&root.join("run")), 0o700);
}

/// The smallest valid `giles.toml`, plus an HTTP listener on a free port (the default 8080 would
/// collide between tests running in parallel).
const MINIMAL_CONFIG: &str =
    "[core]\nhttp_listen = \"127.0.0.1:0\"\n\n[owner]\nidentities = [\"web:owner\"]\n";

/// Creates the data root `root` (mode 0700, as `giles serve` would) holding `giles_toml`.
fn write_config(root: &Path, giles_toml: &str) {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .unwrap();
    fs::write(root.join("giles.toml"), giles_toml).unwrap();
}

#[test]
fn sigterm_stops_cleanly_and_home_flag_beats_giles_home() {
    let env = Env::new();
    let home = env.path("flag-home");
    write_config(&home, MINIMAL_CONFIG);
    let mut cmd = env.command(&["serve", "--home", home.to_str().unwrap()]);
    cmd.env("GILES_HOME", env.path("env-home"));

    let (status, log) = Daemon::start(cmd).stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert!(
        log.contains("signal=\"SIGTERM\"") || log.contains("signal=SIGTERM"),
        "{log}"
    );
    assert!(log.contains("giles stopped"), "{log}");
    // The startup sandbox check logs one line either way (PLAN section 12.1).
    assert!(
        log.contains("plugin sandbox available") || log.contains("plugin sandbox unavailable"),
        "{log}"
    );
    assert_layout(&home);
    // The runtime opened the database and built the (empty) registry.
    assert!(home.join("data/giles.db").is_file());
    assert!(log.contains("registry ready"), "{log}");
    assert!(!env.path("env-home").exists());
    assert!(!env.home().join(".giles").exists());
}

#[test]
fn sigint_stops_cleanly_and_giles_home_beats_home() {
    let env = Env::new();
    write_config(&env.path("env-home"), MINIMAL_CONFIG);
    let mut cmd = env.command(&["serve"]);
    cmd.env("GILES_HOME", env.path("env-home"));

    let (status, log) = Daemon::start(cmd).stop("INT");
    assert!(status.success(), "{status}\n{log}");
    assert!(log.contains("giles stopped"), "{log}");
    assert_layout(&env.path("env-home"));
    assert!(!env.home().join(".giles").exists());
}

#[test]
fn default_root_is_dot_giles_in_home() {
    let env = Env::new();
    write_config(&env.home().join(".giles"), MINIMAL_CONFIG);
    let (status, log) = Daemon::start(env.command(&["serve"])).stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert_layout(&env.home().join(".giles"));
}

#[test]
fn long_root_puts_sockets_in_tmpdir() {
    let env = Env::new();
    let home = env.path(&"h".repeat(120));
    write_config(&home, MINIMAL_CONFIG);
    let cmd = env.command(&["serve", "--home", home.to_str().unwrap()]);

    let (status, log) = Daemon::start(cmd).stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert_layout(&home);
    let socket_dirs: Vec<_> = fs::read_dir(env.tmp())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(socket_dirs.len(), 1, "{socket_dirs:?}");
    let socket_dir = &socket_dirs[0];
    let name = socket_dir.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("giles-"), "{name}");
    assert_eq!(mode(socket_dir), 0o700);
    assert!(
        log.contains(name),
        "the log names the socket directory:\n{log}"
    );
}

#[test]
fn missing_home_is_a_clear_error() {
    let env = Env::new();
    let mut cmd = env.command(&["serve"]);
    cmd.env_remove("HOME");
    let output = output_within_timeout(cmd);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("neither GILES_HOME nor HOME is set"),
        "{stderr}"
    );
}

#[test]
fn missing_config_creates_the_layout_then_fails() {
    let env = Env::new();
    let home = env.path("home");
    let output = output_within_timeout(env.command(&["serve", "--home", home.to_str().unwrap()]));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let file = home.join("giles.toml");
    assert!(
        stderr.contains(&format!("{}: not found", file.display())),
        "{stderr}"
    );
    assert_layout(&home);
}

#[test]
fn invalid_config_fails_with_the_file_and_key() {
    // A typo next to the wrong-typed key doesn't change the error.
    let env = Env::new();
    let home = env.path("home");
    write_config(
        &home,
        "[owner]\nidentities = \"web:owner\"\nnmae = \"me\"\n",
    );
    let output = output_within_timeout(env.command(&["serve", "--home", home.to_str().unwrap()]));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = format!(
        "{}:2:14: owner.identities: invalid type",
        home.join("giles.toml").display()
    );
    assert!(stderr.contains(&expected), "{stderr}");
    assert!(!stderr.contains("unknown key"), "{stderr}");
}

#[test]
fn a_misspelled_sandbox_key_stops_startup() {
    let env = Env::new();
    let home = env.path("home");
    write_config(
        &home,
        "[owner]\nidentities = [\"web:owner\"]\n[toolset.t]\ncommand = \"/bin/true\"\n\
         sandbox = { netwrok = false }\n",
    );
    let output = output_within_timeout(env.command(&["serve", "--home", home.to_str().unwrap()]));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = format!(
        "{}:5:{}: toolset.t.sandbox.netwrok: unknown field `netwrok`",
        home.join("giles.toml").display(),
        "sandbox = { ".len() + 1
    );
    assert!(stderr.contains(&expected), "{stderr}");
}

#[test]
fn unknown_config_keys_are_logged_once_and_do_not_stop_startup() {
    let env = Env::new();
    let home = env.path("home");
    write_config(
        &home,
        "typo = 1\n[core]\nhttp_listen = \"127.0.0.1:0\"\nhttp_listn = 2\n\
         [owner]\nidentities = [\"web:owner\"]\nnmae = \"me\"\n\
         [toolset.t]\ncommand = \"/bin/true\"\nsandbox = { read = [\"/usr\"] }\n\
         [channel.web]\nconfig = { public_ur = \"x\" }\n\
         [provider.anthropic]\nconfig = { api_key = \"sk-x\", modle = 1 }\n\
         [provider.local]\ncomponent = \"openai\"\n\
         config = { models = { m = { contxt = 1 } } }\n",
    );
    let file = home.join("giles.toml").display().to_string();
    let (status, log) =
        Daemon::start(env.command(&["serve", "--home", home.to_str().unwrap()])).stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    for key in [
        "typo",
        "core.http_listn",
        "owner.nmae",
        "channel.web.config.public_ur",
        "provider.anthropic.config.modle",
        "provider.local.config.models.m.contxt",
    ] {
        let expected = format!("unknown key `{key}` (ignored)");
        assert_eq!(log.matches(&expected).count(), 1, "{expected}\n{log}");
    }
    assert!(
        log.contains(&format!("{file}:1:1: unknown key `typo` (ignored)")),
        "{log}"
    );
}

#[test]
fn a_remote_toolset_table_is_ignored_with_a_warning_and_its_name_is_free() {
    let env = Env::new();
    let home = env.path("home");
    let home_arg = home.to_str().unwrap();
    // The table isn't validated: `parallel` has the wrong type, and `nmae` is made up.
    write_config(
        &home,
        &format!(
            "{MINIMAL_CONFIG}\n[toolset.x]\nurl = \"http://127.0.0.1:9/old\"\n\
             parallel = \"yes\"\nnmae = 1\n"
        ),
    );
    let expected = "giles.toml: [toolset.x] is ignored: remote toolsets are managed in the web UI \
                    (Admin → MCP servers) or with giles toolset set";
    let daemon = Daemon::start(env.command(&["serve", "--home", home_arg]));

    // No toolset `x` runs.
    let (ok, stdout, stderr) = run_cli(&env, &["toolset", "list", "--home", home_arg]);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "No toolsets.\n");

    // A stored toolset may take the name.
    let spec = env.path("x.toml");
    fs::write(&spec, "url = \"http://127.0.0.1:9/mcp\"\n").unwrap();
    let spec_arg = spec.to_str().unwrap();
    let (ok, stdout, stderr) = run_cli(
        &env,
        &[
            "toolset", "set", "x", "--file", spec_arg, "--home", home_arg,
        ],
    );
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "Saved toolset `x`.\n");
    let (ok, shown, stderr) = run_cli(&env, &["toolset", "show", "x", "--home", home_arg]);
    assert!(ok, "{stderr}");
    assert!(shown.contains("/mcp") && !shown.contains("/old"), "{shown}");

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    // The warning names the table once, and nothing inside it is reported.
    let warned = log.lines().filter(|line| line.contains(expected));
    assert_eq!(warned.clone().count(), 1, "{log}");
    assert!(
        warned.into_iter().all(|line| line.contains("WARN")),
        "{log}"
    );
    assert!(
        !log.contains("nmae") && !log.contains("invalid type"),
        "{log}"
    );

    // At every start.
    let daemon = Daemon::start(env.command(&["serve", "--home", home_arg]));
    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert_eq!(log.matches(expected).count(), 1, "{log}");
    assert!(!log.contains("a stored toolset is not started"), "{log}");
}

/// The HTTP address in the `giles started` line (`http=http://<addr>/`).
fn http_addr(log: &[String]) -> SocketAddr {
    let line = log.iter().find(|l| l.contains("giles started")).unwrap();
    let url = line.split("http=http://").nth(1).unwrap();
    url.split('/').next().unwrap().parse().unwrap()
}

#[test]
fn serves_http_on_http_listen_until_stopped() {
    let env = Env::new();
    let home = env.path("home");
    write_config(&home, MINIMAL_CONFIG);
    let daemon = Daemon::start(env.command(&["serve", "--home", home.to_str().unwrap()]));
    let addr = http_addr(&daemon.log);

    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response
            .to_ascii_lowercase()
            .contains("\r\ncontent-security-policy: default-src 'self';"),
        "{response}"
    );
    assert!(response.contains("is running."), "{response}");

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert!(
        TcpStream::connect(addr).is_err(),
        "still listening after exit"
    );
}

#[test]
fn busy_http_listen_fails_at_startup() {
    let env = Env::new();
    let home = env.path("home");
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap();
    write_config(
        &home,
        &format!("[core]\nhttp_listen = \"{addr}\"\n\n[owner]\nidentities = [\"web:owner\"]\n"),
    );
    let output = output_within_timeout(env.command(&["serve", "--home", home.to_str().unwrap()]));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("cannot listen for HTTP on {addr}")),
        "{stderr}"
    );
    assert!(!stderr.contains("giles started"), "{stderr}");
}

#[test]
fn one_daemon_per_root_with_a_private_admin_socket_and_token() {
    let env = Env::new();
    let home = env.path("home");
    write_config(&home, MINIMAL_CONFIG);
    let daemon = Daemon::start(env.command(&["serve", "--home", home.to_str().unwrap()]));
    let socket = home.join("run/admin.sock");
    let token_file = home.join("run/admin.token");
    assert_eq!(mode(&socket), 0o600);
    assert_eq!(mode(&token_file), 0o600);
    let token = fs::read_to_string(&token_file).unwrap();
    assert_eq!(token.len(), 64, "{token}");
    assert_eq!(mode(&home.join("run/giles.lock")), 0o600);

    // A second daemon on the same root fails before touching anything, naming the first.
    let output = output_within_timeout(env.command(&["serve", "--home", home.to_str().unwrap()]));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let pid = daemon.child.id();
    assert!(
        stderr.contains("giles is already running") && stderr.contains(&format!("pid {pid}")),
        "{stderr}"
    );
    assert!(socket.exists(), "the running daemon's socket is left alone");
    assert_eq!(fs::read_to_string(&token_file).unwrap(), token);

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert!(!socket.exists() && !token_file.exists(), "removed at exit");

    // The lock went with the process: the next start works and gets a new token.
    let daemon = Daemon::start(env.command(&["serve", "--home", home.to_str().unwrap()]));
    assert_ne!(fs::read_to_string(&token_file).unwrap(), token);
    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
}

/// Runs `giles <args>` to completion: whether it succeeded, its stdout, and its stderr.
fn run_cli(env: &Env, args: &[&str]) -> (bool, String, String) {
    let output = env.command(args).stdout(Stdio::piped()).output().unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Records one provider request in `home`'s database, as the agent loop would.
fn record_usage(home: &Path, agent: &str, usage: giles_types::Usage) {
    fs::create_dir_all(home.join("data")).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let store = giles_store::Store::open(home.join("data/giles.db"))
                .await
                .unwrap();
            store
                .record_usage(giles_store::UsageRecord {
                    at: jiff::Timestamp::now(),
                    provider: "anthropic".into(),
                    model: "claude".into(),
                    agent: giles_types::AgentId::new(agent).unwrap(),
                    user: None,
                    session: None,
                    usage,
                    fallback: false,
                })
                .await
                .unwrap();
        });
}

#[test]
fn status_usage_and_login_link_ask_the_running_daemon() {
    let env = Env::new();
    let home = env.path("home");
    let home_arg = home.to_str().unwrap();
    write_config(
        &home,
        &format!("{MINIMAL_CONFIG}\n[channel.libera]\ncomponent = \"irc\"\n"),
    );
    record_usage(
        &home,
        "jeeves",
        giles_types::Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 300,
            cache_write_tokens: 0,
        },
    );

    // Not running yet.
    let (ok, _, stderr) = run_cli(&env, &["status", "--home", home_arg]);
    assert!(!ok);
    assert!(
        stderr.contains("giles is not running") && stderr.contains("giles serve"),
        "{stderr}"
    );

    let daemon = Daemon::start(env.command(&["serve", "--home", home_arg]));

    let (ok, stdout, stderr) = run_cli(&env, &["status", "--home", home_arg]);
    assert!(ok, "{stderr}");
    let version = format!("giles {}, running since ", env!("CARGO_PKG_VERSION"));
    for expected in [
        version.as_str(),
        "Plugin sandbox: ",
        "Channels:\n  libera  irc (plugin): no status yet\n",
        "Plugins:\n  libera (channel, irc)\n",
        // Supervised: no `giles-channel-irc` anywhere (or no sandbox here), which waits for a new
        // binary.
        "state:     failed (not retried until its binary changes or it's restarted)",
        "error:     ",
        "location:  giles-channel-irc",
    ] {
        assert!(stdout.contains(expected), "{expected:?} in:\n{stdout}");
    }

    let (ok, stdout, stderr) = run_cli(&env, &["usage", "--home", home_arg]);
    assert!(ok, "{stderr}");
    assert!(
        stdout.starts_with("Usage per agent (all time):"),
        "{stdout}"
    );
    let jeeves = stdout.lines().find(|l| l.starts_with("jeeves ")).unwrap();
    assert!(jeeves.ends_with("75.0%"), "{stdout}");
    let (ok, stdout, stderr) = run_cli(
        &env,
        &["usage", "--home", home_arg, "--since", "2099-01-01"],
    );
    assert!(ok, "{stderr}");
    assert!(
        stdout.starts_with("No provider requests since 2099-01-01 00:00:00"),
        "{stdout}"
    );
    let (ok, _, stderr) = run_cli(&env, &["usage", "--home", home_arg, "--since", "soon"]);
    assert!(!ok && stderr.contains("invalid --since"), "{stderr}");

    // No web channel yet, so no login links.
    let (ok, stdout, stderr) = run_cli(
        &env,
        &["admin", "login-link", "--home", home_arg, "--user", "owner"],
    );
    assert!(!ok && stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("web channel"), "{stderr}");

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    let (ok, _, stderr) = run_cli(&env, &["usage", "--home", home_arg]);
    assert!(!ok && stderr.contains("giles is not running"), "{stderr}");
}

/// `POST <path>` with the urlencoded `form` to `addr`; the whole response.
#[cfg(feature = "channel-web")]
fn http_post_form(addr: SocketAddr, path: &str, form: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{form}",
        form.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[cfg(feature = "channel-web")]
#[test]
fn admin_login_link_prints_a_url_that_logs_in_once() {
    let env = Env::new();
    let home = env.path("home");
    let home_arg = home.to_str().unwrap();
    // Port 0: links use the address the listener actually bound.
    write_config(&home, &format!("{MINIMAL_CONFIG}\n[channel.web]\n"));
    let daemon = Daemon::start(env.command(&["serve", "--home", home_arg]));
    let addr = http_addr(&daemon.log);

    let (ok, stdout, stderr) = run_cli(
        &env,
        &["admin", "login-link", "--home", home_arg, "--user", "owner"],
    );
    assert!(ok, "{stderr}");
    assert!(stderr.contains("The link works once, until"), "{stderr}");
    let url = stdout.trim();
    let prefix = format!("http://{addr}/login/");
    let token = url
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("{url} doesn't start with {prefix}"));

    // Opening the link shows the login page; posting its form logs in, once.
    let page = http_request(
        TcpStream::connect(addr).unwrap(),
        &format!("/login/{token}"),
        "",
    );
    assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
    let form = format!("token={token}");
    let first = http_post_form(addr, "/login", &form);
    assert!(first.starts_with("HTTP/1.1 303 "), "{first}");
    assert!(
        first.to_ascii_lowercase().contains("\r\nset-cookie: "),
        "{first}"
    );
    let again = http_post_form(addr, "/login", &form);
    assert!(!again.starts_with("HTTP/1.1 303 "), "{again}");

    // v1 has one user.
    let (ok, stdout, stderr) = run_cli(
        &env,
        &[
            "admin",
            "login-link",
            "--home",
            home_arg,
            "--user",
            "stranger",
        ],
    );
    assert!(!ok && stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("there is no user `stranger`"), "{stderr}");

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
}

#[test]
fn backup_writes_a_database_snapshot_and_an_archive_into_a_directory() {
    let env = Env::new();
    let home = env.path("home");
    let home_arg = home.to_str().unwrap();
    write_config(&home, MINIMAL_CONFIG);
    record_usage(
        &home,
        "jeeves",
        giles_types::Usage {
            input_tokens: 7,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    );
    fs::create_dir(env.path("out")).unwrap();
    // The working directory is reported with symlinks resolved (macOS `/var` is `/private/var`).
    let out = env.path("out").canonicalize().unwrap();
    // A relative directory is the CLI's, not the daemon's, working directory.
    let backup = |dir: &str| {
        let output = env
            .command(&["backup", "--home", home_arg, dir])
            .current_dir(env.dir.path())
            .stdout(Stdio::piped())
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    let (ok, _, stderr) = backup("out");
    assert!(!ok);
    assert!(stderr.contains("giles is not running"), "{stderr}");

    let daemon = Daemon::start(env.command(&["serve", "--home", home_arg]));

    let (ok, stdout, stderr) = backup("out");
    assert!(ok, "{stderr}");
    let paths: Vec<&Path> = stdout.lines().map(Path::new).collect();
    let [database, archive] = paths[..] else {
        panic!("{stdout}");
    };
    assert!(
        database.starts_with(&out) && archive.starts_with(&out),
        "{stdout}"
    );
    assert_eq!(database.extension().unwrap(), "db");
    assert_eq!(archive.extension().unwrap(), "tar");
    assert_eq!(mode(database), 0o600);
    assert_eq!(mode(archive), 0o600);
    // The copy opens as a database with the usage row the daemon had.
    let rows = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let store = giles_store::Store::open(database).await.unwrap();
            store.usage_by_model(None, None).await.unwrap()
        });
    assert_eq!(rows.len(), 1);
    assert!(
        fs::read(archive)
            .unwrap()
            .windows("giles.toml".len())
            .any(|w| w == b"giles.toml")
    );

    let (ok, stdout, stderr) = backup("missing");
    assert!(!ok && stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("does not exist"), "{stderr}");

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
}

/// The journal mode recorded in the database file: WAL is, the rollback modes read as `delete`.
fn recorded_journal_mode(home: &Path) -> String {
    rusqlite::Connection::open(home.join("data/giles.db"))
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap()
}

/// The files SQLite leaves beside the database while it uses a journal.
fn journal_files(home: &Path) -> Vec<&'static str> {
    ["-wal", "-shm", "-journal"]
        .into_iter()
        .filter(|suffix| {
            home.join(format!("data/giles.db{suffix}"))
                .try_exists()
                .unwrap()
        })
        .collect()
}

#[test]
fn journal_mode_switches_the_daemons_database_in_both_directions_across_restarts() {
    let env = Env::new();
    let home = env.path("home");
    let home_arg = home.to_str().unwrap();
    let with_mode = |mode: &str| {
        format!(
            "[core]\nhttp_listen = \"127.0.0.1:0\"\n{mode}\n[owner]\nidentities = [\"web:owner\"]\n"
        )
    };
    write_config(&home, &with_mode(""));
    // An existing database in the default mode, with a row in it.
    record_usage(
        &home,
        "jeeves",
        giles_types::Usage {
            input_tokens: 7,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    );
    assert_eq!(recorded_journal_mode(&home), "wal");
    fs::create_dir(env.path("out")).unwrap();

    // `delete` switches it with no manual step, and the daemon runs without a WAL.
    fs::write(
        home.join("giles.toml"),
        with_mode("journal_mode = \"DELETE\""),
    )
    .unwrap();
    let daemon = Daemon::start(env.command(&["serve", "--home", home_arg]));
    assert_eq!(journal_files(&home), Vec::<&str>::new());
    assert_eq!(recorded_journal_mode(&home), "delete");
    // `giles backup` still writes a snapshot that holds the data and opens in either mode.
    let output = env
        .command(&[
            "backup",
            "--home",
            home_arg,
            env.path("out").to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let database = stdout.lines().find(|l| l.ends_with(".db")).unwrap();
    for mode in [
        giles_store::JournalMode::Delete,
        giles_store::JournalMode::Wal,
    ] {
        let restored = env.path(&format!("restored-{mode}.db"));
        fs::copy(database, &restored).unwrap();
        let rows = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let options = giles_store::OpenOptions::new().journal_mode(mode);
                let store = giles_store::Store::open_with(&restored, options)
                    .await
                    .unwrap();
                store.usage_by_model(None, None).await.unwrap()
            });
        assert_eq!(rows.len(), 1, "{mode}");
    }
    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert_eq!(journal_files(&home), Vec::<&str>::new(), "{log}");
    assert_eq!(recorded_journal_mode(&home), "delete");

    // Back to the default, again with no manual step.
    fs::write(home.join("giles.toml"), with_mode("")).unwrap();
    let daemon = Daemon::start(env.command(&["serve", "--home", home_arg]));
    assert_eq!(recorded_journal_mode(&home), "wal");
    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert_eq!(journal_files(&home), Vec::<&str>::new(), "{log}");
    assert_eq!(recorded_journal_mode(&home), "wal");
}

#[test]
fn a_journal_mode_that_gives_up_the_journal_is_refused_at_startup() {
    let env = Env::new();
    let home = env.path("home");
    for mode in ["memory", "OFF", "wal2"] {
        write_config(
            &home,
            &format!(
                "[core]\njournal_mode = \"{mode}\"\n\n[owner]\nidentities = [\"web:owner\"]\n"
            ),
        );
        let output =
            output_within_timeout(env.command(&["serve", "--home", home.to_str().unwrap()]));
        assert!(!output.status.success(), "{mode}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let file = home.join("giles.toml");
        assert!(
            stderr.contains(&format!("{}:2:1: core.journal_mode: ", file.display())),
            "{mode}: {stderr}"
        );
        assert!(stderr.contains("\"delete\""), "{mode}: {stderr}");
        assert!(!stderr.contains("giles started"), "{mode}: {stderr}");
        // Nothing opened the database.
        assert!(!home.join("data/giles.db").exists(), "{mode}");
    }
}

#[cfg(feature = "provider-anthropic")]
#[test]
fn a_component_that_cannot_be_built_fails_with_the_file_and_key() {
    let env = Env::new();
    let home = env.path("home");
    write_config(
        &home,
        "[owner]\nidentities = [\"web:owner\"]\n\
         [provider.anthropic]\nconfig = { api_key = \"sk-secret-key\", max_tokens = 0 }\n",
    );
    let output = output_within_timeout(env.command(&["serve", "--home", home.to_str().unwrap()]));
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = format!(
        "{}:4:1: provider.anthropic.config: ",
        home.join("giles.toml").display()
    );
    assert!(stderr.contains(&expected), "{stderr}");
    assert!(stderr.contains("`max_tokens`"), "{stderr}");
    assert!(!stderr.contains("sk-secret-key"), "{stderr}");
    assert!(!stderr.contains("giles started"), "{stderr}");
}

/// Installs `body` as the `/bin/sh` plugin `giles-channel-<component>` in `home`.
fn install_script_plugin(home: &Path, component: &str, body: &str) {
    let plugins = home.join("plugins");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&plugins)
        .unwrap();
    let path = plugins.join(format!("giles-channel-{component}"));
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn plugins_that_fail_to_launch_are_supervised_and_giles_runs() {
    let env = Env::new();
    let home = env.path("home");
    write_config(
        &home,
        &format!(
            "{MINIMAL_CONFIG}\n[channel.flaky]\nunsandboxed = true\n\n[channel.boxed]\n\
             sandbox = {{ min_abi = 1000 }}\n"
        ),
    );
    install_script_plugin(
        &home,
        "flaky",
        "echo 'flaky: cannot reach the network' >&2; exit 1",
    );
    // Sandboxed, through the real trampoline, which refuses: on Linux no kernel has Landlock
    // ABI 1000 (or Landlock is missing), and on macOS (which ignores `min_abi`) the sandbox has
    // no `execute` grant for `/bin/sh`.
    install_script_plugin(&home, "boxed", "exit 0");
    let daemon = Daemon::start(env.command(&["serve", "--home", home.to_str().unwrap()]));
    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    // The plugin's stderr under the instance's span, the unsandboxed warning, and the failure.
    assert!(
        log.contains(
            "plugin{kind=channel instance=flaky}: giles_core::plugin_host::stderr: flaky: cannot reach the network"
        ),
        "{log}"
    );
    assert!(
        log.contains(
            "plugin{kind=channel instance=flaky}: giles_core::plugin_host: running the plugin unsandboxed"
        ),
        "{log}"
    );
    // A plugin that exits is relaunched with backoff; a sandbox failure waits for a new binary.
    let flaky = log.lines().find(|line| {
        line.contains("cannot launch the plugin; retrying after a delay")
            && line.contains("supervisor{kind=channel instance=flaky}")
    });
    assert!(
        flaky.is_some_and(|line| line.contains("retry_in=1s")),
        "{log}"
    );
    let boxed = log.lines().find(|line| {
        line.contains("cannot launch the plugin; not retrying until its binary changes")
            && line.contains("supervisor{kind=channel instance=boxed}")
    });
    assert!(boxed.is_some(), "{log}");
    assert!(!home.join("run/pids/channel/flaky.pid").exists());
    assert!(!home.join("run/pids/channel/boxed.pid").exists());
}

#[test]
fn toolsets_that_fail_to_start_leave_giles_running_and_retrying() {
    let env = Env::new();
    let home = env.path("home");
    let script = env.path("broken-server");
    fs::write(
        &script,
        "#!/bin/sh\necho 'broken: no configuration' >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    // The remote toolset is a stored one.
    write_config(
        &home,
        &format!(
            "{MINIMAL_CONFIG}\n[toolset.broken]\ncommand = \"{}\"\nunsandboxed = true\n",
            script.display()
        ),
    );
    store_toolsets(
        &home,
        &[("remote", r#"{ "url": "http://127.0.0.1:9/mcp" }"#)],
    );
    let daemon = Daemon::start(env.command(&["serve", "--home", home.to_str().unwrap()]));
    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert!(
        log.contains("mcp{instance=broken}: giles_core::mcp::stderr: broken: no configuration"),
        "{log}"
    );
    for instance in ["broken", "remote"] {
        let failed = log.lines().find(|line| {
            line.contains(&format!("toolset{{instance={instance}}}"))
                && line.contains("cannot start the toolset; retrying")
        });
        assert!(failed.is_some(), "{instance}\n{log}");
    }
    // Both were registered before the summary, so neither is pending.
    assert!(!log.contains("instance not started"), "{log}");
    assert!(log.contains("giles stopped"), "{log}");
}

/// Stores toolsets in `home`'s database, as the web UI would.
fn store_toolsets(home: &Path, toolsets: &[(&str, &str)]) {
    fs::create_dir_all(home.join("data")).unwrap();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let store = giles_store::Store::open(home.join("data/giles.db"))
                .await
                .unwrap();
            for (name, spec) in toolsets {
                store
                    .toolset_insert(name, spec, jiff::Timestamp::now())
                    .await
                    .unwrap();
            }
        });
}

#[test]
fn stored_toolsets_start_or_are_reported_failed_and_giles_runs() {
    let env = Env::new();
    let home = env.path("home");
    let home_arg = home.to_str().unwrap();
    write_config(
        &home,
        "[core]\nhttp_listen = \"127.0.0.1:0\"\n\n\
         [owner]\nidentities = [\"web:owner\"]\n\n[channel.web]\n",
    );
    // Nothing listens on the discard port, so the valid one starts, fails, and is retried.
    let unreachable = r#"{ "url": "http://127.0.0.1:9/mcp" }"#;
    store_toolsets(
        &home,
        &[
            // Valid: started (and retried, as in the test above).
            ("stored-ok", unreachable),
            // A token from the environment: a stored toolset's comes from `secrets.toml`.
            (
                "stored-bad",
                r#"{ "url": "http://127.0.0.1:9/mcp", "auth": { "bearer": { "env": "TOKEN" } } }"#,
            ),
            // A name that `giles.toml` uses.
            ("web", unreachable),
        ],
    );

    // Giles runs, though two of the three can't. The valid one is registered with the others
    // before the summary (nothing is pending), and is started and retried.
    let mut daemon = Daemon::start(env.command(&["serve", "--home", home_arg]));
    daemon.wait_for_line(&[
        "toolset{instance=stored-ok}",
        "cannot start the toolset; retrying",
    ]);
    let (ok, stdout, stderr) = run_cli(&env, &["status", "--home", home_arg]);
    assert!(ok, "{stderr}");

    // The valid one was started, so it is supervised and not failed.
    let ok_block = stdout.split("stored-ok (toolset)\n").nth(1).expect(&stdout);
    let state = ok_block.lines().next().unwrap();
    assert!(state.starts_with("    state:"), "{stdout}");
    assert!(!state.contains("failed"), "{stdout}");
    // The others are reported failed, with the reason.
    for (instance, reason) in [
        ("stored-bad", "auth.bearer: must be { secret = \"<name>\" }"),
        ("web", "the name is taken by `[channel.web]` in giles.toml"),
    ] {
        let block = stdout
            .split(&format!("{instance} (toolset)\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("{instance} in:\n{stdout}"));
        let mut lines = block.lines();
        let state = lines.next().unwrap();
        assert!(
            state.starts_with("    state:") && state.contains("failed"),
            "{stdout}"
        );
        let error = lines.next().unwrap();
        assert!(
            error.starts_with("    error:") && error.contains(reason),
            "{stdout}"
        );
    }

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert!(!log.contains("instance not started"), "{log}");
    let summary = log.lines().find(|line| line.contains("registry ready"));
    let summary = summary.unwrap_or_else(|| panic!("no registry summary in:\n{log}"));
    assert!(
        summary.contains("toolsets=1") && summary.contains("pending=0"),
        "{summary}"
    );
    for instance in ["stored-bad", "web"] {
        let reported = log.lines().any(|line| {
            line.contains("a stored toolset is not started")
                && line.contains(&format!("toolset={instance}"))
        });
        assert!(reported, "{instance}\n{log}");
    }
    assert!(log.contains("giles stopped"), "{log}");
}

#[test]
fn a_shutdown_during_plugin_launch_kills_the_plugin_and_exits_promptly() {
    let env = Env::new();
    let home = env.path("home");
    write_config(
        &home,
        &format!("{MINIMAL_CONFIG}\n[channel.mute]\nunsandboxed = true\n"),
    );
    // Never prints a handshake line, so the launch would wait the full 10 s.
    install_script_plugin(&home, "mute", "exec sleep 300");
    let mut daemon = Daemon::spawn(env.command(&["serve", "--home", home.to_str().unwrap()]));
    daemon.wait_for_log("plugin process started");
    let started = daemon
        .log
        .iter()
        .find(|l| l.contains("plugin process started"))
        .unwrap();
    let pid = started
        .split("pid=")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let start = Instant::now();
    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}\n{log}",
        start.elapsed()
    );
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let ps = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&ps.stdout);
        if stat.trim().is_empty() || stat.trim_start().starts_with('Z') {
            break;
        }
        assert!(Instant::now() < deadline, "the plugin {pid} still runs");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!home.join("run/pids/channel/mute.pid").exists());
}

/// `GET <path>` over `stream`, with `headers`; the whole response.
#[cfg(feature = "channel-web")]
fn http_request(mut stream: impl Read + Write, path: &str, headers: &str) -> String {
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: giles\r\n{headers}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

#[cfg(feature = "channel-web")]
#[test]
fn the_web_channel_replaces_the_home_page_and_trusts_only_the_proxy_socket() {
    use std::os::unix::net::UnixStream;

    let env = Env::new();
    let home = env.path("home");
    write_config(
        &home,
        &format!("{MINIMAL_CONFIG}[channel.web.config.proxy]\nidentity_header = \"x-user\"\n"),
    );
    let daemon = Daemon::start(env.command(&["serve", "--home", home.to_str().unwrap()]));
    let addr = http_addr(&daemon.log);
    let line = daemon
        .log
        .iter()
        .find(|l| l.contains("trusted-proxy listener open"))
        .unwrap();
    let socket = PathBuf::from(line.split("at=").nth(1).unwrap().trim());
    assert_eq!(mode(&socket), 0o600);

    // The chat list needs a login, and the main listener ignores the identity header.
    let response = http_request(TcpStream::connect(addr).unwrap(), "/", "x-user: owner\r\n");
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    assert!(response.contains("giles admin login-link"), "{response}");

    // The proxy socket believes it.
    let response = http_request(
        UnixStream::connect(&socket).unwrap(),
        "/",
        "x-user: owner\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(response.contains("<h1>Chats</h1>"), "{response}");
    let response = http_request(UnixStream::connect(&socket).unwrap(), "/", "");
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");

    // The admin pages are served from the daemon's own admin service, behind the same login.
    let response = http_request(TcpStream::connect(addr).unwrap(), "/admin", "");
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    let response = http_request(
        UnixStream::connect(&socket).unwrap(),
        "/admin",
        "x-user: owner\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    let version = format!("giles {}", env!("CARGO_PKG_VERSION"));
    assert!(response.contains(&version), "{response}");
    assert!(response.contains(r#"id="channel-web""#), "{response}");

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert!(!socket.exists(), "the socket outlived the daemon");
}

/// A fake OpenAI-compatible provider on a loopback port: `GET /v1/models` lists `test-model`, and
/// every `POST /v1/chat/completions` streams `reply`. Returns the base URL.
#[cfg(feature = "channel-web")]
fn fake_openai(reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let (content_type, body) = if request_line.starts_with("GET /v1/models") {
                    (
                        "application/json",
                        r#"{"object":"list","data":[{"id":"test-model","object":"model"}]}"#
                            .to_owned(),
                    )
                } else {
                    let chunk = |delta: &str, finish: &str| {
                        format!(
                            "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\
                             \"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n"
                        )
                    };
                    let body = [
                        chunk(r#"{"role":"assistant","content":""}"#, "null"),
                        chunk(&format!(r#"{{"content":"{reply}"}}"#), "null"),
                        chunk("{}", "\"stop\""),
                        "data: [DONE]\n\n".to_owned(),
                    ]
                    .concat();
                    ("text/event-stream", body)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    url
}

/// `POST <path>` with the urlencoded `form` over `stream`, with `headers`; the whole response.
#[cfg(feature = "channel-web")]
fn http_post(mut stream: impl Read + Write, path: &str, headers: &str, form: &str) -> String {
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: giles\r\n{headers}Connection: close\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{form}",
        form.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

/// A response's `Location` header.
#[cfg(feature = "channel-web")]
fn location(response: &str) -> &str {
    response
        .lines()
        .find_map(|l| {
            l.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("location"))
        })
        .map(|(_, value)| value.trim())
        .unwrap_or_else(|| panic!("no Location in {response}"))
}

/// End to end: the owner creates an agent on the web, then chats with it, and its
/// (fake) provider answers, all without a restart.
#[cfg(all(feature = "channel-web", feature = "provider-openai"))]
#[test]
fn an_agent_created_on_the_web_answers_in_a_web_chat_without_a_restart() {
    use std::os::unix::net::UnixStream;

    let env = Env::new();
    let home = env.path("home");
    let base_url = fake_openai("Very good, sir.");
    write_config(
        &home,
        &format!(
            "{MINIMAL_CONFIG}[provider.fake]\ncomponent = \"openai\"\n\
             config = {{ base_url = \"{base_url}\" }}\n\
             [channel.web.config.proxy]\nidentity_header = \"x-user\"\n"
        ),
    );
    let daemon = Daemon::start(env.command(&["serve", "--home", home.to_str().unwrap()]));
    let line = daemon
        .log
        .iter()
        .find(|l| l.contains("trusted-proxy listener open"))
        .unwrap();
    let socket = PathBuf::from(line.split("at=").nth(1).unwrap().trim());
    let proxy = || UnixStream::connect(&socket).unwrap();
    const OWNER: &str = "x-user: owner\r\n";

    // The form offers the provider's model; its CSRF token is the login's.
    let page = http_request(proxy(), "/admin/agents/new", OWNER);
    assert!(page.starts_with("HTTP/1.1 200 "), "{page}");
    assert!(
        page.contains(r#"<option value="fake/test-model">"#),
        "{page}"
    );
    let csrf = page
        .split(r#"name="csrf" value=""#)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_owned();

    let created = http_post(
        proxy(),
        "/admin/agents",
        OWNER,
        &format!("csrf={csrf}&id=butler&name=Butler&models=fake%2Ftest-model&soul=Be+brief."),
    );
    assert!(created.starts_with("HTTP/1.1 303 "), "{created}");
    assert_eq!(location(&created), "/admin/agents/butler");
    assert_eq!(
        fs::read_to_string(home.join("agents/butler/SOUL.md")).unwrap(),
        "Be brief."
    );

    let chat = http_request(proxy(), "/chats/new?agent=butler", OWNER);
    assert!(chat.starts_with("HTTP/1.1 303 "), "{chat}");
    let chat = location(&chat).to_owned();
    let posted = http_post(
        proxy(),
        &format!("{chat}/messages"),
        OWNER,
        &format!("csrf={csrf}&text=Tea%2C+please."),
    );
    assert!(posted.starts_with("HTTP/1.1 303 "), "{posted}");

    let deadline = Instant::now() + TIMEOUT;
    loop {
        let page = http_request(proxy(), &chat, OWNER);
        if page.contains("Very good, sir.") {
            assert!(page.contains("with Butler (butler)"), "{page}");
            break;
        }
        assert!(Instant::now() < deadline, "no reply:\n{page}");
        thread::sleep(Duration::from_millis(100));
    }

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    assert!(log.contains("agent created"), "{log}");
}

/// Runs `giles <args>` to completion with `input` on its standard input.
fn run_cli_with_stdin(env: &Env, args: &[&str], input: &str) -> (bool, String, String) {
    let mut child = env
        .command(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn toolset_commands_list_show_set_remove_rename_and_restart_toolsets_of_the_running_daemon() {
    let env = Env::new();
    let home = env.path("home");
    let home_arg = home.to_str().unwrap();
    write_config(
        &home,
        &format!("{MINIMAL_CONFIG}\n[toolset.docs]\ncommand = \"/nonexistent/docs\"\n"),
    );
    // `giles toolset <args> --home <root>`: whether it succeeded, its stdout, and its stderr.
    let toolset = |args: &[&str]| {
        let mut all = vec!["toolset"];
        all.extend_from_slice(args);
        all.extend(["--home", home_arg]);
        run_cli(&env, &all)
    };
    let write_spec = |name: &str, text: &str| {
        let path = env.path(name);
        fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    };

    let (ok, _, stderr) = toolset(&["list"]);
    assert!(!ok && stderr.contains("giles is not running"), "{stderr}");

    let daemon = Daemon::start(env.command(&["serve", "--home", home_arg]));

    // The toolset of giles.toml is listed and can't be shown, changed, or removed.
    let (ok, stdout, stderr) = toolset(&["list"]);
    assert!(ok, "{stderr}");
    assert!(stdout.starts_with("NAME  SOURCE"), "{stdout}");
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("docs") && line.contains("giles.toml")),
        "{stdout}"
    );
    let docs = write_spec("docs.toml", "url = \"http://127.0.0.1:9/docs\"\n");
    for args in [
        &["show", "docs"][..],
        &["set", "docs", "--file", docs.as_str()],
        &["remove", "docs"],
    ] {
        let (ok, stdout, stderr) = toolset(args);
        assert!(!ok && stdout.is_empty(), "{args:?}: {stdout}");
        assert!(stderr.contains("giles.toml"), "{args:?}: {stderr}");
    }

    // A spec the daemon refuses is reported with its key, and nothing is stored.
    let files_spec = "url = \"http://127.0.0.1:9/docs\"\ndeadline = \"90s\"\nparallel = true\n\n\
                      [auth]\nbearer = { secret = \"files-token\" }\n";
    let files = write_spec("files.toml", files_spec);
    let (ok, stdout, stderr) = toolset(&["set", "files", "--file", files.as_str()]);
    assert!(!ok && stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("auth.bearer: "), "{stderr}");
    for (text, expected) in [
        // A stdio server is configured in giles.toml, and a stored token is a secret.
        (
            "command = \"/nonexistent/files\"\n",
            "url: a stored toolset is a remote server",
        ),
        (
            "url = \"http://127.0.0.1:9/docs\"\n\n[auth]\nbearer = { env = \"HOME\" }\n",
            "auth.bearer: must be { secret = \"<name>\" }",
        ),
        ("ulr = \"http://127.0.0.1:9/docs\"\n", "ulr: unknown key"),
        ("url = \n", "invalid TOML at line 1"),
        ("skills = [\"/a\", 2]\n", "skills[1]: "),
    ] {
        let bad = write_spec("bad.toml", text);
        let (ok, stdout, stderr) = toolset(&["set", "files", "--file", bad.as_str()]);
        assert!(!ok && stdout.is_empty(), "{text}: {stdout}");
        assert!(stderr.contains(expected), "{text}: {stderr}");
    }
    let missing = env.path("missing.toml");
    let (ok, _, stderr) = toolset(&["set", "files", "--file", missing.to_str().unwrap()]);
    assert!(!ok && stderr.contains("cannot read"), "{stderr}");
    let (ok, stdout, _) = toolset(&["list"]);
    assert!(ok && !stdout.contains("files"), "{stdout}");

    // Giles reads secrets.toml again, so the same spec is accepted once the secret exists.
    fs::write(
        home.join("secrets.toml"),
        "files-token = \"s3cr3t-value-42\"\n",
    )
    .unwrap();
    fs::set_permissions(home.join("secrets.toml"), fs::Permissions::from_mode(0o600)).unwrap();
    let (ok, stdout, stderr) = toolset(&["set", "files", "--file", files.as_str()]);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "Saved toolset `files`.\n");

    // `show` prints the spec with the references as written, after the `updated_at` comment.
    let (ok, shown, stderr) = toolset(&["show", "FILES"]);
    assert!(ok, "{stderr}");
    assert!(shown.starts_with("# updated_at = "), "{shown}");
    for expected in [
        "url = \"http://127.0.0.1:9/docs\"",
        "parallel = true",
        "secret = \"files-token\"",
    ] {
        assert!(shown.contains(expected), "{expected} in:\n{shown}");
    }
    assert!(!shown.contains("s3cr3t"), "{shown}");
    let (ok, listed, stderr) = toolset(&["list"]);
    assert!(ok, "{stderr}");
    assert!(
        listed
            .lines()
            .any(|line| line.starts_with("files") && line.contains("stored")),
        "{listed}"
    );

    // show, edit, set: the file `show` wrote is a spec, and it carries the time it was read at.
    let edited = shown.replace("/docs", "/other");
    let edited_file = write_spec("edited.toml", &edited);
    let (ok, stdout, stderr) = toolset(&["set", "files", "--file", edited_file.as_str()]);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "Saved toolset `files`.\n");
    let (ok, after, stderr) = toolset(&["show", "files"]);
    assert!(ok, "{stderr}");
    assert!(
        after.contains("url = \"http://127.0.0.1:9/other\""),
        "{after}"
    );
    assert_ne!(after.lines().next(), shown.lines().next(), "{after}");

    // The same file again is stale: the toolset changed since `show` printed it.
    let (ok, stdout, stderr) = toolset(&["set", "files", "--file", edited_file.as_str()]);
    assert!(!ok && stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("was changed since it was read"), "{stderr}");
    let (_, unchanged, _) = toolset(&["show", "files"]);
    assert_eq!(unchanged, after);
    // A file without the comment replaces whatever is there.
    let (ok, stdout, stderr) = toolset(&["set", "Files", "--file", files.as_str()]);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "Saved toolset `files`.\n");

    // A new name creates a toolset, from standard input too. A spec that `show` printed names the
    // time of the toolset it was copied from, which this one doesn't have.
    let (ok, stdout, stderr) = run_cli_with_stdin(
        &env,
        &["toolset", "set", "notes", "--file", "-", "--home", home_arg],
        "url = \"http://127.0.0.1:9/mcp\"\n",
    );
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "Saved toolset `notes`.\n");
    let (ok, stdout, stderr) = run_cli_with_stdin(
        &env,
        &["toolset", "set", "copy", "--file", "-", "--home", home_arg],
        &shown,
    );
    assert!(!ok && stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("there is no toolset called `copy`")
            && stderr.contains("remove the `# updated_at` comment"),
        "{stderr}"
    );
    let (ok, listed, stderr) = toolset(&["list"]);
    assert!(ok, "{stderr}");
    let names: Vec<&str> = listed
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(names, ["docs", "files", "notes"], "{listed}");

    // Restart goes to the supervisor of the toolset.
    let (ok, stdout, stderr) = toolset(&["restart", "NOTES"]);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "Restarting toolset `notes`.\n");
    let (ok, _, stderr) = toolset(&["restart", "ghost"]);
    assert!(!ok && stderr.contains("there is no toolset called `ghost`"));

    // Removal frees the name, and names the toolset as it was stored.
    let (ok, stdout, stderr) = toolset(&["remove", "FILES"]);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "Removed toolset `files`.\n");
    let (ok, _, stderr) = toolset(&["show", "files"]);
    assert!(!ok && stderr.contains("there is no toolset called `files`"));
    let (ok, _, stderr) = toolset(&["remove", "files"]);
    assert!(!ok && stderr.contains("there is no toolset called `files`"));
    let (ok, stdout, stderr) = toolset(&["set", "files", "--file", files.as_str()]);
    assert!(ok, "{stderr}");
    assert_eq!(stdout, "Saved toolset `files`.\n");

    // A rename names the toolset before and after, and says no agent's globs changed. Both names
    // are as stored.
    let (ok, stdout, stderr) = toolset(&["rename", "FILES", "papers"]);
    assert!(ok, "{stderr}");
    assert_eq!(
        stdout,
        "Renamed toolset `files` to `papers`.\n\
         No agent has tool globs for it.\n\
         Sessions that are running keep the old tool names; new sessions use the new ones.\n"
    );
    let (ok, listed, stderr) = toolset(&["list"]);
    assert!(ok, "{stderr}");
    let names: Vec<&str> = listed
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(names, ["docs", "notes", "papers"], "{listed}");
    let (ok, shown, stderr) = toolset(&["show", "papers"]);
    assert!(ok, "{stderr}");
    assert!(
        shown.contains("url = \"http://127.0.0.1:9/docs\""),
        "{shown}"
    );

    // A refused rename prints its reason and changes nothing.
    for (args, reason) in [
        (
            &["rename", "papers", "docs"],
            "the name is taken by `[toolset.docs]` in giles.toml",
        ),
        (
            &["rename", "papers", "NOTES"],
            "there is already a toolset called `NOTES`",
        ),
        (
            &["rename", "papers", "not a name"],
            "use only letters, digits, and `-`",
        ),
        (
            &["rename", "ghost", "other"],
            "there is no toolset called `ghost`",
        ),
        (&["rename", "docs", "other"], "stop Giles first"),
    ] {
        let (ok, stdout, stderr) = toolset(args);
        assert!(!ok && stdout.is_empty(), "{args:?}: {stdout}");
        assert!(stderr.contains(reason), "{args:?}: {stderr}");
    }
    let (ok, listed, stderr) = toolset(&["list"]);
    assert!(ok, "{stderr}");
    let names: Vec<&str> = listed
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert_eq!(names, ["docs", "notes", "papers"], "{listed}");

    let (status, log) = daemon.stop("TERM");
    assert!(status.success(), "{status}\n{log}");
    let (ok, _, stderr) = toolset(&["list"]);
    assert!(!ok && stderr.contains("giles is not running"), "{stderr}");

    // With Giles stopped, rename works on the data root: a stored toolset is renamed as its row,
    // and a toolset of giles.toml, which the running daemon refused, as its table.
    let (ok, stdout, stderr) = toolset(&["rename", "papers", "memos"]);
    assert!(ok, "{stderr}");
    assert!(
        stdout.starts_with("Renamed toolset instance \"papers\" to \"memos\".\n")
            && stdout.contains("database: the stored toolset \"papers\" is now \"memos\""),
        "{stdout}"
    );
    let (ok, stdout, stderr) = toolset(&["rename", "docs", "manuals"]);
    assert!(ok, "{stderr}");
    assert!(
        stdout.contains("giles.toml: [toolset.docs] -> [toolset.manuals]"),
        "{stdout}"
    );
    let config = fs::read_to_string(home.join("giles.toml")).unwrap();
    assert!(config.contains("[toolset.manuals]"), "{config}");
    assert!(!config.contains("[toolset.docs]"), "{config}");
    let (ok, _, stderr) = toolset(&["rename", "papers", "other"]);
    assert!(
        !ok && stderr.contains("no toolset named \"papers\""),
        "{stderr}"
    );
}
