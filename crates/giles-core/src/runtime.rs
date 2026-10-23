//! Startup wiring: everything `giles serve` builds from the configuration before it serves.

use std::path::PathBuf;
use std::sync::Arc;

use giles_store::{BlobStore, Store};
use tokio_util::sync::CancellationToken;

use crate::blobs::BlobMedia;
use crate::config::Config;
use crate::core_tools;
use crate::paths::DataRoot;
use crate::registry::{Registry, RegistryError};
use crate::router::Router;
use crate::sessions::{AgentPolicy, Approvals, Sessions, SessionsOptions, SystemClock, TurnRunner};

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
    /// Startup recovery of the sessions failed (PLAN section 6.3).
    #[error("cannot recover the sessions: {0}")]
    Sessions(#[source] giles_store::Error),
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
    /// from `config` with built-in providers reading attachments from the blob store, and
    /// registers the core tools. The data root's layout must exist. `serve` logs the registry's
    /// summary ([`Registry::log_summary`]) once it has registered the toolsets.
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
        let runtime = Self {
            root,
            config,
            store,
            blobs,
            registry: Arc::new(registry),
        };
        core_tools::register(&runtime)?;
        Ok(runtime)
    }

    /// Starts the session manager: runs startup recovery, resumes queued messages, and starts
    /// consuming the events of every channel registered so far. Turns run on `runner`; approval
    /// replies go to `approvals`. Channels registered later (plugins) are attached with
    /// [`Sessions::attach_channel`]. Everything stops when `shutdown` is cancelled.
    ///
    /// # Errors
    ///
    /// [`StartError::Sessions`] if recovery fails.
    pub async fn start_sessions(
        &self,
        runner: Arc<dyn TurnRunner>,
        approvals: Arc<dyn Approvals>,
        shutdown: CancellationToken,
    ) -> Result<Sessions, StartError> {
        let options = SessionsOptions {
            store: self.store.clone(),
            router: Router::new(&self.config.owner, &self.config.routes),
            agents: AgentPolicy::from_config(&self.config),
            channels: self.registry.clone(),
            runner,
            approvals,
            clock: Arc::new(SystemClock),
        };
        let sessions = Sessions::start(options, shutdown)
            .await
            .map_err(StartError::Sessions)?;
        for (name, channel) in self.registry.channels() {
            let component = self.config.channels[&name].component.clone();
            sessions.attach_channel(name, component, channel);
        }
        Ok(sessions)
    }
}
