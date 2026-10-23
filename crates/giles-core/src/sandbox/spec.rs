//! The trampoline's input: a JSON sandbox spec.
//!
//! The core builds a [`Spec`] for each launch from the instance's `sandbox` grants in `giles.toml`
//! and the per-kind defaults (PLAN section 12.4), writes it with [`Spec::to_json`] to a file, and
//! passes that file's path to `giles sandbox-exec`. The trampoline reads it back with
//! [`Spec::load`], which rejects unknown fields and anything [`Spec::validate`] rejects, so a spec
//! the trampoline doesn't fully understand never runs a command.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The `PATH` the command gets when the spec doesn't set [`Spec::path`].
pub const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// Name of the command's temporary directory inside its data directory (`TMPDIR`).
pub const TMP_DIR_NAME: &str = "tmp";

/// The variable the trampoline sets to the data directory. Derived from [`Spec::data_dir`], so a
/// spec can't set it in [`Spec::env`].
pub const PLUGIN_DATA_ENV: &str = "GILES_PLUGIN_DATA";

/// Prefix of the variables a spec may pass in [`Spec::env`] (besides `LANG`).
const GILES_ENV_PREFIX: &str = "GILES_";

/// What one sandboxed launch may do, and the environment it gets.
///
/// Serialized as a JSON object with these field names. Every field but `data_dir` is optional and
/// defaults to the most restrictive value. The command itself is not part of the spec; it follows
/// `--` on the trampoline's command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The instance's data directory (`data/plugins/<instance>/`, PLAN section 11.6). Absolute,
    /// and it must exist. Always writable; it becomes `HOME`, `GILES_PLUGIN_DATA`, and the working
    /// directory, and `TMPDIR` is [`TMP_DIR_NAME`] inside it.
    pub data_dir: PathBuf,
    /// Extra read grants: absolute paths the command may read, recursively. The backend adds the
    /// platform defaults (the command, system libraries, TLS certificates, DNS config).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read: Vec<PathBuf>,
    /// Extra write grants beyond [`Spec::data_dir`]: absolute paths the command may write.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write: Vec<PathBuf>,
    /// Extra execute grants: absolute paths under which the command may run programs. The command
    /// itself is always granted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execute: Vec<PathBuf>,
    /// IP network access.
    #[serde(default)]
    pub network: Network,
    /// Unix sockets the command may connect to: absolute paths (PLAN section 7.4). Empty (the
    /// default) means no Unix sockets. On Linux a non-empty list lets the command create
    /// `AF_UNIX` sockets at all (Landlock restricts connecting by path only from ABI 9); macOS
    /// allows connecting to exactly these paths. `socketpair` is always allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unix: Vec<PathBuf>,
    /// The lowest Landlock ABI the launch accepts (Linux only; macOS ignores it). Unset accepts
    /// any ABI the host check accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_abi: Option<u32>,
    /// Runs the command without any sandbox, with a warning (the per-instance `unsandboxed = true`
    /// of PLAN section 12). The environment is still scrubbed.
    #[serde(default)]
    pub unsandboxed: bool,
    /// Variables passed to the command: `GILES_*` names (except the derived
    /// [`PLUGIN_DATA_ENV`]) and `LANG`. Nothing else from the trampoline's environment reaches
    /// the command, except `LANG` when this map doesn't set it, and [`Spec::inherit_env`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Variables the command takes from the trampoline's own environment, by name; one the
    /// trampoline doesn't have is left unset. This is how a stdio toolset's configured `env`
    /// (often credentials) reaches it without being written to the spec file: the core sets the
    /// values on the trampoline's process. Any name but the derived `HOME`, `TMPDIR`, `PATH`, and
    /// [`PLUGIN_DATA_ENV`]; an inherited value replaces one from [`Spec::env`].
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub inherit_env: BTreeSet<String>,
    /// The command's `PATH`: absolute directories separated by `:`. Unset means
    /// [`DEFAULT_PATH`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// IP network access for a launch.
