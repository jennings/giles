//! Approvals (PLAN section 6.4): a call to a tool matching the agent's `ask` globs, and a
//! question a tool asks mid-call (MCP elicitation), wait for the requester's `yes`, `yes!`, or
//! `no`. Design: `Docs/technical/approvals.md`.
//!
//! [`ApprovalBoard`] holds every pending approval in memory, keyed by a short numeric code
//! unique among the pending and recently closed ones. It is the [`sessions::Approvals`] the
//! inbound pipeline asks whether a `yes`/`no` is an approval reply and hands the reply to.
//! [`TurnApprovals`] is one turn's side: the dispatcher asks it before a call runs
//! ([`TurnApprovals::approve_call`]) and when a call asks a question
//! ([`TurnApprovals::answer_question`]); the agent loop denies a turn's pending approvals when
//! the requester sends another message ([`TurnApprovals::deny_pending`]). Prompts go out through
//! an [`ApprovalPresenter`]: [`ChannelPresenter`] (the daemon's) shows the web channel's buttons
//! in web chats and sends the prompt text everywhere else ([`TextPresenter`]).
//!
//! Nothing is stored: a restart forgets every pending approval (startup recovery fills its
//! call's slot with `interrupted`).
//!
//! [`sessions::Approvals`]: crate::sessions::Approvals

mod prompt;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use giles_store::{SessionId, SessionKey, SessionKind};
use giles_types::{ApprovalDecision, UserId};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tracing::{debug, info, warn};

pub use prompt::{
    DIGEST_PREFIX_HEX, MAX_SHOWN_CHARS, call_prompt, code_span, escape, question_prompt, shown,
};

use crate::agent_loop::{NOTICE_TIMEOUT, glob};
use crate::commands::{Answer, ApprovalReply};
use crate::config::AgentConfig;
use crate::registry::Registry;
use crate::router::Routed;
use crate::sessions::{Reply, Turn};

/// The result of a call the requester answered `no` (PLAN section 6.4).
pub const DENIED_RESULT: &str = "denied";

/// The result of an `ask` call in a turn nobody attends: a job run (PLAN section 6.4).
pub const UNATTENDED_RESULT: &str = "denied (unattended)";

/// The result of a call whose approval request expired unanswered.
pub const EXPIRED_RESULT: &str = "denied (the approval request expired)";

/// How the result of a call denied by a new message from the requester starts; the message's
/// text follows.
pub const NOT_APPROVED_PREFIX: &str = "Not approved. The user said: ";

/// The most characters of the requester's message a "Not approved" result quotes (the model
/// sees the whole message right after the results).
pub const MAX_QUOTED_CHARS: usize = 300;

/// How long a closed approval's code is remembered, so a late reply to it is told what became
/// of it instead of reaching the agent, and the code isn't reused meanwhile.
pub const CLOSED_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// How an approval ended, for a presenter that shows it (the web channel's buttons).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalOutcome {
    /// Allowed once (`yes`).
    Allowed,
    /// Allowed, and the tool for the rest of the session (`yes!`).
    AllowedForSession,
    /// Denied: `no`, a new message from the requester while it was pending (PLAN section 6.4),
    /// or the request was cancelled (`/stop`).
    Denied,
    /// Not answered within the agent's `approval_timeout`.
    Expired,
}

/// Shows approval prompts in a conversation.
#[async_trait]
pub trait ApprovalPresenter: Send + Sync {
    /// Shows `markdown`, the prompt of approval `code`, in `reply`'s conversation.
    ///
    /// # Errors
    ///
    /// The prompt couldn't be shown; the request is then denied at once.
    async fn present(&self, reply: &Reply, code: &str, markdown: &str) -> giles_types::Result<()>;

    /// Approval `code`, shown in `reply`'s conversation, ended with `outcome`. Must not block.
    fn resolved(&self, reply: &Reply, code: &str, outcome: ApprovalOutcome);
}

/// Sends the prompt text into the conversation; the outcome isn't shown.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextPresenter;

#[async_trait]
impl ApprovalPresenter for TextPresenter {
    async fn present(&self, reply: &Reply, _code: &str, markdown: &str) -> giles_types::Result<()> {
        reply.send(markdown).await.map(drop)
    }

    fn resolved(&self, _reply: &Reply, _code: &str, _outcome: ApprovalOutcome) {}
}

