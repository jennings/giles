//! The `memory_search` core tool (PLAN section 6.7): full-text search over the transcripts of
//! the current context, through [`Store::search`]. In a direct or job session that's every
//! direct session of the session's user with this agent, on every channel; in a group, that
//! group's transcript. Memories aren't searched: those are `memory_recall`'s. Read-only,
//! so it's parallel-safe ([`PARALLEL_SAFE_CORE_TOOLS`](crate::agent_loop::PARALLEL_SAFE_CORE_TOOLS)).

use std::fmt::Write as _;

use async_trait::async_trait;
use giles_store::{
    SNIPPET_END, SNIPPET_START, SearchHit, SearchScope, Session, SessionKind, Store,
};
use giles_types::{Block, Role, ToolSpec};
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use super::{CoreCall, CoreTool};
use crate::agent_loop::ToolOutput;
use crate::agent_loop::history::{message_timezone, strip_time_prefix, time_prefix};
use crate::agent_loop::prompt::memory_user;
use crate::config::LiveConfig;

/// The tool's name.
pub const MEMORY_SEARCH: &str = "memory_search";

/// How many hits a search returns unless the model asks for another number.
pub const DEFAULT_LIMIT: u32 = 10;

/// The most hits a search returns.
pub const MAX_LIMIT: u32 = 50;

/// What a search that matches nothing returns.
pub const NO_MATCHES: &str = "No matches.";

/// The `memory_search` tool.
#[derive(Debug, Clone)]
pub struct MemorySearchTool {
    store: Store,
    config: LiveConfig,
}

impl MemorySearchTool {
    /// Searches `store`, showing times in the zone `config` gives each agent's messages.
    pub fn new(store: Store, config: LiveConfig) -> Self {
        Self { store, config }
    }
}

/// The tool's input.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    query: String,
    limit: Option<u32>,
}

/// Parses the tool's input: the query and the number of hits.
///
/// # Errors
///
/// The message for the model.
pub fn parse_input(input: Value) -> Result<(String, u32), String> {
    let input: Input =
        serde_json::from_value(input).map_err(|error| format!("invalid input: {error}"))?;
    if input.query.trim().is_empty() {
        return Err("query is empty".to_owned());
    }
    let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("limit must be between 1 and {MAX_LIMIT}"));
    }
    Ok((input.query, limit))
}

/// The transcripts a search in `session` covers (PLAN section 6.7), or why there are none.
///
/// # Errors
///
/// The message for the model.
pub fn scope(session: &Session) -> Result<SearchScope, String> {
    if let Some(user) = memory_user(session) {
        return Ok(SearchScope::direct(session.agent.clone(), user.clone()));
    }
    match (session.kind, &session.channel, &session.conversation) {
        (SessionKind::Group, Some(channel), Some(conversation)) => Ok(SearchScope::conversation(
            session.agent.clone(),
            channel.clone(),
            conversation.clone(),
        )),
        _ => Err("this conversation has no transcripts to search".to_owned()),
    }
}

/// The hits as the model sees them, best first: one paragraph per hit with its time, who wrote
/// it, and where, then the snippet (matches in `**`). A user message's time is the one in its
/// stored time prefix (when it was sent); other messages show when they were stored, in `tz`.
pub fn render(hits: &[SearchHit], tz: &TimeZone) -> String {
    if hits.is_empty() {
        return NO_MATCHES.to_owned();
    }
    let mut out = format!(
        "{} {}, best first (matches in **bold**):",
        hits.len(),
        if hits.len() == 1 { "match" } else { "matches" }
    );
    for hit in hits {
        let who = match hit.role {
            Role::Assistant => "you",
            _ => "user",
        };
        let place = hit
            .channel
            .as_deref()
            .map(|channel| format!(" on {channel}"))
            .unwrap_or_default();
        let snippet = hit.snippet.trim();
        let (when, text) = match split_sent_prefix(snippet) {
            Some((prefix, text)) => (prefix, text),
            None => (time_prefix(hit.at, tz), snippet),
        };
        let text = text.replace('\n', " ");
        let _ = write!(out, "\n\n{when} {who}{place}: {text}");
    }
    out
}

/// Splits a snippet that starts with a stored user message's time prefix into the prefix,
/// without match markers (a query word like `22` or `BST` is highlighted inside it too), and the
/// text after it. `None` if the snippet doesn't start with a whole prefix (e.g. it was cut).
pub fn split_sent_prefix(snippet: &str) -> Option<(String, &str)> {
    if !snippet.starts_with('[') {
        return None;
    }
    let end = snippet.find(']')?;
    let prefix = snippet[..=end]
        .replace(SNIPPET_START, "")
        .replace(SNIPPET_END, "");
    let rest = &snippet[end + 1..];
    let text = rest.strip_prefix(' ').unwrap_or(rest);
    strip_time_prefix(&format!("{prefix} "))
        .is_empty()
        .then_some((prefix, text))
}

#[async_trait]
impl CoreTool for MemorySearchTool {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let (query, limit) = match parse_input(call.input) {
            Ok(parsed) => parsed,
            Err(error) => return ToolOutput::error(error),
        };
        let scope = match scope(&call.turn.session) {
            Ok(scope) => scope,
            Err(error) => return ToolOutput::error(error),
        };
        let agent = &call.context.agent;
        let config = self.config.get();
        let Some(agent_config) = config.agents.get(agent) else {
            return ToolOutput::error(format!("agent `{agent}` is not configured"));
        };
        match self.store.search(scope, &query, limit).await {
            Ok(hits) => ToolOutput {
                content: vec![Block::text(render(
                    &hits,
                    &message_timezone(&config, agent_config),
                ))],
                is_error: false,
            },
            Err(error) => {
                warn!(%agent, %error, "memory search failed");
                ToolOutput::error("the search failed")
            }
        }
    }
}

/// The tool's spec.
pub(super) fn spec() -> ToolSpec {
    ToolSpec {
        name: MEMORY_SEARCH.to_owned(),
        description: "Search the transcripts of your earlier conversations with this user, on \
                      every channel, for words. Returns short excerpts, best matches first, \
                      with when each message was sent and who wrote it. Use it to recall \
                      something said before that isn't in your memory."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Words to look for. Messages with more of them, and rarer \
                                    ones, rank first; word endings are ignored (`running` \
                                    finds `run`).",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_LIMIT,
                    "default": DEFAULT_LIMIT,
                    "description": "The most excerpts to return.",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
    }
}
