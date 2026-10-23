//! End-to-end tests of `giles serve`: data root resolution and layout, configuration loading, the
//! HTTP listener, and clean shutdown on signals. Every test points `HOME` and `TMPDIR` into a
//! temporary directory.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
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

/// A running `giles serve` whose stderr is collected line by line.
struct Daemon {
    child: Child,
    lines: Receiver<String>,
    log: Vec<String>,
}

impl Daemon {
    fn start(mut cmd: Command) -> Self {
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
        let mut daemon = Self {
            child,
            lines,
            log: Vec::new(),
        };
        daemon.wait_for_log("giles started");
        daemon
    }

    fn wait_for_log(&mut self, needle: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if self.log.iter().any(|l| l.contains(needle)) {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.log.push(line),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    let _ = self.child.kill();
                    panic!("no {needle:?} in the log:\n{}", self.log.join("\n"));
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
        "data/plugins",
        "backups",
        "run",
        "run/pids",
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
    let output = env.command(&["serve"]).env_remove("HOME").output().unwrap();
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
    let output = env
        .command(&["serve", "--home", home.to_str().unwrap()])
        .output()
        .unwrap();
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
    let env = Env::new();
    let home = env.path("home");
    write_config(
        &home,
        "[owner]\nidentities = [\"web:owner\"]\nnmae = \"me\"\n",
    );
    let output = env
        .command(&["serve", "--home", home.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = format!(
        "{}:3:1: owner.nmae: unknown field `nmae`",
        home.join("giles.toml").display()
    );
    assert!(stderr.contains(&expected), "{stderr}");
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
    let output = env
        .command(&["serve", "--home", home.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!("cannot listen for HTTP on {addr}")),
        "{stderr}"
    );
    assert!(!stderr.contains("giles started"), "{stderr}");
}

#[cfg(feature = "provider-anthropic")]
#[test]
fn a_component_that_cannot_be_built_fails_with_the_file_and_key() {
    let env = Env::new();
    let home = env.path("home");
    write_config(
        &home,
        "[owner]\nidentities = [\"web:owner\"]\n\
         [provider.anthropic]\nconfig = { api_key = \"sk-secret-key\", bogus = 1 }\n",
    );
    let output = env
        .command(&["serve", "--home", home.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = format!(
        "{}:4:1: provider.anthropic.config: ",
        home.join("giles.toml").display()
    );
    assert!(stderr.contains(&expected), "{stderr}");
    assert!(stderr.contains("bogus"), "{stderr}");
    assert!(!stderr.contains("sk-secret-key"), "{stderr}");
    assert!(!stderr.contains("giles started"), "{stderr}");
}
