//! Built-in model providers (PLAN section 4.2), each behind a Cargo feature, and the HTTP,
//! server-sent events, and wire-format plumbing they share.

use async_trait::async_trait;
use bytes::Bytes;
use giles_types::{MediaRef, Result};

use crate::config::ProviderInstance;

#[cfg(feature = "provider-anthropic")]
pub mod anthropic;
#[cfg(any(feature = "provider-anthropic", feature = "provider-openai"))]
pub mod http;
#[cfg(feature = "provider-openai")]
pub mod openai;
#[cfg(any(feature = "provider-anthropic", feature = "provider-openai"))]
pub mod sse;
#[cfg(any(feature = "provider-anthropic", feature = "provider-openai"))]
pub mod wire;

/// Where a provider gets the bytes of the attachments it sends inline (images, PDFs). In the
/// daemon this is the blob store (PLAN section 11.5).
///
/// It returns only bytes: the media type comes from the [`MediaRef`], which the core recorded
/// when it stored the blob. A provider calls it only for media types its model accepts, while
/// building a request, so an error fails that request before anything is sent.
#[async_trait]
pub trait MediaSource: Send + Sync {
    /// The content of `media`. [`giles_types::Error::NotFound`] if it no longer exists.
    async fn read(&self, media: &MediaRef) -> Result<Bytes>;
}

/// The context window `instance`'s configuration sets for `model`, in tokens. For when the
/// provider's model list doesn't report one: the server lists the model under another id (Ollama's
/// `:latest`), not at all, or listing failed. Only the OpenAI-compatible provider has the setting
/// (`models."<id>".context_window`, else `context_window`); listed models already carry it in
/// their `ModelInfo`.
#[cfg_attr(not(feature = "provider-openai"), allow(unused_variables))]
pub fn configured_context_window(instance: &ProviderInstance, model: &str) -> Option<u32> {
    match instance.component.as_str() {
        #[cfg(feature = "provider-openai")]
        "openai" => {
            openai::OpenAiConfig::from_json(&instance.config.to_json())
                .ok()?
                .settings(model)
                .context_window
        }
        _ => None,
    }
}
