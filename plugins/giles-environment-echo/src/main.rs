//! `giles-environment-echo`: an environment plugin for testing the plugin host, never for real use.
//!
//! Workspaces live in memory, so a restarted plugin starts with none. Files are kept per
//! workspace, and a command is one of a few scripts rather than a shell line:
//!
//! - `echo <text>`: writes `<text>` and a newline to stdout and exits with status 0.
//! - `fail`: writes `failed` and a newline to stderr and exits with status 3.
//! - `hang`: writes `started` and a newline to stdout, then never ends. The plugin writes the file
//!   `exec-started` in its data directory when the command starts, and `exec-cancelled` when the
//!   core cancels the call.
//!
//! Any other command fails the call as invalid.
//!
//! Config (all optional): `label` (default `"echo"`), put in the description of the options
//! schema `Info` returns. Unknown keys are refused.
//!
//! Run as a remote plugin (PLAN section 7.6) with the SDK's `serve` subcommand, e.g.
//! `giles-environment-echo serve --listen 127.0.0.1:0 --data <dir> --no-auth`. It prints
//! `listening on <address>` to stdout once it listens. Giles launches it with `socket`.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;

use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Environment, EnvironmentInfo, Error, ExecEvent, ExecExit, ExecRequest, FindRequest, FindResult,
    GrepMatch, GrepRequest, GrepResult, ReadFile, Result, StartOutcome, Timestamp, Workspace,
    WorkspaceDetail, WorkspaceId, WorkspaceList, WorkspaceState,
};
use giles_plugin_sdk::{Configure, EnvironmentPlugin, async_trait};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

/// The file written in the data directory when a `hang` command is cancelled.
const CANCELLED_FILE: &str = "exec-cancelled";

/// The file written in the data directory when a `hang` command starts.
const STARTED_FILE: &str = "exec-started";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_env("GILES_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    EnvironmentPlugin::new("echo", env!("CARGO_PKG_VERSION"))
        .run(Echo::configure)
        .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_label")]
    label: String,
}

fn default_label() -> String {
    "echo".to_owned()
}

/// One workspace and its files.
struct Held {
    workspace: Workspace,
    files: BTreeMap<String, Vec<u8>>,
}

struct Echo {
    label: String,
    data_dir: PathBuf,
    workspaces: Mutex<HashMap<String, Held>>,
}

/// Writes [`CANCELLED_FILE`] when dropped: the stream of a `hang` command is dropped when the
/// core cancels the call.
struct CancelGuard(PathBuf);

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "cancelled");
    }
}

impl Echo {
    async fn configure(ctx: Configure) -> Result<Self> {
        let config: Config = ctx.parse_config()?;
        Ok(Self {
            label: config.label,
            data_dir: ctx.data_dir().to_path_buf(),
            workspaces: Mutex::default(),
        })
    }

    fn with<T>(&self, id: &WorkspaceId, f: impl FnOnce(&mut Held) -> T) -> Result<T> {
        let mut workspaces = self.workspaces.lock().unwrap();
        let held = workspaces
            .get_mut(id.as_str())
            .ok_or_else(|| Error::NotFound(format!("no workspace {id}")))?;
        Ok(f(held))
    }
}

#[async_trait]
impl Environment for Echo {
    async fn info(&self) -> Result<EnvironmentInfo> {
        Ok(EnvironmentInfo {
            options_schema: serde_json::json!({
                "type": "object",
                "description": format!("Options of the {} environment.", self.label),
                "properties": { "note": { "type": "string" } },
            }),
        })
    }

    async fn create_workspace(
        &self,
        id: &WorkspaceId,
        options: serde_json::Value,
    ) -> Result<Workspace> {
        let mut workspaces = self.workspaces.lock().unwrap();
        if workspaces.contains_key(id.as_str()) {
            return Err(Error::InvalidRequest(format!("workspace {id} exists")));
        }
        let workspace = Workspace {
            id: id.clone(),
            description: format!("an echo workspace, options {options}"),
            working_directory: "/work".to_owned(),
            persistence: "nothing survives a restart of the plugin".to_owned(),
            created_at: Timestamp::now(),
            state: WorkspaceState::Running,
            disk_used_bytes: None,
            details: vec![
                WorkspaceDetail::new("Kind", "echo"),
                WorkspaceDetail::new("Options", options.to_string()),
            ],
        };
        workspaces.insert(
            id.as_str().to_owned(),
            Held {
                workspace: workspace.clone(),
                files: BTreeMap::new(),
            },
        );
        Ok(workspace)
    }

