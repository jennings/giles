//! The plugin's configuration: the `[environment.<name>.config]` table of `giles.toml`, or the
//! `[config]` table of a remote plugin's own config file.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::image::{Allowlist, DEFAULT_PATTERNS, normalize_registry};

/// Where the Docker Engine API listens when neither `socket` nor `DOCKER_HOST` says.
pub const DEFAULT_SOCKET: &str = "/var/run/docker.sock";

/// The network workspaces join unless `network` says otherwise.
pub const DEFAULT_NETWORK: &str = "bridge";

/// What the operator configures. Every key is optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The Docker Engine API's unix socket. Default: the path in `DOCKER_HOST` if that is set,
    /// else `/var/run/docker.sock`.
    pub socket: Option<PathBuf>,
    /// Image patterns allowed in addition to the built-in list, as `[registry/]image[:tag]`.
    pub images: Vec<String>,
    /// Whether the built-in list (ubuntu, debian, alpine, python, node, golang, rust, ruby,
    /// eclipse-temurin) is allowed too.
    pub default_images: bool,
    /// Logins for pulling images, by registry host (`docker.io`, `ghcr.io`, ...).
    pub registries: BTreeMap<String, Registry>,
    /// A memory limit per workspace container, e.g. `2g` or `512m`.
    pub memory: Option<String>,
    /// A CPU limit per workspace container, in CPUs (`1.5`).
    pub cpus: Option<f64>,
    /// A limit on the processes in a workspace container.
    pub pids: Option<u32>,
    /// The Docker network containers join: `bridge` (default), `none`, or a named network.
    pub network: String,
    /// Environment variables set in every container. The model can read them.
    pub env: BTreeMap<String, String>,
    /// The directory holding the Linux helper binaries, `giles-workspace-helper-<arch>`. Default:
    /// the directory of the plugin's own executable.
    pub helper_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            socket: None,
            images: Vec::new(),
            default_images: true,
            registries: BTreeMap::new(),
            memory: None,
            cpus: None,
            pids: None,
            network: DEFAULT_NETWORK.to_owned(),
            env: BTreeMap::new(),
            helper_dir: None,
        }
    }
}

/// A registry login.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    /// The account name.
    pub username: String,
    /// The password or access token.
    pub password: String,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl Config {
    /// Parses a TOML table of settings.
    #[cfg(test)]
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::invalid(format!("invalid configuration: {e}")))
    }

    /// Reads settings from a JSON object (a plugin's `Configure` payload).
    pub fn from_json(value: serde_json::Value) -> Result<Self> {
        serde_json::from_value(value)
            .map_err(|e| Error::invalid(format!("invalid configuration: {e}")))
    }
}

/// A [`Config`] checked and resolved into what the backend uses.
#[derive(Debug, Clone)]
pub struct Settings {
    pub(crate) socket: PathBuf,
    pub(crate) allowlist: Allowlist,
    pub(crate) registries: BTreeMap<String, Registry>,
    pub(crate) memory_bytes: Option<i64>,
    pub(crate) nano_cpus: Option<i64>,
    pub(crate) pids_limit: Option<i64>,
    pub(crate) network: String,
    /// `NAME=value` pairs.
    pub(crate) env: Vec<String>,
    pub(crate) helper_dir: Option<PathBuf>,
}

