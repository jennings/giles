//! The workspace service against an in-process environment: owners and slots, leases, the sweep,
//! and reconciliation.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use doozers_store::{SessionKind, Store};
use doozers_types::{
    AgentId, Block, ConversationId, EnvironmentInfo, Error, Message, MessageMeta, RelayReply,
    RelayRequest, Role, Timestamp, UserId, WorkspaceDetail, WorkspaceId,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::fake::{Fake, Scan, TestClock};
use super::*;
use crate::blobs::SweepSchedule;
use crate::config::{Config, InstanceName, LoadOptions};
use crate::paths::DataRoot;

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// `box` stops after 10 minutes and deletes after 7 days; `far` is never connected.
const CONFIG: &str = "[owner]\nidentities = [\"web:owner\"]\n\
    [environment.box]\ncomponent = \"docker\"\naddress = \"http://127.0.0.1:9000\"\n\
    stop_after = \"10m\"\ndelete_after = \"7d\"\n\
    [environment.far]\ncomponent = \"docker\"\naddress = \"http://127.0.0.1:9001\"\n";

struct Harness {
    _dir: TempDir,
    store: Store,
    registry: Arc<Registry>,
    clock: Arc<TestClock>,
    fake: Arc<Fake>,
    ssh_keys: SshKeys,
    workspaces: Workspaces,
}

fn name(name: &str) -> InstanceName {
    InstanceName::new(name).unwrap()
}

impl Harness {
    /// `box` connected to a fake environment with no workspaces.
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
        let registry = Arc::new(Registry::new(root, Arc::new(config)));
        let store = Store::open(dir.path().join("doozers.db")).await.unwrap();
        let clock = TestClock::new();
        let fake = Fake::new(clock.clone());
        let ssh_keys = SshKeys::for_tests(store.clone(), dir.path(), clock.clone()).await;
        let workspaces = Workspaces::new(
            store.clone(),
            registry.clone(),
            clock.clone(),
            ssh_keys.clone(),
        );
        let harness = Self {
            _dir: dir,
            store,
            registry,
            clock,
            fake,
            ssh_keys,
            workspaces,
        };
        harness.connect();
        harness
    }

    fn connect(&self) {
        self.registry
            .register_environment(
                &name("box"),
                self.fake.clone(),
                EnvironmentInfo {
                    options_schema: json!({"type": "object"}),
                },
                crate::registry::EnvironmentCapabilities::default(),
            )
            .unwrap();
    }

    /// Connects `box` again, now as an environment that can relay ssh-agent requests.
    fn connect_relaying(&self) {
        self.registry
            .register_environment(
                &name("box"),
                self.fake.clone(),
                EnvironmentInfo {
                    options_schema: json!({"type": "object"}),
                },
                crate::registry::EnvironmentCapabilities::new(true),
            )
            .unwrap();
    }

    fn disconnect(&self) {
        self.registry.unregister_environment(&name("box")).unwrap();
    }

    /// Creates a workspace for `owner` whose key user is `key_user`.
    async fn create_for(
        &self,
        owner: &WorkspaceOwner,
        key_user: Option<&UserId>,
    ) -> WorkspaceRecord {
        self.workspaces
            .create(owner, "box", json!({"image": "alpine"}), key_user.cloned())
            .await
            .unwrap()
    }

    async fn create(&self, owner: &WorkspaceOwner) -> WorkspaceRecord {
        self.workspaces
            .create(owner, "box", json!({"image": "alpine"}), None)
            .await
            .unwrap()
    }

    async fn record(&self, id: &WorkspaceId) -> WorkspaceRecord {
        self.store.workspace(id).await.unwrap().unwrap()
    }

    /// A message in the conversation `owner` at `at`, in a session of its key.
    async fn say(&self, owner: &WorkspaceOwner, at: Timestamp) {
        let WorkspaceOwner::Conversation {
            agent,
            channel,
            conversation,
            user,
        } = owner
        else {
            panic!("not a conversation");
        };
        let key = doozers_store::SessionKey {
            agent: agent.clone(),
            channel: channel.clone(),
            conversation: conversation.clone(),
        };
        let (session, _) = self
            .store
            .open_session(key, SessionKind::Direct, user.clone(), at)
            .await
            .unwrap();
        self.store
            .append_messages(
                session.id,
                vec![Message {
                    role: Role::User,
                    content: vec![Block::text("hello")],
                    meta: MessageMeta::new(at),
                }],
            )
            .await
            .unwrap();
    }

    fn clock_now(&self) -> Timestamp {
        use crate::sessions::Clock;
        self.clock.now()
    }

    fn later(&self, by: Duration) -> Timestamp {
        self.clock.now().checked_add(by).unwrap()
    }
}

fn chat(conversation: &str) -> WorkspaceOwner {
    WorkspaceOwner::Conversation {
        agent: AgentId::new("jeeves").unwrap(),
        channel: "web".into(),
        conversation: ConversationId::new(conversation).unwrap(),
        user: None,
    }
}

/// `user`'s direct conversation `conversation`.
fn chat_of(conversation: &str, user: &str) -> WorkspaceOwner {
    let WorkspaceOwner::Conversation {
        agent,
        channel,
        conversation,
        ..
    } = chat(conversation)
    else {
        unreachable!();
    };
    WorkspaceOwner::Conversation {
        agent,
        channel,
        conversation,
        user: Some(UserId::new(user).unwrap()),
    }
}

fn job(session: i64) -> WorkspaceOwner {
    WorkspaceOwner::Job {
        session: doozers_store::SessionId::new(session),
    }
}

/// Runs `check` every 10 ms for up to 5 seconds until it holds.
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never happened: {what}");
}

#[tokio::test]
async fn a_workspace_gets_a_core_assigned_id_the_plugin_uses_and_one_owner_has_one() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;

    assert!(record.id.as_str().starts_with("ws-"));
    assert_eq!(record.state, WorkspaceState::Active);
    assert_eq!(record.environment, "box");
    assert_eq!(record.owner, owner);
    assert_eq!(record.options, json!({"image": "alpine"}));
    assert_eq!(record.description.as_deref(), Some("fake box"));
    assert_eq!(record.working_directory.as_deref(), Some("/work"));
    assert_eq!(record.persistence.as_deref(), Some("files persist"));
    // The plugin has the workspace under the core's id, with the options as given.
    assert_eq!(
        h.fake.held()[&record.id].options,
        json!({"image": "alpine"})
    );
    assert_eq!(
        h.workspaces.current(&owner).await.unwrap(),
        Some(record.clone())
    );

    let err = h
        .workspaces
        .create(&owner, "box", json!({}), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::AlreadyHasWorkspace { current } if current.id == record.id),
        "{err}"
    );
    let message = err.to_string();
    assert!(
        message.contains(record.id.as_str()) && message.contains("box"),
        "{message}"
    );
    assert_eq!(h.fake.held().len(), 1, "the refused create made nothing");

    // Other conversations, and a job run, have their own.
    h.create(&chat("c2")).await;
    h.create(&job(5)).await;
    assert_eq!(h.fake.held().len(), 3);
}

#[tokio::test]
async fn create_refuses_what_cant_work_and_leaves_the_owner_free_to_retry() {
    let h = Harness::new().await;
    let owner = chat("c1");

    let err = h
        .workspaces
        .create(&owner, "nope", json!({}), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkspaceError::UnknownEnvironment { .. }),
        "{err}"
    );
    let err = h
        .workspaces
        .create(&owner, "far", json!({}), None)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Unreachable { .. }), "{err}");
    let err = h
        .workspaces
        .create(&owner, "box", json!(["image"]), None)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Refused(_)), "{err}");
    assert!(h.workspaces.current(&owner).await.unwrap().is_none());

    // The plugin refuses the options: its message reaches the model, the half-made workspace is
    // deleted, and the owner can try again at once.
    h.fake
        .fail_next_create(Error::InvalidRequest("unknown image \"nope\"".into()), true);
    let err = h
        .workspaces
        .create(&owner, "box", json!({"image": "nope"}), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::Refused(m) if m == "unknown image \"nope\""),
        "{err}"
    );
    assert_eq!(err.to_string(), "unknown image \"nope\"");
    assert!(h.fake.held().is_empty());
    assert!(h.workspaces.current(&owner).await.unwrap().is_none());
    assert!(
        h.workspaces.last_deleted(&owner).await.unwrap().is_none(),
        "the model saw the error; there is no deleted workspace to report"
    );
    h.create(&owner).await;

    // A backend failure is reported as such.
    h.fake
        .fail_next_create(Error::unavailable("the engine is down"), false);
    let err = h
        .workspaces
        .create(&chat("c2"), "box", json!({}), None)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Environment(_)), "{err}");
    h.create(&chat("c2")).await;
}

