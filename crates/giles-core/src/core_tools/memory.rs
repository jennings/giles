//! The `memory` core tool (PLAN section 6.7): batches of `add`, `patch`, and `remove` on one
//! memory file, through [`crate::memory::apply`].
//!
//! Scopes: `agent` (`agents/<agent>/MEMORY.md`) and `user` (`agents/<agent>/users/<user>/
//! MEMORY.md` of the session's user, in direct and job sessions). `group` is designed but not in
//! v1. Agent-wide memory is loaded into every prompt, so it's written only in turns of the owner
//! (runtime jobs run as their creator, so the owner's jobs count) and in config jobs, which have
//! no requester. The prompt keeps the snapshot it was built with; the tool result shows the
//! file after the batch.

use std::sync::Arc;

use async_trait::async_trait;
use giles_store::Session;
use giles_types::{Block, ToolSpec, UserId};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{CoreCall, CoreTool};
use crate::agent_loop::ToolOutput;
use crate::agent_loop::prompt::{AGENT_MEMORY_TITLE, EMPTY_MEMORY, USER_MEMORY_TITLE, memory_user};
use crate::config::Config;
use crate::identity::owner;
use crate::memory::{self, BatchError, MemoryError, MemoryOp, usage_header};
use crate::paths::DataRoot;

/// The tool's name.
pub const MEMORY: &str = "memory";

/// The `memory` tool.
#[derive(Debug, Clone)]
pub struct MemoryTool {
    root: DataRoot,
    config: Arc<Config>,
}

impl MemoryTool {
    /// Writes memory files under `root`, with each agent's `memory_cap` from `config`.
    pub fn new(root: DataRoot, config: Arc<Config>) -> Self {
        Self { root, config }
    }
}

/// A memory scope (PLAN section 6.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Agent-wide memory.
    Agent,
    /// The session user's memory with this agent.
    User,
}

/// Whose memory file a batch writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The agent's `MEMORY.md`.
    Agent,
    /// This user's `MEMORY.md` with the agent.
    User(UserId),
}

impl Target {
    /// The heading the prompt shows the file under.
    fn title(&self) -> &'static str {
        match self {
            Self::Agent => AGENT_MEMORY_TITLE,
            Self::User(_) => USER_MEMORY_TITLE,
        }
    }
}

/// The tool's input.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    scope: String,
    operations: Vec<Value>,
}

/// Parses the tool's input: the scope and the operations, each error naming what's wrong.
///
/// # Errors
///
/// The message for the model.
pub fn parse_input(input: Value) -> Result<(Scope, Vec<MemoryOp>), String> {
    let input: Input =
        serde_json::from_value(input).map_err(|error| format!("invalid input: {error}"))?;
    let scope = match input.scope.as_str() {
        "agent" => Scope::Agent,
        "user" => Scope::User,
        "group" => return Err("the `group` memory scope isn't available".to_owned()),
        other => {
            return Err(format!(
                "unknown memory scope {other:?}: use \"agent\" or \"user\""
            ));
        }
    };
    if input.operations.is_empty() {
        return Err("operations is empty: give at least one operation".to_owned());
    }
    let operations = input
        .operations
        .into_iter()
        .enumerate()
        .map(|(index, op)| {
            serde_json::from_value(op).map_err(|error| format!("operations[{index}]: {error}"))
        })
        .collect::<Result<_, _>>()?;
    Ok((scope, operations))
}

/// Which file a batch on `scope` writes in a turn of `requester` (`None`: a config job) in
/// `session`, or why it may not.
///
/// - `agent`: only the owner's turns and config jobs (PLAN section 6.7). In v1 the owner is the
///   only user, and there are no admins.
/// - `user`: the memory of the session's user, in a direct or job session, and only in that
///   user's turns or a config job's.
///
/// # Errors
///
/// The message for the model.
pub fn target(
    scope: Scope,
    requester: Option<&UserId>,
    session: &Session,
) -> Result<Target, String> {
    match scope {
        Scope::Agent => match requester {
            Some(user) if *user != owner() => Err(format!(
                "not permitted for {user}: agent-wide memory is written only in the owner's \
                 turns and in config jobs"
            )),
            _ => Ok(Target::Agent),
        },
        Scope::User => {
            let Some(user) = memory_user(session) else {
                return Err(
                    "the `user` memory scope is only available in direct conversations".to_owned(),
                );
            };
            match requester {
                Some(requester) if requester != user => Err(format!(
                    "not permitted for {requester}: this conversation's user memory belongs to \
                     {user}"
                )),
                _ => Ok(Target::User(user.clone())),
            }
        }
    }
}

