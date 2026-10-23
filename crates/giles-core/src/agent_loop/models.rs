//! What the agent loop knows about each model it calls: the provider's [`ModelInfo`], listed
//! once and kept for the daemon's life. The agent loop uses it to send a model only the media
//! types it accepts (PLAN section 5) and for its context window, which decides when a session
//! is compacted ([`super::compaction`]).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use giles_types::ModelInfo;
use tracing::{info, warn};

use super::lock;
use crate::config::{Config, InstanceName};
use crate::registry::ResolvedModel;

/// How long listing a provider's models may take before the turn goes on without the answer.
pub const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(10);

/// How long after a failed listing the next turn tries again; until then the model is unknown.
pub const LIST_MODELS_RETRY: Duration = Duration::from_secs(300);

/// What's known about one model.
#[derive(Debug, Clone)]
enum Known {
    /// The provider listed it (`Some`), or listed its models without it (`None`).
    Listed(Option<Arc<ModelInfo>>),
    /// Listing failed; try again after this.
    Failed(Instant),
}

/// The [`ModelInfo`] of each model, by provider instance and model id.
#[derive(Debug, Default)]
pub(super) struct ModelInfos {
    known: Mutex<HashMap<(InstanceName, String), Known>>,
}

impl ModelInfos {
    /// `model`'s info from its provider's model list, listed on first use. `None` when the
    /// provider doesn't list the model (e.g. an alias of a listed id), or listing failed or took
    /// longer than [`LIST_MODELS_TIMEOUT`] (logged; retried after [`LIST_MODELS_RETRY`]). Then
    /// the provider's own media rules alone decide what it sends, and compaction takes the
    /// context window from the provider instance's configuration, else assumes
    /// [`DEFAULT_CONTEXT_WINDOW`](super::compaction::DEFAULT_CONTEXT_WINDOW) (128,000 tokens).
    pub(super) async fn get(
        &self,
        model: &ResolvedModel,
        config: &Config,
    ) -> Option<Arc<ModelInfo>> {
        let key = (model.instance.clone(), model.model.clone());
        match lock(&self.known).get(&key) {
            Some(Known::Listed(info)) => return info.clone(),
            Some(Known::Failed(retry)) if Instant::now() < *retry => return None,
            _ => {}
        }
        let listed = tokio::time::timeout(LIST_MODELS_TIMEOUT, model.provider.list_models()).await;
        let known = match listed {
            Ok(Ok(models)) => {
                let info = models
                    .into_iter()
                    .find(|listed| listed.id == model.model)
                    .map(Arc::new);
                if info.is_none() {
                    info!(provider = %model.instance, model = %model.model, "the provider doesn't list the model; it decides which media to send");
                }
                Known::Listed(info)
            }
            Ok(Err(error)) => {
                let error = error.to_string();
                let error = match config.providers.get(&model.instance) {
                    Some(instance) => instance.config.scrub(&error),
                    None => error,
                };
                warn!(provider = %model.instance, model = %model.model, %error, "cannot list the provider's models");
                Known::Failed(Instant::now() + LIST_MODELS_RETRY)
            }
            Err(_) => {
                warn!(provider = %model.instance, model = %model.model, "listing the provider's models timed out");
                Known::Failed(Instant::now() + LIST_MODELS_RETRY)
            }
        };
        let info = match &known {
            Known::Listed(info) => info.clone(),
            Known::Failed(_) => None,
        };
        lock(&self.known).insert(key, known);
        info
    }
}
