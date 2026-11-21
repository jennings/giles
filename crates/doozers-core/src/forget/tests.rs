//! Forgetting a user against a temporary data root, the real store, secret store, memory
//! databases, and workspace service (with the in-process fake environment), and a user with data
//! in every place that holds some.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use doozers_store::{
    BlobMeta, BlobStore, InboxState, InviteKind, MemoryOwner, MemoryViewer, NewIdentity,
    NewInboxEvent, NewKnownHost, NewMemory, NewOutbox, NewPasskey, NewUser, OidcProviderSettings,
    OutboxTarget, PasskeyKind, Role, SWEEP_GRACE, SessionKind, SignInMethod, UsageRecord,
    UserState, Verification, WorkspaceState,
};
use doozers_types::{
    AgentId, Block, ConversationId, EnvironmentInfo, EventId, MediaRef, Message, MessageMeta,
    NativeId, Timestamp, Usage,
};
use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::approvals::{ApprovalOutcome, ApprovalPresenter, ApprovalTarget, TurnApprovals};
use crate::config::{Config, InstanceName, LoadOptions, Secret};
use crate::registry::Registry;
use crate::secrets::{KeyFile, LocalSecrets, SecretMetadata};
use crate::sessions::{ChannelLookup, Reply};
use crate::ssh::{KeyType, SshKeys};
use crate::workspaces::fake::{Fake, TestClock};

const CONFIG: &str = "[environment.box]\ncomponent = \"docker\"\n\
    address = \"http://127.0.0.1:9000\"\n";

/// The user the tests forget. Nothing else in the data holds this text, so finding it means a
/// leftover.
const ZED: &str = "zed-4711";

fn id(name: &str) -> UserId {
    UserId::new(name).unwrap()
}

/// A word only `name`'s data holds, which is one token of a full-text index.
fn marker(name: &str) -> String {
    format!("{}marker", name.replace('-', ""))
}

fn agent(name: &str) -> AgentId {
    AgentId::new(name).unwrap()
}

fn chat(name: &str) -> ConversationId {
    ConversationId::new(name).unwrap()
}

/// Shows no prompt: the approval's user is what's under test.
struct Silent;

#[async_trait::async_trait]
impl ApprovalPresenter for Silent {
    async fn present(&self, _: &Reply, _: &str, _: &str) -> doozers_types::Result<()> {
        Ok(())
    }

    fn resolved(&self, _: &Reply, _: &str, _: ApprovalOutcome) {}
}

/// What the activity check says: busy from its `busy_from`th call on (0: never).
#[derive(Debug, Default)]
struct FakeActivity {
    calls: AtomicUsize,
    busy_from: AtomicUsize,
}

#[async_trait::async_trait]
impl UserActivity for FakeActivity {
    async fn is_busy(&self, _: &UserId) -> bool {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let from = self.busy_from.load(Ordering::SeqCst);
        from != 0 && call >= from
    }
}

/// Records the group sessions it is asked to write summaries for.
#[derive(Debug, Default)]
struct FakeSummaries {
    asked: std::sync::Mutex<Vec<SessionId>>,
}

#[async_trait::async_trait]
impl GroupSummaries for FakeSummaries {
    async fn regenerate(&self, session: SessionId) {
        self.asked.lock().unwrap().push(session);
    }
}

/// A secret store whose deletes can be made to fail, over the real one.
#[derive(Debug)]
struct Flaky {
    inner: Arc<dyn SecretProvider>,
    fail_deletes: AtomicBool,
}

#[async_trait::async_trait]
impl SecretProvider for Flaky {
    async fn get(&self, path: &SecretPath) -> Result<Option<Secret>, SecretError> {
        self.inner.get(path).await
    }

    async fn metadata(&self, path: &SecretPath) -> Result<Option<SecretMetadata>, SecretError> {
        self.inner.metadata(path).await
    }

    async fn list(&self, under: Option<&SecretPath>) -> Result<Vec<SecretMetadata>, SecretError> {
        self.inner.list(under).await
    }

    async fn put(
        &self,
        path: &SecretPath,
        value: &Secret,
        by: Option<&UserId>,
    ) -> Result<SecretMetadata, SecretError> {
        self.inner.put(path, value, by).await
    }

    async fn describe(
        &self,
        path: &SecretPath,
        description: &str,
        by: Option<&UserId>,
    ) -> Result<SecretMetadata, SecretError> {
        self.inner.describe(path, description, by).await
    }

