//! What the four memory tools ([`MemoryAddTool`](super::MemoryAddTool),
//! [`MemoryEditTool`](super::MemoryEditTool), [`MemoryForgetTool`](super::MemoryForgetTool), and
//! [`MemoryRecallTool`](super::MemoryRecallTool); PLAN section 6.7) have in common: opening the
//! agent's memory database for a call, who the call acts as, and how results and errors read.
//!
//! Every call gets an [`Access`]: the agent's [`MemoryStore`] (fetched from
//! [`MemoryDbs`](crate::memory_dbs::MemoryDbs) for this call only, never kept), the
//! [`MemoryViewer`] the turn acts as ([`crate::memory::viewer`]), the agent's `memory_cap`, and
//! the time. The prompt keeps the snapshot it was built with, so a result is where the model sees
//! a change: the affected memory with its id, and, when a core memory was involved, the usage of
//! the scope it counts against.

use std::fmt::Write as _;
use std::sync::Arc;

use giles_store::{
    CoreOverflow, Error, Memory, MemoryOwner, MemoryScope, MemoryStore, MemoryViewer, Session,
    SessionId,
};
use giles_types::{AgentId, Block, Timestamp, UserId};
use jiff::tz::TimeZone;
use tracing::warn;

use crate::agent_loop::ToolOutput;
use crate::agent_loop::history::message_timezone;
use crate::config::LiveConfig;
use crate::memory::{self, usage_header};
use crate::memory_dbs::MemoryDbs;
use crate::sessions::Clock;

/// What the memory tools need for their whole life: where the databases are, each agent's
/// `memory_cap`, and the clock.
#[derive(Clone)]
pub(super) struct MemoryEnv {
    dbs: MemoryDbs,
    config: LiveConfig,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for MemoryEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryEnv")
            .field("dbs", &self.dbs)
            .finish_non_exhaustive()
    }
}

impl MemoryEnv {
    pub(super) fn new(dbs: MemoryDbs, config: LiveConfig, clock: Arc<dyn Clock>) -> Self {
        Self { dbs, config, clock }
    }

    /// Opens what a call of `agent` in `session`, in a turn of `requester` (`None`: a config job),
    /// works with.
    ///
    /// # Errors
    ///
    /// The error result to return: the requester isn't allowed in this session's memory, the agent
    /// isn't configured, or the database can't be opened.
    pub(super) async fn access(
        &self,
        agent: &AgentId,
        requester: Option<&UserId>,
        session: &Session,
    ) -> Result<Access, ToolOutput> {
        let viewer = memory::viewer(session, requester).map_err(ToolOutput::error)?;
        let config = self.config.get();
        let Some(agent_config) = config.agents.get(agent) else {
            return Err(ToolOutput::error(format!(
                "agent `{agent}` is not configured"
            )));
        };
        let store = self.dbs.get(agent).await.map_err(|error| {
            warn!(%agent, %error, "opening the memory database failed");
            ToolOutput::error("the memory database is unavailable")
        })?;
        Ok(Access {
            agent: agent.clone(),
            store,
            viewer,
            cap: agent_config.memory_cap as usize,
            tz: message_timezone(&config, agent_config),
            session: session.id,
            now: self.clock.now(),
        })
    }
}

/// One call's view of the agent's memory.
pub(super) struct Access {
    agent: AgentId,
    /// The agent's database, for this call only.
    pub(super) store: MemoryStore,
    /// Who the call acts as.
    pub(super) viewer: MemoryViewer,
    /// The agent's `memory_cap`: the most characters of core memories in one scope.
    pub(super) cap: usize,
    /// The zone times are shown in.
    pub(super) tz: TimeZone,
    /// The session the call runs in, recorded as the source of what it writes.
    pub(super) session: SessionId,
    /// When the call runs.
    pub(super) now: Timestamp,
}

