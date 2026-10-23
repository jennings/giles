//! The session manager against a temporary store, a fake channel, and a fake turn runner. Tokio
//! time is paused: the clock is a fixed base plus Tokio's elapsed time, and waiting for a short
//! sleep means every task has run as far as it can (blocking store calls hold the clock).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc as fmpsc;
use futures::stream::{self, BoxStream, StreamExt};
use giles_store::{InboxId, InboxState, NewInboxEvent, SessionId, SessionKind, Store};
use giles_types::{
    AgentId, Block, ChannelEvent, ChannelInfo, ChannelStatus, ConversationId, ConversationKind,
    ConversationRef, Error, EventId, InboundMessage, Message, MessageId, MessageMeta, NativeId,
    OutboundMessage, OutputMode, Result, Role, SendTarget, Timestamp, UserId,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::inbound::Backoff;
use super::*;
use crate::commands::{Answer, ApprovalReply};
use crate::config::{Identity, OwnerConfig, Route};
use crate::identity;

const INSTANCE: &str = "fake";
const OWNER: &str = "owner";
/// Long enough that only a hang reaches it (paused time jumps straight there).
const WAIT: Duration = Duration::from_secs(3600);

fn base() -> Timestamp {
    Timestamp::from_second(1_800_000_000).unwrap()
}

/// [`base`] plus the Tokio time elapsed since the test started.
struct TestClock {
    start: Instant,
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        base().checked_add(self.start.elapsed()).unwrap()
    }
}

fn agent() -> AgentId {
    AgentId::new("jeeves").unwrap()
}

fn key() -> SessionKey {
    SessionKey {
        agent: agent(),
        channel: INSTANCE.to_owned(),
        conversation: ConversationId::new(OWNER).unwrap(),
    }
}

fn inbound(sender: &str, kind: ConversationKind, text: &str) -> InboundMessage {
    InboundMessage {
        conversation: ConversationRef {
            id: ConversationId::new(sender).unwrap(),
            kind,
            title: None,
        },
        sender: NativeId::new(sender).unwrap(),
        sender_aliases: Vec::new(),
        sender_authenticated: true,
        sender_display_name: None,
        text: text.to_owned(),
        attachments: Vec::new(),
        message_id: None,
        reply_to: None,
        sent_at: None,
    }
}

fn event(id: &str, message: InboundMessage) -> ChannelEvent {
    ChannelEvent::Message {
        id: EventId::new(id).unwrap(),
        message,
    }
}

/// A direct message from the owner.
fn message(id: &str, text: &str) -> ChannelEvent {
    event(id, inbound(OWNER, ConversationKind::Direct, text))
}

/// What the store held when an event was acked.
#[derive(Debug)]
struct Ack {
    event: String,
    /// Event ids of every queued inbox row.
    queued: Vec<String>,
    /// Every open session.
    open: Vec<SessionId>,
}

type EventSender = fmpsc::UnboundedSender<Result<ChannelEvent>>;

/// Each `events()` call takes the next prepared result; with none left, a stream that never
/// yields.
struct FakeChannel {
    info: ChannelInfo,
    store: Store,
    streams: Mutex<VecDeque<Option<fmpsc::UnboundedReceiver<Result<ChannelEvent>>>>>,
    opened: Mutex<Vec<Instant>>,
    acks: mpsc::UnboundedSender<Ack>,
    sent: mpsc::UnboundedSender<OutboundMessage>,
}

impl FakeChannel {
    /// Prepares a stream for the next `events()` call and returns its sender.
    fn push_stream(&self) -> EventSender {
        let (tx, rx) = fmpsc::unbounded();
        self.streams.lock().unwrap().push_back(Some(rx));
        tx
    }

    /// Makes the next `events()` call fail.
    fn push_failure(&self) {
        self.streams.lock().unwrap().push_back(None);
    }
}

