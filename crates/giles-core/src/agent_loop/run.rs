//! One turn, start to end (PLAN section 6.3).

use std::sync::Arc;

use futures::StreamExt;
use giles_store::{InboxId, SessionKind, Slot, UsageRecord};
use giles_types::{
    Block, CacheRetention, CallContext, ChannelConversation, CompletionEvent, CompletionRequest,
    GenerationParams, Message, MessageMeta, Role, StopReason, Usage,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use super::compaction::{self, Compaction};
use super::dispatch::{ToolBatch, ToolRequest};
use super::history::{self, request_messages, user_message};
use super::output::OutputSink;
use super::prompt::{PromptError, SessionPrompt};
use super::{
    AgentLoop, CANCELLED_RESULT, ITERATION_LIMIT_RESULT, NOTICE_TIMEOUT, SKIPPED_RESULT,
    TRUNCATED_RESULT,
};
use crate::approvals::TurnApprovals;
use crate::config::{AgentConfig, BusyMode};
use crate::core_tools::TurnContext;
use crate::media::{self, SessionMedia};
use crate::registry::{RegistryError, ResolvedModel};
use crate::sessions::{INTERRUPTED_RESULT, Turn, TurnMessage, TurnOutcome};

/// The longest provider error text a failure notice shows, in characters.
const MAX_NOTICE_ERROR_CHARS: usize = 300;

/// Why a turn failed.
#[derive(Debug, thiserror::Error)]
pub(super) enum TurnError {
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
pub(super) fn shorten(text: &str, max: usize) -> String {
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
pub(super) enum Received {
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

    let mut prompt = agent_loop.session_prompt(session, agent).await?;
    let model = agent_loop.registry.agent_model(agent)?;
    // The model's context window (compaction) and media types, looked up once per turn.
    let Some(model_info) = or_cancel(&turn.cancel, agent_loop.models.get(&model, &config)).await
    else {
        return Ok(TurnOutcome::Cancelled);
    };
    let mut compaction = Compaction::new(
        compaction::context_window(model_info.as_deref(), &model, &config),
        &prompt,
    );
    let context = CallContexts {
        call: call_context(turn),
        turn: Arc::new(TurnContext {
            session: turn.session.clone(),
            reply: turn.reply.clone(),
            skills: prompt.skills.clone(),
        }),
    };
    let approvals = Arc::new(TurnApprovals::for_turn(
        agent_loop.approvals.clone(),
        agent,
        turn,
    ));
    let mut output = agent_loop.output_for(turn);
    let params = GenerationParams {
        cache_retention: match session.kind {
            // Job runs aren't human-paced (PLAN section 6.7).
            SessionKind::Job => CacheRetention::Short,
            _ => CacheRetention::Long,
        },
        ..GenerationParams::default()
    };
    let cap = iteration_cap(agent, session.kind);
    // Requests made with tools offered.
    let mut iterations = 0_u32;
    // Set once the cap is hit: the next request is the turn's last, with no tools offered.
    let mut last = false;

    loop {
        if turn.cancel.is_cancelled() {
            return Ok(TurnOutcome::Cancelled);
        }
        let history = agent_loop.store.load_history(session.id).await?;
        // Before the request, never inside a tool batch: every slot of the last one is filled.
        let Some(history) = compaction
            .check(agent_loop, turn, agent, &mut prompt, history)
            .await?
        else {
            return Ok(TurnOutcome::Cancelled);
        };
        let mut messages = request_messages(history);
        let media = Arc::new(SessionMedia::from_messages(&messages));
        if !media.is_empty()
            && let Some(info) = &model_info
        {
            media::keep_accepted(&mut messages, &|media_type| info.accepts(media_type));
        }
        let request = CompletionRequest {
            model: model.model.clone(),
            system: prompt.system.clone(),
            messages,
            tools: if last {
                Vec::new()
            } else {
                prompt.tools.specs()
            },
            params: params.clone(),
        };
        if !last {
            iterations += 1;
        }
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
        if last || cap.is_some_and(|cap| iterations >= cap) {
            // None of the calls runs. At the cap, one more request without tools lets the model
            // say where it stopped; after that request the turn ends whatever it asked for.
            agent_loop
                .store
                .fill_empty_slots(session.id, ITERATION_LIMIT_RESULT, agent_loop.clock.now())
                .await?;
            if last {
                warn!(session = %session.id, "the model asked for tools in a request without any");
                return Ok(TurnOutcome::Completed);
            }
            debug!(session = %session.id, iterations, "the turn hit its iteration limit");
            last = true;
            continue;
        }

        let denied = match run_tools(
            agent_loop, turn, &prompt, &context, &approvals, media, calls, &slots,
        )
        .await?
        {
            BatchEnd::Cancelled => {
                fill_empty_slots(agent_loop, turn, CANCELLED_RESULT).await;
                return Ok(TurnOutcome::Cancelled);
            }
            BatchEnd::Done { denied } => denied,
        };

        let steered = agent_loop.at_batch_boundary(turn, denied).await;
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

/// The most requests with tools offered that one turn of an agent's session of `kind` may make
/// (PLAN section 6.3): `max_iterations_jobs` for job sessions, `max_iterations` (none by default)
/// for the others.
fn iteration_cap(agent: &AgentConfig, kind: SessionKind) -> Option<u32> {
    match kind {
        SessionKind::Job => Some(agent.max_iterations_jobs),
        _ => agent.max_iterations,
    }
}

/// Makes one provider request and reads its stream to the end, recording its usage once the
/// stream has started (a request refused before streaming generated nothing and records none).
pub(super) async fn receive(
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

/// How a batch of calls ended.
enum BatchEnd {
    /// [`Turn::cancel`] fired: the calls still running were cancelled (the dispatcher's stream
    /// was dropped, which also cancels their approvals) and their slots are left for the caller.
    Cancelled,
    /// Every call was answered. `denied`: a message from the requester denied a waiting
    /// approval, so it joins the turn at the boundary in either busy mode.
    Done { denied: bool },
}

/// Runs the calls of one assistant message, filling each slot as its result arrives (each media
/// block of a result after its reference, [`media::with_references`]). The requester's new
/// messages are only looked at here
/// ([`Steering::peek`](crate::sessions::Steering::peek)); they join the turn at the boundary, so
/// a turn stopped before then leaves them queued for the next one.
///
/// - With `busy_mode = "steer"`, once the requester has a message queued the calls that haven't
///   started yet are skipped (the dispatcher's `skip` signal); running calls finish.
/// - In either mode, a message that arrives while an approval of the turn waits denies every
///   waiting approval ("Not approved. The user said: ..."). Messages already queued when the
///   batch started, or seen while no approval waited, don't.
#[allow(clippy::too_many_arguments)]
async fn run_tools(
    agent_loop: &AgentLoop,
    turn: &Turn,
    prompt: &SessionPrompt,
    context: &CallContexts,
    approvals: &Arc<TurnApprovals>,
    media: Arc<SessionMedia>,
    calls: Vec<ToolRequest>,
    slots: &[Slot],
) -> Result<BatchEnd, TurnError> {
    debug_assert!(
        calls
            .iter()
            .map(|c| &c.id)
            .eq(slots.iter().map(|s| &s.tool_use_id))
    );
    let steer = turn.busy_mode == BusyMode::Steer;
    let skip = CancellationToken::new();
    // The newest queued requester row looked at so far: later ones are new.
    let mut seen = newest(&peek_steering(turn).await);
    let mut denied = false;
    let mut filled = vec![false; calls.len()];
    let mut outputs = agent_loop.dispatcher.dispatch(ToolBatch {
        calls,
        tools: prompt.tools.clone(),
        context: context.call.clone(),
        media,
        turn: context.turn.clone(),
        skip: skip.clone(),
        approvals: approvals.clone(),
    });
    loop {
        // `biased`: a message that arrived while the model was answering skips every call
        // before one starts.
        let next = tokio::select! {
            biased;
            () = turn.cancel.cancelled() => return Ok(BatchEnd::Cancelled),
            () = turn.steering.changed() => {
                // A wake-up may be spurious: act only on messages actually queued.
                let queued = peek_steering(turn).await;
                if steer && !queued.is_empty() && !skip.is_cancelled() {
                    debug!(session = %turn.session.id, messages = queued.len(), "skipping unstarted calls to read a new message");
                    skip.cancel();
                }
                let new: Vec<&TurnMessage> = queued
                    .iter()
                    .filter(|m| seen.is_none_or(|seen| m.inbox > seen))
                    .collect();
                if !new.is_empty() && approvals.has_pending() {
                    let said: Vec<&str> = new
                        .iter()
                        .map(|m| m.message.text.trim())
                        .filter(|text| !text.is_empty())
                        .collect();
                    approvals.deny_pending(&said.join("\n"));
                    denied = true;
                }
                seen = seen.max(newest(&queued));
                continue;
            }
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
                        media::with_references(output.content),
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
    Ok(BatchEnd::Done { denied })
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

/// What a turn's tool calls see of it, built once per turn.
struct CallContexts {
    /// For toolsets and core tools.
    call: CallContext,
    /// For core tools.
    turn: Arc<TurnContext>,
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

/// The requester's queued messages, taken into the turn ([`crate::sessions::Steering::take`]).
/// A database error is logged and takes nothing: the messages stay queued for the next turn.
pub(super) async fn take_steering(turn: &Turn) -> Vec<TurnMessage> {
    match turn.steering.take().await {
        Ok(messages) => messages,
        Err(error) => {
            error!(session = %turn.session.id, %error, "cannot take the requester's new messages");
            Vec::new()
        }
    }
}

/// The requester's queued messages, left queued ([`crate::sessions::Steering::peek`]). A
/// database error is logged and reads nothing.
async fn peek_steering(turn: &Turn) -> Vec<TurnMessage> {
    match turn.steering.peek().await {
        Ok(messages) => messages,
        Err(error) => {
            error!(session = %turn.session.id, %error, "cannot read the requester's new messages");
            Vec::new()
        }
    }
}

/// The newest inbox row among `messages`.
fn newest(messages: &[TurnMessage]) -> Option<InboxId> {
    messages.iter().map(|m| m.inbox).max()
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
pub(super) async fn or_cancel<F: Future>(
    cancel: &CancellationToken,
    future: F,
) -> Option<F::Output> {
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
