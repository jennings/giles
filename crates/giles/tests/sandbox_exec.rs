//! End-to-end tests of `giles sandbox-exec`: the scrubbed environment, fds 0 to 3 passing through
//! `exec`, and failing closed while the Linux backend is staged. The macOS sandbox itself is
//! tested in `sandbox_macos.rs`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use giles_core::sandbox::Spec;

/// A temporary directory holding a data directory and the spec file.
struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir(env.data_dir()).unwrap();
        env
    }

    fn data_dir(&self) -> PathBuf {
        self.dir.path().join("data")
    }

    fn spec_file(&self) -> PathBuf {
        self.dir.path().join("spec.json")
    }

    /// `giles sandbox-exec <spec> -- <command>` with `spec` written to the spec file, and a
    /// polluted environment that must not reach the command.
    fn command(&self, spec: &Spec, command: &[&str]) -> Command {
        fs::write(self.spec_file(), spec.to_json().unwrap()).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_giles"));
        cmd.arg("sandbox-exec")
            .arg(self.spec_file())
            .arg("--")
            .args(command)
            .env("GILES_LOG", "info")
            .env("GILES_HOME", self.dir.path())
            .env("SECRET_TOKEN", "leaked")
            .env("LANG", "C")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    fn unsandboxed(&self) -> Spec {
        Spec {
            unsandboxed: true,
            ..Spec::new(self.data_dir())
        }
    }
}

