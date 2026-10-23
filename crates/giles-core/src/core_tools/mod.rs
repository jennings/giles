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

mod load_skill;
mod memory;
mod memory_search;
mod schedule;
mod send_attachment;

#[cfg(test)]
mod media_tests;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use async_trait::async_trait;
use giles_store::Session;
use giles_types::{CallContext, ToolSpec};
use serde_json::Value;

use crate::agent_loop::ToolOutput;
use crate::media::SessionMedia;
use crate::registry::RegistryError;
use crate::runtime::Runtime;
use crate::sessions::{Reply, SystemClock};
use crate::skills::SkillSet;

pub use load_skill::{LOAD_SKILL, LoadSkillTool, MAX_INLINE_TEXT_BYTES};
pub use memory::{MEMORY, MemoryTool};
pub use memory_search::{DEFAULT_LIMIT, MAX_LIMIT, MEMORY_SEARCH, MemorySearchTool, NO_MATCHES};
pub use schedule::{
    MAX_LISTED_PROMPT_CHARS, MAX_RUNTIME_JOBS_PER_USER, Request, SCHEDULE, ScheduleTool,
};
pub use send_attachment::{SEND_ATTACHMENT, SendAttachmentTool};

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
    registry.register_core_tool(Arc::new(LoadSkillTool::new(runtime.blobs.clone())))?;
    registry.register_core_tool(Arc::new(SendAttachmentTool::new(runtime.blobs.clone())))?;
    registry.register_core_tool(Arc::new(ScheduleTool::new(
        runtime.store.clone(),
        runtime.config.clone(),
        runtime.jobs.clone(),
        Arc::new(SystemClock),
    )))?;
    Ok(())
}

/// The turn a core-tool call belongs to, built once per turn and shared by its calls.
#[derive(Debug, Clone)]
pub struct TurnContext {
    /// The turn's session, as loaded when the turn started.
    pub session: Session,
    /// The session's conversation, where `send_attachment` sends; `None` for an isolated job run.
    pub reply: Option<Reply>,
    /// The skills the session's prompt lists, which `load_skill` answers from.
    pub skills: Arc<SkillSet>,
    /// Whether the turn is a job run (in a job session or a DM); `schedule` refuses to create
    /// jobs from one, so a job can't spawn more jobs.
    pub job: bool,
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
    /// The media the session can refer to (its history's), for tools that take a media id.
    pub media: Arc<SessionMedia>,
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
