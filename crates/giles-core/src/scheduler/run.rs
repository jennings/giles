//! One run of a job (PLAN sections 6.4, 6.11): find the user's DM, wait for it to be idle if
//! the job says so, run the turn in a fresh job session or the DM, and deliver the reply.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use giles_store::{Entry, OutboxId, Session, SessionKey, StoredMessage};
use giles_types::{Block, Message, MessageMeta, Role, UserId};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::job::JobDef;
use super::{Context, Delivery, DeliveryKind, JobDelivery, in_active_hours};
use crate::agent_loop::history::{job_message, message_timezone};
use crate::approvals::{ApprovalTarget, PromptOutbox};
use crate::config::{AgentConfig, AskTo, JobPrompt, JobSession};
use crate::identity;
use crate::sessions::{ChannelLookup, Clock, JobTurn, Reply, Steering, Turn, TurnOutcome};

/// A reply of exactly this (surrounding white space aside) sends and stores nothing (PLAN
/// section 6.11).
pub const NO_REPLY: &str = "NO_REPLY";

/// What a job run said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobReply {
    /// The last response was exactly [`NO_REPLY`]: nothing is sent or stored.
    NoReply,
    /// No text at all; nothing to send.
    Empty,
    /// The text of every response, in order, paragraphs apart: what an attended turn would have
    /// shown.
    Text(String),
}

/// What the run's `messages` (a transcript, oldest first) said in the messages after seq
/// `after`: [`JobReply::NoReply`] if the last assistant message's text is exactly
/// [`NO_REPLY`], else the text of every assistant message.
pub fn job_reply(messages: &[StoredMessage], after: i64) -> JobReply {
    let texts: Vec<String> = messages
        .iter()
        .filter(|stored| stored.seq > after)
        .filter_map(|stored| match &stored.entry {
            Entry::Message(message) if message.role == Role::Assistant => Some(text_of(message)),
            _ => None,
        })
        .collect();
    if texts.last().is_some_and(|last| last.trim() == NO_REPLY) {
        return JobReply::NoReply;
    }
    let text = texts
        .iter()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.is_empty() {
        JobReply::Empty
    } else {
        JobReply::Text(text)
    }
}

/// A message's top-level `Text` blocks, paragraphs apart.
fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What introduces a job's prompt in its user message.
fn header(job: &JobDef) -> String {
    format!(
        "Scheduled job {}. Your reply is sent to the user; reply exactly {NO_REPLY} to send \
         nothing.",
        job.label()
    )
}

/// The latest DM messages an isolated run's prompt quotes (`dm_excerpt`): the text of each
/// user and assistant message, oldest first; tool calls and results are left out.
fn excerpt(messages: &[StoredMessage]) -> Option<String> {
    let lines: Vec<String> = messages
        .iter()
        .filter_map(|stored| match &stored.entry {
            Entry::Message(message) => {
                let text = text_of(message);
                let text = text.trim();
                if text.is_empty() {
                    return None;
                }
                let who = if message.role == Role::Assistant {
                    "You"
                } else {
                    "User"
                };
                Some(format!("**{who}:** {text}"))
            }
            Entry::EmptySlot { .. } => None,
        })
        .collect();
    (!lines.is_empty()).then(|| {
        format!(
            "The latest messages of your DM with the user, oldest first:\n\n{}",
            lines.join("\n\n")
        )
    })
}

