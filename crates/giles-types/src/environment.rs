//! The [`Environment`] trait: isolated workspaces for an agent's tools (PLAN section 8.8).
//!
//! An environment provides primitives (run a command, read and write a file, find, grep) in
//! workspaces the core creates. The core builds the model's tools (`bash`, `read`, `write`,
//! `edit`, `find`, `grep`) on top of them, so those tools look the same whatever runs underneath.
//!
//! Every method names the workspace by [`WorkspaceId`], which the core chooses. Errors follow the
//! other component traits: [`Error::InvalidRequest`](crate::Error::InvalidRequest) for a refused
//! request, [`Error::NotFound`](crate::Error::NotFound) for a missing workspace or path, and
//! [`Error::Unavailable`](crate::Error::Unavailable) when the backend (a container engine, say)
//! can't be reached. Their messages may be shown to the model.

use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::Result;
use crate::Timestamp;
pub use crate::id::WorkspaceId;

/// A provider of isolated workspaces: a container engine, a VM, a remote machine.
///
/// Built from an `[environment.<name>]` instance; the core reaches a plugin's through the
/// `RemoteEnvironment` adapter. The same trait is what the plugin SDK serves.
#[async_trait]
pub trait Environment: Send + Sync {
    /// What this instance accepts when creating a workspace.
    async fn info(&self) -> Result<EnvironmentInfo>;

    /// Creates a workspace under `id`. `options` is a JSON object matching
    /// [`EnvironmentInfo::options_schema`] (`{}` for none). An id already in use, or options the
    /// environment refuses, are [`Error::InvalidRequest`](crate::Error::InvalidRequest).
    async fn create_workspace(
        &self,
        id: &WorkspaceId,
        options: serde_json::Value,
    ) -> Result<Workspace>;

    /// Deletes a workspace and everything in it. Succeeds if there is no such workspace.
    async fn delete_workspace(&self, id: &WorkspaceId) -> Result<()>;

    /// Makes a workspace ready for [`exec`](Environment::exec) and the file calls, starting it or
    /// recreating what it runs on, and says what that took. The core calls it before each
    /// workspace tool call, so it must be cheap for a workspace that is running.
    async fn start_workspace(&self, id: &WorkspaceId) -> Result<StartOutcome>;

    /// Stops a workspace's processes and keeps its files. Succeeds if it is already stopped.
    async fn stop_workspace(&self, id: &WorkspaceId) -> Result<()>;

    /// Every workspace this instance holds, including ones the core doesn't know about, with the
    /// free space where they live.
    async fn list_workspaces(&self) -> Result<WorkspaceList>;

    /// Runs a command in a started workspace. The stream yields [`ExecEvent::Stdout`] and
    /// [`ExecEvent::Stderr`] pieces as the command writes them, then one [`ExecEvent::Exit`], then
    /// ends.
    ///
    /// Dropping the stream cancels the command: the implementation must kill it and everything it
    /// started.
    async fn exec(
        &self,
        id: &WorkspaceId,
        request: ExecRequest,
    ) -> Result<BoxStream<'static, Result<ExecEvent>>>;

    /// Reads up to `max_bytes` from the start of the file at `path`, which is absolute or relative
    /// to the workspace's working directory.
    async fn read_file(&self, id: &WorkspaceId, path: &str, max_bytes: u64) -> Result<ReadFile>;

    /// Writes `content` to the file at `path` (absolute, or relative to the working directory),
    /// creating missing parent directories. An existing file keeps its permissions.
    async fn write_file(&self, id: &WorkspaceId, path: &str, content: Vec<u8>) -> Result<()>;

    /// Lists the files whose path matches a glob.
    async fn find(&self, id: &WorkspaceId, request: FindRequest) -> Result<FindResult>;

    /// Searches file contents with a regular expression.
    async fn grep(&self, id: &WorkspaceId, request: GrepRequest) -> Result<GrepResult>;
}

/// What an environment instance is, declared once.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvironmentInfo {
    /// The JSON Schema of the options [`Environment::create_workspace`] takes. The model sees it,
    /// so descriptions in it should say what each option does and which values are allowed.
    pub options_schema: serde_json::Value,
}

