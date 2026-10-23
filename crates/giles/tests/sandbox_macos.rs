//! End-to-end tests of the macOS sandbox (Seatbelt through `sandbox-exec`) with the real
//! `giles sandbox-exec`: probes run under the generated profile with `/bin/sh`, `/bin/cat`,
//! `/usr/bin/nc`, and this test binary itself (a Rust runtime), and must reach exactly what the
//! spec grants. Nothing here touches privacy-protected (TCC) locations.
#![cfg(target_os = "macos")]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use giles_core::sandbox::{Network, Spec};

/// A temporary directory holding a data directory, the spec file, and things outside every
/// grant. It's under `/var/folders`, a symbolic link to `/private/var/folders`, so every test
/// also checks that the profile uses real paths.
struct Env {
    dir: tempfile::TempDir,
    data_dir: PathBuf,
}

impl Env {
    fn new() -> Self {
        Self::with_data_dir("data")
    }

    fn with_data_dir(name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join(name);
        fs::create_dir(&data_dir).unwrap();
        Self { dir, data_dir }
    }

    /// A path in the temporary directory, outside the data directory.
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The spec every test starts from: the data directory, and `/bin` and `/usr/bin` runnable
    /// so the probes can use the shell and its tools.
    fn spec(&self) -> Spec {
        Spec {
            execute: vec!["/bin".into(), "/usr/bin".into()],
            ..Spec::new(&self.data_dir)
        }
    }

    /// `giles sandbox-exec <spec> -- <command>`.
    fn command<S: AsRef<std::ffi::OsStr>>(&self, spec: &Spec, command: &[S]) -> Command {
        let spec_file = self.path("spec.json");
        fs::write(&spec_file, spec.to_json().unwrap()).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_giles"));
        cmd.arg("sandbox-exec")
            .arg(spec_file)
            .arg("--")
            .args(command)
            .env("GILES_HOME", self.dir.path())
            .env("SECRET_TOKEN", "leaked")
            .env("LANG", "C")
            .env_remove("GILES_LOG")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Runs `/bin/sh -c <script> sh <args>` under `spec`.
    fn sh(&self, spec: &Spec, script: &str, args: &[&Path]) -> Probe {
        let mut command = vec![Path::new("/bin/sh"), Path::new("-c"), Path::new(script)];
        command.push(Path::new("sh"));
        command.extend(args);
        Probe::run(self.command(spec, &command))
    }
}

/// A finished probe.
struct Probe {
    output: Output,
    stdout: String,
    stderr: String,
}

impl Probe {
    fn run(mut cmd: Command) -> Self {
        Self::from(cmd.output().unwrap())
    }

    fn from(output: Output) -> Self {
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        Self {
            output,
            stdout,
            stderr,
        }
    }

    /// Asserts a zero exit status and returns stdout's lines.
    fn ok(&self) -> Vec<&str> {
        assert!(self.output.status.success(), "{self}");
        self.stdout.lines().collect()
    }
}

impl std::fmt::Display for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}\nstdout:\n{}\nstderr:\n{}",
            self.output.status, self.stdout, self.stderr
        )
    }
}

