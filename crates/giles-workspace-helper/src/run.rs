//! `run` and `kill`: the process-group wrapper.
//!
//! The Docker Engine API can't kill an exec, so every command runs as `run --id ID -- ...`. `run`
//! starts the command in a new process group and records the group id in `<run dir>/<ID>`; the
//! plugin cancels the command with a second exec, `kill --id ID`, which SIGKILLs that group. The
//! command's descendants die with it unless they leave the group (`setsid`).

use std::ffi::OsString;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, Signal, kill_process_group};

use crate::{Args, Failure, Outcome};

/// Where process group records live; `GILES_RUN_DIR` overrides it (for tests).
const DEFAULT_RUN_DIR: &str = "/.giles/run";

/// How often `run` checks whether the command ended while its output pipes stay open, e.g. because
/// a background process it started still holds them.
const EXIT_CHECK_MS: i64 = 50;

fn run_dir() -> PathBuf {
    std::env::var_os("GILES_RUN_DIR").map_or_else(|| PathBuf::from(DEFAULT_RUN_DIR), PathBuf::from)
}

/// An id becomes a file name, so it can't hold a path separator.
fn check_id(id: &str) -> Outcome<()> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(Failure::usage(
            "--id must be 1 to 64 characters of letters, digits, \"-\", and \"_\"",
        ))
    }
}

/// `run --id ID -- COMMAND [ARG...]`: runs the command, forwards its output, and exits with its
/// status (128 + N when signal N ended it).
pub fn run(mut args: Args) -> Outcome<ExitCode> {
    let id = args.required("--id")?;
    check_id(&id)?;
    let separator = args
        .0
        .iter()
        .position(|a| a == "--")
        .ok_or_else(|| Failure::usage("expected `--` before the command"))?;
    let mut command: Vec<OsString> = args.0.split_off(separator + 1);
    args.0.pop();
    args.finish()?;
    if command.is_empty() {
        return Err(Failure::usage("expected a command after `--`"));
    }
    let program = command.remove(0);

    let dir = run_dir();
    fs::create_dir_all(&dir).map_err(|e| Failure::io(&dir, &e))?;
    let record = dir.join(&id);
    let marker = dir.join(format!("{id}.kill"));

    let mut child = match Command::new(&program)
        .args(&command)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "giles-workspace-helper: {}: {}",
                program.to_string_lossy(),
                crate::describe_io(&error)
            );
            return Ok(ExitCode::from(if error.kind() == ErrorKind::NotFound {
                127
            } else {
                126
            }));
        }
    };
    // The child's pid is its group id, because `process_group(0)` made it the group's leader.
    let group = child.id();
    // A `kill` that ran before the record existed left a marker. Writing the record first and then
    // looking for the marker means one side always sees the other.
    let _ = fs::write(&record, group.to_string());
    if marker.exists() {
        signal_group(group);
    }

    let status = forward_output(&mut child);
    let _ = fs::remove_file(&record);
    let _ = fs::remove_file(&marker);
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    Ok(ExitCode::from(u8::try_from(code & 0xff).unwrap_or(1)))
}