#[tokio::test]
async fn deleting_while_the_plugin_is_unreachable_frees_the_slot_and_cleans_up_on_reconnect() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;

    h.disconnect();
    let deleted = h
        .workspaces
        .delete(&owner, DeleteReason::ModelDelete)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted.id, record.id);
    assert_eq!(deleted.state, WorkspaceState::Deleting);
    assert_eq!(deleted.delete_reason, Some(DeleteReason::ModelDelete));
    assert!(h.fake.has(&record.id), "the plugin couldn't be told");
    assert!(h.workspaces.current(&owner).await.unwrap().is_none());
    let last = h.workspaces.last_deleted(&owner).await.unwrap().unwrap();
    assert_eq!(last.id, record.id);
    assert_eq!(last.options, json!({"image": "alpine"}));
    // The slot is free, but nothing can be created while the plugin is away.
    let err = h
        .workspaces
        .create(&owner, "box", json!({}), None)
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Unreachable { .. }), "{err}");

    // A sweep with the plugin away changes nothing.
    assert_eq!(h.workspaces.sweep_once().await, SweepReport::default());
    assert_eq!(h.record(&record.id).await.state, WorkspaceState::Deleting);

    h.connect();
    let report = h.workspaces.reconcile("box").await.unwrap();
    assert_eq!(
        report,
        SweepReport {
            cleaned: 1,
            ..SweepReport::default()
        }
    );
    assert!(!h.fake.has(&record.id));
    let done = h.record(&record.id).await;
    assert_eq!(done.state, WorkspaceState::Deleted);
    assert_eq!(done.delete_reason, Some(DeleteReason::ModelDelete));
    h.create(&owner).await;
}

#[tokio::test]
async fn delete_with_the_plugin_up_removes_the_workspace_and_says_so() {
    let h = Harness::new().await;
    let owner = chat("c1");
    assert!(
        h.workspaces
            .delete(&owner, DeleteReason::UserReset)
            .await
            .unwrap()
            .is_none()
    );
    let record = h.create(&owner).await;
    let deleted = h
        .workspaces
        .delete(&owner, DeleteReason::UserReset)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted.state, WorkspaceState::Deleted);
    assert!(!h.fake.has(&record.id));
    let last = h.workspaces.last_deleted(&owner).await.unwrap().unwrap();
    assert_eq!(last.delete_reason, Some(DeleteReason::UserReset));
    assert_eq!(last.deleted_at, Some(h.clock_now()));
    // A failing plugin leaves it pending.
    let record = h.create(&owner).await;
    h.fake.fail_deletes(true);
    let deleted = h
        .workspaces
        .delete(&owner, DeleteReason::ModelReset)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted.state, WorkspaceState::Deleting);
    assert!(h.fake.has(&record.id));
}

#[tokio::test]
async fn recreating_deletes_the_workspace_and_makes_another_like_it() {
    let h = Harness::new().await;
    let owner = chat("c1");
    assert!(
        h.workspaces
            .recreate(&owner, DeleteReason::UserReset)
            .await
            .unwrap()
            .is_none(),
        "no workspace, nothing to reset"
    );

    let old = h.create(&owner).await;
    let done = h
        .workspaces
        .recreate(&owner, DeleteReason::UserReset)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.discarded.id, old.id);
    assert_eq!(done.discarded.state, WorkspaceState::Deleted);
    assert_eq!(done.discarded.delete_reason, Some(DeleteReason::UserReset));
    let new = done.created.unwrap();
    assert_ne!(new.id, old.id);
    assert_eq!(new.environment, "box");
    assert_eq!(new.options, json!({"image": "alpine"}));
    assert_eq!(new.state, WorkspaceState::Active);
    assert!(!h.fake.has(&old.id));
    assert!(h.fake.has(&new.id));
    assert_eq!(
        h.workspaces.current(&owner).await.unwrap().unwrap().id,
        new.id
    );
    let last = h.workspaces.last_deleted(&owner).await.unwrap().unwrap();
    assert_eq!(last.id, old.id);
    assert_eq!(last.delete_reason, Some(DeleteReason::UserReset));

    // Another owner's workspace isn't touched.
    let other = h.create(&chat("c2")).await;
    h.workspaces
        .recreate(&owner, DeleteReason::UserReset)
        .await
        .unwrap()
        .unwrap();
    assert!(h.fake.has(&other.id));
}

#[tokio::test]
async fn recreating_checks_the_environment_before_deleting_and_reports_a_failed_creation() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let old = h.create(&owner).await;

    // The plugin is away: refused, and nothing is lost.
    h.disconnect();
    let error = h
        .workspaces
        .recreate(&owner, DeleteReason::UserReset)
        .await
        .unwrap_err();
    assert!(
        matches!(error, WorkspaceError::Unreachable { .. }),
        "{error}"
    );
    assert_eq!(
        h.workspaces.current(&owner).await.unwrap().unwrap().id,
        old.id
    );
    assert!(h.fake.has(&old.id));
    h.connect();

    // Creating fails: the old one is gone, and the failure is reported.
    h.fake.fail_next_create(
        doozers_types::Error::InvalidRequest("no space left".to_owned()),
        false,
    );
    let done = h
        .workspaces
        .recreate(&owner, DeleteReason::UserReset)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.discarded.id, old.id);
    let error = done.created.unwrap_err();
    assert_eq!(error.to_string(), "no space left");
    assert!(h.workspaces.current(&owner).await.unwrap().is_none());
    assert!(!h.fake.has(&old.id));
}

#[tokio::test]
async fn the_sessions_hook_resets_the_conversations_workspace() {
    use crate::sessions::{EnvReset, EnvResets};

    let h = Harness::new().await;
    let owner = chat_of("c1", "alice");
    let WorkspaceOwner::Conversation {
        agent,
        channel,
        conversation,
        ..
    } = owner.clone()
    else {
        unreachable!();
    };
    let key = doozers_store::SessionKey {
        agent,
        channel,
        conversation,
    };
    let alice = user("alice");
    assert_eq!(
        h.workspaces.reset(&key, &alice).await,
        EnvReset::NoWorkspace
    );

    let old = h.create_for(&owner, Some(&alice)).await;
    // Another user's workspace in the same conversation id is none of this reset's business.
    let bobs = h
        .create_for(&chat_of("c1", "bob"), Some(&user("bob")))
        .await;
    assert_eq!(h.workspaces.reset(&key, &alice).await, EnvReset::Done);
    assert_eq!(
        h.workspaces
            .current(&chat_of("c1", "bob"))
            .await
            .unwrap()
            .unwrap()
            .id,
        bobs.id
    );
    let new = h.workspaces.current(&owner).await.unwrap().unwrap();
    assert_ne!(new.id, old.id);
    assert_eq!(new.options, old.options);
    assert_eq!(new.key_user, Some(alice.clone()));

    h.disconnect();
    let EnvReset::Failed(message) = h.workspaces.reset(&key, &alice).await else {
        panic!("not refused");
    };
    assert!(
        message.starts_with("The workspace was not reset: "),
        "{message}"
    );
    assert!(message.contains("not reachable"), "{message}");
    h.connect();

    h.fake.fail_next_create(
        doozers_types::Error::InvalidRequest("no space left".to_owned()),
        false,
    );
    let EnvReset::Failed(message) = h.workspaces.reset(&key, &alice).await else {
        panic!("not reported");
    };
    assert!(
        message.contains("deleted, but creating a new one failed: no space left"),
        "{message}"
    );
}

#[tokio::test]
async fn opening_starts_the_workspace_takes_the_lease_and_notes_the_use() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let slot = LeaseSlot::default();
    let err = h.workspaces.open(&owner, &slot).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::NoWorkspace), "{err}");
    assert!(slot.held().is_none());

    let record = h.create(&owner).await;
    h.clock.advance(MINUTE);
    let opened = h.workspaces.open(&owner, &slot).await.unwrap();
    assert_eq!(opened.record.id, record.id);
    assert_eq!(opened.notice, None, "it was running");
    assert_eq!(slot.held(), Some(record.id.clone()));
    assert_eq!(opened.record.last_used_at, h.clock_now());
    assert_eq!(h.record(&record.id).await.last_used_at, h.clock_now());

    // Opened again in the same turn: still one lease, and no deadlock.
    h.workspaces.open(&owner, &slot).await.unwrap();

    // A stopped workspace is started, and the model is told.
    h.fake.insert("unrelated", HOUR, true);
    h.workspaces.sweep_once().await;
    h.clock.advance(11 * MINUTE);
    drop(slot);
    let report = h.workspaces.sweep_once().await;
    assert_eq!(report.stopped, 1);
    let slot = LeaseSlot::default();
    let opened = h.workspaces.open(&owner, &slot).await.unwrap();
    let notice = opened.notice.unwrap();
    assert!(notice.contains("started again"), "{notice}");
    assert_eq!(h.fake.running(&record.id), Some(true));

    // Recreated: the plugin's detail is passed on.
    h.fake.insert("ignored", Duration::ZERO, true);
    h.clock.advance(11 * MINUTE);
    drop(slot);
    h.workspaces.sweep_once().await;
    h.fake.recreate_next_start();
    let opened = h
        .workspaces
        .open(&owner, &LeaseSlot::default())
        .await
        .unwrap();
    let notice = opened.notice.unwrap();
    assert!(notice.contains("recreated"), "{notice}");
    assert!(notice.contains("Installed packages are gone."), "{notice}");
}

