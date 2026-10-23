//! The seccomp filter (PLAN section 12.2), built with `seccompiler`.
//!
//! One launch gets two stacked filters, because a `seccompiler` filter has one action for the
//! syscalls it matches and one for the rest, and a launch needs three outcomes:
//!
//! 1. The allowlist: Docker's default profile ([`super::syscalls::DOCKER_DEFAULT`]) plus every
//!    syscall the second filter decides on is allowed; everything else fails with `ENOSYS`, so
//!    runtimes that probe newer syscalls fall back (glibc's `clone3` -> `clone`).
//! 2. The denials: [`DENIED`], `truncate` below Landlock ABI 3, `socket` for families not granted,
//!    and Docker's conditions on `clone` and `personality` fail with `EPERM`; everything else is
//!    allowed (as far as this filter goes).
//!
//! The kernel runs every filter and takes the most severe result. Every syscall the second filter
//! denies is allowed by the first, so at most one filter returns an errno and the result never
//! depends on how the kernel orders filters with equal severity. Both filters kill the process
//! for any other architecture (32-bit `int 0x80` on x86-64, AArch32), so `socketcall` and the
//! compat syscall tables can't get around them. x32 syscalls (x86-64's ABI with
//! `__X32_SYSCALL_BIT` set) pass the architecture check but match no listed number, so they fail
//! with `ENOSYS`.

use std::collections::BTreeMap;

use libc::c_long;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use super::syscalls::DOCKER_DEFAULT;
use crate::sandbox::host::TRUNCATE_ABI;
use crate::sandbox::{Network, Spec};

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("the Linux sandbox's seccomp table covers only x86_64 and aarch64");

#[cfg(target_arch = "x86_64")]
const ARCH: TargetArch = TargetArch::x86_64;
#[cfg(target_arch = "aarch64")]
const ARCH: TargetArch = TargetArch::aarch64;

/// Always `EPERM` (PLAN section 12.2): io_uring can create and connect sockets without `socket`;
/// System V IPC (shared memory, message queues, semaphores) and POSIX message queues are keyed by
/// number or name in the IPC namespace, which Landlock doesn't cover and which the sandbox shares
/// with the rest of the system (Docker allows them only because each container gets its own IPC
/// namespace); the rest reach into other processes or the kernel.
pub(super) const DENIED: &[(&str, c_long)] = &[
    ("io_uring_setup", libc::SYS_io_uring_setup),
    ("io_uring_enter", libc::SYS_io_uring_enter),
    ("io_uring_register", libc::SYS_io_uring_register),
    ("ptrace", libc::SYS_ptrace),
    ("process_vm_readv", libc::SYS_process_vm_readv),
    ("process_vm_writev", libc::SYS_process_vm_writev),
    ("bpf", libc::SYS_bpf),
    ("perf_event_open", libc::SYS_perf_event_open),
    ("userfaultfd", libc::SYS_userfaultfd),
    ("keyctl", libc::SYS_keyctl),
    ("add_key", libc::SYS_add_key),
    ("request_key", libc::SYS_request_key),
    ("shmget", libc::SYS_shmget),
    ("shmat", libc::SYS_shmat),
    ("shmdt", libc::SYS_shmdt),
    ("shmctl", libc::SYS_shmctl),
    ("msgget", libc::SYS_msgget),
    ("msgsnd", libc::SYS_msgsnd),
    ("msgrcv", libc::SYS_msgrcv),
    ("msgctl", libc::SYS_msgctl),
    ("semget", libc::SYS_semget),
    ("semop", libc::SYS_semop),
    ("semtimedop", libc::SYS_semtimedop),
    ("semctl", libc::SYS_semctl),
    ("mq_open", libc::SYS_mq_open),
    ("mq_unlink", libc::SYS_mq_unlink),
    ("mq_timedsend", libc::SYS_mq_timedsend),
    ("mq_timedreceive", libc::SYS_mq_timedreceive),
    ("mq_notify", libc::SYS_mq_notify),
    ("mq_getsetattr", libc::SYS_mq_getsetattr),
];

/// `clone` flags that create namespaces, which Docker's profile denies without `CAP_SYS_ADMIN`
/// (its mask `0x7E020000`).
const NAMESPACE_FLAGS: &[u64] = &[
    libc::CLONE_NEWNS as u64,
    libc::CLONE_NEWCGROUP as u64,
    libc::CLONE_NEWUTS as u64,
    libc::CLONE_NEWIPC as u64,
    libc::CLONE_NEWUSER as u64,
    libc::CLONE_NEWPID as u64,
    libc::CLONE_NEWNET as u64,
];

/// The `personality` values Docker's profile allows: `PER_LINUX`, `PER_LINUX32`,
/// `UNAME26`, `PER_LINUX32 | UNAME26`, and the query `0xffffffff`.
const PERSONALITIES: &[u64] = &[0, 0x8, 0x20000, 0x20008, 0xffff_ffff];

