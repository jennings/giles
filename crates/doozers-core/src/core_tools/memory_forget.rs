//! The `memory_forget` core tool (PLAN section 6.7): deletes a memory by its id, through
//! [`MemoryStore::forget`](doozers_store::MemoryStore::forget). Nothing is kept of it. The result
//! shows what was forgotten and, for a core memory, the usage of the scope it counted against. See
//! [`super::memory`] for what the memory tools share.

use std::sync::Arc;

use async_trait::async_trait;
use doozers_store::MemoryId;
use doozers_types::ToolSpec;
use serde::Deserialize;
use serde_json::{Value, json};

use super::memory::{MemoryEnv, touched_scopes};
use super::{CoreCall, CoreTool};
use crate::agent_loop::ToolOutput;
use crate::config::LiveConfig;
use crate::memory_dbs::MemoryDbs;
use crate::sessions::Clock;
use crate::users::UserList;

/// The tool's name.
pub const MEMORY_FORGET: &str = "memory_forget";

/// The `memory_forget` tool.
#[derive(Debug, Clone)]
pub struct MemoryForgetTool {
    env: MemoryEnv,
}

impl MemoryForgetTool {
    /// Deletes from the memory databases in `dbs`, with each agent's `memory_cap` from `config`
    /// (for the usage a result shows). `users` gives the requester's time zone, as for the other
    /// memory tools. `clock` is the one they share; deleting reads no time.
    pub fn new(dbs: MemoryDbs, config: LiveConfig, users: UserList, clock: Arc<dyn Clock>) -> Self {
        Self {
            env: MemoryEnv::new(dbs, config, users, clock),
        }
    }
}

/// The tool's input.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    id: i64,
}

/// Parses the tool's input: the memory's id.
///
/// # Errors
///
/// The message for the model.
pub fn parse_input(input: Value) -> Result<MemoryId, String> {
    let input: Input =
        serde_json::from_value(input).map_err(|error| format!("invalid input: {error}"))?;
    Ok(MemoryId::new(input.id))
}

#[async_trait]
impl CoreTool for MemoryForgetTool {
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
        let id = match parse_input(input) {
            Ok(id) => id,
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
        match access.store.forget(&access.viewer, id).await {
            Ok(memory) => {
                let scopes = touched_scopes(Some(&memory), None);
                access.changed("Forgot", &memory, &scopes).await
            }
            Err(error) => access.failure(error),
        }
    }
}

/// The tool's spec.
pub(super) fn spec() -> ToolSpec {
    ToolSpec {
        name: MEMORY_FORGET.to_owned(),
        description: "Delete a memory by its id. It is gone for good: nothing keeps its earlier \
                      text. Use it for what is wrong, outdated, or no longer worth keeping, and to \
                      make room when a scope's core memories are full. Forgetting a shared memory \
                      is only for admins' turns, their scheduled jobs, and jobs from \
                      agent.toml. An id you can't see \
                      reads as no such memory.\n\n\
                      The prompt is a snapshot taken when the conversation started and doesn't \
                      change while it runs: the result of this tool shows what you forgot."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The memory's id, as shown in your prompt, by memory_add, \
                                    and by memory_recall.",
                },
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    }
}
