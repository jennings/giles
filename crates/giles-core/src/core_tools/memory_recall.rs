//! The `memory_recall` core tool (PLAN section 6.7): full-text search over the memories visible in
//! the session, core ones included, through
//! [`MemoryStore::recall`](giles_store::MemoryStore::recall). Each hit shows its id, flags, dates,
//! and content. Read-only, so it's parallel-safe
//! ([`PARALLEL_SAFE_CORE_TOOLS`](crate::agent_loop::PARALLEL_SAFE_CORE_TOOLS)). See
//! [`super::memory`] for what the memory tools share.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use giles_store::Memory;
use giles_types::{Block, Timestamp, ToolSpec};
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde_json::{Value, json};

use super::memory::MemoryEnv;
use super::memory_search::{DEFAULT_LIMIT, MAX_LIMIT};
use super::{CoreCall, CoreTool};
use crate::agent_loop::ToolOutput;
use crate::agent_loop::history::time_prefix;
use crate::config::LiveConfig;
use crate::memory;
use crate::memory_dbs::MemoryDbs;
use crate::sessions::Clock;

/// The tool's name.
pub const MEMORY_RECALL: &str = "memory_recall";

/// What a recall that matches nothing returns.
pub const NO_MEMORIES: &str = "No matching memories.";

/// The `memory_recall` tool.
#[derive(Debug, Clone)]
pub struct MemoryRecallTool {
    env: MemoryEnv,
}

impl MemoryRecallTool {
    /// Searches the memory databases in `dbs`, showing times in the zone `config` gives each
    /// agent's messages.
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
    query: String,
    limit: Option<u32>,
}

/// Parses the tool's input: the query and the number of hits, [`DEFAULT_LIMIT`] if not given and
/// never more than [`MAX_LIMIT`] or less than one.
///
/// # Errors
///
/// The message for the model.
pub fn parse_input(input: Value) -> Result<(String, u32), String> {
    let input: Input =
        serde_json::from_value(input).map_err(|error| format!("invalid input: {error}"))?;
    if input.query.trim().is_empty() {
        return Err("invalid input: query is empty".to_owned());
    }
    let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    Ok((input.query, limit))
}

/// A time as `Thu 2026-10-22 14:03 BST`, as the transcript times read.
fn when(at: Timestamp, tz: &TimeZone) -> String {
    let stamp = time_prefix(at, tz);
    stamp
        .strip_prefix('[')
        .and_then(|stamp| stamp.strip_suffix(']'))
        .unwrap_or(&stamp)
        .to_owned()
}

/// The hits as the model sees them, best first: a line per memory (id, flags, content), then
/// when it was created and last updated, in `tz`.
pub fn render(hits: &[Memory], tz: &TimeZone) -> String {
    if hits.is_empty() {
        return NO_MEMORIES.to_owned();
    }
    let mut out = format!(
        "{} {}, best first:",
        hits.len(),
        if hits.len() == 1 {
            "memory"
        } else {
            "memories"
        }
    );
    for hit in hits {
        let _ = write!(
            out,
            "\n\n{}\ncreated {}, updated {}",
            memory::line(hit),
            when(hit.created_at, tz),
            when(hit.updated_at, tz),
        );
    }
    out
}

#[async_trait]
impl CoreTool for MemoryRecallTool {
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
        let (query, limit) = match parse_input(input) {
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
        match access.store.recall(&access.viewer, &query, limit).await {
            Ok(hits) => ToolOutput {
                content: vec![Block::text(render(&hits, &access.tz))],
                is_error: false,
            },
            Err(error) => access.failure(error),
        }
    }
}

/// The tool's spec.
pub(super) fn spec() -> ToolSpec {
    ToolSpec {
        name: MEMORY_RECALL.to_owned(),
        description: "Search your memories for words: the ones you can see in this \
                      conversation, core or not. Only core memories are in your prompt, so use \
                      this to find everything else you remembered earlier. Returns memories \
                      best matches first (the most recently changed first among equal matches), \
                      each with its id, whether it is core and shared, when it was created and \
                      last changed, and its content. The ids work with memory_edit and \
                      memory_forget. It doesn't search past conversations: that's \
                      memory_search."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Words to look for; any of them may match. Memories with \
                                    more of them, and rarer ones, rank first; word endings are \
                                    ignored (`running` finds `run`). There is no query syntax.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "default": DEFAULT_LIMIT,
                    "description": "The most memories to return.",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
    }
}
