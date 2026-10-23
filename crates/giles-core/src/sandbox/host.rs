//! The startup host check (PLAN section 12.1): can this host apply the sandbox at all?

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

/// What the host offers, when the sandbox can be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostReport {
    /// Linux with Landlock and seccomp filters.
    Linux {
        /// The effective Landlock ABI (PLAN section 12.2's table): the newest the kernel offers,
        /// capped at the newest the sandbox knows.
        landlock_abi: u32,
    },
    /// macOS with `sandbox-exec`.
    Macos {
        /// The `sandbox-exec` binary.
        sandbox_exec: PathBuf,
    },
}

impl fmt::Display for HostReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Linux { landlock_abi } => {
                write!(f, "Landlock ABI {landlock_abi} and seccomp filters")
            }
            Self::Macos { sandbox_exec } => write!(f, "Seatbelt via {}", sandbox_exec.display()),
        }
    }
}

/// The one reason the host can't apply the sandbox. The messages name the cause and, where there
/// is one, the fix; the underlying error is part of the message so it logs as one line.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HostError {
    /// `landlock_create_ruleset` fails with `ENOSYS`: the kernel lacks it, or a container's
    /// seccomp profile denies unlisted syscalls with `ENOSYS` (Podman's default does).
    #[error(
        "Landlock is not available: the kernel is older than 5.13 or was built without \
         CONFIG_SECURITY_LANDLOCK, or a container's seccomp profile blocks \
         landlock_create_ruleset"
    )]
    LandlockMissing,
    /// Landlock is compiled in but not enabled at boot.
    #[error(
        "Landlock is built into the kernel but not enabled: add `landlock` to the `lsm=` kernel \
         boot parameter"
    )]
    LandlockDisabled,
    /// The Landlock probe failed some other way.
    #[error(
        "cannot probe Landlock: {0} (a container's seccomp profile may block \
         landlock_create_ruleset)"
    )]
    LandlockProbe(io::Error),
    /// The kernel has no seccomp.
    #[error("seccomp is not available: the kernel was built without CONFIG_SECCOMP")]
    SeccompMissing,
    /// The kernel has seccomp but no filter mode.
    #[error(
        "seccomp filters are not available: the kernel was built without CONFIG_SECCOMP_FILTER"
    )]
    SeccompFilterMissing,
    /// The seccomp probe failed some other way.
    #[error("cannot probe seccomp: {0}")]
    SeccompProbe(io::Error),
    /// `sandbox-exec` is missing.
    #[error("{} is missing; the macOS sandbox needs it", .0.display())]
    SandboxExecMissing(PathBuf),
    /// `sandbox-exec` exists but isn't an executable file.
    #[error("{} is not an executable file; the macOS sandbox needs it", .0.display())]
    SandboxExecNotExecutable(PathBuf),
    /// `sandbox-exec` can't be checked.
    #[error("cannot check {}: {error}", .path.display())]
    SandboxExecProbe {
        /// The `sandbox-exec` path.
        path: PathBuf,
        /// What went wrong.
        error: io::Error,
    },
    /// `sandbox-exec` can't apply even an allow-all profile, e.g. because Giles itself runs in a
    /// sandbox that forbids it.
    #[error("{} cannot apply a sandbox profile ({status}): {stderr}", .path.display())]
    SandboxExecFailed {
        /// The `sandbox-exec` path.
        path: PathBuf,
        /// Its exit status.
        status: ExitStatus,
        /// What it printed on stderr.
        stderr: String,
    },
}