#[tokio::test]
async fn opening_reports_a_workspace_the_environment_lost_and_a_plugin_that_is_away() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;

    h.disconnect();
    let slot = LeaseSlot::default();
    let err = h.workspaces.open(&owner, &slot).await.unwrap_err();
    assert!(matches!(err, WorkspaceError::Unreachable { .. }), "{err}");
    h.connect();

    h.fake.remove(&record.id);
    let err = h.workspaces.open(&owner, &slot).await.unwrap_err();
    assert!(
        matches!(&err, WorkspaceError::Gone { id, .. } if *id == record.id),
        "{err}"
    );
    assert!(err.to_string().contains("create a new one"), "{err}");
    assert!(h.workspaces.current(&owner).await.unwrap().is_none());
    let last = h.workspaces.last_deleted(&owner).await.unwrap().unwrap();
    assert_eq!(
        last.delete_reason,
        Some(DeleteReason::RemovedOutsideDoozers)
    );
    // And the owner can start over.
    let again = h.create(&owner).await;
    assert_ne!(again.id, record.id);
}

#[tokio::test]
async fn a_new_workspace_in_the_same_turn_replaces_the_lease_on_the_old_one() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let slot = LeaseSlot::default();
    let first = h.create(&owner).await;
    h.workspaces.open(&owner, &slot).await.unwrap();
    assert!(h.workspaces.try_claim(&first.id).is_none());

    h.workspaces
        .delete(&owner, DeleteReason::ModelReset)
        .await
        .unwrap();
    let second = h.create(&owner).await;
    h.workspaces.open(&owner, &slot).await.unwrap();
    assert_eq!(slot.held(), Some(second.id.clone()));
    assert!(
        h.workspaces.try_claim(&first.id).is_some(),
        "the old lease is gone"
    );
    assert!(h.workspaces.try_claim(&second.id).is_none());
}

#[tokio::test]
async fn opening_waits_for_a_sweep_that_holds_the_workspace() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;
    let claim = h.workspaces.try_claim(&record.id).unwrap();

    let service = h.workspaces.clone();
    let slot = LeaseSlot::default();
    let waiting = {
        let slot = slot.clone();
        let owner = owner.clone();
        tokio::spawn(async move { service.open(&owner, &slot).await.map(|o| o.record.id) })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "the open must wait for the claim");
    drop(claim);
    assert_eq!(waiting.await.unwrap().unwrap(), record.id);
}

#[tokio::test]
async fn a_lease_keeps_the_sweep_from_stopping_or_deleting_until_the_turn_ends() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;
    let slot = LeaseSlot::default();
    h.workspaces.open(&owner, &slot).await.unwrap();

    // Far past both `stop_after` and `delete_after`.
    h.clock.advance(30 * DAY);
    assert_eq!(h.workspaces.sweep_once().await, SweepReport::default());
    assert_eq!(h.fake.running(&record.id), Some(true));
    assert_eq!(h.record(&record.id).await.state, WorkspaceState::Active);

    // The turn ends: the sweep deletes it.
    drop(slot);
    let report = h.workspaces.sweep_once().await;
    assert_eq!(report.deleted_inactive, 1);
    assert!(!h.fake.has(&record.id));
}

#[tokio::test]
async fn a_lease_also_holds_back_the_cleanup_of_a_workspace_deleted_in_its_turn() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;
    let slot = LeaseSlot::default();
    h.workspaces.open(&owner, &slot).await.unwrap();
    h.fake.fail_deletes(true);
    h.workspaces
        .delete(&owner, DeleteReason::ModelDelete)
        .await
        .unwrap();
    h.fake.fail_deletes(false);

    assert_eq!(
        h.workspaces.reconcile("box").await.unwrap(),
        SweepReport::default()
    );
    assert!(h.fake.has(&record.id));
    drop(slot);
    let report = h.workspaces.reconcile("box").await.unwrap();
    assert_eq!(report.cleaned, 1);
    assert!(!h.fake.has(&record.id));
}

#[tokio::test]
async fn an_idle_workspace_is_stopped_once_and_only_when_the_plugin_has_it_running() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;

    h.clock.advance(9 * MINUTE);
    assert_eq!(h.workspaces.sweep_once().await, SweepReport::default());
    h.clock.advance(2 * MINUTE);
    let report = h.workspaces.sweep_once().await;
    assert_eq!(
        report,
        SweepReport {
            stopped: 1,
            ..SweepReport::default()
        }
    );
    assert_eq!(h.fake.running(&record.id), Some(false));
    // Stopped already: not stopped again.
    assert_eq!(h.workspaces.sweep_once().await, SweepReport::default());
    assert_eq!(
        h.fake
            .log()
            .iter()
            .filter(|l| l.starts_with("stop"))
            .count(),
        1
    );
    assert_eq!(h.record(&record.id).await.state, WorkspaceState::Active);

    // Using it again restarts the clock.
    let slot = LeaseSlot::default();
    h.workspaces.open(&owner, &slot).await.unwrap();
    drop(slot);
    h.clock.advance(9 * MINUTE);
    assert_eq!(h.workspaces.sweep_once().await, SweepReport::default());
}

#[tokio::test]
async fn a_workspace_is_deleted_when_its_conversation_has_been_inactive_for_delete_after() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;

    // The conversation was active six days in: eight days after creating, that's two idle days.
    h.say(&owner, h.later(6 * DAY)).await;
    h.clock.advance(8 * DAY);
    let report = h.workspaces.sweep_once().await;
    assert_eq!(report.deleted_inactive, 0, "the conversation is active");
    assert_eq!(report.stopped, 1, "but the workspace sat unused");
    assert_eq!(h.record(&record.id).await.state, WorkspaceState::Active);

    h.clock.advance(6 * DAY);
    let report = h.workspaces.sweep_once().await;
    assert_eq!(report.deleted_inactive, 1);
    assert!(!h.fake.has(&record.id));
    let last = h.workspaces.last_deleted(&owner).await.unwrap().unwrap();
    assert_eq!(last.id, record.id);
    assert_eq!(last.state, WorkspaceState::Deleted);
    assert_eq!(last.delete_reason, Some(DeleteReason::Inactive));
    assert_eq!(last.options, json!({"image": "alpine"}));
    assert!(h.workspaces.current(&owner).await.unwrap().is_none());
}

#[tokio::test]
async fn a_workspaces_own_use_keeps_it_alive_and_a_jobs_workspace_has_only_that() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let chat_workspace = h.create(&owner).await;
    let job_workspace = h.create(&job(3)).await;

    h.clock.advance(6 * DAY);
    let slot = LeaseSlot::default();
    h.workspaces.open(&owner, &slot).await.unwrap();
    drop(slot);
    h.clock.advance(2 * DAY);
    // No messages anywhere, but the chat's workspace was used two days ago.
    let report = h.workspaces.sweep_once().await;
    assert_eq!(report.deleted_inactive, 1);
    assert!(h.fake.has(&chat_workspace.id));
    assert!(!h.fake.has(&job_workspace.id));
    assert_eq!(
        h.workspaces
            .last_deleted(&job(3))
            .await
            .unwrap()
            .unwrap()
            .delete_reason,
        Some(DeleteReason::Inactive)
    );
}

#[tokio::test]
async fn deleted_rows_are_kept_for_delete_after_then_pruned_even_with_the_plugin_away() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;
    h.workspaces
        .delete(&owner, DeleteReason::ModelDelete)
        .await
        .unwrap();

    h.clock.advance(6 * DAY);
    assert_eq!(h.workspaces.sweep_once().await.pruned, 0);
    assert!(h.workspaces.last_deleted(&owner).await.unwrap().is_some());

    h.disconnect();
    h.clock.advance(2 * DAY);
    let report = h.workspaces.sweep_once().await;
    assert_eq!(report.pruned, 1);
    assert!(h.store.workspace(&record.id).await.unwrap().is_none());
    assert!(h.workspaces.last_deleted(&owner).await.unwrap().is_none());
}

