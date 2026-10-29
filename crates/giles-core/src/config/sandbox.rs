//! Sandbox grants: `sandbox = { network, read, write, unix, min_abi }` (PLAN sections 11.1 and
//! 12.4).
//!
//! No plugin may be granted any part of the data root except its own data directory, or the
//! directory holding the admin and proxy sockets. Otherwise a file-writing tool could edit
//! `giles.toml` (e.g. set `unsandboxed = true`) or read `secrets.toml`. Nor may it write to (or
//! connect to sockets in) a directory holding a binary Giles runs ([`BinaryDirs`]).

use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde::Serialize;
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::ser::Serializer;

use super::error::{ConfigError, KeyPath};
use super::names::{InstanceName, Kind};
use crate::paths::DataRoot;

/// What a sandboxed plugin may reach beyond the defaults (PLAN section 12.4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Grants {
    /// IP network access.
    pub network: NetworkGrant,
    /// Paths the plugin may read (absolute, lexically normal).
    pub read: Vec<PathBuf>,
    /// Paths the plugin may read and write.
    pub write: Vec<PathBuf>,
    /// Unix sockets the plugin may connect to. Any entry also lets it create `AF_UNIX` sockets.
    pub unix: Vec<PathBuf>,
    /// The lowest Landlock ABI the plugin may run under (Linux only).
    pub min_abi: Option<u32>,
}

/// IP network access for a sandboxed plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum NetworkGrant {
    /// No IP sockets.
    #[default]
    None,
    /// All IP traffic.
    All,
    /// IP sockets, with TCP restricted to these ports (sorted, unique) where the kernel can
    /// enforce it (Landlock ABI 4).
    TcpPorts(Vec<u16>),
}

impl NetworkGrant {
    /// The grant when `sandbox.network` is unset: on for channels (and providers), off for stdio
    /// toolsets.
    pub(crate) fn default_for_toolset(toolset: bool) -> Self {
        if toolset { Self::None } else { Self::All }
    }
}

impl<'de> Deserialize<'de> for NetworkGrant {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NetworkVisitor;

        impl<'de> Visitor<'de> for NetworkVisitor {
            type Value = NetworkGrant;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("true, false, or a list of \"tcp:<port>\"")
            }

            fn visit_bool<E: de::Error>(self, all: bool) -> Result<NetworkGrant, E> {
                Ok(if all {
                    NetworkGrant::All
                } else {
                    NetworkGrant::None
                })
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<NetworkGrant, A::Error> {
                let mut ports = Vec::new();
                while let Some(entry) = seq.next_element::<String>()? {
                    ports.push(parse_port(&entry).map_err(de::Error::custom)?);
                }
                ports.sort_unstable();
                ports.dedup();
                Ok(if ports.is_empty() {
                    NetworkGrant::None
                } else {
                    NetworkGrant::TcpPorts(ports)
                })
            }
        }

        deserializer.deserialize_any(NetworkVisitor)
    }
}

impl Serialize for NetworkGrant {
    /// The form [`Deserialize`] reads: `false`, `true`, or a list of `"tcp:<port>"`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::None => serializer.serialize_bool(false),
            Self::All => serializer.serialize_bool(true),
            Self::TcpPorts(ports) => {
                serializer.collect_seq(ports.iter().map(|port| format!("tcp:{port}")))
            }
        }
    }
}

/// Parses `"tcp:<port>"`, port 1 to 65535.
fn parse_port(entry: &str) -> Result<u16, String> {
    entry
        .strip_prefix("tcp:")
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            format!("invalid network grant {entry:?}: write \"tcp:<port>\", e.g. \"tcp:443\"")
        })
}

/// A `sandbox` table as written, before its paths are checked: the raw form of
/// [`Sandbox`](super::Sandbox).
///
/// Strict, unlike the rest of `giles.toml`: a misspelled `network` or `min_abi` would silently
/// leave the default (network on, no ABI floor) and widen the plugin's sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxSpec {
    /// `network`: `true`, `false`, or a list of `"tcp:<port>"`. Unset means the kind's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkGrant>,
    /// `read`: paths the instance may read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<String>,
    /// `write`: paths the instance may read and write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<String>,
    /// `unix`: Unix sockets the instance may connect to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unix: Vec<String>,
    /// `min_abi`: the lowest Landlock ABI the instance may run under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_abi: Option<u32>,
}