    async fn delete(&self, path: &SecretPath) -> Result<bool, SecretError> {
        if self.fail_deletes.load(Ordering::SeqCst) {
            return Err(SecretError::Unavailable(Box::new(io::Error::other(
                "the secret store is down",
            ))));
        }
        self.inner.delete(path).await
    }
}

struct Harness {
    dir: TempDir,
    root: DataRoot,
    store: Store,
    registry: Arc<Registry>,
    users: Arc<Users>,
    secrets: Arc<Flaky>,
    ssh_keys: SshKeys,
    workspaces: Workspaces,
    fake: Arc<Fake>,
    memory_dbs: MemoryDbs,
    blobs: BlobStore,
    approvals: Arc<ApprovalBoard>,
    activity: Arc<FakeActivity>,
    summaries: Arc<FakeSummaries>,
    clock: Arc<TestClock>,
    forgetter: Forgetter,
}

impl Harness {
    /// An admin `root`, and the members `bob` and [`ZED`].
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        std::fs::write(dir.path().join("doozers.toml"), CONFIG).unwrap();
        let config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                doozers_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let registry = Arc::new(Registry::new(root.clone(), Arc::new(config)));
        let store = Store::open(dir.path().join("doozers.db")).await.unwrap();
        let clock = TestClock::new();
        let fake = Fake::new(clock.clone());
        registry
            .register_environment(
                &InstanceName::new("box").unwrap(),
                fake.clone(),
                EnvironmentInfo {
                    options_schema: json!({"type": "object"}),
                },
                crate::registry::EnvironmentCapabilities::default(),
            )
            .unwrap();
        let local = LocalSecrets::open(
            store.clone(),
            &KeyFile::new(&root, &root.secret_key_file()),
            clock.clone(),
        )
        .await
        .unwrap();
        let secrets = Arc::new(Flaky {
            inner: Arc::new(local),
            fail_deletes: AtomicBool::new(false),
        });
        let users = Arc::new(
            Users::open(
                store.clone(),
                registry.users(),
                &registry.config(),
                &root.config_file(),
            )
            .await
            .unwrap(),
        );
        let ssh_keys = SshKeys::new(
            store.clone(),
            secrets.clone(),
            clock.clone(),
            registry.users(),
        );
        let workspaces = Workspaces::new(
            store.clone(),
            registry.clone(),
            clock.clone(),
            ssh_keys.clone(),
        );
        let memory_dbs = MemoryDbs::new(root.clone(), doozers_store::JournalMode::Wal);
        let blobs = BlobStore::open(root.blobs_dir(), store.clone())
            .await
            .unwrap();
        let approvals = Arc::new(ApprovalBoard::new(Arc::new(Silent)));
        let activity = Arc::new(FakeActivity::default());
        let summaries = Arc::new(FakeSummaries::default());
        let forgetter = Forgetter::new(ForgetOptions {
            store: store.clone(),
            users: users.clone(),
            root: root.clone(),
            memory_dbs: memory_dbs.clone(),
            secrets: secrets.clone(),
            workspaces: workspaces.clone(),
            approvals: approvals.clone(),
            jobs: JobChanges::default(),
            activity: activity.clone(),
            summaries: summaries.clone(),
            clock: clock.clone(),
        });
        let harness = Self {
            dir,
            root,
            store,
            registry,
            users,
            secrets,
            ssh_keys,
            workspaces,
            fake,
            memory_dbs,
            blobs,
            approvals,
            activity,
            summaries,
            clock,
            forgetter,
        };
        harness.add_user("root", Role::Admin).await;
        harness.add_user("bob", Role::Member).await;
        harness.add_user(ZED, Role::Member).await;
        harness
    }

    async fn add_user(&self, name: &str, role: Role) {
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
        self.users
            .create(new, vec![web], self.clock.now())
            .await
            .unwrap();
    }

    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    fn user(&self, name: &str) -> Option<User> {
        self.users.list().get().user(&id(name)).cloned()
    }

    fn secret(&self, path: &str) -> SecretPath {
        SecretPath::new(path).unwrap()
    }

    /// An image message from `user` carrying `media`, or plain text.
    fn said(&self, text: &str, secs: u64, sender: &str, media: Option<MediaRef>) -> Message {
        let mut content = vec![Block::text(text)];
        content.extend(media.map(|media| Block::Image { media }));
        let mut meta = MessageMeta::new(self.now().checked_add(Duration::from_secs(secs)).unwrap());
        meta.sender = Some(id(sender));
        Message {
            role: doozers_types::Role::User,
            content,
            meta,
        }
    }

    fn replied(&self, text: &str, secs: u64) -> Message {
        let mut message = self.said(text, secs, "root", None);
        message.role = doozers_types::Role::Assistant;
        message.meta.sender = None;
        message
    }

    async fn blob(&self, content: &str) -> MediaRef {
        self.blobs
            .put(
                content.as_bytes(),
                1024,
                BlobMeta {
                    media_type: "image/png".into(),
                    name: None,
                },
                self.now(),
            )
            .await
            .unwrap()
    }

    async fn memory(&self, agent_id: &str, viewer: &MemoryViewer, text: &str, shared: bool) {
        let store = self.memory_dbs.get(&agent(agent_id)).await.unwrap();
        store
            .add(
                viewer,
                NewMemory {
                    content: text.to_owned(),
                    core: false,
                    shared,
                    source_session: None,
                },
                2200,
                self.now(),
            )
            .await
            .unwrap();
    }

    /// Gives `name` data in every place there is some: [`populate`] and its counts.
    async fn populate(&self, name: &str) -> Populated {
        let user = id(name);
        let now = self.now();
        // Conversation ids that don't name the user: a web chat's is random.
        let prefix = if name == ZED { "alpha" } else { "beta" };
        let (chat1, chat2) = (format!("{prefix}-chat1"), format!("{prefix}-chat2"));
        let with_group = name == ZED;
        let key = |agent_name: &str, channel: &str, conversation: &str| doozers_store::SessionKey {
            agent: agent(agent_name),
            channel: channel.to_owned(),
            conversation: chat(conversation),
        };

        // Two chats, one with an attachment only this user's messages refer to.
        let image = self.blob(&format!("{name}'s picture")).await;
        let (first, _) = self
            .store
            .open_session(
                key("jeeves", "web", &chat1),
                SessionKind::Direct,
                Some(user.clone()),
                now,
            )
            .await
            .unwrap();
        self.store
            .append_messages(
                first.id,
                vec![
                    self.said(
                        &format!("a secret about my {}", marker(name)),
                        1,
                        name,
                        Some(image.clone()),
                    ),
                    self.replied("noted", 2),
                ],
            )
            .await
            .unwrap();
        self.store
            .insert_summary(first.id, 1, 1, "they have a cat", now)
            .await
            .unwrap();
        let (second, _) = self
            .store
            .open_session(
                key("bunter", "web", &chat2),
                SessionKind::Direct,
                Some(user.clone()),
                now,
            )
            .await
            .unwrap();
        self.store
            .append_messages(second.id, vec![self.said("my tea order", 3, name, None)])
            .await
            .unwrap();
        self.store
            .archive_conversation("web", &chat(&chat2), &user, now)
            .await
            .unwrap();
        // The run of a job they scheduled.
        let run = self
            .store
            .create_job_session(agent("jeeves"), Some(user.clone()), now)
            .await
            .unwrap();
        self.store
            .append_messages(
                run.id,
                vec![self.said("remind me about the cat", 4, name, None)],
            )
            .await
            .unwrap();
        // A group session where they and bob spoke, and two summaries, for the user the tests
        // forget.
        let group = if with_group {
            let (group, _) = self
                .store
                .open_session(key("jeeves", "irc", "room"), SessionKind::Group, None, now)
                .await
                .unwrap();
            self.store
                .append_messages(
                    group.id,
                    vec![
                        self.said(
                            &format!("the vault code is {}", marker(name)),
                            5,
                            name,
                            None,
                        ),
                        self.replied("ok", 6),
                        self.said("bob says hello", 7, "bob", None),
                    ],
                )
                .await
                .unwrap();
            self.store
                .insert_summary(group.id, 1, 2, "the vault code was told", now)
                .await
                .unwrap();
            self.store
                .insert_summary(group.id, 1, 3, "the vault code was told, bob greeted", now)
                .await
                .unwrap();
            Some(group)
        } else {
            None
        };

        // Inbound and outbound messages.
        let mut inbound = vec![("e1", Some(first.id), name), ("e2", None, name)];
        inbound.extend(group.as_ref().map(|group| ("e3", Some(group.id), "bob")));
        for (event, session, sender) in inbound {
            self.store
                .inbox_insert(NewInboxEvent {
                    instance: "web".into(),
                    event_id: EventId::new(format!("{prefix}-{event}")).unwrap(),
                    session,
                    user: Some(id(sender)),
                    payload: json!({"text": "hi"}),
                    state: InboxState::Done,
                    received_at: now,
                })
                .await
                .unwrap();
        }
        let expires = now.checked_add(Duration::from_secs(3600)).unwrap();
        for (user_of, target) in [
            (Some(user.clone()), OutboxTarget::Conversation(chat(&chat1))),
            (
                None,
                OutboxTarget::Recipient(NativeId::new(format!("web:{name}")).unwrap()),
            ),
            (Some(id("bob")), OutboxTarget::Conversation(chat("chat9"))),
        ] {
            self.store
                .outbox_enqueue(NewOutbox {
                    instance: "web".into(),
                    target,
                    kind: "notice".into(),
                    user: user_of,
                    payload: json!({"markdown": "hi"}),
                    created_at: now,
                    expires_at: expires,
                })
                .await
                .unwrap();
        }
        for session in [Some(first.id), None] {
            self.store
                .record_usage(UsageRecord {
                    at: now,
                    provider: "fake".into(),
                    model: "test-model".into(),
                    agent: agent("jeeves"),
                    user: Some(user.clone()),
                    session,
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 20,
                        cache_read_tokens: 3,
                        cache_write_tokens: 4,
                    },
                    fallback: false,
                })
                .await
                .unwrap();
        }
        for creator in [name, "bob"] {
            self.store
                .job_create(agent("jeeves"), id(creator), json!({"prompt": "x"}), now)
                .await
                .unwrap();
        }

        // Ways to sign in, and links.
        let later = now.checked_add(Duration::from_secs(3600)).unwrap();
        for owner in [name, "root"] {
            self.store
                .web_session_create(
                    &format!("token-{name}-{owner}"),
                    id(owner),
                    SignInMethod::Password,
                    now,
                    later,
                )
                .await
                .unwrap();
        }
        self.store
            .user_link_code_create(&user, "irc", "ABCD2345", [7; 16], now, later)
            .await
            .unwrap();
        self.store
            .user_invite_create(
                &format!("{name:a<43}"),
                &user,
                InviteKind::Recovery,
                Some(&id("root")),
                now,
                later,
            )
            .await
            .unwrap();
        self.store
            .user_invite_create(
                &format!("{name:b<43}"),
                &id("bob"),
                InviteKind::Recovery,
                Some(&user),
                now,
                later,
            )
            .await
            .unwrap();
        self.store
            .user_password_set(&user, "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA", now)
            .await
            .unwrap();
        self.store
            .user_passkey_add(
                NewPasskey {
                    user: user.clone(),
                    credential_id: format!("credential-of-{name}").into_bytes(),
                    credential: "{}".into(),
                    kind: PasskeyKind::Discoverable,
                    name: "laptop".into(),
                },
                now,
            )
            .await
            .unwrap();
        if self.store.oidc_provider("google").await.unwrap().is_none() {
            self.store
                .oidc_provider_add(
                    "google",
                    OidcProviderSettings {
                        display_name: "Google".into(),
                        issuer: "https://accounts.example".into(),
                        client_id: "client".into(),
                        client_secret: None,
                        scopes: vec!["openid".into()],
                        enabled: true,
                    },
                    now,
                )
                .await
                .unwrap();
        }
        self.store
            .user_oidc_connect("google", "https://accounts.example", name, &user, now)
            .await
            .unwrap();

        // SSH keys and secrets: the key's, one in the user's space, and one elsewhere that the
        // user created.
        self.ssh_keys
            .generate(&user, KeyType::Ed25519, Some("laptop"))
            .await
            .unwrap();
        self.secrets
            .put(
                &self.secret(&format!("users/{name}/github")),
                &Secret::new("token"),
                Some(&user),
            )
            .await
            .unwrap();
        self.secrets
            .put(
                &self.secret("oidc/google/client-secret"),
                &Secret::new("shh"),
                Some(&user),
            )
            .await
            .unwrap();
        self.store
            .known_host_pin(
                NewKnownHost {
                    environment: "box".into(),
                    host: "git.example".into(),
                    port: 22,
                    key_type: "ssh-ed25519".into(),
                    public_key: "AAAA".into(),
                    fingerprint: "SHA256:abc".into(),
                    first_workspace: None,
                    first_agent: None,
                    first_user: Some(name.to_owned()),
                    added_by: Some(user.clone()),
                },
                now,
            )
            .await
            .unwrap();

        // Memories in two agents' databases: private ones, and a shared one they wrote.
        let member = MemoryViewer::new(MemoryOwner::User(user.clone()), false);
        let admin = MemoryViewer::new(MemoryOwner::User(user.clone()), true);
        self.memory(
            "jeeves",
            &member,
            &format!("{name} likes {}", marker(name)),
            false,
        )
        .await;
        self.memory(
            "jeeves",
            &member,
            &format!("{name} fears bees {}", marker(name)),
            false,
        )
        .await;
        self.memory("jeeves", &admin, "the office closes at six", true)
            .await;
        self.memory(
            "bunter",
            &member,
            &format!("{name} takes two sugars {}", marker(name)),
            false,
        )
        .await;

        // Their profile file, and another in their directory.
        let dir = self.root.user_dir(name).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("USER.md"), format!("# {name}\nlikes cats")).unwrap();
        std::fs::write(dir.join("notes.txt"), "more").unwrap();

        // A workspace for their first chat, and an approval waiting for them in it.
        let workspace = self
            .workspaces
            .create(
                &WorkspaceOwner::Conversation {
                    agent: agent("jeeves"),
                    channel: "web".into(),
                    conversation: chat(&chat1),
                    user: Some(user.clone()),
                },
                "box",
                json!({}),
                Some(user.clone()),
            )
            .await
            .unwrap();
        let lookup: Arc<dyn ChannelLookup> = self.registry.clone();
        let approvals = Arc::new(TurnApprovals::new(
            self.approvals.clone(),
            first.id,
            vec!["ask__*".to_owned()],
            Duration::from_secs(600),
            Some(ApprovalTarget {
                key: first.key().unwrap(),
                user: user.clone(),
                reply: Reply::for_session(&first, lookup).unwrap(),
                outbox: None,
            }),
        ));
        let asked = {
            let approvals = approvals.clone();
            tokio::spawn(async move { approvals.approve_call("ask__run", &json!({})).await })
        };
        while !approvals.has_pending() {
            tokio::task::yield_now().await;
        }
        Populated {
            image,
            workspace: workspace.id,
            asked,
        }
    }
}

