//! The outbox against a real store and registry with fake channels, in paused Tokio time.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use giles_types::{
    ChannelEvent, ChannelInfo, Error, EventId, MediaId, MediaRef, MessageId, OutputMode, Result,
};
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::*;
use crate::config::{Config, LoadOptions};
use crate::paths::DataRoot;

const OWNER_DEFAULT: &str = "whatsapp:+15551234567";

/// Long enough that only a hang reaches it (paused time jumps straight there).
const WAIT: Duration = Duration::from_secs(48 * 60 * 60);

const GILES_TOML: &str = r#"
[owner]
identities = ["whatsapp:+15551234567"]

[channel.chat]
[channel.libera]
component = "irc"
[channel.oftc]
component = "irc"
[channel.phone]
component = "whatsapp"
"#;

fn base() -> Timestamp {
    Timestamp::from_second(1_800_000_000).unwrap()
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
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

/// A channel that fails the sends it's told to and records every attempt.
struct FakeChannel {
    info: ChannelInfo,
    /// Errors for the next sends, in order; once empty, sends succeed.
    script: Mutex<VecDeque<Error>>,
    /// Gates for the next sends, in order: each waits until its sender sends or is dropped
    /// (forever while the test keeps it).
    holds: Mutex<VecDeque<oneshot::Receiver<()>>>,
    /// When each send was attempted, since the test started.
    attempts: Mutex<Vec<Duration>>,
    start: Instant,
    delivered: mpsc::UnboundedSender<OutboundMessage>,
}

impl FakeChannel {
    fn fail_next(&self, errors: impl IntoIterator<Item = Error>) {
        self.script.lock().unwrap().extend(errors);
    }

    /// Makes the next send wait; it goes on when the returned sender sends or is dropped.
    fn hold_next(&self) -> oneshot::Sender<()> {
        let (release, hold) = oneshot::channel();
        self.holds.lock().unwrap().push_back(hold);
        release
    }

    fn attempts(&self) -> Vec<Duration> {
        self.attempts.lock().unwrap().clone()
    }
}

#[async_trait]
impl Channel for FakeChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        Ok(stream::pending().boxed())
    }

    async fn ack(&self, _: &EventId) -> Result<()> {
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        self.attempts.lock().unwrap().push(self.start.elapsed());
        let hold = self.holds.lock().unwrap().pop_front();
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        if let Some(error) = self.script.lock().unwrap().pop_front() {
            return Err(error);
        }
        self.delivered.send(msg).unwrap();
        Ok(MessageId::new("sent").unwrap())
    }
}

/// A registered fake channel and the messages it delivered.
struct Fake {
    channel: Arc<FakeChannel>,
    delivered: mpsc::UnboundedReceiver<OutboundMessage>,
}

impl Fake {
    /// The next delivered message (paused time runs on until it comes).
    async fn next(&mut self) -> OutboundMessage {
        tokio::time::timeout(WAIT, self.delivered.recv())
            .await
            .expect("nothing delivered")
            .unwrap()
    }

    fn nothing_delivered(&mut self) {
        assert!(
            self.delivered.try_recv().is_err(),
            "something was delivered"
        );
    }
}

