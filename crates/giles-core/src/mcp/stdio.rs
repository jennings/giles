//! Stdio MCP servers: spawning, stderr capture with secrets scrubbed, and killing the child.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, watch};
use tracing::Instrument;

use crate::config::{MIN_SCRUB_LEN, REDACTED, Secret, scrub};

/// The `tracing` target of a stdio server's stderr lines, so an operator can filter them
/// (`GILES_LOG=giles_core::mcp::stderr=warn`).
pub const STDERR_TARGET: &str = "giles_core::mcp::stderr";

/// Longest stderr line logged; the rest of a longer line is cut off.
const MAX_STDERR_LINE: usize = 4096;

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

async fn log_stderr(mut stderr: ChildStderr, secrets: Arc<[Secret]>) {
    let secrets: Vec<&Secret> = secrets.iter().collect();
    let mut lines = LineSplitter::default();
    let mut buffer = [0; 4096];
    let mut ready = Vec::new();
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => lines.push(&buffer[..n], &mut ready),
            Err(error) => {
                tracing::debug!(%error, "reading the MCP server's stderr failed");
                break;
            }
        }
        for line in ready.drain(..) {
            let line = scrub_line(line, &secrets);
            tracing::info!(target: STDERR_TARGET, "{line}");
        }
    }
    if let Some(line) = lines.finish() {
        let line = scrub_line(line, &secrets);
        tracing::info!(target: STDERR_TARGET, "{line}");
    }
}

/// A line as logged: `secrets` replaced by `<redacted>`, and a cut line marked with `…`. A cut
/// can split a secret, so a cut line that ends in the start of a secret (at least
/// [`MIN_SCRUB_LEN`] bytes of it, like `scrub`) has that end redacted too.
fn scrub_line(line: Line, secrets: &[&Secret]) -> String {
    let mut text = scrub(&line.text, secrets);
    if line.truncated {
        let partial = secrets
            .iter()
            .map(|secret| secret.expose())
            .filter(|value| value.len() >= MIN_SCRUB_LEN)
            .flat_map(|value| {
                (MIN_SCRUB_LEN..value.len())
                    .filter(|&end| value.is_char_boundary(end))
                    .map(move |end| &value[..end])
            })
            .filter(|prefix| text.ends_with(prefix))
            .map(str::len)
            .max();
        if let Some(len) = partial {
            text.truncate(text.len() - len);
            text.push_str(REDACTED);
        }
        text.push('…');
    }
    text
}

/// One stderr line, before scrubbing.
#[derive(Debug, PartialEq, Eq)]
struct Line {
    text: String,
    /// The line was longer than [`MAX_STDERR_LINE`] and was cut.
    truncated: bool,
}

/// Splits a byte stream into lines: `\n`-terminated, a trailing `\r` dropped, invalid UTF-8
/// replaced, and each line cut to [`MAX_STDERR_LINE`] bytes so a server can't make the core buffer
/// without bound.
#[derive(Default)]
struct LineSplitter {
    line: Vec<u8>,
    /// The current line was cut; skip the rest of it.
    truncated: bool,
}

impl LineSplitter {
    fn push(&mut self, mut bytes: &[u8], out: &mut Vec<Line>) {
        while !bytes.is_empty() {
            let (chunk, rest, complete) = match bytes.iter().position(|&b| b == b'\n') {
                Some(end) => (&bytes[..end], &bytes[end + 1..], true),
                None => (bytes, &[][..], false),
            };
            bytes = rest;
            if !self.truncated {
                let room = MAX_STDERR_LINE - self.line.len();
                if chunk.len() > room {
                    self.line.extend_from_slice(&chunk[..room]);
                    self.truncated = true;
                } else {
                    self.line.extend_from_slice(chunk);
                }
            }
            if complete {
                out.push(self.take());
            }
        }
    }

    /// The last line, if the stream ended without a newline.
    fn finish(mut self) -> Option<Line> {
        (!self.line.is_empty() || self.truncated).then(|| self.take())
    }

    fn take(&mut self) -> Line {
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        let line = Line {
            text: String::from_utf8_lossy(&self.line).into_owned(),
            truncated: self.truncated,
        };
        self.line.clear();
        self.truncated = false;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> (Vec<String>, Option<String>) {
        let mut splitter = LineSplitter::default();
        let mut lines = Vec::new();
        for chunk in chunks {
            splitter.push(chunk, &mut lines);
        }
        let shown = |line| scrub_line(line, &[]);
        (
            lines.into_iter().map(shown).collect(),
            splitter.finish().map(shown),
        )
    }

    #[test]
    fn splits_lines_across_chunks() {
        let (lines, rest) = split(&[b"one\ntw", b"o\r\n\nthr", b"ee"]);
        assert_eq!(lines, ["one", "two", ""]);
        assert_eq!(rest.as_deref(), Some("three"));
    }

    #[test]
    fn nothing_pending_after_a_final_newline() {
        assert_eq!(split(&[b"a\n"]), (vec!["a".to_owned()], None));
    }

    #[test]
    fn long_lines_are_cut_and_the_rest_skipped() {
        let long = vec![b'x'; MAX_STDERR_LINE + 10];
        let (lines, rest) = split(&[&long, b"yyy\nnext\n"]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], format!("{}…", "x".repeat(MAX_STDERR_LINE)));
        assert_eq!(lines[1], "next");
        assert_eq!(rest, None);
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let (lines, _) = split(&[b"caf\xc3\n\xff\n"]);
        assert_eq!(lines, ["caf\u{fffd}", "\u{fffd}"]);
    }

    #[test]
    fn secrets_are_scrubbed_including_one_split_by_the_cut() {
        let token = Secret::new("tok-0123456789");
        let secrets = [&token];
        let line = |text: &str, truncated| Line {
            text: text.to_owned(),
            truncated,
        };
        assert_eq!(
            scrub_line(line("auth tok-0123456789 ok", false), &secrets),
            "auth <redacted> ok"
        );
        // The cut left the first 8 bytes of the token at the end of the line.
        assert_eq!(
            scrub_line(line("auth tok-0123", true), &secrets),
            "auth <redacted>…"
        );
        // Fewer than MIN_SCRUB_LEN bytes are too little to redact.
        assert_eq!(scrub_line(line("auth to", true), &secrets), "auth to…");
        // An uncut line that merely ends like the token is left alone.
        assert_eq!(
            scrub_line(line("auth tok-0", false), &secrets),
            "auth tok-0"
        );
    }

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
