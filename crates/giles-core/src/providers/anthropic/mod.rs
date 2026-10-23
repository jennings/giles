//! The Anthropic Messages API provider. The mapping between the message model and the wire
//! format is described in `Docs/technical/providers.md`.

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by AnthropicProvider, added with streaming")
)]
mod config;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by AnthropicProvider, added with streaming")
)]
mod models;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by AnthropicProvider, added with streaming")
)]
mod request;

pub use config::{AnthropicConfig, ApiKey, DEFAULT_ANTHROPIC_VERSION, DEFAULT_BASE_URL};
pub use models::MEDIA_TYPES;

/// The API family name in [`giles_types::Opaque::api`] for data this provider produces and
/// accepts back.
pub const API: &str = "anthropic";
