//! An in-process [`Environment`] and a manual clock for tests of the workspace service and of
//! the code that calls it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use futures::stream::{self, BoxStream};
use giles_types::{
    Environment, EnvironmentInfo, Error, ExecEvent, ExecRequest, FindRequest, FindResult,
    GrepMatch, GrepRequest, GrepResult, ReadFile, Result, StartOutcome, Timestamp, Workspace,
    WorkspaceId, WorkspaceList, WorkspaceState as PluginState,
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
    /// What `exec` does; `Exec::Refuse` until a test sets it.
    exec: Exec,
    /// Every `exec` request, in order.
    execs: Vec<ExecRequest>,
    /// Files by workspace and path as the caller wrote it (the fake doesn't resolve paths).
    files: BTreeMap<(WorkspaceId, String), Vec<u8>>,
    /// The `max_bytes` of every `read_file`.
    reads: Vec<u64>,
}

/// What a fake `exec` does.
#[derive(Clone, Default)]
pub(crate) enum Exec {
    /// Fails the call as not supported.
    #[default]
    Refuse,
    /// Yields these events.
    Events(Vec<ExecEvent>),
    /// Yields one piece of stdout, `started`, and then never ends. Dropping the stream sets the
    /// flag.
    Hang(Arc<AtomicBool>),
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

    /// Makes every `exec` yield `events`.
    pub(crate) fn script_exec(&self, events: Vec<ExecEvent>) {
        self.state().exec = Exec::Events(events);
    }

    /// Makes every `exec` hang after one piece of output. The returned flag is set when the
    /// command's stream is dropped, as it is when the core cancels the command.
    pub(crate) fn hang_exec(&self) -> Arc<AtomicBool> {
        let dropped = Arc::new(AtomicBool::new(false));
        self.state().exec = Exec::Hang(dropped.clone());
        dropped
    }

    /// Every `exec` request so far.
    pub(crate) fn execs(&self) -> Vec<ExecRequest> {
        self.state().execs.clone()
    }

    /// Puts a file in a workspace.
    pub(crate) fn put_file(&self, id: &WorkspaceId, path: &str, content: &[u8]) {
        self.state()
            .files
            .insert((id.clone(), path.to_owned()), content.to_vec());
    }

    /// A file of a workspace.
    pub(crate) fn file(&self, id: &WorkspaceId, path: &str) -> Option<Vec<u8>> {
        self.state()
            .files
            .get(&(id.clone(), path.to_owned()))
            .cloned()
    }

    /// The `max_bytes` of every `read_file` so far.
    pub(crate) fn reads(&self) -> Vec<u64> {
        self.state().reads.clone()
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
        id: &WorkspaceId,
        request: ExecRequest,
    ) -> Result<BoxStream<'static, Result<ExecEvent>>> {
        let mut state = self.state();
        if !state.workspaces.contains_key(id) {
            return Err(Error::NotFound(format!("workspace {id}")));
        }
        state.execs.push(request);
        match state.exec.clone() {
            Exec::Refuse => unused(),
            Exec::Events(events) => Ok(stream::iter(events.into_iter().map(Ok)).boxed()),
            Exec::Hang(dropped) => {
                let guard = DropFlag(dropped);
                Ok(stream::iter([Ok(ExecEvent::Stdout(b"started\n".to_vec()))])
                    .chain(stream::pending().inspect(move |_: &Result<ExecEvent>| {
                        let _ = &guard;
                    }))
                    .boxed())
            }
        }
    }

    async fn read_file(&self, id: &WorkspaceId, path: &str, max_bytes: u64) -> Result<ReadFile> {
        let mut state = self.state();
        state.reads.push(max_bytes);
        let content = state
            .files
            .get(&(id.clone(), path.to_owned()))
            .ok_or_else(|| Error::NotFound(format!("no such file: {path}")))?;
        let kept = usize::try_from(max_bytes).unwrap_or(usize::MAX);
        Ok(ReadFile {
            content: content[..content.len().min(kept)].to_vec(),
            size: content.len() as u64,
        })
    }

    async fn write_file(&self, id: &WorkspaceId, path: &str, content: Vec<u8>) -> Result<()> {
        let mut state = self.state();
        if !state.workspaces.contains_key(id) {
            return Err(Error::NotFound(format!("workspace {id}")));
        }
        state.files.insert((id.clone(), path.to_owned()), content);
        Ok(())
    }

    async fn find(&self, id: &WorkspaceId, request: FindRequest) -> Result<FindResult> {
        let state = self.state();
        let needle = request.pattern.trim_matches('*');
        let mut paths: Vec<String> = state
            .files
            .keys()
            .filter(|(owner, path)| owner == id && path.contains(needle))
            .map(|(_, path)| path.clone())
            .collect();
        let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
        let truncated = paths.len() > limit;
        paths.truncate(limit);
        Ok(FindResult { paths, truncated })
    }

    async fn grep(&self, id: &WorkspaceId, request: GrepRequest) -> Result<GrepResult> {
        let state = self.state();
        let mut matches = Vec::new();
        for ((owner, path), content) in &state.files {
            if owner != id {
                continue;
            }
            for (n, line) in String::from_utf8_lossy(content).lines().enumerate() {
                if line.contains(&request.pattern) {
                    matches.push(GrepMatch {
                        path: path.clone(),
                        line: n as u64 + 1,
                        text: line.to_owned(),
                    });
                }
            }
        }
        let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
        let truncated = matches.len() > limit;
        matches.truncate(limit);
        Ok(GrepResult { matches, truncated })
    }
}

/// Sets its flag when dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
