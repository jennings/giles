//! The `Describe` cache (PLAN section 7.9): what a plugin binary reported, so `giles status` can
//! show a plugin's capabilities and requested grants while it isn't running.
//!
//! One file per instance, `<root>/cache/describe/<instance>.json`, rewritten at every launch. Its
//! key is the binary's SHA-256, the version the plugin reported, and the instance's config hash
//! (taken before secrets are resolved). A lookup before a launch knows only the binary and the
//! config, so it matches those two and returns the version recorded with them.

use std::fs;
use std::io;
use std::path::PathBuf;

use giles_plugin_sdk::convert::output_mode_from_proto;
use giles_proto::plugin::v1::{DescribeResponse, describe_response};
use giles_types::OutputMode;
use serde::{Deserialize, Serialize};

use super::grpc::hex;
use super::write_private;
use crate::config::{ConfigHash, InstanceName};
use crate::paths::DataRoot;

/// What a channel plugin reported in `Describe`, for `giles status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DescribeInfo {
    /// The plugin's own version.
    pub version: String,
    /// How it wants assistant output delivered.
    pub output: OutputMode,
    /// Whether it can start conversations.
    pub can_initiate: bool,
    /// The sandbox grants it asked for. Advisory only: never applied.
    pub requested_grants: RequestedGrants,
}

/// Sandbox grants a plugin asked for in `Describe` (PLAN section 12.4). Advisory only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RequestedGrants {
    /// All IP traffic.
    pub network_all: bool,
    /// Ports, e.g. `tcp:443`.
    pub network_ports: Vec<String>,
    /// Paths to read.
    pub read: Vec<String>,
    /// Paths to write.
    pub write: Vec<String>,
}

impl DescribeInfo {
    /// The channel plugin's `Describe` answer, or `None` without channel capabilities and a
    /// known output mode (the launch refuses those).
    pub fn from_describe(describe: &DescribeResponse) -> Option<Self> {
        let Some(describe_response::Capabilities::Channel(capabilities)) = &describe.capabilities
        else {
            return None;
        };
        let grants = describe.requested_grants.clone().unwrap_or_default();
        Some(Self {
            version: describe.version.clone(),
            output: output_mode_from_proto(capabilities.output).ok()?,
            can_initiate: capabilities.can_initiate,
            requested_grants: RequestedGrants {
                network_all: grants.network_all,
                network_ports: grants.network_ports,
                read: grants.read,
                write: grants.write,
            },
        })
    }
}

/// One cache file.
#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// The binary's SHA-256, hex.
    binary_sha256: String,
    /// The instance's config hash, hex.
    config_hash: String,
    /// What the plugin reported, its version included.
    describe: DescribeInfo,
}

/// The `Describe` cache in `<root>/cache/describe/`.
#[derive(Debug, Clone)]
pub struct DescribeCache {
    dir: PathBuf,
}

impl DescribeCache {
    /// The cache of `root`.
    pub fn new(root: &DataRoot) -> Self {
        Self {
            dir: root.cache_dir().join("describe"),
        }
    }

    fn file(&self, instance: &InstanceName) -> PathBuf {
        self.dir.join(format!("{instance}.json"))
    }

    /// What `instance` reported when it last ran the binary with SHA-256 `binary` under the
    /// config with hash `config`. `None` if it never did, or the file is unreadable.
    pub fn load(
        &self,
        instance: &InstanceName,
        binary: &[u8; 32],
        config: &ConfigHash,
    ) -> Option<DescribeInfo> {
        let path = self.file(instance);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "cannot read the Describe cache");
                return None;
            }
        };
        let entry: Entry = match serde_json::from_slice(&bytes) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "ignoring a malformed Describe cache");
                return None;
            }
        };
        (entry.binary_sha256 == hex(binary) && entry.config_hash == config.to_string())
            .then_some(entry.describe)
    }

    /// Records what `instance` reported running the binary with SHA-256 `binary` under the
    /// config with hash `config`, replacing the instance's earlier entry (atomically, mode 0600).
    ///
    /// # Errors
    ///
    /// Writing the file failed.
    pub fn store(
        &self,
        instance: &InstanceName,
        binary: &[u8; 32],
        config: &ConfigHash,
        describe: &DescribeInfo,
    ) -> io::Result<()> {
        let entry = Entry {
            binary_sha256: hex(binary),
            config_hash: config.to_string(),
            describe: describe.clone(),
        };
        let json = serde_json::to_vec_pretty(&entry).map_err(io::Error::other)?;
        write_private(&self.file(instance), &json)
    }
}

#[cfg(test)]
mod tests {
    use giles_proto::channel::v1::{ChannelCapabilities, OutputMode as PbOutputMode};
    use giles_proto::plugin::v1::{PluginKind, SandboxGrants};

    use super::*;

    fn config_hash(text: &str) -> ConfigHash {
        ConfigHash::of(&text.parse::<toml::Table>().unwrap())
    }

    fn info(version: &str) -> DescribeInfo {
        DescribeInfo {
            version: version.into(),
            output: OutputMode::Blocks,
            can_initiate: true,
            requested_grants: RequestedGrants {
                network_ports: vec!["tcp:6697".into()],
                ..RequestedGrants::default()
            },
        }
    }

    #[test]
    fn describe_answers_become_infos() {
        let describe = DescribeResponse {
            kind: PluginKind::Channel.into(),
            name: "irc".into(),
            version: "2.1".into(),
            requested_grants: Some(SandboxGrants {
                network_all: false,
                network_ports: vec!["tcp:6697".into()],
                read: vec![],
                write: vec![],
            }),
            capabilities: Some(describe_response::Capabilities::Channel(
                ChannelCapabilities {
                    output: PbOutputMode::Blocks.into(),
                    can_initiate: true,
                },
            )),
        };
        assert_eq!(DescribeInfo::from_describe(&describe), Some(info("2.1")));
        let mut no_output = describe.clone();
        no_output.capabilities = Some(describe_response::Capabilities::Channel(
            ChannelCapabilities::default(),
        ));
        assert_eq!(DescribeInfo::from_describe(&no_output), None);
        let mut provider = describe;
        provider.capabilities = None;
        assert_eq!(DescribeInfo::from_describe(&provider), None);
    }

    #[test]
    fn entries_match_the_binary_and_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("home"), dir.path()).unwrap();
        let cache = DescribeCache::new(&root);
        let instance = InstanceName::new("libera").unwrap();
        let other = InstanceName::new("oftc").unwrap();
        let (binary, config) = ([1; 32], config_hash("network = \"libera\""));
        assert_eq!(cache.load(&instance, &binary, &config), None);

        cache
            .store(&instance, &binary, &config, &info("1.0"))
            .unwrap();
        assert_eq!(cache.load(&instance, &binary, &config), Some(info("1.0")));
        // Another binary, another config, or another instance: no entry.
        assert_eq!(cache.load(&instance, &[2; 32], &config), None);
        let changed = config_hash("network = \"oftc\"");
        assert_eq!(cache.load(&instance, &binary, &changed), None);
        assert_eq!(cache.load(&other, &binary, &config), None);

        // A launch rewrites it.
        cache
            .store(&instance, &binary, &config, &info("1.1"))
            .unwrap();
        assert_eq!(cache.load(&instance, &binary, &config), Some(info("1.1")));
        let path = root.cache_dir().join("describe/libera.json");
        let mode =
            std::os::unix::fs::PermissionsExt::mode(&fs::metadata(&path).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);

        // A damaged file is a miss, not an error.
        fs::write(&path, "{").unwrap();
        assert_eq!(cache.load(&instance, &binary, &config), None);
    }
}