/// The daemon's presenter: in a conversation of the web channel, the prompt is shown with
/// Allow / Allow for this session / Deny buttons ([`WebChannel::send_approval`]) and retired
/// with its outcome ([`WebChannel::resolve_approval`]); on every other channel it's the text
/// ([`TextPresenter`]), outcome unshown.
///
/// [`WebChannel::send_approval`]: crate::channels::web::WebChannel::send_approval
/// [`WebChannel::resolve_approval`]: crate::channels::web::WebChannel::resolve_approval
#[derive(Debug)]
pub struct ChannelPresenter {
    #[cfg_attr(not(feature = "channel-web"), allow(dead_code))]
    registry: Arc<Registry>,
}

impl ChannelPresenter {
    /// Finds the web channel in `registry` when a prompt is shown.
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }

    /// The web channel, if `reply` goes to it.
    #[cfg(feature = "channel-web")]
    fn web(&self, reply: &Reply) -> Option<Arc<crate::channels::web::WebChannel>> {
        self.registry
            .web_channel()
            .filter(|web| web.instance().as_str() == reply.instance())
    }
}

#[async_trait]
impl ApprovalPresenter for ChannelPresenter {
    async fn present(&self, reply: &Reply, code: &str, markdown: &str) -> giles_types::Result<()> {
        #[cfg(feature = "channel-web")]
        if let Some(web) = self.web(reply) {
            return web
                .send_approval(reply.conversation(), code, markdown)
                .map(drop);
        }
        TextPresenter.present(reply, code, markdown).await
    }

    fn resolved(&self, reply: &Reply, code: &str, outcome: ApprovalOutcome) {
        #[cfg(feature = "channel-web")]
        if let Some(web) = self.web(reply)
            && !web.resolve_approval(reply.conversation(), code, outcome)
        {
            debug!(code, "the web chat no longer shows the approval prompt");
        }
        #[cfg(not(feature = "channel-web"))]
        let _ = (reply, code, outcome);
    }
}

/// Sends approval prompts to a user who isn't in the turn's conversation: a job turn's delivery
/// user, through the outbox (PLAN sections 6.4, 6.11). Answers still come through the normal
/// path, from the target's conversation.
#[async_trait]
pub trait PromptOutbox: fmt::Debug + Send + Sync {
    /// Queues `markdown`, the prompt of approval `code`, to expire after `timeout` (when the
    /// approval does).
    ///
    /// # Errors
    ///
    /// The prompt couldn't be queued; the request is then denied at once.
    async fn send(&self, code: &str, markdown: &str, timeout: Duration) -> Result<(), String>;

    /// Approval `code` ended: withdraws its prompt if it hasn't gone out yet. Must not block.
    fn withdraw(&self, code: &str);
}

/// Who asks for approvals and answers them: the requester in the session's conversation, or
/// for a job with `ask_to = "deliver_to"`, the delivery user in their DM.
#[derive(Debug, Clone)]
pub struct ApprovalTarget {
    /// The conversation's session key; replies are accepted only from it.
    pub key: SessionKey,
    /// The user who answers.
    pub user: UserId,
    /// That conversation: where the prompt goes (unless `outbox` is set) and where answers to
    /// replies about closed or unknown codes go.
    pub reply: Reply,
    /// Set for a job turn: the prompt goes through it instead of `reply` and the presenter.
    pub outbox: Option<Arc<dyn PromptOutbox>>,
}

impl ApprovalTarget {
    fn answers(&self, to: &Routed) -> bool {
        self.key == to.session && self.user == to.user
    }
}

/// Every pending approval, in memory. Cheap to share as `Arc<ApprovalBoard>`.
pub struct ApprovalBoard {
    presenter: Arc<dyn ApprovalPresenter>,
    state: Mutex<State>,
    turns: AtomicU64,
}

impl fmt::Debug for ApprovalBoard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock(&self.state);
        f.debug_struct("ApprovalBoard")
            .field("pending", &state.pending.len())
            .field("closed", &state.closed.len())
            .finish_non_exhaustive()
    }
}

impl Default for ApprovalBoard {
    fn default() -> Self {
        Self::new(Arc::new(TextPresenter))
    }
}

#[derive(Default)]
struct State {
    pending: HashMap<String, Pending>,
    closed: HashMap<String, Closed>,
    /// Tools `yes!` allowed, by session.
    allowed: HashMap<SessionId, HashSet<String>>,
    /// Codes handed out when the system's random source failed.
    fallback: u32,
}