/// Copies the child's stdout and stderr to ours until it exits, then returns its status. Output
/// still in the pipes when it exits is copied too, but a background process holding the pipes open
/// doesn't keep this running.
fn forward_output(child: &mut Child) -> ExitStatus {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut sink_out = Sink::new(std::io::stdout());
    let mut sink_err = Sink::new(std::io::stderr());
    let mut exited: Option<ExitStatus> = None;
    let mut buffer = vec![0u8; 64 * 1024];

    while stdout.is_some() || stderr.is_some() {
        if exited.is_none() {
            exited = child.try_wait().ok().flatten();
        }
        // Once the command ended, only output that is already waiting is worth reading.
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: if exited.is_some() {
                0
            } else {
                EXIT_CHECK_MS * 1_000_000
            },
        };
        let mut fds = Vec::new();
        let mut owners = Vec::new();
        if let Some(pipe) = &stdout {
            fds.push(PollFd::new(pipe, PollFlags::IN));
            owners.push(0);
        }
        if let Some(pipe) = &stderr {
            fds.push(PollFd::new(pipe, PollFlags::IN));
            owners.push(1);
        }
        let ready = match poll(&mut fds, Some(&timeout)) {
            Ok(n) => n,
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => break,
        };
        let readable: Vec<usize> = fds
            .iter()
            .zip(&owners)
            .filter(|(fd, _)| !fd.revents().is_empty())
            .map(|(_, owner)| *owner)
            .collect();
        drop(fds);
        for owner in readable {
            if owner == 0 {
                match read_some(stdout.as_mut(), &mut buffer) {
                    0 => stdout = None,
                    n => sink_out.write(&buffer[..n]),
                }
            } else {
                match read_some(stderr.as_mut(), &mut buffer) {
                    0 => stderr = None,
                    n => sink_err.write(&buffer[..n]),
                }
            }
        }
        if ready == 0 && exited.is_some() {
            break;
        }
    }
    exited.unwrap_or_else(|| {
        child
            .wait()
            .unwrap_or_else(|_| ExitStatus::from_raw(1 << 8))
    })
}

/// Reads what the poll said is waiting; 0 means the pipe closed (or failed).
fn read_some<R: Read>(pipe: Option<&mut R>, buffer: &mut [u8]) -> usize {
    let Some(pipe) = pipe else { return 0 };
    loop {
        match pipe.read(buffer) {
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Ok(n) => return n,
            Err(_) => return 0,
        }
    }
}

/// A destination that stops writing after its first failure (the plugin stopped listening) but
/// keeps the helper draining the child's pipes, so the child never blocks on a full pipe.
struct Sink<W: Write> {
    inner: W,
    broken: bool,
}

impl<W: Write> Sink<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            broken: false,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        if !self.broken && (self.inner.write_all(bytes).is_err() || self.inner.flush().is_err()) {
            self.broken = true;
        }
    }
}

/// SIGKILLs a process group; a group that no longer exists is fine.
fn signal_group(group: u32) {
    let pid = i32::try_from(group).ok().and_then(Pid::from_raw);
    if let Some(pid) = pid {
        let _ = kill_process_group(pid, Signal::KILL);
    }
}

/// `kill --id ID`: kills the group recorded by `run --id ID`. If `run` hasn't recorded it yet,
/// leaves a marker that makes `run` kill the group as soon as it starts.
pub fn kill(id: &str) -> Outcome<()> {
    check_id(id)?;
    let dir = run_dir();
    let record = dir.join(id);
    if kill_recorded(&record) {
        return Ok(());
    }
    fs::create_dir_all(&dir).map_err(|e| Failure::io(&dir, &e))?;
    let marker = dir.join(format!("{id}.kill"));
    fs::write(&marker, b"").map_err(|e| Failure::io(&marker, &e))?;
    // `run` may have written its record between the first look and the marker.
    if kill_recorded(&record) {
        let _ = fs::remove_file(&marker);
    }
    Ok(())
}

/// Kills the group in a record file. False if there is no record.
fn kill_recorded(record: &std::path::Path) -> bool {
    let Ok(text) = fs::read_to_string(record) else {
        return false;
    };
    match text.trim().parse() {
        Ok(group) => {
            signal_group(group);
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::check_id;

    #[test]
    fn ids_are_file_names() {
        assert!(check_id("0d6a1c3e-9a3f-4a53-8f5b-0c6a2f9f1a11").is_ok());
        assert!(check_id("job_1").is_ok());
        assert!(check_id("").is_err());
        assert!(check_id("../x").is_err());
        assert!(check_id("a/b").is_err());
        assert!(check_id(&"a".repeat(65)).is_err());
    }
}
