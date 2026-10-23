//! Tool dispatch: running the `ToolUse` blocks of one assistant message (PLAN sections 6.3, 6.4).
//!
//! [`ToolDispatcher`] is the seam. The agent loop hands it a [`ToolBatch`] and fills each
//! result slot as the dispatcher's stream yields `(index, output)`; dropping the stream cancels
//! the calls still running (dropping a [`giles_types::ToolCallHandle`]'s events cancels the
//! call). [`PolicyDispatcher`] is the daemon's: it checks each call against the session's tools
//! and the registry when the call starts, runs consecutive parallel-safe calls together (at most
//! [`MAX_PARALLEL_CALLS`] at once) and every other call alone, gates calls to `ask` tools on the
//! requester's approval, drops a call at its deadline, and stops starting calls once the batch's
//! `skip` signal fires (`busy_mode = "steer"`). Core tools ([`crate::core_tools`]) run through
//! the same path as toolset tools.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream, FuturesUnordered};
use futures::{FutureExt, StreamExt};
use giles_types::{ApprovalAnswer, Block, CallContext, ToolCall, ToolCallEvent, Toolset};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::prompt::SessionTools;
use crate::approvals::TurnApprovals;
use crate::config::{Config, DEFAULT_TOOL_DEADLINE};
use crate::core_tools::{CoreCall, CoreTool, TurnContext};
use crate::media::{SessionMedia, input_media};
use crate::registry::{Registry, ToolTarget};

#[cfg(test)]
mod tests;

/// The error result of a call to a tool the session doesn't offer, that no toolset serves any
/// more, or a core tool with no implementation registered (PLAN section 6.4).
pub const TOOL_UNAVAILABLE: &str = "tool unavailable";

/// How a call dropped at its deadline is answered: `timed out after <deadline>` (PLAN section
/// 6.3).
pub const TIMED_OUT_RESULT: &str = "timed out";

/// The result of a call that hadn't started when the requester sent a new message in `steer`
/// mode (PLAN section 6.3).
pub const SKIPPED_FOR_MESSAGE_RESULT: &str = "Skipped to read a new message";

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
    /// The media the session can refer to: a call's `{"$media": "<id>"}` inputs must be among
    /// them.
    pub media: Arc<SessionMedia>,
    /// The turn, for core tools.
    pub turn: Arc<TurnContext>,
    /// Fired when the requester sent a new message the model should read first (`busy_mode =
    /// "steer"`): calls not started by then get [`SKIPPED_FOR_MESSAGE_RESULT`] and none starts
    /// any more; running calls finish.
    pub skip: CancellationToken,
    /// The turn's approvals: asked before a call to an `ask` tool runs and when a running call
    /// asks a question.
    pub approvals: Arc<TurnApprovals>,
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
///   registry still serves it under the same target, and its implementation is registered: a
///   toolset tool is called on its toolset with its bare name, and the first
///   [`ToolCallEvent::Result`] is the output; a core tool runs its [`CoreTool::call`]. In v1
///   there are no roles, so the session's tools (the agent's allowlist) are the whole policy;
///   with roles, "not permitted for <user>" goes here.
/// - Each `{"$media": "<id>"}` in the input must name media of the session
///   ([`ToolBatch::media`]); the call's [`ToolCall::media`] lists them for the toolset to hand
///   over (PLAN section 5). An unknown id is the call's error result, and nothing runs.
/// - A call to a tool matching the agent's `ask` globs then waits for approval
///   ([`TurnApprovals::approve_call`]); a denial is its error result. The wait doesn't count
///   against the deadline, and the checks above run again once it's approved, since the tool
///   may have gone meanwhile. A barrier waiting for approval holds up the calls after it.
/// - A call still running at its deadline is dropped, which cancels it at the toolset, and gets
///   `timed out after <deadline>`.
/// - Once [`ToolBatch::skip`] fires, no call starts any more: each one still waiting gets
///   [`SKIPPED_FOR_MESSAGE_RESULT`] at once, and the running ones finish.
/// - Progress events are logged. A question the call asks ([`ToolCallEvent::ApprovalRequest`],
///   MCP elicitation) goes to [`TurnApprovals::answer_question`], and the answer back on the
///   handle's `answers`; that wait is part of the call and its deadline.
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
            media,
            turn,
            skip,
            approvals,
        } = batch;
        let config = self.registry.config();
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(index, call)| {
                let policy = CallPolicy::of(&tools, &config, &call.name);
                let registry = self.registry.clone();
                let tools = tools.clone();
                let context = context.clone();
                let turn = turn.clone();
                let approvals = approvals.clone();
                let media = media.clone();
                Scheduled {
                    index,
                    parallel: policy.parallel,
                    call: async move {
                        let mut target = match resolve_call(&registry, &tools, &call.name) {
                            Ok(target) => target,
                            Err(output) => return output,
                        };
                        let call_media = match input_media(&call.input, &media) {
                            Ok(found) => found,
                            Err(error) => return ToolOutput::error(error),
                        };
                        match approvals.approve_call(&call.name, &call.input).await {
                            Err(denied) => return ToolOutput::error(denied),
                            // The tool may have gone while the call waited.
                            Ok(true) => match resolve_call(&registry, &tools, &call.name) {
                                Ok(again) => target = again,
                                Err(output) => return output,
                            },
                            Ok(false) => {}
                        }
                        let running = match target {
                            Target::Toolset(toolset, bare) => {
                                let request = ToolCall {
                                    id: call.id,
                                    name: bare,
                                    input: call.input,
                                    media: call_media,
                                };
                                run_call(toolset, request, &call.name, context, &approvals)
                                    .boxed()
                            }
                            Target::Core(tool) => {
                                let request = CoreCall {
                                    id: call.id,
                                    input: call.input,
                                    context,
                                    turn,
                                    media,
                                };
                                async move { tool.call(request).await }.boxed()
                            }
                        };
                        match tokio::time::timeout(policy.deadline, running).await {
                            Ok(output) => output,
                            Err(_) => {
                                debug!(tool = %call.name, deadline = ?policy.deadline, "tool call timed out");
                                ToolOutput::error(timed_out(policy.deadline))
                            }
                        }
                    }
                    .boxed(),
                }
            })
            .collect();
        schedule(calls, skip)
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
/// stream is polled and there's room for it; dropping the stream drops every running call. Once
/// `skip` fires, every call still waiting is answered with [`SKIPPED_FOR_MESSAGE_RESULT`] at
/// once, and the running ones finish.
fn schedule(
    calls: Vec<Scheduled>,
    skip: CancellationToken,
) -> BoxStream<'static, (usize, ToolOutput)> {
    let scheduler = Scheduler {
        waiting: calls.into(),
        running: FuturesUnordered::new(),
        barrier: false,
        skip,
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
    /// Once fired, no call starts any more.
    skip: CancellationToken,
}