/// The directories no grant may overlap, and the one exception.
pub(crate) struct Protected {
    /// The data root and, if the sockets aren't under it, the socket directory, each with its
    /// canonical form when that differs.
    pub dirs: Vec<(PathBuf, &'static str)>,
    /// The instance's own data directory, each spelling.
    pub own: Vec<PathBuf>,
}

impl Protected {
    /// What's protected for an instance whose data directory is `own`, under data root `root`
    /// with sockets in `socket_dir`.
    pub(crate) fn new(root: &Path, socket_dir: &Path, own: &Path) -> Self {
        let mut dirs = Vec::new();
        for (dir, what) in [
            (root, "the data root"),
            (socket_dir, "the socket directory"),
        ] {
            for form in forms(dir) {
                if !dirs.iter().any(|(d, _)| *d == form) {
                    dirs.push((form, what));
                }
            }
        }
        Self {
            dirs,
            own: forms(own),
        }
    }

    /// Checks one grant: every spelling of it (as written and, if it exists, with symlinks
    /// resolved) must be inside the instance's own data directory or overlap none of the
    /// protected directories, in either direction.
    pub(crate) fn check(&self, grant: &Path) -> Result<(), String> {
        for form in forms(grant) {
            if self.own.iter().any(|own| form.starts_with(own)) {
                continue;
            }
            if let Some((dir, what)) = self
                .dirs
                .iter()
                .find(|(dir, _)| form.starts_with(dir) || dir.starts_with(&form))
            {
                return Err(format!(
                    "{} overlaps {what}, {}; a plugin may only be granted its own data \
                     directory there, {}",
                    form.display(),
                    dir.display(),
                    self.own[0].display(),
                ));
            }
        }
        Ok(())
    }
}

/// `path` as written, plus, if it differs, the path with symlinks resolved: the longest existing
/// ancestor made canonical, followed by the components that don't exist yet.
fn forms(path: &Path) -> Vec<PathBuf> {
    let mut forms = vec![path.to_path_buf()];
    let real = path.ancestors().find_map(|ancestor| {
        let real = fs::canonicalize(ancestor).ok()?;
        let rest = path.strip_prefix(ancestor).ok()?;
        Some(real.join(rest))
    });
    if let Some(real) = real
        && real != path
    {
        forms.push(real);
    }
    forms
}

/// Whether `a` and `b` overlap (one contains the other) in any spelling.
pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
    let b = forms(b);
    forms(a)
        .iter()
        .any(|a| b.iter().any(|b| a.starts_with(b) || b.starts_with(a)))
}

/// A granted path: absolute, without `..`, made lexically normal (no `.`, repeated or trailing
/// separators).
fn grant_path(text: &str) -> Result<PathBuf, String> {
    let path = Path::new(text);
    if !path.is_absolute() {
        return Err(format!("{text:?} is not an absolute path"));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(format!("{text:?} must not contain `..`"));
    }
    Ok(path.components().collect())
}

/// Checks a path the core is about to grant the `kind` instance `instance` (e.g. a stdio server's install prefix,
/// PLAN section 7.10) by the same rule as configured grants: it must not overlap `root`'s data
/// root or socket directory, except inside the instance's own data directory. Symlinks are
/// resolved for paths that exist.
///
/// # Errors
///
/// The reason, naming the directory it overlaps.
pub fn check_grant(
    root: &DataRoot,
    kind: Kind,
    instance: &InstanceName,
    grant: &Path,
) -> Result<(), String> {
    let own = root
        .plugin_data_dir(kind, instance.as_str())
        .map_err(|e| e.to_string())?;
    Protected::new(root.root(), root.socket_dir(), &own).check(grant)
}