#[tokio::test]
async fn reconciliation_deletes_unknown_workspaces_only_after_the_grace_period() {
    let h = Harness::new().await;
    let known = h.create(&chat("c1")).await;
    h.fake.insert("stray-old", 2 * HOUR, true);
    h.fake.insert("stray-new", 5 * MINUTE, true);

    let report = h.workspaces.reconcile("box").await.unwrap();
    assert_eq!(
        report,
        SweepReport {
            unknown_deleted: 1,
            ..SweepReport::default()
        }
    );
    let mut held: Vec<_> = h.fake.held().keys().map(ToString::to_string).collect();
    held.sort();
    let mut expected = [known.id.to_string(), "stray-new".to_owned()];
    expected.sort();
    assert_eq!(held, expected);

    // A row in any state counts as known: a deleted workspace whose plugin cleanup is pending
    // isn't an unknown one.
    h.fake.insert("pending", 3 * HOUR, true);
    h.store
        .workspace_insert(doozers_store::NewWorkspace {
            id: WorkspaceId::new("pending").unwrap(),
            environment: "box".into(),
            owner: chat("c9"),
            options: json!({}),
            key_user: None,
            now: h.clock_now(),
        })
        .await
        .unwrap();
    let report = h.workspaces.reconcile("box").await.unwrap();
    assert_eq!(report.unknown_deleted, 0);
    assert!(
        h.fake
            .held()
            .contains_key(&WorkspaceId::new("pending").unwrap())
    );

    h.clock.advance(HOUR);
    let report = h.workspaces.reconcile("box").await.unwrap();
    assert_eq!(report.unknown_deleted, 1, "stray-new is old enough now");
}

#[tokio::test]
async fn reconciliation_marks_workspaces_the_environment_lost_so_the_model_is_told() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;
    let other = h.create(&chat("c2")).await;
    h.fake.remove(&record.id);

    let report = h.workspaces.reconcile("box").await.unwrap();
    assert_eq!(
        report,
        SweepReport {
            gone: 1,
            ..SweepReport::default()
        }
    );
    assert!(h.workspaces.current(&owner).await.unwrap().is_none());
    let last = h.workspaces.last_deleted(&owner).await.unwrap().unwrap();
    assert_eq!(last.id, record.id);
    assert_eq!(last.state, WorkspaceState::Deleted);
    assert_eq!(
        last.delete_reason,
        Some(DeleteReason::RemovedOutsideDoozers)
    );
    assert_eq!(h.record(&other.id).await.state, WorkspaceState::Active);
    // Nothing left to do the second time.
    assert_eq!(
        h.workspaces.reconcile("box").await.unwrap(),
        SweepReport::default()
    );
}

#[tokio::test]
async fn reconciliation_gives_up_on_a_creation_that_never_finished() {
    let h = Harness::new().await;
    let owner = chat("c1");
    // A create from an hour and a half ago that never got an answer, and one in flight now.
    let stale = WorkspaceId::new("ws-stale").unwrap();
    let fresh = WorkspaceId::new("ws-fresh").unwrap();
    for (id, owner, ago) in [
        (&stale, owner.clone(), 90 * MINUTE),
        (&fresh, chat("c2"), MINUTE),
    ] {
        h.store
            .workspace_insert(doozers_store::NewWorkspace {
                id: id.clone(),
                environment: "box".into(),
                owner,
                options: json!({}),
                key_user: None,
                now: h.clock_now().checked_sub(ago).unwrap(),
            })
            .await
            .unwrap();
    }
    h.fake.insert(stale.as_str(), 90 * MINUTE, true);

    let report = h.workspaces.reconcile("box").await.unwrap();
    assert_eq!(
        report,
        SweepReport {
            stale_creating: 1,
            cleaned: 1,
            ..SweepReport::default()
        }
    );
    let stale = h.record(&stale).await;
    assert_eq!(stale.state, WorkspaceState::Deleted);
    assert_eq!(stale.delete_reason, Some(DeleteReason::CreationFailed));
    assert!(!h.fake.has(&stale.id), "what the attempt left was deleted");
    assert_eq!(h.record(&fresh).await.state, WorkspaceState::Creating);
    // The owner can create at once, and isn't told about the failed one.
    assert!(h.workspaces.last_deleted(&owner).await.unwrap().is_none());
    h.create(&owner).await;
}

#[tokio::test]
async fn a_row_made_active_after_the_snapshot_is_not_mistaken_for_a_lost_one() {
    // The plugin lists after the rows are read, so a workspace created between the two is in the
    // list and has a row; one created after the list has a row but isn't listed, which only
    // `creating` rows can be at that moment (and they are left alone).
    let h = Harness::new().await;
    let id = WorkspaceId::new("ws-inflight").unwrap();
    h.store
        .workspace_insert(doozers_store::NewWorkspace {
            id: id.clone(),
            environment: "box".into(),
            owner: chat("c1"),
            options: json!({}),
            key_user: None,
            now: h.clock_now(),
        })
        .await
        .unwrap();
    assert_eq!(
        h.workspaces.reconcile("box").await.unwrap(),
        SweepReport::default()
    );
    assert_eq!(h.record(&id).await.state, WorkspaceState::Creating);
}

#[tokio::test]
async fn startup_recovery_settles_what_the_previous_process_left() {
    let h = Harness::new().await;
    let creating = WorkspaceId::new("ws-creating").unwrap();
    h.store
        .workspace_insert(doozers_store::NewWorkspace {
            id: creating.clone(),
            environment: "box".into(),
            owner: chat("c1"),
            options: json!({}),
            key_user: None,
            now: h.clock_now(),
        })
        .await
        .unwrap();
    let running_job = h.create(&job(1)).await;
    let chat_workspace = h.create(&chat("c2")).await;

    assert_eq!(h.workspaces.recover().await.unwrap(), 2);
    assert_eq!(h.record(&creating).await.state, WorkspaceState::Deleting);
    let ended = h.record(&running_job.id).await;
    assert_eq!(ended.state, WorkspaceState::Deleting);
    assert_eq!(ended.delete_reason, Some(DeleteReason::JobRunEnded));
    assert_eq!(
        h.record(&chat_workspace.id).await.state,
        WorkspaceState::Active
    );
    // The sweep finishes the cleanup.
    assert_eq!(h.workspaces.sweep_once().await.cleaned, 2);
    assert!(!h.fake.has(&running_job.id));
    assert!(h.fake.has(&chat_workspace.id));
}

#[tokio::test]
async fn the_owner_of_a_turn_is_its_conversation_or_its_job_session() {
    let h = Harness::new().await;
    let key = doozers_store::SessionKey {
        agent: AgentId::new("jeeves").unwrap(),
        channel: "web".into(),
        conversation: ConversationId::new("c1").unwrap(),
    };
    let (direct, _) = h
        .store
        .open_session(
            key.clone(),
            SessionKind::Direct,
            Some(user("alice")),
            h.clock_now(),
        )
        .await
        .unwrap();
    assert_eq!(WorkspaceOwner::of_session(&direct), chat_of("c1", "alice"));
    // A new session of the same conversation (`/new`) is the same owner.
    h.store.end_session(direct.id, h.clock_now()).await.unwrap();
    let (again, _) = h
        .store
        .open_session(
            key.clone(),
            SessionKind::Direct,
            Some(user("alice")),
            h.clock_now(),
        )
        .await
        .unwrap();
    assert_ne!(again.id, direct.id);
    assert_eq!(WorkspaceOwner::of_session(&again), chat_of("c1", "alice"));
    // Another user who takes the conversation id has an owner of their own.
    h.store.end_session(again.id, h.clock_now()).await.unwrap();
    let (bobs, _) = h
        .store
        .open_session(key, SessionKind::Direct, Some(user("bob")), h.clock_now())
        .await
        .unwrap();
    assert_eq!(WorkspaceOwner::of_session(&bobs), chat_of("c1", "bob"));
    // A group's session has no user in its owner.
    let (group, _) = h
        .store
        .open_session(
            doozers_store::SessionKey {
                agent: AgentId::new("jeeves").unwrap(),
                channel: "chat".into(),
                conversation: ConversationId::new("#room").unwrap(),
            },
            SessionKind::Group,
            None,
            h.clock_now(),
        )
        .await
        .unwrap();
    assert!(matches!(
        WorkspaceOwner::of_session(&group),
        WorkspaceOwner::Conversation { user: None, .. }
    ));

    let isolated = h
        .store
        .create_job_session(AgentId::new("jeeves").unwrap(), None, h.clock_now())
        .await
        .unwrap();
    assert_eq!(
        WorkspaceOwner::of_session(&isolated),
        WorkspaceOwner::Job {
            session: isolated.id
        }
    );
}

#[tokio::test]
async fn the_sweep_task_runs_on_its_schedule_and_stops_with_the_token() {
    let h = Harness::new().await;
    let record = h.create(&chat("c1")).await;
    h.clock.advance(11 * MINUTE);

    let shutdown = CancellationToken::new();
    let sweep = h.workspaces.start_sweep(
        SweepSchedule {
            first_delay: Duration::from_millis(20),
            interval: Duration::from_millis(20),
        },
        shutdown.clone(),
    );
    eventually("the idle workspace is stopped", || {
        h.fake.running(&record.id) == Some(false)
    })
    .await;
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), sweep.closed())
        .await
        .expect("the sweep stops with the token");
}

