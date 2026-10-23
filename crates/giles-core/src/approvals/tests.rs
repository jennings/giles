//! The approval board and a turn's approvals, on paused Tokio time, with a fake channel. The
//! whole path through the session manager and the agent loop is in `tests/approvals.rs`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use giles_store::{Session, SessionId, SessionKind};
use giles_types::{
    AgentId, Channel, ChannelEvent, ChannelInfo, ConversationId, EventId, MessageId,
    OutboundMessage, OutputMode, SendTarget,
};
use serde_json::json;

use super::*;
use crate::commands::{Answer, ApprovalReply};
use crate::sessions::{Approvals, ChannelLookup};

/// Records what it sends, as `<conversation>: <markdown>`.
struct FakeChannel {
    info: ChannelInfo,
    sent: Mutex<Vec<String>>,
}

impl Default for FakeChannel {
    fn default() -> Self {
        Self {
            info: ChannelInfo {
                identity_namespace: "chat".to_owned(),
                output: OutputMode::Blocks,
                can_initiate: false,
            },
            sent: Mutex::default(),
        }
    }
}

#[async_trait]
impl Channel for FakeChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(
        &self,
    ) -> giles_types::Result<BoxStream<'static, giles_types::Result<ChannelEvent>>> {
        Ok(stream::pending().boxed())
    }

    async fn ack(&self, _event: &EventId) -> giles_types::Result<()> {
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> giles_types::Result<MessageId> {
        let SendTarget::Conversation(conversation) = msg.target else {
            panic!("not a conversation");
        };
        self.sent
            .lock()
            .unwrap()
            .push(format!("{conversation}: {}", msg.markdown));
        Ok(MessageId::new("m").unwrap())
    }
}

/// The `chat` instance, registered or not.
struct Lookup {
    channel: Arc<FakeChannel>,
    registered: bool,
}

impl ChannelLookup for Lookup {
    fn channel(&self, instance: &str) -> Option<Arc<dyn Channel>> {
        (self.registered && instance == "chat").then(|| self.channel.clone() as Arc<dyn Channel>)
    }
}

/// Records every outcome a presenter is told.
#[derive(Default)]
struct Recorder {
    outcomes: Mutex<Vec<(String, ApprovalOutcome)>>,
}

#[async_trait]
impl ApprovalPresenter for Recorder {
    async fn present(&self, reply: &Reply, code: &str, markdown: &str) -> giles_types::Result<()> {
        TextPresenter.present(reply, code, markdown).await
    }

    fn resolved(&self, _reply: &Reply, code: &str, outcome: ApprovalOutcome) {
        self.outcomes
            .lock()
            .unwrap()
            .push((code.to_owned(), outcome));
    }
}

fn owner() -> UserId {
    UserId::new("owner").unwrap()
}

fn session(id: i64, conversation: &str) -> Session {
    Session {
        id: SessionId::new(id),
        agent: AgentId::new("jeeves").unwrap(),
        kind: SessionKind::Direct,
        channel: Some("chat".to_owned()),
        conversation: Some(ConversationId::new(conversation).unwrap()),
        user: Some(owner()),
        started_at: "2026-10-22T21:00:00Z".parse().unwrap(),
        ended_at: None,
        last_message_at: None,
    }
}

fn routed(conversation: &str, user: &str) -> Routed {
    Routed {
        session: session(1, conversation).key().unwrap(),
        user: UserId::new(user).unwrap(),
    }
}

struct Setup {
    channel: Arc<FakeChannel>,
    recorder: Arc<Recorder>,
    board: Arc<ApprovalBoard>,
}

impl Setup {
    fn new() -> Self {
        let recorder = Arc::new(Recorder::default());
        Self {
            channel: Arc::default(),
            board: Arc::new(ApprovalBoard::new(recorder.clone())),
            recorder,
        }
    }

