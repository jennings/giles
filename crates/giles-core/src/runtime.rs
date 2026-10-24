//! Startup wiring: everything `giles serve` builds from the configuration before it serves.

use std::path::PathBuf;
use std::sync::Arc;

use giles_store::{BlobStore, Store};
use tokio_util::sync::CancellationToken;

use crate::backup;
use crate::blobs::{BlobMedia, BlobSweep, SweepSchedule};
use crate::config::Config;
use crate::core_tools;
use crate::outbox::Outbox;
use crate::paths::DataRoot;
use crate::registry::{Registry, RegistryError};
use crate::rename::{InstanceDataError, check_unused_instance_data};
use crate::router::Router;
use crate::scheduler::{JobChanges, OutboxDelivery, Scheduler, SchedulerOptions};
use crate::sessions::{Approvals, Clock, Sessions, SessionsOptions, SystemClock, TurnRunner};

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
    /// The outbox couldn't drop the approval prompts left from the previous process.
    #[error("cannot start the outbox: {0}")]
    Outbox(#[source] giles_store::Error),
    /// The configuration looks like a rename that was never carried out (PLAN section 7.2).
    #[error(transparent)]
    InstanceData(#[from] InstanceDataError),
}

/// The running core's shared state.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// The data root.
    pub root: DataRoot,
    /// The database, `data/giles.db`.
    pub store: Store,
    /// The blob store, `data/blobs`.
    pub blobs: BlobStore,
    /// Configured instances and their objects, the tool index, the skill index, and the running
    /// configuration ([`Registry::config`]).
    pub registry: Arc<Registry>,
    /// Proactive sends to identities (PLAN section 6.11): job replies, approval prompts of job
    /// turns, login codes. Its sender starts with [`start_outbox`](Self::start_outbox).
    pub outbox: Outbox,
    /// Tells the scheduler that runtime jobs changed (the `schedule` tool).
    pub jobs: JobChanges,
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
        // An existing database is backed up to `backups/` before any migration runs.
        let store = Store::open_with(path.clone(), backup::open_options(&root))
            .await
            .map_err(|source| StartError::Store { path, source })?;
        // Before anything else touches the data: a rename done in `giles.toml` alone would strand
        // the old name's rows and directories (PLAN section 7.2).
        check_unused_instance_data(&root, &config, &store).await?;
        let path = root.blobs_dir();
        let blobs = BlobStore::open(path.clone(), store.clone())
            .await
            .map_err(|source| StartError::Blobs { path, source })?;
        let media = Arc::new(BlobMedia::new(blobs.clone()));
        let registry = Arc::new(Registry::from_config(root.clone(), config.clone(), media)?);
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let runtime = Self {
            root,
            outbox: Outbox::new(store.clone(), registry.clone(), clock),
            store,
            blobs,
            registry,
            jobs: JobChanges::default(),
        };
        core_tools::register(&runtime)?;
        Ok(runtime)
    }

    /// Starts the outbox's sender (PLAN section 6.11): drops the approval prompts left from the
    /// previous process, then sends due rows through the registered channels until `shutdown` is
    /// cancelled. Rows for channels that aren't up yet (plugins) wait and are retried. Wait for
    /// it to stop with [`Outbox::closed`].
    ///
    /// # Errors
    ///
    /// [`StartError::Outbox`] on a database error.
    pub async fn start_outbox(&self, shutdown: CancellationToken) -> Result<(), StartError> {
        self.outbox
            .start(shutdown)
            .await
            .map_err(StartError::Outbox)
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
        let config = self.registry.config();
        let agents = Arc::new(self.registry.live_config().clone());
        let options = SessionsOptions {
            store: self.store.clone(),
            router: Router::new(&config.owner, &config.routes, agents.clone()),
            agents,
            channels: self.registry.clone(),
            runner,
            approvals,
            clock: Arc::new(SystemClock),
        };
        let sessions = Sessions::start(options, shutdown)
            .await
            .map_err(StartError::Sessions)?;
        for (name, channel) in self.registry.channels() {
            let component = config.channels[&name].component.clone();
            sessions.attach_channel(name, component, channel);
        }
        Ok(sessions)
    }

    /// Starts the periodic blob sweep (PLAN section 11.5): the first run a minute after this call,
    /// then every six hours ([`SweepSchedule`]), until `shutdown` is cancelled
    /// ([`BlobSweep::closed`] waits for it).
    pub fn start_blob_sweep(&self, shutdown: CancellationToken) -> BlobSweep {
        BlobSweep::start(
            self.blobs.clone(),
            Arc::new(SystemClock),
            SweepSchedule::default(),
            shutdown,
        )
    }

    /// Starts the scheduler (PLAN section 6.11): config and runtime jobs, running their turns on
    /// `runner` through `sessions` and sending their replies and approval prompts through the
    /// outbox to the owner's default identity. Start it after the outbox and the session
    /// manager; it stops when `shutdown` is cancelled ([`Scheduler::closed`] waits for it).
    pub fn start_scheduler(
        &self,
        sessions: Sessions,
        runner: Arc<dyn TurnRunner>,
        shutdown: CancellationToken,
    ) -> Scheduler {
        Scheduler::start(
            SchedulerOptions {
                store: self.store.clone(),
                registry: self.registry.clone(),
                sessions,
                runner,
                delivery: Arc::new(OutboxDelivery::new(
                    self.outbox.clone(),
                    self.registry.config().owner.clone(),
                )),
                changes: self.jobs.clone(),
                clock: Arc::new(SystemClock),
            },
            shutdown,
        )
    }
}
