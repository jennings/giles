//! The scheduler against a temporary data root and store, the real session manager, a fake
//! runner that answers job turns from a script, and a fake delivery. Tokio time is paused: the
//! clock is a fixed base plus Tokio's elapsed time, so sleeping until a job's time is instant.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use giles_store::{JobKey, OutboxId, Session, SessionKey, SessionKind, Store};
use giles_types::{
    AgentId, Block, Channel, ChannelEvent, ChannelInfo, ConversationId, EventId, Message,
    MessageId, MessageMeta, OutboundMessage, OutputMode, Role, Timestamp, UserId,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::approvals::ApprovalTarget;
use crate::commands::NoPendingApprovals;
use crate::config::{Config, InstanceName, LoadOptions};
use crate::identity::owner;
use crate::paths::DataRoot;
use crate::router::Router;
use crate::routes::RouteList;
use crate::sessions::{SessionsOptions, Turn, TurnOutcome};

const AGENT: &str = "jeeves";
/// Long enough that only a hang reaches it (paused time jumps straight there).
const WAIT: Duration = Duration::from_secs(400 * 24 * 3600);

/// 07:00 in Los Angeles (PDT), the owner's time zone.
fn base() -> Timestamp {
    "2026-10-22T14:00:00Z".parse().unwrap()
}

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

struct TestClock {
    start: Instant,
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        base().checked_add(self.start.elapsed()).unwrap()
    }
}

struct FakeChannel {
    info: ChannelInfo,
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

    async fn send(&self, _msg: OutboundMessage) -> giles_types::Result<MessageId> {
        panic!("jobs send through the delivery, never into a conversation");
    }
}

/// A job turn the fake runner ran.
#[derive(Debug)]
struct Ran {
    session: Session,
    requester: Option<UserId>,
    prompt: String,
    model: Option<String>,
    approvals: Option<ApprovalTarget>,
    at: Timestamp,
}

/// Answers each job turn with the next scripted reply (default `Report.`), storing the prompt
/// and the reply as the agent loop would.
struct FakeRunner {
    store: Store,
    clock: Arc<TestClock>,
    replies: Mutex<VecDeque<&'static str>>,
    ran: mpsc::UnboundedSender<Ran>,
    /// The next turn deletes this job (and tells the scheduler) before it answers.
    cancel_own: Mutex<Option<giles_store::RuntimeJobId>>,
    changes: JobChanges,
}

#[async_trait]
impl TurnRunner for FakeRunner {
    async fn run_turn(&self, turn: Turn) -> TurnOutcome {
        let job = turn.job.expect("only job turns in these tests");
        assert!(turn.reply.is_none());
        assert!(turn.messages.is_empty());
        // Like the model calling `schedule cancel` on its own job mid-turn; a turn that sees
        // its cancel token fire ends `Cancelled`, as the agent loop would.
        let cancel_own = self.cancel_own.lock().unwrap().take();
        if let Some(id) = cancel_own {
            self.store.job_delete(id).await.unwrap();
            self.changes.notify();
            tokio::time::sleep(Duration::from_millis(1)).await;
            if turn.cancel.is_cancelled() {
                return TurnOutcome::Cancelled;
            }
        }
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or("Report.");
        let Block::Text { text: prompt } = &job.prompt.content[0] else {
            panic!("the prompt isn't text");
        };
        self.store
            .append_messages(
                turn.session.id,
                vec![
                    job.prompt.clone(),
                    Message {
                        role: Role::Assistant,
                        content: vec![Block::text(reply)],
                        meta: MessageMeta::new(self.clock.now()),
                    },
                ],
            )
            .await
            .unwrap();
        self.ran
            .send(Ran {
                session: turn.session,
                requester: turn.requester,
                prompt: prompt.clone(),
                model: job.model.map(|m| m.to_string()),
                approvals: job.approvals,
                at: self.clock.now(),
            })
            .unwrap();
        TurnOutcome::Completed
    }
}