struct Harness {
    _dir: TempDir,
    db: std::path::PathBuf,
    registry: Arc<Registry>,
    outbox: Outbox,
    config: Arc<Config>,
    start: Instant,
    shutdown: CancellationToken,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        std::fs::write(root.config_file(), GILES_TOML).unwrap();
        let config = Arc::new(
            Config::load_with(
                &root,
                &LoadOptions {
                    env: &|_| None,
                    giles_exe: None,
                    ..LoadOptions::default()
                },
            )
            .unwrap_or_else(|e| panic!("{e}")),
        );
        let db = dir.path().join("giles.db");
        let store = Store::open(db.clone()).await.unwrap();
        let registry = Arc::new(Registry::new(root, config.clone()));
        let start = Instant::now();
        let outbox = Outbox::new(
            store.clone(),
            registry.clone(),
            Arc::new(TestClock { start }),
        );
        Self {
            _dir: dir,
            db,
            registry,
            outbox,
            config,
            start,
            shutdown: CancellationToken::new(),
        }
    }

    /// Registers a fake channel for `instance` with `namespace`.
    fn register(&self, instance: &str, namespace: &str, can_initiate: bool) -> Fake {
        let (delivered_tx, delivered) = mpsc::unbounded_channel();
        let channel = Arc::new(FakeChannel {
            info: ChannelInfo {
                identity_namespace: namespace.to_owned(),
                output: OutputMode::Blocks,
                can_initiate,
            },
            script: Mutex::default(),
            holds: Mutex::default(),
            attempts: Mutex::default(),
            start: self.start,
            delivered: delivered_tx,
        });
        self.registry
            .register_channel(&InstanceName::new(instance).unwrap(), channel.clone())
            .unwrap();
        Fake { channel, delivered }
    }

    async fn start(&self) {
        self.outbox.start(self.shutdown.clone()).await.unwrap();
    }

    /// Stops the sender and waits for it.
    async fn stop(&self) {
        self.shutdown.cancel();
        self.outbox.closed().await;
    }

    /// Waits until every row is gone (a delivered row is deleted just after its send, and a stop
    /// may cut in between), then stops the sender.
    async fn stop_when_empty(&self) {
        for _ in 0..1000 {
            if self.rows().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let rows = self.rows();
        assert!(rows.is_empty(), "rows left: {rows:?}");
        self.stop().await;
    }

    /// Every row, due or not, read straight from the database.
    fn rows(&self) -> Vec<Row> {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, instance, recipient, kind, user_id, attempts, last_error, created_at,
                    next_attempt_at, expires_at FROM outbox ORDER BY id",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok(Row {
                id: OutboxId::new(r.get(0)?),
                instance: r.get(1)?,
                recipient: r.get(2)?,
                kind: r.get(3)?,
                user: r.get(4)?,
                attempts: r.get(5)?,
                last_error: r.get(6)?,
                created_at: r.get(7)?,
                next_attempt_at: r.get(8)?,
                expires_at: r.get(9)?,
            })
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    }

    fn job(&self, markdown: &str) -> OutboxItem {
        OutboxItem::to_owner(&self.config.owner, OutboxKind::JobReply, markdown)
    }

    fn to(identity: &str, kind: OutboxKind, markdown: &str) -> OutboxItem {
        OutboxItem::new(Identity::new(identity).unwrap(), kind, markdown)
    }
}

/// An `outbox` row; times in Unix milliseconds.
#[derive(Debug)]
struct Row {
    id: OutboxId,
    instance: String,
    recipient: String,
    kind: String,
    user: Option<String>,
    attempts: u32,
    last_error: Option<String>,
    created_at: i64,
    next_attempt_at: i64,
    expires_at: i64,
}

/// [`base`] plus `after`, in Unix milliseconds.
fn ms(after: Duration) -> i64 {
    base().as_millisecond() + i64::try_from(after.as_millis()).unwrap()
}

fn unavailable() -> Error {
    Error::unavailable("not connected")
}

