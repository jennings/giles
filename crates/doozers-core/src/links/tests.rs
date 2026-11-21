//! The identity links against a temporary store, stored users, and, for the pipeline tests, a fake
//! WhatsApp channel and a fake turn runner.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use doozers_store::{NewIdentity, NewUser, Role, Store, UserState, Verification};
use doozers_types::{
    AgentId, Channel, ChannelEvent, ChannelInfo, ConversationId, ConversationKind, ConversationRef,
    Error, EventId, InboundMessage, MessageId, NativeId, OutboundMessage, OutputMode, Result,
    Timestamp, UserId,
};
use futures::channel::mpsc as fmpsc;
use futures::stream::{BoxStream, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::commands::NoPendingApprovals;
use crate::config::{Config, InstanceName, LoadOptions};
use crate::paths::DataRoot;
use crate::router::Router;
use crate::routes::{Route, RouteList};
use crate::sessions::{
    AgentPolicies, AgentPolicy, ChannelLookup, NoEnvResets, Sessions, SessionsOptions, Turn,
    TurnOutcome, TurnRunner,
};
use crate::users::UserList;

fn at(secs: i64) -> Timestamp {
    Timestamp::from_second(1_800_000_000 + secs).unwrap()
}

fn id(text: &str) -> UserId {
    UserId::new(text).unwrap()
}

/// A clock the test moves.
struct TestClock(Mutex<Timestamp>);

impl TestClock {
    fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap();
        *now = now.checked_add(by).unwrap();
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap()
    }
}

/// The namespaces the running channels report.
struct Namespaces(BTreeSet<String>);

impl ChannelNamespaces for Namespaces {
    fn namespaces(&self) -> BTreeSet<String> {
        self.0.clone()
    }
}

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    users: Arc<Users>,
    list: UserList,
    clock: Arc<TestClock>,
    links: Arc<Links>,
}