impl Settings {
    /// Checks `config` and fills in defaults. `docker_host` is the `DOCKER_HOST` environment
    /// variable, if set.
    pub fn resolve(config: Config, docker_host: Option<&str>) -> Result<Self> {
        let socket = match &config.socket {
            Some(path) => strip_unix_scheme(path),
            None => socket_from_docker_host(docker_host)?,
        };

        let defaults: &[&str] = if config.default_images {
            DEFAULT_PATTERNS
        } else {
            &[]
        };
        let allowlist = Allowlist::new(
            defaults
                .iter()
                .copied()
                .chain(config.images.iter().map(String::as_str)),
        )
        .map_err(|e| Error::invalid(format!("images: {e}")))?;

        let mut registries = BTreeMap::new();
        for (host, login) in config.registries {
            let host = normalize_registry(&host)
                .map_err(|e| Error::invalid(format!("registries: {e}")))?;
            if login.username.is_empty() || login.password.is_empty() {
                return Err(Error::invalid(format!(
                    "registries.{host}: username and password are both required"
                )));
            }
            registries.insert(host, login);
        }

        let memory_bytes = config
            .memory
            .as_deref()
            .map(parse_memory)
            .transpose()
            .map_err(|e| Error::invalid(format!("memory: {e}")))?;
        let nano_cpus = config
            .cpus
            .map(|cpus| {
                if cpus.is_finite() && cpus > 0.0 && cpus <= 1024.0 {
                    // Docker counts billionths of a CPU. Rounded, so 0.1 stays 100000000.
                    #[expect(clippy::cast_possible_truncation, reason = "bounded to 1024 CPUs")]
                    Ok((cpus * 1e9).round() as i64)
                } else {
                    Err(Error::invalid(
                        "cpus: must be greater than 0 and at most 1024",
                    ))
                }
            })
            .transpose()?;
        let pids_limit = match config.pids {
            Some(0) => return Err(Error::invalid("pids: must be at least 1")),
            Some(pids) => Some(i64::from(pids)),
            None => None,
        };

        if config.network.trim().is_empty() {
            return Err(Error::invalid("network: must not be empty"));
        }

        let mut env = Vec::new();
        for (name, value) in &config.env {
            if name.is_empty() || name.contains('=') || name.contains('\0') || value.contains('\0')
            {
                return Err(Error::invalid(format!(
                    "env: {name:?} isn't a valid variable name"
                )));
            }
            if name == "HOME" {
                return Err(Error::invalid(
                    "env: HOME is set to /workspace/.home and can't be changed",
                ));
            }
            env.push(format!("{name}={value}"));
        }

        Ok(Self {
            socket,
            allowlist,
            registries,
            memory_bytes,
            nano_cpus,
            pids_limit,
            network: config.network,
            env,
            helper_dir: config.helper_dir,
        })
    }

    /// The allowed image patterns.
    pub fn allowlist(&self) -> &Allowlist {
        &self.allowlist
    }

