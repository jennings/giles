//! The [`Toolset`] trait: tools and skills (PLAN sections 4.4, 6.4, 6.6, 7.10).

use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::Result;
use crate::channel::ConversationRef;
use crate::id::{AgentId, ApprovalId, UserId};
use crate::message::{Block, MediaRef};
use crate::provider::ToolSpec;

/// A set of tools and skills: an MCP server instance, or the core tools.
///
/// Names here are bare; the core adds the `<instance>__` prefix (PLAN section 6.5).
#[async_trait]
pub trait Toolset: Send + Sync {
    /// The tools, with bare names.
    async fn list_tools(&self) -> Result<Vec<ToolSpec>>;

    /// Updated tool lists, for toolsets whose tools change at runtime (MCP `list_changed`). Each
    /// item is the whole new list. `None` if the list is fixed.
    fn tool_changes(&self) -> Option<BoxStream<'static, Vec<ToolSpec>>> {
        None
    }

    /// The skills this toolset provides, with bare names.
    async fn list_skills(&self) -> Result<Vec<SkillSummary>>;

    /// One skill by bare name. An unknown name is [`Error::NotFound`](crate::Error::NotFound).
    async fn get_skill(&self, name: &str) -> Result<Skill>;

    /// Starts a tool call. Dropping the handle's `events` stream cancels the call, and the
    /// cancellation reaches the server.
    async fn call(&self, call: ToolCall, ctx: CallContext) -> Result<ToolCallHandle>;
}

/// A call of one tool, from a [`Block::ToolUse`].
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// The `ToolUse` id.
    pub id: String,
    /// The bare tool name.
    pub name: String,
    /// The tool input, a JSON object. It may refer to media as `{"$media": "<id>"}`.
    pub input: serde_json::Value,
    /// The media the input refers to (PLAN section 5). The toolset hands these files to the server
    /// and replaces the references.
    pub media: Vec<MediaRef>,
}

/// The turn a tool call belongs to. Core tools use it to scope what they read and write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallContext {
    /// The agent running the turn.
    pub agent: AgentId,
    /// The user whose message or runtime job started the turn. `None` for config jobs.
    pub requester: Option<UserId>,
    /// The session's conversation. `None` for isolated job runs, which have none.
    pub conversation: Option<ChannelConversation>,
}

/// A conversation on a specific channel instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChannelConversation {
    /// The channel instance name.
    pub channel: String,
    /// The conversation.
    pub conversation: ConversationRef,
}

/// A running tool call.
///
/// `events` yields any number of [`ToolCallEvent::Progress`] and
/// [`ToolCallEvent::ApprovalRequest`] events, then one [`ToolCallEvent::Result`], then ends. Each
/// approval request is answered on `answers`.
///
/// Dropping `events` cancels the call. Dropping `answers` only says no more answers will come:
/// the call goes on, and an approval request still pending, or made later, is denied.
pub struct ToolCallHandle {
    /// What the call reports. Dropping it cancels the call.
    pub events: BoxStream<'static, Result<ToolCallEvent>>,
    /// Answers to the call's approval requests.
    pub answers: mpsc::Sender<ApprovalAnswer>,
}

impl fmt::Debug for ToolCallHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCallHandle")
            .field("answers", &self.answers)
            .finish_non_exhaustive()
    }
}

/// An event of a running tool call.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ToolCallEvent {
    /// The call made progress (MCP `notifications/progress`).
    Progress {
        /// Progress so far; increases with each event.
        progress: f64,
        /// The total, if known.
        total: Option<f64>,
        /// A description of the current step.
        message: Option<String>,
    },
    /// The call needs a person's approval or answer before it continues (MCP elicitation). The
    /// core asks through the channel (PLAN section 6.4) and sends an [`ApprovalAnswer`] with this
    /// `id`.
    ApprovalRequest {
        /// Identifies the request within the call.
        id: ApprovalId,
        /// What to ask.
        message: String,
        /// The JSON Schema of the requested answer content, if the server asks for data rather
        /// than a yes or no.
        schema: Option<serde_json::Value>,
    },
    /// The call finished.
    Result {
        /// The result content, for a [`Block::ToolResult`].
        content: Vec<Block>,
        /// Whether the call failed.
        is_error: bool,
    },
}

/// The answer to a [`ToolCallEvent::ApprovalRequest`].
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalAnswer {
    /// The request answered.
    pub id: ApprovalId,
    /// The answer.
    pub decision: ApprovalDecision,
}

/// What a person decided about an approval request.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ApprovalDecision {
    /// Go ahead.
    Approve {
        /// Answer content matching the request's schema, if it had one.
        content: Option<serde_json::Value>,
    },
    /// Don't. Also the answer when the request expired or was superseded.
    Deny,
}

/// A skill's name and description, as listed in the system prompt (PLAN section 6.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    /// The skill name.
    pub name: String,
    /// What the skill is for, from its `SKILL.md` front-matter.
    pub description: String,
}

/// A whole skill, as `load_skill` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The skill name.
    pub name: String,
    /// What the skill is for.
    pub description: String,
    /// The Markdown body of `SKILL.md`, without front-matter.
    pub body: String,
    /// The skill directory, holding `SKILL.md` and any resource files. The core hands those files
    /// to tools that need them (PLAN section 6.6).
    pub dir: PathBuf,
}