impl Scheduler {
    /// Starts every waiting call that may start now. This is the one place a call starts.
    fn start(&mut self) {
        if self.skip.is_cancelled() {
            return;
        }
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

    /// The next call to finish (or to be skipped), or `None` once every call has.
    async fn next(&mut self) -> Option<(usize, ToolOutput)> {
        loop {
            if self.skip.is_cancelled()
                && let Some(skipped) = self.waiting.pop_front()
            {
                drop(skipped.call);
                return Some((skipped.index, ToolOutput::error(SKIPPED_FOR_MESSAGE_RESULT)));
            }
            self.start();
            // With calls waiting, a skip must answer them without waiting for the running ones.
            if self.waiting.is_empty() || self.skip.is_cancelled() {
                return self.running.next().await;
            }
            tokio::select! {
                biased;
                next = self.running.next() => return next,
                () = self.skip.cancelled() => {}
            }
        }
    }
}

/// Where a call that passed the call-time checks goes.
enum Target {
    /// A toolset's tool: the toolset, and the tool's bare name.
    Toolset(Arc<dyn Toolset>, String),
    /// A core tool.
    Core(Arc<dyn CoreTool>),
}

/// The call-time checks for a call to `name`: where the call goes, or the call's output
/// ([`TOOL_UNAVAILABLE`]) if it can't run.
fn resolve_call(
    registry: &Registry,
    tools: &SessionTools,
    name: &str,
) -> Result<Target, ToolOutput> {
    let Some(tool) = tools.get(name) else {
        debug!(tool = %name, "call to a tool the session doesn't offer");
        return Err(ToolOutput::error(TOOL_UNAVAILABLE));
    };
    // The session keeps its list when a toolset drops a tool; calls to it fail here.
    let served = registry.tools().snapshot();
    if served.get(name).is_none_or(|now| now.target != tool.target) {
        debug!(tool = %name, "call to a tool that is no longer served");
        return Err(ToolOutput::error(TOOL_UNAVAILABLE));
    }
    match &tool.target {
        ToolTarget::Toolset { instance, tool } => match registry.toolset(instance.as_str()) {
            Some(toolset) => Ok(Target::Toolset(toolset, tool.clone())),
            None => {
                debug!(tool = %name, "the tool's toolset is not registered");
                Err(ToolOutput::error(TOOL_UNAVAILABLE))
            }
        },
        ToolTarget::Core => match registry.core_tool(name) {
            Some(core) => Ok(Target::Core(core)),
            None => {
                debug!(tool = %name, "the core tool has no implementation");
                Err(ToolOutput::error(TOOL_UNAVAILABLE))
            }
        },
    }
}

/// Runs one call of `tool` (its offered name) to its result.
async fn run_call(
    toolset: Arc<dyn Toolset>,
    request: ToolCall,
    tool: &str,
    context: CallContext,
    approvals: &TurnApprovals,
) -> ToolOutput {
    let handle = match toolset.call(request, context).await {
        Ok(handle) => handle,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    let answers = handle.answers;
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
            }) => debug!(tool, progress, ?total, ?message, "tool progress"),
            Ok(ToolCallEvent::ApprovalRequest {
                id,
                message,
                schema,
            }) => {
                if schema.is_some() {
                    debug!(tool, %id, "the question asks for data; it can only be approved or denied");
                }
                let decision = approvals.answer_question(tool, &message).await;
                if answers.send(ApprovalAnswer { id, decision }).await.is_err() {
                    debug!(tool, "the call takes no more answers");
                }
            }
            Ok(other) => debug!(tool, ?other, "unexpected tool event ignored"),
            Err(error) => return ToolOutput::error(error.to_string()),
        }
    }
    ToolOutput::error("the tool call ended without a result")
}
