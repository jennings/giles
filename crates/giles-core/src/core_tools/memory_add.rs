//! The `memory_add` core tool (PLAN section 6.7): remembers one new memory, through
//! [`MemoryStore::add`](giles_store::MemoryStore::add). The result shows the memory with the id the
//! model edits or forgets it by and, for a core memory, the usage of the scope it counts against.
//! See [`super::memory`] for what the memory tools share.

use std::sync::Arc;

use async_trait::async_trait;
use giles_store::{MAX_MEMORY_CHARS, NewMemory};
use giles_types::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};

use super::memory::{MemoryEnv, touched_scopes};
use super::{CoreCall, CoreTool};
use crate::agent_loop::ToolOutput;
use crate::config::LiveConfig;
use crate::memory_dbs::MemoryDbs;
use crate::sessions::Clock;

/// The tool's name.
pub const MEMORY_ADD: &str = "memory_add";

/// The `memory_add` tool.
#[derive(Debug, Clone)]
pub struct MemoryAddTool {
    env: MemoryEnv,
}

impl MemoryAddTool {
    /// Adds to the memory databases in `dbs`, with each agent's `memory_cap` from `config`, at
    /// the time `clock` gives.
    pub fn new(dbs: MemoryDbs, config: LiveConfig, clock: Arc<dyn Clock>) -> Self {
        Self {
            env: MemoryEnv::new(dbs, config, clock),
        }
    }
}

/// The tool's input.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    content: String,
    #[serde(default)]
    core: bool,
    #[serde(default)]
    shared: bool,
}

/// A parsed `memory_add` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Add {
    /// What to remember.
    pub content: String,
    /// Whether to load it into the prompt.
    pub core: bool,
    /// Whether every session can see it.
    pub shared: bool,
}

/// Parses the tool's input: the content and the two flags, which default to false.
///
/// # Errors
///
/// The message for the model.
pub fn parse_input(input: Value) -> Result<Add, String> {
    let input: Input =
        serde_json::from_value(input).map_err(|error| format!("invalid input: {error}"))?;
    if input.content.trim().is_empty() {
        return Err("invalid input: content is empty".to_owned());
    }
    Ok(Add {
        content: input.content,
        core: input.core,
        shared: input.shared,
    })
}

#[async_trait]
impl CoreTool for MemoryAddTool {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let CoreCall {
            input,
            context,
            turn,
            ..
        } = call;
        let add = match parse_input(input) {
            Ok(add) => add,
            Err(error) => return ToolOutput::error(error),
        };
        let access = match self
            .env
            .access(&context.agent, context.requester.as_ref(), &turn.session)
            .await
        {
            Ok(access) => access,
            Err(output) => return output,
        };
        let new = NewMemory {
            content: add.content,
            core: add.core,
            shared: add.shared,
            source_session: Some(access.session),
        };
        match access
            .store
            .add(&access.viewer, new, access.cap, access.now)
            .await
        {
            Ok(memory) => {
                let scopes = touched_scopes(None, Some(&memory));
                access.changed("Added", &memory, &scopes).await
            }
            Err(error) => access.failure(error),
        }
    }
}

/// The tool's spec.
pub(super) fn spec() -> ToolSpec {
    ToolSpec {
        name: MEMORY_ADD.to_owned(),
        description: format!(
            "Remember something. A memory is one short fact, preference, or instruction (at \
             most {MAX_MEMORY_CHARS} characters); the result gives its id, which memory_edit and \
             memory_forget take.\n\n\
             Core memories are loaded into your system prompt at the start of every conversation \
             that can see them, so make a memory core only if you should always know it. Each \
             scope (shared, each user's, each group's) caps the total characters of its core \
             memories; the memory section of your prompt shows each scope's usage. A core memory \
             that doesn't fit is rejected, and the error lists the scope's core memories so you \
             can shorten, forget, or demote some. Memories that aren't core stay out of the \
             prompt: find them with memory_recall.\n\n\
             A memory is private by default: only this user's conversations with you (or this \
             group conversation) can see it. A shared memory is visible in every conversation \
             you have, including with other users and in groups, so only your owner's turns and \
             scheduled jobs can create one. A scheduled job has no user, so what it remembers \
             must be shared.\n\n\
             The prompt is a snapshot taken when the conversation started and doesn't change \
             while it runs: the result of this tool, not the prompt, shows what you changed."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_MEMORY_CHARS,
                    "description": "What to remember: one fact, short, factual, and durable.",
                },
                "core": {
                    "type": "boolean",
                    "default": false,
                    "description": "Load this memory into your prompt in every conversation \
                                    that can see it. Counts against the scope's core cap.",
                },
                "shared": {
                    "type": "boolean",
                    "default": false,
                    "description": "Make this memory visible in all your conversations, not \
                                    only this user's or this group's. Only your owner's turns \
                                    and scheduled jobs can do this.",
                },
            },
            "required": ["content"],
            "additionalProperties": false,
        }),
    }
}
