//! The `memory_edit` core tool (PLAN section 6.7): changes a memory's content, whether it is core,
//! or whether it is shared, through [`MemoryStore::edit`](giles_store::MemoryStore::edit). The
//! content replaces the whole old content (a memory holds one fact). The result shows the memory
//! after the change and the usage of every core scope it left or entered. See [`super::memory`]
//! for what the memory tools share.

use std::sync::Arc;

use async_trait::async_trait;
use giles_store::{MAX_MEMORY_CHARS, MemoryEdit, MemoryId};
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
pub const MEMORY_EDIT: &str = "memory_edit";

/// The `memory_edit` tool.
#[derive(Debug, Clone)]
pub struct MemoryEditTool {
    env: MemoryEnv,
}

impl MemoryEditTool {
    /// Edits the memory databases in `dbs`, with each agent's `memory_cap` from `config`, at the
    /// time `clock` gives.
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
    id: i64,
    content: Option<String>,
    core: Option<bool>,
    shared: Option<bool>,
}

/// Parses the tool's input: the memory's id and the changes, at least one.
///
/// # Errors
///
/// The message for the model.
pub fn parse_input(input: Value) -> Result<(MemoryId, MemoryEdit), String> {
    let input: Input =
        serde_json::from_value(input).map_err(|error| format!("invalid input: {error}"))?;
    if input.content.is_none() && input.core.is_none() && input.shared.is_none() {
        return Err("invalid input: give at least one of content, core, or shared".to_owned());
    }
    if input
        .content
        .as_deref()
        .is_some_and(|c| c.trim().is_empty())
    {
        return Err(
            "invalid input: content is empty; use memory_forget to delete a memory".to_owned(),
        );
    }
    let edit = MemoryEdit {
        content: input.content,
        core: input.core,
        shared: input.shared,
    };
    Ok((MemoryId::new(input.id), edit))
}

#[async_trait]
impl CoreTool for MemoryEditTool {
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
        let (id, edit) = match parse_input(input) {
            Ok(parsed) => parsed,
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
        // What the memory was, to tell which core scopes the edit touched.
        let before = match access.store.get(&access.viewer, id).await {
            Ok(memory) => memory,
            Err(error) => return access.failure(error),
        };
        match access
            .store
            .edit(&access.viewer, id, edit, access.cap, access.now)
            .await
        {
            Ok(after) => {
                let scopes = touched_scopes(Some(&before), Some(&after));
                access.changed("Updated", &after, &scopes).await
            }
            Err(error) => access.failure(error),
        }
    }
}

/// The tool's spec.
pub(super) fn spec() -> ToolSpec {
    ToolSpec {
        name: MEMORY_EDIT.to_owned(),
        description: format!(
            "Change a memory you remembered with memory_add, by its id: what it says, whether it \
             is core (loaded into your prompt), or whether it is shared. `content` replaces the \
             whole old content (at most {MAX_MEMORY_CHARS} characters), so give the full new \
             text; a memory holds one fact. Give at least one of content, core, or shared.\n\n\
             Making a memory core, or making it longer, is rejected if it would take its scope \
             over the core character cap; the error lists that scope's core memories so you can \
             shorten, forget, or demote some (core=false keeps a memory, findable with \
             memory_recall, but out of the prompt). Sharing a memory, or changing or un-sharing a \
             shared one, is only for your owner's turns and scheduled jobs. An id you can't see \
             reads as no such memory.\n\n\
             The prompt is a snapshot taken when the conversation started and doesn't change \
             while it runs: the result of this tool shows the memory as it is now."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "The memory's id, as shown in your prompt, by memory_add, \
                                    and by memory_recall.",
                },
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": MAX_MEMORY_CHARS,
                    "description": "The new content, replacing the old one.",
                },
                "core": {
                    "type": "boolean",
                    "description": "Whether the memory is loaded into your prompt in every \
                                    conversation that can see it.",
                },
                "shared": {
                    "type": "boolean",
                    "description": "Whether the memory is visible in all your conversations, \
                                    not only this user's or this group's. Only your owner's \
                                    turns and scheduled jobs can change it for a memory.",
                },
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
    }
}