#[tokio::test(start_paused = true)]
async fn delivers_to_the_default_identity_on_enqueue() {
    let harness = Harness::new().await;
    let mut phone = harness.register("phone", "whatsapp", true);
    harness.start().await;
    // The sender is idle; the enqueue wakes it.
    tokio::time::sleep(secs(10)).await;
    let media = MediaRef {
        id: MediaId::from_digest([7; 32]),
        media_type: "image/png".to_owned(),
        size: 3,
        name: Some("chart.png".to_owned()),
    };
    let id = harness
        .outbox
        .enqueue(OutboxItem {
            attachments: vec![media.clone()],
            ..harness.job("**Morning.** Rain at 3.")
        })
        .await
        .unwrap();
    let rows = harness.rows();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.id, id);
    assert_eq!(row.instance, "phone");
    assert_eq!(row.recipient, OWNER_DEFAULT);
    assert_eq!(row.kind, "job_reply");
    assert_eq!(row.user.as_deref(), Some("owner"));
    assert_eq!(row.created_at, ms(secs(10)));
    assert_eq!(row.expires_at, ms(secs(10) + DEFAULT_EXPIRY));

    let sent = phone.next().await;
    assert_eq!(
        sent.target,
        SendTarget::Recipient(NativeId::new("+15551234567").unwrap())
    );
    assert_eq!(sent.markdown, "**Morning.** Rain at 3.");
    assert_eq!(sent.attachments, [media]);
    assert_eq!(sent.reply_to, None);
    assert_eq!(phone.channel.attempts(), [secs(10)]);
    harness.stop_when_empty().await;
    assert!(!harness.outbox.remove(id).await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn retries_with_backoff_until_the_channel_accepts() {
    let harness = Harness::new().await;
    let mut phone = harness.register("phone", "whatsapp", true);
    phone
        .channel
        .fail_next([unavailable(), unavailable(), unavailable()]);
    harness.start().await;
    harness.outbox.enqueue(harness.job("hi")).await.unwrap();
    assert_eq!(phone.next().await.markdown, "hi");
    assert_eq!(
        phone.channel.attempts(),
        [secs(0), secs(1), secs(3), secs(7)]
    );
    harness.stop_when_empty().await;
    phone.nothing_delivered();
}

#[tokio::test(start_paused = true)]
async fn records_each_failure_on_the_row() {
    let harness = Harness::new().await;
    let phone = harness.register("phone", "whatsapp", true);
    phone.channel.fail_next([unavailable(), unavailable()]);
    harness.outbox.enqueue(harness.job("hi")).await.unwrap();
    // Driven pass by pass, without the loop.
    harness.outbox.inner.pass().await.unwrap();
    let row = harness.rows().remove(0);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.last_error.as_deref(), Some("not connected"));
    assert_eq!(row.next_attempt_at, ms(secs(1)));
    // Not due yet: nothing happens.
    tokio::time::advance(Duration::from_millis(999)).await;
    harness.outbox.inner.pass().await.unwrap();
    assert_eq!(phone.channel.attempts().len(), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    harness.outbox.inner.pass().await.unwrap();
    let row = harness.rows().remove(0);
    assert_eq!(row.attempts, 2);
    assert_eq!(row.next_attempt_at, ms(secs(3)));
}

#[tokio::test(start_paused = true)]
async fn honors_retry_after() {
    let harness = Harness::new().await;
    let mut phone = harness.register("phone", "whatsapp", true);
    phone.channel.fail_next([
        Error::Unavailable {
            message: "rate limited".to_owned(),
            retry_after: Some(secs(30)),
        },
        Error::Unavailable {
            message: "rate limited".to_owned(),
            retry_after: Some(secs(5)),
        },
    ]);
    harness.start().await;
    harness.outbox.enqueue(harness.job("hi")).await.unwrap();
    phone.next().await;
    // 30 s as asked, then 5 s although the backoff would say 2 s.
    assert_eq!(phone.channel.attempts(), [secs(0), secs(30), secs(35)]);
    harness.stop().await;
}

#[tokio::test(start_paused = true)]
async fn waits_for_a_channel_that_isnt_running_yet() {
    let harness = Harness::new().await;
    harness.start().await;
    harness.outbox.enqueue(harness.job("hi")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    // Attempts at 0 s and 1 s found no channel; the next is at 3 s.
    let row = harness.rows().remove(0);
    assert_eq!(row.instance, "phone");
    assert_eq!(row.attempts, 2);
    assert_eq!(
        row.last_error.as_deref(),
        Some("channel `phone` isn't running")
    );
    let mut phone = harness.register("phone", "whatsapp", true);
    phone.next().await;
    assert_eq!(phone.channel.attempts(), [secs(3)]);
    harness.stop_when_empty().await;
}

#[tokio::test(start_paused = true)]
async fn drops_a_row_the_channel_rejects() {
    for rejection in [
        Error::InvalidRequest("no such number".to_owned()),
        Error::NotFound("attachment".to_owned()),
    ] {
        let harness = Harness::new().await;
        let mut phone = harness.register("phone", "whatsapp", true);
        phone.channel.fail_next([rejection]);
        harness.start().await;
        harness.outbox.enqueue(harness.job("hi")).await.unwrap();
        tokio::time::sleep(secs(600)).await;
        assert_eq!(phone.channel.attempts().len(), 1);
        phone.nothing_delivered();
        assert!(harness.rows().is_empty());
        harness.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn retries_other_errors_until_the_channel_accepts() {
    let harness = Harness::new().await;
    let mut phone = harness.register("phone", "whatsapp", true);
    // E.g. a transport failure that isn't `Unavailable`.
    phone.channel.fail_next([Error::other("connection reset")]);
    harness.start().await;
    harness.outbox.enqueue(harness.job("hi")).await.unwrap();
    phone.next().await;
    assert_eq!(phone.channel.attempts(), [secs(0), secs(1)]);
    harness.stop_when_empty().await;
}

#[tokio::test(start_paused = true)]
async fn a_timed_out_send_is_retried() {
    let harness = Harness::new().await;
    let mut phone = harness.register("phone", "whatsapp", true);
    let _hung = phone.channel.hold_next();
    harness.start().await;
    harness.outbox.enqueue(harness.job("hi")).await.unwrap();
    phone.next().await;
    assert_eq!(phone.channel.attempts(), [secs(0), SEND_TIMEOUT + secs(1)]);
    harness.stop_when_empty().await;
}

#[tokio::test(start_paused = true)]
async fn a_timeout_defers_the_instances_other_rows_to_the_next_pass() {
    let harness = Harness::new().await;
    let mut phone = harness.register("phone", "whatsapp", true);
    let _hung = phone.channel.hold_next();
    harness.outbox.enqueue(harness.job("one")).await.unwrap();
    harness.outbox.enqueue(harness.job("two")).await.unwrap();
    harness.start().await;
    let first = phone.next().await;
    let second = phone.next().await;
    assert_eq!((&*first.markdown, &*second.markdown), ("one", "two"));
    // "two" waited instead of being tried (and possibly timing out) right after "one".
    let retry = SEND_TIMEOUT + secs(1);
    assert_eq!(phone.channel.attempts(), [secs(0), retry, retry]);
    harness.stop().await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_abandons_a_hung_send_and_keeps_its_row() {
    let harness = Harness::new().await;
    let phone = harness.register("phone", "whatsapp", true);
    let _hung = phone.channel.hold_next();
    harness.start().await;
    harness.outbox.enqueue(harness.job("hi")).await.unwrap();
    tokio::time::sleep(secs(1)).await;
    assert_eq!(phone.channel.attempts().len(), 1);
    harness.shutdown.cancel();
    tokio::time::timeout(secs(1), harness.outbox.closed())
        .await
        .expect("the sender waited for the hung send");
    let rows = harness.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].attempts, 0);
}

#[tokio::test(start_paused = true)]
async fn a_row_removed_during_a_pass_is_never_sent() {
    let harness = Harness::new().await;
    let mut phone = harness.register("phone", "whatsapp", true);
    let release = phone.channel.hold_next();
    harness.outbox.enqueue(harness.job("report")).await.unwrap();
    // Not an approval prompt, which `start` would drop since it's enqueued before it.
    let notice = harness
        .outbox
        .enqueue(harness.job("withdrawn"))
        .await
        .unwrap();
    // Both are due when the pass starts.
    harness.start().await;
    // While "report" is being sent, the second row is withdrawn (its approval was answered).
    tokio::time::sleep(secs(1)).await;
    assert!(harness.outbox.remove(notice).await.unwrap());
    release.send(()).unwrap();
    assert_eq!(phone.next().await.markdown, "report");
    tokio::time::sleep(secs(600)).await;
    assert_eq!(phone.channel.attempts(), [secs(0)]);
    phone.nothing_delivered();
    harness.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_row_expiring_during_a_pass_is_never_sent() {
    let harness = Harness::new().await;
    let mut phone = harness.register("phone", "whatsapp", true);
    let release = phone.channel.hold_next();
    harness.outbox.enqueue(harness.job("report")).await.unwrap();
    let notice = OutboxItem {
        expires_at: Some(base().checked_add(secs(10)).unwrap()),
        ..harness.job("soon stale")
    };
    harness.outbox.enqueue(notice).await.unwrap();
    harness.start().await;
    tokio::time::sleep(secs(20)).await;
    release.send(()).unwrap();
    assert_eq!(phone.next().await.markdown, "report");
    tokio::time::sleep(secs(600)).await;
    assert_eq!(phone.channel.attempts(), [secs(0)]);
    phone.nothing_delivered();
    assert!(harness.rows().is_empty());
    harness.stop().await;
}

#[tokio::test(start_paused = true)]
async fn rows_expire_at_their_deadline() {
    let harness = Harness::new().await;
    let phone = harness.register("phone", "whatsapp", true);
    phone.channel.fail_next((0..100).map(|_| unavailable()));
    let deadline = base().checked_add(secs(10)).unwrap();
    harness
        .outbox
        .enqueue(OutboxItem {
            expires_at: Some(deadline),
            ..OutboxItem::to_owner(
                &harness.config.owner,
                OutboxKind::ApprovalPrompt,
                "Allow `x`? Reply `yes 1234` or `no 1234`.",
            )
        })
        .await
        .unwrap();
    harness.outbox.inner.pass().await.unwrap();
    // Due again just before its deadline: still tried.
    tokio::time::advance(Duration::from_millis(9_999)).await;
    harness.outbox.inner.pass().await.unwrap();
    assert_eq!(phone.channel.attempts().len(), 2);
    assert_eq!(harness.rows().len(), 1);
    // At its deadline: dropped without another try.
    tokio::time::advance(Duration::from_millis(1)).await;
    harness.outbox.inner.pass().await.unwrap();
    assert_eq!(phone.channel.attempts().len(), 2);
    assert!(harness.rows().is_empty());
}

#[tokio::test(start_paused = true)]
async fn rows_without_an_expiry_last_a_day() {
    let harness = Harness::new().await;
    harness.start().await;
    harness.outbox.enqueue(harness.job("hi")).await.unwrap();
    tokio::time::sleep(DEFAULT_EXPIRY - Duration::from_millis(1)).await;
    let row = harness.rows().remove(0);
    // 1 s, 2 s, ... then every minute: 6 backoff steps (63 s), then one a minute.
    let minutes = (DEFAULT_EXPIRY.as_secs() - 63) / 60;
    assert_eq!(u64::from(row.attempts), 7 + minutes);
    // The sender wakes at the deadline and drops it (`rows_expire_at_their_deadline` checks the
    // exact boundary pass by pass).
    tokio::time::sleep(secs(1)).await;
    assert!(harness.rows().is_empty());
    harness.stop().await;
}

#[tokio::test(start_paused = true)]
async fn enqueue_checks_the_expiry() {
    let harness = Harness::new().await;
    harness.register("phone", "whatsapp", true);
    for kind in [OutboxKind::ApprovalPrompt, OutboxKind::LoginCode] {
        let item = OutboxItem::to_owner(&harness.config.owner, kind, "x");
        assert!(matches!(
            harness.outbox.enqueue(item).await,
            Err(OutboxError::MissingExpiry(k)) if k == kind
        ));
    }
    let item = OutboxItem {
        expires_at: Some(base()),
        ..harness.job("late")
    };
    assert!(matches!(
        harness.outbox.enqueue(item).await,
        Err(OutboxError::Expired { .. })
    ));
    assert!(harness.rows().is_empty());
}

#[tokio::test(start_paused = true)]
async fn startup_drops_the_approval_prompts_of_the_previous_process() {
    let harness = Harness::new().await;
    let prompt = OutboxItem {
        expires_at: Some(base().checked_add(secs(600)).unwrap()),
        ..OutboxItem::to_owner(&harness.config.owner, OutboxKind::ApprovalPrompt, "Allow?")
    };
    // Enqueued before the sender starts, as if left by the previous process.
    harness.outbox.enqueue(prompt).await.unwrap();
    harness.outbox.enqueue(harness.job("report")).await.unwrap();
    let mut phone = harness.register("phone", "whatsapp", true);
    harness.start().await;
    assert_eq!(phone.next().await.markdown, "report");
    harness.stop_when_empty().await;
    phone.nothing_delivered();
    assert_eq!(phone.channel.attempts().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_removed_prompt_is_never_sent() {
    let harness = Harness::new().await;
    harness.start().await;
    let prompt = OutboxItem {
        expires_at: Some(base().checked_add(secs(600)).unwrap()),
        ..OutboxItem::to_owner(&harness.config.owner, OutboxKind::ApprovalPrompt, "Allow?")
    };
    let id = harness.outbox.enqueue(prompt).await.unwrap();
    tokio::time::sleep(secs(2)).await;
    let mut phone = harness.register("phone", "whatsapp", true);
    // The approval was answered elsewhere before the channel came up.
    assert!(harness.outbox.remove(id).await.unwrap());
    assert!(!harness.outbox.remove(id).await.unwrap());
    tokio::time::sleep(secs(600)).await;
    assert!(phone.channel.attempts().is_empty());
    phone.nothing_delivered();
    harness.stop().await;
}

#[tokio::test(start_paused = true)]
async fn routes_by_identity_namespace() {
    let harness = Harness::new().await;
    let mut libera = harness.register("libera", "irc:libera.chat", true);
    let mut oftc = harness.register("oftc", "irc:oftc", true);
    harness.start().await;
    let item = Harness::to("irc:oftc:account:stephen", OutboxKind::Notice, "hello");
    harness.outbox.enqueue(item).await.unwrap();
    let sent = oftc.next().await;
    assert_eq!(
        sent.target,
        SendTarget::Recipient(NativeId::new("account:stephen").unwrap())
    );
    harness.stop().await;
    libera.nothing_delivered();
    assert!(libera.channel.attempts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_identity_of_several_stopped_instances_waits_for_its_channel() {
    let harness = Harness::new().await;
    harness.start().await;
    let item = Harness::to("irc:oftc:account:stephen", OutboxKind::Notice, "hello");
    harness.outbox.enqueue(item).await.unwrap();
    assert_eq!(harness.rows()[0].instance, "libera,oftc");
    tokio::time::sleep(secs(5)).await;
    // `libera` comes up with another namespace; `oftc` then gets it.
    let mut libera = harness.register("libera", "irc:libera.chat", true);
    tokio::time::sleep(secs(60)).await;
    let mut oftc = harness.register("oftc", "irc:oftc", true);
    oftc.next().await;
    harness.stop().await;
    libera.nothing_delivered();
    assert!(libera.channel.attempts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn refuses_identities_no_channel_can_reach() {
    let harness = Harness::new().await;
    harness.register("chat", "chat", false);
    let item = Harness::to("chat:owner", OutboxKind::LoginCode, "Code 123456");
    let item = OutboxItem {
        expires_at: Some(base().checked_add(secs(600)).unwrap()),
        ..item
    };
    match harness.outbox.enqueue(item).await {
        Err(OutboxError::CannotInitiate { instance, identity }) => {
            assert_eq!(instance.as_str(), "chat");
            assert_eq!(identity.as_str(), "chat:owner");
        }
        other => panic!("{other:?}"),
    }
    let item = Harness::to("email:me@example.com", OutboxKind::Notice, "hi");
    assert!(matches!(
        harness.outbox.enqueue(item).await,
        Err(OutboxError::Unowned(identity)) if identity.as_str() == "email:me@example.com"
    ));
    assert!(harness.rows().is_empty());
}

#[tokio::test(start_paused = true)]
async fn drops_a_row_whose_channel_turns_out_unable_to_initiate() {
    let harness = Harness::new().await;
    harness.start().await;
    harness
        .outbox
        .enqueue(Harness::to("chat:owner", OutboxKind::Notice, "hi"))
        .await
        .unwrap();
    tokio::time::sleep(secs(2)).await;
    let mut chat = harness.register("chat", "chat", false);
    tokio::time::sleep(secs(600)).await;
    assert!(harness.rows().is_empty());
    assert!(chat.channel.attempts().is_empty());
    chat.nothing_delivered();
    harness.stop().await;
}

#[test]
fn until_rounds_up_to_the_millisecond() {
    let now = base();
    let at = |nanos: u64| now.checked_add(Duration::from_nanos(nanos)).unwrap();
    assert_eq!(until(now, at(1)), Duration::from_millis(1));
    assert_eq!(until(now, at(1_000_000)), Duration::from_millis(1));
    assert_eq!(until(now, at(1_000_001)), Duration::from_millis(2));
    assert_eq!(until(now, now), Duration::ZERO);
    assert_eq!(until(at(5), now), Duration::ZERO);
}