/// The `socket` address families `spec` grants: none by default, `AF_INET` and `AF_INET6` with
/// any network grant, `AF_UNIX` with any `unix` grant.
pub(super) fn socket_families(spec: &Spec) -> Vec<libc::c_int> {
    let mut families = Vec::new();
    if spec.network != Network::Off {
        families.extend([libc::AF_INET, libc::AF_INET6]);
    }
    if !spec.unix.is_empty() {
        families.push(libc::AF_UNIX);
    }
    families
}

/// The two filters for a launch under `spec` with Landlock ABI `abi`, in the order to install
/// them (see the module docs).
pub(super) fn filters(spec: &Spec, abi: u32) -> Result<[BpfProgram; 2], seccompiler::BackendError> {
    let denials = denials(spec, abi)?;
    let mut allowed: BTreeMap<i64, Vec<SeccompRule>> = DOCKER_DEFAULT
        .iter()
        .map(|&(_, nr)| (nr, Vec::new()))
        .collect();
    // Everything the second filter decides on, so that it alone decides.
    allowed.extend(denials.keys().map(|&nr| (nr, Vec::new())));
    let allow = SeccompFilter::new(
        allowed,
        SeccompAction::Errno(libc::ENOSYS as u32),
        SeccompAction::Allow,
        ARCH,
    )?;
    let deny = SeccompFilter::new(
        denials,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        ARCH,
    )?;
    Ok([allow.try_into()?, deny.try_into()?])
}

/// The second filter's rules: each syscall fails with `EPERM` if any of its rules matches (no
/// rules: always).
fn denials(
    spec: &Spec,
    abi: u32,
) -> Result<BTreeMap<i64, Vec<SeccompRule>>, seccompiler::BackendError> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        DENIED.iter().map(|&(_, nr)| (nr, Vec::new())).collect();
    if abi < TRUNCATE_ABI {
        rules.insert(libc::SYS_truncate, Vec::new());
    }

    // `socket` with any family but the granted ones; with none granted, every `socket`.
    let families = socket_families(spec);
    let socket = if families.is_empty() {
        Vec::new()
    } else {
        vec![SeccompRule::new(
            families
                .iter()
                .map(|&family| arg(0, SeccompCmpOp::Ne, family as u64))
                .collect::<Result<_, _>>()?,
        )?]
    };
    rules.insert(libc::SYS_socket, socket);

    // `clone` with any namespace flag (one rule per flag: rules are alternatives).
    let clone = NAMESPACE_FLAGS
        .iter()
        .map(|&flag| SeccompRule::new(vec![arg(0, SeccompCmpOp::MaskedEq(flag), flag)?]))
        .collect::<Result<_, _>>()?;
    rules.insert(libc::SYS_clone, clone);

    // `personality` with any value but Docker's.
    let personality = SeccompRule::new(
        PERSONALITIES
            .iter()
            .map(|&value| arg(0, SeccompCmpOp::Ne, value))
            .collect::<Result<_, _>>()?,
    )?;
    rules.insert(libc::SYS_personality, vec![personality]);
    Ok(rules)
}

/// A condition on the low 32 bits of argument `index` (every argument checked here is an `int`
/// or flags that fit in 32 bits, and the kernel ignores the high bits of an `int`).
fn arg(
    index: u8,
    op: SeccompCmpOp,
    value: u64,
) -> Result<SeccompCondition, seccompiler::BackendError> {
    SeccompCondition::new(index, SeccompCmpArgLen::Dword, op, value)
}