struct Pending {
    target: ApprovalTarget,
    session: SessionId,
    turn: u64,
    tool: String,
    /// A question a running call asked, rather than a call waiting to start.
    question: bool,
    decide: oneshot::Sender<Decision>,
}

struct Closed {
    target: ApprovalTarget,
    why: Closing,
    at: Instant,
}

/// Why an approval isn't pending any more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closing {
    Answered,
    Expired,
    Cancelled,
    Superseded,
}

/// What the waiting call is told.
#[derive(Debug)]
enum Decision {
    Allow,
    Deny,
    /// The requester sent this instead of answering.
    DenyBy(String),
}

/// How one wait ended.
#[derive(Debug)]
enum Waited {
    Decided(Decision),
    Expired,
    Unsent(String),
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ApprovalBoard {
    /// A board showing prompts through `presenter`.
    pub fn new(presenter: Arc<dyn ApprovalPresenter>) -> Self {
        Self {
            presenter,
            state: Mutex::default(),
            turns: AtomicU64::new(1),
        }
    }

    /// Registers an approval and returns its code and where its decision arrives.
    fn open(
        &self,
        target: &ApprovalTarget,
        session: SessionId,
        turn: u64,
        tool: &str,
        question: bool,
    ) -> (String, oneshot::Receiver<Decision>) {
        let mut state = lock(&self.state);
        state.prune();
        let code = state.fresh_code();
        let (decide, decided) = oneshot::channel();
        state.pending.insert(
            code.clone(),
            Pending {
                target: target.clone(),
                session,
                turn,
                tool: tool.to_owned(),
                question,
                decide,
            },
        );
        (code, decided)
    }

    /// Ends pending approval `code` for `why`, telling the presenter `outcome`. Nothing if it
    /// isn't pending any more.
    fn close(&self, code: &str, why: Closing, outcome: ApprovalOutcome) {
        let closed = lock(&self.state).close(code, why);
        if let Some(pending) = closed {
            self.resolved(&pending.target, code, outcome);
        }
    }

    /// Approval `code` of `target` ended with `outcome`: its prompt is withdrawn from the
    /// outbox, or the presenter is told.
    fn resolved(&self, target: &ApprovalTarget, code: &str, outcome: ApprovalOutcome) {
        match &target.outbox {
            Some(outbox) => outbox.withdraw(code),
            None => self.presenter.resolved(&target.reply, code, outcome),
        }
    }

    fn is_allowed(&self, session: SessionId, tool: &str) -> bool {
        lock(&self.state)
            .allowed
            .get(&session)
            .is_some_and(|tools| tools.contains(tool))
    }

    /// Denies every approval pending in `turn` with `said`.
    fn supersede(&self, turn: u64, said: &str) {
        let mut resolved = Vec::new();
        {
            let mut state = lock(&self.state);
            let codes: Vec<String> = state
                .pending
                .iter()
                .filter(|(_, p)| p.turn == turn)
                .map(|(code, _)| code.clone())
                .collect();
            for code in codes {
                if let Some(pending) = state.close(&code, Closing::Superseded) {
                    info!(
                        code,
                        tool = pending.tool,
                        "approval denied by a new message"
                    );
                    let _ = pending.decide.send(Decision::DenyBy(said.to_owned()));
                    resolved.push((pending.target, code));
                }
            }
        }
        for (target, code) in resolved {
            self.resolved(&target, &code, ApprovalOutcome::Denied);
        }
    }
}

impl State {
    fn codes_for(&self, to: &Routed) -> Vec<String> {
        let mut codes: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.target.answers(to))
            .map(|(code, _)| code.clone())
            .collect();
        codes.sort();
        codes
    }

    /// Forgets closed codes older than [`CLOSED_RETENTION`].
    fn prune(&mut self) {
        let now = Instant::now();
        self.closed
            .retain(|_, closed| now.duration_since(closed.at) < CLOSED_RETENTION);
    }

