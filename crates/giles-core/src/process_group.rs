//! Process groups of the children the core launches (plugins and stdio MCP servers): each runs as
//! the leader of its own group, and stopping it means signalling the group, so its children go
//! too.
//!
//! The group id is the leader's pid, which stays reserved only while the leader is unreaped. So
//! the group may be signalled only while the leader runs or has exited without being reaped yet;
//! [`exited_unreaped`] checks for an exit without reaping.

#![allow(unsafe_code)]

use std::io;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};

/// How often [`wait_exited`] checks for an exit when `SIGCHLD` can't be watched.
const EXIT_POLL: Duration = Duration::from_millis(50);

/// How often it checks anyway while watching `SIGCHLD` (a safety net; signals can't be lost, but
/// they coalesce).
const EXIT_POLL_FALLBACK: Duration = Duration::from_secs(1);

/// Waits until child `pid` has exited, without reaping it ([`exited_unreaped`]). Cancel-safe.
pub(crate) async fn wait_exited(pid: u32) -> io::Result<()> {
    // Subscribed before each check, so an exit between the check and the wait still wakes us.
    let mut sigchld = signal(SignalKind::child()).ok();
    while !exited_unreaped(pid)? {
        match &mut sigchld {
            Some(sigchld) => {
                tokio::select! {
                    _ = sigchld.recv() => {}
                    () = tokio::time::sleep(EXIT_POLL_FALLBACK) => {}
                }
            }
            None => tokio::time::sleep(EXIT_POLL).await,
        }
    }
    Ok(())
}

/// Whether child `pid` has exited, without reaping it (`waitid` with `WNOWAIT`), so its pid stays
/// reserved.
pub(crate) fn exited_unreaped(pid: u32) -> io::Result<bool> {
    // SAFETY: `siginfo_t` is a plain C struct, for which all zero bytes is a valid value.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a valid, writable `siginfo_t`; `waitid` only writes it.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // With `WNOHANG`, a child that hasn't exited leaves `si_pid` 0.
    Ok(siginfo_pid(&info) != 0)
}

#[cfg(target_os = "linux")]
fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    // SAFETY: `waitid` filled `info` for a child event (or left it zeroed), so the pid field is
    // the one in use.
    unsafe { info.si_pid() }
}

#[cfg(target_os = "macos")]
fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
}

/// Sends `signal` to process group `pgid`. A group that no longer exists is not an error.
pub(crate) fn signal_group(pgid: u32, signal: libc::c_int) -> io::Result<()> {
    let pgid = libc::pid_t::try_from(pgid).map_err(io::Error::other)?;
    // SAFETY: `kill` takes plain integers; a negative pid addresses the process group.
    if unsafe { libc::kill(-pgid, signal) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}
