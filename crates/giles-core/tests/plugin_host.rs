//! The plugin host's launch failures, with small shell scripts as plugins (and as a fake sandbox
//! trampoline): handshake timeout, malformed line, early exits (76, 125), stderr capture and
//! scrubbing, process-group kill, and orphan cleanup. The echo plugin's end-to-end launches are in
//! `plugins/giles-channel-echo/tests/`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use giles_core::config::{Builtins, ChannelInstance, Config, LoadOptions};
use giles_core::paths::DataRoot;
use giles_core::plugin_host::{LaunchError, Launcher};
use giles_core::sandbox::Spec;

/// A data root with `giles.toml` declaring the channel plugins `channels` (TOML tables).
struct Home {
    _dir: tempfile::TempDir,
    root: DataRoot,
}

impl Home {
    fn new(channels: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("home"), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        fs::write(
            root.config_file(),
            format!("[owner]\nidentities = [\"web:owner\"]\n\n{channels}"),
        )
        .unwrap();
        Self { _dir: dir, root }
    }

    fn config(&self) -> Config {
        let env = |name: &str| (name == "TEST_SECRET").then(|| "s3cret-value-42".into());
        Config::load_with(
            &self.root,
            &LoadOptions {
                builtins: Builtins::V1,
                env: &env,
                giles_exe: None,
            },
        )
        .unwrap()
    }

    fn channel(&self, name: &str) -> ChannelInstance {
        self.config().channels[name].clone()
    }

    /// Installs `body` as the `/bin/sh` plugin `giles-channel-<component>`.
    fn script(&self, component: &str, body: &str) -> PathBuf {
        let path = self
            .root
            .plugins_dir()
            .join(format!("giles-channel-{component}"));
        write_script(&path, body);
        path
    }

    fn launcher(&self) -> Launcher {
        self.launcher_with(PathBuf::from("/nonexistent/giles"), Ok(()))
            .handshake_timeout(Duration::from_millis(500))
    }

    fn launcher_with(&self, trampoline: PathBuf, sandbox: Result<(), String>) -> Launcher {
        Launcher::new(self.root.clone(), trampoline, sandbox).env(|_| None)
    }

    fn scratch(&self, name: &str) -> PathBuf {
        self.root.root().parent().unwrap().join(name)
    }
}

