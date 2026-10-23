//! Core tools (PLAN section 6.5): the tools the core implements itself, offered under bare,
//! reserved names ([`CORE_TOOLS`](crate::registry::CORE_TOOLS)).
//!
//! Each tool implements [`CoreTool`] and is registered with
//! [`Registry::register_core_tool`](crate::registry::Registry::register_core_tool), which also
//! adds its spec to the tool index. The dispatcher
//! ([`PolicyDispatcher`](crate::agent_loop::PolicyDispatcher)) runs a core-tool call like any
//! other: it must be offered by the session, it passes the approval gate, and it has the default
//! deadline. A tool keeps what it needs for its whole life (the store, the data root) in its own
//! fields; what it needs to know about the turn arrives in each [`CoreCall`]. [`register`] adds
//! every core tool at startup.

mod memory;
mod memory_search;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use async_trait::async_trait;
use giles_store::Session;
use giles_types::{CallContext, ToolSpec};
use serde_json::Value;

use crate::agent_loop::ToolOutput;
use crate::registry::RegistryError;
use crate::runtime::Runtime;

pub use memory::{MEMORY, MemoryTool};
pub use memory_search::{DEFAULT_LIMIT, MAX_LIMIT, MEMORY_SEARCH, MemorySearchTool, NO_MATCHES};

/// Registers every core tool with `runtime`'s registry, so sessions started from now on can
/// offer them (each agent's `tools` globs still decide).
///
/// # Errors
///
/// [`RegistryError::ToolName`] if a tool's name isn't reserved for core tools (a bug).
pub fn register(runtime: &Runtime) -> Result<(), RegistryError> {
    let registry = &runtime.registry;
    registry.register_core_tool(Arc::new(MemoryTool::new(
        runtime.root.clone(),
        runtime.config.clone(),
    )))?;
    registry.register_core_tool(Arc::new(MemorySearchTool::new(
        runtime.store.clone(),
        runtime.config.clone(),
    )))?;
    Ok(())
}

/// The turn a core-tool call belongs to, built once per turn and shared by its calls.
#[derive(Debug, Clone)]
pub struct TurnContext {
    /// The turn's session, as loaded when the turn started.
    pub session: Session,
}

/// One call of a core tool.
#[derive(Debug, Clone)]
pub struct CoreCall {
    /// The `ToolUse` id.
    pub id: String,
    /// The input the model gave.
    pub input: Value,
    /// The turn as toolsets see it: agent, requester, conversation.
    pub context: CallContext,
    /// The turn.
    pub turn: Arc<TurnContext>,
}

/// A tool the core implements.
#[async_trait]
pub trait CoreTool: Send + Sync {
    /// The spec offered to the model. Its name is one of
    /// [`CORE_TOOLS`](crate::registry::CORE_TOOLS).
    fn spec(&self) -> ToolSpec;

    /// Runs one call to its result. Failures are error results; dropping the future cancels the
    /// call.
    async fn call(&self, call: CoreCall) -> ToolOutput;
}
