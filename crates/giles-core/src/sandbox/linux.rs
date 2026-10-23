//! The Linux backend: Landlock and seccomp (PLAN section 12.2).

use std::io;
use std::ptr;

use libc::{c_int, c_long};

use super::backend::{Launch, SandboxBackend};
use super::host::{HostError, HostReport};
use super::{SandboxError, Spec};

/// `LANDLOCK_CREATE_RULESET_VERSION` from `<linux/landlock.h>` (not in `libc`): with a null
/// attribute and size 0, `landlock_create_ruleset` returns the newest ABI version.
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1 << 0;

/// Landlock for the filesystem and TCP ports, a seccomp allowlist for syscalls and socket
/// families.
pub(crate) struct Linux;

impl SandboxBackend for Linux {
    /// Landlock must be enabled (ABI 1 or later) and seccomp must support filters.
    fn check_host() -> Result<HostReport, HostError> {
        let landlock_abi = landlock_abi()?;
        seccomp_filters()?;
        Ok(HostReport::Linux { landlock_abi })
    }

    /// Staged: TASKS.md 2.13 builds the Landlock ruleset (newest ABI, `min_abi` enforced) and the
    /// seccomp filter. Until then this fails closed.
    fn apply(_spec: &Spec, _launch: Launch) -> Result<Launch, SandboxError> {
        Err(SandboxError::NotImplemented {
            sandbox: "Linux (Landlock and seccomp)",
            task: "2.13",
        })
    }
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// The kernel's newest Landlock ABI.
#[allow(unsafe_code)]
fn landlock_abi() -> Result<u32, HostError> {
    // SAFETY: with a null attribute pointer, size 0, and only the VERSION flag, the kernel reads
    // and writes no memory and creates no file descriptor; it only returns the ABI version.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            ptr::null::<libc::c_void>(),
            0_usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    landlock_outcome(ret, if ret < 0 { errno() } else { 0 })
}

/// Interprets `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`.
fn landlock_outcome(ret: c_long, errno: i32) -> Result<u32, HostError> {
    if ret >= 1 {
        return u32::try_from(ret).map_err(|_| {
            HostError::LandlockProbe(io::Error::other(format!("impossible ABI version {ret}")))
        });
    }
    Err(match errno {
        libc::ENOSYS => HostError::LandlockMissing,
        libc::EOPNOTSUPP => HostError::LandlockDisabled,
        _ if ret == 0 => HostError::LandlockProbe(io::Error::other("ABI version 0")),
        errno => HostError::LandlockProbe(io::Error::from_raw_os_error(errno)),
    })
}

/// Checks for seccomp filter mode the way systemd does: `PR_GET_SECCOMP` fails with `EINVAL`
/// without `CONFIG_SECCOMP`, and installing a null filter fails with `EFAULT` only when filters
/// are supported (`EINVAL` otherwise).
#[allow(unsafe_code)]
fn seccomp_filters() -> Result<(), HostError> {
    let unused: libc::c_ulong = 0;
    // SAFETY: PR_GET_SECCOMP takes no pointers and changes nothing.
    let ret = unsafe { libc::prctl(libc::PR_GET_SECCOMP, unused, unused, unused, unused) };
    seccomp_mode_outcome(ret, if ret < 0 { errno() } else { 0 })?;
    // SAFETY: the filter pointer is null, so the kernel fails to copy the program (EFAULT) or
    // rejects the mode (EINVAL) before installing anything; no memory of ours is accessed.
    let ret = unsafe {
        libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::c_ulong::from(libc::SECCOMP_MODE_FILTER),
            ptr::null::<libc::c_void>(),
            unused,
            unused,
        )
    };
    seccomp_filter_outcome(ret, if ret < 0 { errno() } else { 0 })
}

/// Interprets `prctl(PR_GET_SECCOMP)`: 0 (no filter yet) and 2 (already filtered, e.g. by a
/// container runtime; filters stack) are fine.
fn seccomp_mode_outcome(ret: c_int, errno: i32) -> Result<(), HostError> {
    match (ret, errno) {
        (0 | 2, _) => Ok(()),
        (-1, libc::EINVAL) => Err(HostError::SeccompMissing),
        (-1, errno) => Err(HostError::SeccompProbe(io::Error::from_raw_os_error(errno))),
        (mode, _) => Err(HostError::SeccompProbe(io::Error::other(format!(
            "unexpected seccomp mode {mode}"
        )))),
    }
}

/// Interprets `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, NULL)`.
fn seccomp_filter_outcome(ret: c_int, errno: i32) -> Result<(), HostError> {
    match (ret, errno) {
        (-1, libc::EFAULT) => Ok(()),
        (-1, libc::EINVAL) => Err(HostError::SeccompFilterMissing),
        (-1, errno) => Err(HostError::SeccompProbe(io::Error::from_raw_os_error(errno))),
        (ret, _) => Err(HostError::SeccompProbe(io::Error::other(format!(
            "installing a null seccomp filter returned {ret}"
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn landlock_outcomes() {
        assert_eq!(landlock_outcome(6, 0).unwrap(), 6);
        assert!(matches!(
            landlock_outcome(-1, libc::ENOSYS),
            Err(HostError::LandlockMissing)
        ));
        assert!(matches!(
            landlock_outcome(-1, libc::EOPNOTSUPP),
            Err(HostError::LandlockDisabled)
        ));
        let err = landlock_outcome(-1, libc::EPERM).unwrap_err();
        assert!(matches!(err, HostError::LandlockProbe(_)));
        assert!(err.to_string().contains("seccomp profile"), "{err}");
        assert!(matches!(
            landlock_outcome(0, 0),
            Err(HostError::LandlockProbe(_))
        ));
    }

    #[test]
    fn seccomp_outcomes() {
        seccomp_mode_outcome(0, 0).unwrap();
        seccomp_mode_outcome(2, 0).unwrap();
        assert!(matches!(
            seccomp_mode_outcome(-1, libc::EINVAL),
            Err(HostError::SeccompMissing)
        ));
        assert!(matches!(
            seccomp_mode_outcome(1, 0),
            Err(HostError::SeccompProbe(_))
        ));
        seccomp_filter_outcome(-1, libc::EFAULT).unwrap();
        assert!(matches!(
            seccomp_filter_outcome(-1, libc::EINVAL),
            Err(HostError::SeccompFilterMissing)
        ));
        assert!(matches!(
            seccomp_filter_outcome(-1, libc::EACCES),
            Err(HostError::SeccompProbe(_))
        ));
        assert!(matches!(
            seccomp_filter_outcome(0, 0),
            Err(HostError::SeccompProbe(_))
        ));
    }

    /// The real probes on this host. Prints what it finds (run with `--nocapture` to see it);
    /// the probes must not fail in an unexpected way, and a working seccomp must be reported.
    #[test]
    fn host_probes_run() {
        let result = Linux::check_host();
        eprintln!("host check: {result:?}");
        match result {
            Ok(HostReport::Linux { landlock_abi }) => assert!(landlock_abi >= 1),
            Ok(other) => panic!("{other:?}"),
            Err(err) => assert!(
                matches!(
                    err,
                    HostError::LandlockMissing | HostError::LandlockDisabled
                ),
                "{err}"
            ),
        }
        // seccomp is needed by every container runtime and CI kernel we run on.
        seccomp_filters().unwrap();
    }
}