#[async_trait]
impl Channel for FakeChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        self.opened.lock().unwrap().push(Instant::now());
        let next = self.streams.lock().unwrap().pop_front();
        match next {
            Some(Some(rx)) => Ok(rx.boxed()),
            Some(None) => Err(Error::unavailable("down")),
            None => Ok(stream::pending().boxed()),
        }
    }

    async fn ack(&self, event: &EventId) -> Result<()> {
        let mut queued = Vec::new();
        for session in self.store.inbox_sessions_with_queued().await.unwrap() {
            for row in self.store.inbox_queued(session).await.unwrap() {
                queued.push(row.event_id.to_string());
            }
        }
        let open = self
            .store
            .open_sessions()
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        self.acks
            .send(Ack {
                event: event.to_string(),
                queued,
                open,
            })
            .unwrap();
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        self.sent.send(msg).unwrap();
        Ok(MessageId::new("sent").unwrap())
    }
}

/// Only [`INSTANCE`] has a channel, and only if `registered` (a built-in, registered before
/// startup recovery; a plugin channel isn't).
struct Lookup {
    channel: Arc<FakeChannel>,
    registered: bool,
}

impl ChannelLookup for Lookup {
    fn channel(&self, instance: &str) -> Option<Arc<dyn Channel>> {
        (self.registered && instance == INSTANCE).then(|| self.channel.clone() as Arc<dyn Channel>)
    }
}

/// A turn the fake runner started; the test ends it through `done` (or `/stop` does).
struct Started {
    session: SessionId,
    requester: Option<UserId>,
    texts: Vec<String>,
    rows: Vec<InboxId>,
    steering: Steering,
    reply: Option<Reply>,
    done: oneshot::Sender<TurnOutcome>,
}

impl Started {
    fn complete(self) {
        self.done.send(TurnOutcome::Completed).unwrap();
    }
}

struct FakeRunner {
    turns: mpsc::UnboundedSender<Started>,
}

#[async_trait]
impl TurnRunner for FakeRunner {
    async fn run_turn(&self, turn: Turn) -> TurnOutcome {
        let (done, outcome) = oneshot::channel();
        let cancel = turn.cancel.clone();
        self.turns
            .send(Started {
                session: turn.session.id,
                requester: turn.requester,
                texts: turn
                    .messages
                    .iter()
                    .map(|m| m.message.text.clone())
                    .collect(),
                rows: turn.messages.iter().map(|m| m.inbox).collect(),
                steering: turn.steering,
                reply: turn.reply,
                done,
            })
            .unwrap();
        // `biased`: a turn completed just as shutdown starts reports `Completed`.
        tokio::select! {
            biased;
            outcome = outcome => outcome.unwrap_or(TurnOutcome::Completed),
            () = cancel.cancelled() => TurnOutcome::Cancelled,
        }
    }
}

/// Approvals with a switchable "something is pending" and a log of answers.
#[derive(Default)]
struct FakeApprovals {
    pending: AtomicBool,
    answers: Mutex<Vec<ApprovalReply>>,
}

#[async_trait]
impl Approvals for FakeApprovals {
    fn is_pending(&self, _: &Routed, _: Option<&str>) -> bool {
        self.pending.load(Ordering::SeqCst)
    }

    async fn answer(&self, _: Routed, reply: ApprovalReply) {
        self.answers.lock().unwrap().push(reply);
    }
}

/// A store and a channel, before the session manager starts.
struct Setup {
    dir: TempDir,
    store: Store,
    channel: Arc<FakeChannel>,
    clock: Arc<TestClock>,
    policy: AgentPolicy,
    /// Whether the channel is registered for recovery (it's attached by `start` either way).
    registered: bool,
    approvals: Arc<FakeApprovals>,
    acks: mpsc::UnboundedReceiver<Ack>,
    sent: mpsc::UnboundedReceiver<OutboundMessage>,
}

