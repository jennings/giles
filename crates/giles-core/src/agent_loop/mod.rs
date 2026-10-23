//! The agent loop (PLAN sections 6.3, 6.7, 6.9, 11.7): prompt assembly and turns. Design:
//! `Docs/technical/agent-loop.md`.
//!
//! [`AgentLoop`] is the [`TurnRunner`] `giles serve` hands the session manager. A turn stores
//! the requester's messages (with their time prefix), then loops: build a request from the
//! session's cached prompt prefix and its stored history, stream the provider, store the
//! assistant message with one result slot per `ToolUse` in one transaction, deliver its text,
//! run its tools through the [`ToolDispatcher`] filling each slot, and repeat until the model
//! stops asking for tools. Every provider request records one `usage` row.
//!
//! Turn control (PLAN section 6.3): `/stop` cancels [`Turn::cancel`], which drops the running
//! request or the dispatcher's stream (cancelling every running call at its toolset) and fills
//! the unanswered slots with `cancelled`. With `busy_mode = "steer"` the requester's new
//! messages join the turn after the current tool batch, and calls of the batch that haven't
//! started are skipped.
//!
//! Approvals (PLAN section 6.4): each turn gets a [`TurnApprovals`](crate::approvals::TurnApprovals)
//! from the [`ApprovalBoard`] the session manager answers; the dispatcher gates `ask` calls and
//! questions on it, and a requester message during a wait denies it.
//!
//! Seams for later tasks: [`ToolDispatcher`] (the [`PolicyDispatcher`] runs calls; 2.11/2.12 add
//! core tools) and [`AgentLoop::rebuild_prompt`] (2.10 compaction).

pub mod dispatch;
pub mod glob;
pub mod history;
mod models;
pub mod output;
pub mod prompt;
mod run;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use giles_store::{Session, SessionId, SessionKey, Store};

pub use dispatch::{
    CallPolicy, MAX_PARALLEL_CALLS, PARALLEL_SAFE_CORE_TOOLS, PolicyDispatcher,
    SKIPPED_FOR_MESSAGE_RESULT, TIMED_OUT_RESULT, TOOL_UNAVAILABLE, ToolBatch, ToolDispatcher,
    ToolOutput, ToolRequest,
};
pub use output::{ChannelOutput, OutputSink};
pub use prompt::{PromptError, SessionPrompt, SessionTools};

use crate::approvals::ApprovalBoard;
use crate::config::{AgentConfig, BusyMode};
use crate::registry::Registry;
use crate::runtime::Runtime;
use crate::sessions::{Clock, SystemClock, Turn, TurnMessage, TurnOutcome, TurnRunner};

/// The result of every unanswered call of a turn that `/stop` or shutdown cancelled (PLAN
/// section 6.3).
pub const CANCELLED_RESULT: &str = "cancelled";

/// The result of every call in a response cut off by `Stop(MaxTokens)` (PLAN section 6.3).
pub const TRUNCATED_RESULT: &str = "truncated";

/// The result of a call the dispatcher finished without answering.
pub const SKIPPED_RESULT: &str = "skipped";

/// The result of every call of the response that hit the turn's iteration cap (PLAN section 6.3).
pub const ITERATION_LIMIT_RESULT: &str = "iteration limit";

/// How long a failure notice may take to send before it's given up (logged).
pub const NOTICE_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs turns: the agent loop. Cheap to share as `Arc<AgentLoop>`.
///
/// Each session's prompt prefix ([`SessionPrompt`]) is assembled at its first turn and cached
/// in memory by session key, so every later request of the session reuses the same tools and
/// `system` bytes. Only the newest session of a key is kept. After a daemon restart the cache
/// is empty and the first turn assembles the prefix again from the files and tools of that
/// moment: if memory files changed meanwhile, that one turn misses the provider's cache.
pub struct AgentLoop {
    registry: Arc<Registry>,
    store: Store,
    clock: Arc<dyn Clock>,
    dispatcher: Arc<dyn ToolDispatcher>,
    approvals: Arc<ApprovalBoard>,
    prompts: Mutex<HashMap<SessionKey, (SessionId, Arc<SessionPrompt>)>>,
    models: models::ModelInfos,
}