///
/// In JSON: `false` ([`Network::Off`]), `true` ([`Network::All`]), or a list of `"tcp:<port>"`
/// grants ([`Network::Tcp`]); an empty list is `false`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Network {
    /// No IP sockets.
    #[default]
    Off,
    /// All IP traffic.
    All,
    /// IP sockets, with TCP connections allowed only to these remote ports (enforced where the
    /// kernel has Landlock ABI 4 or later; PLAN section 12.4). Never empty, never port 0. No port
    /// may be bound.
    Tcp(BTreeSet<u16>),
}

/// A spec that can't be read or is invalid.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpecError {
    /// The spec file can't be read.
    #[error("cannot read sandbox spec {}", .path.display())]
    Read {
        /// The spec file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The spec is not valid JSON or doesn't match the format. (The JSON error is part of the
    /// message, not a separate source, so it reads as one line.)
    #[error("invalid sandbox spec: {0}")]
    Json(serde_json::Error),
    /// A path in the spec is unusable.
    #[error("invalid sandbox spec: `{field}` path {} {reason}", .path.display())]
    Path {
        /// The spec field holding the path.
        field: &'static str,
        /// The offending path.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A variable in `env` is not one a spec may pass.
    #[error(
        "invalid sandbox spec: `env` may only set GILES_* variables (other than \
         {PLUGIN_DATA_ENV}) and LANG, not {0:?}"
    )]
    EnvName(String),
    /// A value in `env` contains a NUL byte.
    #[error("invalid sandbox spec: the value of `env.{0}` contains a NUL byte")]
    EnvValue(String),
    /// A name in `inherit_env` can't be passed.
    #[error(
        "invalid sandbox spec: `inherit_env` may not name {0:?}: it is empty, contains `=` or \
         NUL, or is set by the sandbox (HOME, TMPDIR, PATH, {PLUGIN_DATA_ENV})"
    )]
    InheritEnvName(String),
    /// A `network` port list is empty or contains port 0.
    #[error("invalid sandbox spec: `network` {0}")]
    Network(&'static str),
    /// `min_abi` is 0.
    #[error("invalid sandbox spec: `min_abi` must be at least 1")]
    MinAbi,
}

impl Spec {
    /// A spec for `data_dir` with no extra grants, no network, and nothing in the environment
    /// beyond the defaults.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            read: Vec::new(),
            write: Vec::new(),
            execute: Vec::new(),
            network: Network::Off,
            unix: Vec::new(),
            min_abi: None,
            unsandboxed: false,
            env: BTreeMap::new(),
            inherit_env: BTreeSet::new(),
            path: None,
        }
    }

    /// Reads, parses, and validates the spec file at `path`.
    pub fn load(path: &Path) -> Result<Self, SpecError> {
        let json = fs::read_to_string(path).map_err(|source| SpecError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&json)
    }

    /// Parses and validates a JSON spec.
    pub fn parse(json: &str) -> Result<Self, SpecError> {
        let spec: Self = serde_json::from_str(json).map_err(SpecError::Json)?;
        spec.validate()?;
        Ok(spec)
    }

    /// The spec as JSON, for the trampoline. Validates it first, so the core finds a bad spec
    /// before it launches anything.
    pub fn to_json(&self) -> Result<String, SpecError> {
        self.validate()?;
        serde_json::to_string(self).map_err(SpecError::Json)
    }

    /// Checks everything the type doesn't: every path is absolute, contains no `..` and no NUL;
    /// `env` names and values; `network` ports; `min_abi`. Doesn't touch the filesystem.
    pub fn validate(&self) -> Result<(), SpecError> {
        check_path("data_dir", &self.data_dir)?;
        for (field, paths) in [
            ("read", &self.read),
            ("write", &self.write),
            ("execute", &self.execute),
            ("unix", &self.unix),
        ] {
            for path in paths {
                check_path(field, path)?;
            }
        }
        if let Some(search_path) = &self.path {
            if search_path.is_empty() {
                return Err(path_error("path", "", "is empty"));
            }
            for dir in search_path.split(':') {
                check_path("path", Path::new(dir))?;
            }
        }
        for (name, value) in &self.env {
            if !is_passable_env_name(name) {
                return Err(SpecError::EnvName(name.clone()));
            }
            if value.contains('\0') {
                return Err(SpecError::EnvValue(name.clone()));
            }
        }
        if let Some(name) = self
            .inherit_env
            .iter()
            .find(|name| !is_inheritable_env_name(name))
        {
            return Err(SpecError::InheritEnvName(name.clone()));
        }
        if let Network::Tcp(ports) = &self.network {
            if ports.is_empty() {
                return Err(SpecError::Network(
                    "port list is empty; use `false` for no network",
                ));
            }
            if ports.contains(&0) {
                return Err(SpecError::Network("port 0 is not a port"));
            }
        }
        if self.min_abi == Some(0) {
            return Err(SpecError::MinAbi);
        }
        Ok(())
    }

    /// The command's temporary directory: [`TMP_DIR_NAME`] in the data directory.
    pub fn tmp_dir(&self) -> PathBuf {
        self.data_dir.join(TMP_DIR_NAME)
    }

    /// The command's `PATH`: [`Spec::path`], else [`DEFAULT_PATH`].
    pub fn search_path(&self) -> &str {
        self.path.as_deref().unwrap_or(DEFAULT_PATH)
    }
}

