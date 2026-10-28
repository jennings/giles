//! Toolsets: MCP servers the core launches in their sandbox or reaches over HTTP (PLAN sections
//! 7.5, 7.6, 7.9, 7.10). See `Docs/technical/toolsets.md`.
//!
//! [`ToolsetLauncher`] makes one connection to one configured toolset. A stdio server's `command`
//! is resolved with the operator's `PATH`, its install prefix (and its script interpreter's) is
//! granted read and execute ([`grants`]), and it runs through the sandbox trampoline with a
//! per-instance spec in `run/sandbox/<instance>.json`, its data directory
//! `data/plugins/<instance>/`, network off unless granted, and only its configured `env` beyond
//! the sandbox's scrubbed environment.
//!
//! [`ToolsetHost`] registers every configured toolset at boot and supervises it ([`lifecycle`]):
//! starts, restarts with backoff, `idle_stop`, and the tool-list cache ([`cache`]). It also adds,
//! replaces, and removes one toolset while Giles runs.
//!
//! [`ToolsetManager`] ([`manager`]) keeps the toolsets stored in `giles.db` and changes them: it
//! validates a spec, stores it, and adds, replaces, renames, or removes the toolset in the live
//! configuration and the host.

mod cache;
mod grants;
mod host;
mod launch;
mod lifecycle;
pub mod manager;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod manager_tests;

pub use self::host::ToolsetHost;
pub use self::launch::{ResolvedProgram, ToolsetLauncher};
pub use self::lifecycle::{HEALTHY_RESET, RESTART_BACKOFF_MAX, RESTART_BACKOFF_MIN};
pub use self::manager::{
    AgentGlobChange, GlobChange, RESERVED_TOOLSET_NAME, Removed, RenamePreview, ToolsetManager,
    ToolsetManagerError, ToolsetManagerOptions, ToolsetRenamed, ToolsetSource, ToolsetView,
};