/// What [`Harness::populate`] made that the tests look at again.
struct Populated {
    /// The attachment only the user's chat refers to.
    image: MediaRef,
    workspace: doozers_types::WorkspaceId,
    /// The approval waiting for the user: it resolves when they are forgotten.
    asked: tokio::task::JoinHandle<Result<bool, String>>,
}

/// Every byte of a compacted copy of the database, which is what `doozers backup` keeps.
async fn database_bytes(store: &Store, dir: &Path) -> Vec<u8> {
    let copy = dir.join("scan.db");
    let _ = std::fs::remove_file(&copy);
    store.vacuum_into(copy.clone()).await.unwrap();
    std::fs::read(copy).unwrap()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[tokio::test]
async fn the_preview_counts_what_would_be_deleted() {
    let h = Harness::new().await;
    h.populate(ZED).await;
    h.populate("bob").await;

    let preview = h.forgetter.preview(&id("root"), &id(ZED)).await.unwrap();
    assert_eq!(preview.user.username, ZED);
    let data = &preview.data;
    assert_eq!(
        (
            data.direct_sessions,
            data.conversations,
            data.job_sessions,
            data.messages,
            data.summaries
        ),
        (2, 2, 1, 4, 1)
    );
    assert_eq!((data.group_messages, data.group_summaries), (1, 2));
    assert_eq!((data.inbox, data.outbox, data.usage), (2, 2, 2));
    assert_eq!(
        (data.runtime_jobs, data.web_sessions, data.identities),
        (1, 1, 1)
    );
    assert_eq!(
        (
            data.link_codes,
            data.passwords,
            data.passkeys,
            data.oidc_connections
        ),
        (1, 1, 1, 1)
    );
    assert_eq!((data.invites, data.invites_made, data.ssh_keys), (1, 1, 1));
    assert_eq!(
        preview.memories,
        [
            AgentMemories {
                agent: agent("bunter"),
                memories: 1
            },
            AgentMemories {
                agent: agent("jeeves"),
                memories: 2
            },
        ]
    );
    assert_eq!(preview.secrets, 2, "the SSH key's and users/{ZED}/github");
    assert!(preview.profile);
    assert_eq!(preview.workspaces.len(), 1);
    assert_eq!(preview.workspaces[0].environment, "box");
    assert_eq!(preview.approvals, 1);
    assert!(!preview.busy);
    assert_eq!(preview.refusal, None);

    // The same user, asked about by themselves, is refused; so is the last active admin.
    let own = h.forgetter.preview(&id(ZED), &id(ZED)).await.unwrap();
    assert_eq!(own.refusal, Some(Refusal::Yourself));
    assert!(own.is_blocked());
    let last = h.forgetter.preview(&id(ZED), &id("root")).await.unwrap();
    assert_eq!(last.refusal, Some(Refusal::LastAdmin));
    h.activity.busy_from.store(1, Ordering::SeqCst);
    assert!(
        h.forgetter
            .preview(&id("root"), &id(ZED))
            .await
            .unwrap()
            .busy
    );
    assert!(matches!(
        h.forgetter.preview(&id("root"), &id("nobody")).await,
        Err(ForgetError::NoSuchUser(_))
    ));
}

#[tokio::test]
async fn forgetting_a_user_leaves_nothing_of_them_and_nothing_of_anyone_elses_changed() {
    let h = Harness::new().await;
    let zed = h.populate(ZED).await;
    let bob = h.populate("bob").await;
    let tokens = h.store.usage_by_agent(None).await.unwrap();

    let forgotten = h.forgetter.forget(&id("root"), &id(ZED)).await.unwrap();
    assert_eq!(forgotten.user.id, id(ZED));
    assert_eq!(forgotten.conversations.len(), 2);
    assert_eq!(
        (
            forgotten.memories,
            forgotten.shared_memories,
            forgotten.secrets
        ),
        (3, 1, 2)
    );
    assert_eq!((forgotten.workspaces, forgotten.approvals), (1, 1));
    assert_eq!(forgotten.group_sessions, 1);
    assert!(forgotten.profile);
    assert_eq!(forgotten.data.direct_sessions, 2);

    // The approval was denied, the workspace is gone from its environment, the user is gone.
    assert_eq!(zed.asked.await.unwrap(), Err("denied".to_owned()));
    assert!(!h.fake.has(&zed.workspace));
    assert!(h.user(ZED).is_none());
    assert!(h.store.user(&id(ZED)).await.unwrap().is_none());

    // Nothing in the database, the memory databases, the secret store, or the files names them.
    let bytes = database_bytes(&h.store, h.dir.path()).await;
    assert!(
        !contains(&bytes, ZED),
        "the database still has the user's id"
    );
    assert!(
        !contains(&bytes, &marker(ZED)),
        "the database, or its full-text index, still has the user's words"
    );
    assert!(
        contains(&bytes, &marker("bob")),
        "the other user's words stay"
    );
    for agent_name in ["jeeves", "bunter"] {
        let copy = h.dir.path().join(format!("{agent_name}-scan.db"));
        h.memory_dbs
            .get(&agent(agent_name))
            .await
            .unwrap()
            .vacuum_into(copy.clone())
            .await
            .unwrap();
        let bytes = std::fs::read(&copy).unwrap();
        assert!(!contains(&bytes, ZED), "{agent_name}");
        assert!(
            !contains(&bytes, &marker(ZED)),
            "{agent_name}'s full-text index"
        );
        assert!(contains(&bytes, &marker("bob")), "{agent_name}");
    }
    assert!(!h.root.user_dir(ZED).unwrap().exists());
    let secrets = h.secrets.list(None).await.unwrap();
    assert!(secrets.iter().all(|s| s.path.owner() != Some(ZED)));
    assert!(
        secrets
            .iter()
            .all(|s| s.created_by.as_ref() != Some(&id(ZED)))
    );
    assert!(
        secrets
            .iter()
            .all(|s| s.updated_by.as_ref() != Some(&id(ZED)))
    );
    assert!(
        secrets
            .iter()
            .any(|s| s.path.as_str() == "oidc/google/client-secret"),
        "a secret outside their space stays, without its creator"
    );
    assert!(h.ssh_keys.list(&id(ZED)).await.unwrap().is_empty());
    assert!(h.store.workspaces_all().await.unwrap().iter().all(|w| {
        w.key_user.as_ref() != Some(&id(ZED))
            && !matches!(&w.owner, WorkspaceOwner::Conversation { user: Some(u), .. } if u == &id(ZED))
    }));
    assert_eq!(
        h.store.user_data(&id(ZED)).await.unwrap(),
        UserData::default()
    );

    // Their usage stays, as numbers.
    assert_eq!(h.store.usage_by_agent(None).await.unwrap(), tokens);

    // The attachment only they referenced is swept; the other user's is not.
    let long_after = h
        .now()
        .checked_add(SWEEP_GRACE + Duration::from_secs(3600))
        .unwrap();
    let report = h.blobs.sweep(long_after, SWEEP_GRACE).await.unwrap();
    assert_eq!(report.blobs, 1);
    assert!(h.blobs.stat(&zed.image.id).await.unwrap().is_none());
    assert!(h.blobs.stat(&bob.image.id).await.unwrap().is_some());

    // Everything of bob's is as it was, and bob can still be forgotten the same way.
    let preview = h.forgetter.preview(&id("root"), &id("bob")).await.unwrap();
    assert_eq!(preview.data.direct_sessions, 2);
    assert_eq!(preview.memories.len(), 2);
    assert_eq!(
        (preview.secrets, preview.workspaces.len(), preview.approvals),
        (2, 1, 1)
    );
    assert!(preview.profile);
    let group = h
        .store
        .find_open_session(doozers_store::SessionKey {
            agent: agent("jeeves"),
            channel: "irc".into(),
            conversation: chat("room"),
        })
        .await
        .unwrap()
        .unwrap();
    let messages = h.store.recent_messages(group.id, 10).await.unwrap();
    assert_eq!(messages.len(), 3);
}

#[tokio::test]
async fn a_group_message_of_the_user_is_a_tombstone_and_its_summary_is_gone() {
    let h = Harness::new().await;
    h.populate(ZED).await;
    h.forgetter.forget(&id("root"), &id(ZED)).await.unwrap();
    let group = h
        .store
        .find_open_session(doozers_store::SessionKey {
            agent: agent("jeeves"),
            channel: "irc".into(),
            conversation: chat("room"),
        })
        .await
        .unwrap()
        .unwrap();
    let texts: Vec<String> = h
        .store
        .recent_messages(group.id, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|stored| match stored.entry {
            doozers_store::Entry::Message(message) => match &message.content[0] {
                Block::Text { text } => text.clone(),
                other => format!("{other:?}"),
            },
            doozers_store::Entry::EmptySlot { .. } => String::new(),
        })
        .collect();
    assert_eq!(texts, [doozers_store::TOMBSTONE, "ok", "bob says hello"]);
    // Summaries are cumulative: both covered their message, so the history is every message
    // again, and the group's summaries are asked for anew.
    let history = h.store.load_history(group.id).await.unwrap();
    assert_eq!(history.summary, None);
    assert_eq!(history.messages.len(), 3);
    h.forgetter.settled().await;
    assert_eq!(*h.summaries.asked.lock().unwrap(), [group.id]);
}

#[tokio::test]
async fn refusing_yourself_the_last_admin_and_a_busy_user_changes_nothing() {
    let h = Harness::new().await;
    h.populate(ZED).await;

    // Yourself.
    assert!(matches!(
        h.forgetter.forget(&id(ZED), &id(ZED)).await,
        Err(ForgetError::Yourself)
    ));
    // The last active admin: the guard is the disable, which changes nothing when it refuses.
    assert!(matches!(
        h.forgetter.forget(&id(ZED), &id("root")).await,
        Err(ForgetError::LastAdmin)
    ));
    assert_eq!(h.user("root").unwrap().state, UserState::Active);
    // Nobody.
    assert!(matches!(
        h.forgetter.forget(&id("root"), &id("nobody")).await,
        Err(ForgetError::NoSuchUser(_))
    ));
    // Busy before anything, and busy once the user was disabled (their turn began meanwhile):
    // the user is active again.
    for busy_from in [1, 2] {
        h.activity.calls.store(0, Ordering::SeqCst);
        h.activity.busy_from.store(busy_from, Ordering::SeqCst);
        let refused = h.forgetter.forget(&id("root"), &id(ZED)).await.unwrap_err();
        assert!(matches!(refused, ForgetError::Busy), "{refused}");
        assert!(refused.is_refusal());
        assert_eq!(h.user(ZED).unwrap().state, UserState::Active);
    }
    // A removal refused only by what is stored: a message of theirs is waiting for a turn.
    h.activity.busy_from.store(0, Ordering::SeqCst);
    let (session, _) = h
        .store
        .open_session(
            doozers_store::SessionKey {
                agent: agent("jeeves"),
                channel: "web".into(),
                conversation: chat("chat1"),
            },
            SessionKind::Direct,
            Some(id(ZED)),
            h.now(),
        )
        .await
        .unwrap();
    h.store
        .inbox_insert(NewInboxEvent {
            instance: "web".into(),
            event_id: EventId::new("queued").unwrap(),
            session: Some(session.id),
            user: Some(id(ZED)),
            payload: json!({}),
            state: InboxState::Queued,
            received_at: h.now(),
        })
        .await
        .unwrap();
    assert!(matches!(
        h.forgetter.forget(&id("root"), &id(ZED)).await,
        Err(ForgetError::Busy)
    ));
    assert_eq!(h.user(ZED).unwrap().state, UserState::Active);
    assert!(h.store.user_data(&id(ZED)).await.unwrap().direct_sessions >= 2);
}

#[tokio::test]
async fn a_removal_that_fails_midway_leaves_the_user_disabled_and_a_retry_finishes_it() {
    let h = Harness::new().await;
    let zed = h.populate(ZED).await;

    h.secrets.fail_deletes.store(true, Ordering::SeqCst);
    let failed = h.forgetter.forget(&id("root"), &id(ZED)).await.unwrap_err();
    assert!(matches!(failed, ForgetError::Secrets(_)), "{failed}");
    assert!(!failed.is_refusal());
    // The summaries were started when the rows went, before the failure, so a retry that finds
    // none left to delete doesn't lose them.
    h.forgetter.settled().await;
    assert_eq!(h.summaries.asked.lock().unwrap().len(), 1);
    // The user can't be used, but the removal went as far as the secrets: the rows and the
    // workspace are gone, the secrets, memories, files, and the user are not.
    assert_eq!(h.user(ZED).unwrap().state, UserState::Disabled);
    let data = h.store.user_data(&id(ZED)).await.unwrap();
    assert_eq!(
        (
            data.direct_sessions,
            data.web_sessions,
            data.passwords,
            data.ssh_keys
        ),
        (0, 0, 0, 0)
    );
    assert!(!h.fake.has(&zed.workspace));
    assert!(h.root.user_dir(ZED).unwrap().exists());
    assert_eq!(
        h.memory_dbs
            .get(&agent("jeeves"))
            .await
            .unwrap()
            .user_memories(&id(ZED))
            .await
            .unwrap(),
        2
    );

    h.secrets.fail_deletes.store(false, Ordering::SeqCst);
    let forgotten = h.forgetter.forget(&id("root"), &id(ZED)).await.unwrap();
    assert_eq!(forgotten.secrets, 2);
    assert_eq!(forgotten.memories, 3);
    assert!(forgotten.profile);
    assert!(h.user(ZED).is_none());
    assert!(!h.root.user_dir(ZED).unwrap().exists());
    let bytes = database_bytes(&h.store, h.dir.path()).await;
    assert!(!contains(&bytes, ZED));
    // And a third time there is no one to forget.
    assert!(matches!(
        h.forgetter.forget(&id("root"), &id(ZED)).await,
        Err(ForgetError::NoSuchUser(_))
    ));
}

#[tokio::test]
async fn a_disabled_user_is_forgotten_and_a_workspace_the_plugin_keeps_is_left_to_the_sweep() {
    let h = Harness::new().await;
    let zed = h.populate(ZED).await;
    h.users
        .set_state(&id(ZED), UserState::Disabled, h.now())
        .await
        .unwrap();
    h.fake.fail_deletes(true);

    h.forgetter.forget(&id("root"), &id(ZED)).await.unwrap();
    assert!(h.fake.has(&zed.workspace), "the plugin couldn't be told");
    let record = h.store.workspace(&zed.workspace).await.unwrap().unwrap();
    assert_eq!(record.state, WorkspaceState::Deleting);
    assert_eq!(record.key_user, None);
    assert!(
        matches!(
            record.owner,
            WorkspaceOwner::Conversation { user: None, .. }
        ),
        "{:?}",
        record.owner
    );
    let bytes = database_bytes(&h.store, h.dir.path()).await;
    assert!(!contains(&bytes, ZED));
}
