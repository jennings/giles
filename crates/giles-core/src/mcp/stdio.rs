//! Stdio MCP servers: spawning, stderr capture with secrets scrubbed, and killing the child.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, watch};
use tracing::Instrument;

use crate::config::Secret;
use crate::stderr_lines;

/// The `tracing` target of a stdio server's stderr lines, so an operator can filter them
/// (`GILES_LOG=giles_core::mcp::stderr=warn`).
pub const STDERR_TARGET: &str = "giles_core::mcp::stderr";

/// A stdio MCP server run directly, unsandboxed.
///
/// [`DirectCommand::command`] builds the [`Command`] that
/// [`McpToolset::connect_stdio`](super::McpToolset::connect_stdio) runs, and
/// [`DirectCommand::secrets`] the values to scrub from its output. Task 2.19 builds its own
/// `Command` instead, wrapping the server in the sandbox trampoline (`giles sandbox-exec`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectCommand {
    /// The server executable, an absolute path (PLAN section 7.10).
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<String>,
    /// Its environment. Nothing else is inherited except `PATH`. The values are treated as
    /// secrets: they are scrubbed from the server's stderr and from error messages.
    pub env: BTreeMap<String, Secret>,
    /// Its working directory; the daemon's if `None`.
    pub cwd: Option<PathBuf>,
}

impl DirectCommand {
    /// `program` with no arguments and no extra environment.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    /// The command to spawn: a cleared environment with the daemon's `PATH` and then
    /// [`env`](Self::env), so the daemon's own variables don't leak to the server.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args).env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        for (name, value) in &self.env {
            command.env(name, value.expose());
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command
    }

    /// The [`env`](Self::env) values, for
    /// [`McpToolset::connect_stdio`](super::McpToolset::connect_stdio) to scrub.
    pub fn secrets(&self) -> Vec<Secret> {
        self.env.values().cloned().collect()
    }
}

/// A spawned server: its stdio pipes, the guard that kills it, and its exit status once known.
pub(crate) struct Spawned {
    pub(crate) stdout: ChildStdout,
    pub(crate) stdin: ChildStdin,
    pub(crate) guard: ChildGuard,
    pub(crate) exit: watch::Receiver<Option<ExitStatus>>,
}

/// Kills the server when dropped.
///
/// A supervisor task owns the child: it logs and publishes the exit status if the server stops by
/// itself, and kills and reaps it when this guard is dropped. If the runtime is gone by then,
/// `kill_on_drop` still kills the child.
pub(crate) struct ChildGuard {
    _stop: oneshot::Sender<()>,
}

/// Spawns `command` with piped stdio. Stdin and stdout carry the MCP messages; stderr lines are
/// logged under `span` at `info` with target [`STDERR_TARGET`], with `secrets` scrubbed.
pub(crate) fn spawn(
    mut command: Command,
    span: &tracing::Span,
    secrets: Arc<[Secret]>,
) -> io::Result<Spawned> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(io::Error::other("the child's stdio wasn't piped"));
    };
    tracing::debug!(parent: span, pid = child.id(), "started MCP server");
    tokio::spawn(log_stderr(stderr, secrets).instrument(span.clone()));
    let (stop, stopped) = oneshot::channel();
    let (exited, exit) = watch::channel(None);
    tokio::spawn(supervise(child, stopped, exited).instrument(span.clone()));
    Ok(Spawned {
        stdout,
        stdin,
        guard: ChildGuard { _stop: stop },
        exit,
    })
}

async fn supervise(
    mut child: Child,
    stop: oneshot::Receiver<()>,
    exited: watch::Sender<Option<ExitStatus>>,
) {
    tokio::select! {
        status = child.wait() => match status {
            Ok(status) => {
                if status.success() {
                    tracing::info!("MCP server exited");
                } else {
                    tracing::warn!(%status, "MCP server exited");
                }
                exited.send_replace(Some(status));
            }
            Err(error) => tracing::warn!(%error, "waiting for the MCP server failed"),
        },
        // The guard was dropped (the sender never sends).
        _ = stop => match child.kill().await {
            Ok(()) => tracing::debug!("stopped MCP server"),
            Err(error) => tracing::warn!(%error, "killing the MCP server failed"),
        },
    }
}

async fn log_stderr(stderr: ChildStderr, secrets: Arc<[Secret]>) {
    let secrets: Vec<&Secret> = secrets.iter().collect();
    stderr_lines::forward(stderr, &secrets, |line| {
        tracing::info!(target: STDERR_TARGET, "{line}");
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_passes_only_path_and_the_configured_env() {
        let mut server = DirectCommand::new("/usr/bin/env");
        server
            .env
            .insert("GILES_TEST".to_owned(), Secret::new("value-1"));
        assert_eq!(server.secrets(), [Secret::new("value-1")]);
        let output = server.command().output().await.unwrap();
        assert!(output.status.success());
        let mut vars: Vec<String> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with("PATH="))
            .map(str::to_owned)
            .collect();
        vars.sort();
        assert_eq!(vars, ["GILES_TEST=value-1"]);
    }
}