impl Setup {
    async fn new(policy: AgentPolicy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let (acks_tx, acks) = mpsc::unbounded_channel();
        let (sent_tx, sent) = mpsc::unbounded_channel();
        let channel = Arc::new(FakeChannel {
            info: ChannelInfo {
                identity_namespace: INSTANCE.to_owned(),
                output: OutputMode::Blocks,
                can_initiate: false,
            },
            store: store.clone(),
            streams: Mutex::default(),
            opened: Mutex::default(),
            acks: acks_tx,
            sent: sent_tx,
        });
        Self {
            dir,
            store,
            channel,
            clock: Arc::new(TestClock {
                start: Instant::now(),
            }),
            policy,
            registered: true,
            approvals: Arc::default(),
            acks,
            sent,
        }
    }

    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Starts the session manager and attaches the channel.
    async fn start(self) -> Harness {
        let (turns_tx, turns) = mpsc::unbounded_channel();
        let identities = vec![Identity::new(format!("{INSTANCE}:{OWNER}")).unwrap()];
        let router = Router::new(
            &OwnerConfig {
                default: identities[0].clone(),
                identities,
                timezone: None,
            },
            &[Route {
                channel: None,
                kind: None,
                conversation: None,
                agent: agent(),
            }],
        );
        let shutdown = CancellationToken::new();
        let sessions = Sessions::start(
            SessionsOptions {
                store: self.store.clone(),
                router,
                agents: HashMap::from([(agent(), self.policy)]),
                channels: Arc::new(Lookup {
                    channel: self.channel.clone(),
                    registered: self.registered,
                }),
                runner: Arc::new(FakeRunner { turns: turns_tx }),
                approvals: self.approvals.clone(),
                clock: self.clock.clone(),
            },
            shutdown.clone(),
        )
        .await
        .unwrap();
        sessions.attach_channel(
            InstanceName::new(INSTANCE).unwrap(),
            INSTANCE.to_owned(),
            self.channel.clone(),
        );
        Harness {
            setup: self,
            sessions,
            shutdown,
            turns,
        }
    }
}

struct Harness {
    setup: Setup,
    sessions: Sessions,
    shutdown: CancellationToken,
    turns: mpsc::UnboundedReceiver<Started>,
}

impl Harness {
    /// Started with one live event stream, whose sender is returned.
    async fn new(policy: AgentPolicy) -> (Self, EventSender) {
        let setup = Setup::new(policy).await;
        let events = setup.channel.push_stream();
        (setup.start().await, events)
    }

    fn store(&self) -> &Store {
        &self.setup.store
    }

    async fn ack(&mut self) -> Ack {
        tokio::time::timeout(WAIT, self.setup.acks.recv())
            .await
            .expect("no ack")
            .unwrap()
    }

    async fn turn(&mut self) -> Started {
        tokio::time::timeout(WAIT, self.turns.recv())
            .await
            .expect("no turn started")
            .unwrap()
    }

    /// Lets every task run until all are waiting.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    async fn assert_no_turn(&mut self) {
        self.settle().await;
        if let Ok(turn) = self.turns.try_recv() {
            panic!("unexpected turn: {:?}", turn.texts);
        }
    }

    async fn state(&self, row: InboxId) -> InboxState {
        self.store().inbox_entry(row).await.unwrap().unwrap().state
    }

    /// Whether (`INSTANCE`, `event`) is free as a dedupe key: inserts a `done` row to find out.
    async fn key_is_free(&self, event: &str) -> bool {
        self.store()
            .inbox_insert(done_row(event, self.setup.now()))
            .await
            .unwrap()
            .is_some()
    }
}

fn done_row(event: &str, received_at: Timestamp) -> NewInboxEvent {
    NewInboxEvent {
        instance: INSTANCE.to_owned(),
        event_id: EventId::new(event).unwrap(),
        session: None,
        user: None,
        payload: json!(null),
        state: InboxState::Done,
        received_at,
    }
}

fn queue() -> AgentPolicy {
    AgentPolicy {
        busy_mode: BusyMode::Queue,
        idle_reset: None,
    }
}

fn steer() -> AgentPolicy {
    AgentPolicy {
        busy_mode: BusyMode::Steer,
        idle_reset: None,
    }
}