fn write_script(path: &Path, body: &str) {
    let mut file = fs::File::create(path).unwrap();
    writeln!(file, "#!/bin/sh\n{body}").unwrap();
    drop(file);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// An unsandboxed plugin channel instance `name`.
fn unsandboxed(name: &str) -> String {
    format!(
        "[channel.{name}]\nunsandboxed = true\nconfig = {{ token = {{ env = \"TEST_SECRET\" }} }}\n\n"
    )
}

async fn launch_err(home: &Home, launcher: &Launcher, name: &str) -> LaunchError {
    match launcher.launch_channel(&home.channel(name)).await {
        Ok(plugin) => panic!("launched {:?}", plugin.instance),
        Err(error) => error,
    }
}

/// Whether `pid` is gone (or a zombie, which only its parent can reap).
fn gone(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    stat.trim().is_empty() || stat.trim_start().starts_with('Z')
}

async fn wait_gone(pid: u32) {
    let start = Instant::now();
    while !gone(pid) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{pid} still runs"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The log of this test process, shared by every test (a thread-scoped subscriber would miss
/// events whose callsites another test's thread registered first).
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Log {
    fn global() -> &'static Log {
        static LOG: OnceLock<Log> = OnceLock::new();
        LOG.get_or_init(|| {
            let log = Log::default();
            let writer = log.clone();
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .with_writer(move || writer.clone())
                .init();
            log
        })
    }

    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn no_handshake_line_times_out_and_kills_the_plugin() {
    let home = Home::new(&unsandboxed("mute"));
    let started = home.scratch("started");
    home.script(
        "mute",
        &format!(
            "echo $$ > {}; echo waiting >&2; exec sleep 300",
            started.display()
        ),
    );
    let error = launch_err(&home, &home.launcher(), "mute").await;
    assert!(
        matches!(error, LaunchError::HandshakeTimeout { .. }),
        "{error}"
    );
    assert!(error.is_retryable());
    assert!(error.to_string().ends_with(": waiting"), "{error}");
    let pid: u32 = fs::read_to_string(started).unwrap().trim().parse().unwrap();
    wait_gone(pid).await;
    assert!(!home.root.pid_file("mute").unwrap().exists());
}

#[tokio::test]
async fn a_malformed_handshake_line_fails_the_launch() {
    let home = Home::new(&unsandboxed("garbled"));
    home.script(
        "garbled",
        "echo '1|1|tcp|127.0.0.1:80|grpc'; exec sleep 300",
    );
    let error = launch_err(&home, &home.launcher(), "garbled").await;
    assert!(matches!(error, LaunchError::Handshake { .. }), "{error}");
    assert!(
        error.to_string().contains("unsupported transport"),
        "{error}"
    );
    assert!(error.is_retryable());
}

#[tokio::test]
async fn a_plugin_choosing_an_unoffered_protocol_is_refused() {
    let home = Home::new(&unsandboxed("future"));
    home.script("future", "echo '1|2|fd|3|grpc'; exec sleep 300");
    let error = launch_err(&home, &home.launcher(), "future").await;
    assert!(
        matches!(error, LaunchError::Protocol { chosen: 2 }),
        "{error}"
    );
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn exit_76_is_no_shared_protocol_and_not_retryable() {
    let home = Home::new(&unsandboxed("old"));
    home.script(
        "old",
        "echo \"plugin speaks 7, core offers $GILES_PLUGIN_PROTOCOLS\" >&2; exit 76",
    );
    let error = launch_err(&home, &home.launcher(), "old").await;
    assert!(
        matches!(error, LaunchError::NoSharedProtocol { .. }),
        "{error}"
    );
    assert!(!error.is_retryable());
    assert!(
        error
            .to_string()
            .ends_with(": plugin speaks 7, core offers 1"),
        "{error}"
    );
}

#[tokio::test]
async fn an_unsandboxed_plugin_is_launched_with_the_socket_subcommand_alone() {
    let home = Home::new(&unsandboxed("argv"));
    home.script("argv", "echo \"$# $1\" >&2; exit 3");
    let error = launch_err(&home, &home.launcher(), "argv").await;
    let LaunchError::Exited { stderr, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(stderr[0], "1 socket");
}

#[tokio::test]
async fn an_early_exit_logs_scrubbed_stderr_under_the_instance_span() {
    let log = Log::global();
    let home = Home::new(&unsandboxed("leaky"));
    // The plugin reads the token from stdin and prints it with the config secret; it never sees
    // the secret in its environment, so the script carries it.
    home.script(
        "leaky",
        "read token; echo \"token=$token secret=s3cret-value-42 env=$(env | sort | tr '\\n' ' ')\" >&2; exit 3",
    );
    let error = launch_err(&home, &home.launcher(), "leaky").await;
    let LaunchError::Exited { status, stderr } = &error else {
        panic!("{error}");
    };
    assert_eq!(status.code(), Some(3));
    assert!(error.is_retryable());
    let line = &stderr[0];
    assert!(
        line.starts_with("token=<redacted> secret=<redacted> env="),
        "{line}"
    );
    // The scrubbed environment of PLAN section 12.4, nothing from the core's.
    let data = home.root.plugin_data_dir("leaky").unwrap();
    let data = data.display();
    let mut expected = vec![
        format!("GILES_PLUGIN_DATA={data}"),
        "GILES_PLUGIN_MAGIC=giles-plugin:5b2f0c7e9a1d4836".to_owned(),
        "GILES_PLUGIN_PROTOCOLS=1".to_owned(),
        format!("HOME={data}"),
        "PATH=/usr/bin:/bin".to_owned(),
        format!("TMPDIR={data}/tmp"),
    ];
    if let Ok(lang) = std::env::var("LANG") {
        expected.push(format!("LANG={lang}"));
    }
    let mut vars: Vec<&str> = line["token=<redacted> secret=<redacted> env=".len()..]
        .split_whitespace()
        // Variables the shell itself adds.
        .filter(|var| {
            !["SHLVL=", "_=", "PWD=", "OLDPWD="]
                .iter()
                .any(|p| var.starts_with(p))
        })
        .collect();
    vars.sort_unstable();
    expected.sort();
    assert_eq!(vars, expected);
    let contents = log.contents();
    assert!(
        contents
            .contains("plugin{instance=leaky}: giles_core::plugin_host::stderr: token=<redacted>"),
        "{contents}"
    );
    assert!(!contents.contains("s3cret-value-42"), "{contents}");
    assert!(
        contents.contains(
            "plugin{instance=leaky}: giles_core::plugin_host: running the plugin unsandboxed"
        ),
        "{contents}"
    );
}

#[tokio::test]
async fn a_failing_trampoline_is_a_non_retryable_sandbox_error() {
    let home = Home::new("[channel.boxed]\nsandbox = { read = [\"/opt\"] }\n");
    let plugin = home.script("boxed", "exit 0");
    let out = home.scratch("trampoline");
    let trampoline = home.scratch("fake-giles");
    write_script(
        &trampoline,
        &format!(
            "cat \"$2\" > {out}.spec; ls -l \"$2\" > {out}.mode; echo \"$1|$3|$4|$5|$#\" > {out}.args\n\
             echo 'Error: the fake sandbox refuses' >&2; exit 125",
            out = out.display()
        ),
    );
    let launcher = home.launcher_with(trampoline, Ok(()));
    let error = launch_err(&home, &launcher, "boxed").await;
    assert!(matches!(error, LaunchError::Sandbox { .. }), "{error}");
    assert!(!error.is_retryable());
    assert!(
        error
            .to_string()
            .ends_with(": Error: the fake sandbox refuses"),
        "{error}"
    );

    let binary = plugin.canonicalize().unwrap();
    let spec_file = home.root.sandbox_spec_file("boxed").unwrap();
    assert_eq!(
        fs::read_to_string(format!("{}.args", out.display())).unwrap(),
        format!("sandbox-exec|--|{}|socket|5\n", binary.display())
    );
    assert!(
        fs::read_to_string(format!("{}.mode", out.display()))
            .unwrap()
            .starts_with("-rw-------"),
    );
    let spec =
        Spec::parse(&fs::read_to_string(format!("{}.spec", out.display())).unwrap()).unwrap();
    assert_eq!(spec, Spec::load(&spec_file).unwrap());
    assert_eq!(spec.data_dir, home.root.plugin_data_dir("boxed").unwrap());
    assert_eq!(spec.read, [binary, PathBuf::from("/opt")]);
    assert_eq!(spec.network, giles_core::sandbox::Network::All);
    assert!(!spec.unsandboxed);
}

#[tokio::test]
async fn sandboxed_instances_wait_for_a_working_sandbox() {
    let home = Home::new("[channel.boxed]\n");
    home.script("boxed", "exit 0");
    let launcher = home.launcher_with(
        PathBuf::from("/nonexistent/giles"),
        Err("Landlock is disabled".into()),
    );
    let error = launch_err(&home, &launcher, "boxed").await;
    assert_eq!(
        error.to_string(),
        "the plugin sandbox is unavailable on this host: Landlock is disabled"
    );
    assert!(!error.is_retryable());
    // Nothing was started or written.
    assert!(!home.root.pid_file("boxed").unwrap().exists());
    assert!(!home.root.sandbox_spec_file("boxed").unwrap().exists());
}

#[tokio::test]
async fn a_missing_binary_names_where_it_was_looked_for() {
    let home = Home::new(&unsandboxed("absent"));
    let error = launch_err(&home, &home.launcher(), "absent").await;
    assert!(matches!(error, LaunchError::NotFound { .. }), "{error}");
    assert!(
        error
            .to_string()
            .starts_with("plugin binary giles-channel-absent not found in "),
        "{error}"
    );
}

#[tokio::test]
async fn a_failed_launch_kills_the_plugins_whole_process_group() {
    let home = Home::new(&unsandboxed("forky"));
    let child = home.scratch("child");
    // fd 3 is closed first (so `Describe` fails at once), then a background child joins the group.
    home.script(
        "forky",
        &format!(
            "exec 3>&-; sleep 300 & echo $! > {}; echo '1|1|fd|3|grpc'; exec sleep 300",
            child.display()
        ),
    );
    let error = launch_err(&home, &home.launcher(), "forky").await;
    assert!(
        matches!(
            error,
            LaunchError::Rpc {
                call: "Describe",
                ..
            } | LaunchError::Connect(_)
        ),
        "{error}"
    );
    let child: u32 = fs::read_to_string(child).unwrap().trim().parse().unwrap();
    wait_gone(child).await;
    assert!(!home.root.pid_file("forky").unwrap().exists());
}

#[tokio::test]
async fn orphans_running_the_plugin_binary_are_killed_and_others_left_alone() {
    let home = Home::new(&unsandboxed("sleepy"));
    // The "plugin" is a copy of `sleep`: run with no arguments it exits at once, which fails the
    // launch after the cleanup this test is about.
    let binary = home.root.plugins_dir().join("giles-channel-sleepy");
    fs::copy("/bin/sleep", &binary).unwrap();
    let binary = binary.canonicalize().unwrap();
    let pid_file = home.root.pid_file("sleepy").unwrap();
    let record = |pid: u32| fs::write(&pid_file, format!("{pid}\n{}\n", binary.display())).unwrap();

    // An unrelated process (a different executable) under a recorded pid survives.
    let mut unrelated = Command::new("/bin/sleep").arg("300").spawn().unwrap();
    record(unrelated.id());
    let error = launch_err(&home, &home.launcher(), "sleepy").await;
    assert!(matches!(error, LaunchError::Exited { .. }), "{error}");
    assert!(
        unrelated.try_wait().unwrap().is_none(),
        "the unrelated process was killed"
    );
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();

    // An orphan running the plugin binary is killed before the launch.
    let mut orphan = Command::new(&binary).arg("300").spawn().unwrap();
    // Wait until it has exec'd, so its executable is the plugin binary.
    tokio::time::sleep(Duration::from_millis(200)).await;
    record(orphan.id());
    let error = launch_err(&home, &home.launcher(), "sleepy").await;
    assert!(matches!(error, LaunchError::Exited { .. }), "{error}");
    let status = orphan.wait().unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(libc::SIGKILL)
    );
    assert!(!pid_file.exists());
}
