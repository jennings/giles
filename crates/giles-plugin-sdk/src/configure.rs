//! What a plugin's `configure` function gets, whatever its kind.

use std::path::{Path, PathBuf};

use giles_types::Result;
use serde::de::DeserializeOwned;

use crate::blobs::Blobs;

/// What `configure` gets: the instance config and the plugin's resources.
#[derive(Debug)]
pub struct Configure {
    instance: String,
    config: serde_json::Value,
    data_dir: PathBuf,
    blobs: Blobs,
}

impl Configure {
    pub(crate) fn new(
        instance: String,
        config: serde_json::Value,
        data_dir: PathBuf,
        blobs: Blobs,
    ) -> Self {
        Self {
            instance,
            config,
            data_dir,
            blobs,
        }
    }

    /// The instance name, e.g. `libera` for `[channel.libera]`. For logs and messages only: the
    /// plugin's state belongs in [`data_dir`](Configure::data_dir), which already is per instance.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The instance's `config` table from `giles.toml`, as JSON, with secrets resolved.
    ///
    /// A remote environment plugin (`serve --config FILE`) is the exception: its settings are the
    /// `[config]` table of its own file (`{}` if the file has none), and the core sends none.
    pub fn config(&self) -> &serde_json::Value {
        &self.config
    }

    /// Deserializes [`config`](Configure::config). A mismatch is
    /// [`Error::InvalidRequest`](giles_types::Error::InvalidRequest) naming the offending key, e.g.
    /// `invalid config: tls.verify: invalid type: string "yes", expected a boolean`; the core shows
    /// it to the operator. Use `#[serde(deny_unknown_fields)]` to catch misspelled keys.
    pub fn parse_config<T: DeserializeOwned>(&self) -> Result<T> {
        serde_path_to_error::deserialize(&self.config).map_err(|e| {
            let path = e.path().to_string();
            let error = e.into_inner();
            giles_types::Error::InvalidRequest(if path == "." {
                format!("invalid config: {error}")
            } else {
                format!("invalid config: {path}: {error}")
            })
        })
    }

    /// The instance's data directory: the plugin's only writable directory, kept across restarts
    /// and upgrades (`GILES_PLUGIN_DATA` for a launched plugin, the [`RemoteServer`]'s for a
    /// remote one). The SDK's own state is in its `giles-sdk/` subdirectory; leave that alone.
    ///
    /// [`RemoteServer`]: crate::RemoteServer
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The plugin's blob store, for attachments in both directions.
    pub fn blobs(&self) -> &Blobs {
        &self.blobs
    }
}