fn path_error(field: &'static str, path: impl Into<PathBuf>, reason: &'static str) -> SpecError {
    SpecError::Path {
        field,
        path: path.into(),
        reason,
    }
}

/// Rejects relative paths, `..` components (they would make grant overlap checks unreliable), and
/// NUL bytes (they can't reach a syscall).
fn check_path(field: &'static str, path: &Path) -> Result<(), SpecError> {
    if path.as_os_str().as_bytes().contains(&0) {
        return Err(path_error(field, path, "contains a NUL byte"));
    }
    if !path.is_absolute() {
        return Err(path_error(field, path, "is not absolute"));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(path_error(field, path, "contains `..`"));
    }
    Ok(())
}

/// `LANG`, or `GILES_` followed by at least one of `A-Z`, `0-9`, `_`, except
/// [`PLUGIN_DATA_ENV`].
fn is_passable_env_name(name: &str) -> bool {
    if name == "LANG" {
        return true;
    }
    match name.strip_prefix(GILES_ENV_PREFIX) {
        Some(rest) => {
            !rest.is_empty()
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                && name != PLUGIN_DATA_ENV
        }
        None => false,
    }
}

/// The variables the trampoline derives from the spec ([`scrubbed_env`](super::scrubbed_env)),
/// which a spec can neither set nor inherit.
pub const DERIVED_ENV: [&str; 4] = ["HOME", "TMPDIR", "PATH", PLUGIN_DATA_ENV];

/// Any valid variable name except the [`DERIVED_ENV`] ones.
fn is_inheritable_env_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['=', '\0']) && !DERIVED_ENV.contains(&name)
}

/// Parses one `tcp:<port>` grant.
fn parse_port_grant(grant: &str) -> Result<u16, String> {
    let port = grant
        .strip_prefix("tcp:")
        .ok_or_else(|| format!("network grant {grant:?} is not `tcp:<port>`"))?;
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(format!(
            "network grant {grant:?} has no valid port (1 to 65535)"
        )),
        Ok(port) => Ok(port),
    }
}