fn send(events: &EventSender, event: ChannelEvent) {
    events.unbounded_send(Ok(event)).unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_message_is_stored_before_its_ack_and_runs_as_a_turn() {
    let (mut h, events) = Harness::new(queue()).await;
    send(&events, message("e1", "hello"));

    let ack = h.ack().await;
    assert_eq!(ack.event, "e1");
    assert_eq!(ack.queued, ["e1"], "acked before it was stored");
    assert_eq!(ack.open.len(), 1);

    let turn = h.turn().await;
    assert_eq!(turn.texts, ["hello"]);
    assert_eq!(turn.requester, Some(identity::owner()));
    assert_eq!(turn.session, ack.open[0]);
    let session = h.store().session(turn.session).await.unwrap().unwrap();
    assert_eq!(session.key(), Some(key()));
    assert_eq!(session.kind, SessionKind::Direct);
    assert_eq!(session.user, Some(identity::owner()));
    let reply = turn.reply.as_ref().unwrap();
    assert_eq!(
        (reply.instance(), reply.conversation().as_str()),
        (INSTANCE, OWNER)
    );
    let row = turn.rows[0];
    assert_eq!(h.state(row).await, InboxState::Started);
    assert!(h.sessions.is_running(turn.session));

    turn.complete();
    h.settle().await;
    assert_eq!(h.state(row).await, InboxState::Done);
    assert!(!h.sessions.is_running(session.id));
}

#[tokio::test(start_paused = true)]
async fn a_redelivered_message_is_acked_and_ignored() {
    let (mut h, events) = Harness::new(queue()).await;
    send(&events, message("e1", "hello"));
    h.ack().await;
    let turn = h.turn().await;

    // Again while the turn runs, and again after it ended.
    send(&events, message("e1", "hello"));
    assert_eq!(h.ack().await.event, "e1");
    turn.complete();
    h.settle().await;
    send(&events, message("e1", "hello"));
    let ack = h.ack().await;
    assert_eq!(ack.event, "e1");
    assert!(ack.queued.is_empty());
    h.assert_no_turn().await;
}

#[tokio::test(start_paused = true)]
async fn dropped_messages_are_acked_and_not_stored() {
    let (mut h, events) = Harness::new(queue()).await;
    let mut unauthenticated = inbound(OWNER, ConversationKind::Direct, "hi");
    unauthenticated.sender_authenticated = false;
    let drops = [
        event("g1", inbound(OWNER, ConversationKind::Group, "hi all")),
        event("u1", inbound("stranger", ConversationKind::Direct, "hi")),
        event("a1", unauthenticated),
    ];
    for drop in drops {
        send(&events, drop);
        let ack = h.ack().await;
        assert!(ack.queued.is_empty() && ack.open.is_empty(), "{ack:?}");
    }
    h.assert_no_turn().await;
    for id in ["g1", "u1", "a1"] {
        assert!(h.key_is_free(id).await, "{id} was stored");
    }
}

#[tokio::test(start_paused = true)]
async fn one_turn_at_a_time_and_the_queue_runs_next_as_one_batch() {
    let (mut h, events) = Harness::new(queue()).await;
    send(&events, message("e1", "one"));
    h.ack().await;
    let first = h.turn().await;

    send(&events, message("e2", "two"));
    send(&events, message("e3", "three"));
    assert_eq!(h.ack().await.queued, ["e2"]);
    assert_eq!(h.ack().await.queued, ["e2", "e3"]);
    h.assert_no_turn().await;
    // Queue mode still wakes the running turn (it may be waiting on an approval); this one
    // doesn't take them, so they run next.
    tokio::time::timeout(WAIT, first.steering.changed())
        .await
        .expect("steering not woken");

    let session = first.session;
    first.complete();
    let second = h.turn().await;
    assert_eq!(second.texts, ["two", "three"]);
    assert_eq!(second.session, session);
    second.complete();
    h.assert_no_turn().await;
}

#[tokio::test(start_paused = true)]
async fn other_senders_wait_for_their_own_turn() {
    // v1 routes only the owner, so the rows of a second person are inserted directly.
    let setup = Setup::new(queue()).await;
    let now = setup.now();
    let (session, _) = setup
        .store
        .open_session(key(), SessionKind::Direct, Some(identity::owner()), now)
        .await
        .unwrap();
    for (id, user, text) in [
        ("e1", "ann", "a1"),
        ("e2", "bob", "b1"),
        ("e3", "ann", "a2"),
    ] {
        setup
            .store
            .inbox_insert(NewInboxEvent {
                instance: INSTANCE.to_owned(),
                event_id: EventId::new(id).unwrap(),
                session: Some(session.id),
                user: Some(UserId::new(user).unwrap()),
                payload: serde_json::to_value(message(id, text)).unwrap(),
                state: InboxState::Queued,
                received_at: now,
            })
            .await
            .unwrap();
    }
    let mut h = setup.start().await;

    let first = h.turn().await;
    assert_eq!(first.requester, Some(UserId::new("ann").unwrap()));
    assert_eq!(first.texts, ["a1", "a2"]);
    h.assert_no_turn().await;
    first.complete();
    let second = h.turn().await;
    assert_eq!(second.requester, Some(UserId::new("bob").unwrap()));
    assert_eq!(second.texts, ["b1"]);
    second.complete();
    h.assert_no_turn().await;
}

#[tokio::test(start_paused = true)]
async fn steering_hands_the_requesters_new_messages_to_the_running_turn() {
    let (mut h, events) = Harness::new(steer()).await;
    send(&events, message("e1", "one"));
    h.ack().await;
    let first = h.turn().await;
    assert!(first.steering.take().await.unwrap().is_empty());

    send(&events, message("e2", "two"));
    h.ack().await;
    tokio::time::timeout(WAIT, first.steering.changed())
        .await
        .expect("steering not woken");
    let taken = first.steering.take().await.unwrap();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].message.text, "two");
    assert_eq!(h.state(taken[0].inbox).await, InboxState::Started);

    // Not taken: it starts the next turn.
    send(&events, message("e3", "three"));
    h.ack().await;
    h.assert_no_turn().await;
    let steering = first.steering;
    first.done.send(TurnOutcome::Completed).unwrap();
    let second = h.turn().await;
    assert_eq!(second.texts, ["three"]);
    assert_eq!(h.state(taken[0].inbox).await, InboxState::Done);
    // A finished turn's steering takes nothing.
    send(&events, message("e4", "four"));
    h.ack().await;
    assert!(steering.take().await.unwrap().is_empty());
    second.complete();
    assert_eq!(h.turn().await.texts, ["four"]);
}

