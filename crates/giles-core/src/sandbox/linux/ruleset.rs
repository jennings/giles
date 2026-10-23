//! The Landlock ruleset (PLAN section 12.2). `Docs/technical/sandbox.md` explains each default.
//!
//! Deny by default: the ruleset handles every filesystem right of [`ABI`], TCP binding (and
//! connecting, unless the network is fully open), and scoping of signals and abstract Unix
//! sockets, then allows exactly the grants. The `landlock` crate applies it best effort: rights
//! the kernel's ABI doesn't have are dropped, which is why the backend logs what the effective ABI
//! lacks and enforces `min_abi` before getting here.

use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, NetPort, PathBeneath, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, RulesetError, RulesetStatus, Scope,
};
use tracing::{debug, warn};

use super::paths::{Resolved, Target, resolve};
use crate::sandbox::roots::writable_roots;
use crate::sandbox::{Network, SandboxError, Spec};

/// The newest Landlock ABI this backend asks for. The kernel provides what it can of it.
pub(super) const ABI: ABI = ABI::V9;

/// System files and directories every launch may read: shared libraries and their cache
/// (PLAN section 12.4's "system libraries"; `ld.so` falls back to slow directory scans without
/// the cache), read-only system data in `/usr/share` (time zones, locales, CA certificates on
/// Debian, which `/etc/ssl/certs` links into), TLS certificates and OpenSSL configuration, DNS
/// configuration (`getaddrinfo` reads `/etc/hosts`, `/etc/resolv.conf`, and `nsswitch.conf`),
/// the protocol and service databases, the local time zone, randomness, and the CPU list
/// (`sysconf(_SC_NPROCESSORS_ONLN)`). Missing entries are skipped.
const SYSTEM_READ: &[&str] = &[
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/lib64",
    "/usr/share",
    "/etc/ld.so.cache",
    "/etc/ld.so.preload",
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/etc/crypto-policies",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/nsswitch.conf",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/services",
    "/etc/protocols",
    "/etc/localtime",
    "/dev/random",
    "/dev/urandom",
    "/dev/zero",
    "/sys/devices/system/cpu",
];

/// The dynamic loaders (the `PT_INTERP` paths the ABIs fix), which must be executable: the
/// kernel opens a dynamically linked program's loader for execution. glibc's, then musl's.
#[cfg(target_arch = "x86_64")]
const LOADERS: &[&str] = &["/lib64/ld-linux-x86-64.so.2", "/lib/ld-musl-x86_64.so.1"];
#[cfg(target_arch = "aarch64")]
const LOADERS: &[&str] = &["/lib/ld-linux-aarch64.so.1", "/lib/ld-musl-aarch64.so.1"];

/// Read and written by every launch: redirects to and from `/dev/null`.
const SYSTEM_WRITE: &[&str] = &["/dev/null"];

/// What a rule allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Grant {
    /// Reading files and listing directories.
    Read,
    /// Reading, and running programs.
    Execute,
    /// Reading files and writing to existing ones (devices).
    ReadWrite,
    /// Everything but running programs and creating device nodes (the writable roots): a plugin
    /// with `CAP_MKNOD` could otherwise make a node for a disk and bypass every file rule.
    Write,
    /// Connecting to a Unix socket at this path (Landlock ABI 9).
    UnixSocket,
}

impl Grant {
    /// The rights, for a directory or (with `dir` false) for anything else, which can only carry
    /// file rights.
    pub(super) fn access(self, dir: bool) -> BitFlags<AccessFs> {
        let read = AccessFs::ReadFile | AccessFs::ReadDir;
        let access = match self {
            Self::Read => read,
            Self::Execute => read | AccessFs::Execute,
            Self::ReadWrite => read | AccessFs::WriteFile | AccessFs::Truncate | AccessFs::IoctlDev,
            Self::Write => {
                AccessFs::from_all(ABI)
                    & !(AccessFs::Execute | AccessFs::MakeChar | AccessFs::MakeBlock)
            }
            Self::UnixSocket => AccessFs::ResolveUnix.into(),
        };
        if dir {
            access
        } else {
            access & AccessFs::from_file(ABI)
        }
    }
}

