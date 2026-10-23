//! The Linux backend: Landlock and seccomp (PLAN section 12.2). The Landlock rules are in
//! [`ruleset`], the paths they're built from in [`paths`], and the seccomp filter in [`seccomp`];
//! `Docs/technical/sandbox.md` explains them.

mod paths;
mod ruleset;
mod seccomp;
mod syscalls;

use std::io;
use std::ptr;

use libc::{c_int, c_long};
use tracing::{info, warn};

use super::backend::{Launch, SandboxBackend};
use super::host::{HostError, HostReport};
use super::{Network, SandboxError, Spec};

/// `LANDLOCK_CREATE_RULESET_VERSION` from `<linux/landlock.h>` (not in `libc`): with a null
/// attribute and size 0, `landlock_create_ruleset` returns the newest ABI version.
const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1 << 0;

/// Landlock for the filesystem and TCP ports, a seccomp allowlist for syscalls and socket
/// families.
pub(crate) struct Linux;

impl SandboxBackend for Linux {
    /// Landlock must be enabled (ABI 1 or later) and seccomp must support filters. Reports the
    /// effective Landlock ABI.
    fn check_host() -> Result<HostReport, HostError> {
        let landlock_abi = effective_abi(landlock_abi()?);
        seccomp_filters()?;
        Ok(HostReport::Linux { landlock_abi })
    }

    /// Restricts this process with Landlock and then seccomp, which both survive `exec`, and
    /// returns `launch` unchanged. Fails (and nothing runs) when Landlock is missing, when its
    /// ABI is below `min_abi`, or when either can't be applied.
    fn apply(spec: &Spec, launch: Launch) -> Result<Launch, SandboxError> {
        let abi = effective_abi(landlock_abi().map_err(SandboxError::Host)?);
        if let Some(required) = spec.min_abi
            && abi < required
        {
            return Err(SandboxError::LandlockAbi {
                required,
                available: abi,
            });
        }
        let lacking = lacking(abi, spec);
        if lacking.is_empty() {
            info!(landlock_abi = abi, "sandboxing with Landlock and seccomp");
        } else {
            warn!(
                landlock_abi = abi,
                lacking = %lacking.join("; "),
                "sandboxing with Landlock and seccomp, but this kernel's Landlock ABI lacks \
                 protections this launch would use",
            );
        }
        // Built before anything is restricted; installing it only makes syscalls.
        let filters =
            seccomp::filters(spec, abi).map_err(|err| SandboxError::Seccomp(err.into()))?;
        ruleset::restrict(spec, &launch.program)?;
        seccomp::apply(&filters).map_err(|err| SandboxError::Seccomp(err.into()))?;
        Ok(launch)
    }
}

/// The Landlock ABI the sandbox uses on a kernel offering `kernel`: the kernel's, capped at the
/// newest this backend knows.
fn effective_abi(kernel: u32) -> u32 {
    kernel.min(ruleset::ABI as u32)
}

