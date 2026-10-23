//! Toolsets: MCP servers the core launches in their sandbox or reaches over HTTP (PLAN section
//! 7.10). See `Docs/technical/toolsets.md`.
//!
//! [`ToolsetLauncher`] makes one connection to one configured toolset. A stdio server's `command`
//! is resolved with the operator's `PATH`, its install prefix (and its script interpreter's) is
//! granted read and execute ([`grants`]), and it runs through the sandbox trampoline with a
//! per-instance spec in `run/sandbox/<instance>.json`, its data directory
//! `data/plugins/<instance>/`, network off unless granted, and only its configured `env` beyond
//! the sandbox's scrubbed environment.

mod grants;
mod launch;

pub use self::launch::{ResolvedProgram, ToolsetLauncher};
