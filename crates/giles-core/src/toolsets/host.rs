//! Every configured toolset, started at boot, changed while Giles runs, and stopped at shutdown.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::FutureExt as _;
use giles_types::{Error, Result};
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, debug, info, warn};

use super::cache::{ToolCache, file_digest};
use super::launch::ToolsetLauncher;
use super::lifecycle::{Connection, Connector, Control, Managed, Report};
use crate::admin::{PluginControl, PluginRuntime};
use crate::config::{InstanceName, Kind, ToolsetInstance, ToolsetTransport};
use crate::registry::{Registry, RegistryError};

/// The configured toolsets of a running core (PLAN section 7.5): each is registered at boot
/// (offering its cached tools) and supervised, from its first start until it's replaced or
/// removed ([`replace`](Self::replace), [`remove`](Self::remove)) or Giles shuts down. Toolsets
/// can be [added](Self::add) while Giles runs.
#[derive(Debug)]
pub struct ToolsetHost {
    shutdown: CancellationToken,
    tasks: Mutex<JoinSet<()>>,
    started: Mutex<Vec<oneshot::Receiver<()>>>,
    controls: Mutex<BTreeMap<InstanceName, Control>>,
    dynamic: Dynamic,
}

/// What changing a toolset after boot needs, and how to stop each supervisor.
struct Dynamic {
    registry: Arc<Registry>,
    connector: Arc<dyn Connector>,
    cache: ToolCache,
    /// The supervisors, by toolset name. Held for the whole of a change, so changes to the host
    /// can't interleave.
    supervised: AsyncMutex<BTreeMap<InstanceName, Supervised>>,
}

impl fmt::Debug for Dynamic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dynamic").finish_non_exhaustive()
    }
}

/// One toolset's supervisor, which can be stopped and waited for on its own.
struct Supervised {
    stop: CancellationToken,
    ended: oneshot::Receiver<()>,
}

impl Supervised {
    /// Stops the supervisor, which drops its connection (a stdio server's process group is
    /// killed) and fails the calls running on it as unavailable, and waits until it has ended.
    async fn stop(self) {
        self.stop.cancel();
        // The sender is dropped when the supervisor's task ends, whether or not it sent.
        let _ = self.ended.await;
    }
}

impl ToolsetHost {
    /// Registers every toolset in `registry`'s configuration and starts it with `launcher`:
    /// stdio servers in their sandbox, remote servers over HTTP. Each instance offers its cached
    /// tools (if the cache matches its program and configuration) from the moment it's
    /// registered; [`ToolsetHost::started`] waits for the first starts.
    pub async fn start(launcher: ToolsetLauncher, registry: &Arc<Registry>) -> Self {
        let cache = ToolCache::new(launcher.root().cache_dir().join("toolsets"));
        Self::start_with(Arc::new(LaunchConnector { launcher }), registry, cache).await
    }

    pub(crate) async fn start_with(
        connector: Arc<dyn Connector>,
        registry: &Arc<Registry>,
        cache: ToolCache,
    ) -> Self {
        let host = Self {
            shutdown: CancellationToken::new(),
            tasks: Mutex::new(JoinSet::new()),
            started: Mutex::new(Vec::new()),
            controls: Mutex::new(BTreeMap::new()),
            dynamic: Dynamic {
                registry: registry.clone(),
                connector,
                cache,
                supervised: AsyncMutex::default(),
            },
        };
        {
            let mut supervised = host.dynamic.supervised.lock().await;
            for instance in registry.config().toolsets.values() {
                match host.launch(&mut supervised, instance).await {
                    Ok(started) => lock(&host.started).push(started),
                    Err(error) => {
                        warn!(toolset = %instance.name, %error, "cannot register the toolset");
                    }
                }
            }
        }
        host
    }

    /// Registers `instance`, offering its cached tools, and spawns its supervisor. A supervisor
    /// still running under the same name is stopped first (and, if `instance` can't be
    /// registered, its toolset is unregistered). The receiver resolves once the first start has
    /// succeeded or failed.
    async fn launch(
        &self,
        supervised: &mut BTreeMap<InstanceName, Supervised>,
        instance: &ToolsetInstance,
    ) -> Result<oneshot::Receiver<()>, RegistryError> {
        let stopped_old = match supervised.remove(&instance.name) {
            Some(old) => {
                old.stop().await;
                true
            }
            None => false,
        };
        let Dynamic {
            registry,
            connector,
            cache,
            ..
        } = &self.dynamic;
        let cached = cached_tools(&**connector, cache, instance).await;
        let stop = self.shutdown.child_token();
        let managed = Managed::new(
            instance.clone(),
            cached,
            connector.clone(),
            cache.clone(),
            stop.clone(),
        );
        let registered = registry
            .register_toolset(&instance.name, managed.toolset)
            .await;
        if let Err(error) = registered {
            if stopped_old {
                // The old supervisor is gone, so the old toolset must not stay offered.
                lock(&self.controls).remove(&instance.name);
                registry.unregister_toolset(&instance.name);
            }
            return Err(error);
        }
        lock(&self.controls).insert(instance.name.clone(), managed.control);
        let (started_tx, started_rx) = oneshot::channel();
        let (ended_tx, ended) = oneshot::channel();
        let span = tracing::info_span!("toolset", instance = %instance.name);
        let supervisor = managed.supervisor.run(started_tx).instrument(span);
        {
            let mut tasks = lock(&self.tasks);
            // Reaps the supervisors of toolsets replaced or removed since.
            while tasks.try_join_next().is_some() {}
            tasks.spawn(async move {
                supervisor.await;
                let _ = ended_tx.send(());
            });
        }
        supervised.insert(instance.name.clone(), Supervised { stop, ended });
        Ok(started_rx)
    }

