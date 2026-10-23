//! End-to-end tests of the Linux sandbox (Landlock and seccomp) with the real
//! `giles sandbox-exec`: probes run under it with `/bin/sh` and its tools, `perl`, and this test
//! binary itself (a Rust program, see [`probe`]), and must reach exactly what the spec grants.
//!
//! Every sandboxed launch needs Landlock. On a kernel without it (the Apple `container` kernel
//! has none) each test says so on stderr and passes, except
//! [`without_landlock_nothing_runs`], which runs only there. With `GILES_REQUIRE_LANDLOCK=1`
//! (CI's Linux job) a missing Landlock fails the tests instead.
#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use giles_core::sandbox::{HostReport, Network, Spec, check_host};

/// The effective Landlock ABI, or `None` (after saying why) when the sandbox can't run here.
fn landlock_abi() -> Option<u32> {
    match check_host() {
        Ok(HostReport::Linux { landlock_abi }) => Some(landlock_abi),
        Ok(other) => panic!("{other:?}"),
        Err(err) => {
            assert!(
                std::env::var_os("GILES_REQUIRE_LANDLOCK").is_none_or(|v| v != "1"),
                "GILES_REQUIRE_LANDLOCK=1, but the sandbox can't run here: {err}"
            );
            eprintln!(
                "skipping: the Linux sandbox can't run on this kernel ({err}); set \
                 GILES_REQUIRE_LANDLOCK=1 to fail instead"
            );
            None
        }
    }
}

/// Returns from the test, after saying why, when Landlock is missing.
macro_rules! require_landlock {
    () => {
        match landlock_abi() {
            Some(abi) => abi,
            None => return,
        }
    };
}

/// A temporary directory holding a data directory, the spec file, and things outside every
/// grant.
struct Env {
    dir: tempfile::TempDir,
    data_dir: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data");
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

    /// Runs this test binary's [`probe`] under `spec` with `actions`, one per line of output.
    fn probe(&self, spec: &Spec, actions: &[String]) -> Probe {
        let mut spec = spec.clone();
        spec.env.insert(PROBE.into(), actions.join("\n"));
        let exe = std::env::current_exe().unwrap();
        Probe::run(self.command(
            &spec,
            &[
                exe.as_os_str(),
                "probe".as_ref(),
                "--exact".as_ref(),
                "--nocapture".as_ref(),
                "--test-threads=1".as_ref(),
            ],
        ))
    }
}

