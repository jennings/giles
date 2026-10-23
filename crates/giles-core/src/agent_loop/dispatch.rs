//! Tool dispatch: running the `ToolUse` blocks of one assistant message (PLAN sections 6.3, 6.4).
//!
//! [`ToolDispatcher`] is the seam. The agent loop hands it a [`ToolBatch`] and fills each
//! result slot as the dispatcher's stream yields `(index, output)`; dropping the stream cancels
//! the calls still running (dropping a [`giles_types::ToolCallHandle`]'s events cancels the
//! call). [`PolicyDispatcher`] is the daemon's: it checks each call against the session's tools
//! and the registry when the call starts, runs consecutive parallel-safe calls together (at most
//! [`MAX_PARALLEL_CALLS`] at once) and every other call alone, and drops a call at its toolset's
//! deadline. 2.11 and 2.12 add the core tools.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream, FuturesUnordered};
use futures::{FutureExt, StreamExt};
use giles_types::{Block, CallContext, ToolCall, ToolCallEvent};
use serde_json::Value;
use tracing::debug;

use super::prompt::SessionTools;
use crate::config::{Config, DEFAULT_TOOL_DEADLINE};
use crate::registry::{Registry, ToolTarget};

#[cfg(test)]
mod tests;

/// The error result of a call to a tool the session doesn't offer, that no toolset serves any
/// more, or (until 2.11/2.12) a core tool (PLAN section 6.4).
pub const TOOL_UNAVAILABLE: &str = "tool unavailable";

/// How a call dropped at its deadline is answered: `timed out after <deadline>` (PLAN section
/// 6.3).
pub const TIMED_OUT_RESULT: &str = "timed out";

/// The most parallel-safe calls that run at once (PLAN section 6.3).
pub const MAX_PARALLEL_CALLS: usize = 8;

/// The core tools that only read, so they're parallel-safe (PLAN section 6.3).
pub const PARALLEL_SAFE_CORE_TOOLS: [&str; 2] = ["load_skill", "memory_search"];

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

/// How a call is scheduled: decided from the session's tools and the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallPolicy {
    /// Whether it may run alongside other parallel-safe calls.
    pub parallel: bool,
    /// How long it may run before it's dropped.
    pub deadline: Duration,
}

impl CallPolicy {
    /// The policy of a call to `name`: a toolset tool takes its instance's `parallel` and
    /// `deadline`; a core tool is parallel-safe if it's one of [`PARALLEL_SAFE_CORE_TOOLS`]
    /// and has the default deadline. A tool the session doesn't offer runs nothing (its call is
    /// answered at once), so it's parallel-safe and never holds up another call.
    pub fn of(tools: &SessionTools, config: &Config, name: &str) -> Self {
        match tools.get(name).map(|tool| &tool.target) {
            None => Self {
                parallel: true,
                deadline: DEFAULT_TOOL_DEADLINE,
            },
            Some(ToolTarget::Core) => Self {
                parallel: PARALLEL_SAFE_CORE_TOOLS.contains(&name),
                deadline: DEFAULT_TOOL_DEADLINE,
            },
            // Registered toolsets are always configured.
            Some(ToolTarget::Toolset { instance, .. }) => config.toolsets.get(instance).map_or(
                Self {
                    parallel: false,
                    deadline: DEFAULT_TOOL_DEADLINE,
                },
                |toolset| Self {
                    parallel: toolset.parallel,
                    deadline: toolset.deadline,
                },
            ),
        }
    }
}

/// The daemon's dispatcher (PLAN sections 6.3, 6.4).
///
/// - Calls start in model order. Consecutive parallel-safe calls ([`CallPolicy`]) run together,
///   at most [`MAX_PARALLEL_CALLS`] at once (the next starts as one finishes); any other call is
///   a barrier: it starts once everything before it has finished and runs alone.
/// - When a call starts it gets [`TOOL_UNAVAILABLE`] unless the session offers the tool, the
///   registry still serves it under the same target, and it's a toolset tool whose toolset is
///   registered; then it's called on that toolset with its bare name, and the first
///   [`ToolCallEvent::Result`] is the output. In v1 there are no roles, so the session's tools
///   (the agent's allowlist) are the whole policy; with roles, "not permitted for <user>" goes
///   here.
/// - A call still running at its deadline is dropped, which cancels it at the toolset, and gets
///   `timed out after <deadline>`.
/// - Progress events are logged. Approval requests are denied, since nothing answers them before
///   TASKS.md 2.7 (the handle's `answers` sender is dropped, which denies them).
pub struct PolicyDispatcher {
    registry: Arc<Registry>,
}

