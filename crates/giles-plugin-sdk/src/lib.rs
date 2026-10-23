//! SDK for Giles plugins: serves a component trait implementation as an out-of-process gRPC
//! plugin.
//!
//! A Giles channel plugin is a binary named `giles-channel-<name>` whose `main` hands a
//! [`giles_types::Channel`] implementation to [`ChannelPlugin::serve`]. The SDK does the plugin
//! side of the protocol (PLAN sections 7.4 and 8):
//!
//! - the launch handshake: the magic cookie, the protocol choice, gRPC on the socket inherited as
//!   fd 3, the per-launch token from stdin checked on every call, and the handshake line;
//! - `giles.plugin.v1.PluginService` (`Describe`, `Configure`, `Shutdown`, blob transfer),
//!   `grpc.health.v1.Health`, and `giles.channel.v1.ChannelService`, all with 16 MiB message
//!   limits;
//! - at-least-once event delivery: every event from [`Channel::events`] is written to the data
//!   directory before it's sent, and sent again until the core acks it, across restarts;
//! - blobs: attachments in both directions through [`Blobs`];
//! - exiting when the core goes away (stdin closes) or asks it to (`Shutdown`).
//!
//! With the `tls` feature (on by default), the `tls` module gives the plugin's own connections
//! `rustls` with the bundled Mozilla roots.
//!
//! See [`ChannelPlugin`] for a complete example. The user guide's "Writing a Plugin" page and
//! `Docs/technical/plugin-sdk.md` in the repository describe the behavior in detail.
//!
//! [`Channel::events`]: giles_types::Channel::events

mod auth;
mod base;
mod blobs;
mod channel;
pub mod convert;
mod dirs;
pub mod render;
mod server;
mod spool;
mod startup;
#[cfg(test)]
mod test_support;
#[cfg(feature = "tls")]
pub mod tls;

pub use async_trait::async_trait;
pub use base::Grants;
pub use blobs::Blobs;
pub use channel::{ChannelPlugin, Configure};
pub use giles_types;