    /// A code no pending or remembered approval has: four digits, or six once half of the
    /// four-digit codes are taken.
    fn fresh_code(&mut self) -> String {
        let (low, span) = if self.pending.len() + self.closed.len() < 4_500 {
            (1_000, 9_000)
        } else {
            (100_000, 900_000)
        };
        loop {
            let mut bytes = [0_u8; 4];
            let random = if getrandom::fill(&mut bytes).is_ok() {
                u32::from_le_bytes(bytes)
            } else {
                self.fallback = self.fallback.wrapping_add(7_919);
                self.fallback
            };
            let code = (low + random % span).to_string();
            if !self.pending.contains_key(&code) && !self.closed.contains_key(&code) {
                return code;
            }
        }
    }

    fn close(&mut self, code: &str, why: Closing) -> Option<Pending> {
        let pending = self.pending.remove(code)?;
        self.closed.insert(
            code.to_owned(),
            Closed {
                target: pending.target.clone(),
                why,
                at: Instant::now(),
            },
        );
        Some(pending)
    }
}

#[async_trait]
impl crate::sessions::Approvals for ApprovalBoard {
    /// Whether a `yes`/`no` from `to` is an approval reply: when an approval is pending for
    /// them in that conversation (a missing or unknown code is then answered by
    /// [`answer`](Self::answer)), or `code` names one of theirs that closed recently (they're
    /// told what became of it).
    fn is_pending(&self, to: &Routed, code: Option<&str>) -> bool {
        let mut state = lock(&self.state);
        state.prune();
        state.pending.values().any(|p| p.target.answers(to))
            || code.is_some_and(|code| {
                state
                    .closed
                    .get(code)
                    .is_some_and(|closed| closed.target.answers(to))
            })
    }

    async fn answer(&self, to: Routed, reply: ApprovalReply) {
        let mut resolved = Vec::new();
        let mut respond = None;
        {
            let mut state = lock(&self.state);
            state.prune();
            let mine = state.codes_for(&to);
            let code = match &reply.code {
                Some(code) if mine.contains(code) => Some(code.clone()),
                Some(code) => {
                    if let Some(closed) = state.closed.get(code).filter(|c| c.target.answers(&to)) {
                        respond =
                            Some((closed.target.reply.clone(), closed_text(code, closed.why)));
                    } else if let Some(first) = mine.first() {
                        let text = format!(
                            "No approval request `{code}` is waiting for you (waiting: {}).",
                            list(&mine)
                        );
                        respond = Some((state.pending[first].target.reply.clone(), text));
                    }
                    None
                }
                None if mine.len() == 1 => Some(mine[0].clone()),
                None => {
                    if let Some(first) = mine.first() {
                        let text = format!(
                            "More than one approval request is waiting for you ({}). Reply with \
                             its code, e.g. `yes {first}`.",
                            list(&mine)
                        );
                        respond = Some((state.pending[first].target.reply.clone(), text));
                    }
                    None
                }
            };
            if let Some(code) = code
                && let Some(pending) = state.close(&code, Closing::Answered)
            {
                let (decision, outcome) = match reply.answer {
                    Answer::Yes => (Decision::Allow, ApprovalOutcome::Allowed),
                    Answer::YesForSession if pending.question => {
                        (Decision::Allow, ApprovalOutcome::Allowed)
                    }
                    Answer::YesForSession => (Decision::Allow, ApprovalOutcome::AllowedForSession),
                    Answer::No => (Decision::Deny, ApprovalOutcome::Denied),
                };
                info!(code, tool = pending.tool, ?outcome, "approval answered");
                if outcome == ApprovalOutcome::AllowedForSession {
                    state
                        .allowed
                        .entry(pending.session)
                        .or_default()
                        .insert(pending.tool.clone());
                    // Calls of the same tool waiting in the session are allowed too.
                    let same: Vec<String> = state
                        .pending
                        .iter()
                        .filter(|(_, p)| {
                            !p.question && p.session == pending.session && p.tool == pending.tool
                        })
                        .map(|(code, _)| code.clone())
                        .collect();
                    for code in same {
                        if let Some(other) = state.close(&code, Closing::Answered) {
                            let _ = other.decide.send(Decision::Allow);
                            resolved.push((other.target, code, outcome));
                        }
                    }
                }
                let _ = pending.decide.send(decision);
                resolved.push((pending.target, code, outcome));
            }
        }
        for (target, code, outcome) in resolved {
            self.resolved(&target, &code, outcome);
        }
        match respond {
            Some((reply, text)) => send_text(&reply, &text).await,
            None if reply.code.is_none() => {
                debug!(agent = %to.session.agent, "approval reply with nothing pending; ignored");
            }
            None => {}
        }
    }
}