#[derive(Default)]
struct FakeDelivery {
    queued: Mutex<Vec<(OutboxId, Delivery)>>,
    removed: Mutex<Vec<OutboxId>>,
    sent: tokio::sync::Notify,
}

#[async_trait]
impl JobDelivery for FakeDelivery {
    async fn enqueue(&self, delivery: Delivery) -> Result<OutboxId, String> {
        let mut queued = self.queued.lock().unwrap();
        let id = OutboxId::new(i64::try_from(queued.len()).unwrap() + 1);
        queued.push((id, delivery));
        self.sent.notify_one();
        Ok(id)
    }

    async fn remove(&self, id: OutboxId) {
        self.removed.lock().unwrap().push(id);
    }
}

struct Harness {
    _dir: TempDir,
    store: Store,
    registry: Arc<Registry>,
    clock: Arc<TestClock>,
    sessions: Sessions,
    delivery: Arc<FakeDelivery>,
    changes: JobChanges,
    replies: Arc<FakeRunner>,
    ran: mpsc::UnboundedReceiver<Ran>,
    shutdown: CancellationToken,
    scheduler: Option<Scheduler>,
}

impl Harness {
    /// A data root whose agent has `jobs` (TOML `[[jobs]]` tables), with `files` in its directory.
    async fn new(jobs: &str, files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        let write = |relative: &str, contents: &str| {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        };
        write(
            "giles.toml",
            "[owner]\nidentities = [\"fake:owner\"]\ntimezone = \"America/Los_Angeles\"\n\
             [provider.anthropic]\n[provider.ollama]\ncomponent = \"openai\"\n[channel.fake]\n",
        );
        for (name, contents) in files {
            write(&format!("agents/{AGENT}/{name}"), contents);
        }
        write(
            &format!("agents/{AGENT}/agent.toml"),
            &format!("models = [\"anthropic/test-model\"]\n{jobs}"),
        );
        let config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let config = Arc::new(config);
        let registry = Arc::new(Registry::new(root, config.clone()));
        registry
            .register_channel(
                &InstanceName::new("fake").unwrap(),
                Arc::new(FakeChannel {
                    info: ChannelInfo {
                        identity_namespace: "fake".to_owned(),
                        output: OutputMode::Blocks,
                        can_initiate: true,
                    },
                }),
            )
            .unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let clock = Arc::new(TestClock {
            start: Instant::now(),
        });
        let (ran_tx, ran) = mpsc::unbounded_channel();
        let changes = JobChanges::default();
        let runner = Arc::new(FakeRunner {
            store: store.clone(),
            clock: clock.clone(),
            replies: Mutex::default(),
            ran: ran_tx,
            cancel_own: Mutex::default(),
            changes: changes.clone(),
        });
        let shutdown = CancellationToken::new();
        let sessions = Sessions::start(
            SessionsOptions {
                store: store.clone(),
                router: Router::new(
                    &config.owner,
                    RouteList::new(config.routes.clone()),
                    Arc::new(registry.live_config().clone()),
                ),
                agents: Arc::new(registry.live_config().clone()),
                channels: registry.clone(),
                runner: runner.clone(),
                approvals: Arc::new(NoPendingApprovals),
                clock: clock.clone(),
            },
            shutdown.clone(),
        )
        .await
        .unwrap();
        Self {
            _dir: dir,
            store,
            registry,
            clock,
            sessions,
            delivery: Arc::default(),
            changes,
            replies: runner,
            ran,
            shutdown,
            scheduler: None,
        }
    }

    /// Starts the scheduler and lets each job's task read its last run and compute its first
    /// time. Until then a manual advance (which, unlike auto-advance, doesn't wait for the
    /// store's blocking reads) could move the clock past a job's first time before it's computed.
    async fn start(&mut self) {
        self.scheduler = Some(Scheduler::start(
            SchedulerOptions {
                store: self.store.clone(),
                registry: self.registry.clone(),
                sessions: self.sessions.clone(),
                runner: self.replies.clone(),
                delivery: self.delivery.clone(),
                changes: self.changes.clone(),
                clock: self.clock.clone(),
            },
            self.shutdown.clone(),
        ));
        self.settle().await;
    }