/// Validates a `sandbox` table at `path` in `file`. `toolset` picks the network default.
pub(crate) fn grants(
    file: &Path,
    path: &KeyPath,
    raw: Option<SandboxSpec>,
    toolset: bool,
    protected: &Protected,
) -> Result<Grants, ConfigError> {
    let Some(raw) = raw else {
        return Ok(Grants {
            network: NetworkGrant::default_for_toolset(toolset),
            ..Grants::default()
        });
    };
    let paths = |key: &str, texts: Vec<String>| {
        texts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let at = path.key(key).index(i);
                let granted = grant_path(text).map_err(|r| ConfigError::at(file, at.clone(), r))?;
                protected
                    .check(&granted)
                    .map_err(|r| ConfigError::at(file, at, r))?;
                Ok(granted)
            })
            .collect::<Result<Vec<_>, ConfigError>>()
    };
    if raw.min_abi == Some(0) {
        return Err(ConfigError::at(
            file,
            path.key("min_abi"),
            "must be at least 1",
        ));
    }
    Ok(Grants {
        network: raw
            .network
            .unwrap_or_else(|| NetworkGrant::default_for_toolset(toolset)),
        read: paths("read", raw.read)?,
        write: paths("write", raw.write)?,
        unix: paths("unix", raw.unix)?,
        min_abi: raw.min_abi,
    })
}

/// The directories holding binaries Giles runs: the `giles` binary itself (which is also the
/// sandbox trampoline) and every launched plugin and stdio server, each as found now. A plugin
/// may read them (the core itself adds an install prefix to read grants, PLAN section 7.10), but
/// a `write` or `unix` grant overlapping one is rejected: a plugin that replaced a binary there
/// would run its code at the next launch, possibly unsandboxed or with another instance's grants.
#[derive(Debug, Default)]
pub(crate) struct BinaryDirs {
    dirs: Vec<(PathBuf, String)>,
}

impl BinaryDirs {
    /// Adds the directory of `binary`, described as `what`, in each spelling (as found and with
    /// symlinks resolved, so a symlinked binary protects its real directory too).
    pub(crate) fn add(&mut self, binary: &Path, what: String) {
        for form in forms(binary) {
            if let Some(dir) = form.parent()
                && !self.dirs.iter().any(|(d, _)| d == dir)
            {
                self.dirs.push((dir.to_path_buf(), what.clone()));
            }
        }
    }

    /// Adds directory `dir` itself, described as `what`, in each spelling: a toolset's skill
    /// directory, which the core reads for the model (PLAN section 6.6), so no plugin may write
    /// there and race those reads.
    pub(crate) fn add_dir(&mut self, dir: &Path, what: String) {
        for form in forms(dir) {
            if !self.dirs.iter().any(|(d, _)| *d == form) {
                self.dirs.push((form, what.clone()));
            }
        }
    }