/// Runs `job` of `agent` once. Returns `false` if `stop` or shutdown cut it off before it
/// started (its run isn't recorded).
pub(super) async fn run(
    context: &Context,
    job: &JobDef,
    agent: &AgentConfig,
    stop: &CancellationToken,
) -> bool {
    let label = job.label();
    let user = job.creator.clone().unwrap_or_else(identity::owner);
    let dm = match latest_dm(context, &job.agent, &user).await {
        Ok(dm) => dm,
        Err(error) => {
            error!(job = %label, %error, "cannot find the user's DM; the job runs without it");
            None
        }
    };
    if job.idle_only
        && let Some(dm) = &dm
    {
        tokio::select! {
            biased;
            () = stop.cancelled() => return false,
            () = context.sessions.wait_idle(dm.id) => {}
        }
    }
    if stop.is_cancelled() || context.shutdown.is_cancelled() {
        return false;
    }
    let now = context.clock.now();
    // The wait may have run past the end of the job's hours.
    let tz = message_timezone(&context.registry.config(), agent);
    if !in_active_hours(job.active_hours.as_ref(), &tz, now) {
        debug!(job = %label, "the DM went quiet after the job's active hours; skipped");
        return true;
    }
    if let Err(error) = context
        .store
        .job_set_last_run(job.agent.clone(), job.key.clone(), now)
        .await
    {
        error!(job = %label, %error, "cannot record the job's run; running it anyway");
    }
    info!(job = %label, agent = %job.agent, "job started");

    let prompt = match &job.prompt {
        JobPrompt::Inline(text) => text.clone(),
        // Read at each run, so an edit applies from the next one.
        JobPrompt::File(path) => match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(error) => {
                warn!(job = %label, path = %path.display(), %error, "cannot read the job's prompt file; skipped");
                return true;
            }
        },
    };
    let dm_key = dm.as_ref().and_then(Session::key);
    let approvals = match (job.ask_to, &dm_key) {
        (Some(AskTo::DeliverTo), Some(key)) => Some(ApprovalTarget {
            key: key.clone(),
            user: user.clone(),
            reply: Reply::for_key(key, context.registry.clone() as Arc<dyn ChannelLookup>),
            outbox: Some(Arc::new(OutboxPrompts::new(
                context.delivery.clone(),
                context.clock.clone(),
                user.clone(),
                label.clone(),
            ))),
        }),
        (Some(_), None) => {
            info!(job = %label, "no DM with the user to take approval replies; ask tools are denied");
            None
        }
        _ => None,
    };
    let run = Run {
        context,
        job,
        agent,
        user,
        prompt,
        approvals,
        stop,
    };
    match (job.session, dm_key) {
        (JobSession::Dm, Some(key)) => run.in_dm(key).await,
        (JobSession::Dm, None) => {
            info!(job = %label, "no DM with the user yet; the job runs in a session of its own");
            run.isolated(None).await;
        }
        (JobSession::Isolated, _) => run.isolated(dm).await,
    }
    true
}

/// The user's DM with `agent` on their default identity: the direct session with the newest
/// activity on a channel instance that owns the identity, open or ended. `None` if they have
/// none yet, or no identity (v1 has only the owner's).
async fn latest_dm(
    context: &Context,
    agent: &giles_types::AgentId,
    user: &UserId,
) -> Result<Option<Session>, giles_store::Error> {
    if *user != identity::owner() {
        return Ok(None);
    }
    let config = context.registry.config();
    let identity = config.owner.default.as_str();
    let instances = owning_instances(
        identity,
        config.channels.iter().map(|(name, channel)| {
            let info = context
                .registry
                .channel(name.as_str())
                .map(|c| c.info().clone());
            (name.to_string(), channel.component.clone(), info)
        }),
    );
    if instances.is_empty() {
        return Ok(None);
    }
    context
        .store
        .latest_direct_session(agent.clone(), user.clone(), instances)
        .await
}

/// The channel instances that can own `identity` (`<namespace>:<native id>`), given each
/// configured instance's name, component, and registered channel's info: those whose channel
/// namespace followed by `:` starts the identity with a native id left, and those with no
/// channel registered whose component is the identity's first segment (as the outbox routes).
pub(crate) fn owning_instances(
    identity: &str,
    instances: impl IntoIterator<Item = (String, String, Option<giles_types::ChannelInfo>)>,
) -> Vec<String> {
    let component = identity.split(':').next().unwrap_or(identity);
    instances
        .into_iter()
        .filter(|(_, instance_component, info)| match info {
            Some(info) => identity
                .strip_prefix(info.identity_namespace.as_str())
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|native| !native.is_empty()),
            None => instance_component == component,
        })
        .map(|(name, _, _)| name)
        .collect()
}

/// One run, once its prompt and approvals are settled.
struct Run<'a> {
    context: &'a Context,
    job: &'a JobDef,
    agent: &'a AgentConfig,
    user: UserId,
    prompt: String,
    approvals: Option<ApprovalTarget>,
    /// Cancelled at shutdown and when the job is cancelled or deleted: ends the run's waits
    /// before its turn (for the DM's hold). A started turn isn't cut off by it.
    stop: &'a CancellationToken,
}