/// `` `4821`, `1093` ``.
fn list(codes: &[String]) -> String {
    codes
        .iter()
        .map(|code| format!("`{code}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a reply to closed approval `code` is told.
fn closed_text(code: &str, why: Closing) -> String {
    match why {
        Closing::Answered => format!("Approval request `{code}` was already answered."),
        Closing::Expired => format!(
            "Approval request `{code}` expired and was denied. Ask again if you still want it."
        ),
        Closing::Cancelled => format!("Approval request `{code}` was cancelled."),
        Closing::Superseded => {
            format!("Approval request `{code}` was already denied by your message.")
        }
    }
}

async fn send_text(reply: &Reply, text: &str) {
    match tokio::time::timeout(NOTICE_TIMEOUT, reply.send(text)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            warn!(instance = reply.instance(), %error, "cannot answer an approval reply")
        }
        Err(_) => warn!(
            instance = reply.instance(),
            "answering an approval reply timed out"
        ),
    }
}

/// The result of a call denied because the requester said `said` (their messages' text)
/// instead of answering: `Not approved. The user said: <said>`, quoting at most
/// [`MAX_QUOTED_CHARS`] characters.
pub fn not_approved(said: &str) -> String {
    let said = said.trim();
    if said.is_empty() {
        return "Not approved. The user sent a new message.".to_owned();
    }
    let mut quoted: String = said.chars().take(MAX_QUOTED_CHARS).collect();
    if quoted.len() < said.len() {
        quoted.push('…');
    }
    format!("{NOT_APPROVED_PREFIX}{quoted}")
}

/// One turn's approvals: what needs approval, who answers, and the approvals pending now.
pub struct TurnApprovals {
    board: Arc<ApprovalBoard>,
    turn: u64,
    ask: Vec<String>,
    timeout: Duration,
    session: SessionId,
    /// `None`: nobody attends the turn, so every approval is denied.
    target: Option<ApprovalTarget>,
    pending: AtomicUsize,
}

impl fmt::Debug for TurnApprovals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnApprovals")
            .field("turn", &self.turn)
            .field("ask", &self.ask)
            .field("attended", &self.target.is_some())
            .finish_non_exhaustive()
    }
}

impl TurnApprovals {
    /// Approvals asked of `target` (`None`: denied as unattended) for calls of a turn in
    /// `session` to tools matching `ask`, each waiting at most `timeout`.
    pub fn new(
        board: Arc<ApprovalBoard>,
        session: SessionId,
        ask: Vec<String>,
        timeout: Duration,
        target: Option<ApprovalTarget>,
    ) -> Self {
        let turn = board.turns.fetch_add(1, Ordering::Relaxed);
        Self {
            board,
            turn,
            ask,
            timeout,
            session,
            target,
            pending: AtomicUsize::new(0),
        }
    }

    /// The approvals of `turn` of `agent`: its `ask` globs and `approval_timeout`, answered by
    /// the requester in the session's conversation. A job run (PLAN section 6.4) is answered
    /// only by the target the scheduler set (`ask_to = "deliver_to"`), else it's unattended, in
    /// a job session or a DM alike.
    pub fn for_turn(board: Arc<ApprovalBoard>, agent: &AgentConfig, turn: &Turn) -> Self {
        let target = match (
            &turn.job,
            &turn.session.kind,
            turn.session.key(),
            &turn.requester,
            &turn.reply,
        ) {
            (Some(job), ..) => job.approvals.clone(),
            (None, SessionKind::Job, ..) => None,
            (None, _, Some(key), Some(user), Some(reply)) => Some(ApprovalTarget {
                key,
                user: user.clone(),
                reply: reply.clone(),
                outbox: None,
            }),
            _ => None,
        };
        Self::new(
            board,
            turn.session.id,
            agent.ask.clone(),
            agent.approval_timeout,
            target,
        )
    }