    /// Checks the `write` and `unix` grants of the `sandbox` table at `path` in `file`.
    pub(crate) fn check(
        &self,
        file: &Path,
        path: &KeyPath,
        grants: &Grants,
    ) -> Result<(), ConfigError> {
        for (key, paths) in [("write", &grants.write), ("unix", &grants.unix)] {
            for (i, grant) in paths.iter().enumerate() {
                let overlap = forms(grant).into_iter().find_map(|form| {
                    self.dirs
                        .iter()
                        .find(|(dir, _)| form.starts_with(dir) || dir.starts_with(&form))
                        .map(|(dir, what)| (form, dir, what))
                });
                if let Some((form, dir, what)) = overlap {
                    return Err(ConfigError::at(
                        file,
                        path.key(key).index(i),
                        format!(
                            "{} overlaps {}, the directory of {what}; plugins may read there \
                             but not write or use sockets",
                            form.display(),
                            dir.display(),
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(parse_port("tcp:443"), Ok(443));
        assert_eq!(parse_port("tcp:65535"), Ok(65535));
        for bad in [
            "443",
            "tcp:",
            "tcp:0",
            "tcp:65536",
            "udp:53",
            "tcp:-1",
            "TCP:443",
            "tcp: 443",
        ] {
            let err = parse_port(bad).unwrap_err();
            assert!(err.contains("tcp:<port>"), "{bad}: {err}");
        }
    }

    #[test]
    fn network_grants_deserialize() {
        #[derive(Deserialize)]
        struct Doc {
            network: NetworkGrant,
        }
        let parse = |src: &str| toml::from_str::<Doc>(src).map(|d| d.network);
        assert_eq!(parse("network = true").unwrap(), NetworkGrant::All);
        assert_eq!(parse("network = false").unwrap(), NetworkGrant::None);
        assert_eq!(parse("network = []").unwrap(), NetworkGrant::None);
        assert_eq!(
            parse("network = [\"tcp:6697\", \"tcp:443\", \"tcp:443\"]").unwrap(),
            NetworkGrant::TcpPorts(vec![443, 6697])
        );
        let err = parse("network = \"all\"").unwrap_err();
        assert!(err.message().contains("true, false, or a list"), "{err}");
        let err = parse("network = [\"udp:53\"]").unwrap_err();
        assert!(err.message().contains("tcp:<port>"), "{err}");
    }

    #[test]
    fn grant_paths() {
        assert_eq!(grant_path("/a/b"), Ok(PathBuf::from("/a/b")));
        assert_eq!(grant_path("/a//./b/"), Ok(PathBuf::from("/a/b")));
        assert!(grant_path("a/b").unwrap_err().contains("absolute"));
        assert!(grant_path("").unwrap_err().contains("absolute"));
        assert!(grant_path("/a/../b").unwrap_err().contains("`..`"));
    }

    /// A data root that doesn't exist, so only the lexical checks apply.
    fn protected() -> Protected {
        Protected::new(
            Path::new("/srv/giles"),
            Path::new("/srv/giles/run"),
            Path::new("/srv/giles/data/toolset/files"),
        )
    }

    #[test]
    fn overlap_with_the_data_root_in_either_direction() {
        let p = protected();
        for ok in [
            "/srv/giles/data/toolset/files",
            "/srv/giles/data/toolset/files/sub",
            "/srv/other",
            "/srv/giles-other",
            "/srv/gile",
            "/opt/homebrew",
            "/srv/giles2/data",
        ] {
            assert_eq!(p.check(Path::new(ok)), Ok(()), "{ok}");
        }
        for bad in [
            "/",
            "/srv",
            "/srv/giles",
            "/srv/giles/giles.toml",
            "/srv/giles/secrets.toml",
            "/srv/giles/plugins",
            "/srv/giles/plugins/giles-channel-irc",
            "/srv/giles/data",
            "/srv/giles/data/toolset",
            "/srv/giles/data/toolset/files-other",
            "/srv/giles/data/toolset/other",
            "/srv/giles/data/channel/files",
            "/srv/giles/data/provider/files",
            "/srv/giles/run/admin.sock",
        ] {
            let err = p.check(Path::new(bad)).unwrap_err();
            assert!(
                err.contains("overlaps the data root, /srv/giles"),
                "{bad}: {err}"
            );
            assert!(err.contains("/srv/giles/data/toolset/files"), "{err}");
        }
    }

    #[test]
    fn overlap_with_a_fallback_socket_directory() {
        let p = Protected::new(
            Path::new("/srv/giles"),
            Path::new("/tmp/giles-0123456789abcdef"),
            Path::new("/srv/giles/data/toolset/x"),
        );
        assert_eq!(p.check(Path::new("/tmp/other")), Ok(()));
        for bad in [
            "/tmp",
            "/tmp/giles-0123456789abcdef",
            "/tmp/giles-0123456789abcdef/admin.sock",
        ] {
            let err = p.check(Path::new(bad)).unwrap_err();
            assert!(
                err.contains("overlaps the socket directory"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn symlinks_are_resolved_when_they_exist() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let own = root.join("data/toolset/files");
        fs::create_dir_all(&own).unwrap();
        fs::create_dir_all(dir.path().join("outside")).unwrap();
        let link_to_root = dir.path().join("link");
        std::os::unix::fs::symlink(&root, &link_to_root).unwrap();
        let link_in_own = own.join("escape");
        std::os::unix::fs::symlink(root.join("secrets.toml"), &link_in_own).unwrap();
        fs::write(root.join("secrets.toml"), "").unwrap();
        let link_to_outside = own.join("docs");
        std::os::unix::fs::symlink(dir.path().join("outside"), &link_to_outside).unwrap();

        let p = Protected::new(&root, &root.join("run"), &own);
        assert_eq!(p.check(&dir.path().join("outside")), Ok(()));
        assert_eq!(p.check(&own.join("sub")), Ok(()));
        // A grant that leads into the data root through a symlink.
        let err = p.check(&link_to_root.join("giles.toml")).unwrap_err();
        assert!(err.contains("overlaps the data root"), "{err}");
        assert!(p.check(&link_to_root).is_err());
        // A symlink inside the instance's own directory that leads out of it.
        assert!(p.check(&link_in_own).is_err());
        // One leading somewhere harmless is fine.
        assert_eq!(p.check(&link_to_outside), Ok(()));
    }
}