/// What Landlock ABI `abi` can't enforce of `spec` (PLAN section 12.2's table), for the log.
fn lacking(abi: u32, spec: &Spec) -> Vec<&'static str> {
    let mut lacking = Vec::new();
    if abi < seccomp::TRUNCATE_ABI {
        lacking.push(
            "truncation rules (ABI 3, Linux 6.2): seccomp denies path-based truncate instead",
        );
    }
    if abi < 4 && matches!(spec.network, Network::Tcp(_)) {
        lacking.push("TCP port rules (ABI 4, Linux 6.7): the port list allows all IP traffic");
    }
    if abi < 6 {
        lacking.push(
            "signal and abstract Unix socket scoping (ABI 6, Linux 6.12): the plugin can signal \
             any process of the same user, including Giles",
        );
    }
    if abi < 9 && !spec.unix.is_empty() {
        lacking.push(
            "Unix socket rules (ABI 9, Linux 7.1): the `unix` grants allow connecting to every \
             socket the plugin can reach",
        );
    }
    lacking
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
    use std::ffi::OsStr;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Output, Stdio};

    use super::*;

    #[test]
    fn the_effective_abi_is_capped_at_the_newest_known() {
        assert_eq!(effective_abi(1), 1);
        assert_eq!(effective_abi(6), 6);
        assert_eq!(effective_abi(9), 9);
        assert_eq!(effective_abi(12), 9);
    }

    #[test]
    fn lacking_follows_the_abi_table() {
        let plain = Spec::new("/d");
        let ports = Spec {
            network: Network::Tcp([443].into()),
            unix: vec!["/run/s".into()],
            ..Spec::new("/d")
        };
        let count = |abi, spec: &Spec| lacking(abi, spec).len();
        assert_eq!(count(1, &plain), 2);
        assert_eq!(count(1, &ports), 4);
        assert_eq!(count(3, &ports), 3);
        assert_eq!(count(4, &ports), 2);
        assert_eq!(count(4, &plain), 1);
        assert_eq!(count(6, &plain), 0);
        assert_eq!(count(6, &ports), 1);
        assert_eq!(count(9, &ports), 0);
        assert!(lacking(5, &plain)[0].contains("including Giles"));
    }

    /// Set in a child to turn [`seccomp_probe`] into the probe: `off` or `network`.
    const PROBE: &str = "GILES_SECCOMP_PROBE";

    /// The spec and Landlock ABI each probe mode runs under.
    fn probe_setup(mode: &str) -> (Spec, u32) {
        match mode {
            // ABI 2: seccomp also denies path-based `truncate`.
            "off" => (Spec::new("/d"), 2),
            "network" => (
                Spec {
                    network: Network::All,
                    ..Spec::new("/d")
                },
                6,
            ),
            other => panic!("unknown probe mode {other}"),
        }
    }

    /// Runs `program` with `args` and `env` under the seccomp filters of probe `mode`, installed
    /// between `fork` and `exec` as the trampoline does before its `exec` (Landlock isn't
    /// applied: these tests run on kernels without it too).
    #[allow(unsafe_code)]
    fn under_filters(mode: &str, program: &OsStr, args: &[&str], env: &[(&str, &str)]) -> Output {
        let (spec, abi) = probe_setup(mode);
        let filters = seccomp::filters(&spec, abi).unwrap();
        let mut cmd = Command::new(program);
        cmd.args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null());
        // SAFETY: the closure runs in the forked child and only makes the `prctl` and `seccomp`
        // syscalls on memory allocated before the fork; it allocates only to report an error.
        unsafe {
            cmd.pre_exec(move || seccomp::apply(&filters).map_err(io::Error::other));
        }
        cmd.output().unwrap()
    }

    fn run_probe(mode: &str) {
        let exe = std::env::current_exe().unwrap();
        let output = under_filters(
            mode,
            exe.as_os_str(),
            &[
                "sandbox::linux::tests::seccomp_probe",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ],
            &[(PROBE, mode)],
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("probe: ok"),
            "{}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
    }

    /// A Rust program (this test binary, threads and all) runs under the filters, and each
    /// syscall gets the filter's answer from the real kernel.
    #[test]
    fn seccomp_filters_on_this_kernel() {
        run_probe("off");
        run_probe("network");
    }

    /// A glibc program (the shell) and the programs it runs work under the filters.
    #[test]
    fn a_shell_runs_under_the_filters() {
        let output = under_filters(
            "off",
            OsStr::new("/bin/sh"),
            &[
                "-c",
                "echo $((1 + 2)); /bin/ls / >/dev/null && echo ls; date >/dev/null",
            ],
            &[],
        );
        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "3\nls\n");
    }

    /// The errno of a syscall that just returned `ret`, or 0 for success.
    fn errno_of(ret: c_long) -> i32 {
        if ret < 0 { errno() } else { 0 }
    }

    /// The probe for [`seccomp_filters_on_this_kernel`]; does nothing in a normal test run.
    #[test]
    #[allow(unsafe_code)]
    fn seccomp_probe() {
        let Ok(mode) = std::env::var(PROBE) else {
            return;
        };
        let network = mode == "network";
        let syscall0 = |nr: c_long| {
            // SAFETY: every argument is 0 (null pointers, zero sizes, no flags); the syscalls
            // used here then read and write no memory of ours and create nothing that outlives
            // the probe.
            errno_of(unsafe { libc::syscall(nr, 0, 0, 0, 0, 0) })
        };

        // The denied list: EPERM.
        assert_eq!(syscall0(libc::SYS_io_uring_setup), libc::EPERM);
        assert_eq!(syscall0(libc::SYS_ptrace), libc::EPERM, "PTRACE_TRACEME");
        assert_eq!(syscall0(libc::SYS_bpf), libc::EPERM);
        assert_eq!(syscall0(libc::SYS_userfaultfd), libc::EPERM);
        // System V IPC and POSIX message queues (without the filter: EINVAL, EFAULT, or a new
        // private queue).
        for nr in [
            libc::SYS_shmget,
            libc::SYS_shmat,
            libc::SYS_shmctl,
            libc::SYS_msgget,
            libc::SYS_msgsnd,
            libc::SYS_semget,
            libc::SYS_semop,
            libc::SYS_mq_open,
            libc::SYS_mq_unlink,
        ] {
            assert_eq!(syscall0(nr), libc::EPERM, "syscall {nr}");
        }
        // Unlisted: ENOSYS (without the filter: EPERM for lack of privileges, EINVAL, ...).
        assert_eq!(syscall0(libc::SYS_mount), libc::ENOSYS);
        assert_eq!(syscall0(libc::SYS_acct), libc::ENOSYS);
        assert_eq!(syscall0(libc::SYS_clone3), libc::ENOSYS);
        assert_eq!(syscall0(libc::SYS_unshare), libc::ENOSYS);
        // Docker's conditions.
        let user_ns = (libc::CLONE_NEWUSER | libc::SIGCHLD) as c_long;
        // SAFETY: as above; a child, should the filter fail to stop it, exits at once.
        let ret = unsafe { libc::syscall(libc::SYS_clone, user_ns, 0, 0, 0, 0) };
        if ret == 0 {
            // SAFETY: `_exit` in the child of a raw `clone`.
            unsafe { libc::_exit(0) };
        }
        assert_eq!(errno_of(ret), libc::EPERM, "clone(CLONE_NEWUSER)");
        // SAFETY: as above; ADDR_NO_RANDOMIZE would only change this process.
        let ret = unsafe { libc::syscall(libc::SYS_personality, 0x0040000) };
        assert_eq!(errno_of(ret), libc::EPERM, "personality(ADDR_NO_RANDOMIZE)");

        // Socket families.
        let socket = |family| {
            // SAFETY: `socket` takes no pointers; a new descriptor is closed right away.
            let fd = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            let errno = errno_of(fd.into());
            if fd >= 0 {
                // SAFETY: `fd` is ours.
                unsafe { libc::close(fd) };
            }
            errno
        };
        let expected = if network { 0 } else { libc::EPERM };
        assert_eq!(socket(libc::AF_INET), expected, "AF_INET");
        assert_eq!(socket(libc::AF_INET6), expected, "AF_INET6");
        assert_eq!(socket(libc::AF_UNIX), libc::EPERM, "AF_UNIX");
        assert_eq!(socket(libc::AF_NETLINK), libc::EPERM, "AF_NETLINK");
        let (mut a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
        std::io::Write::write_all(&mut a, b"p").unwrap();
        let mut byte = [0];
        std::io::Read::read_exact(&mut b, &mut byte).unwrap();

        // Path-based truncate below Landlock ABI 3.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "data").unwrap();
        let truncated = std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(1);
        assert!(truncated.is_ok(), "ftruncate: {truncated:?}");
        let path = std::ffi::CString::new(file.to_str().unwrap()).unwrap();
        // SAFETY: `path` is a valid NUL-terminated string.
        let ret = unsafe { libc::truncate(path.as_ptr(), 0) };
        let expected = if network { 0 } else { libc::EPERM };
        assert_eq!(errno_of(ret.into()), expected, "truncate");

        // Threads and the clock.
        let handles: Vec<_> = (0..4).map(|i| std::thread::spawn(move || i * 2)).collect();
        let sum: i32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(sum, 12);
        assert!(std::time::SystemTime::now() > std::time::UNIX_EPOCH);
        println!("probe: ok");
    }

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
    /// With `GILES_REQUIRE_LANDLOCK=1` (CI's Linux job), Landlock must be available.
    #[test]
    fn host_probes_run() {
        let result = Linux::check_host();
        eprintln!("host check: {result:?}");
        match result {
            Ok(HostReport::Linux { landlock_abi }) => assert!(landlock_abi >= 1),
            Ok(other) => panic!("{other:?}"),
            Err(err) => {
                assert!(
                    matches!(
                        err,
                        HostError::LandlockMissing | HostError::LandlockDisabled
                    ),
                    "{err}"
                );
                assert!(
                    std::env::var_os("GILES_REQUIRE_LANDLOCK").is_none_or(|v| v != "1"),
                    "GILES_REQUIRE_LANDLOCK=1, but: {err}"
                );
            }
        }
        // seccomp is needed by every container runtime and CI kernel we run on.
        seccomp_filters().unwrap();
    }
}