#[tokio::test(start_paused = true)]
async fn new_ends_the_session_once_after_its_ack() {
    let (mut h, events) = Harness::new(queue()).await;
    send(&events, message("e1", "one"));
    h.ack().await;
    let first = h.turn().await;
    let old = first.session;
    first.complete();

    send(&events, message("n1", "/new"));
    let ack = h.ack().await;
    assert_eq!(ack.open, [old], "acted before the ack");
    h.settle().await;
    let ended = h.store().session(old).await.unwrap().unwrap();
    assert!(ended.ended_at.is_some());
    assert!(!h.key_is_free("n1").await, "/new left no dedupe key");

    send(&events, message("e2", "two"));
    h.ack().await;
    let second = h.turn().await;
    let new = second.session;
    assert_ne!(new, old);
    second.complete();

    // A redelivered `/new` doesn't end the new session.
    send(&events, message("n1", "/new"));
    h.ack().await;
    h.settle().await;
    let open = h.store().find_open_session(key()).await.unwrap().unwrap();
    assert_eq!(open.id, new);
    h.assert_no_turn().await;
}

#[tokio::test(start_paused = true)]
async fn stop_cancels_the_running_turn() {
    let (mut h, events) = Harness::new(queue()).await;
    // Nothing running: acked, nothing happens.
    send(&events, message("s0", "/stop"));
    h.ack().await;
    h.assert_no_turn().await;

    send(&events, message("e1", "one"));
    h.ack().await;
    let turn = h.turn().await;
    send(&events, message("s1", "/stop"));
    h.ack().await;
    h.settle().await;
    assert!(turn.done.is_closed(), "the runner is still waiting");
    assert_eq!(h.state(turn.rows[0]).await, InboxState::Done);
    assert!(!h.sessions.stop(&key()));
}

