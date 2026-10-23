//! The macOS backend: a generated Seatbelt profile applied through `sandbox-exec` (PLAN section
//! 12.3).

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::backend::{Launch, SandboxBackend};
use super::host::{HostError, HostReport};
use super::{SandboxError, Spec};

/// Where macOS ships `sandbox-exec`.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// A deny-by-default Seatbelt profile, applied by wrapping the command in `sandbox-exec`.
pub(crate) struct Macos;

impl SandboxBackend for Macos {
    /// `/usr/bin/sandbox-exec` must be an executable file.
    fn check_host() -> Result<HostReport, HostError> {
        check_sandbox_exec(Path::new(SANDBOX_EXEC))
    }

    /// Staged: TASKS.md 2.14 generates the profile and wraps the launch in `sandbox-exec`. Until
    /// then this fails closed.
    fn apply(_spec: &Spec, _launch: Launch) -> Result<Launch, SandboxError> {
        Err(SandboxError::NotImplemented {
            sandbox: "macOS (Seatbelt)",
            task: "2.14",
        })
    }
}

/// Checks that `path` is an executable regular file (following symlinks).
fn check_sandbox_exec(path: &Path) -> Result<HostReport, HostError> {
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
    Ok(HostReport::Macos {
        sandbox_exec: path.to_owned(),
    })
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
        assert_eq!(
            check_sandbox_exec(&file).unwrap(),
            HostReport::Macos { sandbox_exec: file }
        );
    }

    /// Every supported macOS ships `/usr/bin/sandbox-exec`.
    #[test]
    fn this_host_has_sandbox_exec() {
        assert_eq!(
            Macos::check_host().unwrap(),
            HostReport::Macos {
                sandbox_exec: SANDBOX_EXEC.into()
            }
        );
    }
}
