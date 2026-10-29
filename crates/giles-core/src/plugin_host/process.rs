//! The plugin process: spawning it in its own process group with its end of the socket pair as
//! fd 3 and the token on stdin, capturing its stderr, its pid file, and stopping the whole group.

#![allow(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use giles_proto::handshake::PLUGIN_FD;
use tokio::io::AsyncWriteExt as _;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tracing::Instrument as _;

use crate::config::Secret;
use crate::process_group::{exited_unreaped, signal_group, wait_exited};
use crate::stderr_lines;

/// The `tracing` target of a plugin's stderr lines, so an operator can filter them
/// (`GILES_LOG=info,giles_core::plugin_host::stderr=warn`). The level is the one the line itself
/// carries if it is a log message, else `info`.
pub const STDERR_TARGET: &str = "giles_core::plugin_host::stderr";

/// How many of the latest stderr lines a [`PluginProcess`] keeps for error messages.
const STDERR_TAIL_LINES: usize = 20;

/// How long a process gets to exit after `SIGKILL` before the core stops waiting for it.
const KILL_WAIT: Duration = Duration::from_secs(5);

/// What to run and how.
pub(crate) struct SpawnSpec<'a> {
    /// The program: the trampoline, or the plugin itself when unsandboxed.
    pub program: &'a Path,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// `Some`: exactly this environment and working directory (the unsandboxed launch, which
    /// does what the trampoline would). `None`: inherit the core's (the trampoline scrubs it).
    pub env: Option<(BTreeMap<OsString, OsString>, &'a Path)>,
    /// The per-launch token, written to stdin as one line.
    pub token: &'a Secret,
    /// The pid file to write once the process runs, and the binary to record in it.
    pub pid_file: &'a Path,
    /// The plugin binary recorded in the pid file.
    pub binary: &'a Path,
    /// Values scrubbed from stderr.
    pub secrets: Arc<[Secret]>,
}

/// A spawned plugin: the process, its stdout (for the handshake line), and the core's end of the
/// socket pair.
pub(crate) struct Spawned {
    pub process: PluginProcess,
    pub stdout: ChildStdout,
    pub core_end: StdUnixStream,
}

/// A running plugin process, the leader of its own process group (and session).
///
/// Holds the plugin's stdin open (closing it tells the plugin the core is gone), captures its
/// stderr, and owns its pid file.
///
/// The plugin's children die with it: the moment the plugin is seen to have exited, and before it
/// is reaped, the rest of its process group gets `SIGKILL`. The group is only ever signalled while
/// the plugin is unreaped, when its pid (the group id) can't belong to anyone else. Dropping a
/// running process kills the whole group the same way; either way the pid file is removed.
pub struct PluginProcess {
    child: Child,
    /// The process id, which is also the process group id.
    pid: u32,
    stdin: Option<ChildStdin>,
    pid_file: PathBuf,
    status: Option<ExitStatus>,
    stderr: Option<JoinHandle<()>>,
    tail: StderrTail,
}

impl std::fmt::Debug for PluginProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginProcess")
            .field("pid", &self.pid)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Spawns `spec` in a new session (so a new process group), with `plugin_end` of the socket pair
/// as fd 3, writes the token line to its stdin, writes the pid file, and starts logging its
/// stderr under `span`.
pub(crate) async fn spawn(spec: SpawnSpec<'_>, span: &tracing::Span) -> io::Result<Spawned> {
    let (core_end, plugin_end) = StdUnixStream::pair()?;
    let mut command = Command::new(spec.program);
    command
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some((env, cwd)) = &spec.env {
        command.env_clear().envs(env).current_dir(cwd);
    }
    let fd = plugin_end.as_raw_fd();
    // SAFETY: the closure runs in the child between `fork` and `exec` and calls only `setsid`,
    // `fcntl`, and `dup2`, which are async-signal-safe; it allocates nothing. `fd` stays open in
    // the parent until `spawn` returns, so it is open in the child.
    unsafe {
        command.pre_exec(move || child_setup(fd));
    }
    let mut child = command.spawn()?;
    drop(plugin_end);
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("the plugin exited before it could be tracked"))?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err(io::Error::other("the plugin's stdio wasn't piped"));
    };
    let tail = StderrTail::default();
    let stderr =
        tokio::spawn(log_stderr(stderr, spec.secrets, tail.clone()).instrument(span.clone()));
    let mut process = PluginProcess {
        child,
        pid,
        stdin: None,
        pid_file: spec.pid_file.to_path_buf(),
        status: None,
        stderr: Some(stderr),
        tail,
    };
    write_pid_file(spec.pid_file, pid, spec.binary)?;
    // A plugin that died at once closes stdin; that shows up as a handshake failure with its exit
    // status, which says more than this write's `EPIPE`.
    let line = format!("{}\n", spec.token.expose());
    if let Err(error) = stdin.write_all(line.as_bytes()).await {
        tracing::debug!(parent: span, %error, "cannot write the token to the plugin's stdin");
    }
    process.stdin = Some(stdin);
    Ok(Spawned {
        process,
        stdout,
        core_end,
    })
}

