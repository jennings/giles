//! Startup wiring: everything `giles serve` builds from the configuration before it serves.

use std::path::PathBuf;
use std::sync::Arc;

use giles_store::{BlobStore, Store};

use crate::blobs::BlobMedia;
use crate::config::Config;
use crate::paths::DataRoot;
use crate::registry::{Registry, RegistryError};

/// Startup failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    /// The database couldn't be opened or migrated.
    #[error("cannot open the database {}: {source}", .path.display())]
    Store {
        /// `data/giles.db`.
        path: PathBuf,
        /// Why.
        source: giles_store::Error,
    },
    /// The blob store couldn't be opened.
    #[error("cannot open the blob store {}: {source}", .path.display())]
    Blobs {
        /// `data/blobs`.
        path: PathBuf,
        /// Why.
        source: giles_store::Error,
    },
    /// A configured component couldn't be built.
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

/// The running core's shared state.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// The data root.
    pub root: DataRoot,
    /// The configuration, as loaded at startup.
    pub config: Arc<Config>,
    /// The database, `data/giles.db`.
    pub store: Store,
    /// The blob store, `data/blobs`.
    pub blobs: BlobStore,
    /// Configured instances and their objects, the tool index, and the skill index.
    pub registry: Arc<Registry>,
}

impl Runtime {
    /// Opens (and migrates) the database and the blob store under `root`, builds the registry
    /// from `config` with built-in providers reading attachments from the blob store, and logs
    /// what's registered and what's still pending. The data root's layout must exist.
    ///
    /// # Errors
    ///
    /// The first failure, as a [`StartError`].
    pub async fn start(root: DataRoot, config: Arc<Config>) -> Result<Self, StartError> {
        let path = root.database_file();
        let store = Store::open(path.clone())
            .await
            .map_err(|source| StartError::Store { path, source })?;
        let path = root.blobs_dir();
        let blobs = BlobStore::open(path.clone(), store.clone())
            .await
            .map_err(|source| StartError::Blobs { path, source })?;
        let media = Arc::new(BlobMedia::new(blobs.clone()));
        let registry = Registry::from_config(root.clone(), config.clone(), media)?;
        registry.log_summary();
        Ok(Self {
            root,
            config,
            store,
            blobs,
            registry: Arc::new(registry),
        })
    }
}