impl Run<'_> {
    fn label(&self) -> String {
        self.job.label()
    }

    /// The turn's prompt message: the header, the excerpt if any, and the prompt.
    fn prompt_message(&self, excerpt: Option<String>) -> Message {
        let mut text = header(self.job);
        if let Some(excerpt) = excerpt {
            text.push_str("\n\n");
            text.push_str(&excerpt);
        }
        text.push_str("\n\n");
        text.push_str(self.prompt.trim());
        let config = self.context.registry.config();
        let tz = message_timezone(&config, self.agent);
        job_message(
            &text,
            self.context.clock.now(),
            self.job.creator.as_ref(),
            &tz,
        )
    }

    fn turn(&self, session: Session, cancel: CancellationToken, prompt: Message) -> Turn {
        Turn {
            session,
            requester: self.job.creator.clone(),
            messages: Vec::new(),
            busy_mode: self.agent.busy_mode,
            cancel,
            steering: Steering::detached(),
            reply: None,
            job: Some(JobTurn {
                prompt,
                model: self.job.model.clone(),
                approvals: self.approvals.clone(),
            }),
        }
    }

    /// An isolated run: a fresh job session owned by the user (so their memory is in its
    /// prompt), with the DM's latest messages if the job asks; the reply is delivered and
    /// appended to the DM.
    async fn isolated(self, dm: Option<Session>) {
        let label = self.label();
        let store = &self.context.store;
        let excerpt = match (self.job.dm_excerpt, &dm) {
            (Some(count), Some(dm)) => match store.recent_messages(dm.id, count).await {
                Ok(messages) => excerpt(&messages),
                Err(error) => {
                    warn!(job = %label, %error, "cannot read the DM excerpt; running without it");
                    None
                }
            },
            _ => None,
        };
        let now = self.context.clock.now();
        let session = match store
            .create_job_session(self.job.agent.clone(), Some(self.user.clone()), now)
            .await
        {
            Ok(session) => session,
            Err(error) => {
                error!(job = %label, %error, "cannot create the job's session");
                return;
            }
        };
        let id = session.id;
        let prompt = self.prompt_message(excerpt);
        // Once started, the turn ends only at shutdown: a job that cancels itself (its `stop`)
        // still delivers this run's reply.
        let turn = self.turn(session, self.context.shutdown.child_token(), prompt);
        let outcome = self.context.runner.run_turn(turn).await;
        if let Err(error) = store.end_session(id, self.context.clock.now()).await {
            warn!(job = %label, session = %id, %error, "cannot end the job's session");
        }
        if !self.finished(&outcome) {
            return;
        }
        match self.reply(id, 0).await {
            Some(JobReply::NoReply) => {
                debug!(job = %label, "NO_REPLY: nothing sent or kept");
                if let Err(error) = store.delete_session(id).await {
                    warn!(job = %label, session = %id, %error, "cannot delete the job's session");
                }
            }
            Some(JobReply::Text(text)) => {
                self.deliver(&text).await;
                if let Some(key) = dm.as_ref().and_then(Session::key) {
                    self.append_to_dm(key, text).await;
                }
            }
            Some(JobReply::Empty) | None => {}
        }
    }

    /// A `dm` run: its own turn in the user's DM session, after the turns ahead of it. The
    /// reply is in the DM's transcript already and is delivered; a `NO_REPLY` run is removed
    /// from the transcript.
    async fn in_dm(self, key: SessionKey) {
        let label = self.label();
        let store = &self.context.store;
        let Some(hold) = self.hold_dm(key, true).await else {
            return;
        };
        let session = hold.session().clone();
        let id = session.id;
        let before = match store.last_seq(id).await {
            Ok(seq) => seq,
            Err(error) => {
                error!(job = %label, %error, "cannot read the DM's transcript; skipped");
                return;
            }
        };
        let prompt = self.prompt_message(None);
        // Once started, the turn ends only with `/stop` in the DM or shutdown (the hold's token):
        // a job that cancels itself still delivers this run's reply.
        let turn = self.turn(session, hold.cancel().clone(), prompt);
        let outcome = self.context.runner.run_turn(turn).await;
        if !self.finished(&outcome) {
            return;
        }
        match self.reply(id, before).await {
            Some(JobReply::NoReply) => {
                debug!(job = %label, "NO_REPLY: nothing sent or kept");
                if let Err(error) = store.truncate_messages(id, before).await {
                    warn!(job = %label, session = %id, %error, "cannot remove the job's turn from the DM");
                }
            }
            Some(JobReply::Text(text)) => self.deliver(&text).await,
            Some(JobReply::Empty) | None => {}
        }
        drop(hold);
    }

    /// Holds the user's open DM session for `key` (opened if needed), after the turns ahead.
    /// `None` at shutdown, and with `until_stopped`, if the run is stopped while it waits.
    async fn hold_dm(
        &self,
        key: SessionKey,
        until_stopped: bool,
    ) -> Option<crate::sessions::SessionHold> {
        let session = match self
            .context
            .sessions
            .open_direct(key, self.user.clone())
            .await
        {
            Ok(session) => session,
            Err(error) => {
                error!(job = %self.label(), %error, "cannot open the user's DM session");
                return None;
            }
        };
        let hold = self.context.sessions.hold(session.id);
        if !until_stopped {
            return hold.await;
        }
        tokio::select! {
            biased;
            () = self.stop.cancelled() => None,
            hold = hold => hold,
        }
    }

    /// Whether the turn completed; otherwise logs how it ended. Only a completed turn's reply
    /// is delivered.
    fn finished(&self, outcome: &TurnOutcome) -> bool {
        match outcome {
            TurnOutcome::Completed => true,
            TurnOutcome::Cancelled => {
                info!(job = %self.label(), "job turn cancelled; nothing delivered");
                false
            }
            other => {
                warn!(job = %self.label(), outcome = ?other, "job turn failed; nothing delivered");
                false
            }
        }
    }

    async fn reply(&self, session: giles_store::SessionId, after: i64) -> Option<JobReply> {
        match self.context.store.load_history(session).await {
            Ok(history) => Some(job_reply(&history.messages, after)),
            Err(error) => {
                error!(job = %self.label(), %error, "cannot read the job's reply");
                None
            }
        }
    }

    async fn deliver(&self, text: &str) {
        let delivery = Delivery {
            user: self.user.clone(),
            kind: DeliveryKind::JobReply,
            markdown: text.to_owned(),
            expires_at: None,
        };
        match self.context.delivery.enqueue(delivery).await {
            Ok(id) => info!(job = %self.label(), outbox = %id, "job reply queued"),
            Err(reason) => warn!(job = %self.label(), %reason, "cannot queue the job's reply"),
        }
    }

    /// Appends `text` to the user's DM as an assistant message, between its turns, so a reply
    /// to it has context.
    async fn append_to_dm(&self, key: SessionKey, text: String) {
        // Not stopped by `stop`: a delivered reply always reaches the DM.
        let Some(hold) = self.hold_dm(key, false).await else {
            return;
        };
        let message = Message {
            role: Role::Assistant,
            content: vec![Block::text(text)],
            meta: MessageMeta::new(self.context.clock.now()),
        };
        if let Err(error) = self
            .context
            .store
            .append_messages(hold.session().id, vec![message])
            .await
        {
            warn!(job = %self.label(), %error, "cannot append the job's reply to the DM");
        }
    }
}