    /// A turn's approvals in session 1 (conversation `owner`), asking for `ask__*` tools.
    fn turn(&self, registered: bool) -> Arc<TurnApprovals> {
        let session = session(1, "owner");
        let lookup = Arc::new(Lookup {
            channel: self.channel.clone(),
            registered,
        });
        Arc::new(TurnApprovals::new(
            self.board.clone(),
            session.id,
            vec!["ask__*".to_owned()],
            Duration::from_secs(600),
            Some(ApprovalTarget {
                key: session.key().unwrap(),
                user: owner(),
                reply: Reply::for_session(&session, lookup).unwrap(),
                outbox: None,
            }),
        ))
    }

    fn sent(&self) -> Vec<String> {
        std::mem::take(&mut *self.channel.sent.lock().unwrap())
    }

    fn outcomes(&self) -> Vec<(String, ApprovalOutcome)> {
        std::mem::take(&mut *self.recorder.outcomes.lock().unwrap())
    }
}

/// Asks for `tool` in the background; the handle yields the decision.
fn ask(
    approvals: &Arc<TurnApprovals>,
    tool: &str,
) -> tokio::task::JoinHandle<Result<bool, String>> {
    let approvals = approvals.clone();
    let tool = tool.to_owned();
    tokio::spawn(async move { approvals.approve_call(&tool, &json!({"x": 1})).await })
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

/// The code of the one prompt sent.
fn code_of(sent: &[String]) -> String {
    assert_eq!(sent.len(), 1, "{sent:?}");
    sent[0]
        .rsplit_once("`no ")
        .unwrap()
        .1
        .trim_end_matches("`.")
        .to_owned()
}

fn reply(answer: Answer, code: Option<&str>) -> ApprovalReply {
    ApprovalReply {
        answer,
        code: code.map(str::to_owned),
    }
}

#[tokio::test(start_paused = true)]
async fn tools_outside_ask_run_at_once() {
    let s = Setup::new();
    let approvals = s.turn(true);
    assert_eq!(
        approvals.approve_call("other__x", &json!({})).await,
        Ok(false)
    );
    assert!(s.sent().is_empty());
}

#[tokio::test(start_paused = true)]
async fn only_the_requester_in_the_same_conversation_answers() {
    let s = Setup::new();
    let approvals = s.turn(true);
    let waiting = ask(&approvals, "ask__run");
    settle().await;
    let code = code_of(&s.sent());
    assert!(approvals.has_pending());

    let me = routed("owner", "owner");
    assert!(s.board.is_pending(&me, None));
    assert!(s.board.is_pending(&me, Some(&code)));
    // Another conversation or another person: an ordinary message.
    for other in [routed("elsewhere", "owner"), routed("owner", "guest")] {
        assert!(!s.board.is_pending(&other, None));
        assert!(!s.board.is_pending(&other, Some(&code)));
        s.board.answer(other, reply(Answer::Yes, Some(&code))).await;
    }
    settle().await;
    assert!(!waiting.is_finished());

    s.board.answer(me.clone(), reply(Answer::Yes, None)).await;
    assert_eq!(waiting.await.unwrap(), Ok(true));
    assert!(!approvals.has_pending());
    assert_eq!(s.outcomes(), [(code.clone(), ApprovalOutcome::Allowed)]);
    // Nothing pending: a bare `yes` is an ordinary message again.
    assert!(!s.board.is_pending(&me, None));
    assert!(s.sent().is_empty());
}

#[tokio::test(start_paused = true)]
async fn closed_codes_are_remembered_for_a_day_then_forgotten() {
    let s = Setup::new();
    let approvals = s.turn(true);
    let waiting = ask(&approvals, "ask__run");
    settle().await;
    let code = code_of(&s.sent());
    let me = routed("owner", "owner");
    s.board
        .answer(me.clone(), reply(Answer::No, Some(&code)))
        .await;
    assert_eq!(waiting.await.unwrap(), Err(DENIED_RESULT.to_owned()));
    assert_eq!(s.outcomes(), [(code.clone(), ApprovalOutcome::Denied)]);

    // A second answer is consumed and told.
    assert!(s.board.is_pending(&me, Some(&code)));
    s.board
        .answer(me.clone(), reply(Answer::Yes, Some(&code)))
        .await;
    assert_eq!(
        s.sent(),
        [format!(
            "owner: Approval request `{code}` was already answered."
        )]
    );

    tokio::time::sleep(CLOSED_RETENTION).await;
    assert!(!s.board.is_pending(&me, Some(&code)));
}

#[tokio::test(start_paused = true)]
async fn yes_bang_is_remembered_per_session() {
    let s = Setup::new();
    let approvals = s.turn(true);
    let waiting = ask(&approvals, "ask__run");
    settle().await;
    let code = code_of(&s.sent());
    s.board
        .answer(
            routed("owner", "owner"),
            reply(Answer::YesForSession, Some(&code)),
        )
        .await;
    assert_eq!(waiting.await.unwrap(), Ok(true));
    assert_eq!(s.outcomes(), [(code, ApprovalOutcome::AllowedForSession)]);

    // A later turn of the session: allowed without asking. Another tool still asks.
    let later = s.turn(true);
    assert_eq!(later.approve_call("ask__run", &json!({})).await, Ok(false));
    assert!(s.sent().is_empty());
    let other = ask(&later, "ask__other");
    settle().await;
    assert_eq!(s.sent().len(), 1);
    other.abort();

    // Another session asks again.
    let elsewhere = TurnApprovals::new(
        s.board.clone(),
        SessionId::new(2),
        vec!["ask__*".to_owned()],
        Duration::from_secs(600),
        None,
    );
    assert_eq!(
        elsewhere.approve_call("ask__run", &json!({})).await,
        Err(UNATTENDED_RESULT.to_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn an_expired_request_is_denied_and_reported() {
    let s = Setup::new();
    let approvals = s.turn(true);
    let waiting = ask(&approvals, "ask__run");
    settle().await;
    let code = code_of(&s.sent());
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(waiting.await.unwrap(), Err(EXPIRED_RESULT.to_owned()));
    assert_eq!(s.outcomes(), [(code, ApprovalOutcome::Expired)]);
    assert!(!approvals.has_pending());
}

/// Queues prompts (or refuses to), records withdrawals.
#[derive(Debug, Default)]
struct Outbox {
    refuse: bool,
    queued: Mutex<Vec<(String, String, Duration)>>,
    withdrawn: Mutex<Vec<String>>,
}

#[async_trait]
impl PromptOutbox for Outbox {
    async fn send(&self, code: &str, markdown: &str, timeout: Duration) -> Result<(), String> {
        if self.refuse {
            return Err("no channel for the identity".to_owned());
        }
        self.queued
            .lock()
            .unwrap()
            .push((code.to_owned(), markdown.to_owned(), timeout));
        Ok(())
    }

    fn withdraw(&self, code: &str) {
        self.withdrawn.lock().unwrap().push(code.to_owned());
    }
}

#[tokio::test(start_paused = true)]
async fn a_job_targets_prompts_go_through_the_outbox_and_are_withdrawn_when_they_end() {
    let s = Setup::new();
    let for_outbox = |outbox: Arc<Outbox>| {
        let session = session(1, "owner");
        let lookup = Arc::new(Lookup {
            channel: s.channel.clone(),
            registered: true,
        });
        Arc::new(TurnApprovals::new(
            s.board.clone(),
            session.id,
            vec!["ask__*".to_owned()],
            Duration::from_secs(600),
            Some(ApprovalTarget {
                key: session.key().unwrap(),
                user: owner(),
                reply: Reply::for_session(&session, lookup).unwrap(),
                outbox: Some(outbox),
            }),
        ))
    };

    // Queued with the approval's timeout, not sent into the conversation; the presenter isn't
    // told how it ended, the outbox withdraws the prompt.
    let outbox = Arc::new(Outbox::default());
    let approvals = for_outbox(outbox.clone());
    let waiting = ask(&approvals, "ask__run");
    settle().await;
    let (code, markdown, timeout) = outbox.queued.lock().unwrap()[0].clone();
    assert!(markdown.contains(&format!("`yes {code}`")), "{markdown}");
    assert_eq!(timeout, Duration::from_secs(600));
    assert!(s.sent().is_empty());
    let me = routed("owner", "owner");
    assert!(s.board.is_pending(&me, Some(&code)));
    s.board.answer(me, reply(Answer::Yes, Some(&code))).await;
    assert_eq!(waiting.await.unwrap(), Ok(true));
    assert_eq!(*outbox.withdrawn.lock().unwrap(), [code]);
    assert!(s.outcomes().is_empty());

    // Expired: withdrawn too.
    let waiting = ask(&approvals, "ask__run");
    settle().await;
    let code = outbox.queued.lock().unwrap()[1].0.clone();
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(waiting.await.unwrap(), Err(EXPIRED_RESULT.to_owned()));
    assert_eq!(outbox.withdrawn.lock().unwrap().last(), Some(&code));

    // A prompt that can't be queued denies the call at once.
    let refusing = for_outbox(Arc::new(Outbox {
        refuse: true,
        ..Outbox::default()
    }));
    assert_eq!(
        refusing.approve_call("ask__run", &json!({})).await,
        Err(format!(
            "{DENIED_RESULT} (the approval request could not be sent: no channel for the identity)"
        ))
    );
}

#[tokio::test(start_paused = true)]
async fn a_new_message_denies_only_its_turns_requests() {
    let s = Setup::new();
    let mine = s.turn(true);
    let other_turn = s.turn(true);
    let first = ask(&mine, "ask__a");
    let second = ask(&mine, "ask__b");
    let other = ask(&other_turn, "ask__c");
    settle().await;
    assert_eq!(s.sent().len(), 3);

    mine.deny_pending("Stop, I changed my mind");
    let said = "Not approved. The user said: Stop, I changed my mind".to_owned();
    assert_eq!(first.await.unwrap(), Err(said.clone()));
    assert_eq!(second.await.unwrap(), Err(said));
    settle().await;
    assert!(!other.is_finished());
    let outcomes = s.outcomes();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|(_, o)| *o == ApprovalOutcome::Denied));
    other.abort();
}

#[tokio::test(start_paused = true)]
async fn a_dropped_wait_cancels_its_request() {
    let s = Setup::new();
    let approvals = s.turn(true);
    let waiting = ask(&approvals, "ask__run");
    settle().await;
    let code = code_of(&s.sent());
    waiting.abort();
    let _ = waiting.await;
    assert!(!approvals.has_pending());
    assert_eq!(s.outcomes(), [(code.clone(), ApprovalOutcome::Denied)]);
    let me = routed("owner", "owner");
    assert!(!s.board.is_pending(&me, None));
    s.board.answer(me, reply(Answer::Yes, Some(&code))).await;
    assert_eq!(
        s.sent(),
        [format!("owner: Approval request `{code}` was cancelled.")]
    );
}

#[tokio::test(start_paused = true)]
async fn a_prompt_that_cannot_be_sent_denies_at_once() {
    let s = Setup::new();
    let approvals = s.turn(false);
    assert_eq!(
        approvals.approve_call("ask__run", &json!({})).await,
        Err(
            "denied (the approval request could not be sent: unavailable: channel `chat` is not \
             available yet)"
                .to_owned()
        )
    );
    assert!(!approvals.has_pending());
    assert!(!s.board.is_pending(&routed("owner", "owner"), None));
}

#[tokio::test(start_paused = true)]
async fn a_question_is_approved_without_content_or_denied() {
    let s = Setup::new();
    let approvals = s.turn(true);
    for (answer, want) in [
        (Answer::Yes, ApprovalDecision::Approve { content: None }),
        // `yes!` answers a question like `yes`; nothing is allowed for the session.
        (
            Answer::YesForSession,
            ApprovalDecision::Approve { content: None },
        ),
        (Answer::No, ApprovalDecision::Deny),
    ] {
        let asking = approvals.clone();
        let waiting =
            tokio::spawn(async move { asking.answer_question("ci__deploy", "Ship it?").await });
        settle().await;
        let sent = s.sent();
        let code = code_of(&sent);
        assert_eq!(
            sent[0],
            format!(
                "owner: The tool `ci__deploy` asks: `Ship it?` Reply `yes {code}` or `no {code}`."
            )
        );
        s.board
            .answer(routed("owner", "owner"), reply(answer, Some(&code)))
            .await;
        assert_eq!(waiting.await.unwrap(), want);
    }
    assert!(!s.board.is_allowed(SessionId::new(1), "ci__deploy"));
    // Nobody attends: declined at once.
    let unattended = TurnApprovals::new(
        s.board.clone(),
        SessionId::new(1),
        Vec::new(),
        Duration::from_secs(600),
        None,
    );
    assert_eq!(
        unattended.answer_question("ci__deploy", "Ship it?").await,
        ApprovalDecision::Deny
    );
}

#[tokio::test(start_paused = true)]
async fn codes_are_unique_among_pending_and_closed_requests() {
    let s = Setup::new();
    let approvals = s.turn(true);
    let target = approvals.target.clone().unwrap();
    let mut codes = HashSet::new();
    for n in 0..2_000 {
        let (code, _decided) = s
            .board
            .open(&target, SessionId::new(1), 1, "ask__run", false);
        assert_eq!(code.len(), 4, "{code}");
        assert!(codes.insert(code.clone()), "{code} handed out twice");
        if n % 2 == 0 {
            s.board
                .close(&code, Closing::Answered, ApprovalOutcome::Allowed);
        }
    }
}

#[test]
fn the_not_approved_result_quotes_the_message() {
    assert_eq!(
        not_approved("  Use the other one  "),
        "Not approved. The user said: Use the other one"
    );
    assert_eq!(
        not_approved(""),
        "Not approved. The user sent a new message."
    );
    let long = "a".repeat(MAX_QUOTED_CHARS + 10);
    assert_eq!(
        not_approved(&long),
        format!("{NOT_APPROVED_PREFIX}{}…", "a".repeat(MAX_QUOTED_CHARS))
    );
}

#[cfg(feature = "channel-web")]
#[tokio::test]
async fn the_channel_presenter_gives_web_chats_buttons_and_others_text() {
    struct NoMedia;

    #[async_trait]
    impl crate::providers::MediaSource for NoMedia {
        async fn read(&self, media: &giles_types::MediaRef) -> giles_types::Result<bytes::Bytes> {
            Err(giles_types::Error::NotFound(media.id.to_string()))
        }
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("giles.toml"),
        "[owner]\nidentities = [\"fake:owner\"]\n[channel.chat]\ncomponent = \"web\"\n",
    )
    .unwrap();
    let root = crate::paths::DataRoot::new(dir.path(), std::path::Path::new("/tmp")).unwrap();
    let config = crate::config::Config::load_with(
        &root,
        &crate::config::LoadOptions {
            env: &|_| None,
            giles_exe: None,
            ..crate::config::LoadOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let registry =
        Arc::new(Registry::from_config(root, Arc::new(config), Arc::new(NoMedia)).unwrap());
    let web = registry.web_channel().unwrap();
    let presenter = ChannelPresenter::new(registry.clone());

    let chat = session(1, &"f".repeat(32));
    let id = chat.conversation.clone().unwrap();
    let reply = Reply::for_session(&chat, registry.clone()).unwrap();
    // Shown with buttons: the web chat has the prompt to retire.
    presenter
        .present(&reply, "4821", "Allow it?")
        .await
        .unwrap();
    assert!(web.resolve_approval(&id, "4821", ApprovalOutcome::Denied));
    // `resolved` retires it.
    presenter
        .present(&reply, "1093", "Allow it?")
        .await
        .unwrap();
    presenter.resolved(&reply, "1093", ApprovalOutcome::Expired);
    assert!(!web.resolve_approval(&id, "1093", ApprovalOutcome::Denied));

    // Another channel gets the text through `Reply::send` (here: no such channel registered).
    let mut other = session(2, "owner");
    other.channel = Some("irc".to_owned());
    let reply = Reply::for_session(&other, registry.clone()).unwrap();
    let error = presenter
        .present(&reply, "5555", "Allow it?")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not available"), "{error}");
}