/// Two users, `ann` and `bob`, who can sign in to the web, and channels in the namespaces `web`,
/// `whatsapp`, `discord`, and `irc:libera.chat`.
async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("doozers.db")).await.unwrap();
    let root = DataRoot::new(dir.path(), &dir.path().join("tmp")).unwrap();
    std::fs::write(root.config_file(), "").unwrap();
    let config = Config::load_with(
        &root,
        &LoadOptions {
            env: &|_| None,
            doozers_exe: None,
            ..LoadOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let list = UserList::default();
    let users = Arc::new(
        Users::open(store.clone(), list.clone(), &config, &root.config_file())
            .await
            .unwrap(),
    );
    for (name, role) in [("ann", Role::Member), ("bob", Role::Admin)] {
        let new = NewUser {
            id: id(name),
            username: name.to_owned(),
            display_name: name.to_uppercase(),
            role,
            timezone: None,
            default_identity: Some(format!("web:{name}")),
        };
        let web = NewIdentity {
            namespace: "web".to_owned(),
            native_id: name.to_owned(),
            verification: Verification::Admin,
        };
        users.create(new, vec![web], at(0)).await.unwrap();
    }
    let clock = Arc::new(TestClock(Mutex::new(at(1))));
    let namespaces = ["web", "whatsapp", "discord", "irc:libera.chat"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let links = Arc::new(Links::new(
        store.clone(),
        users.clone(),
        Arc::new(Namespaces(namespaces)),
        clock.clone(),
    ));
    World {
        _dir: dir,
        store,
        users,
        list,
        clock,
        links,
    }
}

fn request(namespace: &str, sender: &str, code: Option<&str>) -> LinkRequest {
    LinkRequest {
        namespace: namespace.to_owned(),
        sender: NativeId::new(sender).unwrap(),
        conversation: ConversationId::new(sender).unwrap(),
        code: code.map(str::to_owned),
        linked: false,
    }
}

impl World {
    /// What the core answers to `request` now.
    async fn link(&self, request: &LinkRequest) -> Option<String> {
        self.links.link(request, self.clock.now()).await.unwrap()
    }

    /// The user the router would resolve `<namespace>:<sender>` to now.
    fn resolve(&self, namespace: &str, sender: &str) -> Option<UserId> {
        self.list
            .get()
            .resolve(namespace, &NativeId::new(sender).unwrap())
            .map(|user| user.id.clone())
    }
}

const PHONE: &str = "+15551234567";

#[tokio::test]
async fn the_right_code_links_the_sender_to_the_user_who_asked() {
    let w = world().await;
    let code = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    assert_eq!(code.namespace, "whatsapp");
    assert_eq!(code.code.len(), CODE_LEN);
    assert!(
        code.code.bytes().all(|b| ALPHABET.contains(&b)),
        "{}",
        code.code
    );
    assert_eq!(code.command(), format!("/link {}", code.code));
    assert_eq!(
        code.expires_at,
        w.clock.now().checked_add(CODE_TTL).unwrap()
    );
    assert_eq!(w.resolve("whatsapp", PHONE), None);

    let reply = w
        .link(&request("whatsapp", PHONE, Some(&code.code)))
        .await
        .unwrap();
    assert!(reply.starts_with("Done."), "{reply}");
    assert!(reply.contains("ANN"), "{reply}");
    // The router's snapshot has it at once, as a `link` identity of the user.
    assert_eq!(w.resolve("whatsapp", PHONE), Some(id("ann")));
    let ann = w.links.user(&id("ann")).unwrap();
    let linked = ann
        .identities
        .iter()
        .find(|i| i.identity.as_str() == "whatsapp:+15551234567")
        .unwrap();
    assert_eq!(linked.verification, Verification::Link);
    assert_eq!(linked.verified_at, w.clock.now());
    // The code is spent: it can't be used again, and the page doesn't list it.
    assert!(w.links.codes(&id("ann")).await.unwrap().is_empty());
    let reply = w
        .link(&request("whatsapp", "+15557654321", Some(&code.code)))
        .await
        .unwrap();
    assert!(reply.starts_with("That code didn't work."), "{reply}");
    assert_eq!(w.resolve("whatsapp", "+15557654321"), None);
}

#[tokio::test]
async fn the_code_is_typed_in_either_case() {
    let w = world().await;
    let code = w.links.start(&id("bob"), "discord").await.unwrap();
    let typed = code.code.to_ascii_lowercase();
    let reply = w
        .link(&request("discord", "82198898841029460", Some(&typed)))
        .await
        .unwrap();
    assert!(reply.starts_with("Done."), "{reply}");
    assert_eq!(w.resolve("discord", "82198898841029460"), Some(id("bob")));
}

#[tokio::test]
async fn a_wrong_code_gets_one_answer_whether_or_not_any_code_exists() {
    let w = world().await;
    let nothing = w
        .link(&request("whatsapp", "+15550000001", Some("ABCD2345")))
        .await
        .unwrap();
    w.links.start(&id("ann"), "whatsapp").await.unwrap();
    let pending = w
        .link(&request("whatsapp", "+15550000002", Some("ABCD2345")))
        .await
        .unwrap();
    assert_eq!(nothing, pending);
    assert!(pending.contains("4 tries left"), "{pending}");
    // A malformed code and no code at all cost a try and say the same.
    for code in [Some("short"), Some("ABCD-2345"), Some("ABCD234!"), None] {
        let reply = w
            .link(&request("whatsapp", "+15550000003", code))
            .await
            .unwrap();
        assert!(
            reply.starts_with("That code didn't work."),
            "{code:?}: {reply}"
        );
    }
}

#[tokio::test]
async fn five_wrong_codes_lock_the_sender_out_for_an_hour_and_even_the_right_code_fails() {
    let w = world().await;
    let code = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    let wrong = request("whatsapp", PHONE, Some("AAAAAAAA"));
    for left in (1..SENDER_ATTEMPTS).rev() {
        let reply = w.link(&wrong).await.unwrap();
        let tries = if left == 1 {
            "1 try left"
        } else {
            &format!("{left} tries left")
        };
        assert!(reply.contains(tries), "{reply}");
    }
    // The last try says the sender is locked out; after that, nothing is said at all.
    let reply = w.link(&wrong).await.unwrap();
    assert!(reply.starts_with("Too many wrong codes."), "{reply}");
    let right = request("whatsapp", PHONE, Some(&code.code));
    assert_eq!(w.link(&right).await, None);
    assert_eq!(w.link(&wrong).await, None);
    assert_eq!(w.resolve("whatsapp", PHONE), None);
    // Another sender isn't affected, and the lock is on the identity, not the code.
    assert!(
        w.link(&request("whatsapp", "+15557654321", Some("AAAAAAAA")))
            .await
            .is_some()
    );
    // The lock ends an hour after it began. The code has expired by then, so the sender asks for
    // a new one, which works.
    w.clock.advance(LOCKOUT);
    let reply = w.link(&right).await.unwrap();
    assert!(
        reply.contains("4 tries left"),
        "an expired code is wrong: {reply}"
    );
    let fresh = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    let reply = w
        .link(&request("whatsapp", PHONE, Some(&fresh.code)))
        .await
        .unwrap();
    assert!(reply.starts_with("Done."), "{reply}");
}

#[tokio::test]
async fn wrong_codes_are_counted_for_a_window_and_a_success_clears_them() {
    let w = world().await;
    let wrong = request("whatsapp", PHONE, Some("AAAAAAAA"));
    for _ in 0..3 {
        w.link(&wrong).await.unwrap();
    }
    // An hour after the first wrong code the count starts again.
    w.clock.advance(SENDER_WINDOW);
    let reply = w.link(&wrong).await.unwrap();
    assert!(reply.contains("4 tries left"), "{reply}");
    // A success forgets the count.
    let code = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    assert!(
        w.link(&request("whatsapp", PHONE, Some(&code.code)))
            .await
            .unwrap()
            .starts_with("Done.")
    );
    w.links
        .remove_identity(
            &id("ann"),
            &Identity::new(format!("whatsapp:{PHONE}")).unwrap(),
        )
        .await
        .unwrap();
    let reply = w.link(&wrong).await.unwrap();
    assert!(reply.contains("4 tries left"), "{reply}");
}

#[tokio::test]
async fn an_expired_code_does_not_link_and_the_page_says_it_expired() {
    let w = world().await;
    let code = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    let state = |codes: Vec<LinkCodeInfo>| {
        codes
            .into_iter()
            .map(|c| (c.namespace, c.state))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        state(w.links.codes(&id("ann")).await.unwrap()),
        [("whatsapp".to_owned(), CodeState::Pending)]
    );
    w.clock.advance(CODE_TTL - Duration::from_secs(1));
    // One second before it runs out it still works, if it is right, so check by asking for the
    // state first and linking after the expiry.
    assert_eq!(
        state(w.links.codes(&id("ann")).await.unwrap()),
        [("whatsapp".to_owned(), CodeState::Pending)]
    );
    w.clock.advance(Duration::from_secs(1));
    assert_eq!(
        state(w.links.codes(&id("ann")).await.unwrap()),
        [("whatsapp".to_owned(), CodeState::Expired)]
    );
    let reply = w
        .link(&request("whatsapp", PHONE, Some(&code.code)))
        .await
        .unwrap();
    assert!(reply.starts_with("That code didn't work."), "{reply}");
    assert_eq!(w.resolve("whatsapp", PHONE), None);
    // An expired code is kept a day, then pruned when the next one is made.
    w.clock.advance(EXPIRED_KEPT);
    w.links.start(&id("bob"), "discord").await.unwrap();
    assert!(w.links.codes(&id("ann")).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_code_that_has_had_too_many_wrong_tries_is_cancelled() {
    let w = world().await;
    let code = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    // Strangers, each with tries of their own: every wrong code counts against the pending code.
    let mut sent = 0;
    for stranger in 0.. {
        let sender = format!("+1555000{stranger:04}");
        for _ in 0..SENDER_ATTEMPTS - 1 {
            if sent == CODE_ATTEMPTS {
                break;
            }
            w.link(&request("whatsapp", &sender, Some("AAAAAAAA")))
                .await
                .unwrap();
            sent += 1;
        }
        if sent == CODE_ATTEMPTS {
            break;
        }
    }
    let codes = w.links.codes(&id("ann")).await.unwrap();
    assert_eq!(codes[0].state, CodeState::Cancelled);
    // The right code no longer works, from a sender who never got a wrong one.
    let reply = w
        .link(&request("whatsapp", PHONE, Some(&code.code)))
        .await
        .unwrap();
    assert!(reply.starts_with("That code didn't work."), "{reply}");
    assert_eq!(w.resolve("whatsapp", PHONE), None);
    // Starting again makes a new code, which works.
    let fresh = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    assert!(
        w.link(&request("whatsapp", PHONE, Some(&fresh.code)))
            .await
            .unwrap()
            .starts_with("Done.")
    );
}

#[tokio::test]
async fn a_code_works_in_its_own_namespace_only() {
    let w = world().await;
    let code = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    let reply = w
        .link(&request("discord", "82198898841029460", Some(&code.code)))
        .await
        .unwrap();
    assert!(reply.starts_with("That code didn't work."), "{reply}");
    assert_eq!(w.resolve("discord", "82198898841029460"), None);
    // The code is still good where it was made for.
    assert!(
        w.link(&request("whatsapp", PHONE, Some(&code.code)))
            .await
            .unwrap()
            .starts_with("Done.")
    );
}

#[tokio::test]
async fn a_linked_sender_is_refused_without_a_count_and_keeps_the_code() {
    let w = world().await;
    let code = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    let mut known = request("whatsapp", PHONE, Some(&code.code));
    known.linked = true;
    for _ in 0..SENDER_ATTEMPTS + 2 {
        let reply = w.link(&known).await.unwrap();
        assert!(
            reply.starts_with("This account is already linked"),
            "{reply}"
        );
    }
    // Nothing was counted, and the code is unused.
    let codes = w.links.codes(&id("ann")).await.unwrap();
    assert_eq!(codes[0].state, CodeState::Pending);
    assert_eq!(w.resolve("whatsapp", PHONE), None);
}

#[tokio::test]
async fn an_identity_another_user_has_cant_be_claimed_with_a_code() {
    let w = world().await;
    w.users
        .add_identity(
            &id("bob"),
            "whatsapp",
            PHONE,
            Verification::Imported,
            w.clock.now(),
        )
        .await
        .unwrap();
    let code = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    // The router would mark this sender as linked, but a message that raced a change gets here
    // unmarked: the answer is the generic failure and the identity stays bob's.
    let reply = w
        .link(&request("whatsapp", PHONE, Some(&code.code)))
        .await
        .unwrap();
    assert!(reply.starts_with("That code didn't work."), "{reply}");
    assert_eq!(w.resolve("whatsapp", PHONE), Some(id("bob")));
    assert_eq!(
        w.links.codes(&id("ann")).await.unwrap()[0].state,
        CodeState::Pending
    );
}

#[tokio::test]
async fn a_namespace_no_channel_reports_is_rejected_with_the_ones_that_exist() {
    let w = world().await;
    let err = w
        .links
        .start(&id("ann"), "discord:discord")
        .await
        .unwrap_err();
    assert!(matches!(err, LinkError::UnknownNamespace { .. }), "{err}");
    assert_eq!(
        err.to_string(),
        "No running channel uses the identity namespace `discord:discord`. The namespaces are \
         `discord`, `irc:libera.chat`, `whatsapp`."
    );
    for namespace in ["", "irc", "WhatsApp", "whatsapp:"] {
        let err = w.links.start(&id("ann"), namespace).await.unwrap_err();
        assert!(
            matches!(err, LinkError::UnknownNamespace { .. }),
            "{namespace:?}: {err}"
        );
    }
    // Nothing was stored.
    assert!(w.links.codes(&id("ann")).await.unwrap().is_empty());
    assert_eq!(
        w.links.namespaces(),
        ["discord", "irc:libera.chat", "whatsapp"]
    );
}

#[tokio::test]
async fn the_web_namespace_is_not_linked_and_a_channel_less_install_says_so() {
    let w = world().await;
    for namespace in ["web", "web:proxy"] {
        let err = w.links.start(&id("ann"), namespace).await.unwrap_err();
        assert!(matches!(err, LinkError::WebNamespace), "{namespace}: {err}");
    }
    let only_web = Links::new(
        w.store.clone(),
        w.users.clone(),
        Arc::new(Namespaces(BTreeSet::from(["web".to_owned()]))),
        w.clock.clone(),
    );
    let err = only_web.start(&id("ann"), "whatsapp").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "No running channel uses the identity namespace `whatsapp`. No channel besides the web \
         chat is running."
    );
}

#[tokio::test]
async fn only_an_active_user_can_make_a_code_and_a_new_code_replaces_the_old() {
    let w = world().await;
    let err = w.links.start(&id("nobody"), "whatsapp").await.unwrap_err();
    assert!(matches!(err, LinkError::UserUnavailable), "{err}");
    let first = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    let second = w.links.start(&id("ann"), "whatsapp").await.unwrap();
    assert_ne!(first.code, second.code);
    assert_eq!(w.links.codes(&id("ann")).await.unwrap().len(), 1);
    let reply = w
        .link(&request("whatsapp", PHONE, Some(&first.code)))
        .await
        .unwrap();
    assert!(reply.starts_with("That code didn't work."), "{reply}");
    // A disabled user's code can't be used, and they can't make one.
    w.users
        .set_state(&id("ann"), UserState::Disabled, w.clock.now())
        .await
        .unwrap();
    let err = w.links.start(&id("ann"), "discord").await.unwrap_err();
    assert!(matches!(err, LinkError::UserUnavailable), "{err}");
    let reply = w
        .link(&request("whatsapp", "+15557654321", Some(&second.code)))
        .await
        .unwrap();
    assert!(reply.starts_with("That code didn't work."), "{reply}");
}

#[tokio::test]
async fn a_user_removes_an_identity_and_sets_the_default_but_not_the_web_login() {
    let w = world().await;
    for (namespace, sender) in [("whatsapp", PHONE), ("discord", "82198898841029460")] {
        let code = w.links.start(&id("ann"), namespace).await.unwrap();
        w.link(&request(namespace, sender, Some(&code.code)))
            .await
            .unwrap();
    }
    let identity = |text: &str| Identity::new(text).unwrap();
    // The default is the first identity ann had, `web:ann`, and they choose another.
    let user = w
        .links
        .set_default_identity(&id("ann"), &identity("discord:82198898841029460"))
        .await
        .unwrap();
    assert_eq!(
        user.default_identity.as_ref().map(Identity::as_str),
        Some("discord:82198898841029460")
    );
    assert!(
        w.links
            .set_default_identity(&id("ann"), &identity("whatsapp:+1"))
            .await
            .is_err()
    );
    // Removing the default moves it to the oldest identity left.
    let user = w
        .links
        .remove_identity(&id("ann"), &identity("discord:82198898841029460"))
        .await
        .unwrap();
    assert_eq!(
        user.default_identity.as_ref().map(Identity::as_str),
        Some("web:ann")
    );
    assert_eq!(w.resolve("discord", "82198898841029460"), None);
    // The web login identity isn't removable here, and one the user doesn't have is refused.
    let err = w
        .links
        .remove_identity(&id("ann"), &identity("web:ann"))
        .await
        .unwrap_err();
    assert!(matches!(err, LinkError::WebNamespace), "{err}");
    assert_eq!(w.resolve("web", "ann"), Some(id("ann")));
    let err = w
        .links
        .remove_identity(&id("ann"), &identity("whatsapp:+19990000000"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, LinkError::Users(UserError::Invalid(_))),
        "{err}"
    );
    // Another user's identity isn't removable through ann.
    let code = w.links.start(&id("bob"), "discord").await.unwrap();
    w.link(&request("discord", "42", Some(&code.code)))
        .await
        .unwrap();
    let err = w
        .links
        .remove_identity(&id("ann"), &identity("discord:42"))
        .await
        .unwrap_err();
    assert!(
        matches!(err, LinkError::Users(UserError::Invalid(_))),
        "{err}"
    );
    assert_eq!(w.resolve("discord", "42"), Some(id("bob")));
}

#[test]
fn senders_are_counted_in_a_bounded_table_that_keeps_locks() {
    let mut limits = SenderLimits::default();
    let now = 1_000;
    for n in 0..MAX_TRACKED {
        assert_eq!(
            limits.failed(&format!("s{n}"), now),
            Failure::TriesLeft(SENDER_ATTEMPTS - 1)
        );
    }
    // Lock one, then fill the table with more senders: the lock stays and the table doesn't grow.
    for _ in 1..SENDER_ATTEMPTS {
        limits.failed("s0", now);
    }
    assert!(limits.locked("s0", now));
    for n in 0..MAX_TRACKED {
        limits.failed(&format!("t{n}"), now + 1);
    }
    assert!(limits.senders.len() <= MAX_TRACKED);
    assert!(limits.locked("s0", now + 1));
    // When every entry is a lock, a new sender is treated as locked.
    let mut full = SenderLimits::default();
    for n in 0..MAX_TRACKED {
        for _ in 0..SENDER_ATTEMPTS {
            full.failed(&format!("s{n}"), now);
        }
    }
    // A sender the table can't hold is locked before its code is looked at, so it gets no free
    // tries; one that has a count is judged by it.
    assert!(full.locked("new", now));
    assert!(full.locked("s7", now));
    assert!(!limits.locked("new", now));
    assert_eq!(full.failed("new", now), Failure::Full);
    assert_eq!(full.senders.len(), MAX_TRACKED);
    assert!(!full.locked("new", now + millis(LOCKOUT)));
    // Counts whose window ended make room.
    let later = now + millis(LOCKOUT) + 1;
    assert_eq!(
        full.failed("new", later),
        Failure::TriesLeft(SENDER_ATTEMPTS - 1)
    );
}

// The pipeline: a fake WhatsApp channel, the router over the stored users, and the session pump.

const AGENT: &str = "jeeves";
const WAIT: Duration = Duration::from_secs(10);

/// What the fake runner saw of a turn.
#[derive(Debug, PartialEq, Eq)]
struct Started {
    requester: Option<UserId>,
    texts: Vec<String>,
}

struct FakeRunner(mpsc::UnboundedSender<Started>);

#[async_trait]
impl TurnRunner for FakeRunner {
    async fn run_turn(&self, turn: Turn) -> TurnOutcome {
        self.0
            .send(Started {
                requester: turn.requester,
                texts: turn
                    .messages
                    .iter()
                    .map(|m| m.message.text.clone())
                    .collect(),
            })
            .unwrap();
        TurnOutcome::Completed
    }
}

/// A channel in the namespace `whatsapp`: events come from the test, acks and sends go back to it.
struct FakeWhatsApp {
    info: ChannelInfo,
    events: Mutex<VecDeque<fmpsc::UnboundedReceiver<Result<ChannelEvent>>>>,
    acks: mpsc::UnboundedSender<String>,
    sent: mpsc::UnboundedSender<OutboundMessage>,
}

#[async_trait]
impl Channel for FakeWhatsApp {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        match self.events.lock().unwrap().pop_front() {
            Some(rx) => Ok(rx.boxed()),
            None => Err(Error::unavailable("only one stream")),
        }
    }

    async fn ack(&self, event: &EventId) -> Result<()> {
        self.acks.send(event.to_string()).unwrap();
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        self.sent.send(msg).unwrap();
        Ok(MessageId::new("sent").unwrap())
    }
}

struct Lookup(Arc<FakeWhatsApp>);

impl ChannelLookup for Lookup {
    fn channel(&self, instance: &str) -> Option<Arc<dyn Channel>> {
        (instance == "whatsapp").then(|| self.0.clone() as Arc<dyn Channel>)
    }
}

struct Pipeline {
    w: World,
    sessions: Sessions,
    shutdown: CancellationToken,
    events: fmpsc::UnboundedSender<Result<ChannelEvent>>,
    acks: mpsc::UnboundedReceiver<String>,
    sent: mpsc::UnboundedReceiver<OutboundMessage>,
    turns: mpsc::UnboundedReceiver<Started>,
    next_event: u32,
}

impl Pipeline {
    async fn start() -> Self {
        let w = world().await;
        let (events, rx) = fmpsc::unbounded();
        let (acks_tx, acks) = mpsc::unbounded_channel();
        let (sent_tx, sent) = mpsc::unbounded_channel();
        let channel = Arc::new(FakeWhatsApp {
            info: ChannelInfo {
                identity_namespace: "whatsapp".to_owned(),
                output: OutputMode::Blocks,
                can_initiate: false,
            },
            events: Mutex::new(VecDeque::from([rx])),
            acks: acks_tx,
            sent: sent_tx,
        });
        let agent = AgentId::new(AGENT).unwrap();
        let agents: Arc<dyn AgentPolicies> =
            Arc::new(HashMap::from([(agent.clone(), AgentPolicy::default())]));
        let router = Router::new(
            w.list.clone(),
            RouteList::new(vec![Route {
                channel: None,
                kind: None,
                conversation: None,
                agent,
            }]),
            agents.clone(),
        );
        let (turns_tx, turns) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let sessions = Sessions::start(
            SessionsOptions {
                store: w.store.clone(),
                router,
                agents,
                channels: Arc::new(Lookup(channel.clone())),
                runner: Arc::new(FakeRunner(turns_tx)),
                approvals: Arc::new(NoPendingApprovals),
                env_resets: Arc::new(NoEnvResets),
                links: w.links.clone(),
                clock: w.clock.clone(),
            },
            shutdown.clone(),
        )
        .await
        .unwrap();
        sessions.attach_channel(
            InstanceName::new("whatsapp").unwrap(),
            "whatsapp".to_owned(),
            channel,
        );
        Self {
            w,
            sessions,
            shutdown,
            events,
            acks,
            sent,
            turns,
            next_event: 0,
        }
    }

    /// Delivers a message from `sender` and waits until the core has acked it.
    async fn deliver(&mut self, message: InboundMessage) {
        self.next_event += 1;
        let id = format!("e{}", self.next_event);
        let event = ChannelEvent::Message {
            id: EventId::new(&id).unwrap(),
            message,
        };
        self.events.unbounded_send(Ok(event)).unwrap();
        let acked = tokio::time::timeout(WAIT, self.acks.recv())
            .await
            .expect("the event wasn't acked")
            .unwrap();
        assert_eq!(acked, id);
    }

    /// The text of the next message the core sent.
    async fn reply(&mut self) -> String {
        let sent = tokio::time::timeout(WAIT, self.sent.recv())
            .await
            .expect("nothing was sent")
            .unwrap();
        sent.markdown
    }

    /// Asserts that the core sends nothing and starts no turn.
    async fn assert_quiet(&mut self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(sent) = self.sent.try_recv() {
            panic!("unexpected reply: {}", sent.markdown);
        }
        if let Ok(turn) = self.turns.try_recv() {
            panic!("unexpected turn: {turn:?}");
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.sessions.closed().await;
    }
}

fn from(sender: &str, kind: ConversationKind, text: &str) -> InboundMessage {
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

fn dm(sender: &str, text: &str) -> InboundMessage {
    from(sender, ConversationKind::Direct, text)
}

#[tokio::test]
async fn a_user_adds_a_whatsapp_identity_with_link_and_its_messages_are_theirs() {
    let mut p = Pipeline::start().await;
    let code = p.w.links.start(&id("ann"), "whatsapp").await.unwrap();

    // Before the link the number is nobody: its messages are dropped without a word.
    p.deliver(dm(PHONE, "hello?")).await;
    p.assert_quiet().await;

    // `/link` with the code is answered on the channel, makes the identity ann's, and never
    // reaches an agent or makes a session.
    p.deliver(dm(PHONE, &code.command())).await;
    let reply = p.reply().await;
    assert!(reply.starts_with("Done."), "{reply}");
    p.assert_quiet().await;
    assert!(p.w.store.open_sessions().await.unwrap().is_empty());
    assert_eq!(p.w.resolve("whatsapp", PHONE), Some(id("ann")));
    assert_eq!(
        p.w.store
            .user(&id("ann"))
            .await
            .unwrap()
            .unwrap()
            .identities[1]
            .verification,
        Verification::Link
    );

    // From then on its messages are ann's, as the turn's requester.
    p.deliver(dm(PHONE, "hello again")).await;
    let turn = tokio::time::timeout(WAIT, p.turns.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        turn,
        Started {
            requester: Some(id("ann")),
            texts: vec!["hello again".to_owned()],
        }
    );
    assert_eq!(p.w.store.open_sessions().await.unwrap().len(), 1);

    // The code is spent. A second number that tries it is told it didn't work, and is not linked.
    p.deliver(dm("+15557654321", &code.command())).await;
    let reply = p.reply().await;
    assert!(reply.starts_with("That code didn't work."), "{reply}");
    assert_eq!(p.w.resolve("whatsapp", "+15557654321"), None);

    // The linked number sending `/link` again is told it is linked, and nothing reaches an agent.
    p.deliver(dm(PHONE, "/link ABCD2345")).await;
    let reply = p.reply().await;
    assert!(
        reply.starts_with("This account is already linked"),
        "{reply}"
    );
    p.assert_quiet().await;
    p.stop().await;
}

#[tokio::test]
async fn link_from_a_sender_the_channel_did_not_verify_or_from_a_group_is_dropped() {
    let mut p = Pipeline::start().await;
    let code = p.w.links.start(&id("ann"), "whatsapp").await.unwrap();

    let mut forged = dm(PHONE, &code.command());
    forged.sender_authenticated = false;
    p.deliver(forged).await;
    p.assert_quiet().await;

    p.deliver(from(PHONE, ConversationKind::Group, &code.command()))
        .await;
    p.assert_quiet().await;

    // Neither got the code a try or linked anything: the right code still works.
    assert_eq!(p.w.resolve("whatsapp", PHONE), None);
    p.deliver(dm(PHONE, &code.command())).await;
    let reply = p.reply().await;
    assert!(reply.starts_with("Done."), "{reply}");
    p.stop().await;
}

#[tokio::test]
async fn wrong_codes_from_the_channel_count_and_lock_the_sender_out() {
    let mut p = Pipeline::start().await;
    let code = p.w.links.start(&id("ann"), "whatsapp").await.unwrap();
    for left in (1..SENDER_ATTEMPTS).rev() {
        p.deliver(dm(PHONE, "/link AAAAAAAA")).await;
        let reply = p.reply().await;
        assert!(reply.contains(&format!(" {left} ")), "{reply}");
    }
    p.deliver(dm(PHONE, "/link AAAAAAAA")).await;
    let reply = p.reply().await;
    assert!(reply.starts_with("Too many wrong codes."), "{reply}");
    // Locked out: the right code is dropped without an answer, and nothing is linked.
    p.deliver(dm(PHONE, &code.command())).await;
    p.assert_quiet().await;
    assert_eq!(p.w.resolve("whatsapp", PHONE), None);
    p.stop().await;
}

#[tokio::test]
async fn the_inbox_never_keeps_a_link_event() {
    let mut p = Pipeline::start().await;
    let code = p.w.links.start(&id("ann"), "whatsapp").await.unwrap();
    p.deliver(dm(PHONE, &code.command())).await;
    p.reply().await;
    // The event id is free: nothing was recorded under it, so nothing holds the code.
    let row = doozers_store::NewInboxEvent {
        instance: "whatsapp".to_owned(),
        event_id: EventId::new("e1").unwrap(),
        session: None,
        user: None,
        payload: serde_json::json!(null),
        state: doozers_store::InboxState::Done,
        received_at: at(5),
    };
    assert!(p.w.store.inbox_insert(row).await.unwrap().is_some());
    p.stop().await;
}