/// A workspace as the environment sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The id the core chose.
    pub id: WorkspaceId,
    /// What the workspace is, for the model and the operator, e.g. the image and its digest.
    pub description: String,
    /// The absolute directory commands start in and relative paths resolve against.
    pub working_directory: String,
    /// What survives a stop and a recreate, for the model.
    pub persistence: String,
    /// When the workspace was created.
    pub created_at: Timestamp,
    /// Whether it is ready for commands.
    pub state: WorkspaceState,
    /// Disk the workspace uses, in bytes, if the environment can tell.
    pub disk_used_bytes: Option<u64>,
    /// What else the operator may want to know, in the order to show it. The core shows these as
    /// they are and never interprets them.
    pub details: Vec<WorkspaceDetail>,
}

/// One display-ready fact about a workspace, shown to the operator as "label: value". The model
/// doesn't see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDetail {
    /// A short name, e.g. `Image`.
    pub label: String,
    /// The text to show, e.g. `debian:stable`.
    pub value: String,
}

impl WorkspaceDetail {
    /// A detail with the given label and value.
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Whether a workspace is ready for commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WorkspaceState {
    /// Ready for [`Environment::exec`] and the file calls.
    Running,
    /// Stopped; [`Environment::start_workspace`] makes it ready again.
    Stopped,
}

/// What [`Environment::start_workspace`] had to do.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartOutcome {
    /// The workspace was running.
    Running,
    /// It was stopped and started again. Background processes from before have ended.
    Started {
        /// What the model should know about it, if anything.
        detail: String,
    },
    /// What it ran on was gone and was recreated. Files under the working directory survived;
    /// `detail` says what didn't, e.g. which packages are gone.
    Recreated {
        /// What the model should know about it.
        detail: String,
    },
}

/// The workspaces an environment holds, from [`Environment::list_workspaces`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceList {
    /// Every workspace.
    pub workspaces: Vec<Workspace>,
    /// Free space where workspaces are stored, in bytes, if the environment can tell.
    pub free_bytes: Option<u64>,
}

/// A command for [`Environment::exec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// The command line, run with `/bin/sh -c` in the working directory.
    pub command: String,
    /// How long the command may run before the environment kills it and reports
    /// [`ExecExit::timed_out`].
    pub timeout: Duration,
    /// The most stdout and stderr bytes, together, the environment sends. The rest is counted in
    /// [`ExecExit::dropped_bytes`] and discarded.
    pub output_limit: u64,
}

/// An event of a running command, from [`Environment::exec`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecEvent {
    /// A piece of standard output.
    Stdout(Vec<u8>),
    /// A piece of standard error.
    Stderr(Vec<u8>),
    /// The command ended. Always the last event.
    Exit(ExecExit),
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecExit {
    /// The exit status. A command killed by signal N exits with 128 + N.
    pub code: i32,
    /// The environment killed the command at [`ExecRequest::timeout`].
    pub timed_out: bool,
    /// Output bytes past [`ExecRequest::output_limit`], which were discarded.
    pub dropped_bytes: u64,
}

/// A file's content, from [`Environment::read_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFile {
    /// The file's first bytes, at most the `max_bytes` asked for.
    pub content: Vec<u8>,
    /// The file's full size in bytes.
    pub size: u64,
}

/// A file search for [`Environment::find`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindRequest {
    /// A glob matched against paths relative to `path`, e.g. `**/*.rs`. `**` crosses directories.
    pub pattern: String,
    /// The directory to search. `None`: the working directory.
    pub path: Option<String>,
    /// The most paths to return.
    pub limit: u32,
}

/// The files [`Environment::find`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindResult {
    /// Matching files, relative to the searched directory, sorted. `.git` directories and files
    /// that `.gitignore` files exclude are skipped.
    pub paths: Vec<String>,
    /// More files matched than [`FindRequest::limit`].
    pub truncated: bool,
}

/// A content search for [`Environment::grep`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepRequest {
    /// A regular expression in Rust regex syntax.
    pub pattern: String,
    /// The file or directory to search. `None`: the working directory.
    pub path: Option<String>,
    /// Only search files whose relative path matches this glob, if set.
    pub glob: Option<String>,
    /// Match letters regardless of case.
    pub case_insensitive: bool,
    /// The most matching lines to return.
    pub limit: u32,
}

/// One line [`Environment::grep`] matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// The file, relative to the searched directory.
    pub path: String,
    /// The line number, counting from 1.
    pub line: u64,
    /// The line, without its line ending.
    pub text: String,
}

/// The lines [`Environment::grep`] matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepResult {
    /// Matching lines in path order, then line order. Binary files, `.git` directories, and files
    /// that `.gitignore` files exclude are skipped.
    pub matches: Vec<GrepMatch>,
    /// More lines matched than [`GrepRequest::limit`].
    pub truncated: bool,
}
