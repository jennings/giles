//! SDK for Giles plugins: serves a component trait implementation as an out-of-process gRPC
//! plugin.
//!
//! A Giles channel plugin is a binary named `giles-channel-<name>` whose `main` hands a
//! [`giles_types::Channel`] implementation to [`ChannelPlugin::run`]. An environment plugin is
//! `giles-environment-<name>` and hands a [`giles_types::Environment`] to
//! [`EnvironmentPlugin::run`]. The binary then has two subcommands: `socket`, which is how Giles
//! launches it, and `serve`, which runs it as a remote plugin for an operator. The SDK does the
//! plugin side of the protocol (PLAN sections 7.4 and 8):
//!
//! - the launch handshake: the magic cookie, the protocol choice, gRPC on the socket inherited as
//!   fd 3, the per-launch token from stdin checked on every call, and the handshake line;
//! - `giles.plugin.v1.PluginService` (`Describe`, `Configure`, `Shutdown`, blob transfer),
//!   `grpc.health.v1.Health`, and the kind's service (`giles.channel.v1.ChannelService` or
//!   `giles.environment.v1.EnvironmentService`), all with 16 MiB message limits;
//! - at-least-once event delivery: every event from [`Channel::events`] is written to the data
//!   directory before it's sent, and sent again until the core acks it, across restarts;
//! - blobs: attachments in both directions through [`Blobs`];
//! - exiting when the core goes away (stdin closes) or asks it to (`Shutdown`).
//!
//! The same plugin can instead run as a remote plugin (PLAN section 7.6) that its operator starts
//! and the core connects to over TCP: the `serve` subcommand of [`ChannelPlugin::run`], or, for a
//! plugin with its own command line, [`ChannelPlugin::serve_remote`] with a [`RemoteServer`]
//! (optionally TLS, and a [`RemoteAuth`] check of the core's token). [`ChannelPlugin::serve`] is
//! the launched half alone. [`EnvironmentPlugin`] has the same three entry points; a remote
//! environment plugin takes its own settings from the `[config]` table of its `serve --config`
//! file.
//!
//! With the `tls` feature (on by default), the `tls` module gives the plugin's own connections
//! `rustls` with the bundled Mozilla roots, and a remote plugin its TLS server config. The
//! `config` module parses durations and sizes in a plugin's `config` table the way `giles.toml`
//! writes them.
//!
//! See [`ChannelPlugin`] and [`EnvironmentPlugin`] for complete examples. The user guide's
//! "Writing a plugin" page and `Docs/technical/plugin-sdk.md` in the repository describe the
//! behavior in detail.
//!
//! [`Channel::events`]: giles_types::Channel::events

mod auth;
mod base;
mod blobs;
mod channel;
mod cli;
pub mod config;
mod configure;
pub mod convert;
mod dirs;
mod environment;
mod remote;
pub mod render;
mod run;
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
pub use channel::ChannelPlugin;
pub use configure::Configure;
pub use environment::EnvironmentPlugin;
pub use giles_types;
pub use remote::{RemoteAuth, RemoteServer};