/// Whether a missing path is worth a warning: grants from the spec are, system defaults (which
/// vary between distributions) aren't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Spec(&'static str),
    System,
}

/// Every path rule for `spec` and `program` except the writable roots: the system defaults,
/// the command, and the spec's `read`, `execute`, and `unix` grants.
fn path_grants<'a>(spec: &'a Spec, program: &'a Path) -> Vec<(&'a Path, Grant, Origin)> {
    let system = |paths: &'static [&'static str], grant| {
        paths
            .iter()
            .map(move |path| (Path::new(*path), grant, Origin::System))
    };
    let from_spec = |field, paths: &'a [PathBuf], grant| {
        paths
            .iter()
            .map(move |path| (path.as_path(), grant, Origin::Spec(field)))
    };
    system(SYSTEM_READ, Grant::Read)
        .chain(system(LOADERS, Grant::Execute))
        .chain(system(SYSTEM_WRITE, Grant::ReadWrite))
        .chain([(program, Grant::Execute, Origin::Spec("command"))])
        .chain(from_spec("read", &spec.read, Grant::Read))
        .chain(from_spec("execute", &spec.execute, Grant::Execute))
        .chain(from_spec("unix", &spec.unix, Grant::UnixSocket))
        .collect()
}

/// Builds the ruleset for `spec` and `program` (absolute) and restricts the calling thread.
///
/// The writable roots (the data directory and `write` grants) are opened first, by
/// [`writable_roots`]; every other path is then opened once, following no link at or below a
/// root, where the command could have planted it on an earlier run (or plants it right now, from
/// a child that outlived that run). Each rule comes from the descriptor that walk opened.
pub(super) fn restrict(spec: &Spec, program: &Path) -> Result<(), SandboxError> {
    let roots = writable_roots(
        &std::iter::once(&spec.data_dir)
            .chain(&spec.write)
            .map(PathBuf::as_path)
            .collect::<Vec<_>>(),
        resolve,
    )?;
    let stops: Vec<PathBuf> = roots.iter().map(|root| root.real.clone()).collect();

    let mut ruleset = handled(&spec.network)
        .and_then(Ruleset::create)
        .map_err(landlock)?;
    for (path, root) in std::iter::once(&spec.data_dir)
        .chain(&spec.write)
        .zip(roots)
    {
        ruleset = add_path(ruleset, path, root, Grant::Write, Origin::Spec("write"))?;
    }
    for (path, grant, origin) in path_grants(spec, program) {
        ruleset = add_path(ruleset, path, resolve(path, &stops), grant, origin)?;
    }
    if let Network::Tcp(ports) = &spec.network {
        for &port in ports {
            ruleset = ruleset
                .add_rule(NetPort::new(port, AccessNet::ConnectTcp))
                .map_err(landlock)?;
        }
    }
    let status = ruleset.restrict_self().map_err(landlock)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(landlock("the kernel enforced none of the ruleset"));
    }
    debug!(?status, "Landlock ruleset applied");
    Ok(())
}

/// A ruleset handling every filesystem right, scoping, and the network rights `network` needs
/// restricted: everything but connecting with `network = true` (binding is never granted);
/// both with a port list (connecting to those ports is then allowed by rule) or with no network
/// (seccomp already denies IP sockets; this is a second layer).
fn handled(network: &Network) -> Result<Ruleset, RulesetError> {
    let net = match network {
        Network::All => AccessNet::BindTcp.into(),
        Network::Off | Network::Tcp(_) => AccessNet::from_all(ABI),
    };
    Ruleset::default()
        .handle_access(AccessFs::from_all(ABI))?
        .handle_access(net)?
        .scope(Scope::from_all(ABI))
}

