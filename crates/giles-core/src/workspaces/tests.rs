//! The workspace service against an in-process environment: owners and slots, leases, the sweep,
//! and reconciliation.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use giles_store::{SessionKind, Store};
use giles_types::{
    AgentId, Block, ConversationId, EnvironmentInfo, Error, Message, MessageMeta, Role, Timestamp,
    WorkspaceId,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::fake::{Fake, TestClock};
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
        std::fs::write(dir.path().join("giles.toml"), CONFIG).unwrap();
        let config = Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let registry = Arc::new(Registry::new(root, Arc::new(config)));
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let clock = TestClock::new();
        let fake = Fake::new(clock.clone());
        let workspaces = Workspaces::new(store.clone(), registry.clone(), clock.clone());
        let harness = Self {
            _dir: dir,
            store,
            registry,
            clock,
            fake,
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
            )
            .unwrap();
    }

    fn disconnect(&self) {
        self.registry.unregister_environment(&name("box")).unwrap();
    }

    async fn create(&self, owner: &WorkspaceOwner) -> WorkspaceRecord {
        self.workspaces
            .create(owner, "box", json!({"image": "alpine"}))
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
        } = owner
        else {
            panic!("not a conversation");
        };
        let key = giles_store::SessionKey {
            agent: agent.clone(),
            channel: channel.clone(),
            conversation: conversation.clone(),
        };
        let (session, _) = self
            .store
            .open_session(key, SessionKind::Direct, None, at)
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
    }
}

fn job(session: i64) -> WorkspaceOwner {
    WorkspaceOwner::Job {
        session: giles_store::SessionId::new(session),
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
        .create(&owner, "box", json!({}))
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
        .create(&owner, "nope", json!({}))
        .await
        .unwrap_err();
    assert!(
        matches!(err, WorkspaceError::UnknownEnvironment { .. }),
        "{err}"
    );
    let err = h
        .workspaces
        .create(&owner, "far", json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, WorkspaceError::Unreachable { .. }), "{err}");
    let err = h
        .workspaces
        .create(&owner, "box", json!(["image"]))
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
        .create(&owner, "box", json!({"image": "nope"}))
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
        .create(&chat("c2"), "box", json!({}))
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
        .create(&owner, "box", json!({}))
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
        giles_types::Error::InvalidRequest("no space left".to_owned()),
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
    let owner = chat("c1");
    let WorkspaceOwner::Conversation {
        agent,
        channel,
        conversation,
    } = owner.clone()
    else {
        unreachable!();
    };
    let key = giles_store::SessionKey {
        agent,
        channel,
        conversation,
    };
    assert_eq!(h.workspaces.reset(&key).await, EnvReset::NoWorkspace);

    let old = h.create(&owner).await;
    assert_eq!(h.workspaces.reset(&key).await, EnvReset::Done);
    let new = h.workspaces.current(&owner).await.unwrap().unwrap();
    assert_ne!(new.id, old.id);
    assert_eq!(new.options, old.options);

    h.disconnect();
    let EnvReset::Failed(message) = h.workspaces.reset(&key).await else {
        panic!("not refused");
    };
    assert!(
        message.starts_with("The workspace was not reset: "),
        "{message}"
    );
    assert!(message.contains("not reachable"), "{message}");
    h.connect();

    h.fake.fail_next_create(
        giles_types::Error::InvalidRequest("no space left".to_owned()),
        false,
    );
    let EnvReset::Failed(message) = h.workspaces.reset(&key).await else {
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
    assert_eq!(last.delete_reason, Some(DeleteReason::RemovedOutsideGiles));
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
        .workspace_insert(giles_store::NewWorkspace {
            id: WorkspaceId::new("pending").unwrap(),
            environment: "box".into(),
            owner: chat("c9"),
            options: json!({}),
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
    assert_eq!(last.delete_reason, Some(DeleteReason::RemovedOutsideGiles));
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
            .workspace_insert(giles_store::NewWorkspace {
                id: id.clone(),
                environment: "box".into(),
                owner,
                options: json!({}),
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
        .workspace_insert(giles_store::NewWorkspace {
            id: id.clone(),
            environment: "box".into(),
            owner: chat("c1"),
            options: json!({}),
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
        .workspace_insert(giles_store::NewWorkspace {
            id: creating.clone(),
            environment: "box".into(),
            owner: chat("c1"),
            options: json!({}),
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
    let key = giles_store::SessionKey {
        agent: AgentId::new("jeeves").unwrap(),
        channel: "web".into(),
        conversation: ConversationId::new("c1").unwrap(),
    };
    let (direct, _) = h
        .store
        .open_session(key.clone(), SessionKind::Direct, None, h.clock_now())
        .await
        .unwrap();
    assert_eq!(WorkspaceOwner::of_session(&direct), chat("c1"));
    // A new session of the same conversation (`/new`) is the same owner.
    h.store.end_session(direct.id, h.clock_now()).await.unwrap();
    let (again, _) = h
        .store
        .open_session(key, SessionKind::Direct, None, h.clock_now())
        .await
        .unwrap();
    assert_ne!(again.id, direct.id);
    assert_eq!(WorkspaceOwner::of_session(&again), chat("c1"));

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
        Some(DeleteReason::RemovedOutsideGiles)
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
        .create(&chat("c1"), "box", options.clone())
        .await
        .unwrap();
    assert_eq!(h.record(&record.id).await.options, options);
}
