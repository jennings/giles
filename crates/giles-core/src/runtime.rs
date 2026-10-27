//! Startup wiring: everything `giles serve` builds from the configuration before it serves.

use std::path::PathBuf;
use std::sync::Arc;

use giles_store::{BlobStore, JournalMode, Store};
use tokio_util::sync::CancellationToken;

use crate::backup;
use crate::blobs::{BlobMedia, BlobSweep, SweepSchedule};
use crate::config::Config;
use crate::core_tools;
use crate::memory_dbs::MemoryDbs;
use crate::outbox::Outbox;
use crate::paths::DataRoot;
use crate::registry::{Registry, RegistryError};
use crate::rename::{InstanceDataError, check_unused_instance_data};
use crate::router::Router;
use crate::routes::{Routes, RoutesError};
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
    /// SQLite wouldn't switch the database to the configured journal mode.
    #[error(
        "cannot open the database {}: {source}\n{}",
        .path.display(),
        journal_mode_advice(*.mode)
    )]
    JournalMode {
        /// `data/giles.db`.
        path: PathBuf,
        /// `[core] journal_mode`.
        mode: JournalMode,
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
    /// The routes couldn't be imported from `giles.toml` or loaded.
    #[error(transparent)]
    Routes(#[from] RoutesError),
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

/// How to get out of a journal mode SQLite refused, naming the config option.
fn journal_mode_advice(mode: JournalMode) -> String {
    if mode == JournalMode::Wal {
        "WAL needs shared memory, which network file systems and some container mounts lack. Set \
         `journal_mode = \"delete\"` in the `[core]` table of giles.toml, which works everywhere \
         (and move the database to a local disk if you can)."
            .to_owned()
    } else {
        format!(
            "Stop other programs that have the database open and start again, or change \
             `journal_mode` (currently \"{mode}\") in the `[core]` table of giles.toml."
        )
    }
}

/// The running core's shared state.
#[derive(Debug, Clone)]
pub struct Runtime {
    /// The data root.
    pub root: DataRoot,
    /// The database, `data/giles.db`.
    pub store: Store,
    /// The agents' memory databases, `agents/<agent>/memory.db`, opened on first use in the
    /// journal mode of `giles.db`.
    pub memory_dbs: MemoryDbs,
    /// The blob store, `data/blobs`.
    pub blobs: BlobStore,
    /// Configured instances and their objects, the tool index, the skill index, and the running
    /// configuration ([`Registry::config`]).
    pub registry: Arc<Registry>,
    /// The routing rules, stored in the database and changed through the web UI. The router
    /// reads them ([`Routes::list`]).
    pub routes: Arc<Routes>,
    /// Proactive sends to identities (PLAN section 6.11): job replies, approval prompts of job
    /// turns, login codes. Its sender starts with [`start_outbox`](Self::start_outbox).
    pub outbox: Outbox,
    /// Tells the scheduler that runtime jobs changed (the `schedule` tool).
    pub jobs: JobChanges,
}

impl Runtime {
    /// Opens (and migrates) the database and the blob store under `root`, builds the registry
    /// from `config` with built-in providers reading attachments from the blob store, imports
    /// `giles.toml`'s `[[routes]]` if that never happened and loads the routes ([`Routes::open`]),
    /// and registers the core tools. The data root's layout must exist. `serve` logs the registry's
    /// summary ([`Registry::log_summary`]) once it has registered the toolsets.
    ///
    /// # Errors
    ///
    /// The first failure, as a [`StartError`].
    pub async fn start(root: DataRoot, config: Arc<Config>) -> Result<Self, StartError> {
        let path = root.database_file();
        // An existing database is backed up to `backups/` before any migration runs.
        let mode = config.core.journal_mode;
        let options = backup::open_options(&root).journal_mode(mode);
        let store =
            Store::open_with(path.clone(), options)
                .await
                .map_err(|source| match source {
                    giles_store::Error::JournalMode { .. } => {
                        StartError::JournalMode { path, mode, source }
                    }
                    source => StartError::Store { path, source },
                })?;
        // Before anything else touches the data: a rename done in `giles.toml` alone would strand
        // the old name's rows and directories (PLAN section 7.2).
        check_unused_instance_data(&root, &config, &store).await?;
        let path = root.blobs_dir();
        let blobs = BlobStore::open(path.clone(), store.clone())
            .await
            .map_err(|source| StartError::Blobs { path, source })?;
        let media = Arc::new(BlobMedia::new(blobs.clone()));
        let registry = Arc::new(Registry::from_config(root.clone(), config.clone(), media)?);
        let routes = Arc::new(Routes::open(store.clone(), registry.live_config().clone()).await?);
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let runtime = Self {
            memory_dbs: MemoryDbs::new(root.clone(), mode),
            root,
            outbox: Outbox::new(store.clone(), registry.clone(), clock),
            store,
            blobs,
            registry,
            routes,
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
            router: Router::new(&config.owner, self.routes.list(), agents.clone()),
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    /// A data root whose `giles.toml` has `[core]` set to `core` and the minimal owner.
    fn data_root(core: &str) -> (tempfile::TempDir, DataRoot) {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        root.create_layout().unwrap();
        set_core(&root, core);
        (dir, root)
    }

    fn set_core(root: &DataRoot, core: &str) {
        std::fs::write(
            root.config_file(),
            format!("[core]\n{core}\n[owner]\nidentities = [\"web:owner\"]\n"),
        )
        .unwrap();
    }

    async fn start(root: &DataRoot) -> Result<Runtime, StartError> {
        Runtime::start(root.clone(), Arc::new(Config::load(root).unwrap())).await
    }

    /// The journal mode recorded in the database file: WAL is, the rollback modes read `delete`.
    fn recorded_mode(root: &DataRoot) -> String {
        rusqlite::Connection::open(root.database_file())
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap()
    }

    fn exists(root: &DataRoot, suffix: &str) -> bool {
        let mut name = root.database_file().into_os_string();
        name.push(suffix);
        Path::new(&name).exists()
    }

    #[tokio::test]
    async fn the_configured_journal_mode_reaches_the_database() {
        for (core, wal) in [
            ("", true),
            ("journal_mode = \"wal\"", true),
            ("journal_mode = \"delete\"", false),
            ("journal_mode = \"truncate\"", false),
            ("journal_mode = \"persist\"", false),
        ] {
            let (_dir, root) = data_root(core);
            let runtime = start(&root).await.unwrap();
            assert_eq!(exists(&root, "-wal"), wal, "{core}");
            drop(runtime);
            assert!(
                !exists(&root, "-wal") && !exists(&root, "-journal"),
                "{core}"
            );
            assert_eq!(
                recorded_mode(&root),
                if wal { "wal" } else { "delete" },
                "{core}"
            );
        }
    }

    #[tokio::test]
    async fn changing_the_option_switches_an_existing_database_at_the_next_start() {
        let (_dir, root) = data_root("");
        drop(start(&root).await.unwrap());
        assert_eq!(recorded_mode(&root), "wal");
        for (core, expected) in [
            ("journal_mode = \"delete\"", "delete"),
            ("", "wal"),
            ("journal_mode = \"persist\"", "delete"),
            ("journal_mode = \"wal\"", "wal"),
        ] {
            set_core(&root, core);
            drop(start(&root).await.unwrap());
            assert_eq!(recorded_mode(&root), expected, "{core}");
        }
    }

    #[tokio::test]
    async fn a_journal_mode_sqlite_refuses_is_reported_with_the_option_that_changes_it() {
        // A database file that can't be written can't be switched, which stands in for a file
        // system that can't run WAL.
        let refused = |from: &'static str, to: &'static str| async move {
            let (_dir, root) = data_root(from);
            drop(start(&root).await.unwrap());
            let file = root.database_file();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
            // Root ignores the permission bits, so there's nothing to refuse.
            if std::fs::OpenOptions::new().write(true).open(&file).is_ok() {
                return None;
            }
            set_core(&root, to);
            let err = start(&root).await.unwrap_err();
            Some(err)
        };

        if let Some(err) = refused("journal_mode = \"delete\"", "").await {
            assert!(matches!(err, StartError::JournalMode { .. }), "{err}");
            let message = err.to_string();
            assert!(message.contains("giles.db"), "{message}");
            assert!(message.contains("journal mode to wal"), "{message}");
            assert!(message.contains("[core]"), "{message}");
            assert!(message.contains("journal_mode = \"delete\""), "{message}");
        }
        if let Some(err) = refused("", "journal_mode = \"truncate\"").await {
            assert!(matches!(err, StartError::JournalMode { .. }), "{err}");
            let message = err.to_string();
            assert!(message.contains("journal mode to truncate"), "{message}");
            assert!(
                message.contains("`journal_mode` (currently \"truncate\")"),
                "{message}"
            );
            assert!(!message.contains("network"), "{message}");
        }
    }

    /// A file that isn't a database is the plain store error: the journal-mode advice would
    /// point at the wrong problem.
    #[tokio::test]
    async fn a_file_that_is_not_a_database_is_a_plain_store_error_whatever_the_mode() {
        for core in ["", "journal_mode = \"delete\""] {
            let (_dir, root) = data_root(core);
            std::fs::write(root.database_file(), "not a database".repeat(200)).unwrap();
            let err = start(&root).await.unwrap_err();
            assert!(matches!(err, StartError::Store { .. }), "{core}: {err:?}");
            let message = err.to_string();
            assert!(message.contains("not a database"), "{core}: {message}");
            assert!(!message.contains("journal_mode"), "{core}: {message}");
        }
    }
}