impl Serialize for Network {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => serializer.serialize_bool(false),
            Self::All => serializer.serialize_bool(true),
            Self::Tcp(ports) => {
                let mut seq = serializer.serialize_seq(Some(ports.len()))?;
                for port in ports {
                    seq.serialize_element(&format!("tcp:{port}"))?;
                }
                seq.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Network {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NetworkVisitor;

        impl<'de> Visitor<'de> for NetworkVisitor {
            type Value = Network;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`true`, `false`, or a list of `tcp:<port>` grants")
            }

            fn visit_bool<E: de::Error>(self, all: bool) -> Result<Network, E> {
                Ok(if all { Network::All } else { Network::Off })
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Network, A::Error> {
                let mut ports = BTreeSet::new();
                while let Some(grant) = seq.next_element::<String>()? {
                    ports.insert(parse_port_grant(&grant).map_err(de::Error::custom)?);
                }
                Ok(if ports.is_empty() {
                    Network::Off
                } else {
                    Network::Tcp(ports)
                })
            }
        }

        deserializer.deserialize_any(NetworkVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_err(json: &str) -> String {
        Spec::parse(json).unwrap_err().to_string()
    }

    #[test]
    fn minimal_spec_is_fully_restrictive() {
        let spec = Spec::parse(r#"{"data_dir": "/d"}"#).unwrap();
        assert_eq!(spec, Spec::new("/d"));
        assert_eq!(spec.network, Network::Off);
        assert!(spec.unix.is_empty() && !spec.unsandboxed);
        assert_eq!(spec.search_path(), DEFAULT_PATH);
        assert_eq!(spec.tmp_dir(), Path::new("/d/tmp"));
    }

    #[test]
    fn full_spec_parses() {
        let spec = Spec::parse(
            r#"{
                "data_dir": "/g/data/plugins/irc",
                "read": ["/opt/homebrew"],
                "write": ["/srv/out"],
                "execute": ["/opt/homebrew/bin"],
                "network": ["tcp:6697", "tcp:443", "tcp:443"],
                "unix": ["/run/user/1000/bus"],
                "min_abi": 4,
                "unsandboxed": false,
                "env": {"GILES_PLUGIN_MAGIC": "cookie", "LANG": "C.UTF-8"},
                "path": "/opt/homebrew/bin:/usr/bin"
            }"#,
        )
        .unwrap();
        assert_eq!(spec.read, [Path::new("/opt/homebrew")]);
        assert_eq!(spec.write, [Path::new("/srv/out")]);
        assert_eq!(spec.execute, [Path::new("/opt/homebrew/bin")]);
        assert_eq!(spec.network, Network::Tcp(BTreeSet::from([443, 6697])));
        assert_eq!(spec.unix, [Path::new("/run/user/1000/bus")]);
        assert_eq!(spec.min_abi, Some(4));
        assert_eq!(spec.env["GILES_PLUGIN_MAGIC"], "cookie");
        assert_eq!(spec.search_path(), "/opt/homebrew/bin:/usr/bin");
    }

    #[test]
    fn to_json_round_trips() {
        let mut spec = Spec::new("/d");
        spec.read.push("/r".into());
        spec.unix.push("/s.sock".into());
        spec.network = Network::Tcp(BTreeSet::from([443]));
        spec.min_abi = Some(6);
        spec.env.insert("GILES_PLUGIN_PROTOCOLS".into(), "1".into());
        for spec in [spec, Spec::new("/d")] {
            assert_eq!(Spec::parse(&spec.to_json().unwrap()).unwrap(), spec);
        }
        for network in [Network::Off, Network::All] {
            let spec = Spec {
                network,
                ..Spec::new("/d")
            };
            assert_eq!(Spec::parse(&spec.to_json().unwrap()).unwrap(), spec);
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = parse_err(r#"{"data_dir": "/d", "unsandbox": true}"#);
        assert!(err.contains("unknown field `unsandbox`"), "{err}");
    }

    #[test]
    fn data_dir_is_required_and_absolute() {
        assert!(parse_err("{}").contains("missing field `data_dir`"));
        for (json, reason) in [
            (r#"{"data_dir": "d"}"#, "`data_dir` path d is not absolute"),
            (r#"{"data_dir": ""}"#, "`data_dir` path  is not absolute"),
            (
                r#"{"data_dir": "/a/../b"}"#,
                "`data_dir` path /a/../b contains `..`",
            ),
            (r#"{"data_dir": "/a\u0000"}"#, "contains a NUL byte"),
        ] {
            let err = parse_err(json);
            assert!(err.contains(reason), "{json}: {err}");
        }
    }

    #[test]
    fn relative_and_dotdot_grants_are_rejected() {
        for field in ["read", "write", "execute", "unix"] {
            for (path, reason) in [("rel/x", "is not absolute"), ("/x/..", "contains `..`")] {
                let err = parse_err(&format!(r#"{{"data_dir": "/d", "{field}": ["{path}"]}}"#));
                assert!(
                    err.contains(&format!("`{field}` path {path} {reason}")),
                    "{err}"
                );
            }
        }
    }

    #[test]
    fn search_path_entries_must_be_absolute() {
        for path in ["", "bin", "/usr/bin:", "/usr/bin::/bin", "/usr/bin:./bin"] {
            let json = format!(r#"{{"data_dir": "/d", "path": "{path}"}}"#);
            assert!(parse_err(&json).contains("`path` path"), "{path:?}");
        }
        Spec::parse(r#"{"data_dir": "/d", "path": "/a:/b"}"#).unwrap();
    }

    #[test]
    fn only_giles_and_lang_variables_may_be_passed() {
        for name in ["GILES_PLUGIN_MAGIC", "GILES_X_1", "LANG"] {
            let json = format!(r#"{{"data_dir": "/d", "env": {{"{name}": "v"}}}}"#);
            Spec::parse(&json).unwrap();
        }
        for name in [
            "HOME",
            "TMPDIR",
            "PATH",
            "LC_ALL",
            "GILES_",
            "GILES_lower",
            "GILES_A=B",
            "giles_x",
            PLUGIN_DATA_ENV,
        ] {
            let json = format!(r#"{{"data_dir": "/d", "env": {{"{name}": "v"}}}}"#);
            let err = parse_err(&json);
            assert!(err.contains("`env` may only set"), "{name}: {err}");
        }
        let err = parse_err(r#"{"data_dir": "/d", "env": {"GILES_X": "a\u0000b"}}"#);
        assert!(err.contains("`env.GILES_X` contains a NUL byte"), "{err}");
    }

    #[test]
    fn inherited_variables_are_any_name_but_the_derived_ones() {
        let spec = Spec::parse(
            r#"{"data_dir": "/d", "inherit_env": ["GITHUB_TOKEN", "lower_case", "LANG"]}"#,
        )
        .unwrap();
        assert_eq!(
            spec.inherit_env,
            BTreeSet::from(["GITHUB_TOKEN", "LANG", "lower_case"].map(String::from))
        );
        assert_eq!(Spec::parse(&spec.to_json().unwrap()).unwrap(), spec);
        for name in [
            "HOME",
            "TMPDIR",
            "PATH",
            PLUGIN_DATA_ENV,
            "",
            "A=B",
            "A\\u0000",
        ] {
            let json = format!(r#"{{"data_dir": "/d", "inherit_env": ["{name}"]}}"#);
            let err = parse_err(&json);
            assert!(err.contains("`inherit_env` may not name"), "{name}: {err}");
        }
    }

    #[test]
    fn network_grants() {
        let network = |json: &str| {
            Spec::parse(&format!(r#"{{"data_dir": "/d", "network": {json}}}"#)).map(|s| s.network)
        };
        assert_eq!(network("false").unwrap(), Network::Off);
        assert_eq!(network("true").unwrap(), Network::All);
        assert_eq!(network("[]").unwrap(), Network::Off);
        assert_eq!(
            network(r#"["tcp:1", "tcp:65535"]"#).unwrap(),
            Network::Tcp(BTreeSet::from([1, 65535]))
        );
        for (json, reason) in [
            (r#"["tcp:0"]"#, "has no valid port"),
            (r#"["tcp:65536"]"#, "has no valid port"),
            (r#"["tcp:"]"#, "has no valid port"),
            (r#"["tcp:-1"]"#, "has no valid port"),
            (r#"["udp:53"]"#, "is not `tcp:<port>`"),
            (r#"["443"]"#, "is not `tcp:<port>`"),
            ("[443]", "expected a string"),
            (r#""all""#, "`true`, `false`, or a list"),
        ] {
            let err = network(json).unwrap_err().to_string();
            assert!(err.contains(reason), "{json}: {err}");
        }
    }

    #[test]
    fn constructed_network_is_validated() {
        for (ports, reason) in [
            (BTreeSet::new(), "port list is empty"),
            (BTreeSet::from([0, 443]), "port 0"),
        ] {
            let spec = Spec {
                network: Network::Tcp(ports),
                ..Spec::new("/d")
            };
            let err = spec.to_json().unwrap_err().to_string();
            assert!(err.contains(reason), "{err}");
        }
    }

    #[test]
    fn min_abi_must_be_positive() {
        assert!(parse_err(r#"{"data_dir": "/d", "min_abi": 0}"#).contains("at least 1"));
        assert!(parse_err(r#"{"data_dir": "/d", "min_abi": -1}"#).contains("invalid value"));
    }

    #[test]
    fn missing_spec_file_names_the_path() {
        let err = Spec::load(Path::new("/nonexistent/spec.json")).unwrap_err();
        assert!(matches!(err, SpecError::Read { .. }));
        assert!(err.to_string().contains("/nonexistent/spec.json"), "{err}");
    }
}
