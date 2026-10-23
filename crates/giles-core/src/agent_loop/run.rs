//! One turn, start to end (PLAN section 6.3).

use futures::StreamExt;
use giles_store::{SessionKind, Slot, UsageRecord};
use giles_types::{
    Block, CacheRetention, CallContext, ChannelConversation, CompletionEvent, CompletionRequest,
    GenerationParams, Message, MessageMeta, Role, StopReason, Usage,
};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use super::dispatch::{ToolBatch, ToolRequest};
use super::history::{self, request_messages, user_message};
use super::output::OutputSink;
use super::prompt::{PromptError, SessionPrompt};
use super::{AgentLoop, CANCELLED_RESULT, NOTICE_TIMEOUT, SKIPPED_RESULT, TRUNCATED_RESULT};
use crate::registry::{RegistryError, ResolvedModel};
use crate::sessions::{INTERRUPTED_RESULT, Turn, TurnOutcome};

/// The longest provider error text a failure notice shows, in characters.
const MAX_NOTICE_ERROR_CHARS: usize = 300;

/// Why a turn failed.
#[derive(Debug, thiserror::Error)]
enum TurnError {
    #[error("agent `{0}` is not configured")]
    UnknownAgent(String),
    #[error(transparent)]
    Model(#[from] RegistryError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error("database error: {0}")]
    Store(#[from] giles_store::Error),
    /// The provider failed; the text is scrubbed of the instance's secrets.
    #[error("the model request failed: {0}")]
    Provider(String),
}

impl TurnError {
    /// What the requester is told.
    fn notice(&self) -> String {
        match self {
            Self::Provider(error) => format!(
                "Sorry, the model request failed: {}",
                shorten(error, MAX_NOTICE_ERROR_CHARS)
            ),
            other => format!("Sorry, something went wrong: {other}"),
        }
    }
}

/// `text` cut to at most `max` characters, marked with `…` when cut.
fn shorten(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// Runs `turn` and reports how it ended. A failure is logged by the session code; here the
/// requester is told and no slot is left empty.
pub(super) async fn run_turn(agent_loop: &AgentLoop, turn: &Turn) -> TurnOutcome {
    match drive(agent_loop, turn).await {
        Ok(outcome) => outcome,
        Err(failure) => {
            fill_empty_slots(agent_loop, turn, INTERRUPTED_RESULT).await;
            send_notice(turn, &failure.notice()).await;
            TurnOutcome::Failed(failure.to_string())
        }
    }
}

/// What the requester is told when a turn panicked.
pub(super) const PANIC_NOTICE: &str = "Sorry, something went wrong: the turn stopped unexpectedly.";

/// The text of a caught panic's payload.
pub(super) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-text panic".to_owned())
}

/// Ends a turn whose future panicked with `message` like a failed one: logs it, fills the
/// turn's empty slots with `interrupted`, and tells the requester.
pub(super) async fn after_panic(
    agent_loop: &AgentLoop,
    turn: &Turn,
    message: String,
) -> TurnOutcome {
    error!(session = %turn.session.id, %message, "the turn panicked");
    fill_empty_slots(agent_loop, turn, INTERRUPTED_RESULT).await;
    send_notice(turn, PANIC_NOTICE).await;
    TurnOutcome::Failed(format!("the turn panicked: {message}"))
}

/// How one provider request ended.
enum Received {
    /// The response is complete.
    Complete(Response),
    /// The turn was cancelled while it streamed; nothing of it is stored.
    Cancelled,
}

/// A complete response: its blocks, why it stopped, and its token usage.
#[derive(Debug, Default)]
pub(super) struct Response {
    pub(super) blocks: Vec<Block>,
    pub(super) stop: Option<StopReason>,
    pub(super) usage: Usage,
}

impl Response {
    /// Takes one streamed event: stores complete blocks and the latest usage. Returns the stop
    /// reason once `Stop` arrives.
    pub(super) fn push(&mut self, event: CompletionEvent) -> Option<StopReason> {
        match event {
            CompletionEvent::BlockStop { block, .. } => self.blocks.push(block),
            CompletionEvent::Usage(usage) => self.usage = usage,
            CompletionEvent::Stop(reason) => {
                self.stop = Some(reason);
                return Some(reason);
            }
            _ => {}
        }
        None
    }
}

async fn drive(agent_loop: &AgentLoop, turn: &Turn) -> Result<TurnOutcome, TurnError> {
    let session = &turn.session;
    // Slots an earlier turn left empty (it panicked, or filling them failed) get `interrupted`
    // now, so this turn's synthetic results can't relabel them.
    if agent_loop
        .store
        .fill_empty_slots(session.id, INTERRUPTED_RESULT, agent_loop.clock.now())
        .await?
    {
        warn!(session = %session.id, "an earlier turn left empty slots; filled as interrupted");
    }
    let config = agent_loop.registry.config().clone();
    let agent = config
        .agents
        .get(&session.agent)
        .ok_or_else(|| TurnError::UnknownAgent(session.agent.to_string()))?;

    // The messages are part of the transcript however the turn ends.
    let tz = history::message_timezone(&config, agent);
    let messages = turn
        .messages
        .iter()
        .map(|message| user_message(message, turn.requester.as_ref(), &tz))
        .collect();
    agent_loop
        .store
        .append_messages(session.id, messages)
        .await?;

    let prompt = agent_loop.session_prompt(session, agent).await?;
    let model = agent_loop.registry.agent_model(agent)?;
    let context = call_context(turn);
    let mut output = agent_loop.output_for(turn);
    let params = GenerationParams {
        cache_retention: match session.kind {
            // Job runs aren't human-paced (PLAN section 6.7).
            SessionKind::Job => CacheRetention::Short,
            _ => CacheRetention::Long,
        },
        ..GenerationParams::default()
    };

    loop {
        if turn.cancel.is_cancelled() {
            return Ok(TurnOutcome::Cancelled);
        }
        let history = agent_loop.store.load_history(session.id).await?;
        let request = CompletionRequest {
            model: model.model.clone(),
            system: prompt.system.clone(),
            messages: request_messages(history),
            tools: prompt.tools.specs(),
            params: params.clone(),
        };
        let response = match receive(agent_loop, turn, &model, request, output.as_mut()).await? {
            Received::Complete(response) => response,
            Received::Cancelled => {
                output.message_aborted().await;
                return Ok(TurnOutcome::Cancelled);
            }
        };
        if response.blocks.is_empty() {
            warn!(session = %session.id, "the model sent an empty response");
            return Ok(TurnOutcome::Completed);
        }

        let mut meta = MessageMeta::new(agent_loop.clock.now());
        meta.provider = Some(model.instance.to_string());
        meta.model = Some(model.model.clone());
        meta.usage = Some(response.usage);
        let message = Message {
            role: Role::Assistant,
            content: response.blocks,
            meta,
        };
        let calls = tool_requests(&message);
        let slots = agent_loop
            .store
            .append_messages(session.id, vec![message.clone()])
            .await?
            .pop()
            .map(|appended| appended.slots)
            .unwrap_or_default();
        if or_cancel(&turn.cancel, output.message_end(&message))
            .await
            .is_none()
        {
            fill_empty_slots(agent_loop, turn, CANCELLED_RESULT).await;
            return Ok(TurnOutcome::Cancelled);
        }
        if calls.is_empty() {
            return Ok(TurnOutcome::Completed);
        }
        if response.stop == Some(StopReason::MaxTokens) {
            // The calls may be cut off mid-input; none of them runs.
            agent_loop
                .store
                .fill_empty_slots(session.id, TRUNCATED_RESULT, agent_loop.clock.now())
                .await?;
            return Ok(TurnOutcome::Completed);
        }

        if !run_tools(agent_loop, turn, &prompt, &context, calls, &slots).await? {
            fill_empty_slots(agent_loop, turn, CANCELLED_RESULT).await;
            return Ok(TurnOutcome::Cancelled);
        }

        let steered = agent_loop.at_batch_boundary(turn).await;
        if !steered.is_empty() {
            let messages = steered
                .iter()
                .map(|message| user_message(message, turn.requester.as_ref(), &tz))
                .collect();
            agent_loop
                .store
                .append_messages(session.id, messages)
                .await?;
        }
    }
}

/// Makes one provider request and reads its stream to the end, recording its usage once the
/// stream has started (a request refused before streaming generated nothing and records none).
async fn receive(
    agent_loop: &AgentLoop,
    turn: &Turn,
    model: &ResolvedModel,
    request: CompletionRequest,
    output: &mut dyn OutputSink,
) -> Result<Received, TurnError> {
    let Some(started) = or_cancel(&turn.cancel, model.provider.complete(request)).await else {
        return Ok(Received::Cancelled);
    };
    let mut stream = started.map_err(|error| provider_error(agent_loop, model, &error))?;
    let mut response = Response::default();
    let end: Result<bool, String> = loop {
        let item = tokio::select! {
            biased;
            () = turn.cancel.cancelled() => break Ok(false),
            item = stream.next() => item,
        };
        match item {
            Some(Ok(event)) => {
                output.event(&event).await;
                if response.push(event).is_some() {
                    break Ok(true);
                }
            }
            Some(Err(error)) => break Err(error.to_string()),
            None => break Err("the response ended before it was complete".to_owned()),
        }
    };
    drop(stream);
    record_usage(agent_loop, turn, model, response.usage).await;
    match end {
        Ok(true) => Ok(Received::Complete(response)),
        Ok(false) => Ok(Received::Cancelled),
        Err(error) => {
            output.message_aborted().await;
            Err(provider_error(agent_loop, model, &error))
        }
    }
}

/// A provider failure, with the provider instance's secrets scrubbed from the text.
fn provider_error(
    agent_loop: &AgentLoop,
    model: &ResolvedModel,
    error: &dyn std::fmt::Display,
) -> TurnError {
    let text = error.to_string();
    let scrubbed = match agent_loop.registry.config().providers.get(&model.instance) {
        Some(instance) => instance.config.scrub(&text),
        None => text,
    };
    TurnError::Provider(scrubbed)
}

/// Records one `usage` row (PLAN section 11.7). A failure is logged; the turn goes on.
async fn record_usage(agent_loop: &AgentLoop, turn: &Turn, model: &ResolvedModel, usage: Usage) {
    let record = UsageRecord {
        at: agent_loop.clock.now(),
        provider: model.instance.to_string(),
        model: model.model.clone(),
        agent: turn.session.agent.clone(),
        user: turn.requester.clone(),
        session: Some(turn.session.id),
        usage,
        fallback: false,
    };
    if let Err(error) = agent_loop.store.record_usage(record).await {
        error!(session = %turn.session.id, %error, "cannot record usage");
    }
}

/// Runs the calls of one assistant message, filling each slot as its result arrives. Returns
/// `false` if the turn was cancelled first: the calls still running are cancelled (the
/// dispatcher's stream is dropped) and their slots are left for the caller.
async fn run_tools(
    agent_loop: &AgentLoop,
    turn: &Turn,
    prompt: &SessionPrompt,
    context: &CallContext,
    calls: Vec<ToolRequest>,
    slots: &[Slot],
) -> Result<bool, TurnError> {
    debug_assert!(
        calls
            .iter()
            .map(|c| &c.id)
            .eq(slots.iter().map(|s| &s.tool_use_id))
    );
    let mut filled = vec![false; calls.len()];
    let mut outputs = agent_loop.dispatcher.dispatch(ToolBatch {
        calls,
        tools: prompt.tools.clone(),
        context: context.clone(),
    });
    loop {
        let next = tokio::select! {
            biased;
            () = turn.cancel.cancelled() => return Ok(false),
            next = outputs.next() => next,
        };
        let Some((index, output)) = next else {
            break;
        };
        match (slots.get(index), filled.get_mut(index)) {
            (Some(slot), Some(done)) if !*done => {
                agent_loop
                    .store
                    .fill_slot(
                        slot.id,
                        output.content,
                        output.is_error,
                        agent_loop.clock.now(),
                    )
                    .await?;
                *done = true;
            }
            _ => warn!(
                index,
                "the dispatcher answered a call twice or out of range"
            ),
        }
    }
    if filled.contains(&false) {
        warn!(session = %turn.session.id, "the dispatcher left calls unanswered");
        agent_loop
            .store
            .fill_empty_slots(turn.session.id, SKIPPED_RESULT, agent_loop.clock.now())
            .await?;
    }
    Ok(true)
}

/// The `ToolUse` blocks of `message`, in order.
fn tool_requests(message: &Message) -> Vec<ToolRequest> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::ToolUse { id, name, input } => Some(ToolRequest {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// The turn as toolsets see it.
fn call_context(turn: &Turn) -> CallContext {
    let conversation = match (&turn.session.channel, turn.messages.first()) {
        (Some(channel), Some(first)) => Some(ChannelConversation {
            channel: channel.clone(),
            conversation: first.message.conversation.clone(),
        }),
        _ => None,
    };
    CallContext {
        agent: turn.session.agent.clone(),
        requester: turn.requester.clone(),
        conversation,
    }
}

/// Fills the session's empty slots with `text`, logging a failure (the startup recovery fills
/// whatever is left).
async fn fill_empty_slots(agent_loop: &AgentLoop, turn: &Turn, text: &str) {
    let now = agent_loop.clock.now();
    if let Err(error) = agent_loop
        .store
        .fill_empty_slots(turn.session.id, text, now)
        .await
    {
        error!(session = %turn.session.id, %error, "cannot fill the turn's empty slots");
    }
}

/// Tells the requester `text`, waiting at most [`NOTICE_TIMEOUT`].
async fn send_notice(turn: &Turn, text: &str) {
    let Some(reply) = &turn.reply else {
        return;
    };
    match tokio::time::timeout(NOTICE_TIMEOUT, reply.send(text)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => warn!(instance = reply.instance(), %error, "cannot send a notice"),
        Err(_) => warn!(instance = reply.instance(), "sending a notice timed out"),
    }
}

/// Runs `future` unless `cancel` fires first.
async fn or_cancel<F: Future>(cancel: &CancellationToken, future: F) -> Option<F::Output> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        output = future => Some(output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_are_short() {
        assert_eq!(shorten("abc", 3), "abc");
        assert_eq!(shorten("abcd", 3), "abc…");
        assert_eq!(shorten("ééé", 2), "éé…");
        let notice = TurnError::Provider("x".repeat(1000)).notice();
        let prefix = "Sorry, the model request failed: ";
        assert!(notice.starts_with(&format!("{prefix}xxx")));
        assert_eq!(
            notice.chars().count(),
            prefix.chars().count() + MAX_NOTICE_ERROR_CHARS + 1
        );
    }

    #[test]
    fn the_last_usage_wins_and_only_stopped_blocks_count() {
        let mut response = Response::default();
        let usage = |input| Usage {
            input_tokens: input,
            ..Usage::default()
        };
        assert_eq!(response.push(CompletionEvent::Usage(usage(1))), None);
        assert_eq!(
            response.push(CompletionEvent::TextDelta {
                index: 0,
                text: "partial".to_owned()
            }),
            None
        );
        response.push(CompletionEvent::BlockStop {
            index: 0,
            block: Block::text("whole"),
        });
        response.push(CompletionEvent::Usage(usage(7)));
        assert_eq!(
            response.push(CompletionEvent::Stop(StopReason::EndTurn)),
            Some(StopReason::EndTurn)
        );
        assert_eq!(response.blocks, [Block::text("whole")]);
        assert_eq!(response.usage, usage(7));
    }
}
