//! The generated Seatbelt profile (PLAN section 12.3). `Docs/technical/sandbox.md` explains each
//! rule and why it's there.
//!
//! Deny by default, then allow exactly what a plugin needs. Every rule carries at least one
//! filter: an `(allow <operation>)` without one would allow that operation everywhere.

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use super::paths::resolve;
use crate::sandbox::roots::writable_roots;
use crate::sandbox::{Network, SandboxError, Spec};

/// System directories, read-only: libraries and frameworks (PLAN section 12.4's "system
/// libraries"; dyld maps the shared cache without reading files, but `dlopen`, bundles, and their
/// resources read these), `/usr/share` (locales, terminfo, time zone names), TLS certificates for
/// OpenSSL-based runtimes (`/etc/ssl/cert.pem`), and the time zone database (without it local time
/// is UTC).
const SYSTEM_READ_SUBPATHS: &[&str] = &[
    "/usr/lib",
    "/usr/share",
    "/System/Library",
    "/private/etc/ssl",
    "/private/var/db/timezone",
];

/// Single system files, as real paths.
const SYSTEM_READ_LITERALS: &[&str] = &[
    // dyld opens `/` at startup and aborts the process without it. Lists only `/` itself.
    "/",
    // DNS configuration, for runtimes with their own resolver (`getaddrinfo` asks
    // mDNSResponder instead).
    "/private/etc/hosts",
    "/private/etc/resolv.conf",
    "/private/var/run/resolv.conf",
    // `getservbyname` and `getprotobyname`.
    "/private/etc/services",
    "/private/etc/protocols",
    // The local time zone (a link into `/var/db/timezone`).
    "/private/etc/localtime",
    "/dev/random",
    "/dev/urandom",
    // `/bin/sh` reads it to pick the shell, and prints an error without it.
    "/private/var/select/sh",
];

/// The system's links that system paths resolve through (`/etc/localtime`,
/// `/var/run/mDNSResponder`): time zones and name resolution fail without them. Links in the
/// grants' own paths (like `/tmp`) come from [`lookup_paths`].
const SYSTEM_LINKS: &[&str] = &["/etc", "/var"];

/// Hardware facts: Rust's `std` aborts without `hw.pagesize_compat`, Node without `hw.ncpu`,
/// and Python's `os.cpu_count()` returns `None`.
const SYSCTL_PREFIXES: &[&str] = &["hw."];

/// What `uname` reports: Node aborts without them.
const SYSCTL_NAMES: &[&str] = &[
    "kern.hostname",
    "kern.osrelease",
    "kern.ostype",
    "kern.version",
];

/// mDNSResponder's socket: `getaddrinfo` resolves names through it. Allowed with any network
/// grant; it's the system resolver, not a general Unix socket grant.
const DNS_SOCKET: &str = "/private/var/run/mDNSResponder";

/// A profile for one launch, with every path already resolved to its real path.
#[derive(Debug)]
pub(super) struct Profile {
    /// The command.
    program: PathBuf,
    /// `execute` grants.
    execute: BTreeSet<PathBuf>,
    /// `read` and `execute` grants.
    read: BTreeSet<PathBuf>,
    /// The data directory and `write` grants.
    write: BTreeSet<PathBuf>,
    /// `unix` grants.
    unix: BTreeSet<PathBuf>,
    /// What the process `lstat`s to reach all of the above.
    lookup: BTreeSet<PathBuf>,
    /// IP network access.
    network: Network,
}

impl Profile {
    /// Resolves the spec's paths and `program` (absolute) on this host.
    ///
    /// The writable roots (the data directory and `write` grants) are resolved first, by
    /// [`writable_roots`]; every other path is then resolved once, following no link at or
    /// below a root, where the command could have planted it on an earlier run (or plants it
    /// right now, from a child that outlived that run). The data directory itself is checked by
    /// the trampoline first.
    pub(super) fn new(spec: &Spec, program: &Path) -> Result<Self, SandboxError> {
        let roots = writable_roots(
            &std::iter::once(&spec.data_dir)
                .chain(&spec.write)
                .map(PathBuf::as_path)
                .collect::<Vec<_>>(),
            resolve,
        )?;
        let stops: Vec<PathBuf> = roots.iter().map(|root| root.real.clone()).collect();
        let mut lookup: BTreeSet<PathBuf> = SYSTEM_LINKS.iter().map(PathBuf::from).collect();
        for root in &roots {
            lookup.extend(root.visited.iter().cloned());
        }
        let mut real = |path: &Path| {
            let resolved = resolve(path, &stops);
            lookup.extend(resolved.visited);
            resolved.real
        };
        let program = real(program);
        let execute: BTreeSet<PathBuf> = spec.execute.iter().map(|p| real(p)).collect();
        // Running a program means reading it.
        let read = spec
            .read
            .iter()
            .map(|p| real(p))
            .chain(execute.iter().cloned())
            .collect();
        let unix = spec.unix.iter().map(|p| real(p)).collect();
        let write = stops.into_iter().collect();
        Ok(Self {
            program,
            execute,
            read,
            write,
            unix,
            lookup,
            network: spec.network.clone(),
        })
    }