/// Approval prompts of a job turn with `ask_to = "deliver_to"`: queued for the user's default
/// identity, expiring with the approval, and withdrawn when it resolves first.
pub(super) struct OutboxPrompts {
    delivery: Arc<dyn JobDelivery>,
    clock: Arc<dyn Clock>,
    user: UserId,
    job: String,
    queued: Mutex<HashMap<String, OutboxId>>,
}

impl OutboxPrompts {
    pub(super) fn new(
        delivery: Arc<dyn JobDelivery>,
        clock: Arc<dyn Clock>,
        user: UserId,
        job: String,
    ) -> Self {
        Self {
            delivery,
            clock,
            user,
            job,
            queued: Mutex::default(),
        }
    }
}

impl fmt::Debug for OutboxPrompts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboxPrompts")
            .field("user", &self.user)
            .field("job", &self.job)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl PromptOutbox for OutboxPrompts {
    async fn send(&self, code: &str, markdown: &str, timeout: Duration) -> Result<(), String> {
        let expires_at = self
            .clock
            .now()
            .checked_add(timeout)
            .map_err(|error| error.to_string())?;
        let delivery = Delivery {
            user: self.user.clone(),
            kind: DeliveryKind::ApprovalPrompt,
            markdown: format!("Scheduled job {}: {markdown}", self.job),
            expires_at: Some(expires_at),
        };
        let id = self.delivery.enqueue(delivery).await?;
        self.queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(code.to_owned(), id);
        Ok(())
    }

    fn withdraw(&self, code: &str) {
        let id = self
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(code);
        if let Some(id) = id {
            let delivery = self.delivery.clone();
            tokio::spawn(async move { delivery.remove(id).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use giles_store::StoredMessageId;
    use giles_types::{ChannelInfo, OutputMode, Timestamp};

    use super::*;

    fn stored(seq: i64, role: Role, content: Vec<Block>) -> StoredMessage {
        StoredMessage {
            id: StoredMessageId::new(seq),
            seq,
            entry: Entry::Message(Message {
                role,
                content,
                meta: MessageMeta::new(Timestamp::UNIX_EPOCH),
            }),
        }
    }

    fn said(seq: i64, role: Role, text: &str) -> StoredMessage {
        stored(seq, role, vec![Block::text(text)])
    }

    fn call(seq: i64) -> StoredMessage {
        stored(
            seq,
            Role::Assistant,
            vec![
                Block::text("Let me look."),
                Block::ToolUse {
                    id: "t1".into(),
                    name: "github__list".into(),
                    input: serde_json::json!({}),
                },
            ],
        )
    }

    fn result(seq: i64) -> StoredMessage {
        stored(
            seq,
            Role::User,
            vec![Block::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![Block::text("none")],
                is_error: false,
            }],
        )
    }

    #[test]
    fn the_reply_is_every_response_and_no_reply_is_decided_by_the_last() {
        let earlier = said(1, Role::Assistant, "Earlier, before the job.");
        let prompt = said(2, Role::User, "Check in.");
        let run = [
            earlier.clone(),
            prompt.clone(),
            call(3),
            result(4),
            said(5, Role::Assistant, "All quiet.\n"),
        ];
        assert_eq!(
            job_reply(&run, 2),
            JobReply::Text("Let me look.\n\nAll quiet.".into())
        );
        // Narration before the last response doesn't stop a NO_REPLY.
        let quiet = [
            prompt.clone(),
            call(3),
            result(4),
            said(5, Role::Assistant, " NO_REPLY\n"),
        ];
        assert_eq!(job_reply(&quiet, 0), JobReply::NoReply);
        // Not exactly NO_REPLY: delivered.
        let chatty = [
            prompt.clone(),
            said(3, Role::Assistant, "NO_REPLY, nothing new"),
        ];
        assert_eq!(
            job_reply(&chatty, 0),
            JobReply::Text("NO_REPLY, nothing new".into())
        );
        // Messages up to `after` (the DM before a `dm` run) don't count.
        assert_eq!(job_reply(&[earlier, prompt], 1), JobReply::Empty);
    }

    #[test]
    fn an_excerpt_quotes_the_text_of_each_message() {
        let messages = [
            result(1),
            said(2, Role::User, "[Thu 2026-10-22 14:03 PDT] Any plans?"),
            call(3),
            result(4),
            said(5, Role::Assistant, "Tea at four."),
        ];
        assert_eq!(
            excerpt(&messages).unwrap(),
            "The latest messages of your DM with the user, oldest first:\n\n\
             **User:** [Thu 2026-10-22 14:03 PDT] Any plans?\n\n\
             **You:** Let me look.\n\n\
             **You:** Tea at four."
        );
        assert_eq!(excerpt(&[result(1)]), None);
    }

    #[test]
    fn identities_belong_to_instances_by_namespace_or_component() {
        let info = |namespace: &str| {
            Some(ChannelInfo {
                identity_namespace: namespace.to_owned(),
                output: OutputMode::Blocks,
                can_initiate: true,
            })
        };
        let instances = || {
            vec![
                (
                    "irc-libera".to_owned(),
                    "irc".to_owned(),
                    info("irc:libera.chat"),
                ),
                (
                    "irc-oftc".to_owned(),
                    "irc".to_owned(),
                    info("irc:oftc.net"),
                ),
                ("irc-new".to_owned(), "irc".to_owned(), None),
                (
                    "whatsapp".to_owned(),
                    "whatsapp".to_owned(),
                    info("whatsapp"),
                ),
            ]
        };
        assert_eq!(
            owning_instances("irc:libera.chat:account:stephen", instances()),
            ["irc-libera", "irc-new"]
        );
        assert_eq!(
            owning_instances("whatsapp:15551234567", instances()),
            ["whatsapp"]
        );
        // A namespace must be followed by `:` and a native id.
        assert!(owning_instances("whatsappx:1", instances()).is_empty());
        assert!(owning_instances("whatsapp:", instances()).is_empty());
        assert!(owning_instances("email:me@example.com", instances()).is_empty());
    }
}