/// The memory as the model sees it: `<heading> [usage]:` and the entries (or `(empty)`).
fn render(heading: &str, entries: &[String], used: usize, cap: usize) -> String {
    let body = if entries.is_empty() {
        EMPTY_MEMORY.to_owned()
    } else {
        entries.join(&format!("\n{}\n", memory::ENTRY_SEPARATOR))
    };
    format!("{heading} {}:\n\n{body}", usage_header(used, cap))
}

#[async_trait]
impl CoreTool for MemoryTool {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let (scope, operations) = match parse_input(call.input) {
            Ok(parsed) => parsed,
            Err(error) => return ToolOutput::error(error),
        };
        let target = match target(scope, call.context.requester.as_ref(), &call.turn.session) {
            Ok(target) => target,
            Err(error) => return ToolOutput::error(error),
        };
        let agent = &call.context.agent;
        let Some(config) = self.config.agents.get(agent) else {
            return ToolOutput::error(format!("agent `{agent}` is not configured"));
        };
        let cap = config.memory_cap as usize;
        let path = match &target {
            Target::Agent => memory::agent_memory_file(&self.root, agent.as_str()),
            Target::User(user) => {
                memory::user_memory_file(&self.root, agent.as_str(), user.as_str())
            }
        };
        let path = match path {
            Ok(path) => path,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        let title = target.title();
        match memory::apply(&path, operations, cap).await {
            Ok(doc) => ToolOutput {
                content: vec![Block::text(render(
                    &format!("Saved. {title} is now"),
                    doc.entries(),
                    doc.chars(),
                    cap,
                ))],
                is_error: false,
            },
            Err(MemoryError::Batch(
                ref error @ BatchError::OverCap {
                    ref entries,
                    used,
                    cap,
                    ..
                },
            )) => ToolOutput::error(format!(
                "{error}\n\n{}",
                render(&format!("{title} is unchanged"), entries, used, cap)
            )),
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }
}

/// The tool's spec.
pub(super) fn spec() -> ToolSpec {
    let old_text = json!({
        "type": "string",
        "minLength": 1,
        "description": "Text copied exactly from the memory.",
    });
    ToolSpec {
        name: MEMORY.to_owned(),
        description: "Change your memory: the memory files shown in your system prompt. The \
                      prompt keeps the snapshot taken when this conversation started; the result \
                      of this tool shows the memory after your change. Entries are separated by \
                      lines holding only `§`. Operations apply in order, all or nothing: `add` \
                      appends a new entry; `patch` replaces `old_text`, which must occur exactly \
                      once, with `content` (empty content deletes it); `remove` deletes the one \
                      entry containing `old_text`. Each scope has a character cap, shown with \
                      its usage in the prompt; a batch that would end over the cap changes \
                      nothing and shows the current entries, so you can merge, shorten, or \
                      remove entries in your next batch. Keep entries short, factual, and \
                      durable."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["agent", "user"],
                    "description": "`agent`: what you remember in general, shared by all your \
                                    conversations. `user`: what you remember about the user \
                                    you're talking to.",
                },
                "operations": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Applied in order, each seeing the previous ones' result.",
                    "items": {
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"type": "string", "enum": ["add"]},
                                    "content": {
                                        "type": "string",
                                        "minLength": 1,
                                        "description": "The new entry.",
                                    },
                                },
                                "required": ["op", "content"],
                                "additionalProperties": false,
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"type": "string", "enum": ["patch"]},
                                    "old_text": old_text,
                                    "content": {
                                        "type": "string",
                                        "description": "The replacement; may be empty.",
                                    },
                                },
                                "required": ["op", "old_text", "content"],
                                "additionalProperties": false,
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"type": "string", "enum": ["remove"]},
                                    "old_text": old_text,
                                },
                                "required": ["op", "old_text"],
                                "additionalProperties": false,
                            },
                        ],
                    },
                },
            },
            "required": ["scope", "operations"],
            "additionalProperties": false,
        }),
    }
}