    fn reply_with(&self, reply: &'static str) {
        self.replies.replies.lock().unwrap().push_back(reply);
    }

    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    async fn ran(&mut self) -> Ran {
        tokio::time::timeout(WAIT, self.ran.recv())
            .await
            .expect("no job ran")
            .unwrap()
    }

    /// Lets every task run until all are waiting, without passing a job's time.
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    async fn assert_nothing_ran(&mut self) {
        self.settle().await;
        if let Ok(ran) = self.ran.try_recv() {
            panic!("unexpected run: {ran:?}");
        }
    }

    /// Advances to `at` and lets the tasks run.
    async fn advance_to(&self, at: Timestamp) {
        let left = at.duration_since(self.now());
        tokio::time::advance(Duration::try_from(left).unwrap()).await;
        self.settle().await;
    }

    fn deliveries(&self) -> Vec<Delivery> {
        self.delivery
            .queued
            .lock()
            .unwrap()
            .iter()
            .map(|(_, d)| d.clone())
            .collect()
    }

    /// Waits until the delivery has `count` sends queued.
    async fn delivered(&self, count: usize) -> Vec<Delivery> {
        tokio::time::timeout(WAIT, async {
            loop {
                let notified = self.delivery.sent.notified();
                if self.deliveries().len() >= count {
                    return self.deliveries();
                }
                notified.await;
            }
        })
        .await
        .expect("nothing delivered")
    }

    fn key() -> SessionKey {
        SessionKey {
            agent: AgentId::new(AGENT).unwrap(),
            channel: "fake".to_owned(),
            conversation: ConversationId::new("owner").unwrap(),
        }
    }

    /// The owner's DM with `texts` in it, alternating user and assistant.
    async fn dm(&self, texts: &[&str]) -> Session {
        let (session, _) = self
            .store
            .open_session(Self::key(), SessionKind::Direct, Some(owner()), self.now())
            .await
            .unwrap();
        let messages = texts
            .iter()
            .enumerate()
            .map(|(i, text)| Message {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: vec![Block::text(*text)],
                meta: MessageMeta::new(self.now()),
            })
            .collect();
        self.store
            .append_messages(session.id, messages)
            .await
            .unwrap();
        session
    }

    /// The texts of `session`'s messages.
    async fn texts(&self, session: giles_store::SessionId) -> Vec<String> {
        self.store
            .load_history(session)
            .await
            .unwrap()
            .messages
            .into_iter()
            .filter_map(|m| match m.entry {
                giles_store::Entry::Message(message) => match &message.content[0] {
                    Block::Text { text } => Some(text.clone()),
                    _ => None,
                },
                giles_store::Entry::EmptySlot { .. } => None,
            })
            .collect()
    }

    async fn last_run(&self, key: JobKey) -> Option<Timestamp> {
        self.store
            .job_last_run(AgentId::new(AGENT).unwrap(), key)
            .await
            .unwrap()
    }

    async fn stop(mut self) {
        self.shutdown.cancel();
        let scheduler = self.scheduler.take().unwrap();
        tokio::time::timeout(WAIT, async {
            scheduler.closed().await;
            self.sessions.closed().await;
        })
        .await
        .expect("tasks still running after shutdown");
    }
}

fn config_key(id: &str) -> JobKey {
    JobKey::Config(id.to_owned())
}