/// Adds the rule for `path` (as the spec spells it), opened as `resolved`, or logs why there is
/// none.
fn add_path(
    ruleset: RulesetCreated,
    path: &Path,
    resolved: Resolved,
    grant: Grant,
    origin: Origin,
) -> Result<RulesetCreated, SandboxError> {
    match resolved.target {
        Target::Found { fd, dir } => ruleset
            .add_rule(PathBeneath::new(fd, grant.access(dir)))
            .map_err(landlock),
        Target::LinkInRoot(link) => {
            warn!(
                path = %path.display(),
                link = %link.display(),
                "not granting a sandbox path that goes through a symbolic link inside a writable \
                 grant (the plugin may have planted it)",
            );
            Ok(ruleset)
        }
        Target::Unreachable(err) => {
            match origin {
                Origin::Spec(field) => warn!(
                    field,
                    path = %path.display(),
                    error = %err,
                    "not granting a sandbox path that can't be opened; Landlock grants existing \
                     files only",
                ),
                Origin::System => {
                    debug!(path = %path.display(), error = %err, "no such system path")
                }
            }
            Ok(ruleset)
        }
    }
}

fn landlock(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> SandboxError {
    SandboxError::Landlock(err.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_map_to_rights() {
        let read = AccessFs::ReadFile | AccessFs::ReadDir;
        assert_eq!(Grant::Read.access(true), read);
        assert_eq!(Grant::Read.access(false), AccessFs::ReadFile);
        assert_eq!(
            Grant::Execute.access(false),
            AccessFs::ReadFile | AccessFs::Execute
        );
        let write = Grant::Write.access(true);
        assert!(
            !write.contains(AccessFs::Execute),
            "writable roots aren't executable"
        );
        for right in [AccessFs::MakeChar, AccessFs::MakeBlock] {
            assert!(
                !write.contains(right),
                "writable roots can't hold new devices: {right:?}"
            );
        }
        for right in [
            AccessFs::WriteFile,
            AccessFs::MakeReg,
            AccessFs::MakeDir,
            AccessFs::RemoveFile,
            AccessFs::Refer,
            AccessFs::Truncate,
            AccessFs::ResolveUnix,
        ] {
            assert!(write.contains(right), "{right:?}");
        }
        assert!(!Grant::Write.access(false).contains(AccessFs::MakeReg));
        assert_eq!(Grant::UnixSocket.access(false), AccessFs::ResolveUnix);
        // Files only ever get file rights (the kernel rejects anything else).
        for grant in [
            Grant::Read,
            Grant::Execute,
            Grant::ReadWrite,
            Grant::Write,
            Grant::UnixSocket,
        ] {
            let file = grant.access(false);
            assert_eq!(file & AccessFs::from_file(ABI), file, "{grant:?}");
        }
    }

    #[test]
    fn path_grants_cover_the_command_and_the_spec() {
        let spec = Spec {
            read: vec!["/opt/r".into()],
            execute: vec!["/opt/x".into()],
            unix: vec!["/run/s.sock".into()],
            write: vec!["/w".into()],
            ..Spec::new("/data")
        };
        let grants = path_grants(&spec, Path::new("/opt/plugin"));
        let find = |path: &str| {
            grants
                .iter()
                .find(|(p, ..)| *p == Path::new(path))
                .map(|&(_, grant, origin)| (grant, origin))
        };
        assert_eq!(
            find("/opt/plugin"),
            Some((Grant::Execute, Origin::Spec("command")))
        );
        assert_eq!(find("/opt/r"), Some((Grant::Read, Origin::Spec("read"))));
        assert_eq!(
            find("/opt/x"),
            Some((Grant::Execute, Origin::Spec("execute")))
        );
        assert_eq!(
            find("/run/s.sock"),
            Some((Grant::UnixSocket, Origin::Spec("unix")))
        );
        assert_eq!(
            find("/etc/resolv.conf"),
            Some((Grant::Read, Origin::System))
        );
        assert_eq!(find("/dev/null"), Some((Grant::ReadWrite, Origin::System)));
        assert_eq!(find(LOADERS[0]), Some((Grant::Execute, Origin::System)));
        // The writable roots are added separately, before everything else.
        assert_eq!(find("/w"), None);
        assert_eq!(find("/data"), None);
    }
}
