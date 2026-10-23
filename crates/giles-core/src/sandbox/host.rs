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

impl HostReport {
    /// The protections this host's sandbox can't give a plugin, for the operator: on Linux, what
    /// the effective Landlock ABI lacks (PLAN section 12.2's table); nothing on macOS.
    /// `tcp_ports`: the plugin's network grant is a TCP port list; `unix`: it has Unix socket
    /// grants.
    pub fn lacking(&self, tcp_ports: bool, unix: bool) -> Vec<&'static str> {
        match self {
            Self::Linux { landlock_abi } => landlock_lacking(*landlock_abi, tcp_ports, unix),
            Self::Macos { .. } => Vec::new(),
        }
    }
}

/// The first Landlock ABI that restricts `truncate` itself; below it seccomp denies path-based
/// `truncate` (`ftruncate` needs a descriptor opened for writing, which Landlock checks).
pub(super) const TRUNCATE_ABI: u32 = 3;

/// What Landlock ABI `abi` can't enforce for a plugin (PLAN section 12.2's table). `tcp_ports`:
/// its network grant is a TCP port list; `unix`: it has Unix socket grants.
pub(super) fn landlock_lacking(abi: u32, tcp_ports: bool, unix: bool) -> Vec<&'static str> {
    let mut lacking = Vec::new();
    if abi < TRUNCATE_ABI {
        lacking.push(
            "truncation rules (ABI 3, Linux 6.2): seccomp denies path-based truncate instead",
        );
    }
    if abi < 4 && tcp_ports {
        lacking.push("TCP port rules (ABI 4, Linux 6.7): the port list allows all IP traffic");
    }
    if abi < 6 {
        lacking.push(
            "signal and abstract Unix socket scoping (ABI 6, Linux 6.12): the plugin can signal \
             any process of the same user, including Giles",
        );
    }
    if abi < 9 && unix {
        lacking.push(
            "Unix socket rules (ABI 9, Linux 7.1): the `unix` grants allow connecting to every \
             socket the plugin can reach",
        );
    }
    lacking
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lacking_follows_the_abi_table() {
        let count = |abi, ports| landlock_lacking(abi, ports, ports).len();
        assert_eq!(count(1, false), 2);
        assert_eq!(count(1, true), 4);
        assert_eq!(count(3, true), 3);
        assert_eq!(count(4, true), 2);
        assert_eq!(count(4, false), 1);
        assert_eq!(count(6, false), 0);
        assert_eq!(count(6, true), 1);
        assert_eq!(count(9, true), 0);
        assert!(landlock_lacking(5, false, false)[0].contains("including Giles"));
    }

    #[test]
    fn a_host_report_lists_what_its_sandbox_lacks() {
        let linux = HostReport::Linux { landlock_abi: 5 };
        assert_eq!(
            linux.lacking(false, false),
            landlock_lacking(5, false, false)
        );
        // Scoping (ABI 6) and Unix socket rules (ABI 9).
        assert_eq!(linux.lacking(true, true).len(), 2);
        let macos = HostReport::Macos {
            sandbox_exec: "/usr/bin/sandbox-exec".into(),
        };
        assert!(macos.lacking(true, true).is_empty());
    }
}