#[tokio::test(start_paused = true)]
async fn a_cron_job_fires_in_the_owners_time_zone_in_a_session_of_its_own() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"morning\"\ncron = \"0 8 * * *\"\nprompt = \"Plan my day.\"\n",
        &[],
    )
    .await;
    h.start().await;
    h.assert_nothing_ran().await;

    // 08:00 in Los Angeles.
    let ran = h.ran().await;
    assert_eq!(ran.at, ts("2026-10-22T15:00:00Z"));
    assert_eq!(ran.session.kind, SessionKind::Job);
    assert_eq!(ran.session.user, Some(owner()));
    assert_eq!(ran.requester, None, "a config job has no requester");
    assert_eq!(ran.model, None);
    assert!(ran.approvals.is_none(), "ask tools are denied unattended");
    assert_eq!(
        ran.prompt,
        "[Thu 2026-10-22 08:00 PDT] Scheduled job `morning`. Your reply is sent to the user; reply \
         exactly NO_REPLY to send nothing.\n\nPlan my day."
    );
    let delivered = h.delivered(1).await;
    assert_eq!(
        delivered,
        [Delivery {
            user: owner(),
            kind: DeliveryKind::JobReply,
            markdown: "Report.".into(),
            expires_at: None,
        }]
    );
    assert_eq!(
        h.last_run(config_key("morning")).await,
        Some(ts("2026-10-22T15:00:00Z"))
    );
    // The run's session is ended and kept, with the prompt and the reply.
    let session = h.store.session(ran.session.id).await.unwrap().unwrap();
    assert!(session.ended_at.is_some());
    assert_eq!(h.texts(session.id).await.len(), 2);

    // The next morning.
    assert_eq!(h.ran().await.at, ts("2026-10-23T15:00:00Z"));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn active_hours_skip_the_times_outside_them() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"check-in\"\ncron = \"*/30 * * * *\"\nactive_hours = \"08:00-09:00\"\n\
         prompt = \"Anything?\"\n",
        &[],
    )
    .await;
    h.start().await;
    // 07:30 is skipped; 08:00 and 08:30 run; 09:00 to 07:30 the next day are skipped.
    assert_eq!(h.ran().await.at, ts("2026-10-22T15:00:00Z"));
    assert_eq!(h.ran().await.at, ts("2026-10-22T15:30:00Z"));
    // Each run's reply is delivered after its turn, so wait for the second before the next day.
    assert_eq!(h.delivered(2).await.len(), 2);
    assert_eq!(h.ran().await.at, ts("2026-10-23T15:00:00Z"));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn runs_missed_while_down_are_coalesced_into_one_at_startup() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"hourly\"\ncron = \"0 * * * *\"\nprompt = \"Tick.\"\n\
         [[jobs]]\nid = \"new\"\ncron = \"0 * * * *\"\nprompt = \"Tock.\"\n",
        &[],
    )
    .await;
    // `hourly` last ran five hours ago; `new` never ran.
    h.store
        .job_set_last_run(
            AgentId::new(AGENT).unwrap(),
            config_key("hourly"),
            ts("2026-10-22T09:00:00Z"),
        )
        .await
        .unwrap();
    h.start().await;
    let first = h.ran().await;
    assert_eq!(first.at, base());
    assert!(first.prompt.contains("Tick."));
    h.assert_nothing_ran().await;
    assert_eq!(h.last_run(config_key("hourly")).await, Some(base()));

    // Both at the next hour, once each.
    let mut next = [h.ran().await, h.ran().await];
    next.sort_by_key(|ran| ran.prompt.contains("Tock."));
    assert!(next.iter().all(|ran| ran.at == ts("2026-10-22T15:00:00Z")));
    assert!(next[1].prompt.contains("Tock."));
    h.assert_nothing_ran().await;
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn no_reply_sends_and_keeps_nothing() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"quiet\"\ncron = \"0 8 * * *\"\nprompt = \"Anything?\"\n\
         [[jobs]]\nid = \"quiet-dm\"\ncron = \"0 9 * * *\"\nsession = \"dm\"\nprompt = \"And now?\"\n",
        &[],
    )
    .await;
    let dm = h.dm(&["Hi", "Hello."]).await;
    h.reply_with(" NO_REPLY\n");
    h.reply_with("NO_REPLY");
    h.start().await;

    // Isolated: no delivery, no DM message, and the run's session is gone.
    let ran = h.ran().await;
    h.settle().await;
    assert!(h.deliveries().is_empty());
    assert_eq!(h.store.session(ran.session.id).await.unwrap(), None);
    assert_eq!(h.texts(dm.id).await, ["Hi", "Hello."]);

    // In the DM: the run's prompt and reply are removed again.
    let ran = h.ran().await;
    assert_eq!(ran.session.id, dm.id);
    h.settle().await;
    assert!(h.deliveries().is_empty());
    assert_eq!(h.texts(dm.id).await, ["Hi", "Hello."]);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_isolated_reply_is_delivered_and_appended_to_the_dm_with_an_excerpt_in_the_prompt() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"digest\"\ncron = \"0 8 * * *\"\ndm_excerpt = 2\n\
         model = \"ollama/qwen3:8b\"\nprompt_file = \"digest.md\"\n",
        &[("digest.md", "Summarize the day.\n")],
    )
    .await;
    let dm = h
        .dm(&["Book the dentist", "Booked for Friday.", "Thanks"])
        .await;
    h.reply_with("Your digest.");
    h.start().await;

    let ran = h.ran().await;
    assert_eq!(ran.session.kind, SessionKind::Job);
    assert_eq!(ran.model.as_deref(), Some("ollama/qwen3:8b"));
    assert!(
        ran.prompt.ends_with(
            "The latest messages of your DM with the user, oldest first:\n\n\
             **You:** Booked for Friday.\n\n**User:** Thanks\n\nSummarize the day."
        ),
        "{}",
        ran.prompt
    );
    assert_eq!(h.delivered(1).await[0].markdown, "Your digest.");
    h.settle().await;
    assert_eq!(
        h.texts(dm.id).await,
        [
            "Book the dentist",
            "Booked for Friday.",
            "Thanks",
            "Your digest."
        ]
    );

    // The prompt file is read at each run.
    std::fs::write(
        h.registry.config().agents[&AgentId::new(AGENT).unwrap()]
            .dir
            .join("digest.md"),
        "Just the headlines.",
    )
    .unwrap();
    assert!(h.ran().await.prompt.ends_with("Just the headlines."));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_dm_job_runs_as_its_own_turn_in_the_dm_after_the_turn_ahead() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"check-in\"\ncron = \"0 8 * * *\"\nsession = \"dm\"\n\
         prompt = \"How's it going?\"\n",
        &[],
    )
    .await;
    let dm = h.dm(&["Hi", "Hello."]).await;
    h.start().await;
    // A turn is running in the DM when the job fires.
    let busy = h.sessions.hold(dm.id).await.unwrap();
    h.advance_to(ts("2026-10-22T15:00:00Z")).await;
    h.assert_nothing_ran().await;
    drop(busy);

    let ran = h.ran().await;
    assert_eq!(ran.session.id, dm.id);
    assert_eq!(ran.session.kind, SessionKind::Direct);
    assert_eq!(h.delivered(1).await[0].markdown, "Report.");
    // The prompt and reply are in the DM already; nothing is appended twice.
    let texts = h.texts(dm.id).await;
    assert_eq!(texts.len(), 4);
    assert!(texts[2].ends_with("How's it going?"));
    assert_eq!(texts[3], "Report.");
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_dm_job_without_a_dm_runs_in_a_session_of_its_own() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"check-in\"\ncron = \"0 8 * * *\"\nsession = \"dm\"\nprompt = \"Hi?\"\n",
        &[],
    )
    .await;
    h.start().await;
    let ran = h.ran().await;
    assert_eq!(ran.session.kind, SessionKind::Job);
    assert_eq!(h.delivered(1).await.len(), 1);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn idle_only_waits_while_the_dm_is_busy() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"nudge\"\ncron = \"0 8 * * *\"\nidle_only = true\nprompt = \"Nudge.\"\n",
        &[],
    )
    .await;
    let dm = h.dm(&["Hi"]).await;
    h.start().await;
    let busy = h.sessions.hold(dm.id).await.unwrap();
    h.advance_to(ts("2026-10-22T15:00:00Z")).await;
    // Busy for ten more minutes: the isolated run waits too.
    h.advance_to(ts("2026-10-22T15:10:00Z")).await;
    h.assert_nothing_ran().await;
    assert_eq!(h.last_run(config_key("nudge")).await, None);
    let released = h.now();
    drop(busy);
    let ran = h.ran().await;
    // It ran once the DM went quiet, and its run is recorded from then.
    assert!(ran.at >= released && ran.at < ts("2026-10-22T15:10:01Z"));
    assert_eq!(ran.session.kind, SessionKind::Job);
    let last = h.last_run(config_key("nudge")).await.unwrap();
    assert!(last >= released && last <= ran.at);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn ask_to_routes_approval_prompts_through_the_delivery_and_answers_through_the_dm() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"early\"\ncron = \"0 8 * * *\"\nask_to = \"deliver_to\"\nprompt = \"Tidy.\"\n\
         [[jobs]]\nid = \"tidy\"\ncron = \"0 9 * * *\"\nask_to = \"deliver_to\"\n\
         prompt = \"Tidy.\"\n",
        &[],
    )
    .await;
    h.start().await;
    // Without a DM to take the answer, ask tools are denied.
    let ran = h.ran().await;
    assert!(ran.approvals.is_none());

    let dm = h.dm(&["Hi"]).await;
    let ran = h.ran().await;
    let target = ran.approvals.expect("no approval target");
    assert_eq!(Some(&target.key), dm.key().as_ref());
    assert_eq!(target.user, owner());
    assert_eq!(target.reply.conversation().as_str(), "owner");
    let outbox = target.outbox.expect("prompts don't go through the outbox");
    outbox
        .send(
            "4821",
            "Allow `x`? Reply `yes 4821` or `no 4821`.",
            Duration::from_secs(600),
        )
        .await
        .unwrap();
    let prompt = h.deliveries().pop().unwrap();
    assert_eq!(
        prompt,
        Delivery {
            user: owner(),
            kind: DeliveryKind::ApprovalPrompt,
            markdown: "Scheduled job `tidy`: Allow `x`? Reply `yes 4821` or `no 4821`.".into(),
            expires_at: Some(h.now().checked_add(Duration::from_secs(600)).unwrap()),
        }
    );
    let id = h.delivery.queued.lock().unwrap().last().unwrap().0;
    outbox.withdraw("4821");
    h.settle().await;
    assert_eq!(*h.delivery.removed.lock().unwrap(), [id]);
    // Withdrawn once.
    outbox.withdraw("4821");
    h.settle().await;
    assert_eq!(h.delivery.removed.lock().unwrap().len(), 1);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn runtime_jobs_run_as_their_creator_and_one_shots_are_deleted() {
    let mut h = Harness::new("", &[]).await;
    h.start().await;
    let agent = AgentId::new(AGENT).unwrap();
    let once = h
        .store
        .job_create(
            agent.clone(),
            owner(),
            json!({"at": "2026-10-22T15:00:00Z", "prompt": "Tea time."}),
            h.now(),
        )
        .await
        .unwrap();
    let cancelled = h
        .store
        .job_create(
            agent.clone(),
            owner(),
            json!({"at": "2026-10-22T16:00:00Z", "prompt": "Never."}),
            h.now(),
        )
        .await
        .unwrap();
    h.changes.notify();
    h.settle().await;
    h.store.job_delete(cancelled.id).await.unwrap();
    h.changes.notify();

    let ran = h.ran().await;
    assert_eq!(ran.at, ts("2026-10-22T15:00:00Z"));
    assert_eq!(ran.requester, Some(owner()));
    assert!(ran.prompt.contains("Scheduled job #1."));
    assert!(ran.prompt.ends_with("Tea time."));
    h.settle().await;
    assert_eq!(h.store.job(once.id).await.unwrap(), None);
    // The cancelled one never runs.
    h.advance_to(ts("2026-10-22T17:00:00Z")).await;
    h.assert_nothing_ran().await;
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_one_shot_missed_while_down_runs_at_startup() {
    let mut h = Harness::new("", &[]).await;
    let job = h
        .store
        .job_create(
            AgentId::new(AGENT).unwrap(),
            owner(),
            json!({"at": "2026-10-22T13:00:00Z", "prompt": "Late."}),
            ts("2026-10-22T12:00:00Z"),
        )
        .await
        .unwrap();
    h.start().await;
    assert_eq!(h.ran().await.at, base());
    h.settle().await;
    assert_eq!(h.store.job(job.id).await.unwrap(), None);
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_idle_only_run_waiting_past_its_active_hours_is_skipped() {
    let mut h = Harness::new(
        "[[jobs]]\nid = \"nudge\"\ncron = \"55 8 * * *\"\nidle_only = true\n\
         active_hours = \"08:00-09:00\"\nprompt = \"Nudge.\"\n",
        &[],
    )
    .await;
    let dm = h.dm(&["Hi"]).await;
    h.start().await;
    let busy = h.sessions.hold(dm.id).await.unwrap();
    // Fires at 08:55, waits; the DM goes quiet at 09:10, outside the hours.
    h.advance_to(ts("2026-10-22T16:10:00Z")).await;
    drop(busy);
    h.assert_nothing_ran().await;
    assert_eq!(h.last_run(config_key("nudge")).await, None);
    // The next day it runs inside the hours.
    assert_eq!(h.ran().await.at, ts("2026-10-23T15:55:00Z"));
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_job_stops_its_run_waiting_for_the_dm() {
    let mut h = Harness::new("", &[]).await;
    let dm = h.dm(&["Hi"]).await;
    h.start().await;
    let job = h
        .store
        .job_create(
            AgentId::new(AGENT).unwrap(),
            owner(),
            json!({"at": "2026-10-22T15:00:00Z", "prompt": "Ping.", "session": "dm"}),
            h.now(),
        )
        .await
        .unwrap();
    h.changes.notify();
    h.settle().await;
    let busy = h.sessions.hold(dm.id).await.unwrap();
    h.advance_to(ts("2026-10-22T15:01:00Z")).await;
    // Waiting behind the busy DM; cancelled meanwhile.
    h.store.job_delete(job.id).await.unwrap();
    h.changes.notify();
    h.settle().await;
    drop(busy);
    h.assert_nothing_ran().await;
    assert!(
        !h.sessions.is_running(dm.id),
        "the cancelled run still holds the DM"
    );
    h.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_job_that_cancels_itself_still_delivers_and_appends_its_reply() {
    let mut h = Harness::new("", &[]).await;
    let dm = h.dm(&["Has it shipped?"]).await;
    h.start().await;
    let job = h
        .store
        .job_create(
            AgentId::new(AGENT).unwrap(),
            owner(),
            json!({"cron": "0 * * * *", "prompt": "Shipped yet? If so, tell me and stop."}),
            h.now(),
        )
        .await
        .unwrap();
    h.changes.notify();
    *h.replies.cancel_own.lock().unwrap() = Some(job.id);
    h.reply_with("It shipped.");

    h.ran().await;
    assert_eq!(h.delivered(1).await[0].markdown, "It shipped.");
    h.settle().await;
    assert_eq!(h.texts(dm.id).await, ["Has it shipped?", "It shipped."]);
    // Cancelled: no further runs.
    h.advance_to(ts("2026-10-22T17:00:00Z")).await;
    h.assert_nothing_ran().await;
    h.stop().await;
}
