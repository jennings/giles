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
        "run/sandbox",
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
    let output = env
        .command(&["serve", "--home", home.to_str().unwrap()])
        .output()
        .unwrap();
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
            "plugin{instance=flaky}: giles_core::plugin_host::stderr: flaky: cannot reach the network"
        ),
        "{log}"
    );
    assert!(
        log.contains(
            "plugin{instance=flaky}: giles_core::plugin_host: running the plugin unsandboxed"
        ),
        "{log}"
    );
    // A plugin that exits is relaunched with backoff; a sandbox failure waits for a new binary.
    let flaky = log.lines().find(|line| {
        line.contains("cannot launch the plugin; retrying after a delay")
            && line.contains("supervisor{instance=flaky}")
    });
    assert!(
        flaky.is_some_and(|line| line.contains("retry_in=1s")),
        "{log}"
    );
    let boxed = log.lines().find(|line| {
        line.contains("cannot launch the plugin; not retrying until its binary changes")
            && line.contains("supervisor{instance=boxed}")
    });
    assert!(boxed.is_some(), "{log}");
    assert!(!home.join("run/pids/flaky.pid").exists());
    assert!(!home.join("run/pids/boxed.pid").exists());
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
    write_config(
        &home,
        &format!(
            "{MINIMAL_CONFIG}\n[toolset.broken]\ncommand = \"{}\"\nunsandboxed = true\n\n\
             [toolset.remote]\nurl = \"http://127.0.0.1:9/mcp\"\n",
            script.display()
        ),
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
    assert!(!home.join("run/pids/mute.pid").exists());
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