impl Access {
    /// The result of a change: `verb` and the memory as the model reads it, then the usage of each
    /// of `scopes` (the core scopes the change touched).
    pub(super) async fn changed(
        &self,
        verb: &str,
        memory: &Memory,
        scopes: &[MemoryScope],
    ) -> ToolOutput {
        let mut text = format!("{verb} {}", memory::line(memory));
        if !scopes.is_empty() {
            match self.store.core_memories(&self.viewer).await {
                Ok(core) => {
                    for scope in scopes {
                        let used = match scope {
                            MemoryScope::Shared => core.shared.chars,
                            MemoryScope::Private(_) => core.own.chars,
                        };
                        let _ = write!(
                            text,
                            "\n{} {}",
                            usage_title(scope),
                            usage_header(used, self.cap)
                        );
                    }
                }
                Err(error) => {
                    warn!(agent = %self.agent, %error, "reading the core memories failed")
                }
            }
        }
        ToolOutput {
            content: vec![Block::text(text)],
            is_error: false,
        }
    }

    /// The error result for a store error, in words the model can act on. Unknown and invisible
    /// memories read the same, so an id of someone else's memory leaks nothing.
    pub(super) fn failure(&self, error: Error) -> ToolOutput {
        ToolOutput::error(match error {
            Error::UnknownMemory(id) => format!("no memory #{id}"),
            Error::SharedWriteForbidden => "shared memories can be created, shared, changed, or \
                forgotten only in the owner's turns and in config jobs; private memories aren't \
                affected"
                .to_owned(),
            Error::MemoryTooLong { chars, max } => format!(
                "the memory is {chars} characters; the limit is {max}. Shorten it, or split it \
                 into several memories"
            ),
            Error::CoreCapExceeded(overflow) => overflow_text(&overflow),
            Error::InvalidRequest(message) => format!("invalid input: {message}"),
            error => {
                warn!(agent = %self.agent, %error, "a memory tool call failed");
                "the memory database failed".to_owned()
            }
        })
    }
}

/// The core scopes a change touches: the scope the memory counted against before (if it was
/// core) and the one it counts against after (if it is core), once each.
pub(super) fn touched_scopes(before: Option<&Memory>, after: Option<&Memory>) -> Vec<MemoryScope> {
    let mut scopes = Vec::new();
    for memory in [before, after].into_iter().flatten() {
        let scope = memory.scope();
        if memory.core && !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    scopes
}

/// How a scope's core memories are named in the middle of a sentence.
fn core_label(scope: &MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Shared => "the shared core memories",
        MemoryScope::Private(MemoryOwner::Group { .. }) => "this group's core memories",
        MemoryScope::Private(_) => "your core memories",
    }
}

/// How a scope's core memories head a usage line.
fn usage_title(scope: &MemoryScope) -> &'static str {
    match scope {
        MemoryScope::Shared => "Shared core memories",
        MemoryScope::Private(MemoryOwner::Group { .. }) => "This group's core memories",
        MemoryScope::Private(_) => "Your core memories",
    }
}

/// The error for a write that would take a scope over its core cap: the overflow, what to do, and
/// the scope's core memories as they still are.
fn overflow_text(overflow: &CoreOverflow) -> String {
    let mut text = format!(
        "not saved: {} would use {}/{} characters, {} over the cap; nothing was changed. Make \
         room by shortening or forgetting some of them, or demote some with memory_edit \
         core=false (a memory that isn't core is still found with memory_recall). A memory that \
         is too long for the cap can only be saved without core.\n\n{} {}:",
        core_label(&overflow.scope),
        overflow.cap + overflow.overflow,
        overflow.cap,
        overflow.overflow,
        usage_title(&overflow.scope),
        usage_header(overflow.used, overflow.cap),
    );
    if overflow.memories.is_empty() {
        text.push_str("\n(none)");
    }
    for memory in &overflow.memories {
        let _ = write!(
            text,
            "\n{} — {} chars",
            memory::line(memory),
            memory.chars()
        );
    }
    text
}
