//! Domain types and component traits (`Provider`, `Channel`, `Toolset`) shared by the Giles core
//! and its plugins.
//!
//! - [`message`]: the content-block message model stored in transcripts (PLAN section 5).
//! - [`provider`]: the [`Provider`] trait and the completion request and event stream.
//! - [`channel`]: the [`Channel`] trait and the messages channels exchange with the core.
//! - [`toolset`]: the [`Toolset`] trait, tool calls, approvals, and skills.
//! - [`id`]: validated identifier newtypes.
//!
//! Every type is re-exported at the crate root. `prost`-generated types never appear here; the
//! gRPC adapters convert at the boundary.

pub mod channel;
mod error;
pub mod id;
pub mod message;
pub mod provider;
pub mod toolset;

pub use channel::{
    AuthChallenge, Channel, ChannelEvent, ChannelInfo, ChannelStatus, ConversationKind,
    ConversationRef, InboundMessage, OutboundMessage, OutputMode, SendTarget,
};
pub use error::{Error, Result};
pub use id::{
    AgentId, ApprovalId, ConversationId, EventId, IdError, MediaId, MessageId, NativeId, UserId,
};
/// An instant in time (UTC), used for message and event times. Serialized as RFC 3339.
pub use jiff::Timestamp;
pub use message::{
    Block, BlockKind, MediaRef, Message, MessageMeta, Opaque, Role, StopReason, Usage,
};
pub use provider::{
    CacheRetention, CompletionEvent, CompletionRequest, GenerationParams, ModelInfo, Provider,
    ToolSpec,
};
pub use toolset::{
    ApprovalAnswer, ApprovalDecision, CallContext, ChannelConversation, Skill, SkillSummary,
    ToolCall, ToolCallEvent, ToolCallHandle, Toolset,
};
