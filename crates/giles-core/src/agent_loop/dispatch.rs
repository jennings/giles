//! Tool dispatch: running the `ToolUse` blocks of one assistant message (PLAN sections 6.3, 6.4).
//!
//! [`ToolDispatcher`] is the seam. The agent loop hands it a [`ToolBatch`] and fills each
//! result slot as the dispatcher's stream yields `(index, output)`; dropping the stream cancels
//! the calls still running (dropping a [`giles_types::ToolCallHandle`]'s events cancels the
//! call). [`SequentialDispatcher`] runs the calls one at a time, in model order. TASKS.md 2.5
//! replaces it with call-time policy, parallel-safe runs, per-toolset deadlines, and iteration
//! caps; 2.11 and 2.12 add the core tools.

use std::sync::Arc;

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use giles_types::{Block, CallContext, ToolCall, ToolCallEvent};
use serde_json::Value;
use tracing::debug;

use super::prompt::SessionTools;
use crate::registry::{Registry, ToolTarget};

/// The error result of a call to a tool the session doesn't offer, that no toolset serves any
/// more, or (until 2.11/2.12) a core tool (PLAN section 6.4).
pub const TOOL_UNAVAILABLE: &str = "tool unavailable";

/// One `ToolUse` block to run.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRequest {
    /// The `ToolUse` id.
    pub id: String,
    /// The tool's offered name (prefixed, or a bare core tool).
    pub name: String,
    /// The input the model gave.
    pub input: Value,
}

/// The `ToolUse` blocks of one assistant message, and what running them needs.
#[derive(Debug, Clone)]
pub struct ToolBatch {
    /// The calls, in the order the model gave them.
    pub calls: Vec<ToolRequest>,
    /// The session's tools: only these may run.
    pub tools: Arc<SessionTools>,
    /// The turn, for toolsets and core tools.
    pub context: CallContext,
}

/// A tool's result, for its slot.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// The result blocks.
    pub content: Vec<Block>,
    /// Whether the call failed.
    pub is_error: bool,
}

impl ToolOutput {
    /// An error result with one text block.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![Block::text(text)],
            is_error: true,
        }
    }
}

/// Runs the calls of a [`ToolBatch`].
pub trait ToolDispatcher: Send + Sync {
    /// Starts the batch. The stream yields `(index into batch.calls, output)` once per call, as
    /// each finishes, in any order, then ends. The agent loop fills each slot as its output
    /// arrives and drops the stream to cancel the rest (`/stop`); calls must not outlive it.
    fn dispatch(&self, batch: ToolBatch) -> BoxStream<'static, (usize, ToolOutput)>;
}

/// Runs calls one at a time, in model order. A call gets [`TOOL_UNAVAILABLE`] unless the
/// session offers the tool, the registry still serves it, and it's a toolset tool whose toolset
/// is registered; then it's called on that toolset with its bare name, and the first
/// [`ToolCallEvent::Result`] is the output. Progress events are logged. Approval requests are
/// denied, since nothing answers them before TASKS.md 2.7 (the handle's `answers` sender is
/// dropped, which denies them).
pub struct SequentialDispatcher {
    registry: Arc<Registry>,
}

impl SequentialDispatcher {
    /// Dispatches to the toolsets registered in `registry` at call time.
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl std::fmt::Debug for SequentialDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SequentialDispatcher")
            .finish_non_exhaustive()
    }
}

impl ToolDispatcher for SequentialDispatcher {
    fn dispatch(&self, batch: ToolBatch) -> BoxStream<'static, (usize, ToolOutput)> {
        let registry = self.registry.clone();
        let ToolBatch {
            calls,
            tools,
            context,
        } = batch;
        stream::iter(calls.into_iter().enumerate())
            .then(move |(index, call)| {
                let registry = registry.clone();
                let tools = tools.clone();
                let context = context.clone();
                async move { (index, run_call(&registry, &tools, call, context).await) }
            })
            .boxed()
    }
}

/// Runs one call to its result.
async fn run_call(
    registry: &Registry,
    tools: &SessionTools,
    call: ToolRequest,
    context: CallContext,
) -> ToolOutput {
    let Some(tool) = tools.get(&call.name) else {
        debug!(tool = %call.name, "call to a tool the session doesn't offer");
        return ToolOutput::error(TOOL_UNAVAILABLE);
    };
    // The session keeps its list when a toolset drops a tool; calls to it fail here.
    let served = registry.tools().snapshot();
    if served
        .get(&call.name)
        .is_none_or(|now| now.target != tool.target)
    {
        debug!(tool = %call.name, "call to a tool that is no longer served");
        return ToolOutput::error(TOOL_UNAVAILABLE);
    }
    let (instance, bare) = match &tool.target {
        ToolTarget::Toolset { instance, tool } => (instance, tool),
        // Core tools have no implementation yet (TASKS.md 2.11, 2.12).
        _ => return ToolOutput::error(TOOL_UNAVAILABLE),
    };
    let Some(toolset) = registry.toolset(instance.as_str()) else {
        debug!(tool = %call.name, "the tool's toolset is not registered");
        return ToolOutput::error(TOOL_UNAVAILABLE);
    };
    let request = ToolCall {
        id: call.id,
        name: bare.clone(),
        input: call.input,
        media: Vec::new(),
    };
    let handle = match toolset.call(request, context).await {
        Ok(handle) => handle,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    // Nothing answers approval requests yet: dropping the sender denies them.
    drop(handle.answers);
    let mut events = handle.events;
    while let Some(event) = events.next().await {
        match event {
            Ok(ToolCallEvent::Result { content, is_error }) => {
                return ToolOutput { content, is_error };
            }
            Ok(ToolCallEvent::Progress {
                progress,
                total,
                message,
            }) => debug!(tool = %call.name, progress, ?total, ?message, "tool progress"),
            Ok(ToolCallEvent::ApprovalRequest { id, .. }) => {
                debug!(tool = %call.name, %id, "approval request denied: approvals are not built yet");
            }
            Ok(other) => debug!(tool = %call.name, ?other, "unexpected tool event ignored"),
            Err(error) => return ToolOutput::error(error.to_string()),
        }
    }
    ToolOutput::error("the tool call ended without a result")
}
