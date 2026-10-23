//! Orphan cleanup (PLAN section 7.4): before launching an instance, kill a plugin an earlier core
//! left running, as recorded in its pid file, but only if that process still runs the expected
//! plugin binary.

#![allow(unsafe_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::process_group::signal_group;

/// How long to wait for a killed orphan to disappear before launching anyway.
const ORPHAN_EXIT_WAIT: Duration = Duration::from_secs(5);

/// What a pid file records.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PidRecord {
    pub pid: u32,
    pub binary: PathBuf,
}

/// Parses a pid file: the pid, then the binary, one per line.
pub(crate) fn parse_pid_file(text: &str) -> Option<PidRecord> {
    let mut lines = text.lines();
    let pid: u32 = lines.next()?.parse().ok()?;
    let binary = PathBuf::from(lines.next()?);
    (pid > 1 && binary.is_absolute() && lines.next().is_none()).then_some(PidRecord { pid, binary })
}

/// What [`cleanup`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Cleanup {
    /// No pid file.
    None,
    /// The recorded process is gone or runs something else; only the pid file was removed.
    Stale,
    /// The recorded process ran `binary` and was killed.
    Killed(u32),
}

/// Reads `pid_file`; if the process it names still runs `binary` (absolute, symlinks resolved)
/// and the file recorded that binary, kills its process group (or just it, if it doesn't lead a
/// group) and waits briefly for it to go. The pid file is removed either way.
///
/// # Errors
///
/// The pid file can't be read or removed, or the kill failed.
pub(crate) async fn cleanup(pid_file: &Path, binary: &Path) -> io::Result<Cleanup> {
    let text = match fs::read_to_string(pid_file) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Cleanup::None),
        Err(e) => return Err(e),
    };
    let result = match parse_pid_file(&text) {
        Some(record) if record.binary == binary && runs(record.pid, binary) => {
            kill_orphan(record.pid)?;
            let start = Instant::now();
            while runs(record.pid, binary) && start.elapsed() < ORPHAN_EXIT_WAIT {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Cleanup::Killed(record.pid)
        }
        Some(record) => {
            tracing::debug!(
                pid = record.pid,
                recorded = %record.binary.display(),
                "the recorded plugin process is gone",
            );
            Cleanup::Stale
        }
        None => {
            tracing::warn!(pid_file = %pid_file.display(), "ignoring a malformed pid file");
            Cleanup::Stale
        }
    };
    fs::remove_file(pid_file)?;
    Ok(result)
}

/// `SIGKILL` to the orphan's process group if it leads one (plugins do), else to it alone.
fn kill_orphan(pid: u32) -> io::Result<()> {
    let raw = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
    // SAFETY: `getpgid` takes a plain integer and only reads the process's group.
    if unsafe { libc::getpgid(raw) } == raw {
        return signal_group(pid, libc::SIGKILL);
    }
    // SAFETY: `kill` takes plain integers.
    if unsafe { libc::kill(raw, libc::SIGKILL) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

/// Whether process `pid` is running `binary`.
fn runs(pid: u32, binary: &Path) -> bool {
    executable(pid).is_some_and(|exe| exe == binary)
}

/// The executable process `pid` runs, if it exists and can be read.
#[cfg(target_os = "linux")]
pub(crate) fn executable(pid: u32) -> Option<PathBuf> {
    let exe = fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    // A binary replaced since the launch (an upgrade) shows as "<path> (deleted)"; it is still
    // the plugin that path named.
    let deleted = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)"));
    Some(deleted.map_or(exe.clone(), PathBuf::from))
}

/// The executable process `pid` runs, if it exists and can be read.
#[cfg(target_os = "macos")]
pub(crate) fn executable(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;

    let pid = libc::c_int::try_from(pid).ok()?;
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let size = u32::try_from(buffer.len()).ok()?;
    // SAFETY: `buffer` is writable for `size` bytes, which `proc_pidpath` never exceeds; it
    // returns the number of bytes written, or 0 or less on error.
    let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), size) };
    let len = usize::try_from(len).ok().filter(|&len| len > 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..len])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_files_parse_strictly() {
        assert_eq!(
            parse_pid_file("123\n/b/giles-channel-irc\n"),
            Some(PidRecord {
                pid: 123,
                binary: "/b/giles-channel-irc".into()
            })
        );
        for bad in [
            "",
            "123\n",
            "x\n/b\n",
            "1\n/b\n",
            "0\n/b\n",
            "12\nb\n",
            "12\n/b\nmore\n",
        ] {
            assert_eq!(parse_pid_file(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn executable_of_this_process_is_the_test_binary() {
        let me = std::env::current_exe().unwrap().canonicalize().unwrap();
        assert_eq!(executable(std::process::id()), Some(me));
        assert_eq!(executable(u32::MAX >> 2), None);
    }
}
