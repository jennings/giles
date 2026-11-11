//! An in-process [`Environment`] and a manual clock for tests of the workspace service and of
//! the code that calls it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use giles_types::{
    Environment, EnvironmentInfo, Error, ExecEvent, ExecRequest, FindRequest, FindResult,
    GrepRequest, GrepResult, ReadFile, Result, StartOutcome, Timestamp, Workspace, WorkspaceId,
    WorkspaceList, WorkspaceState as PluginState,
};
use serde_json::{Value, json};

use crate::sessions::Clock;

/// The time tests start at.
pub(crate) const START: &str = "2026-11-10T12:00:00Z";

/// A clock that moves only when told to.
pub(crate) struct TestClock(Mutex<Timestamp>);

impl TestClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(START.parse().unwrap())))
    }

    pub(crate) fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *now = now.checked_add(by).unwrap();
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One workspace the fake holds.
#[derive(Debug, Clone)]
pub(crate) struct Held {
    pub(crate) running: bool,
    pub(crate) created_at: Timestamp,
    pub(crate) options: Value,
}

#[derive(Default)]
struct State {
    workspaces: BTreeMap<WorkspaceId, Held>,
    /// Every call that changed something, as `<call> <id>`.
    log: Vec<String>,
    /// The next `create_workspace` fails with this, after leaving `partial` behind if set.
    create_error: Option<Error>,
    partial: bool,
    /// `delete_workspace` and `list_workspaces` fail while set.
    delete_fails: bool,
    list_fails: bool,
    /// The next `start_workspace` of a stopped workspace says it recreated it.
    recreate: bool,
}

/// An environment that keeps its workspaces in memory.
pub(crate) struct Fake {
    state: Mutex<State>,
    clock: Arc<TestClock>,
}

impl Fake {
    pub(crate) fn new(clock: Arc<TestClock>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            clock,
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts a workspace there as if made outside Giles, `age` ago.
    pub(crate) fn insert(&self, id: &str, age: Duration, running: bool) {
        let created_at = self.clock.now().checked_sub(age).unwrap();
        self.state().workspaces.insert(
            WorkspaceId::new(id).unwrap(),
            Held {
                running,
                created_at,
                options: json!({}),
            },
        );
    }

    pub(crate) fn remove(&self, id: &WorkspaceId) {
        self.state().workspaces.remove(id);
    }

    pub(crate) fn held(&self) -> BTreeMap<WorkspaceId, Held> {
        self.state().workspaces.clone()
    }

    pub(crate) fn has(&self, id: &WorkspaceId) -> bool {
        self.state().workspaces.contains_key(id)
    }

    pub(crate) fn running(&self, id: &WorkspaceId) -> Option<bool> {
        self.state().workspaces.get(id).map(|held| held.running)
    }

    /// The changing calls so far, e.g. `stop ws-1`.
    pub(crate) fn log(&self) -> Vec<String> {
        self.state().log.clone()
    }

    pub(crate) fn fail_next_create(&self, error: Error, leave_partial: bool) {
        let mut state = self.state();
        state.create_error = Some(error);
        state.partial = leave_partial;
    }

    pub(crate) fn fail_deletes(&self, fails: bool) {
        self.state().delete_fails = fails;
    }

    pub(crate) fn fail_lists(&self, fails: bool) {
        self.state().list_fails = fails;
    }

    pub(crate) fn recreate_next_start(&self) {
        self.state().recreate = true;
    }

    fn workspace(id: &WorkspaceId, held: &Held) -> Workspace {
        Workspace {
            id: id.clone(),
            description: "fake box".to_owned(),
            working_directory: "/work".to_owned(),
            persistence: "files persist".to_owned(),
            created_at: held.created_at,
            state: if held.running {
                PluginState::Running
            } else {
                PluginState::Stopped
            },
            disk_used_bytes: None,
        }
    }
}

fn unused<T>() -> Result<T> {
    Err(Error::other("not used by these tests"))
}

#[async_trait]
impl Environment for Fake {
    async fn info(&self) -> Result<EnvironmentInfo> {
        Ok(EnvironmentInfo {
            options_schema: json!({"type": "object"}),
        })
    }

    async fn create_workspace(&self, id: &WorkspaceId, options: Value) -> Result<Workspace> {
        let mut state = self.state();
        let held = Held {
            running: true,
            created_at: self.clock.now(),
            options,
        };
        if let Some(error) = state.create_error.take() {
            if state.partial {
                state.workspaces.insert(id.clone(), held);
            }
            return Err(error);
        }
        let workspace = Self::workspace(id, &held);
        state.workspaces.insert(id.clone(), held);
        state.log.push(format!("create {id}"));
        Ok(workspace)
    }

    async fn delete_workspace(&self, id: &WorkspaceId) -> Result<()> {
        let mut state = self.state();
        if state.delete_fails {
            return Err(Error::unavailable("the engine is down"));
        }
        state.workspaces.remove(id);
        state.log.push(format!("delete {id}"));
        Ok(())
    }

    async fn start_workspace(&self, id: &WorkspaceId) -> Result<StartOutcome> {
        let mut state = self.state();
        let recreate = std::mem::take(&mut state.recreate);
        let Some(held) = state.workspaces.get_mut(id) else {
            return Err(Error::NotFound(format!("workspace {id}")));
        };
        if held.running {
            return Ok(StartOutcome::Running);
        }
        held.running = true;
        state.log.push(format!("start {id}"));
        Ok(if recreate {
            StartOutcome::Recreated {
                detail: "Installed packages are gone.".to_owned(),
            }
        } else {
            StartOutcome::Started {
                detail: String::new(),
            }
        })
    }

    async fn stop_workspace(&self, id: &WorkspaceId) -> Result<()> {
        let mut state = self.state();
        if let Some(held) = state.workspaces.get_mut(id) {
            held.running = false;
        }
        state.log.push(format!("stop {id}"));
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<WorkspaceList> {
        let state = self.state();
        if state.list_fails {
            return Err(Error::unavailable("the engine is down"));
        }
        Ok(WorkspaceList {
            workspaces: state
                .workspaces
                .iter()
                .map(|(id, held)| Self::workspace(id, held))
                .collect(),
            free_bytes: None,
        })
    }

    async fn exec(
        &self,
        _id: &WorkspaceId,
        _request: ExecRequest,
    ) -> Result<BoxStream<'static, Result<ExecEvent>>> {
        unused()
    }

    async fn read_file(&self, _id: &WorkspaceId, _path: &str, _max_bytes: u64) -> Result<ReadFile> {
        unused()
    }

    async fn write_file(&self, _id: &WorkspaceId, _path: &str, _content: Vec<u8>) -> Result<()> {
        unused()
    }

    async fn find(&self, _id: &WorkspaceId, _request: FindRequest) -> Result<FindResult> {
        unused()
    }

    async fn grep(&self, _id: &WorkspaceId, _request: GrepRequest) -> Result<GrepResult> {
        unused()
    }
}