    /// The profile text, for `sandbox-exec -p`. Bytes, because paths needn't be UTF-8.
    pub(super) fn render(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4096);
        out.extend_from_slice(b"(version 1)\n(deny default)\n");

        let program = || Filter::Literal(&self.program);
        rule(
            &mut out,
            "process-exec",
            std::iter::once(program()).chain(self.execute.iter().map(|p| Filter::Subpath(p))),
        );
        // Forking has nothing to filter on; children inherit the profile.
        out.extend_from_slice(b"(allow process-fork)\n");
        rule(&mut out, "signal", [Filter::Raw("(target same-sandbox)")]);

        rule(
            &mut out,
            "sysctl-read",
            SYSCTL_PREFIXES
                .iter()
                .map(|p| Filter::SysctlPrefix(p))
                .chain(SYSCTL_NAMES.iter().map(|n| Filter::SysctlName(n))),
        );

        rule(
            &mut out,
            "file-read*",
            SYSTEM_READ_LITERALS
                .iter()
                .map(|p| Filter::Literal(Path::new(p)))
                .chain(
                    SYSTEM_READ_SUBPATHS
                        .iter()
                        .map(|p| Filter::Subpath(Path::new(p))),
                ),
        );
        rule(
            &mut out,
            "file-read* file-write-data",
            [Filter::Literal(Path::new("/dev/null"))],
        );
        rule(
            &mut out,
            "file-read-metadata",
            self.lookup.iter().map(|p| Filter::Literal(p)),
        );

        rule(
            &mut out,
            "file-read*",
            std::iter::once(program()).chain(self.read.iter().map(|p| Filter::Subpath(p))),
        );
        rule(
            &mut out,
            "file-read* file-write*",
            self.write.iter().map(|p| Filter::Subpath(p)),
        );
        // The roots themselves can't be removed, renamed, or replaced (say, by a link that a
        // later launch would follow); later rules win. Their contents stay writable.
        if !self.write.is_empty() {
            out.extend_from_slice(b"(deny file-write-unlink file-write-create");
            for root in &self.write {
                out.extend_from_slice(b"\n  ");
                Filter::Literal(root).render(&mut out);
            }
            out.extend_from_slice(b")\n");
        }

        let ip: Vec<Filter<'_>> = match &self.network {
            Network::Off => Vec::new(),
            Network::All => vec![Filter::Raw("(remote ip \"*:*\")")],
            Network::Tcp(ports) => ports.iter().map(|&port| Filter::RemoteTcp(port)).collect(),
        };
        if !ip.is_empty() {
            rule(
                &mut out,
                "network-outbound",
                ip.into_iter()
                    .chain([Filter::UnixSocket(Path::new(DNS_SOCKET))]),
            );
        }
        rule(
            &mut out,
            "network-outbound",
            self.unix.iter().map(|p| Filter::UnixSocket(p)),
        );
        out
    }
}