impl PolicyDispatcher {
    /// Dispatches to the toolsets registered in `registry` at call time, with the policies of
    /// its configuration.
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl std::fmt::Debug for PolicyDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyDispatcher").finish_non_exhaustive()
    }
}

impl ToolDispatcher for PolicyDispatcher {
    fn dispatch(&self, batch: ToolBatch) -> BoxStream<'static, (usize, ToolOutput)> {
        let ToolBatch {
            calls,
            tools,
            context,
        } = batch;
        let config = self.registry.config();
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| {
                let policy = CallPolicy::of(&tools, config, &call.name);
                let registry = self.registry.clone();
                let tools = tools.clone();
                let context = context.clone();
                Scheduled {
                    index,
                    parallel: policy.parallel,
                    call: async move {
                        let name = call.name.clone();
                        let running = run_call(&registry, &tools, call, context);
                        match tokio::time::timeout(policy.deadline, running).await {
                            Ok(output) => output,
                            Err(_) => {
                                debug!(tool = %name, deadline = ?policy.deadline, "tool call timed out");
                                ToolOutput::error(timed_out(policy.deadline))
                            }
                        }
                    }
                    .boxed(),
                }
            })
            .collect();
        schedule(calls)
    }
}

/// `timed out after <deadline>`, the deadline in its largest whole unit.
fn timed_out(deadline: Duration) -> String {
    let ms = deadline.as_millis();
    let amount = [(3_600_000, "h"), (60_000, "m"), (1_000, "s")]
        .into_iter()
        .find(|(unit, _)| ms > 0 && ms.is_multiple_of(*unit))
        .map_or_else(
            || format!("{ms}ms"),
            |(unit, name)| format!("{}{name}", ms / unit),
        );
    format!("{TIMED_OUT_RESULT} after {amount}")
}

/// A call waiting to start.
struct Scheduled {
    index: usize,
    parallel: bool,
    call: BoxFuture<'static, ToolOutput>,
}

/// Runs `calls` in order, the parallel-safe ones together (up to [`MAX_PARALLEL_CALLS`]) and
/// every other one alone, yielding `(index, output)` as each finishes. A call starts when the
/// stream is polled and there's room for it; dropping the stream drops every running call.
fn schedule(calls: Vec<Scheduled>) -> BoxStream<'static, (usize, ToolOutput)> {
    let scheduler = Scheduler {
        waiting: calls.into(),
        running: FuturesUnordered::new(),
        barrier: false,
    };
    stream::unfold(scheduler, |mut scheduler| async move {
        let next = scheduler.next().await?;
        Some((next, scheduler))
    })
    .boxed()
}

struct Scheduler {
    waiting: VecDeque<Scheduled>,
    running: FuturesUnordered<BoxFuture<'static, (usize, ToolOutput)>>,
    /// Whether the call running is a barrier (it runs alone).
    barrier: bool,
}

impl Scheduler {
    /// Starts every waiting call that may start now.
    fn start(&mut self) {
        while let Some(next) = self.waiting.front() {
            let room = self.running.is_empty()
                || (!self.barrier && next.parallel && self.running.len() < MAX_PARALLEL_CALLS);
            if !room {
                break;
            }
            let Some(Scheduled {
                index,
                parallel,
                call,
            }) = self.waiting.pop_front()
            else {
                break;
            };
            self.barrier = !parallel;
            self.running
                .push(call.map(move |output| (index, output)).boxed());
        }
    }

    /// The next call to finish, or `None` once every call has.
    async fn next(&mut self) -> Option<(usize, ToolOutput)> {
        self.start();
        self.running.next().await
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