/// In the child: a new session (and process group), and `fd` as fd 3 without close-on-exec.
fn child_setup(fd: RawFd) -> io::Result<()> {
    // SAFETY: `setsid` takes no arguments and only changes this process's session and group.
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error());
    }
    if fd == PLUGIN_FD {
        // `dup2` onto itself would keep the close-on-exec flag, so clear it.
        // SAFETY: `fcntl` on an open descriptor only reads and sets its flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        // SAFETY: as above.
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } == -1
        {
            return Err(io::Error::last_os_error());
        }
    // SAFETY: `fd` is open; `dup2` makes fd 3 a copy without close-on-exec, closing whatever fd 3
    // was (std and tokio open everything else close-on-exec, so nothing else leaks).
    } else if unsafe { libc::dup2(fd, PLUGIN_FD) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl PluginProcess {
    /// The process id, which is also its process group id.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Waits for the process to exit, then kills whatever is left of its group and reaps it.
    /// Cancel-safe.
    ///
    /// # Errors
    ///
    /// Waiting failed (not expected for a child of this process).
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.reap_if_exited()? {
                return Ok(status);
            }
            wait_exited(self.pid).await?;
        }
    }

    /// The exit status, if the process has exited (then what's left of its group is killed and it
    /// is reaped, as in [`PluginProcess::wait`]).
    pub fn exit_status(&mut self) -> Option<ExitStatus> {
        match self.reap_if_exited() {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(pid = self.pid, %error, "cannot check whether the plugin exited");
                None
            }
        }
    }

    /// If the plugin has exited: `SIGKILL` to the rest of its group while the unreaped plugin still
    /// reserves the group id, then reap it.
    fn reap_if_exited(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        if !exited_unreaped(self.pid)? {
            return Ok(None);
        }
        // Best effort: the plugin is gone either way. (macOS answers `EPERM` for a group whose
        // only member is the zombie plugin.)
        if let Err(error) = self.signal_group(libc::SIGKILL) {
            tracing::debug!(pid = self.pid, %error, "cannot signal the exited plugin's group");
        }
        let status = self
            .child
            .try_wait()?
            .ok_or_else(|| io::Error::other("the exited plugin couldn't be reaped"))?;
        self.status = Some(status);
        Ok(Some(status))
    }

    /// Closes the plugin's stdin. A Giles plugin takes end-of-file as the core being gone and
    /// exits (PLAN section 7.4).
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Up to the last 20 lines the plugin wrote to stderr, scrubbed, oldest first.
    pub fn stderr_tail(&self) -> Vec<String> {
        self.tail.lines()
    }

    /// Waits up to `timeout` for the plugin's stderr to reach end-of-file (every line logged).
    pub async fn stderr_closed(&mut self, timeout: Duration) {
        if let Some(task) = &mut self.stderr
            && tokio::time::timeout(timeout, task).await.is_ok()
        {
            self.stderr = None;
        }
    }

    /// Stops the process group: `SIGTERM`, up to `grace` for the plugin to exit, then `SIGKILL`.
    /// Once the plugin itself has exited, whatever is left of its group gets `SIGKILL` too (before
    /// the plugin is reaped), and the pid file is removed.
    ///
    /// # Errors
    ///
    /// Waiting failed, or the plugin didn't exit within 5 seconds of `SIGKILL`.
    pub async fn terminate(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        if self.exit_status().is_none() {
            if let Err(error) = self.signal_group(libc::SIGTERM) {
                tracing::debug!(pid = self.pid, %error, "cannot send SIGTERM to the plugin's group");
            }
            if let Ok(status) = tokio::time::timeout(grace, self.wait()).await {
                status?;
            } else {
                tracing::warn!(
                    pid = self.pid,
                    ?grace,
                    "the plugin ignored SIGTERM; killing it"
                );
            }
        }
        self.kill().await
    }

    /// Kills the process group with `SIGKILL`, waits for the plugin, and removes the pid file.
    ///
    /// # Errors
    ///
    /// Waiting failed, or the plugin didn't exit within 5 seconds of `SIGKILL`.
    pub async fn kill(&mut self) -> io::Result<ExitStatus> {
        // Only while the plugin runs, so the group id is still its pid. An exited plugin is reaped
        // by `exit_status`, which kills the rest of its group first; after that the id may belong
        // to someone else.
        if self.exit_status().is_none()
            && let Err(error) = self.signal_group(libc::SIGKILL)
        {
            // Waiting below reports a plugin that didn't die.
            tracing::debug!(pid = self.pid, %error, "cannot send SIGKILL to the plugin's group");
        }
        let status = tokio::time::timeout(KILL_WAIT, self.wait())
            .await
            .map_err(|_| io::Error::other("the plugin didn't exit after SIGKILL"))??;
        self.remove_pid_file();
        Ok(status)
    }

    /// Sends `signal` to the process group. A group that no longer exists is not an error.
    fn signal_group(&self, signal: libc::c_int) -> io::Result<()> {
        signal_group(self.pid, signal)
    }

    fn remove_pid_file(&self) {
        if let Err(error) = fs::remove_file(&self.pid_file)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(pid_file = %self.pid_file.display(), %error, "cannot remove the pid file");
        }
    }
}