impl fmt::Debug for AgentLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentLoop")
            .field("store", &self.store)
            .field("prompts", &lock(&self.prompts).len())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl AgentLoop {
    /// An agent loop resolving agents, models, tools, and skills through `registry` (and its
    /// configuration and data root), storing transcripts and usage in `store`, running tools
    /// through `dispatcher`, and asking for approvals on `approvals` (which must be the board
    /// the session manager hands approval replies to).
    pub fn new(
        registry: Arc<Registry>,
        store: Store,
        clock: Arc<dyn Clock>,
        dispatcher: Arc<dyn ToolDispatcher>,
        approvals: Arc<ApprovalBoard>,
    ) -> Self {
        Self {
            registry,
            store,
            clock,
            dispatcher,
            approvals,
            prompts: Mutex::default(),
            models: models::ModelInfos::default(),
        }
    }

    /// The daemon's agent loop: the runtime's registry and store, the system clock, the
    /// [`PolicyDispatcher`], and `approvals`.
    pub fn from_runtime(runtime: &Runtime, approvals: Arc<ApprovalBoard>) -> Self {
        Self::new(
            runtime.registry.clone(),
            runtime.store.clone(),
            Arc::new(SystemClock),
            Arc::new(PolicyDispatcher::new(runtime.registry.clone())),
            approvals,
        )
    }

    /// The prompt prefix of `session`: cached, or assembled now if this is the session's first
    /// turn since startup. A session without a key (an isolated job run) isn't cached.
    ///
    /// # Errors
    ///
    /// As [`prompt::assemble`].
    pub async fn session_prompt(
        &self,
        session: &Session,
        agent: &AgentConfig,
    ) -> Result<Arc<SessionPrompt>, PromptError> {
        let key = session.key();
        if let Some(key) = &key
            && let Some((id, prompt)) = lock(&self.prompts).get(key)
            && *id == session.id
        {
            return Ok(prompt.clone());
        }
        let prompt = Arc::new(prompt::assemble(&self.registry, agent, session).await?);
        self.remember(key, session.id, prompt.clone());
        Ok(prompt)
    }

    /// Rebuilds `session`'s `system` from the files as they are now (a fresh memory snapshot),
    /// keeping its tools, and caches the result: for compaction (TASKS.md 2.10, PLAN section
    /// 6.7). Without a cached prefix (after a restart) the tools are taken fresh too.
    ///
    /// # Errors
    ///
    /// As [`prompt::assemble`].
    pub async fn rebuild_prompt(
        &self,
        session: &Session,
        agent: &AgentConfig,
    ) -> Result<Arc<SessionPrompt>, PromptError> {
        let key = session.key();
        let cached_tools = key.as_ref().and_then(|key| {
            lock(&self.prompts)
                .get(key)
                .filter(|(id, _)| *id == session.id)
                .map(|(_, prompt)| prompt.tools.clone())
        });
        let prompt = match cached_tools {
            Some(tools) => SessionPrompt {
                system: prompt::build_system(&self.registry, agent, session, &tools).await?,
                tools,
            },
            None => prompt::assemble(&self.registry, agent, session).await?,
        };
        let prompt = Arc::new(prompt);
        self.remember(key, session.id, prompt.clone());
        Ok(prompt)
    }

    fn remember(&self, key: Option<SessionKey>, session: SessionId, prompt: Arc<SessionPrompt>) {
        if let Some(key) = key {
            lock(&self.prompts).insert(key, (session, prompt));
        }
    }

    /// Where the turn's responses go ([`output::output_for`]).
    fn output_for(&self, turn: &Turn) -> Box<dyn OutputSink> {
        output::output_for(turn)
    }

    /// The tool-batch boundary (PLAN section 6.3): every slot of the last batch is filled and
    /// the next request isn't built yet. The messages returned are stored after the tool
    /// results, so the model sees them: the requester's queued messages with `busy_mode =
    /// "steer"`, or in either mode when one of them denied an approval during the batch
    /// (`denied`; every queued one joins, in order). Otherwise, with `"queue"`, none join the
    /// running turn: they stay queued and start the next turn.
    async fn at_batch_boundary(&self, turn: &Turn, denied: bool) -> Vec<TurnMessage> {
        match turn.busy_mode {
            BusyMode::Steer => run::take_steering(turn).await,
            BusyMode::Queue if denied => run::take_steering(turn).await,
            BusyMode::Queue => Vec::new(),
        }
    }
}

#[async_trait]
impl TurnRunner for AgentLoop {
    /// Runs the turn. A panic inside it is caught and ends the turn like a failure: its empty
    /// slots get `interrupted` and the requester is told.
    async fn run_turn(&self, turn: Turn) -> TurnOutcome {
        match AssertUnwindSafe(run::run_turn(self, &turn))
            .catch_unwind()
            .await
        {
            Ok(outcome) => outcome,
            Err(panic) => {
                let message = run::panic_message(panic.as_ref());
                drop(panic);
                run::after_panic(self, &turn, message).await
            }
        }
    }
}