#[tokio::test]
async fn a_plugin_that_connects_is_reconciled_at_once() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let lost = h.create(&owner).await;
    h.disconnect();
    h.fake.remove(&lost.id);
    h.fake.insert("stray", 2 * HOUR, true);

    let shutdown = CancellationToken::new();
    // The first sweep is an hour away: only the connection can do this.
    let sweep = h.workspaces.start_sweep(
        SweepSchedule {
            first_delay: HOUR,
            interval: HOUR,
        },
        shutdown.clone(),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        h.fake.has(&WorkspaceId::new("stray").unwrap()),
        "no plugin, no reconciliation"
    );
    h.connect();
    eventually("the stray workspace is deleted", || {
        !h.fake.has(&WorkspaceId::new("stray").unwrap())
    })
    .await;
    for _ in 0..500 {
        if h.workspaces.current(&owner).await.unwrap().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.workspaces
            .last_deleted(&owner)
            .await
            .unwrap()
            .unwrap()
            .delete_reason,
        Some(DeleteReason::RemovedOutsideDoozers)
    );
    shutdown.cancel();
    sweep.closed().await;
}

#[tokio::test]
async fn a_failing_list_skips_the_environment_and_tries_again_next_time() {
    let h = Harness::new().await;
    let record = h.create(&chat("c1")).await;
    h.clock.advance(30 * DAY);
    h.fake.fail_lists(true);
    assert_eq!(h.workspaces.sweep_once().await, SweepReport::default());
    assert!(h.workspaces.reconcile("box").await.is_err());
    assert_eq!(h.record(&record.id).await.state, WorkspaceState::Active);
    h.fake.fail_lists(false);
    assert_eq!(h.workspaces.sweep_once().await.deleted_inactive, 1);
}

#[tokio::test]
async fn options_round_trip_through_the_table_as_given() {
    let h = Harness::new().await;
    let options: Value = json!({"image": "alpine", "env": {"A": ["1", 2, null]}, "n": 1.5});
    let record = h
        .workspaces
        .create(&chat("c1"), "box", options.clone(), None)
        .await
        .unwrap();
    assert_eq!(h.record(&record.id).await.options, options);
}

// -- The operator's view: overview, stop, delete --

#[tokio::test]
async fn the_overview_joins_the_rows_with_what_the_plugin_says_and_who_holds_a_lease() {
    let h = Harness::new().await;
    let free = h.create(&chat("free")).await;
    let busy = h.create(&chat("busy")).await;
    h.fake.report_space(Some(5 << 30), Some(4096));
    let slot = LeaseSlot::default();
    h.workspaces.open(&busy.owner, &slot).await.unwrap();

    let overview = h.workspaces.overview().await.unwrap();
    let rows: Vec<_> = overview
        .workspaces
        .iter()
        .map(|w| (w.record.id.clone(), w.in_use))
        .collect();
    assert_eq!(rows.len(), 2);
    assert!(rows.contains(&(busy.id.clone(), true)), "{rows:?}");
    assert!(rows.contains(&(free.id.clone(), false)), "{rows:?}");
    let live = overview
        .workspaces
        .iter()
        .find(|w| w.record.id == free.id)
        .unwrap();
    let Live::Known(known) = &live.live else {
        panic!("{:?}", live.live);
    };
    assert_eq!(known.disk_used_bytes, Some(4096));
    assert_eq!(known.details, vec![WorkspaceDetail::new("Box", "fake")]);
    let environment = overview
        .environments
        .iter()
        .find(|e| e.name == "box")
        .unwrap();
    assert_eq!(environment.free_bytes, Some(5 << 30));
    assert_eq!(environment.problem, None);

    // The lease ends with the turn.
    drop(slot);
    let overview = h.workspaces.overview().await.unwrap();
    assert!(overview.workspaces.iter().all(|w| !w.in_use));
}

#[tokio::test]
async fn the_overview_says_unknown_for_an_environment_that_is_down_or_failing() {
    let h = Harness::new().await;
    let record = h.create(&chat("c1")).await;

    h.fake.fail_lists(true);
    let overview = h.workspaces.overview().await.unwrap();
    assert_eq!(overview.workspaces[0].live, Live::Unknown);
    let problem = overview
        .environments
        .iter()
        .find(|e| e.name == "box")
        .unwrap()
        .problem
        .clone()
        .unwrap();
    assert!(problem.contains("failed to list"), "{problem}");
    h.fake.fail_lists(false);

    h.disconnect();
    let overview = h.workspaces.overview().await.unwrap();
    assert_eq!(overview.workspaces[0].live, Live::Unknown);
    let problems: Vec<_> = overview
        .environments
        .iter()
        .map(|e| (e.name.as_str(), e.problem.as_deref()))
        .collect();
    assert!(
        problems.contains(&("box", Some("is not connected right now"))),
        "{problems:?}"
    );
    assert!(
        problems.contains(&("far", Some("is not connected right now"))),
        "{problems:?}"
    );

    // What the plugin lacks is told apart from not knowing.
    h.connect();
    h.fake.remove(&record.id);
    let overview = h.workspaces.overview().await.unwrap();
    assert_eq!(overview.workspaces[0].live, Live::Missing);
}

#[tokio::test]
async fn an_operator_stop_stops_the_workspace_and_changes_nothing_else() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;
    h.clock.advance(HOUR);

    h.workspaces.stop_by_id(&record.id).await.unwrap();
    assert_eq!(h.fake.running(&record.id), Some(false));
    assert_eq!(h.record(&record.id).await, record, "the row is untouched");

    // The next call starts it again.
    let opened = h
        .workspaces
        .open(&owner, &LeaseSlot::default())
        .await
        .unwrap();
    assert!(opened.notice.unwrap().contains("stopped"));
    assert_eq!(h.fake.running(&record.id), Some(true));
}

#[tokio::test]
async fn stopping_a_workspace_a_turn_is_using_stops_it_under_the_turn() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;
    let slot = LeaseSlot::default();
    h.workspaces.open(&owner, &slot).await.unwrap();
    assert!(h.workspaces.in_use(&record.id));

    h.workspaces.stop_by_id(&record.id).await.unwrap();
    assert_eq!(h.fake.running(&record.id), Some(false));
    // The turn's next call starts it again, with the lease it already holds.
    let opened = h.workspaces.open(&owner, &slot).await.unwrap();
    assert!(opened.notice.is_some());
    assert_eq!(h.fake.running(&record.id), Some(true));
}

#[tokio::test]
async fn stopping_needs_a_connected_active_workspace() {
    let h = Harness::new().await;
    let record = h.create(&chat("c1")).await;
    let missing = WorkspaceId::new("ws-nope").unwrap();
    assert!(matches!(
        h.workspaces.stop_by_id(&missing).await,
        Err(ManageError::NotFound)
    ));
    h.disconnect();
    assert!(matches!(
        h.workspaces.stop_by_id(&record.id).await,
        Err(ManageError::Unreachable { .. })
    ));
    h.connect();
    h.workspaces.delete_by_id(&record.id).await.unwrap();
    assert!(matches!(
        h.workspaces.stop_by_id(&record.id).await,
        Err(ManageError::NotFound)
    ));
}

#[tokio::test]
async fn an_operator_delete_uses_the_operator_reason_and_frees_the_owner() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;

    let deleted = h.workspaces.delete_by_id(&record.id).await.unwrap();
    assert_eq!(deleted.state, WorkspaceState::Deleted);
    assert_eq!(deleted.delete_reason, Some(DeleteReason::Operator));
    assert!(!h.fake.has(&record.id));
    assert!(h.workspaces.current(&owner).await.unwrap().is_none());
    let last = h.workspaces.last_deleted(&owner).await.unwrap().unwrap();
    assert_eq!(last.delete_reason, Some(DeleteReason::Operator));
    assert!(matches!(
        h.workspaces.open(&owner, &LeaseSlot::default()).await,
        Err(WorkspaceError::NoWorkspace)
    ));
    // The owner can create again.
    h.create(&owner).await;

    // A second delete finds nothing.
    assert!(matches!(
        h.workspaces.delete_by_id(&record.id).await,
        Err(ManageError::NotFound)
    ));
}