    async fn delete_workspace(&self, id: &WorkspaceId) -> Result<()> {
        self.workspaces.lock().unwrap().remove(id.as_str());
        Ok(())
    }

    async fn start_workspace(&self, id: &WorkspaceId) -> Result<StartOutcome> {
        self.with(id, |held| {
            if held.workspace.state == WorkspaceState::Stopped {
                held.workspace.state = WorkspaceState::Running;
                StartOutcome::Started {
                    detail: "started again".to_owned(),
                }
            } else {
                StartOutcome::Running
            }
        })
    }

    async fn stop_workspace(&self, id: &WorkspaceId) -> Result<()> {
        // Succeeds for a workspace that doesn't exist, as the trait says.
        let _ = self.with(id, |held| held.workspace.state = WorkspaceState::Stopped);
        Ok(())
    }

    async fn list_workspaces(&self) -> Result<WorkspaceList> {
        let mut workspaces: Vec<Workspace> = self
            .workspaces
            .lock()
            .unwrap()
            .values()
            .map(|held| held.workspace.clone())
            .collect();
        workspaces.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(WorkspaceList {
            workspaces,
            free_bytes: Some(1 << 30),
        })
    }

    async fn exec(
        &self,
        id: &WorkspaceId,
        request: ExecRequest,
    ) -> Result<BoxStream<'static, Result<ExecEvent>>> {
        self.with(id, |_| ())?;
        let exit = |code| ExecExit {
            code,
            timed_out: false,
            dropped_bytes: 0,
        };
        let command = request.command.as_str();
        if let Some(text) = command.strip_prefix("echo ") {
            Ok(stream::iter([
                Ok(ExecEvent::Stdout(format!("{text}\n").into_bytes())),
                Ok(ExecEvent::Exit(exit(0))),
            ])
            .boxed())
        } else if command == "fail" {
            Ok(stream::iter([
                Ok(ExecEvent::Stderr(b"failed\n".to_vec())),
                Ok(ExecEvent::Exit(exit(3))),
            ])
            .boxed())
        } else if command == "hang" {
            let guard = CancelGuard(self.data_dir.join(CANCELLED_FILE));
            let _ = std::fs::write(self.data_dir.join(STARTED_FILE), "started");
            Ok(stream::iter([Ok(ExecEvent::Stdout(b"started\n".to_vec()))])
                .chain(stream::pending::<Result<ExecEvent>>().inspect(move |_| {
                    let _ = &guard;
                }))
                .boxed())
        } else {
            Err(Error::InvalidRequest(format!(
                "the echo environment can't run {command:?}"
            )))
        }
    }

    async fn read_file(&self, id: &WorkspaceId, path: &str, max_bytes: u64) -> Result<ReadFile> {
        self.with(id, |held| {
            let content = held
                .files
                .get(path)
                .ok_or_else(|| Error::NotFound(format!("no file {path}")))?;
            let kept = usize::try_from(max_bytes).unwrap_or(usize::MAX);
            Ok(ReadFile {
                content: content[..content.len().min(kept)].to_vec(),
                size: content.len() as u64,
            })
        })?
    }

    async fn write_file(&self, id: &WorkspaceId, path: &str, content: Vec<u8>) -> Result<()> {
        self.with(id, |held| held.files.insert(path.to_owned(), content))?;
        Ok(())
    }

    async fn find(&self, id: &WorkspaceId, request: FindRequest) -> Result<FindResult> {
        self.with(id, |held| {
            let needle = request.pattern.trim_matches('*');
            let mut paths: Vec<String> = held
                .files
                .keys()
                .filter(|path| path.contains(needle))
                .cloned()
                .collect();
            let limit = usize::try_from(request.limit).unwrap_or(usize::MAX);
            let truncated = paths.len() > limit;
            paths.truncate(limit);
            FindResult { paths, truncated }
        })
    }

    async fn grep(&self, id: &WorkspaceId, request: GrepRequest) -> Result<GrepResult> {
        self.with(id, |held| {
            let mut matches = Vec::new();
            for (path, content) in &held.files {
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
            GrepResult { matches, truncated }
        })
    }
}