/// Installs `filters` (from [`filters`]) on the calling thread, after `PR_SET_NO_NEW_PRIVS`.
/// Async-signal-safe: it only makes syscalls, so it may run between `fork` and `exec`.
pub(super) fn apply(filters: &[BpfProgram; 2]) -> Result<(), seccompiler::Error> {
    for filter in filters {
        seccompiler::apply_filter(filter)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};
    use std::path::PathBuf;

    use seccompiler::sock_filter;

    use super::*;

    const ALLOW: u32 = 0x7fff_0000;
    const KILL_PROCESS: u32 = 0x8000_0000;
    const ERRNO: u32 = 0x0005_0000;

    /// The `AUDIT_ARCH_*` value of this architecture.
    #[cfg(target_arch = "x86_64")]
    const NATIVE: u32 = 62 | 0x8000_0000 | 0x4000_0000;
    #[cfg(target_arch = "aarch64")]
    const NATIVE: u32 = 183 | 0x8000_0000 | 0x4000_0000;

    /// What the kernel decides for one syscall.
    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        Allow,
        Errno(i32),
        Kill,
    }

    /// Runs a classic BPF program on a `struct seccomp_data` (the subset seccomp filters use:
    /// absolute word loads, `and`, conditional and unconditional jumps, returns).
    fn run(program: &[sock_filter], arch: u32, nr: i64, args: [u64; 6]) -> u32 {
        let mut data = Vec::with_capacity(64);
        data.extend_from_slice(&(nr as i32).to_ne_bytes());
        data.extend_from_slice(&arch.to_ne_bytes());
        data.extend_from_slice(&0_u64.to_ne_bytes()); // instruction pointer
        for arg in args {
            data.extend_from_slice(&arg.to_ne_bytes());
        }
        let mut acc: u32 = 0;
        let mut pc = 0;
        loop {
            let insn = &program[pc];
            pc += 1;
            match insn.code {
                0x20 => {
                    // BPF_LD | BPF_W | BPF_ABS
                    let at = insn.k as usize;
                    acc = u32::from_ne_bytes(data[at..at + 4].try_into().unwrap());
                }
                0x54 => acc &= insn.k,         // BPF_ALU | BPF_AND | BPF_K
                0x05 => pc += insn.k as usize, // BPF_JMP | BPF_JA
                0x15 | 0x25 | 0x35 => {
                    let taken = match insn.code {
                        0x15 => acc == insn.k, // JEQ
                        0x25 => acc > insn.k,  // JGT
                        _ => acc >= insn.k,    // JGE
                    };
                    pc += usize::from(if taken { insn.jt } else { insn.jf });
                }
                0x06 => return insn.k, // BPF_RET | BPF_K
                code => panic!("unexpected BPF instruction {code:#x} at {}", pc - 1),
            }
        }
    }

    /// The combined result of both filters, as the kernel computes it: the most severe action
    /// wins (`KILL_PROCESS` < `ERRNO` < `ALLOW` as signed values).
    fn outcome(filters: &[BpfProgram; 2], arch: u32, nr: i64, args: [u64; 6]) -> Outcome {
        let results: Vec<u32> = filters.iter().map(|f| run(f, arch, nr, args)).collect();
        let errnos = results.iter().filter(|r| *r & 0xffff_0000 == ERRNO).count();
        assert!(errnos <= 1, "both filters return an errno for {nr}");
        let worst = results.into_iter().min_by_key(|r| *r as i32).unwrap();
        match worst & 0xffff_0000 {
            KILL_PROCESS => Outcome::Kill,
            ERRNO => Outcome::Errno((worst & 0xffff) as i32),
            ALLOW => Outcome::Allow,
            other => panic!("unexpected action {other:#x}"),
        }
    }

    fn spec() -> Spec {
        Spec::new("/data")
    }

    fn check(filters: &[BpfProgram; 2], nr: c_long, args: [u64; 6]) -> Outcome {
        outcome(filters, NATIVE, nr, args)
    }

    const NO_ARGS: [u64; 6] = [0; 6];

    fn socket(family: libc::c_int) -> [u64; 6] {
        [family as u64, libc::SOCK_STREAM as u64, 0, 0, 0, 0]
    }

    #[test]
    fn docker_syscalls_are_allowed_and_others_get_enosys() {
        let filters = filters(&spec(), 6).unwrap();
        let denied: HashSet<&str> = DENIED.iter().map(|(name, _)| *name).collect();
        for &(name, nr) in DOCKER_DEFAULT {
            let expected = if denied.contains(name) {
                Outcome::Errno(libc::EPERM)
            } else {
                Outcome::Allow
            };
            if name != "truncate" {
                assert_eq!(check(&filters, nr, NO_ARGS), expected, "{name}");
            }
        }
        // Docker allows System V IPC and POSIX message queues (in a container's own IPC
        // namespace); the sandbox has none, so they're denied.
        for name in [
            "shmget",
            "shmat",
            "msgget",
            "semget",
            "semtimedop",
            "mq_open",
        ] {
            assert!(denied.contains(name), "{name}");
            assert!(
                DOCKER_DEFAULT.iter().any(|(n, _)| *n == name),
                "{name} isn't in Docker's list"
            );
        }
        for nr in [libc::SYS_clone3, libc::SYS_mount, libc::SYS_unshare, 999] {
            assert_eq!(
                check(&filters, nr, NO_ARGS),
                Outcome::Errno(libc::ENOSYS),
                "{nr}"
            );
        }
        // The table names each syscall once, and only syscalls.
        let names: HashSet<&str> = DOCKER_DEFAULT.iter().map(|(name, _)| *name).collect();
        let numbers: HashSet<c_long> = DOCKER_DEFAULT.iter().map(|(_, nr)| *nr).collect();
        assert_eq!(names.len(), DOCKER_DEFAULT.len());
        assert_eq!(numbers.len(), DOCKER_DEFAULT.len());
        for name in [
            "read",
            "execve",
            "socketpair",
            "landlock_restrict_self",
            "truncate",
        ] {
            assert!(names.contains(name), "{name}");
        }
        for name in [
            "socket",
            "clone",
            "personality",
            "ptrace",
            "mount",
            "clone3",
        ] {
            assert!(!names.contains(name), "{name}");
        }
    }

    #[test]
    fn denied_syscalls_get_eperm() {
        let filters = filters(&spec(), 6).unwrap();
        for &(name, nr) in DENIED {
            assert_eq!(
                check(&filters, nr, NO_ARGS),
                Outcome::Errno(libc::EPERM),
                "{name}"
            );
        }
    }

    #[test]
    fn other_architectures_are_killed() {
        let filters = filters(&spec(), 6).unwrap();
        // AUDIT_ARCH_I386 and AUDIT_ARCH_ARM.
        for arch in [3 | 0x4000_0000, 40 | 0x4000_0000] {
            assert_eq!(
                outcome(&filters, arch, libc::SYS_read, NO_ARGS),
                Outcome::Kill
            );
        }
    }

    #[test]
    fn socket_families_follow_the_grants() {
        let off = filters(&spec(), 6).unwrap();
        let network = filters(
            &Spec {
                network: Network::Tcp(BTreeSet::from([443])),
                ..spec()
            },
            6,
        )
        .unwrap();
        let unix = filters(
            &Spec {
                unix: vec![PathBuf::from("/run/x.sock")],
                ..spec()
            },
            6,
        )
        .unwrap();
        let families = [
            libc::AF_INET,
            libc::AF_INET6,
            libc::AF_UNIX,
            libc::AF_NETLINK,
        ];
        let outcomes = |filters: &[BpfProgram; 2]| -> Vec<bool> {
            families
                .iter()
                .map(
                    |&family| match check(filters, libc::SYS_socket, socket(family)) {
                        Outcome::Allow => true,
                        Outcome::Errno(libc::EPERM) => false,
                        other => panic!("{other:?}"),
                    },
                )
                .collect()
        };
        assert_eq!(outcomes(&off), [false, false, false, false]);
        assert_eq!(outcomes(&network), [true, true, false, false]);
        assert_eq!(outcomes(&unix), [false, false, true, false]);
        // Only the low 32 bits of the family count, as in the kernel (it's an `int`).
        let mut high = socket(libc::AF_INET);
        high[0] |= 1 << 32;
        assert_eq!(check(&network, libc::SYS_socket, high), Outcome::Allow);
        let mut high = socket(libc::AF_UNIX);
        high[0] |= 1 << 32;
        assert_eq!(
            check(&network, libc::SYS_socket, high),
            Outcome::Errno(libc::EPERM)
        );
        // `socketpair` needs no grant.
        for filters in [&off, &network, &unix] {
            assert_eq!(
                check(filters, libc::SYS_socketpair, socket(libc::AF_UNIX)),
                Outcome::Allow
            );
        }
    }

    #[test]
    fn clone_may_not_create_namespaces() {
        let filters = filters(&spec(), 6).unwrap();
        let clone = |flags: u64| check(&filters, libc::SYS_clone, [flags, 0, 0, 0, 0, 0]);
        // What `fork` and `pthread_create` pass.
        assert_eq!(clone(libc::SIGCHLD as u64), Outcome::Allow);
        assert_eq!(clone(0x003d_0f00), Outcome::Allow);
        for &flag in NAMESPACE_FLAGS {
            assert_eq!(
                clone(flag | libc::SIGCHLD as u64),
                Outcome::Errno(libc::EPERM)
            );
        }
    }

    #[test]
    fn personality_is_limited_to_dockers_values() {
        let filters = filters(&spec(), 6).unwrap();
        let personality =
            |value: u64| check(&filters, libc::SYS_personality, [value, 0, 0, 0, 0, 0]);
        for &value in PERSONALITIES {
            assert_eq!(personality(value), Outcome::Allow, "{value:#x}");
        }
        // ADDR_NO_RANDOMIZE turns off ASLR.
        assert_eq!(personality(0x0040000), Outcome::Errno(libc::EPERM));
    }

    #[test]
    fn path_truncate_is_denied_below_landlock_abi_3() {
        for (abi, expected) in [
            (1, Outcome::Errno(libc::EPERM)),
            (2, Outcome::Errno(libc::EPERM)),
            (3, Outcome::Allow),
            (6, Outcome::Allow),
        ] {
            let filters = filters(&spec(), abi).unwrap();
            assert_eq!(
                check(&filters, libc::SYS_truncate, NO_ARGS),
                expected,
                "ABI {abi}"
            );
            assert_eq!(
                check(&filters, libc::SYS_ftruncate, NO_ARGS),
                Outcome::Allow
            );
        }
    }
}
