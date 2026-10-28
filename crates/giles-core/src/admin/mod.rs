//! The admin service (PLAN section 8.7): `giles.admin.v1.AdminService` on `run/admin.sock`, for
//! the `giles` CLI and the web admin pages.
//!
//! - [`InstanceLock`]: `giles serve` takes an exclusive `flock` on `run/giles.lock` before
//!   anything else, so one core at a time uses a data root (and only then is an `admin.sock` left
//!   behind by a crash stale).
//! - [`AdminListener`] writes a fresh 256-bit token to `run/admin.token` (mode 0600), binds the
//!   socket (mode 0600), and serves an [`AdminService`]. Every call must carry the token as
//!   `authorization: Bearer <token>`, checked in constant time; anything else is
//!   `UNAUTHENTICATED`. File modes alone don't separate the core from its plugins, which run as
//!   the same user.
//! - [`connect`] is the client side: check the socket directory, connect, read the token.
//!
//! Other subsystems plug in through traits: [`PluginControl`] (plugin supervision) and
//! [`LoginLinks`] (implemented by the built-in web channel). Without them, the RPCs that need them
//! answer `UNAVAILABLE`. See `Docs/technical/admin.md`.

mod auth;
mod client;
mod lock;
mod plugins;
mod server;
mod service;
#[cfg(test)]
mod tests;

pub use self::auth::BearerToken;
pub use self::client::{AdminClient, ConnectError, connect};
pub use self::lock::{InstanceLock, LockError};
pub use self::plugins::{MergedControl, PluginControl, PluginRunState, PluginRuntime};
pub use self::server::{AdminListener, BindError, DRAIN_TIMEOUT};
pub use self::service::{
    AdminOptions, AdminService, ChannelStatuses, LoginLink, LoginLinkError, LoginLinks,
};
