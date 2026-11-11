//! Running commands in a workspace container.
//!
//! The Docker Engine API has no call that kills an exec. Every command therefore runs under the
//! helper's process-group wrapper (`giles-workspace-helper run --id ID -- ...`), and cancelling it
//! (a timeout, or dropping the stream) runs a second exec, `giles-workspace-helper kill --id ID`.

use std::pin::Pin;
use std::time::Duration;

use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::time::{Instant, Sleep};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::error::{Error, Result};
use crate::helper::HELPER_PATH;
use crate::types::{ExecEvent, ExecExit, ExecRequest};
use crate::{DockerEnvironment, WORKSPACE_DIR};

/// How long after a kill the command's output may keep flowing before the exec is abandoned.
const KILL_GRACE: Duration = Duration::from_secs(10);

/// How long to wait for the engine to record an exec's exit status after its output ends.
const EXIT_STATUS_WAIT: Duration = Duration::from_secs(5);

type Output =
    Pin<Box<dyn Stream<Item = std::result::Result<LogOutput, bollard::errors::Error>> + Send>>;

/// What a command that ran to completion wrote and returned.
pub(crate) struct Plain {
    pub code: i64,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// What [`DockerEnvironment::run_helper`] collected from a wrapped command.
pub(crate) struct Collected {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub dropped: u64,
}

struct Attached {
    exec_id: String,
    output: Output,
    input: Pin<Box<dyn AsyncWrite + Send>>,
}

/// Kills a wrapped command when dropped, unless the command already ended.
struct KillOnDrop {
    env: DockerEnvironment,
    container: String,
    run_id: String,
    armed: bool,
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            warn!(run = %self.run_id, "can't kill a cancelled command outside a runtime");
            return;
        };
        let (env, container, run_id) = (
            self.env.clone(),
            self.container.clone(),
            self.run_id.clone(),
        );
        runtime.spawn(async move { env.kill_run(&container, &run_id).await });
    }
}

