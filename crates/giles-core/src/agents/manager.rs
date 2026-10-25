//! [`AgentManager`]: what the web UI's agent pages call to change the agents of the running core
//! (TASKS.md 5.1, 5.4). Creating, editing, and removing an agent run under the manager's one lock,
//! so they are serialized with each other. What each does is in its own module:
//! [`create`](super::create), [`edit`](super::edit), [`remove`](super::remove).

use std::sync::Arc;
use std::time::Duration;

use tracing::debug;

use giles_store::Store;

use super::create::remove_stale_staging;
use super::remove::AgentActivity;
use crate::config::LiveConfig;
use crate::registry::Registry;
use crate::routes::Routes;
use crate::scheduler::JobChanges;

/// How long the form waits for each provider's model list.
const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(5);

/// What an [`AgentManager`] works with.
#[derive(Debug, Clone)]
pub struct AgentManagerOptions {
    /// The data root and the running configuration the agents are in.
    pub registry: Arc<Registry>,
    /// The database: the runtime jobs of a removed agent.
    pub store: Store,
    /// The routes, which keep an agent from being removed while they name it.
    pub routes: Arc<Routes>,
    /// Tells the scheduler that runtime jobs were deleted.
    pub jobs: JobChanges,
    /// Whether an agent is being worked on, which keeps it from being removed.
    pub activity: Arc<dyn AgentActivity>,
}

/// Manages the agents of the running core. Shared as `Arc<AgentManager>`.
#[derive(Debug)]
pub struct AgentManager {
    pub(super) registry: Arc<Registry>,
    pub(super) store: Store,
    pub(super) routes: Arc<Routes>,
    pub(super) jobs: JobChanges,
    pub(super) activity: Arc<dyn AgentActivity>,
    /// Held for a whole change, so changes to agents run one at a time.
    pub(super) lock: tokio::sync::Mutex<()>,
}

impl AgentManager {
    /// Manages agents under the registry's data root and its running configuration. `giles serve`
    /// builds one at startup, before it serves; building it removes the staging directories a
    /// creation cut off by a crash left in `agents/` (`.<agent>.creating/`). Blocking (a
    /// directory listing).
    pub fn new(options: AgentManagerOptions) -> Self {
        remove_stale_staging(&options.registry.root().agents_dir());
        Self {
            registry: options.registry,
            store: options.store,
            routes: options.routes,
            jobs: options.jobs,
            activity: options.activity,
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The running configuration the agents are in.
    pub fn config(&self) -> &LiveConfig {
        self.registry.live_config()
    }

    /// Every `<provider>/<model>` the registered providers list, sorted, for the form to offer.
    /// A provider whose listing fails or takes longer than 5 seconds offers nothing (logged at
    /// `debug`, with secrets scrubbed); its models can still be typed.
    pub async fn known_models(&self) -> Vec<String> {
        let config = self.registry.config();
        let listings = self.registry.providers().into_iter().map(|(name, provider)| {
            let config = config.clone();
            async move {
                match tokio::time::timeout(LIST_MODELS_TIMEOUT, provider.list_models()).await {
                    Ok(Ok(models)) => models
                        .into_iter()
                        .map(|model| format!("{name}/{}", model.id))
                        .collect(),
                    Ok(Err(error)) => {
                        let error = error.to_string();
                        let error = match config.providers.get(&name) {
                            Some(instance) => instance.config.scrub(&error),
                            None => error,
                        };
                        debug!(provider = %name, %error, "cannot list the provider's models for the agent form");
                        Vec::new()
                    }
                    Err(_) => {
                        debug!(provider = %name, "listing the provider's models for the agent form timed out");
                        Vec::new()
                    }
                }
            }
        });
        let mut models: Vec<String> = futures::future::join_all(listings)
            .await
            .into_iter()
            .flatten()
            .collect();
        models.sort();
        models.dedup();
        models
    }
}