#[tokio::test]
async fn deleting_a_workspace_a_turn_is_using_stops_it_first_and_the_turn_finds_it_gone() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;
    let slot = LeaseSlot::default();
    h.workspaces.open(&owner, &slot).await.unwrap();
    assert!(h.workspaces.in_use(&record.id));

    let deleted = h.workspaces.delete_by_id(&record.id).await.unwrap();
    assert_eq!(deleted.state, WorkspaceState::Deleted);
    let log = h.fake.log();
    let tail = &log[log.len() - 2..];
    assert_eq!(
        tail,
        [
            format!("stop {}", record.id),
            format!("delete {}", record.id)
        ]
    );
    assert!(!h.fake.has(&record.id));
    // The turn still holds its lease, and its next call finds no workspace.
    assert!(matches!(
        h.workspaces.open(&owner, &slot).await,
        Err(WorkspaceError::NoWorkspace)
    ));
}

#[tokio::test]
async fn an_operator_delete_while_the_plugin_is_down_is_finished_later() {
    let h = Harness::new().await;
    let owner = chat("c1");
    let record = h.create(&owner).await;
    h.disconnect();

    let deleted = h.workspaces.delete_by_id(&record.id).await.unwrap();
    assert_eq!(deleted.state, WorkspaceState::Deleting);
    assert_eq!(deleted.delete_reason, Some(DeleteReason::Operator));
    assert!(h.workspaces.current(&owner).await.unwrap().is_none());
    assert!(h.fake.has(&record.id), "the plugin wasn't told yet");

    h.connect();
    h.workspaces.reconcile("box").await.unwrap();
    assert!(!h.fake.has(&record.id));
    assert_eq!(h.record(&record.id).await.state, WorkspaceState::Deleted);
}

#[tokio::test]
async fn a_workspace_still_being_created_is_not_stopped_or_deleted_by_an_operator() {
    let h = Harness::new().await;
    let id = WorkspaceId::new("ws-making").unwrap();
    h.store
        .workspace_insert(doozers_store::NewWorkspace {
            id: id.clone(),
            environment: "box".into(),
            owner: chat("c1"),
            options: json!({}),
            key_user: None,
            now: h.clock_now(),
        })
        .await
        .unwrap();
    assert!(matches!(
        h.workspaces.stop_by_id(&id).await,
        Err(ManageError::StillCreating)
    ));
    assert!(matches!(
        h.workspaces.delete_by_id(&id).await,
        Err(ManageError::StillCreating)
    ));
    assert_eq!(h.record(&id).await.state, WorkspaceState::Creating);
}

#[tokio::test]
async fn the_registry_keeps_the_capabilities_an_environment_plugin_reported() {
    let h = Harness::new().await;
    let ssh_relay = |h: &Harness| {
        h.registry
            .environment("box")
            .unwrap()
            .capabilities
            .ssh_relay
    };
    assert!(!ssh_relay(&h));

    h.disconnect();
    h.registry
        .register_environment(
            &name("box"),
            h.fake.clone(),
            EnvironmentInfo {
                options_schema: json!({"type": "object"}),
            },
            crate::registry::EnvironmentCapabilities::new(true),
        )
        .unwrap();
    assert!(ssh_relay(&h));
    // Kept while the plugin is away, replaced by the next connection's.
    h.disconnect();
    assert!(ssh_relay(&h));
    h.connect();
    assert!(!ssh_relay(&h));
}

// ---- whose SSH keys a workspace gets, and its relay ----

fn user(name: &str) -> UserId {
    UserId::new(name).unwrap()
}

/// A request from client connection `connection` on a workspace's relay.
fn agent_request(connection: u64, message: Vec<u8>) -> RelayRequest {
    RelayRequest::Agent(doozers_types::AgentRequest {
        connection,
        message,
    })
}

/// The agent message of the next reply on the relay of `id`.
async fn agent_reply(h: &Harness, id: &WorkspaceId) -> Vec<u8> {
    match h.fake.relay_reply(id).await.expect("the relay ended") {
        RelayReply::Agent(reply) => reply.message,
        other => panic!("{other:?}"),
    }
}

/// The labels of the keys the agent of workspace `id` offers on `connection`.
async fn offered(h: &Harness, id: &WorkspaceId, connection: u64) -> Vec<String> {
    h.fake.relay_send(id, agent_request(connection, vec![11]));
    crate::ssh::agent_tests::labels(&agent_reply(h, id).await)
}

async fn open(h: &Harness, owner: &WorkspaceOwner) -> Opened {
    h.workspaces
        .open(owner, &LeaseSlot::default())
        .await
        .unwrap()
}

#[tokio::test]
async fn the_key_user_comes_from_the_creating_turns_session_and_requester() {
    let h = Harness::new().await;
    let now = h.clock_now();
    let session = |kind: SessionKind, user: Option<&str>| doozers_store::Session {
        id: doozers_store::SessionId::new(1),
        agent: AgentId::new("jeeves").unwrap(),
        kind,
        channel: (kind != SessionKind::Job).then(|| "web".to_owned()),
        conversation: (kind != SessionKind::Job).then(|| ConversationId::new("c1").unwrap()),
        user: user.map(user_id),
        started_at: now,
        ended_at: None,
        last_message_at: None,
    };
    fn user_id(name: &str) -> UserId {
        UserId::new(name).unwrap()
    }
    let owner = user("owner");
    let alice = user("alice");

    // A direct conversation: its user, whoever the requester is.
    assert_eq!(
        key_user(&session(SessionKind::Direct, Some("owner")), Some(&owner)),
        Some(owner.clone())
    );
    assert_eq!(
        key_user(&session(SessionKind::Direct, Some("owner")), None),
        Some(owner.clone())
    );
    assert_eq!(
        key_user(&session(SessionKind::Direct, Some("alice")), Some(&owner)),
        Some(alice.clone())
    );
    assert_eq!(
        key_user(&session(SessionKind::Direct, None), Some(&owner)),
        None
    );
    // An isolated job run: the requester, the job's creator. A config job has none.
    assert_eq!(
        key_user(&session(SessionKind::Job, Some("owner")), Some(&alice)),
        Some(alice.clone())
    );
    assert_eq!(
        key_user(&session(SessionKind::Job, Some("owner")), None),
        None
    );
    // A group conversation: none, whoever speaks.
    assert_eq!(
        key_user(&session(SessionKind::Group, None), Some(&owner)),
        None
    );
    assert_eq!(
        key_user(&session(SessionKind::Group, Some("owner")), Some(&owner)),
        None
    );
}

#[tokio::test]
async fn a_workspace_keeps_its_key_user_through_a_reset() {
    let h = Harness::new().await;
    let owner = user("owner");
    let first = h.create_for(&chat("c1"), Some(&owner)).await;
    assert_eq!(first.key_user, Some(owner.clone()));
    assert_eq!(h.record(&first.id).await.key_user, Some(owner.clone()));
    let none = h.create_for(&chat("c2"), None).await;
    assert_eq!(none.key_user, None);

    for (conversation, expected) in [("c1", Some(owner.clone())), ("c2", None)] {
        let reset = h
            .workspaces
            .recreate(&chat(conversation), DeleteReason::UserReset)
            .await
            .unwrap()
            .unwrap();
        let created = reset.created.unwrap();
        assert_ne!(created.id, reset.discarded.id);
        assert_eq!(created.key_user, expected, "{conversation}");
        assert_eq!(h.record(&created.id).await.key_user, expected);
    }
}

#[tokio::test]
async fn opening_a_workspace_opens_one_relay_that_answers_with_the_key_users_keys() {
    let h = Harness::new().await;
    h.connect_relaying();
    let owner = user("owner");
    let alice = user("alice");
    h.ssh_keys
        .generate(&owner, crate::ssh::KeyType::Ed25519, Some("laptop"))
        .await
        .unwrap();
    h.ssh_keys
        .generate(&alice, crate::ssh::KeyType::Ed25519, Some("hers"))
        .await
        .unwrap();

    // Creating doesn't open it; the first open does, and a second open doesn't open another.
    let record = h.create_for(&chat("c1"), Some(&owner)).await;
    assert_eq!(h.fake.relay_calls(), 0);
    open(&h, &chat("c1")).await;
    assert_eq!(h.fake.relay_calls(), 1);
    h.fake.relay_settled(&record.id, true).await;
    open(&h, &chat("c1")).await;
    assert_eq!(h.fake.relay_calls(), 1);
    assert_eq!(offered(&h, &record.id, 1).await, ["laptop"]);

    // A workspace without a key user gets a relay too, and an agent with no keys.
    let group = h.create_for(&chat("group"), None).await;
    open(&h, &chat("group")).await;
    assert_eq!(h.fake.relay_calls(), 2);
    assert!(offered(&h, &group.id, 1).await.is_empty());

    // An isolated job run gets its creator's keys, and a config job's none.
    let runtime_job = h.create_for(&job(41), Some(&alice)).await;
    let config_job = h.create_for(&job(42), None).await;
    open(&h, &job(41)).await;
    open(&h, &job(42)).await;
    assert_eq!(h.fake.relay_calls(), 4);
    assert_eq!(offered(&h, &runtime_job.id, 1).await, ["hers"]);
    assert!(offered(&h, &config_job.id, 1).await.is_empty());
}

