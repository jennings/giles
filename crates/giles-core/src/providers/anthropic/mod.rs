//! The Anthropic Messages API provider. The mapping between the message model and the wire
//! format is described in `Docs/technical/providers.md`.

mod config;
mod models;
mod provider;
mod request;
mod stream;

pub use config::{AnthropicConfig, ApiKey, DEFAULT_ANTHROPIC_VERSION, DEFAULT_BASE_URL};
pub use models::MEDIA_TYPES;
pub use provider::AnthropicProvider;

/// The API family name in [`giles_types::Opaque::api`] for data this provider produces and
/// accepts back.
pub const API: &str = "anthropic";