#[tokio::test(start_paused = true)]
async fn approval_replies_go_to_the_hook_once() {
    let (mut h, events) = Harness::new(queue()).await;
    h.setup.approvals.pending.store(true, Ordering::SeqCst);
    send(&events, message("y1", "yes 4821"));
    h.ack().await;
    send(&events, message("y1", "yes 4821"));
    h.ack().await;
    h.settle().await;
    let answers = h.setup.approvals.answers.lock().unwrap().clone();
    assert_eq!(
        answers,
        [ApprovalReply {
            answer: Answer::Yes,
            code: Some("4821".to_owned()),
        }]
    );
    h.assert_no_turn().await;

    // Nothing pending: an ordinary message.
    h.setup.approvals.pending.store(false, Ordering::SeqCst);
    send(&events, message("y2", "yes"));
    h.ack().await;
    assert_eq!(h.turn().await.texts, ["yes"]);
}

#[tokio::test(start_paused = true)]
async fn idle_reset_ends_a_quiet_session_at_the_boundary() {
    let idle = Duration::from_secs(3600);
    let (mut h, events) = Harness::new(AgentPolicy {
        busy_mode: BusyMode::Queue,
        idle_reset: Some(idle),
    })
    .await;
    let user_message = |at| Message {
        role: Role::User,
        content: vec![Block::text("hi")],
        meta: MessageMeta::new(at),
    };

    send(&events, message("e1", "one"));
    h.ack().await;
    let first = h.turn().await;
    let session = first.session;
    let last = h.setup.now();
    h.store()
        .append_messages(session, vec![user_message(last)])
        .await
        .unwrap();
    first.complete();
    h.settle().await;

    // Quiet for a second less than `idle_reset`: the same session.
    let target = last.checked_add(idle - Duration::from_secs(1)).unwrap();
    tokio::time::advance(target.duration_since(h.setup.now()).unsigned_abs()).await;
    send(&events, message("e2", "two"));
    h.ack().await;
    let second = h.turn().await;
    assert_eq!(second.session, session);

    // Quiet for longer than `idle_reset` while that turn runs: still the same session.
    tokio::time::advance(idle * 2).await;
    send(&events, message("e3", "three"));
    h.ack().await;
    second.complete();
    let third = h.turn().await;
    assert_eq!(third.session, session);
    let last = h.setup.now();
    h.store()
        .append_messages(session, vec![user_message(last)])
        .await
        .unwrap();
    third.complete();
    h.settle().await;

    // Quiet for exactly `idle_reset`: a fresh session.
    let target = last.checked_add(idle).unwrap();
    tokio::time::advance(target.duration_since(h.setup.now()).unsigned_abs()).await;
    send(&events, message("e4", "four"));
    h.ack().await;
    let fourth = h.turn().await;
    assert_ne!(fourth.session, session);
    let old = h.store().session(session).await.unwrap().unwrap();
    assert!(old.ended_at.is_some());
}