impl Drop for PluginProcess {
    fn drop(&mut self) {
        if self.status.is_none() {
            // Unreaped, so the group id is still the plugin's. `kill_on_drop` would kill only the
            // leader; take the whole group. Tokio reaps the dropped child in the background.
            let _ = self.signal_group(libc::SIGKILL);
        }
        self.remove_pid_file();
    }
}

/// The latest stderr lines, shared with the logging task.
#[derive(Clone, Default)]
struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn push(&self, line: String) {
        let mut lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    fn lines(&self) -> Vec<String> {
        let lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        lines.iter().cloned().collect()
    }
}

async fn log_stderr(stderr: tokio::process::ChildStderr, secrets: Arc<[Secret]>, tail: StderrTail) {
    let secrets: Vec<&Secret> = secrets.iter().collect();
    stderr_lines::forward(stderr, &secrets, |line| {
        stderr_lines::log_line!(STDERR_TARGET, &line);
        tail.push(line);
    })
    .await;
}

/// The pid file: the pid and the plugin binary, one per line.
pub(crate) fn pid_file_contents(pid: u32, binary: &Path) -> String {
    format!("{pid}\n{}\n", binary.display())
}

/// Writes the pid file atomically, mode 0600.
fn write_pid_file(path: &Path, pid: u32, binary: &Path) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("the pid file has no parent directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(pid_file_contents(pid, binary).as_bytes())?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    /// Spawns `script` under `sh` the way plugins are spawned.
    async fn spawn_script(dir: &Path, script: &str) -> Spawned {
        let token = Secret::new("token-1234");
        let sh = Path::new("/bin/sh");
        let pid_file = dir.join("p.pid");
        spawn(
            SpawnSpec {
                program: sh,
                args: vec!["-c".into(), script.into()],
                env: None,
                token: &token,
                pid_file: &pid_file,
                binary: sh,
                secrets: Arc::from([token.clone()]),
            },
            &tracing::Span::none(),
        )
        .await
        .unwrap()
    }

    /// Whether `pid` is gone (or a zombie, which only its parent can reap).
    fn gone(pid: u32) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        stat.trim().is_empty() || stat.trim_start().starts_with('Z')
    }

    async fn wait_gone(pid: u32) {
        let start = Instant::now();
        while !gone(pid) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "{pid} still runs"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn read_file_eventually(path: &Path) -> String {
        let start = Instant::now();
        loop {
            if let Ok(text) = fs::read_to_string(path)
                && text.ends_with('\n')
            {
                return text;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "no {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn the_plugin_gets_fd3_the_token_and_its_own_group() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let script = format!(
            "read token; echo \"$token\" > {out}.token; echo hello >&3; \
             ps -o pgid= -p $$ > {out}.pgid; echo done > {out}; cat >/dev/null",
            out = out.display()
        );
        let mut spawned = spawn_script(dir.path(), &script).await;
        read_file_eventually(&out).await;
        let pid = spawned.process.pid();
        assert_eq!(
            fs::read_to_string(format!("{}.token", out.display())).unwrap(),
            "token-1234\n"
        );
        let pgid = fs::read_to_string(format!("{}.pgid", out.display())).unwrap();
        assert_eq!(pgid.trim(), pid.to_string());
        let mut buf = [0; 6];
        std::io::Read::read_exact(&mut spawned.core_end, &mut buf).unwrap();
        assert_eq!(&buf, b"hello\n");
        assert_eq!(
            fs::read_to_string(dir.path().join("p.pid")).unwrap(),
            format!("{pid}\n/bin/sh\n")
        );
        // Closing stdin ends `cat`, so the plugin exits by itself.
        spawned.process.close_stdin();
        let status = tokio::time::timeout(Duration::from_secs(10), spawned.process.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
    }

    #[tokio::test]
    async fn terminate_kills_the_whole_group_and_removes_the_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        let child_pid = dir.path().join("child");
        let script = format!("sleep 300 & echo $! > {}; wait", child_pid.display());
        let mut spawned = spawn_script(dir.path(), &script).await;
        let child: u32 = read_file_eventually(&child_pid)
            .await
            .trim()
            .parse()
            .unwrap();
        assert!(!gone(child));
        let status = spawned
            .process
            .terminate(Duration::from_secs(10))
            .await
            .unwrap();
        assert!(!status.success());
        wait_gone(child).await;
        assert!(!dir.path().join("p.pid").exists());
    }

    #[tokio::test]
    async fn a_plugin_ignoring_sigterm_is_killed_after_the_grace_period() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let script = format!(
            "trap '' TERM; echo > {}; while :; do sleep 1; done",
            ready.display()
        );
        let mut spawned = spawn_script(dir.path(), &script).await;
        read_file_eventually(&ready).await;
        let start = Instant::now();
        let status = spawned
            .process
            .terminate(Duration::from_millis(300))
            .await
            .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGKILL)
        );
    }

    #[tokio::test]
    async fn a_plugin_that_exits_takes_its_children_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let child_pid = dir.path().join("child");
        let script = format!("sleep 300 & echo $! > {}; exit 0", child_pid.display());
        let mut spawned = spawn_script(dir.path(), &script).await;
        let status = tokio::time::timeout(Duration::from_secs(10), spawned.process.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        let child: u32 = read_file_eventually(&child_pid)
            .await
            .trim()
            .parse()
            .unwrap();
        wait_gone(child).await;
        // Reaped: later calls only report the status.
        assert_eq!(spawned.process.exit_status(), Some(status));
        assert_eq!(spawned.process.kill().await.unwrap(), status);
    }

    #[tokio::test]
    async fn dropping_the_process_kills_its_group() {
        let dir = tempfile::tempdir().unwrap();
        let child_pid = dir.path().join("child");
        let script = format!("sleep 300 & echo $! > {}; wait", child_pid.display());
        let spawned = spawn_script(dir.path(), &script).await;
        let child: u32 = read_file_eventually(&child_pid)
            .await
            .trim()
            .parse()
            .unwrap();
        let pid = spawned.process.pid();
        drop(spawned);
        wait_gone(child).await;
        wait_gone(pid).await;
        assert!(!dir.path().join("p.pid").exists());
    }

    #[tokio::test]
    async fn stderr_is_scrubbed_and_its_tail_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut spawned = spawn_script(
            dir.path(),
            "for i in $(seq 1 25); do echo \"line $i\" >&2; done; echo 'token token-1234' >&2",
        )
        .await;
        spawned.process.wait().await.unwrap();
        spawned.process.stderr_closed(Duration::from_secs(10)).await;
        let tail = spawned.process.stderr_tail();
        assert_eq!(tail.len(), STDERR_TAIL_LINES);
        assert_eq!(tail[0], "line 7");
        assert_eq!(tail.last().unwrap(), "token <redacted>");
    }
}