#[tokio::test]
async fn a_closed_connection_is_forgotten() {
    let h = Harness::new().await;
    h.connect_relaying();
    let owner = user("owner");
    h.ssh_keys
        .generate(&owner, crate::ssh::KeyType::Ed25519, Some("laptop"))
        .await
        .unwrap();
    let record = h.create_for(&chat("c1"), Some(&owner)).await;
    open(&h, &chat("c1")).await;

    // Connection 5 is bound as forwarded, so it sees no keys; connection 6 is unaffected.
    let host = crate::ssh::agent_tests::ed25519_host();
    let signature = crate::ssh::agent_tests::host_signature(&host, b"session");
    let bind = crate::ssh::agent_tests::bind_request(host.public_key(), b"session", &signature, 1);
    h.fake.relay_send(&record.id, agent_request(5, bind));
    assert_eq!(agent_reply(&h, &record.id).await, [6]);
    assert!(offered(&h, &record.id, 5).await.is_empty());
    assert_eq!(offered(&h, &record.id, 6).await, ["laptop"]);

    // The plugin says 5 closed: a new client that gets the number starts fresh.
    h.fake.relay_send(
        &record.id,
        RelayRequest::AgentClosed(doozers_types::AgentClosed { connection: 5 }),
    );
    assert_eq!(offered(&h, &record.id, 5).await, ["laptop"]);
}

#[tokio::test]
async fn a_relay_that_ended_is_opened_again_on_the_next_open() {
    let h = Harness::new().await;
    h.connect_relaying();
    let record = h.create_for(&chat("c1"), None).await;
    open(&h, &chat("c1")).await;
    h.fake.relay_settled(&record.id, true).await;

    // The plugin ends it (a restart, say): the next open starts another.
    h.fake.relay_end(&record.id);
    h.fake.relay_settled(&record.id, false).await;
    for _ in 0..500 {
        if !h.workspaces.inner.relays.running(&record.id).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.fake.relay_calls(), 1);
    open(&h, &chat("c1")).await;
    assert_eq!(h.fake.relay_calls(), 2);
    h.fake.relay_settled(&record.id, true).await;
    assert!(offered(&h, &record.id, 1).await.is_empty());
}

#[tokio::test]
async fn a_relay_that_cant_be_opened_doesnt_fail_the_open_and_is_tried_again() {
    let h = Harness::new().await;
    h.connect_relaying();
    h.create_for(&chat("c1"), None).await;
    h.fake.fail_relays(true);
    // The workspace is usable without it.
    open(&h, &chat("c1")).await;
    assert_eq!(h.fake.relay_calls(), 1);
    open(&h, &chat("c1")).await;
    assert_eq!(h.fake.relay_calls(), 2, "tried again on the next use");

    h.fake.fail_relays(false);
    open(&h, &chat("c1")).await;
    assert_eq!(h.fake.relay_calls(), 3);
    open(&h, &chat("c1")).await;
    assert_eq!(h.fake.relay_calls(), 3, "and now it is open");
}

#[tokio::test]
async fn a_workspace_that_was_started_or_recreated_gets_a_new_relay() {
    let h = Harness::new().await;
    h.connect_relaying();
    let owner = chat("c1");
    let record = h.create_for(&owner, None).await;
    open(&h, &owner).await;
    assert_eq!(h.fake.relay_calls(), 1);
    assert_eq!(h.fake.relays_dropped(), 0);

    // Stopped behind the core's back: the open starts it, and the old relay is replaced even
    // though it hadn't noticed.
    h.fake.stop_workspace(&record.id).await.unwrap();
    let opened = open(&h, &owner).await;
    assert!(opened.notice.unwrap().contains("started again"));
    assert_eq!(h.fake.relay_calls(), 2);
    h.fake.relay_settled(&record.id, true).await;
    eventually("the old relay is dropped", || h.fake.relays_dropped() == 1).await;

    // The same for a recreated one.
    h.fake.stop_workspace(&record.id).await.unwrap();
    h.fake.recreate_next_start();
    let opened = open(&h, &owner).await;
    assert!(opened.notice.unwrap().contains("recreated"));
    assert_eq!(h.fake.relay_calls(), 3);
    eventually("the second relay is dropped", || {
        h.fake.relays_dropped() == 2
    })
    .await;

    // A running workspace keeps its relay.
    open(&h, &owner).await;
    assert_eq!(h.fake.relay_calls(), 3);
}

#[tokio::test]
async fn deleting_stopping_and_the_sweep_end_the_relay() {
    let h = Harness::new().await;
    h.connect_relaying();

    // The owner's delete.
    let first = h.create_for(&chat("c1"), None).await;
    open(&h, &chat("c1")).await;
    h.fake.relay_settled(&first.id, true).await;
    h.workspaces
        .delete(&chat("c1"), DeleteReason::ModelDelete)
        .await
        .unwrap();
    h.fake.relay_settled(&first.id, false).await;
    assert!(!h.workspaces.inner.relays.running(&first.id).await);

    // An operator's stop, and the next call opening it again.
    let second = h.create_for(&chat("c2"), None).await;
    open(&h, &chat("c2")).await;
    h.fake.relay_settled(&second.id, true).await;
    h.workspaces.stop_by_id(&second.id).await.unwrap();
    h.fake.relay_settled(&second.id, false).await;
    assert_eq!(h.fake.running(&second.id), Some(false));
    let calls = h.fake.relay_calls();
    open(&h, &chat("c2")).await;
    assert_eq!(h.fake.relay_calls(), calls + 1);
    h.fake.relay_settled(&second.id, true).await;

    // The sweep's stop of an idle workspace.
    h.clock.advance(11 * MINUTE);
    let report = h.workspaces.sweep_once().await;
    assert_eq!(report.stopped, 1);
    h.fake.relay_settled(&second.id, false).await;

    // An operator's delete, and the sweep deleting an abandoned workspace.
    let third = h.create_for(&chat("c3"), None).await;
    open(&h, &chat("c3")).await;
    h.fake.relay_settled(&third.id, true).await;
    h.workspaces.delete_by_id(&third.id).await.unwrap();
    h.fake.relay_settled(&third.id, false).await;
    open(&h, &chat("c2")).await;
    h.fake.relay_settled(&second.id, true).await;
    h.clock.advance(8 * DAY);
    let report = h.workspaces.sweep_once().await;
    assert_eq!(report.deleted_inactive, 1);
    h.fake.relay_settled(&second.id, false).await;
}

#[tokio::test]
async fn an_environment_that_cant_relay_never_gets_one() {
    let h = Harness::new().await;
    let record = h.create_for(&chat("c1"), Some(&user("owner"))).await;
    open(&h, &chat("c1")).await;
    open(&h, &chat("c1")).await;
    assert_eq!(h.fake.relay_calls(), 0);
    assert!(!h.workspaces.inner.relays.running(&record.id).await);
}

// ---- known hosts over the relay ----

/// A key as `ssh` offers it: its type and its wire blob.
fn host_key() -> (String, Vec<u8>) {
    let host = crate::ssh::agent_tests::ed25519_host();
    (
        host.public_key().algorithm().as_str().to_owned(),
        host.public_key().to_bytes().unwrap(),
    )
}

fn known_hosts_request(id: u64, host: &str, key: &(String, Vec<u8>)) -> RelayRequest {
    RelayRequest::KnownHosts(doozers_types::KnownHostsRequest {
        id,
        host: host.to_owned(),
        port: 22,
        key_type: key.0.clone(),
        key: key.1.clone(),
    })
}

/// The next reply on the relay of `id`, which must be a known-hosts one.
async fn known_hosts_reply(h: &Harness, id: &WorkspaceId) -> doozers_types::KnownHostsReply {
    match h.fake.relay_reply(id).await.expect("the relay ended") {
        RelayReply::KnownHosts(reply) => reply,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_lookup_on_the_relay_records_the_host_for_the_workspaces_environment_and_owner() {
    let h = Harness::new().await;
    h.connect_relaying();
    let owner = user("owner");
    let record = h.create_for(&chat("c1"), Some(&owner)).await;
    open(&h, &chat("c1")).await;
    let key = host_key();
    h.fake
        .scan_returns("github.com", 22, Scan::Key(key.1.clone()));

    h.fake
        .relay_send(&record.id, known_hosts_request(7, "github.com", &key));
    let reply = known_hosts_reply(&h, &record.id).await;
    assert_eq!((reply.id, reply.keys), (7, vec![key.1.clone()]));

    let rows = h.workspaces.known_hosts().all().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].environment, "box");
    assert_eq!(rows[0].host, "github.com");
    assert_eq!(rows[0].first_workspace.as_deref(), Some(record.id.as_str()));
    assert_eq!(rows[0].first_agent.as_deref(), Some("jeeves"));
    assert_eq!(rows[0].first_user.as_deref(), Some("owner"));
    // The scan was asked of the workspace's own environment, for the workspace.
    let scans = h.fake.scans();
    assert_eq!(scans.len(), 1);
    assert_eq!(scans[0].0, record.id);

    // A workspace without a key user records no user, and the stored key answers it.
    let group = h.create_for(&chat("group"), None).await;
    open(&h, &chat("group")).await;
    h.fake
        .relay_send(&group.id, known_hosts_request(1, "github.com", &key));
    assert_eq!(known_hosts_reply(&h, &group.id).await.keys, [key.1]);
    assert_eq!(h.fake.scans().len(), 1, "a known host isn't scanned again");
}

