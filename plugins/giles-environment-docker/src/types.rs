//! The values the environment methods take and return. They mirror `giles_types::environment`, so
//! the adapter to the SDK's trait is field-for-field.

use std::time::Duration;

use jiff::Timestamp;

/// What an environment instance accepts when creating a workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvironmentInfo {
    /// The JSON Schema of the creation options. The model sees it.
    pub options_schema: serde_json::Value,
}

/// A workspace as the plugin sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The id the core chose.
    pub id: String,
    /// What the workspace is: the image and its digest.
    pub description: String,
    /// The directory commands start in and relative paths resolve against.
    pub working_directory: String,
    /// What survives a stop and a recreate, for the model.
    pub persistence: String,
    /// When the workspace was created.
    pub created_at: Timestamp,
    /// Whether it is ready for commands.
    pub state: WorkspaceState,
    /// Disk the workspace's volume uses, if the engine says.
    pub disk_used_bytes: Option<u64>,
}

/// Whether a workspace is ready for commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceState {
    /// Its container is running.
    Running,
    /// Its container is stopped.
    Stopped,
}

/// What `start_workspace` had to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// The container was running.
    Running,
    /// It was stopped and started again.
    Started {
        /// What the model should know.
        detail: String,
    },
    /// The container was gone and was recreated from its volume and recorded image.
    Recreated {
        /// What the model should know.
        detail: String,
    },
}

/// The workspaces an instance holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceList {
    /// Every workspace labeled for this instance.
    pub workspaces: Vec<Workspace>,
    /// Free space where the engine stores volumes, if it could be measured.
    pub free_bytes: Option<u64>,
}

/// A command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    /// Run with `/bin/sh -c` in `/workspace`.
    pub command: String,
    /// How long it may run before it is killed.
    pub timeout: Duration,
    /// The most stdout and stderr bytes, together, to send.
    pub output_limit: u64,
}

/// An event of a running command.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// The exit status; 128 + N for a command killed by signal N.
    pub code: i32,
    /// The plugin killed the command at its timeout.
    pub timed_out: bool,
    /// Output bytes past the limit, discarded.
    pub dropped_bytes: u64,
}

/// A file's content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadFile {
    /// The first bytes, at most `max_bytes`.
    pub content: Vec<u8>,
    /// The file's full size.
    pub size: u64,
}

/// A file search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindRequest {
    /// A glob matched against paths relative to `path`.
    pub pattern: String,
    /// The directory to search; `None` is the working directory.
    pub path: Option<String>,
    /// The most paths to return.
    pub limit: u32,
}

/// The files a search found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindResult {
    /// Matching files, relative to the searched directory, sorted.
    pub paths: Vec<String>,
    /// More files matched than the limit.
    pub truncated: bool,
}

/// A content search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepRequest {
    /// A regular expression in Rust regex syntax.
    pub pattern: String,
    /// The file or directory to search; `None` is the working directory.
    pub path: Option<String>,
    /// Only search files whose relative path matches this glob.
    pub glob: Option<String>,
    /// Match letters regardless of case.
    pub case_insensitive: bool,
    /// The most matching lines to return.
    pub limit: u32,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// The file, relative to the searched directory.
    pub path: String,
    /// The line number, from 1.
    pub line: u64,
    /// The line without its ending.
    pub text: String,
}

/// The lines a search matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepResult {
    /// Matching lines in path order, then line order.
    pub matches: Vec<GrepMatch>,
    /// More lines matched than the limit.
    pub truncated: bool,
}
