//! The tool-list cache (PLAN section 7.9): each toolset's last tool list, in
//! `<root>/cache/toolsets/<instance>.json`, so a stdio server that is stopped (idle, crashed, or
//! not started yet) still has its tools offered.
//!
//! An entry records its key: the source (`sha256:<hex>` of a stdio server's program, or a remote
//! server's URL), the instance's config hash, and the version the server reported. The version is
//! only known once the server runs, and every start lists the tools again and replaces the entry,
//! so a lookup before a start matches the source and the config hash.

use std::fs::{self, File};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use giles_types::ToolSpec;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::config::{ConfigHash, InstanceName};
use crate::plugin_host::write_private;

/// The file format's version; an entry with another one is ignored.
const FORMAT: u32 = 1;

/// What a cached list is valid for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CacheKey {
    /// `sha256:<hex>` of a stdio server's program, or a remote server's URL.
    pub source: String,
    /// The instance's config hash, taken before secrets are resolved.
    pub config_hash: String,
    /// The version the server reported in `initialize` (`serverInfo.version`).
    pub server_version: Option<String>,
}

impl CacheKey {
    /// Whether this key was stored for `source` and `config_hash`.
    fn matches(&self, source: &str, config_hash: &ConfigHash) -> bool {
        self.source == source && self.config_hash == config_hash.to_string()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    format: u32,
    key: CacheKey,
    tools: Vec<ToolSpec>,
}

/// The cache directory.
#[derive(Debug, Clone)]
pub(crate) struct ToolCache {
    dir: PathBuf,
}

impl ToolCache {
    /// A cache in `dir` (`<root>/cache/toolsets/`), created on the first store.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn file(&self, instance: &InstanceName) -> PathBuf {
        self.dir.join(format!("{instance}.json"))
    }

    /// `instance`'s cached tools, if they were stored for `source` and `config_hash`. A missing,
    /// unreadable, or stale entry is `None` (all but a missing one logged).
    pub(crate) fn lookup(
        &self,
        instance: &InstanceName,
        source: &str,
        config_hash: &ConfigHash,
    ) -> Option<Vec<ToolSpec>> {
        let file = self.file(instance);
        let json = match fs::read(&file) {
            Ok(json) => json,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => {
                warn!(toolset = %instance, file = %file.display(), %error, "cannot read the tool cache");
                return None;
            }
        };
        let entry: Entry = match serde_json::from_slice(&json) {
            Ok(entry) => entry,
            Err(error) => {
                warn!(toolset = %instance, file = %file.display(), %error, "ignoring an unreadable tool cache");
                return None;
            }
        };
        if entry.format != FORMAT || !entry.key.matches(source, config_hash) {
            debug!(toolset = %instance, key = ?entry.key, "the cached tools are stale");
            return None;
        }
        Some(entry.tools)
    }

    /// Stores `tools` as `instance`'s list for `key`, atomically (mode 0600). A failure is logged:
    /// the cache only matters while the server is stopped.
    pub(crate) fn store(&self, instance: &InstanceName, key: &CacheKey, tools: &[ToolSpec]) {
        let file = self.file(instance);
        let entry = serde_json::json!({ "format": FORMAT, "key": key, "tools": tools });
        if let Err(error) = write_private(&file, entry.to_string().as_bytes()) {
            warn!(toolset = %instance, file = %file.display(), %error, "cannot write the tool cache");
        }
    }
}

/// `sha256:<hex>` of the file at `path`.
pub(crate) fn file_digest(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        match file.read(&mut buffer)? {
            0 => break,
            n => hasher.update(&buffer[..n]),
        }
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(format!("sha256:{hex}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: format!("{name} things"),
            input_schema: json!({"type": "object"}),
        }
    }

    fn hash(text: &str) -> ConfigHash {
        ConfigHash::of(&toml::from_str(text).unwrap())
    }

    fn key(source: &str, config: &ConfigHash, version: &str) -> CacheKey {
        CacheKey {
            source: source.to_owned(),
            config_hash: config.to_string(),
            server_version: Some(version.to_owned()),
        }
    }

    #[test]
    fn a_stored_list_is_found_for_its_source_and_config_only() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ToolCache::new(dir.path().join("cache/toolsets"));
        let files = InstanceName::new("files").unwrap();
        let config = hash("command = \"x\"");
        assert_eq!(cache.lookup(&files, "sha256:aa", &config), None);

        cache.store(&files, &key("sha256:aa", &config, "1.0"), &[tool("read")]);
        assert_eq!(
            cache.lookup(&files, "sha256:aa", &config),
            Some(vec![tool("read")])
        );
        // Another binary or another config: stale.
        assert_eq!(cache.lookup(&files, "sha256:bb", &config), None);
        assert_eq!(
            cache.lookup(&files, "sha256:aa", &hash("command = \"y\"")),
            None
        );
        // Another instance has its own entry.
        assert_eq!(
            cache.lookup(&InstanceName::new("other").unwrap(), "sha256:aa", &config),
            None
        );
        // A start stores again, replacing the entry.
        cache.store(&files, &key("sha256:bb", &config, "1.1"), &[tool("write")]);
        assert_eq!(cache.lookup(&files, "sha256:aa", &config), None);
        assert_eq!(
            cache.lookup(&files, "sha256:bb", &config),
            Some(vec![tool("write")])
        );
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &fs::metadata(dir.path().join("cache/toolsets/files.json"))
                .unwrap()
                .permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_damaged_or_foreign_entry_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ToolCache::new(dir.path().to_path_buf());
        let files = InstanceName::new("files").unwrap();
        let config = hash("");
        fs::write(dir.path().join("files.json"), "{not json").unwrap();
        assert_eq!(cache.lookup(&files, "s", &config), None);
        let entry = json!({
            "format": FORMAT + 1,
            "key": key("s", &config, "1"),
            "tools": [tool("x")],
        });
        fs::write(dir.path().join("files.json"), entry.to_string()).unwrap();
        assert_eq!(cache.lookup(&files, "s", &config), None);
    }

    #[test]
    fn digests_are_sha256_of_the_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        fs::write(&path, "abc").unwrap();
        assert_eq!(
            file_digest(&path).unwrap(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
