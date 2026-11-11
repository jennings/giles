//! The `bash` tool: runs one command line in the workspace.
//!
//! Each call is a fresh `/bin/sh -c` in the workspace's working directory, so `cd` and `export`
//! don't carry over to the next call. The plugin streams the output and reports how the command
//! ended; the core keeps the head and the tail of the combined output within a fixed cap
//! ([`HEAD_BYTES`] and [`TAIL_BYTES`]) and says how much it left out, the same for every plugin.
//!
//! The command's own timeout is the call's `timeout` (at most [`MAX_TIMEOUT`]), enforced by the
//! plugin. The dispatcher's deadline for the call ([`DEADLINE`]) is longer than that, so it never
//! cuts a command off before its own timeout. Dropping the call (the turn was cancelled, `/stop`)
//! drops the exec stream, which makes the plugin kill the command and everything it started.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt as _;
use giles_types::{ExecEvent, ExecExit, ExecRequest, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use super::{Env, parse, plugin_error};
use crate::agent_loop::ToolOutput;
use crate::core_tools::{CoreCall, CoreTool};

/// The tool's name.
pub const BASH: &str = "bash";

/// How long a command may run when the call sets no `timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

/// The longest `timeout` a call may set.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);

/// How long the dispatcher lets a `bash` call run: [`MAX_TIMEOUT`] and a minute for the plugin to
/// kill the command and for the call to finish.
pub const DEADLINE: Duration = Duration::from_secs(MAX_TIMEOUT.as_secs() + 60);

/// The most output bytes, stdout and stderr together, that the core asks the plugin to send.
pub const EXEC_OUTPUT_LIMIT: u64 = 1 << 20;

/// How much of the start of the output a result keeps. A result keeps at most this and
/// [`TAIL_BYTES`] of a command's output.
pub const HEAD_BYTES: usize = 10 * 1024;

/// How much of the end of the output a result keeps. The end is where errors usually are.
pub const TAIL_BYTES: usize = 20 * 1024;

/// The `bash` tool.
#[derive(Debug, Clone)]
pub(in crate::core_tools) struct BashTool(pub(in crate::core_tools) Env);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    command: String,
    timeout: Option<u64>,
}

#[async_trait]
impl CoreTool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: BASH.to_owned(),
            description: format!(
                "Run a shell command in this conversation's workspace. Each call is a fresh \
                 `/bin/sh -c` in the working directory: `cd`, `export`, and shell variables \
                 don't carry over to the next call, and processes you leave running end when the \
                 call does or when the workspace stops. Files do persist.\n\n\
                 The result gives the exit code, whether the command timed out, and its stdout \
                 and stderr. Long output keeps only its first {} KiB and last {} KiB, and says \
                 how much was left out: redirect to a file and use read, grep, or head and tail \
                 to see the rest. The call fails if there is no workspace; see \
                 environment_create.",
                HEAD_BYTES / 1024,
                TAIL_BYTES / 1024,
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The command line, run with `/bin/sh -c`.",
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_TIMEOUT.as_secs(),
                        "default": DEFAULT_TIMEOUT.as_secs(),
                        "description": "Seconds the command may run before it is killed.",
                    },
                },
                "required": ["command"],
                "additionalProperties": false,
            }),
        }
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let input: Input = match parse(call.input.clone()) {
            Ok(input) => input,
            Err(output) => return output,
        };
        if input.command.trim().is_empty() {
            return ToolOutput::error("invalid input: command is empty");
        }
        let timeout = match input.timeout {
            None => DEFAULT_TIMEOUT,
            Some(seconds) if (1..=MAX_TIMEOUT.as_secs()).contains(&seconds) => {
                Duration::from_secs(seconds)
            }
            Some(seconds) => {
                return ToolOutput::error(format!(
                    "invalid input: timeout is {seconds} seconds; it must be between 1 and {}",
                    MAX_TIMEOUT.as_secs()
                ));
            }
        };
        let open = match self.0.open(&call).await {
            Ok(open) => open,
            Err(output) => return output,
        };
        let request = ExecRequest {
            command: input.command,
            timeout,
            output_limit: EXEC_OUTPUT_LIMIT,
        };
        let mut events = match open.client.exec(&open.record.id, request).await {
            Ok(events) => events,
            Err(error) => return open.plugin_error(&error),
        };
        // `events` is dropped when this future is: that is what cancels the command.
        let mut capture = Capture::new(HEAD_BYTES, TAIL_BYTES);
        let mut exit = None;
        while let Some(event) = events.next().await {
            match event {
                Ok(ExecEvent::Stdout(bytes)) => capture.push(Stream::Stdout, bytes),
                Ok(ExecEvent::Stderr(bytes)) => capture.push(Stream::Stderr, bytes),
                Ok(ExecEvent::Exit(done)) => {
                    exit = Some(done);
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    let mut output = plugin_error(&error);
                    if let Some(rendered) = capture.render_partial() {
                        let message =
                            format!("{}\nOutput until then:\n{rendered}", output_text(&output));
                        output = ToolOutput::error(message);
                    }
                    return open.finish(output);
                }
            }
        }
        let Some(exit) = exit else {
            return open.finish(ToolOutput::error(
                "the environment ended the command without an exit status",
            ));
        };
        let failed = exit.code != 0 || exit.timed_out;
        let text = render(&capture, exit, timeout);
        let output = ToolOutput {
            content: vec![giles_types::Block::text(text)],
            is_error: failed,
        };
        open.finish(output)
    }
}

