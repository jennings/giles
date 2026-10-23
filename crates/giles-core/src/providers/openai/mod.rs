//! The OpenAI-compatible Chat Completions provider, for OpenAI, OpenRouter, Ollama, and other
//! compatible servers. The mapping between the message model and the wire format is described in
//! `Docs/technical/providers.md`.

mod config;
mod models;
mod provider;
mod request;
mod stream;

pub use config::{DEFAULT_BASE_URL, DEFAULT_IMAGE_TYPES, Dialect, ModelSettings, OpenAiConfig};
pub use provider::OpenAiProvider;

/// The API family name in [`giles_types::Opaque::api`] for data this provider produces and
/// accepts back: the field a `Reasoning` block's text streamed in.
pub const API: &str = "openai";