    /// Waits until every toolset's first start has succeeded or failed (a failed one keeps
    /// retrying in the background).
    pub async fn started(&self) {
        let started = std::mem::take(&mut *lock(&self.started));
        futures::future::join_all(started).await;
    }

    /// Stops every toolset: stdio servers are killed with their process groups, and remote
    /// connections closed. Calls still running fail. A change in progress finishes first, so none
    /// starts a supervisor that this doesn't wait for, and a change after this does nothing.
    pub async fn stop(&self) {
        let _changes = self.dynamic.supervised.lock().await;
        self.shutdown.cancel();
        let mut tasks = std::mem::take(&mut *lock(&self.tasks));
        while tasks.join_next().await.is_some() {}
    }

    /// Adds a toolset while Giles runs: looks up its cached tools, registers it, and starts it in
    /// the background. New sessions get its tools as soon as they're known. `instance` must be
    /// in the registry's live configuration already
    /// ([`LiveConfig::add_toolset`](crate::config::LiveConfig::add_toolset)). A toolset still
    /// running under the same name is stopped first, as [`replace`](Self::replace) does. Does
    /// nothing once the host has been [stopped](Self::stop).
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotConfigured`] if `instance` isn't in the live configuration; nothing
    /// starts.
    pub async fn add(&self, instance: &ToolsetInstance) -> Result<(), RegistryError> {
        let mut supervised = self.dynamic.supervised.lock().await;
        if self.shutdown.is_cancelled() {
            return Ok(());
        }
        self.launch(&mut supervised, instance).await?;
        info!(toolset = %instance.name, "toolset added");
        Ok(())
    }

    /// Replaces a running toolset with `instance`, which has the same name: stops the old
    /// supervisor (its connection is dropped, a stdio server's process group is killed, and
    /// calls running on it fail as unavailable), then adds the new one as [`add`](Self::add)
    /// does. Its `config_hash` differs from the old one's, so the tool-list cache misses and the
    /// new toolset offers only what its own server reports. `instance` must be in the registry's
    /// live configuration already
    /// ([`LiveConfig::replace_toolset`](crate::config::LiveConfig::replace_toolset)).
    ///
    /// # Errors
    ///
    /// [`RegistryError::NotConfigured`] if `instance` isn't in the live configuration. The old
    /// supervisor has been stopped by then, so the old toolset is unregistered too.
    pub async fn replace(&self, instance: &ToolsetInstance) -> Result<(), RegistryError> {
        let mut supervised = self.dynamic.supervised.lock().await;
        if self.shutdown.is_cancelled() {
            return Ok(());
        }
        self.launch(&mut supervised, instance).await?;
        info!(toolset = %instance.name, "toolset replaced");
        Ok(())
    }

    /// Removes a toolset while Giles runs: stops its supervisor (as [`replace`](Self::replace)
    /// does), then unregisters it, which withdraws its tools from new sessions. A call running
    /// on it fails as unavailable, and so does a call to its tool from a session that still
    /// lists it. The caller takes it out of the live configuration
    /// ([`LiveConfig::remove_toolset`](crate::config::LiveConfig::remove_toolset)).
    pub async fn remove(&self, name: &InstanceName) {
        let mut supervised = self.dynamic.supervised.lock().await;
        if let Some(old) = supervised.remove(name) {
            old.stop().await;
        }
        lock(&self.controls).remove(name);
        self.dynamic.registry.unregister_toolset(name);
        info!(toolset = %name, "toolset removed");
    }
}

/// What the admin service's `Status`, `ListPlugins`, and `RestartPlugin` see of the toolsets:
/// each instance's supervisor state, and a restart that drops its connection and starts again at
/// once (a failed toolset gets a new try too).
impl PluginControl for ToolsetHost {
    fn plugins(&self) -> Vec<PluginRuntime> {
        lock(&self.controls)
            .iter()
            .map(|(instance, control)| plugin_runtime(instance, control.report()))
            .collect()
    }

    fn restart(&self, kind: Kind, instance: &str) -> bool {
        if kind != Kind::Toolset {
            return false;
        }
        match lock(&self.controls).get(instance) {
            Some(control) => {
                control.request_restart();
                true
            }
            None => false,
        }
    }
}

/// A supervisor's report as the admin service shows it. A stdio server's `pid` stays unset: it
/// runs under the sandbox trampoline, so the process the core spawned isn't the server.
fn plugin_runtime(instance: &InstanceName, report: Report) -> PluginRuntime {
    PluginRuntime {
        kind: Kind::Toolset,
        instance: instance.clone(),
        binary: report.binary,
        state: report.state,
        restarts: report.restarts,
        last_error: report.last_error,
        version: report.version,
        requested_grants: None,
        tool_count: Some(u32::try_from(report.tools).unwrap_or(u32::MAX)),
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
        let (toolset, source, binary) = match &instance.transport {
            ToolsetTransport::Stdio(server) => {
                // The digest of exactly the program launched.
                let resolved = self.launcher.resolve(&instance.name, server)?;
                let source = program_digest(&instance.name, resolved.program.clone()).await?;
                let toolset = self
                    .launcher
                    .launch(&instance.name, server, &resolved)
                    .await?;
                (toolset, source, Some(resolved.program))
            }
            ToolsetTransport::Http(remote) => (
                self.launcher.connect(instance).await?,
                remote.url.to_string(),
                None,
            ),
        };
        let server_version = toolset.server_version();
        let toolset = Arc::new(toolset);
        let watched = toolset.clone();
        Ok(Connection {
            toolset,
            source,
            server_version,
            binary,
            closed: async move { watched.closed().await }.boxed(),
        })
    }
}