    /// Whether a call to `tool` (with `input`) may run: at once if it doesn't match the `ask`
    /// globs or `yes!` allowed it in this session (`Ok(false)`); otherwise once the requester
    /// says `yes` (`Ok(true)`: it waited, so the caller should check again that the tool is
    /// still there).
    ///
    /// # Errors
    ///
    /// The denial, as the call's error result: [`DENIED_RESULT`], [`UNATTENDED_RESULT`],
    /// [`EXPIRED_RESULT`], [`not_approved`], or that the prompt couldn't be sent.
    pub async fn approve_call(&self, tool: &str, input: &Value) -> Result<bool, String> {
        if !glob::matches_any(&self.ask, tool) {
            return Ok(false);
        }
        if self.board.is_allowed(self.session, tool) {
            debug!(tool, "allowed for the session");
            return Ok(false);
        }
        let Some(target) = &self.target else {
            info!(tool, "approval denied: nobody attends the turn");
            return Err(UNATTENDED_RESULT.to_owned());
        };
        let input = input.to_string();
        match self
            .wait(target, tool, false, |code| call_prompt(tool, &input, code))
            .await
        {
            Waited::Decided(Decision::Allow) => Ok(true),
            Waited::Decided(Decision::Deny) => Err(DENIED_RESULT.to_owned()),
            Waited::Decided(Decision::DenyBy(said)) => Err(not_approved(&said)),
            Waited::Expired => Err(EXPIRED_RESULT.to_owned()),
            Waited::Unsent(error) => Err(format!(
                "{DENIED_RESULT} (the approval request could not be sent: {error})"
            )),
        }
    }

    /// The answer to `message`, a question a running call of `tool` asked (MCP elicitation):
    /// approved (without content: v1 answers only yes or no) once the requester says `yes`,
    /// denied otherwise, and at once when nobody attends the turn.
    pub async fn answer_question(&self, tool: &str, message: &str) -> ApprovalDecision {
        let Some(target) = &self.target else {
            info!(tool, "question denied: nobody attends the turn");
            return ApprovalDecision::Deny;
        };
        match self
            .wait(target, tool, true, |code| {
                question_prompt(tool, message, code)
            })
            .await
        {
            Waited::Decided(Decision::Allow) => ApprovalDecision::Approve { content: None },
            _ => ApprovalDecision::Deny,
        }
    }

    /// Whether an approval of this turn is waiting for an answer.
    pub fn has_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst) > 0
    }

    /// Denies every approval of this turn that is waiting, because the requester sent
    /// `said` instead of answering: a call gets [`not_approved`]`(said)`, a question is
    /// declined.
    pub fn deny_pending(&self, said: &str) {
        self.board.supersede(self.turn, said);
    }

    /// Registers an approval, shows its prompt, and waits for the decision, at most the
    /// timeout. Dropping the future (`/stop`, the call's deadline) cancels the approval.
    async fn wait(
        &self,
        target: &ApprovalTarget,
        tool: &str,
        question: bool,
        prompt: impl FnOnce(&str) -> String,
    ) -> Waited {
        let (code, decided) = self
            .board
            .open(target, self.session, self.turn, tool, question);
        let _waiting = Waiting {
            approvals: self,
            code: &code,
        };
        self.pending.fetch_add(1, Ordering::SeqCst);
        info!(tool, code, question, "approval requested");
        let markdown = prompt(&code);
        let waited = tokio::time::timeout(self.timeout, async {
            let shown = match &target.outbox {
                Some(outbox) => outbox.send(&code, &markdown, self.timeout).await,
                None => self
                    .board
                    .presenter
                    .present(&target.reply, &code, &markdown)
                    .await
                    .map_err(|error| error.to_string()),
            };
            if let Err(error) = shown {
                warn!(tool, code, %error, "cannot send an approval prompt; denied");
                return Waited::Unsent(error);
            }
            match decided.await {
                Ok(decision) => Waited::Decided(decision),
                Err(_) => Waited::Decided(Decision::Deny),
            }
        })
        .await;
        waited.unwrap_or_else(|_| {
            info!(tool, code, "approval request expired");
            self.board
                .close(&code, Closing::Expired, ApprovalOutcome::Expired);
            Waited::Expired
        })
    }
}

/// One approval waiting in [`TurnApprovals::wait`]: leaves the pending count, and cancels the
/// approval if it's still pending (the wait was dropped or the prompt not sent).
struct Waiting<'a> {
    approvals: &'a TurnApprovals,
    code: &'a str,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.approvals.pending.fetch_sub(1, Ordering::SeqCst);
        self.approvals
            .board
            .close(self.code, Closing::Cancelled, ApprovalOutcome::Denied);
    }
}