/// A finished launch.
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

    /// Asserts a zero exit status and returns the [`probe`]'s results, one per action.
    fn results(&self) -> Vec<&str> {
        self.ok()
            .into_iter()
            .filter_map(|line| line.strip_prefix("probe: "))
            .collect()
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

/// The result line for an errno.
fn errno(code: i32) -> String {
    format!("errno {code}")
}

#[test]
fn without_landlock_nothing_runs() {
    if check_host().is_ok() {
        return; // Covered by every other test here.
    }
    let env = Env::new();
    let marker = env.path("ran");
    let probe = env.sh(&env.spec(), r#"touch "$1""#, &[&marker]);
    assert_eq!(probe.output.status.code(), Some(125), "{probe}");
    assert!(probe.stderr.contains("so it was not run"), "{probe}");
    assert!(probe.stderr.contains("Landlock"), "{probe}");
    assert!(!marker.exists(), "the command ran");
}

#[test]
fn the_launch_logs_the_effective_abi() {
    let abi = require_landlock!();
    let env = Env::new();
    let probe = env.sh(&env.spec(), "true", &[]);
    probe.ok();
    assert!(
        probe.stderr.contains(&format!("landlock_abi={abi}")),
        "{probe}"
    );
}

#[test]
fn min_abi_is_enforced() {
    let abi = require_landlock!();
    let env = Env::new();
    let marker = env.path("ran");
    let spec = Spec {
        min_abi: Some(abi + 1),
        ..env.spec()
    };
    let probe = env.sh(&spec, r#"touch "$1""#, &[&marker]);
    assert_eq!(probe.output.status.code(), Some(125), "{probe}");
    assert!(
        probe
            .stderr
            .contains(&format!("requires Landlock ABI {}", abi + 1)),
        "{probe}"
    );
    assert!(!marker.exists());

    let spec = Spec {
        min_abi: Some(abi),
        ..env.spec()
    };
    assert_eq!(env.sh(&spec, "echo ran", &[]).ok(), ["ran"]);
}

#[test]
fn data_dir_and_its_tmp_are_writable() {
    require_landlock!();
    let env = Env::new();
    let script = r#"
        set -e
        echo data > "$HOME/f" && cat "$GILES_PLUGIN_DATA/f"
        echo tmp > "$TMPDIR/t" && cat "$TMPDIR/t"
        mkdir sub && echo nested > sub/x && cat sub/x && mv sub/x y && rm -r sub y
        echo long > g && truncate -s 2 g && cat g && echo
        pwd -P
    "#;
    let probe = env.sh(&env.spec(), script, &[]);
    assert_eq!(
        probe.ok(),
        [
            "data",
            "tmp",
            "nested",
            "lo",
            env.data_dir.to_str().unwrap()
        ]
    );
}

#[test]
fn everything_outside_the_grants_is_denied() {
    require_landlock!();
    let env = Env::new();
    // A stand-in for `~/.ssh`: a private key outside every grant.
    let ssh = env.path("home/.ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::write(ssh.join("id_ed25519"), "PRIVATE KEY").unwrap();
    let script = r#"
        cat "$1" || echo read-denied
        ls "$2" || echo list-denied
        echo x > "$3" || echo write-denied
        echo x > "$4" || echo overwrite-denied
        truncate -s 0 "$4" || echo truncate-denied
        cp "$4" "$HOME/copy" || echo copy-denied
        /bin/rm "$4" || echo remove-denied
    "#;
    let spec_file = env.path("spec.json");
    let probe = env.sh(
        &env.spec(),
        script,
        &[&ssh.join("id_ed25519"), &ssh, &env.path("new"), &spec_file],
    );
    assert_eq!(
        probe.ok(),
        [
            "read-denied",
            "list-denied",
            "write-denied",
            "overwrite-denied",
            "truncate-denied",
            "copy-denied",
            "remove-denied"
        ]
    );
    assert!(probe.stderr.contains("Permission denied"), "{probe}");
    assert!(!probe.stdout.contains("PRIVATE KEY"));
    assert!(!env.path("new").exists());
    assert!(!env.data_dir.join("copy").exists());
    // The spec file (written before the probe ran) is intact.
    assert!(fs::read_to_string(&spec_file).unwrap().contains("data_dir"));
}

#[test]
fn read_and_write_grants() {
    require_landlock!();
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
        echo x > "$1/w" || echo write-readable-denied
        truncate -s 0 "$1/r" || echo truncate-readable-denied
        echo written > "$2/w" && cat "$2/w"
    "#;
    let probe = env.sh(&spec, script, &[&readable, &writable]);
    assert_eq!(
        probe.ok(),
        [
            "readable",
            "write-readable-denied",
            "truncate-readable-denied",
            "written"
        ]
    );
    assert!(!readable.join("w").exists());
    assert_eq!(
        fs::read_to_string(readable.join("r")).unwrap(),
        "readable\n"
    );
}

/// Programs run only from the command and `execute` grants; the data directory is writable but
/// not executable, even for a binary that is executable everywhere else.
#[test]
fn only_granted_programs_run() {
    require_landlock!();
    let env = Env::new();
    let planted = env.data_dir.join("true");
    fs::copy("/bin/true", &planted).unwrap();
    fs::set_permissions(&planted, fs::Permissions::from_mode(0o755)).unwrap();
    let spec = Spec {
        execute: Vec::new(),
        ..env.spec()
    };
    let script = r#"
        echo builtin
        /bin/true || echo true-denied
        "$HOME/true" || echo planted-denied
    "#;
    let probe = env.sh(&spec, script, &[]);
    assert_eq!(probe.ok(), ["builtin", "true-denied", "planted-denied"]);
    // With `/bin` and `/usr/bin` granted, programs there run; the data directory still doesn't.
    let probe = env.sh(
        &env.spec(),
        r#"/bin/true && echo ran; "$HOME/true" || echo planted-denied"#,
        &[],
    );
    assert_eq!(probe.ok(), ["ran", "planted-denied"]);
}

/// Connects to each address and reports which connections went through, both as the probe saw
/// it and as the listeners saw it.
fn tcp_probe(env: &Env, network: Network, listeners: &[TcpListener]) -> Vec<bool> {
    let spec = Spec {
        network,
        ..env.spec()
    };
    let actions: Vec<String> = listeners
        .iter()
        .map(|l| format!("connect {}", l.local_addr().unwrap()))
        .collect();
    let probe = env.probe(&spec, &actions);
    let results = probe.results();
    assert_eq!(results.len(), listeners.len(), "{probe}");
    results
        .iter()
        .zip(listeners)
        .map(|(result, listener)| {
            listener.set_nonblocking(true).unwrap();
            let accepted = listener.accept().is_ok();
            assert_eq!(*result == "ok", accepted, "{probe}");
            accepted
        })
        .collect()
}

fn listeners(n: usize) -> Vec<TcpListener> {
    (0..n)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect()
}

#[test]
fn network_off_denies_ip_sockets() {
    require_landlock!();
    let env = Env::new();
    assert_eq!(tcp_probe(&env, Network::Off, &listeners(1)), [false]);
    let probe = env.probe(&env.spec(), &["socket inet".into(), "socket inet6".into()]);
    assert_eq!(probe.results(), [errno(libc::EPERM), errno(libc::EPERM)]);
}

#[test]
fn network_on_allows_tcp() {
    require_landlock!();
    let env = Env::new();
    assert_eq!(tcp_probe(&env, Network::All, &listeners(2)), [true, true]);
}

/// Landlock restricts TCP ports from ABI 4; below it a port list allows all IP traffic
/// (PLAN section 12.4).
#[test]
fn port_list_allows_only_listed_ports() {
    let abi = require_landlock!();
    let env = Env::new();
    let listeners = listeners(2);
    let granted = listeners[0].local_addr().unwrap().port();
    let result = tcp_probe(&env, Network::Tcp(BTreeSet::from([granted])), &listeners);
    if abi >= 4 {
        assert_eq!(result, [true, false]);
    } else {
        eprintln!("Landlock ABI {abi} has no port rules; the port list allows all IP traffic");
        assert_eq!(result, [true, true]);
    }
}

/// Unix sockets need a `unix` grant: without one `socket(AF_UNIX)` fails. With one, Landlock
/// ABI 9 allows connecting only to the granted paths (below it, to every reachable socket), and
/// ABI 6 keeps abstract sockets outside the sandbox out of reach.
#[test]
fn unix_sockets_need_a_grant() {
    let abi = require_landlock!();
    let env = Env::new();
    let granted = env.path("granted.sock");
    let other = env.path("other.sock");
    let _listeners = [
        UnixListener::bind(&granted).unwrap(),
        UnixListener::bind(&other).unwrap(),
    ];
    let name = format!("giles-sandbox-test-{}", std::process::id());
    let _abstract =
        UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
    let actions = [
        format!("unix {}", granted.display()),
        format!("unix {}", other.display()),
        format!("abstract {name}"),
        "socketpair".into(),
    ];

    let probe = env.probe(&env.spec(), &actions);
    let denied = errno(libc::EPERM);
    assert_eq!(probe.results(), [&denied, &denied, &denied, "ok"]);

    let spec = Spec {
        unix: vec![granted.clone()],
        ..env.spec()
    };
    let probe = env.probe(&spec, &actions);
    let results = probe.results();
    assert_eq!(results[0], "ok", "{probe}");
    let landlock_denied = errno(libc::EACCES);
    let expected_other = if abi >= 9 {
        landlock_denied.as_str()
    } else {
        "ok"
    };
    assert_eq!(results[1], expected_other, "{probe}");
    let expected_abstract = if abi >= 6 { denied.as_str() } else { "ok" };
    assert_eq!(results[2], expected_abstract, "{probe}");
    assert_eq!(results[3], "ok", "{probe}");
}

/// The syscall filter: the denied list fails with `EPERM`, unlisted syscalls with `ENOSYS`.
#[test]
fn seccomp_denials() {
    require_landlock!();
    let env = Env::new();
    let actions = ["io_uring_setup", "ptrace", "mount", "clone3"].map(|a| format!("syscall {a}"));
    let probe = env.probe(&env.spec(), &actions);
    assert_eq!(
        probe.results(),
        [
            errno(libc::EPERM),
            errno(libc::EPERM),
            errno(libc::ENOSYS),
            errno(libc::ENOSYS)
        ]
    );
}

/// Removes a System V shared memory segment when dropped.
struct RemoveShm(libc::c_int);

impl Drop for RemoveShm {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: `IPC_RMID` takes no buffer.
        unsafe {
            libc::shmctl(self.0, libc::IPC_RMID, std::ptr::null_mut());
        }
    }
}

/// System V IPC and POSIX message queues are keyed in the IPC namespace, which the sandbox shares
/// with everything else and Landlock doesn't cover: a segment created outside is out of reach,
/// and so is creating any.
#[test]
#[allow(unsafe_code)]
fn ipc_is_out_of_reach() {
    require_landlock!();
    let env = Env::new();
    let key = 0x4769_0000 | (std::process::id() & 0xffff) as libc::key_t;
    // SAFETY: `shmget` takes no pointers.
    let id = unsafe { libc::shmget(key, 4096, libc::IPC_CREAT | libc::IPC_EXCL | 0o600) };
    assert!(id >= 0, "shmget: {}", io::Error::last_os_error());
    let _segment = RemoveShm(id);

    let actions = [
        format!("shmget {key}"),
        "syscall msgget".into(),
        "syscall semget".into(),
        "syscall mq_open".into(),
    ];
    let probe = env.probe(&env.spec(), &actions);
    assert_eq!(probe.results(), vec![errno(libc::EPERM); 4], "{probe}");
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

/// The inherited socket pair end and stdin work with no grants: they're already open.
#[test]
fn fd3_socket_pair_and_stdin_pass_through() {
    require_landlock!();
    let env = Env::new();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let script = r#"
        read token
        printf 'token=%s\n' "$token" >&3
        exit 7
    "#;
    let mut cmd = env.command(&env.spec(), &["/bin/sh", "-c", script]);
    cmd.stdin(Stdio::piped());
    let theirs = OwnedFd::from(theirs);
    pass_as_fd3(&mut cmd, &theirs);
    let mut child = cmd.spawn().unwrap();
    drop(theirs);
    child.stdin.take().unwrap().write_all(b"tok123\n").unwrap();
    let probe = Probe::from(child.wait_with_output().unwrap());
    // The trampoline `exec`s, so the command's status is the trampoline's.
    assert_eq!(probe.output.status.code(), Some(7), "{probe}");
    let mut received = String::new();
    let mut ours = ours;
    ours.read_to_string(&mut received).unwrap();
    assert_eq!(received, "token=tok123\n");
}

#[test]
fn exit_statuses_are_preserved() {
    require_landlock!();
    let env = Env::new();
    let probe = env.sh(&env.spec(), "exit 7", &[]);
    assert_eq!(probe.output.status.code(), Some(7), "{probe}");

    let missing = env.path("missing-plugin");
    let probe = Probe::run(env.command(&env.spec(), &[&missing]));
    assert_eq!(probe.output.status.code(), Some(127), "{probe}");

    let not_executable = env.path("plugin");
    fs::write(&not_executable, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&not_executable, fs::Permissions::from_mode(0o644)).unwrap();
    let probe = Probe::run(env.command(&env.spec(), &[&not_executable]));
    assert_eq!(probe.output.status.code(), Some(126), "{probe}");
}

/// From Landlock ABI 6 the plugin can signal only its own processes; below it, any process of
/// the same user.
#[test]
fn signals_reach_only_the_plugin_and_its_children() {
    let abi = require_landlock!();
    let env = Env::new();
    let script = r#"
        kill -0 "$1"; echo "outside=$?"
        sleep 5 & kill "$!"; wait "$!"; echo "child=$?"
    "#;
    let ours = std::process::id().to_string();
    let probe = env.sh(&env.spec(), script, &[Path::new(&ours)]);
    let outside = if abi >= 6 { "outside=1" } else { "outside=0" };
    assert_eq!(probe.ok(), [outside, "child=143"]);
}

/// A plugin that replaced its data directory with a link would get the link's target as its
/// data directory on the next launch. Removing or renaming the directory is denied (that needs
/// rights on its parent, which isn't granted), and a data directory that is a link, however it
/// got there, stops the launch.
#[test]
fn data_dir_cannot_be_swapped_for_a_link() {
    require_landlock!();
    let env = Env::new();
    let victim = env.path("victim");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("secret"), "TOPSECRET\n").unwrap();

    // Launch 1: the plugin tries to move its data directory out of the way.
    let script = r#"
        cd /
        rm -rf "$HOME" 2>/dev/null; echo "rm=$?"
        mv "$HOME" "$HOME.old" 2>/dev/null; echo "mv=$?"
        rmdir "$HOME" 2>/dev/null; echo "rmdir=$?"
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
/// have planted them on an earlier run, so the grant gives nothing.
#[test]
fn grants_never_follow_links_the_plugin_planted() {
    require_landlock!();
    let env = Env::new();
    let victim = env.path("victim");
    fs::create_dir(&victim).unwrap();
    fs::write(victim.join("secret"), "TOPSECRET\n").unwrap();
    let planted = env.data_dir.join("notes");

    // Launch 1: the plugin plants a link where its granted directory should be.
    let probe = env.sh(
        &env.spec(),
        r#"ln -s "$1" "$2"; echo "ln=$?""#,
        &[&victim, &planted],
    );
    assert_eq!(probe.ok(), ["ln=0"]);

    // Launch 2, with that directory granted: nothing is granted through the link.
    let spec = Spec {
        read: vec![planted.clone()],
        ..env.spec()
    };
    let probe = env.sh(&spec, r#"cat "$1/secret" || echo denied"#, &[&planted]);
    assert_eq!(probe.ok(), ["denied"]);
    assert!(
        probe
            .stderr
            .contains("goes through a symbolic link inside a writable grant"),
        "{probe}"
    );
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
/// resolves its grants. Opening each grant in one walk that follows no link inside a writable
/// directory, and building the rule from that descriptor, leaves nothing to race.
#[test]
fn a_racing_link_never_redirects_a_grant() {
    require_landlock!();
    let env = Env::new();
    let victim = env.path("victim");
    fs::create_dir(&victim).unwrap();
    let secret = victim.join("secret");
    fs::write(&secret, "TOPSECRET\n").unwrap();
    let notes = env.data_dir.join("notes");

    // Launch 1 leaves a child behind that keeps creating and removing `notes` -> victim.
    let script = r#"
        perl -e 'while (1) { symlink($ARGV[0], $ARGV[1]); unlink($ARGV[1]) }' \
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

/// A Rust program (this test binary) under a channel-like spec: threads, the clock, files in
/// `TMPDIR`, a socket pair, name resolution, and an outbound TCP connection to the granted port
/// work.
#[test]
fn rust_runtime_runs_under_the_sandbox() {
    require_landlock!();
    let env = Env::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let spec = Spec {
        network: Network::Tcp(BTreeSet::from([port])),
        ..Spec::new(&env.data_dir)
    };
    let actions = [
        "runtime".to_owned(),
        format!("resolve localhost:{port}"),
        "socketpair".to_owned(),
    ];
    let probe = env.probe(&spec, &actions);
    assert_eq!(probe.results(), ["ok", "ok", "ok"]);
    listener.set_nonblocking(true).unwrap();
    assert!(listener.accept().is_ok(), "{probe}");
}

/// The variable that turns [`probe`] into the probe: actions, one per line.
const PROBE: &str = "GILES_SANDBOX_PROBE";

/// Runs one probe action and describes the result: `ok` or `errno <n>`.
#[allow(unsafe_code)]
fn act(action: &str) -> String {
    let (verb, arg) = action.split_once(' ').unwrap_or((action, ""));
    let result: io::Result<()> = match verb {
        "connect" => TcpStream::connect(arg).map(drop),
        "resolve" => arg
            .to_socket_addrs()
            .and_then(|addrs| TcpStream::connect(&addrs.collect::<Vec<_>>()[..]))
            .map(drop),
        "unix" => UnixStream::connect(arg).map(drop),
        "abstract" => SocketAddr::from_abstract_name(arg.as_bytes())
            .and_then(|addr| UnixStream::connect_addr(&addr))
            .map(drop),
        "socket" => {
            let family = match arg {
                "inet" => libc::AF_INET,
                "inet6" => libc::AF_INET6,
                other => panic!("unknown family {other}"),
            };
            raw(|| {
                // SAFETY: `socket` takes no pointers.
                let fd = unsafe { libc::socket(family, libc::SOCK_STREAM, 0) };
                fd.into()
            })
        }
        "socketpair" => UnixStream::pair().and_then(|(mut a, mut b)| {
            a.write_all(b"p")?;
            let mut byte = [0];
            b.read_exact(&mut byte)
        }),
        "shmget" => {
            let key: libc::key_t = arg.parse().unwrap();
            raw(|| {
                // SAFETY: `shmget` takes no pointers.
                unsafe { libc::shmget(key, 0, 0) }.into()
            })
        }
        "syscall" => {
            let nr = match arg {
                "io_uring_setup" => libc::SYS_io_uring_setup,
                "ptrace" => libc::SYS_ptrace,
                "mount" => libc::SYS_mount,
                "clone3" => libc::SYS_clone3,
                "msgget" => libc::SYS_msgget,
                "semget" => libc::SYS_semget,
                "mq_open" => libc::SYS_mq_open,
                other => panic!("unknown syscall {other}"),
            };
            raw(|| {
                // SAFETY: every argument is 0 (null pointers, zero sizes, no flags), so these
                // syscalls read and write no memory of ours.
                unsafe { libc::syscall(nr, 0, 0, 0, 0, 0) }
            })
        }
        "runtime" => runtime(),
        other => panic!("unknown probe action {other}"),
    };
    match result {
        Ok(()) => "ok".into(),
        Err(err) => errno(err.raw_os_error().unwrap_or(-1)),
    }
}

/// A raw syscall's result: its errno when it returned -1.
fn raw(call: impl FnOnce() -> libc::c_long) -> io::Result<()> {
    if call() < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// What a Rust runtime does at startup and in normal use.
fn runtime() -> io::Result<()> {
    let handles: Vec<_> = (0..4).map(|i| std::thread::spawn(move || i * 2)).collect();
    let sum: i32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(sum, 12);
    assert!(std::time::SystemTime::now() > std::time::UNIX_EPOCH);
    let file = std::env::temp_dir().join("probe");
    fs::write(&file, "x")?;
    assert_eq!(fs::read_to_string(&file)?, "x");
    let mut random = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(())
}

/// The probe for the tests above; does nothing in a normal test run.
#[test]
fn probe() {
    let Ok(actions) = std::env::var(PROBE) else {
        return;
    };
    // libtest has already printed `test probe ... ` without a newline.
    println!();
    for action in actions.lines() {
        println!("probe: {}", act(action));
    }
}
