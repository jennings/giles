//! One core per data root: an exclusive `flock` on `run/giles.lock`.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use crate::paths::DataRoot;

/// The data root's single-instance lock, held for the life of the core. The kernel releases it
/// when the process exits, however it exits, so a crash never leaves a stale lock. The file
/// descriptor is close-on-exec, so plugins don't inherit it.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

/// The lock couldn't be taken.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockError {
    /// Another core holds it.
    #[error(
        "giles is already running with the data root {}{} (it holds {}); stop it first",
        .root.display(),
        .pid.map(|pid| format!(" as pid {pid}")).unwrap_or_default(),
        .path.display(),
    )]
    Held {
        /// The data root.
        root: PathBuf,
        /// The lock file.
        path: PathBuf,
        /// The pid the other core wrote, if it could be read.
        pid: Option<u32>,
    },
    /// The lock file couldn't be opened, locked, or written.
    #[error("cannot lock {}: {source}", .path.display())]
    Io {
        /// The lock file.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
}

impl InstanceLock {
    /// Takes the exclusive lock on [`DataRoot::lock_file`] (mode 0600) without waiting and
    /// writes this process's pid into it. `run/` must exist.
    ///
    /// # Errors
    ///
    /// [`LockError::Held`] if another process holds it; [`LockError::Io`] otherwise.
    pub fn acquire(root: &DataRoot) -> Result<Self, LockError> {
        let path = root.lock_file();
        let io_error = |source| LockError::Io {
            path: path.clone(),
            source,
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(io_error)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let pid = fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.trim().parse().ok());
                return Err(LockError::Held {
                    root: root.root().to_owned(),
                    path,
                    pid,
                });
            }
            Err(TryLockError::Error(source)) => return Err(io_error(source)),
        }
        file.set_len(0).map_err(io_error)?;
        writeln!(file, "{}", std::process::id()).map_err(io_error)?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    #[test]
    fn a_second_lock_fails_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
        fs::create_dir(root.run_dir()).unwrap();

        let first = InstanceLock::acquire(&root).unwrap();
        let mode = fs::metadata(root.lock_file()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let pid = std::process::id();
        assert_eq!(
            fs::read_to_string(root.lock_file()).unwrap(),
            format!("{pid}\n")
        );

        // flock locks belong to the open file, so a second open in this process conflicts too.
        let err = InstanceLock::acquire(&root).unwrap_err();
        assert!(
            matches!(err, LockError::Held { pid: Some(p), .. } if p == pid),
            "{err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("already running"), "{message}");
        assert!(message.contains(&format!("pid {pid}")), "{message}");

        drop(first);
        let _again = InstanceLock::acquire(&root).unwrap();
    }

    #[test]
    fn a_missing_run_dir_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::new(dir.path().join("none"), Path::new("/tmp")).unwrap();
        assert!(matches!(
            InstanceLock::acquire(&root),
            Err(LockError::Io { .. })
        ));
    }
}