#[tokio::test]
async fn a_slow_scan_doesnt_hold_up_agent_requests_on_the_same_relay() {
    let h = Harness::new().await;
    h.connect_relaying();
    let owner = user("owner");
    h.ssh_keys
        .generate(&owner, crate::ssh::KeyType::Ed25519, Some("laptop"))
        .await
        .unwrap();
    let record = h.create_for(&chat("c1"), Some(&owner)).await;
    open(&h, &chat("c1")).await;
    let key = host_key();
    let gate = Arc::new(tokio::sync::Notify::new());
    h.fake
        .scan_returns("slow.example", 22, Scan::Gated(gate.clone(), key.1.clone()));

    h.fake
        .relay_send(&record.id, known_hosts_request(1, "slow.example", &key));
    // Wait until the scan is running, then ask the agent: it answers while the scan waits.
    for _ in 0..500 {
        if !h.fake.scans().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.fake.scans().len(), 1);
    assert_eq!(offered(&h, &record.id, 1).await, ["laptop"]);

    // Releasing the scan answers the lookup.
    gate.notify_one();
    let reply = known_hosts_reply(&h, &record.id).await;
    assert_eq!((reply.id, reply.keys), (1, vec![key.1]));
}

#[tokio::test]
async fn lookups_past_the_in_flight_cap_are_answered_with_no_key() {
    let h = Harness::new().await;
    h.connect_relaying();
    let record = h.create_for(&chat("c1"), None).await;
    open(&h, &chat("c1")).await;
    let key = host_key();
    let gate = Arc::new(tokio::sync::Notify::new());
    let cap = crate::ssh::relay::MAX_LOOKUPS as u64;
    for id in 1..=cap + 1 {
        h.fake.scan_returns(
            &format!("h{id}.example"),
            22,
            Scan::Gated(gate.clone(), key.1.clone()),
        );
    }
    // The first `cap` lookups wait on their scans.
    for id in 1..=cap {
        h.fake.relay_send(
            &record.id,
            known_hosts_request(id, &format!("h{id}.example"), &key),
        );
    }
    for _ in 0..500 {
        if h.fake.scans().len() as u64 == cap {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.fake.scans().len() as u64, cap);

    // One more is refused at once, without a scan.
    h.fake.relay_send(
        &record.id,
        known_hosts_request(cap + 1, &format!("h{}.example", cap + 1), &key),
    );
    let refused = known_hosts_reply(&h, &record.id).await;
    assert_eq!((refused.id, refused.keys.len()), (cap + 1, 0));
    assert_eq!(h.fake.scans().len() as u64, cap);

    // The others finish once their scans do, and make room for a new lookup.
    for _ in 0..cap {
        gate.notify_one();
    }
    let mut answered = Vec::new();
    for _ in 0..cap {
        let reply = known_hosts_reply(&h, &record.id).await;
        assert_eq!(reply.keys, vec![key.1.clone()]);
        answered.push(reply.id);
    }
    answered.sort_unstable();
    assert_eq!(answered, (1..=cap).collect::<Vec<_>>());
    h.fake
        .relay_send(&record.id, known_hosts_request(99, "h1.example", &key));
    assert_eq!(known_hosts_reply(&h, &record.id).await.keys, [key.1]);
}

#[tokio::test]
async fn a_lookup_that_names_no_host_the_core_knows_gets_no_key() {
    let h = Harness::new().await;
    h.connect_relaying();
    let record = h.create_for(&chat("c1"), None).await;
    open(&h, &chat("c1")).await;
    let key = host_key();
    h.fake
        .relay_send(&record.id, known_hosts_request(3, "no route", &key));
    let reply = known_hosts_reply(&h, &record.id).await;
    assert_eq!((reply.id, reply.keys.len()), (3, 0));
    assert!(h.workspaces.known_hosts().all().await.unwrap().is_empty());
}

#[tokio::test]
async fn two_users_of_one_conversation_id_over_time_get_workspaces_of_their_own() {
    // An IRC direct conversation is the sender's nickname: Alice used it, her session ended, and
    // Bob took the nickname.
    let h = Harness::new().await;
    h.connect_relaying();
    let alice = user("alice");
    let bob = user("bob");
    h.ssh_keys
        .generate(&alice, crate::ssh::KeyType::Ed25519, Some("hers"))
        .await
        .unwrap();
    h.ssh_keys
        .generate(&bob, crate::ssh::KeyType::Ed25519, Some("his"))
        .await
        .unwrap();
    let (hers_owner, his_owner) = (chat_of("nick", "alice"), chat_of("nick", "bob"));
    let hers = h.create_for(&hers_owner, Some(&alice)).await;
    h.fake.put_file(&hers.id, "tax-return.txt", b"alice's");

    // Bob's turns find no workspace of his, whatever Alice has, and creating his own doesn't
    // touch hers.
    assert!(matches!(
        h.workspaces.open(&his_owner, &LeaseSlot::default()).await,
        Err(WorkspaceError::NoWorkspace)
    ));
    let his = h.create_for(&his_owner, Some(&bob)).await;
    assert_ne!(his.id, hers.id);
    assert_eq!(h.record(&hers.id).await.state, WorkspaceState::Active);
    assert!(
        h.fake
            .read_file(&his.id, "tax-return.txt", 100)
            .await
            .is_err(),
        "the new workspace has none of the old one's files"
    );

    // Each one's turns open their own, and its relay offers only that user's keys.
    let opened = open(&h, &hers_owner).await;
    assert_eq!(opened.record.id, hers.id);
    assert_eq!(opened.notice, None);
    let opened = open(&h, &his_owner).await;
    assert_eq!(opened.record.id, his.id);
    assert_eq!(opened.notice, None);
    h.fake.relay_settled(&hers.id, true).await;
    h.fake.relay_settled(&his.id, true).await;
    assert_eq!(offered(&h, &hers.id, 1).await, ["hers"]);
    assert_eq!(offered(&h, &his.id, 1).await, ["his"]);

    // A reset, a delete, and what the model is told after one are each user's own.
    h.workspaces
        .recreate(&his_owner, DeleteReason::UserReset)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        h.workspaces.current(&hers_owner).await.unwrap().unwrap().id,
        hers.id
    );
    assert_eq!(h.workspaces.last_deleted(&hers_owner).await.unwrap(), None);
    h.workspaces
        .delete(&hers_owner, DeleteReason::ModelDelete)
        .await
        .unwrap();
    assert!(h.workspaces.current(&his_owner).await.unwrap().is_some());
    assert_eq!(
        h.workspaces
            .last_deleted(&his_owner)
            .await
            .unwrap()
            .unwrap()
            .delete_reason,
        Some(DeleteReason::UserReset)
    );

    // A group conversation's workspace, with no user, is a third owner.
    let group = h.create_for(&chat("nick"), None).await;
    assert_ne!(group.id, hers.id);
    assert!(h.workspaces.current(&his_owner).await.unwrap().is_some());
}

#[tokio::test]
async fn a_turn_still_running_after_the_conversation_changed_hands_keeps_its_own_workspace() {
    let h = Harness::new().await;
    let (hers_owner, his_owner) = (chat_of("nick", "alice"), chat_of("nick", "bob"));
    let hers = h.create_for(&hers_owner, Some(&user("alice"))).await;

    // Alice's turn is running (it holds the lease) when `/new` ended her session and Bob's first
    // turn started in the same conversation id.
    let slot = LeaseSlot::default();
    h.workspaces.open(&hers_owner, &slot).await.unwrap();
    let his = h.create_for(&his_owner, Some(&user("bob"))).await;
    open(&h, &his_owner).await;

    // Neither deleted the other's, and each turn's next call finds its own.
    assert_eq!(h.record(&hers.id).await.state, WorkspaceState::Active);
    assert_eq!(h.record(&his.id).await.state, WorkspaceState::Active);
    let again = h.workspaces.open(&hers_owner, &slot).await.unwrap();
    assert_eq!(again.record.id, hers.id);
    assert_eq!(again.notice, None);
    assert_eq!(slot.held(), Some(hers.id.clone()));
    assert_eq!(open(&h, &his_owner).await.record.id, his.id);
}