/// Makes `file` the child's fd 3, without `FD_CLOEXEC`, as the core does with the socket pair.
#[allow(unsafe_code)]
fn pass_as_fd3(cmd: &mut Command, file: &File) {
    let fd = file.as_raw_fd();
    // SAFETY: the closure runs in the forked child before `exec` and only calls `dup2` and
    // `fcntl`, which are async-signal-safe; it touches no memory shared with the parent.
    unsafe {
        cmd.pre_exec(move || {
            // `dup2` onto itself would keep `FD_CLOEXEC`, so clear it explicitly in that case.
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

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn run(mut cmd: Command) -> (Output, String) {
    let output = cmd.output().unwrap();
    let stderr = text(&output.stderr);
    (output, stderr)
}

#[test]
fn unsandboxed_command_gets_only_the_scrubbed_environment() {
    let env = Env::new();
    let mut spec = env.unsandboxed();
    spec.env
        .insert("GILES_PLUGIN_MAGIC".into(), "cookie".into());
    spec.env.insert("GILES_PLUGIN_PROTOCOLS".into(), "1".into());
    let (output, stderr) = run(env.command(&spec, &["/usr/bin/env"]));
    assert!(output.status.success(), "{}\n{stderr}", output.status);

    let vars: BTreeMap<String, String> = text(&output.stdout)
        .lines()
        .map(|line| {
            let (name, value) = line.split_once('=').unwrap();
            (name.to_owned(), value.to_owned())
        })
        .collect();
    let data = env.data_dir().display().to_string();
    let tmp = env.data_dir().join("tmp").display().to_string();
    let expected: BTreeMap<String, String> = [
        ("GILES_PLUGIN_DATA", data.as_str()),
        ("GILES_PLUGIN_MAGIC", "cookie"),
        ("GILES_PLUGIN_PROTOCOLS", "1"),
        ("HOME", data.as_str()),
        ("LANG", "C"),
        ("PATH", "/usr/bin:/bin"),
        ("TMPDIR", tmp.as_str()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    assert_eq!(vars, expected);

    let mode = fs::metadata(&tmp).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    assert!(stderr.contains("running unsandboxed"), "{stderr}");
}

#[test]
fn unsandboxed_command_inherits_fds_0_to_3_and_its_exit_status() {
    let env = Env::new();
    let fd3_path = env.dir.path().join("fd3");
    let fd3 = File::create(&fd3_path).unwrap();
    let script = r#"
        read token
        printf 'token=%s\n' "$token" >&3
        printf 'args=%s\n' "$*" >&3
        printf 'cwd=%s\n' "$(pwd -P)" >&3
        echo to-stdout
        echo to-stderr >&2
        exit 7
    "#;
    let mut cmd = env.command(
        &env.unsandboxed(),
        &["/bin/sh", "-c", script, "sh", "one", "--two"],
    );
    cmd.stdin(Stdio::piped());
    pass_as_fd3(&mut cmd, &fd3);
    let mut child = cmd.spawn().unwrap();
    drop(fd3);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"tok123\n").unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    let stderr = text(&output.stderr);

    // The trampoline `exec`s, so the command's status is the trampoline's.
    assert_eq!(output.status.code(), Some(7), "{stderr}");
    assert_eq!(text(&output.stdout), "to-stdout\n");
    assert!(stderr.ends_with("to-stderr\n"), "{stderr}");
    let data_dir = fs::canonicalize(env.data_dir()).unwrap();
    assert_eq!(
        fs::read_to_string(&fd3_path).unwrap(),
        format!("token=tok123\nargs=one --two\ncwd={}\n", data_dir.display())
    );
}

#[cfg(target_os = "linux")]
#[test]
fn sandboxed_spec_fails_closed_until_the_backend_lands() {
    let env = Env::new();
    let marker = env.dir.path().join("ran");
    let script = format!("touch '{}'", marker.display());
    let (output, stderr) =
        run(env.command(&Spec::new(env.data_dir()), &["/bin/sh", "-c", &script]));
    assert_eq!(output.status.code(), Some(125), "{stderr}");
    assert!(
        stderr.contains("sandbox is not implemented yet"),
        "{stderr}"
    );
    assert!(stderr.contains("refusing to run"), "{stderr}");
    assert!(!marker.exists(), "the command ran");
}

#[test]
fn trampoline_failures_exit_125_and_name_the_cause() {
    let env = Env::new();
    let fails_with = |cmd: Command, needle: &str| {
        let (output, stderr) = run(cmd);
        assert_eq!(output.status.code(), Some(125), "{stderr}");
        assert!(stderr.contains(needle), "{needle:?} not in:\n{stderr}");
    };

    let relative_command = env.command(&env.unsandboxed(), &["sh", "-c", "true"]);
    fails_with(relative_command, "the command sh is not an absolute path");

    let missing_data_dir = env.command(
        &Spec {
            unsandboxed: true,
            ..Spec::new(env.dir.path().join("missing"))
        },
        &["/bin/sh", "-c", "true"],
    );
    fails_with(missing_data_dir, "unusable data directory");

    let bad_grant = env.command(&env.unsandboxed(), &["/bin/sh", "-c", "true"]);
    let json = format!(
        r#"{{"data_dir": "{}", "unsandboxed": true, "read": ["etc"]}}"#,
        env.data_dir().display()
    );
    fs::write(env.spec_file(), json).unwrap();
    fails_with(bad_grant, "`read` path etc is not absolute");

    let missing_spec = env.command(&env.unsandboxed(), &["/bin/sh", "-c", "true"]);
    fs::remove_file(env.spec_file()).unwrap();
    fails_with(missing_spec, "cannot read sandbox spec");

    let mut bad_log_filter = env.command(&env.unsandboxed(), &["/bin/sh", "-c", "true"]);
    bad_log_filter.env("GILES_LOG", "[");
    fails_with(bad_log_filter, "GILES_LOG");
}

#[test]
fn exec_failures_exit_126_or_127_like_env() {
    let env = Env::new();
    let missing = env.dir.path().join("missing-plugin");
    let (output, stderr) = run(env.command(&env.unsandboxed(), &[missing.to_str().unwrap()]));
    assert_eq!(output.status.code(), Some(127), "{stderr}");
    assert!(stderr.contains("cannot run"), "{stderr}");

    let not_executable = env.dir.path().join("plugin");
    fs::write(&not_executable, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&not_executable, fs::Permissions::from_mode(0o644)).unwrap();
    let (output, stderr) =
        run(env.command(&env.unsandboxed(), &[not_executable.to_str().unwrap()]));
    assert_eq!(output.status.code(), Some(126), "{stderr}");
    assert!(stderr.contains("cannot run"), "{stderr}");
}