impl DockerEnvironment {
    /// Runs `command` with `/bin/sh -c` in `/workspace` of a started workspace and streams its
    /// output, then its exit. Dropping the stream kills the command and everything it started.
    ///
    /// At most `output_limit` bytes of output are sent; the rest is counted in
    /// [`ExecExit::dropped_bytes`]. A command still running after `timeout` is killed, and its
    /// exit says so.
    pub async fn exec(
        &self,
        id: &str,
        request: ExecRequest,
    ) -> Result<BoxStream<'static, Result<ExecEvent>>> {
        crate::check_workspace_id(id)?;
        let container = self.running_container(id).await?;
        let command = vec!["/bin/sh".to_owned(), "-c".to_owned(), request.command];
        self.exec_wrapped(
            container,
            command,
            None,
            request.timeout,
            request.output_limit,
        )
        .await
    }

    /// The name of a workspace's running container.
    pub(crate) async fn running_container(&self, id: &str) -> Result<String> {
        use bollard::models::ContainerSummaryStateEnum::RUNNING;
        match self.find_container(id).await? {
            Some(c) if c.state == Some(RUNNING) => Ok(c.id.unwrap_or_else(|| id.to_owned())),
            Some(_) | None => {
                if self.find_volume(id).await?.is_none() {
                    Err(Error::not_found(format!("there is no workspace {id}")))
                } else {
                    Err(Error::invalid(format!(
                        "the workspace {id} isn't running; start it first"
                    )))
                }
            }
        }
    }

    /// Runs `command` under the process-group wrapper and streams its events.
    pub(crate) async fn exec_wrapped(
        &self,
        container: String,
        command: Vec<String>,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
        output_limit: u64,
    ) -> Result<BoxStream<'static, Result<ExecEvent>>> {
        let run_id = Uuid::new_v4().to_string();
        // Armed before anything is sent, so cancelling this future at any point kills the command.
        let guard = KillOnDrop {
            env: self.clone(),
            container: container.clone(),
            run_id: run_id.clone(),
            armed: true,
        };
        let mut cmd = vec![
            HELPER_PATH.to_owned(),
            "run".to_owned(),
            "--id".to_owned(),
            run_id,
            "--".to_owned(),
        ];
        cmd.extend(command);
        let attached = match self.start_exec(&container, cmd, stdin.is_some()).await {
            Ok(attached) => attached,
            Err(error) => {
                // Nothing started, so there is nothing to kill.
                let mut guard = guard;
                guard.armed = false;
                return Err(error);
            }
        };
        let Attached {
            exec_id,
            output,
            mut input,
        } = attached;
        if let Some(bytes) = stdin {
            input
                .write_all(&bytes)
                .await
                .map_err(|e| Error::Unavailable(format!("sending input to the command: {e}")))?;
            input
                .shutdown()
                .await
                .map_err(|e| Error::Unavailable(format!("closing the command's input: {e}")))?;
        }
        let state = StreamState {
            env: self.clone(),
            exec_id,
            output,
            _input: input,
            deadline: Box::pin(tokio::time::sleep_until(Instant::now() + timeout)),
            grace: None,
            timed_out: false,
            sent: 0,
            dropped: 0,
            limit: output_limit,
            guard,
            done: false,
        };
        Ok(futures::stream::unfold(state, next_event).boxed())
    }

    /// Runs a helper subcommand to completion and collects its output. `timeout` kills it.
    pub(crate) async fn run_helper(
        &self,
        container: String,
        args: &[&str],
        stdin: Option<Vec<u8>>,
        timeout: Duration,
        output_limit: u64,
    ) -> Result<Collected> {
        let mut command = vec![HELPER_PATH.to_owned()];
        command.extend(args.iter().map(|a| (*a).to_owned()));
        let mut events = self
            .exec_wrapped(container, command, stdin, timeout, output_limit)
            .await?;
        let mut collected = Collected {
            code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: false,
            dropped: 0,
        };
        while let Some(event) = events.next().await {
            match event? {
                ExecEvent::Stdout(bytes) => collected.stdout.extend_from_slice(&bytes),
                ExecEvent::Stderr(bytes) => collected.stderr.extend_from_slice(&bytes),
                ExecEvent::Exit(exit) => {
                    collected.code = exit.code;
                    collected.timed_out = exit.timed_out;
                    collected.dropped = exit.dropped_bytes;
                }
            }
        }
        Ok(collected)
    }

    /// Runs a command directly (no wrapper, no timeout) and collects what it wrote. For short
    /// housekeeping commands.
    pub(crate) async fn exec_plain(&self, container: &str, command: Vec<String>) -> Result<Plain> {
        let Attached {
            exec_id,
            mut output,
            input,
        } = self.start_exec(container, command, false).await?;
        drop(input);
        let mut plain = Plain {
            code: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        while let Some(item) = output.next().await {
            match item.map_err(|e| Error::docker("reading a command's output", &e))? {
                LogOutput::StdErr { message } => plain.stderr.extend_from_slice(&message),
                LogOutput::StdOut { message } | LogOutput::Console { message } => {
                    plain.stdout.extend_from_slice(&message);
                }
                LogOutput::StdIn { .. } => {}
            }
        }
        plain.code = self.exit_code(&exec_id).await?;
        Ok(plain)
    }

    async fn start_exec(
        &self,
        container: &str,
        command: Vec<String>,
        stdin: bool,
    ) -> Result<Attached> {
        let created = self
            .docker()
            .create_exec(
                container,
                CreateExecOptions {
                    attach_stdin: Some(stdin),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(false),
                    working_dir: Some(WORKSPACE_DIR.to_owned()),
                    cmd: Some(command),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| Error::docker("starting a command in the workspace", &e))?;
        let started = self
            .docker()
            .start_exec(
                &created.id,
                Some(StartExecOptions {
                    detach: false,
                    tty: false,
                    // Output is relayed as it arrives; the cap is only the decoder's buffer.
                    output_capacity: Some(64 * 1024),
                }),
            )
            .await
            .map_err(|e| Error::docker("starting a command in the workspace", &e))?;
        match started {
            StartExecResults::Attached { output, input } => Ok(Attached {
                exec_id: created.id,
                output,
                input,
            }),
            StartExecResults::Detached => Err(Error::other("the engine detached the command")),
        }
    }

    /// The exit status of a finished exec, waiting briefly for the engine to record it.
    async fn exit_code(&self, exec_id: &str) -> Result<i64> {
        let deadline = Instant::now() + EXIT_STATUS_WAIT;
        loop {
            let inspect = self
                .docker()
                .inspect_exec(exec_id)
                .await
                .map_err(|e| Error::docker("reading a command's exit status", &e))?;
            if inspect.running != Some(true)
                && let Some(code) = inspect.exit_code
            {
                return Ok(code);
            }
            if Instant::now() >= deadline {
                return Err(Error::Unavailable(
                    "the engine didn't report the command's exit status".to_owned(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Kills the process group of a wrapped command with a second exec. Best effort.
    pub(crate) async fn kill_run(&self, container: &str, run_id: &str) {
        let command = vec![
            HELPER_PATH.to_owned(),
            "kill".to_owned(),
            "--id".to_owned(),
            run_id.to_owned(),
        ];
        match tokio::time::timeout(Duration::from_secs(15), self.exec_plain(container, command))
            .await
        {
            Ok(Ok(out)) if out.code == 0 => debug!(run = run_id, "killed command"),
            Ok(Ok(out)) => warn!(
                run = run_id,
                code = out.code,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "the kill command failed"
            ),
            Ok(Err(e)) => warn!(run = run_id, error = %e, "couldn't run the kill command"),
            Err(_) => warn!(run = run_id, "the kill command timed out"),
        }
    }
}

struct StreamState {
    env: DockerEnvironment,
    exec_id: String,
    output: Output,
    /// Held so the connection's write half stays open; the command may read standard input.
    _input: Pin<Box<dyn AsyncWrite + Send>>,
    deadline: Pin<Box<Sleep>>,
    /// Set once the command has been killed for running too long.
    grace: Option<Pin<Box<Sleep>>>,
    timed_out: bool,
    sent: u64,
    dropped: u64,
    limit: u64,
    guard: KillOnDrop,
    done: bool,
}

/// Produces the next event of a command's stream, or `None` after the exit event.
async fn next_event(mut state: StreamState) -> Option<(Result<ExecEvent>, StreamState)> {
    if state.done {
        return None;
    }
    loop {
        enum Step {
            Output(Option<std::result::Result<LogOutput, bollard::errors::Error>>),
            TimedOut,
            GraceOver,
        }
        let step = {
            let state = &mut state;
            tokio::select! {
                item = state.output.next() => Step::Output(item),
                () = &mut state.deadline, if !state.timed_out => Step::TimedOut,
                () = async { state.grace.as_mut().expect("grace is set while timed out").await },
                    if state.timed_out => Step::GraceOver,
            }
        };
        match step {
            Step::TimedOut => {
                state.timed_out = true;
                state.grace = Some(Box::pin(tokio::time::sleep(KILL_GRACE)));
                state.guard.armed = false;
                let (container, run_id) =
                    (state.guard.container.clone(), state.guard.run_id.clone());
                state.env.kill_run(&container, &run_id).await;
            }
            Step::GraceOver => {
                // Something still holds the output open after the kill; report what we know.
                return finish(state, Some(137)).await;
            }
            Step::Output(Some(Ok(piece))) => {
                let (bytes, stderr) = match piece {
                    LogOutput::StdOut { message } | LogOutput::Console { message } => {
                        (message, false)
                    }
                    LogOutput::StdErr { message } => (message, true),
                    LogOutput::StdIn { .. } => continue,
                };
                let remaining = state.limit.saturating_sub(state.sent);
                let take = usize::try_from(remaining).map_or(bytes.len(), |r| r.min(bytes.len()));
                state.sent += take as u64;
                state.dropped += (bytes.len() - take) as u64;
                if take == 0 {
                    continue;
                }
                let chunk = bytes[..take].to_vec();
                let event = if stderr {
                    ExecEvent::Stderr(chunk)
                } else {
                    ExecEvent::Stdout(chunk)
                };
                return Some((Ok(event), state));
            }
            Step::Output(Some(Err(e))) => {
                state.done = true;
                return Some((
                    Err(Error::docker("reading the command's output", &e)),
                    state,
                ));
            }
            Step::Output(None) => return finish(state, None).await,
        }
    }
}

/// Ends the stream with the exit event. `code` overrides the engine's record.
async fn finish(
    mut state: StreamState,
    code: Option<i32>,
) -> Option<(Result<ExecEvent>, StreamState)> {
    state.done = true;
    let code = match code {
        Some(code) => Ok(code),
        None => state
            .env
            .exit_code(&state.exec_id)
            .await
            .map(|c| i32::try_from(c).unwrap_or(i32::MAX)),
    };
    match code {
        Ok(code) => {
            // The command ended; there is nothing left to kill.
            state.guard.armed = false;
            let exit = ExecExit {
                code,
                timed_out: state.timed_out,
                dropped_bytes: state.dropped,
            };
            Some((Ok(ExecEvent::Exit(exit)), state))
        }
        Err(e) => Some((Err(e), state)),
    }
}