/// One SBPL filter.
#[derive(Debug, Clone, Copy)]
enum Filter<'a> {
    /// Exactly this path.
    Literal(&'a Path),
    /// This path and everything under it.
    Subpath(&'a Path),
    /// One sysctl.
    SysctlName(&'a str),
    /// Sysctls starting with this.
    SysctlPrefix(&'a str),
    /// TCP to this port on any host.
    RemoteTcp(u16),
    /// Connecting to the Unix socket at this path.
    UnixSocket(&'a Path),
    /// A fixed filter.
    Raw(&'static str),
}

impl Filter<'_> {
    fn render(self, out: &mut Vec<u8>) {
        match self {
            Self::Literal(path) => tagged(out, "literal", path.as_os_str().as_bytes()),
            Self::Subpath(path) => tagged(out, "subpath", path.as_os_str().as_bytes()),
            Self::SysctlName(name) => tagged(out, "sysctl-name", name.as_bytes()),
            Self::SysctlPrefix(name) => tagged(out, "sysctl-name-prefix", name.as_bytes()),
            Self::RemoteTcp(port) => {
                tagged(out, "remote tcp", format!("*:{port}").as_bytes());
            }
            Self::UnixSocket(path) => {
                out.extend_from_slice(b"(remote unix-socket ");
                tagged(out, "path-literal", path.as_os_str().as_bytes());
                out.push(b')');
            }
            Self::Raw(filter) => out.extend_from_slice(filter.as_bytes()),
        }
    }
}

/// Appends `(allow <operations> <filters>...)`, or nothing when there are no filters: a rule
/// without a filter would allow the operations everywhere.
fn rule<'a>(out: &mut Vec<u8>, operations: &str, filters: impl IntoIterator<Item = Filter<'a>>) {
    let mut filters = filters.into_iter().peekable();
    if filters.peek().is_none() {
        return;
    }
    out.extend_from_slice(b"(allow ");
    out.extend_from_slice(operations.as_bytes());
    for filter in filters {
        out.extend_from_slice(b"\n  ");
        filter.render(out);
    }
    out.extend_from_slice(b")\n");
}

/// Appends `(<tag> "<value>")` with `value` as an SBPL string literal.
fn tagged(out: &mut Vec<u8>, tag: &str, value: &[u8]) {
    out.push(b'(');
    out.extend_from_slice(tag.as_bytes());
    out.push(b' ');
    string_literal(out, value);
    out.push(b')');
}

/// Appends `value` as an SBPL (Scheme) string literal: in quotes, with `\` and `"` escaped.
/// Every other byte, including newlines and bytes that aren't UTF-8, stands for itself.
fn string_literal(out: &mut Vec<u8>, value: &[u8]) {
    out.push(b'"');
    for &byte in value {
        if matches!(byte, b'\\' | b'"') {
            out.push(b'\\');
        }
        out.push(byte);
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn render(spec: &Spec) -> String {
        String::from_utf8(Profile::new(spec, Path::new("/bin/sh")).unwrap().render()).unwrap()
    }

    /// Each rule as one line, so tests can look at whole rules.
    fn rules(profile: &str) -> Vec<String> {
        profile
            .split("\n(")
            .map(|rule| rule.trim_end().replace("\n  ", " "))
            .collect()
    }

    #[test]
    fn default_profile_denies_by_default() {
        let spec = Spec::new("/tmp/giles-test/data");
        let profile = render(&spec);
        assert!(
            profile.starts_with("(version 1)\n(deny default)\n"),
            "{profile}"
        );
        let rules = rules(&profile);
        // Every rule but forking is filtered; nothing allows everything.
        for rule in &rules[2..] {
            assert!(
                (rule.starts_with("allow ") || rule.starts_with("deny "))
                    && (rule.contains(" (") || rule == "allow process-fork)"),
                "unfiltered rule: {rule}"
            );
        }
        assert!(!profile.contains("(allow default"), "{profile}");
        assert!(!profile.contains("mach-"), "{profile}");
        assert!(!profile.contains("network-"), "{profile}");
        assert!(!profile.contains("iokit"), "{profile}");
        assert!(!profile.contains(DNS_SOCKET), "{profile}");
        // The data directory, as a real path, is the only writable place.
        let writable: Vec<_> = rules.iter().filter(|r| r.contains("file-write*")).collect();
        assert_eq!(
            writable,
            ["allow file-read* file-write* (subpath \"/private/tmp/giles-test/data\"))"]
        );
        // ...and the directory itself can't be removed or replaced, by a rule after that one.
        let deny = "deny file-write-unlink file-write-create \
                    (literal \"/private/tmp/giles-test/data\"))";
        let allow_at = rules.iter().position(|r| r == writable[0]).unwrap();
        assert_eq!(
            rules.iter().position(|r| r == deny),
            Some(allow_at + 1),
            "{profile}"
        );
        // The command is the only program.
        assert!(
            rules.contains(&"allow process-exec (literal \"/bin/sh\"))".to_owned()),
            "{profile}"
        );
        // Its parents can be looked up (for getcwd), through the `/tmp` link.
        let metadata = rules
            .iter()
            .find(|r| r.starts_with("allow file-read-metadata"))
            .unwrap();
        // (`/tmp/giles-test` doesn't exist, so there's nothing more to look up.)
        for path in ["/tmp", "/private", "/private/tmp"] {
            assert!(
                metadata.contains(&format!("(literal \"{path}\")")),
                "{metadata}"
            );
        }
    }

    #[test]
    fn grants_become_real_path_rules() {
        let spec = Spec {
            read: vec!["/tmp/giles-test/read".into()],
            write: vec!["/tmp/giles-test/write".into()],
            execute: vec!["/usr/bin".into()],
            ..Spec::new("/tmp/giles-test/data")
        };
        let rules = rules(&render(&spec));
        assert!(rules.contains(
            &"allow process-exec (literal \"/bin/sh\") (subpath \"/usr/bin\"))".to_owned()
        ));
        assert!(
            rules.contains(
                &"allow file-read* (literal \"/bin/sh\") \
              (subpath \"/private/tmp/giles-test/read\") (subpath \"/usr/bin\"))"
                    .to_owned()
            )
        );
        assert!(
            rules.contains(
                &"allow file-read* file-write* (subpath \"/private/tmp/giles-test/data\") \
              (subpath \"/private/tmp/giles-test/write\"))"
                    .to_owned()
            )
        );
    }

    #[test]
    fn network_grants_allow_ip_and_the_resolver_only() {
        let network = |network: Network| {
            render(&Spec {
                network,
                ..Spec::new("/d")
            })
        };
        let all = network(Network::All);
        assert!(
            all.contains(&format!(
                "(allow network-outbound\n  (remote ip \"*:*\")\n  \
                 (remote unix-socket (path-literal \"{DNS_SOCKET}\")))\n"
            )),
            "{all}"
        );
        let ports = network(Network::Tcp(BTreeSet::from([443, 6697])));
        assert!(
            ports.contains(&format!(
                "(allow network-outbound\n  (remote tcp \"*:443\")\n  (remote tcp \"*:6697\")\n  \
                 (remote unix-socket (path-literal \"{DNS_SOCKET}\")))\n"
            )),
            "{ports}"
        );
        assert!(!ports.contains("remote ip"), "{ports}");
        // No grant allows listening.
        for profile in [all, ports] {
            assert!(!profile.contains("network-inbound"), "{profile}");
            assert!(!profile.contains("network-bind"), "{profile}");
        }
    }

    #[test]
    fn unix_grants_allow_connecting_to_those_sockets_only() {
        let spec = Spec {
            unix: vec!["/tmp/a.sock".into(), "/var/run/b.sock".into()],
            ..Spec::new("/d")
        };
        let profile = render(&spec);
        assert!(
            profile.contains(
                "(allow network-outbound\n  \
                 (remote unix-socket (path-literal \"/private/tmp/a.sock\"))\n  \
                 (remote unix-socket (path-literal \"/private/var/run/b.sock\")))\n"
            ),
            "{profile}"
        );
        // A Unix grant is not a network grant.
        assert!(!profile.contains("remote ip"), "{profile}");
        assert!(!profile.contains(DNS_SOCKET), "{profile}");
    }

    #[test]
    fn string_literals_escape_quotes_and_backslashes_only() {
        let mut out = Vec::new();
        string_literal(&mut out, b"a\"b\\c\nd\xff");
        assert_eq!(out, b"\"a\\\"b\\\\c\nd\xff\"");

        // A path can't close the string and add rules.
        let hostile = r#"/d/x")(allow default)(literal ""#;
        let profile = render(&Spec::new(hostile));
        assert!(
            profile.contains(r#"(subpath "/d/x\")(allow default)(literal \"")"#),
            "{profile}"
        );
    }

    /// A link inside a writable root may have been planted by the command, so grants there
    /// resolve without following it: to the link's own place, which grants nothing beyond it.
    #[test]
    fn links_in_writable_roots_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = std::fs::canonicalize(dir.path()).unwrap();
        let data = dir.path().join("data");
        let notes = dir.path().join("notes");
        let outside = dir.path().join("outside");
        for d in [&data, &notes, &outside] {
            std::fs::create_dir(d).unwrap();
        }
        std::fs::create_dir(outside.join("deep")).unwrap();
        std::os::unix::fs::symlink(&outside, data.join("agent.sock")).unwrap();
        std::os::unix::fs::symlink(&outside, notes.join("docs")).unwrap();
        std::os::unix::fs::symlink(&outside, data.join("nested")).unwrap();
        let profile = |spec: Spec| Profile::new(&spec, Path::new("/bin/sh")).unwrap();

        let p = profile(Spec {
            unix: vec![data.join("agent.sock")],
            ..Spec::new(&data)
        });
        assert_eq!(p.unix, BTreeSet::from([real_dir.join("data/agent.sock")]));

        let p = profile(Spec {
            write: vec![notes.clone()],
            read: vec![notes.join("docs/file")],
            ..Spec::new(&data)
        });
        assert!(p.read.contains(&real_dir.join("notes/docs/file")), "{p:?}");

        // A `write` grant nested in the data directory doesn't follow the data directory's
        // links either, though it's resolved before anything is known to contain it.
        let p = profile(Spec {
            write: vec![data.join("nested/deep")],
            ..Spec::new(&data)
        });
        assert_eq!(
            p.write,
            BTreeSet::from([real_dir.join("data"), real_dir.join("data/nested/deep")])
        );

        // The same link is followed when nothing makes its directory writable.
        let p = profile(Spec {
            read: vec![notes.join("docs/file")],
            ..Spec::new(&data)
        });
        assert!(p.read.contains(&real_dir.join("outside/file")), "{p:?}");
    }
}