fn real(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

#[test]
fn data_dir_and_its_tmp_are_writable() {
    let env = Env::new();
    let script = r#"
        set -e
        echo data > "$HOME/f" && cat "$GILES_PLUGIN_DATA/f"
        echo tmp > "$TMPDIR/t" && cat "$TMPDIR/t"
        mkdir sub && echo nested > sub/x && cat sub/x && rm -r sub
        pwd -P
    "#;
    let probe = env.sh(&env.spec(), script, &[]);
    let data_dir = real(&env.data_dir);
    assert_eq!(
        probe.ok(),
        ["data", "tmp", "nested", data_dir.to_str().unwrap()]
    );
}

#[test]
fn everything_outside_the_grants_is_denied() {
    let env = Env::new();
    // A stand-in for `~/.ssh`: a private key outside every grant.
    let ssh = env.path("home/.ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::write(ssh.join("id_ed25519"), "PRIVATE KEY").unwrap();
    let script = r#"
        cat "$1"; echo "read=$?"
        ls "$2"; echo "list=$?"
        echo x > "$3"; echo "write=$?"
        echo x > "$4"; echo "overwrite=$?"
        /bin/cp "$4" "$HOME/copy"; echo "copy=$?"
    "#;
    let spec_file = env.path("spec.json");
    let probe = env.sh(
        &env.spec(),
        script,
        &[&ssh.join("id_ed25519"), &ssh, &env.path("new"), &spec_file],
    );
    assert_eq!(
        probe.ok(),
        ["read=1", "list=1", "write=1", "overwrite=1", "copy=1"]
    );
    assert!(probe.stderr.contains("Operation not permitted"), "{probe}");
    assert!(!env.path("new").exists());
    assert!(!env.data_dir.join("copy").exists());
    // The spec file (written before the probe ran) is intact.
    assert!(fs::read_to_string(&spec_file).unwrap().contains("data_dir"));
}

#[test]
fn read_and_write_grants() {
    let env = Env::new();
    let readable = env.path("readable");
    let writable = env.path("writable");
    fs::create_dir_all(&readable).unwrap();
    fs::create_dir_all(&writable).unwrap();
    fs::write(readable.join("r"), "readable\n").unwrap();
    let spec = Spec {
        read: vec![readable.clone()],
        write: vec![writable.clone()],
        ..env.spec()
    };
    let script = r#"
        cat "$1/r"
        echo x > "$1/w"; echo "write-readable=$?"
        echo written > "$2/w" && cat "$2/w"
    "#;
    let probe = env.sh(&spec, script, &[&readable, &writable]);
    assert_eq!(probe.ok(), ["readable", "write-readable=1", "written"]);
    assert!(!readable.join("w").exists());
}

/// Connects with `nc -z` to each port and reports which connections went through, both as `nc`
/// saw it and as the listeners saw it.
fn tcp_probe(env: &Env, network: Network, listeners: &[TcpListener]) -> Vec<bool> {
    let spec = Spec {
        network,
        ..env.spec()
    };
    let ports: Vec<String> = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port().to_string())
        .collect();
    let script = r#"for port in "$@"; do nc -z -w 2 127.0.0.1 "$port"; echo "$?"; done"#;
    let ports: Vec<&Path> = ports.iter().map(Path::new).collect();
    let probe = env.sh(&spec, script, &ports);
    let connected: Vec<bool> = probe.ok().iter().map(|status| *status == "0").collect();
    for (listener, connected) in listeners.iter().zip(&connected) {
        listener.set_nonblocking(true).unwrap();
        let accepted = match listener.accept() {
            Ok(_) => true,
            Err(err) if err.kind() == ErrorKind::WouldBlock => false,
            Err(err) => panic!("{err}"),
        };
        assert_eq!(accepted, *connected, "{probe}");
    }
    connected
}

fn listeners(n: usize) -> Vec<TcpListener> {
    (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect()
}

#[test]
fn network_off_denies_tcp() {
    let env = Env::new();
    assert_eq!(tcp_probe(&env, Network::Off, &listeners(1)), [false]);
}

#[test]
fn network_on_allows_tcp() {
    let env = Env::new();
    assert_eq!(tcp_probe(&env, Network::All, &listeners(2)), [true, true]);
}

#[test]
fn port_list_allows_only_listed_ports() {
    let env = Env::new();
    let listeners = listeners(2);
    let granted = listeners[0].local_addr().unwrap().port();
    let network = Network::Tcp(BTreeSet::from([granted]));
    assert_eq!(tcp_probe(&env, network, &listeners), [true, false]);
}

/// Whether `nc -U` can connect to `socket` under `spec`.
fn unix_connects(env: &Env, spec: &Spec, socket: &Path) -> bool {
    let probe = env.sh(spec, r#"nc -U -w 1 "$1" </dev/null; echo "$?""#, &[socket]);
    probe.ok() == ["0"]
}

#[test]
fn unix_sockets_need_a_grant_for_that_path() {
    let env = Env::new();
    let socket = env.path("agent.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let other = env.path("other.sock");

    assert!(!unix_connects(&env, &env.spec(), &socket));
    // A network grant is not a Unix socket grant.
    let network = Spec {
        network: Network::All,
        ..env.spec()
    };
    assert!(!unix_connects(&env, &network, &socket));
    let wrong = Spec {
        unix: vec![other],
        ..env.spec()
    };
    assert!(!unix_connects(&env, &wrong, &socket));
    // Granted as spelled (through the `/var` link); the profile uses the real path.
    let granted = Spec {
        unix: vec![socket.clone()],
        ..env.spec()
    };
    assert!(unix_connects(&env, &granted, &socket));
}

/// `getaddrinfo` resolves through mDNSResponder's socket, so a network grant must reach it, and
/// no network must not.
#[test]
fn dns_resolver_socket_comes_with_network_grants() {
    let env = Env::new();
    let resolver = Path::new("/var/run/mDNSResponder");
    assert!(!unix_connects(&env, &env.spec(), resolver));
    for network in [Network::All, Network::Tcp(BTreeSet::from([443]))] {
        let spec = Spec {
            network,
            ..env.spec()
        };
        assert!(unix_connects(&env, &spec, resolver));
    }
}

/// Makes `fd` the child's fd 3, without `FD_CLOEXEC`, as the core does with the socket pair.
#[allow(unsafe_code)]
fn pass_as_fd3(cmd: &mut Command, fd: &OwnedFd) {
    let fd = fd.as_raw_fd();
    // SAFETY: the closure runs in the forked child before `exec` and only calls `dup2` and
    // `fcntl`, which are async-signal-safe; it touches no memory shared with the parent.
    unsafe {
        cmd.pre_exec(move || {
            let ret = if fd == 3 {
                libc::fcntl(3, libc::F_SETFD, 0)
            } else {
                libc::dup2(fd, 3)
            };
            if ret == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// The inherited socket pair end and stdin work under deny-by-default with no grants: they're
/// already open.
#[test]
fn fd3_socket_pair_and_stdin_pass_through() {
    let env = Env::new();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let script = r#"
        read token
        printf 'token=%s\n' "$token" >&3
        read reply <&3
        echo "reply=$reply"
    "#;
    let mut cmd = env.command(&env.spec(), &["/bin/sh", "-c", script]);
    cmd.stdin(Stdio::piped());
    let theirs = OwnedFd::from(theirs);
    pass_as_fd3(&mut cmd, &theirs);
    let mut child = cmd.spawn().unwrap();
    drop(theirs);
    child.stdin.take().unwrap().write_all(b"tok123\n").unwrap();
    let mut reader = BufReader::new(ours.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "token=tok123\n");
    (&ours).write_all(b"pong\n").unwrap();
    let probe = Probe::from(child.wait_with_output().unwrap());
    assert_eq!(probe.ok(), ["reply=pong"]);
}

#[test]
fn environment_is_scrubbed_under_the_sandbox() {
    let env = Env::new();
    let mut spec = env.spec();
    spec.env
        .insert("GILES_PLUGIN_MAGIC".into(), "cookie".into());
    let probe = Probe::run(env.command(&spec, &["/usr/bin/env"]));
    let vars: BTreeMap<&str, &str> = probe
        .ok()
        .into_iter()
        .map(|line| line.split_once('=').unwrap())
        .collect();
    let data = env.data_dir.to_str().unwrap();
    let tmp = env.data_dir.join("tmp");
    let expected = BTreeMap::from([
        ("GILES_PLUGIN_DATA", data),
        ("GILES_PLUGIN_MAGIC", "cookie"),
        ("HOME", data),
        ("LANG", "C"),
        ("PATH", "/usr/bin:/bin"),
        ("TMPDIR", tmp.to_str().unwrap()),
    ]);
    assert_eq!(vars, expected);
    assert!(!probe.stderr.contains("unsandboxed"), "{probe}");
}

#[test]
fn exit_statuses_are_preserved() {
    let env = Env::new();
    let status = |probe: Probe| (probe.output.status.code(), probe);

    let (code, probe) = status(env.sh(&env.spec(), "exit 7", &[]));
    assert_eq!(code, Some(7), "{probe}");

    let missing = env.path("missing-plugin");
    let (code, probe) = status(Probe::run(env.command(&env.spec(), &[&missing])));
    assert_eq!(code, Some(127), "{probe}");
    assert!(probe.stderr.contains("cannot run"), "{probe}");

    let not_executable = env.path("plugin");
    fs::write(&not_executable, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&not_executable, fs::Permissions::from_mode(0o644)).unwrap();
    for program in [not_executable.as_path(), env.dir.path()] {
        let (code, probe) = status(Probe::run(env.command(&env.spec(), &[program])));
        assert_eq!(code, Some(126), "{probe}");
        assert!(probe.stderr.contains("cannot run"), "{probe}");
    }

    let bad_spec = env.command(&env.spec(), &["/bin/sh"]);
    fs::write(env.path("spec.json"), "{}").unwrap();
    let (code, probe) = status(Probe::run(bad_spec));
    assert_eq!(code, Some(125), "{probe}");
}

/// The command is always runnable, but a script's interpreter needs an `execute` grant like any
/// other program. Without one `sandbox-exec` can't `exec` the script and exits 71.
#[test]
fn script_interpreters_need_an_execute_grant() {
    let env = Env::new();
    let script = env.path("plugin");
    fs::write(&script, "#!/bin/sh\necho ran\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let probe = Probe::run(env.command(&Spec::new(&env.data_dir), &[&script]));
    assert_eq!(probe.output.status.code(), Some(71), "{probe}");
    assert!(probe.stderr.contains("execvp()"), "{probe}");
    assert!(probe.stdout.is_empty(), "{probe}");

    let probe = Probe::run(env.command(&env.spec(), &[&script]));
    assert_eq!(probe.ok(), ["ran"]);
}

#[test]
fn signals_reach_only_the_plugin_and_its_children() {
    let env = Env::new();
    let script = r#"
        kill -0 "$1"; echo "outside=$?"
        sleep 5 & kill "$!"; wait "$!"; echo "child=$?"
    "#;
    let ours = std::process::id().to_string();
    let probe = env.sh(&env.spec(), script, &[Path::new(&ours)]);
    assert_eq!(probe.ok(), ["outside=1", "child=143"]);
}

/// A plugin that replaced its data directory with a link would get the link's target as its
/// data directory on the next launch. Removing or renaming the directory is denied, and a data
/// directory that is a link, however it got there, stops the launch.
#[test]
fn data_dir_cannot_be_swapped_for_a_link() {
    let env = Env::new();
    let victim = env.path("victim");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("secret"), "TOPSECRET\n").unwrap();

    // Launch 1: the plugin tries to move its data directory out of the way.
    let script = r#"
        cd /
        rm -rf "$HOME" 2>/dev/null; echo "rm=$?"
        mv "$HOME" "$HOME.old" 2>/dev/null; echo "mv=$?"
        /bin/rmdir "$HOME" 2>/dev/null; echo "rmdir=$?"
    "#;
    let probe = env.sh(&env.spec(), script, &[]);
    assert_eq!(probe.ok(), ["rm=1", "mv=1", "rmdir=1"]);
    assert!(fs::symlink_metadata(&env.data_dir).unwrap().is_dir());

    // Had it succeeded, the next launch refuses to follow the link.
    fs::remove_dir_all(&env.data_dir).unwrap();
    std::os::unix::fs::symlink(&victim, &env.data_dir).unwrap();
    let probe = env.sh(
        &env.spec(),
        r#"cat "$HOME/secret"; echo x > "$HOME/new""#,
        &[],
    );
    assert_eq!(probe.output.status.code(), Some(125), "{probe}");
    assert!(probe.stderr.contains("is a symbolic link"), "{probe}");
    assert!(!probe.stdout.contains("TOPSECRET"), "{probe}");
    assert_eq!(
        fs::read_dir(&victim).unwrap().count(),
        1,
        "the launch touched the link's target"
    );
}

/// A grant inside a writable directory isn't resolved through links there: the plugin could
/// have planted them on an earlier run, so the grant stays where it's spelled.
#[test]
fn grants_never_follow_links_the_plugin_planted() {
    let env = Env::new();
    let socket = env.path("docker.sock");
    let _listener = UnixListener::bind(&socket).unwrap();
    let planted = env.data_dir.join("agent.sock");

    // Launch 1: the plugin plants a link where its granted socket should be.
    let probe = env.sh(
        &env.spec(),
        r#"ln -s "$1" "$2"; echo "ln=$?""#,
        &[&socket, &planted],
    );
    assert_eq!(probe.ok(), ["ln=0"]);

    // Launch 2, with that socket granted: the grant is the link's place, not its target.
    let spec = Spec {
        unix: vec![planted.clone()],
        ..env.spec()
    };
    assert!(!unix_connects(&env, &spec, &planted));
    assert!(!unix_connects(&env, &spec, &socket));
}

/// Kills a process when dropped.
struct KillOnDrop(libc::pid_t);

impl Drop for KillOnDrop {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: `kill` only sends a signal.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
    }
}

/// A plugin's child can outlive it and flip a link in the data directory while the next launch
/// resolves its grants. Resolving each grant in one walk that follows no link inside a writable
/// directory leaves nothing to race.
#[test]
fn a_racing_link_never_redirects_a_grant() {
    let env = Env::new();
    let victim = env.path("victim");
    fs::create_dir(&victim).unwrap();
    let secret = victim.join("secret");
    fs::write(&secret, "TOPSECRET\n").unwrap();
    let notes = env.data_dir.join("notes");

    // Launch 1 leaves a child behind that keeps creating and removing `notes` -> victim.
    let script = r#"
        /usr/bin/perl -e 'while (1) { symlink($ARGV[0], $ARGV[1]); unlink($ARGV[1]) }' \
            "$1" "$2" </dev/null >/dev/null 2>&1 &
        echo "$!"
    "#;
    let probe = env.sh(&env.spec(), script, &[&victim, &notes]);
    let _racer = KillOnDrop(probe.ok()[0].parse().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs::symlink_metadata(&notes).is_ok_and(|m| m.file_type().is_symlink()) {
        assert!(
            Instant::now() < deadline,
            "the racer never planted the link"
        );
        std::thread::sleep(Duration::from_millis(1));
    }

    // The next launches grant `notes` and try the secret both ways.
    let spec = Spec {
        read: vec![notes.clone()],
        ..env.spec()
    };
    let through_link = notes.join("secret");
    let start = Instant::now();
    let mut launches = 0;
    while launches < 200 && start.elapsed() < Duration::from_secs(6) {
        let probe =
            Probe::run(env.command(&spec, &[Path::new("/bin/cat"), &secret, &through_link]));
        assert!(
            !probe.stdout.contains("TOPSECRET"),
            "launch {launches} read the secret: {probe}"
        );
        launches += 1;
    }
    assert!(launches >= 30, "only {launches} launches");
}

/// Paths are SBPL string literals: quotes, backslashes, and newlines can't break out of them.
#[test]
fn hostile_data_dir_names_are_escaped() {
    for name in [
        "q\"uote",
        "back\\slash",
        "trailing\\",
        "x\")(allow default)(literal \"",
        "new\nline",
        "caf\u{e9} (1); #|",
    ] {
        let env = Env::with_data_dir(name);
        let sibling = env.path(&format!("{name}-sibling"));
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("secret"), "secret").unwrap();
        let script = r#"
            echo ok > "$HOME/f" && cat "$HOME/f"
            cat "$1/secret"; echo "sibling=$?"
        "#;
        let probe = env.sh(&env.spec(), script, &[&sibling]);
        assert_eq!(probe.ok(), ["ok", "sibling=1"], "{name:?}");
    }
}

/// The variable that turns [`rust_runtime_probe`] into the probe.
const PROBE_PORT: &str = "GILES_SANDBOX_PROBE_PORT";

/// A Rust program (this test binary) under a channel-like spec: threads, the clock, files in
/// `TMPDIR`, a socket pair, name resolution, and an outbound TCP connection work; an ungranted
/// Unix socket doesn't.
#[test]
fn rust_runtime_runs_under_the_sandbox() {
    let env = Env::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let socket = env.path("ungranted.sock");
    let _unix = UnixListener::bind(&socket).unwrap();
    let mut spec = Spec {
        network: Network::Tcp(BTreeSet::from([port])),
        ..Spec::new(&env.data_dir)
    };
    spec.env.insert(PROBE_PORT.into(), port.to_string());
    spec.env.insert(
        "GILES_SANDBOX_PROBE_SOCKET".into(),
        socket.display().to_string(),
    );
    let exe = std::env::current_exe().unwrap();
    let mut cmd = env.command(
        &spec,
        &[
            exe.as_os_str(),
            "rust_runtime_probe".as_ref(),
            "--exact".as_ref(),
            "--nocapture".as_ref(),
            "--test-threads=1".as_ref(),
        ],
    );
    let child = cmd.spawn().unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream.write_all(b"hello\n").unwrap();
    let probe = Probe::from(child.wait_with_output().unwrap());
    assert!(probe.output.status.success(), "{probe}");
    assert!(probe.stdout.contains("probe: ok"), "{probe}");
}

/// The probe for [`rust_runtime_runs_under_the_sandbox`]; does nothing in a normal test run.
#[test]
fn rust_runtime_probe() {
    let Ok(port) = std::env::var(PROBE_PORT) else {
        return;
    };
    let threads = std::thread::available_parallelism().unwrap().get();
    let handles: Vec<_> = (0..threads.min(4))
        .map(|i| std::thread::spawn(move || i * 2))
        .collect();
    let sum: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(sum < 16);
    assert!(std::time::SystemTime::now() > std::time::UNIX_EPOCH);

    let file = std::env::temp_dir().join("probe");
    fs::write(&file, "x").unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "x");

    let (mut a, mut b) = UnixStream::pair().unwrap();
    a.write_all(b"p").unwrap();
    let mut byte = [0];
    b.read_exact(&mut byte).unwrap();

    let addrs: Vec<_> = ("localhost", port.parse::<u16>().unwrap())
        .to_socket_addrs()
        .unwrap()
        .collect();
    let stream = TcpStream::connect(&addrs[..]).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    assert_eq!(line, "hello\n");

    let socket = std::env::var("GILES_SANDBOX_PROBE_SOCKET").unwrap();
    let err = UnixStream::connect(socket).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied, "{err}");
    println!("probe: ok");
}