fn output_text(output: &ToolOutput) -> &str {
    match output.content.first() {
        Some(giles_types::Block::Text { text }) => text,
        _ => "",
    }
}

/// The result text: exit code, a timeout note, then each stream that wrote anything.
fn render(capture: &Capture, exit: ExecExit, timeout: Duration) -> String {
    let mut text = format!("Exit code: {}\n", exit.code);
    if exit.timed_out {
        let _ = writeln!(
            text,
            "Timed out: the command was killed after {} seconds.",
            timeout.as_secs()
        );
    }
    let stdout = capture.stream(Stream::Stdout);
    let stderr = capture.stream(Stream::Stderr);
    if stdout.is_empty() && stderr.is_empty() {
        text.push_str("(no output)\n");
    }
    if !stdout.is_empty() {
        let _ = write!(text, "\nstdout:\n{stdout}");
        if !stdout.ends_with('\n') {
            text.push('\n');
        }
    }
    if !stderr.is_empty() {
        let _ = write!(text, "\nstderr:\n{stderr}");
        if !stderr.ends_with('\n') {
            text.push('\n');
        }
    }
    if exit.dropped_bytes > 0 {
        let _ = writeln!(
            text,
            "\n[The environment dropped {} more bytes of output after the first {} KiB.]",
            exit.dropped_bytes,
            EXEC_OUTPUT_LIMIT / 1024
        );
    }
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

/// A piece of output and the stream it came from.
#[derive(Debug)]
struct Piece {
    stream: Stream,
    bytes: Vec<u8>,
}

/// The head and the tail of a command's combined output.
///
/// The first `head_cap` bytes in arrival order are kept, and then the last `tail_cap` bytes; what
/// arrives between is counted per stream and dropped.
#[derive(Debug)]
struct Capture {
    head_cap: usize,
    tail_cap: usize,
    head: Vec<Piece>,
    head_len: usize,
    tail: VecDeque<Piece>,
    tail_len: usize,
    /// Bytes dropped from the middle, per stream: stdout, stderr.
    dropped: [u64; 2],
}

impl Capture {
    fn new(head_cap: usize, tail_cap: usize) -> Self {
        Self {
            head_cap,
            tail_cap,
            head: Vec::new(),
            head_len: 0,
            tail: VecDeque::new(),
            tail_len: 0,
            dropped: [0; 2],
        }
    }

    fn push(&mut self, stream: Stream, mut bytes: Vec<u8>) {
        if self.head_len < self.head_cap && !bytes.is_empty() {
            let take = (self.head_cap - self.head_len).min(bytes.len());
            let rest = bytes.split_off(take);
            self.head_len += bytes.len();
            self.head.push(Piece { stream, bytes });
            bytes = rest;
        }
        if bytes.is_empty() {
            return;
        }
        self.tail_len += bytes.len();
        self.tail.push_back(Piece { stream, bytes });
        while self.tail_len > self.tail_cap {
            let excess = self.tail_len - self.tail_cap;
            let Some(front) = self.tail.front_mut() else {
                break;
            };
            let cut = excess.min(front.bytes.len());
            self.dropped[front.stream as usize] += cut as u64;
            self.tail_len -= cut;
            if cut == front.bytes.len() {
                self.tail.pop_front();
            } else {
                front.bytes.drain(..cut);
            }
        }
    }

    /// What `stream` wrote, as text: its head, a marker if bytes were dropped, and its tail.
    fn stream(&self, stream: Stream) -> String {
        let of = |pieces: &mut dyn Iterator<Item = &Piece>| -> String {
            let mut bytes = Vec::new();
            for piece in pieces.filter(|piece| piece.stream == stream) {
                bytes.extend_from_slice(&piece.bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let mut text = of(&mut self.head.iter());
        let tail = of(&mut self.tail.iter());
        let dropped = self.dropped[stream as usize];
        if dropped > 0 {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            let _ = writeln!(text, "[... {dropped} bytes of output left out ...]");
        }
        text.push_str(&tail);
        text
    }

    /// Both streams as far as they went, for a command whose output stream failed; `None` if
    /// nothing was written.
    fn render_partial(&self) -> Option<String> {
        let stdout = self.stream(Stream::Stdout);
        let stderr = self.stream(Stream::Stderr);
        if stdout.is_empty() && stderr.is_empty() {
            return None;
        }
        let mut text = String::new();
        if !stdout.is_empty() {
            let _ = write!(text, "stdout:\n{stdout}");
            if !stdout.ends_with('\n') {
                text.push('\n');
            }
        }
        if !stderr.is_empty() {
            let _ = write!(text, "stderr:\n{stderr}");
        }
        Some(text)
    }
}