/// The state a restart leaves behind: a turn on the fake channel was running (two started rows
/// and a tool call without a result) with one more message queued behind it, and a turn on a
/// channel that never comes back was running. Returns the rows: e1, e2, e3 started, e4 queued.
async fn seed_interrupted(setup: &Setup) -> Vec<InboxId> {
    let now = setup.now();
    let store = &setup.store;
    // A turn on the fake channel was running: one started row, a tool call without a result,
    // and one more message queued behind it.
    let (running, _) = store
        .open_session(key(), SessionKind::Direct, Some(identity::owner()), now)
        .await
        .unwrap();
    let tool_use = Message {
        role: Role::Assistant,
        content: vec![Block::ToolUse {
            id: "t1".to_owned(),
            name: "memory".to_owned(),
            input: json!({}),
        }],
        meta: MessageMeta::new(now),
    };
    store
        .append_messages(running.id, vec![tool_use])
        .await
        .unwrap();
    // A turn on a channel that is never attached: logged only.
    let elsewhere = SessionKey {
        channel: "gone".to_owned(),
        ..key()
    };
    let (gone, _) = store
        .open_session(elsewhere, SessionKind::Direct, Some(identity::owner()), now)
        .await
        .unwrap();
    let mut rows = Vec::new();
    for (id, session, text) in [
        ("e1", running.id, "one"),
        ("e2", running.id, "two"),
        ("e3", gone.id, "three"),
        ("e4", running.id, "four"),
    ] {
        let row = store
            .inbox_insert(NewInboxEvent {
                instance: INSTANCE.to_owned(),
                event_id: EventId::new(id).unwrap(),
                session: Some(session),
                user: Some(identity::owner()),
                payload: serde_json::to_value(message(id, text)).unwrap(),
                state: InboxState::Queued,
                received_at: now,
            })
            .await
            .unwrap()
            .unwrap();
        rows.push(row);
    }
    store
        .inbox_start(vec![rows[0], rows[1], rows[2]])
        .await
        .unwrap();
    rows
}

fn assert_notice(notice: &OutboundMessage) {
    assert_eq!(notice.markdown, INTERRUPTED_NOTICE);
    assert_eq!(
        notice.target,
        SendTarget::Conversation(ConversationId::new(OWNER).unwrap())
    );
}

