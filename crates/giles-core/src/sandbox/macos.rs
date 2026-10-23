//! The macOS backend: a generated deny-by-default Seatbelt profile applied through `sandbox-exec`
//! (PLAN section 12.3). The rules are in [`profile`]; `Docs/technical/sandbox.md` explains them.

mod paths;
mod profile;

use std::ffi::{CString, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use self::profile::Profile;
use super::backend::{Launch, SandboxBackend};
use super::host::{HostError, HostReport};
use super::{SandboxError, Spec};

/// Where macOS ships `sandbox-exec`.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The host check's profile: it only has to compile and apply.
const PROBE_PROFILE: &str = "(version 1)(allow default)";

/// The host check's command.
const PROBE_COMMAND: &str = "/usr/bin/true";

/// A deny-by-default Seatbelt profile, applied by wrapping the command in `sandbox-exec`.
pub(crate) struct Macos;

impl SandboxBackend for Macos {
    /// `/usr/bin/sandbox-exec` must be an executable file and able to apply a profile.
    fn check_host() -> Result<HostReport, HostError> {
        let path = Path::new(SANDBOX_EXEC);
        check_sandbox_exec(path)?;
        probe_sandbox_exec(path)?;
        Ok(HostReport::Macos {
            sandbox_exec: path.to_owned(),
        })
    }

    /// Returns `sandbox-exec -p <profile> <program> <args>`. `sandbox-exec` applies the profile
    /// to itself and `exec`s the program, so the pid, the file descriptors, the environment, and
    /// the working directory pass through.
    ///
    /// `sandbox-exec` reports its own `exec` failures with status 71, so the program is checked
    /// here first, keeping the trampoline's 126 and 127.
    fn apply(spec: &Spec, launch: Launch) -> Result<Launch, SandboxError> {
        if let Err(source) = check_executable(&launch.program) {
            return Err(SandboxError::Exec {
                program: launch.program,
                source,
            });
        }
        let profile = Profile::new(spec, &launch.program)?.render();
        let mut args = Vec::with_capacity(launch.args.len() + 3);
        args.push(OsString::from("-p"));
        args.push(OsString::from_vec(profile));
        args.push(launch.program.into_os_string());
        args.extend(launch.args);
        Ok(Launch {
            program: SANDBOX_EXEC.into(),
            args,
        })
    }
}

/// Checks that `path` is an executable regular file (following symlinks).
fn check_sandbox_exec(path: &Path) -> Result<(), HostError> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(HostError::SandboxExecMissing(path.to_owned()));
        }
        Err(error) => {
            return Err(HostError::SandboxExecProbe {
                path: path.to_owned(),
                error,
            });
        }
    };
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(HostError::SandboxExecNotExecutable(path.to_owned()));
    }
    Ok(())
}

/// Runs `<path> -p '(version 1)(allow default)' /usr/bin/true`: fails when the profile can't be
/// applied, e.g. because Giles itself runs in a sandbox that forbids it.
fn probe_sandbox_exec(path: &Path) -> Result<(), HostError> {
    let output = Command::new(path)
        .args(["-p", PROBE_PROFILE, PROBE_COMMAND])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| HostError::SandboxExecProbe {
            path: path.to_owned(),
            error,
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(HostError::SandboxExecFailed {
        path: path.to_owned(),
        status: output.status,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// What `execve(program)` would fail with, roughly: missing (`NotFound`), not a regular file, or
/// not executable by this process (`PermissionDenied`).
#[allow(unsafe_code)]
fn check_executable(program: &Path) -> io::Result<()> {
    if !fs::metadata(program)?.is_file() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    let path = CString::new(program.as_os_str().as_bytes())?;
    // SAFETY: `path` is a valid NUL-terminated string that outlives the call; `access` only
    // reads it.
    if unsafe { libc::access(path.as_ptr(), libc::X_OK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;

    use super::*;

    #[test]
    fn sandbox_exec_must_be_an_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(matches!(
            check_sandbox_exec(&missing),
            Err(HostError::SandboxExecMissing(p)) if p == missing
        ));
        assert!(matches!(
            check_sandbox_exec(dir.path()),
            Err(HostError::SandboxExecNotExecutable(_))
        ));
        let file = dir.path().join("sandbox-exec");
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            check_sandbox_exec(&file),
            Err(HostError::SandboxExecNotExecutable(_))
        ));
        fs::set_permissions(&file, Permissions::from_mode(0o755)).unwrap();
        check_sandbox_exec(&file).unwrap();
    }

    #[test]
    fn probe_reports_a_sandbox_exec_that_cannot_apply_a_profile() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("sandbox-exec");
        fs::write(
            &fake,
            "#!/bin/sh\necho 'sandbox-exec: sandbox_apply: Operation not permitted' >&2\nexit 71\n",
        )
        .unwrap();
        fs::set_permissions(&fake, Permissions::from_mode(0o755)).unwrap();
        let err = probe_sandbox_exec(&fake).unwrap_err();
        assert!(matches!(err, HostError::SandboxExecFailed { .. }), "{err}");
        let message = err.to_string();
        assert!(message.contains("exit status: 71"), "{message}");
        assert!(
            message.contains("sandbox_apply: Operation not permitted"),
            "{message}"
        );
    }

    /// Every supported macOS ships `/usr/bin/sandbox-exec`, and it can apply a profile.
    #[test]
    fn this_host_can_apply_a_profile() {
        assert_eq!(
            Macos::check_host().unwrap(),
            HostReport::Macos {
                sandbox_exec: SANDBOX_EXEC.into()
            }
        );
    }

    #[test]
    fn programs_are_checked_like_execve() {
        let dir = tempfile::tempdir().unwrap();
        let kind = |path: &Path| check_executable(path).unwrap_err().kind();
        assert_eq!(kind(&dir.path().join("missing")), io::ErrorKind::NotFound);
        assert_eq!(kind(dir.path()), io::ErrorKind::PermissionDenied);
        let file = dir.path().join("plugin");
        fs::write(&file, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&file, Permissions::from_mode(0o644)).unwrap();
        assert_eq!(kind(&file), io::ErrorKind::PermissionDenied);
        fs::set_permissions(&file, Permissions::from_mode(0o755)).unwrap();
        check_executable(&file).unwrap();
    }

    #[test]
    fn apply_wraps_the_launch_in_sandbox_exec() {
        let dir = tempfile::tempdir().unwrap();
        let spec = Spec::new(dir.path());
        let launch = Launch {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "true".into()],
        };
        let wrapped = Macos::apply(&spec, launch).unwrap();
        assert_eq!(wrapped.program, Path::new(SANDBOX_EXEC));
        let [flag, profile, program, rest @ ..] = wrapped.args.as_slice() else {
            panic!("{wrapped:?}");
        };
        assert_eq!(flag, "-p");
        assert!(
            profile
                .as_bytes()
                .starts_with(b"(version 1)\n(deny default)\n"),
            "{profile:?}"
        );
        assert_eq!(program, "/bin/sh");
        assert_eq!(rest, ["-c", "true"]);

        let missing = Launch {
            program: dir.path().join("missing"),
            args: Vec::new(),
        };
        let err = Macos::apply(&spec, missing).unwrap_err();
        assert_eq!(err.exit_code(), super::super::EXIT_NOT_FOUND, "{err}");
    }
}
