//! Every configured toolset, started at boot and stopped at shutdown.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::FutureExt as _;
use giles_types::{Error, Result};
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, debug, info, warn};

use super::cache::{ToolCache, file_digest};
use super::launch::ToolsetLauncher;
use super::lifecycle::{Connection, Connector, Managed};
use crate::config::{InstanceName, ToolsetInstance, ToolsetTransport};
use crate::registry::Registry;

/// The configured toolsets of a running core (PLAN section 7.5): each is registered at boot
/// (offering its cached tools) and supervised, from its first start to shutdown.
#[derive(Debug)]
pub struct ToolsetHost {
    shutdown: CancellationToken,
    tasks: Mutex<JoinSet<()>>,
    started: Mutex<Vec<oneshot::Receiver<()>>>,
}

impl ToolsetHost {
    /// Registers every toolset in `registry`'s configuration and starts it with `launcher`:
    /// stdio servers in their sandbox, remote servers over HTTP. Each instance offers its cached
    /// tools (if the cache matches its program and configuration) from the moment it's
    /// registered; [`ToolsetHost::started`] waits for the first starts.
    pub async fn start(launcher: ToolsetLauncher, registry: &Registry) -> Self {
        let cache = ToolCache::new(launcher.root().cache_dir().join("toolsets"));
        Self::start_with(Arc::new(LaunchConnector { launcher }), registry, cache).await
    }

    pub(crate) async fn start_with(
        connector: Arc<dyn Connector>,
        registry: &Registry,
        cache: ToolCache,
    ) -> Self {
        let shutdown = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let mut started = Vec::new();
        for instance in registry.config().toolsets.values() {
            let cached = cached_tools(&*connector, &cache, instance).await;
            let managed = Managed::new(
                instance.clone(),
                cached,
                connector.clone(),
                cache.clone(),
                shutdown.clone(),
            );
            if let Err(error) = registry
                .register_toolset(&instance.name, managed.toolset)
                .await
            {
                warn!(toolset = %instance.name, %error, "cannot register the toolset");
                continue;
            }
            let (started_tx, started_rx) = oneshot::channel();
            let span = tracing::info_span!("toolset", instance = %instance.name);
            tasks.spawn(managed.supervisor.run(started_tx).instrument(span));
            started.push(started_rx);
        }
        Self {
            shutdown,
            tasks: Mutex::new(tasks),
            started: Mutex::new(started),
        }
    }

    /// Waits until every toolset's first start has succeeded or failed (a failed one keeps
    /// retrying in the background).
    pub async fn started(&self) {
        let started = std::mem::take(&mut *lock(&self.started));
        futures::future::join_all(started).await;
    }

    /// Stops every toolset: stdio servers are killed with their process groups, and remote
    /// connections closed. Calls still running fail.
    pub async fn stop(&self) {
        self.shutdown.cancel();
        let mut tasks = std::mem::take(&mut *lock(&self.tasks));
        while tasks.join_next().await.is_some() {}
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The cached tools a stdio instance offers before its server first answers. A remote server's
/// tools are offered only while it's reachable (PLAN section 7.6), so it starts with none.
async fn cached_tools(
    connector: &dyn Connector,
    cache: &ToolCache,
    instance: &ToolsetInstance,
) -> Vec<giles_types::ToolSpec> {
    if !matches!(instance.transport, ToolsetTransport::Stdio(_)) {
        return Vec::new();
    }
    let source = match connector.source(instance).await {
        Ok(source) => source,
        Err(error) => {
            debug!(toolset = %instance.name, %error, "no cache lookup");
            return Vec::new();
        }
    };
    match cache.lookup(&instance.name, &source, &instance.config_hash) {
        Some(tools) => {
            info!(toolset = %instance.name, tools = tools.len(), "offering the cached tools");
            tools
        }
        None => Vec::new(),
    }
}

/// Connections through a [`ToolsetLauncher`].
struct LaunchConnector {
    launcher: ToolsetLauncher,
}

/// `sha256:<hex>` of a stdio server's program, off the async threads.
async fn program_digest(instance: &InstanceName, program: std::path::PathBuf) -> Result<String> {
    let digest = tokio::task::spawn_blocking(move || {
        file_digest(&program).map_err(|error| (program, error))
    })
    .await
    .map_err(Error::other)?;
    digest.map_err(|(program, error)| {
        Error::unavailable(format!(
            "toolset {instance}: cannot read {}: {error}",
            program.display()
        ))
    })
}

#[async_trait]
impl Connector for LaunchConnector {
    async fn source(&self, instance: &ToolsetInstance) -> Result<String> {
        match &instance.transport {
            ToolsetTransport::Stdio(server) => {
                let resolved = self.launcher.resolve(&instance.name, server)?;
                program_digest(&instance.name, resolved.program).await
            }
            ToolsetTransport::Http(remote) => Ok(remote.url.to_string()),
        }
    }

    async fn connect(&self, instance: &ToolsetInstance) -> Result<Connection> {
        let (toolset, source) = match &instance.transport {
            ToolsetTransport::Stdio(server) => {
                // The digest of exactly the program launched.
                let resolved = self.launcher.resolve(&instance.name, server)?;
                let source = program_digest(&instance.name, resolved.program.clone()).await?;
                let toolset = self
                    .launcher
                    .launch(&instance.name, server, &resolved)
                    .await?;
                (toolset, source)
            }
            ToolsetTransport::Http(remote) => (
                self.launcher.connect(instance).await?,
                remote.url.to_string(),
            ),
        };
        let server_version = toolset.server_version();
        let toolset = Arc::new(toolset);
        let watched = toolset.clone();
        Ok(Connection {
            toolset,
            source,
            server_version,
            closed: async move { watched.closed().await }.boxed(),
        })
    }
}