#[tokio::test(start_paused = true)]
async fn startup_recovery_marks_interrupted_turns_and_tells_the_requester() {
    let setup = Setup::new(queue()).await;
    let rows = seed_interrupted(&setup).await;

    let mut h = setup.start().await;
    let notice = h.setup.sent.try_recv().expect("no notice sent");
    assert_notice(&notice);
    h.settle().await;
    assert!(h.setup.sent.try_recv().is_err(), "one notice per session");
    for row in &rows[..3] {
        assert_eq!(h.state(*row).await, InboxState::Unknown);
    }
    let unfilled = h
        .store()
        .fill_all_empty_slots("again", h.setup.now())
        .await
        .unwrap();
    assert!(unfilled.is_empty(), "slots left empty: {unfilled:?}");

    // The queued message resumes; nothing is replayed.
    let turn = h.turn().await;
    assert_eq!(turn.texts, ["four"]);
    turn.complete();
    h.assert_no_turn().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_cancels_turns_and_leaves_their_rows_for_recovery() {
    let (mut h, events) = Harness::new(queue()).await;
    send(&events, message("e1", "one"));
    h.ack().await;
    let turn = h.turn().await;
    h.shutdown.cancel();
    tokio::time::timeout(WAIT, h.sessions.closed())
        .await
        .expect("tasks still running after shutdown");
    assert!(turn.done.is_closed());
    assert_eq!(h.state(turn.rows[0]).await, InboxState::Started);
}

#[tokio::test(start_paused = true)]
async fn a_restart_notice_waits_for_its_channel_to_be_attached() {
    let mut setup = Setup::new(queue()).await;
    setup.registered = false;
    seed_interrupted(&setup).await;

    // `start` attaches the channel after recovery, which sends the kept notice.
    let mut h = setup.start().await;
    h.settle().await;
    let notice = h.setup.sent.try_recv().expect("no notice sent on attach");
    assert_notice(&notice);
    assert!(h.setup.sent.try_recv().is_err(), "one notice per session");

    // Attaching again sends nothing more.
    h.sessions.attach_channel(
        InstanceName::new(INSTANCE).unwrap(),
        INSTANCE.to_owned(),
        h.setup.channel.clone(),
    );
    h.settle().await;
    assert!(h.setup.sent.try_recv().is_err(), "notice sent twice");
    h.turn().await.complete();
}

#[tokio::test(start_paused = true)]
async fn a_turn_that_completes_as_shutdown_starts_is_finished() {
    let (mut h, events) = Harness::new(queue()).await;
    send(&events, message("e1", "one"));
    h.ack().await;
    let turn = h.turn().await;
    let row = turn.rows[0];
    turn.complete();
    h.shutdown.cancel();
    h.settle().await;
    assert_eq!(h.state(row).await, InboxState::Done);
}

#[tokio::test(start_paused = true)]
async fn a_database_failure_leaves_the_event_unacked_and_reopens_the_stream() {
    let setup = Setup::new(queue()).await;
    let first = setup.channel.push_stream();
    let second = setup.channel.push_stream();
    let channel = setup.channel.clone();
    // A second connection makes every inbox insert fail.
    let db = rusqlite::Connection::open(setup.dir.path().join("giles.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_inbox BEFORE INSERT ON inbox
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let start = Instant::now();
    let mut h = setup.start().await;

    send(&first, message("e1", "one"));
    h.settle().await;
    assert!(
        h.setup.acks.try_recv().is_err(),
        "acked an event it didn't store"
    );
    h.assert_no_turn().await;
    assert_eq!(channel.opened.lock().unwrap().len(), 1);
    tokio::time::sleep(Backoff::INITIAL).await;
    let reopened = channel.opened.lock().unwrap()[1];
    assert_eq!(reopened.duration_since(start), Backoff::INITIAL);

    // The channel resends it on the new stream; now it's stored and acked once.
    db.execute_batch("DROP TRIGGER fail_inbox").unwrap();
    send(&second, message("e1", "one"));
    let ack = h.ack().await;
    assert_eq!(
        (ack.event.as_str(), ack.queued.as_slice()),
        ("e1", ["e1".to_owned()].as_slice())
    );
    assert_eq!(h.turn().await.texts, ["one"]);
    h.settle().await;
    assert!(h.setup.acks.try_recv().is_err(), "acked twice");
}

#[tokio::test(start_paused = true)]
async fn dedupe_keys_are_pruned_after_thirty_days() {
    let setup = Setup::new(queue()).await;
    let now = setup.now();
    let day = Duration::from_secs(24 * 3600);
    for (id, age) in [("old", 31), ("recent", 29)] {
        let at = now.checked_sub(day * age).unwrap();
        setup.store.inbox_insert(done_row(id, at)).await.unwrap();
    }
    let h = setup.start().await;
    h.settle().await;
    assert!(h.key_is_free("old").await, "not pruned at startup");
    assert!(!h.key_is_free("recent").await, "pruned too early");

    // Two days later, an hourly prune has removed it.
    tokio::time::advance(day * 2).await;
    h.settle().await;
    assert!(h.key_is_free("recent").await, "not pruned later");
}

#[tokio::test(start_paused = true)]
async fn the_event_stream_is_reopened_with_backoff() {
    let setup = Setup::new(queue()).await;
    setup.channel.push_failure();
    drop(setup.channel.push_stream()); // ends at once, having delivered nothing
    let live = setup.channel.push_stream();
    let channel = setup.channel.clone();
    let start = Instant::now();
    let mut h = setup.start().await;
    let opened_at = |n: usize| {
        let opened = channel.opened.lock().unwrap();
        opened.get(n).map(|at| at.duration_since(start))
    };

    send(&live, message("e1", "one"));
    h.ack().await;
    assert_eq!(opened_at(0), Some(Duration::ZERO));
    assert_eq!(opened_at(1), Some(Duration::from_secs(1)));
    assert_eq!(opened_at(2), Some(Duration::from_secs(3)));

    // A status event is recorded and acked.
    live.unbounded_send(Ok(ChannelEvent::Status {
        id: EventId::new("st1").unwrap(),
        status: ChannelStatus::Connected,
    }))
    .unwrap();
    assert_eq!(h.ack().await.event, "st1");
    let status = h.sessions.channel_status(INSTANCE).unwrap();
    assert_eq!(status.status, ChannelStatus::Connected);
    assert!(h.sessions.channel_statuses().contains_key(INSTANCE));

    // A stream that delivered events resets the backoff; a stream error reopens too.
    let ended = Instant::now();
    live.unbounded_send(Err(Error::unavailable("lost")))
        .unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    let reopened = channel.opened.lock().unwrap()[3];
    assert_eq!(reopened.duration_since(ended), Backoff::INITIAL);
    h.turn().await.complete();
}