    /// The socket the Docker API is reached on.
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

fn strip_unix_scheme(path: &Path) -> PathBuf {
    match path.to_str().and_then(|p| p.strip_prefix("unix://")) {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// The socket path `DOCKER_HOST` names, or the default when it is unset or empty.
fn socket_from_docker_host(docker_host: Option<&str>) -> Result<PathBuf> {
    match docker_host.map(str::trim).filter(|h| !h.is_empty()) {
        None => Ok(PathBuf::from(DEFAULT_SOCKET)),
        Some(host) => match host.strip_prefix("unix://") {
            Some(path) if !path.is_empty() => Ok(PathBuf::from(path)),
            _ => Err(Error::invalid(format!(
                "DOCKER_HOST is {host:?}, but only unix sockets are supported; set `socket`"
            ))),
        },
    }
}

/// Parses a memory size: a number with an optional unit of `b`, `k`, `m`, `g`, or `t` (powers of
/// 1024, case-insensitive, with an optional trailing `b` or `ib`), such as `2g`, `512MB`, or
/// `1.5g`. A bare number is bytes.
pub fn parse_memory(text: &str) -> std::result::Result<i64, String> {
    let lower = text.trim().to_ascii_lowercase();
    let trimmed = lower
        .strip_suffix("ib")
        .or_else(|| lower.strip_suffix('b'))
        .unwrap_or(&lower);
    let (number, factor) = match trimmed.chars().last() {
        Some('k') => (&trimmed[..trimmed.len() - 1], 1u64 << 10),
        Some('m') => (&trimmed[..trimmed.len() - 1], 1 << 20),
        Some('g') => (&trimmed[..trimmed.len() - 1], 1 << 30),
        Some('t') => (&trimmed[..trimmed.len() - 1], 1 << 40),
        _ => (trimmed, 1),
    };
    let number = number.trim();
    let bad = || format!("{text:?} isn't a memory size like \"2g\" or \"512m\"");
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return Err(bad());
    }
    let value: f64 = number.parse().map_err(|_| bad())?;
    #[expect(
        clippy::cast_precision_loss,
        reason = "factor is a power of two below 2^53"
    )]
    let bytes = value * factor as f64;
    // Docker refuses less than 6 MiB.
    if !bytes.is_finite() || bytes < 6.0 * 1024.0 * 1024.0 || bytes > i64::MAX as f64 {
        return Err(format!("{text:?} is out of range (at least 6m)"));
    }
    #[expect(clippy::cast_possible_truncation, reason = "range checked above")]
    Ok(bytes as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(toml: &str) -> Result<Settings> {
        Settings::resolve(Config::from_toml(toml)?, None)
    }

    #[test]
    fn an_empty_config_uses_the_defaults() {
        let settings = resolve("").unwrap();
        assert_eq!(settings.socket(), Path::new("/var/run/docker.sock"));
        assert_eq!(settings.network, "bridge");
        assert_eq!(settings.memory_bytes, None);
        assert_eq!(settings.nano_cpus, None);
        assert_eq!(settings.pids_limit, None);
        assert!(settings.env.is_empty());
        assert_eq!(settings.helper_dir, None);
        assert_eq!(
            settings.allowlist().describe(),
            "ubuntu:*, debian:*, alpine:*, python:*, node:*, golang:*, rust:*, ruby:*, eclipse-temurin:*"
        );
    }

    #[test]
    fn a_full_config_parses() {
        let settings = resolve(
            r#"
            socket = "/run/user/1000/podman/podman.sock"
            images = ["ghcr.io/acme/tools:*", "busybox"]
            default_images = false
            memory = "2g"
            cpus = 1.5
            pids = 512
            network = "none"
            helper_dir = "/opt/giles"

            [env]
            CI = "1"

            [registries."docker.io"]
            username = "me"
            password = "token"
            "#,
        )
        .unwrap();
        assert_eq!(
            settings.socket(),
            Path::new("/run/user/1000/podman/podman.sock")
        );
        assert_eq!(
            settings.allowlist().describe(),
            "ghcr.io/acme/tools:*, busybox"
        );
        assert_eq!(settings.memory_bytes, Some(2 << 30));
        assert_eq!(settings.nano_cpus, Some(1_500_000_000));
        assert_eq!(settings.pids_limit, Some(512));
        assert_eq!(settings.network, "none");
        assert_eq!(settings.env, ["CI=1"]);
        assert_eq!(
            settings.helper_dir.as_deref(),
            Some(Path::new("/opt/giles"))
        );
        assert_eq!(settings.registries["docker.io"].username, "me");
    }

    #[test]
    fn default_images_false_with_no_images_allows_nothing() {
        let settings = resolve("default_images = false").unwrap();
        assert_eq!(settings.allowlist().describe(), "none");
    }

    #[test]
    fn configured_images_add_to_the_defaults() {
        let settings = resolve(r#"images = ["busybox:1.37"]"#).unwrap();
        assert!(
            settings
                .allowlist()
                .describe()
                .ends_with("eclipse-temurin:*, busybox:1.37")
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = Config::from_toml("memroy = \"2g\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("memroy"), "{error}");
        assert!(
            Config::from_toml(
                "[registries.\"docker.io\"]\nusername = \"a\"\npassword = \"b\"\nextra = 1"
            )
            .is_err()
        );
    }

    #[test]
    fn json_config_is_the_same_shape() {
        let config = Config::from_json(serde_json::json!({
            "images": ["busybox"], "pids": 64, "registries": {"ghcr.io": {"username": "u", "password": "p"}}
        }))
        .unwrap();
        let settings = Settings::resolve(config, None).unwrap();
        assert_eq!(settings.pids_limit, Some(64));
        assert!(settings.registries.contains_key("ghcr.io"));
        assert!(Config::from_json(serde_json::json!({"nope": 1})).is_err());
    }

    #[test]
    fn registry_hosts_normalize() {
        let settings =
            resolve("[registries.\"index.docker.io\"]\nusername = \"a\"\npassword = \"b\"\n")
                .unwrap();
        assert!(settings.registries.contains_key("docker.io"));
    }

    #[test]
    fn registries_need_both_credentials() {
        assert!(resolve("[registries.\"ghcr.io\"]\nusername = \"a\"\npassword = \"\"\n").is_err());
    }

    #[test]
    fn debug_output_hides_passwords() {
        let config = Config::from_toml(
            "[registries.\"ghcr.io\"]\nusername = \"a\"\npassword = \"hunter2\"\n",
        )
        .unwrap();
        assert!(!format!("{config:?}").contains("hunter2"));
    }

    #[test]
    fn bad_values_are_refused_with_the_key_named() {
        for (toml, key) in [
            ("images = [\"ub*:1\"]", "images"),
            ("memory = \"lots\"", "memory"),
            ("memory = \"1k\"", "memory"),
            ("cpus = 0.0", "cpus"),
            ("cpus = -1.0", "cpus"),
            ("pids = 0", "pids"),
            ("network = \"\"", "network"),
            ("[env]\n\"A=B\" = \"x\"", "env"),
            ("[env]\nHOME = \"/root\"", "env"),
            (
                "[registries.\"bad host!\"]\nusername = \"a\"\npassword = \"b\"",
                "registries",
            ),
        ] {
            let error = resolve(toml).unwrap_err().to_string();
            assert!(error.contains(key), "{toml}: {error}");
        }
    }

    #[test]
    fn the_socket_follows_docker_host() {
        let default = |host| socket_from_docker_host(host).map(|p| p.display().to_string());
        assert_eq!(default(None).unwrap(), "/var/run/docker.sock");
        assert_eq!(default(Some("")).unwrap(), "/var/run/docker.sock");
        assert_eq!(
            default(Some("unix:///run/podman/podman.sock")).unwrap(),
            "/run/podman/podman.sock"
        );
        assert!(
            default(Some("tcp://127.0.0.1:2375"))
                .unwrap_err()
                .to_string()
                .contains("only unix")
        );
        assert!(default(Some("ssh://host")).is_err());
    }

    #[test]
    fn an_explicit_socket_beats_docker_host() {
        let config = Config::from_toml("socket = \"unix:///tmp/d.sock\"").unwrap();
        let settings = Settings::resolve(config, Some("unix:///other.sock")).unwrap();
        assert_eq!(settings.socket(), Path::new("/tmp/d.sock"));
        let settings = Settings::resolve(Config::default(), Some("unix:///other.sock")).unwrap();
        assert_eq!(settings.socket(), Path::new("/other.sock"));
    }

    #[test]
    fn memory_sizes_parse() {
        assert_eq!(parse_memory("2g").unwrap(), 2 << 30);
        assert_eq!(parse_memory("2G").unwrap(), 2 << 30);
        assert_eq!(parse_memory("2GB").unwrap(), 2 << 30);
        assert_eq!(parse_memory("2GiB").unwrap(), 2 << 30);
        assert_eq!(parse_memory("512m").unwrap(), 512 << 20);
        assert_eq!(parse_memory("1.5g").unwrap(), 3 << 29);
        assert_eq!(parse_memory("8388608").unwrap(), 8 << 20);
        assert_eq!(parse_memory("8192k").unwrap(), 8 << 20);
        assert_eq!(parse_memory(" 1t ").unwrap(), 1 << 40);
        for bad in ["", "g", "-1g", "1x", "1.2.3g", "5m", "0"] {
            assert!(parse_memory(bad).is_err(), "{bad:?}");
        }
    }
}
