//! The plugin's files in its data directory: the session store, the lock that keeps two processes
//! off it, and the marker that a logout is waiting to be cleaned up.
//!
//! ```text
//! <GILES_PLUGIN_DATA>/
//!   whatsapp.db          the session store (SQLite; also -wal and -shm while open)
//!   lock                 held exclusively while the plugin runs
//!   logged-out           present after WhatsApp logged the device out
//!   whatsapp.db.logged-out   (and -wal, -shm) the store of the logged-out session, kept aside
//!   giles-sdk/           the SDK's own state
//! ```

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use giles_plugin_sdk::giles_types::Error;

/// The session store's file name.
const STORE: &str = "whatsapp.db";
/// The lock file's name.
const LOCK: &str = "lock";
/// The marker that the last session was logged out.
const LOGGED_OUT: &str = "logged-out";
/// The suffixes of the files SQLite keeps for a database.
const SQLITE_SUFFIXES: [&str; 3] = ["", "-wal", "-shm"];

/// The data directory, locked for this process.
///
/// Two processes on one session store would both connect as the same linked device, and WhatsApp
/// would disconnect one with a "replaced" error, over and over. The lock is an `flock` on `lock`,
/// released by the operating system when the process exits, however it exits.
#[derive(Debug)]
pub(crate) struct DataDir {
    root: PathBuf,
    /// Held for the lock; never read.
    _lock: File,
}

impl DataDir {
    /// Locks `root`, which must exist.
    ///
    /// # Errors
    ///
    /// The directory can't be used, or another process holds the lock.
    pub(crate) fn open(root: &Path) -> Result<Self, Error> {
        let path = root.join(LOCK);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| Error::other(format!("opening {}: {e}", path.display())))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Error::other(format!(
                    "another process is using the WhatsApp session in {}: two clients on one \
                     session would keep replacing each other",
                    root.display()
                )));
            }
            Err(TryLockError::Error(e)) => {
                return Err(Error::other(format!("locking {}: {e}", path.display())));
            }
        }
        Ok(Self {
            root: root.to_owned(),
            _lock: lock,
        })
    }

    /// The session store's path.
    pub(crate) fn store(&self) -> PathBuf {
        self.root.join(STORE)
    }

    /// Records that WhatsApp logged the device out, so the next start links a new session.
    pub(crate) fn mark_logged_out(&self) -> io::Result<()> {
        fs::write(self.root.join(LOGGED_OUT), b"")
    }

    /// If the last session was logged out, moves its store aside (to `whatsapp.db.logged-out`,
    /// replacing an older one) and clears the marker, so the client starts without a session.
    /// Returns whether it did.
    ///
    /// The old store is useless to WhatsApp (the device is gone) but kept, not deleted: a logout
    /// the owner didn't expect is easier to look into with it.
    pub(crate) fn clear_logged_out_session(&self) -> io::Result<bool> {
        let marker = self.root.join(LOGGED_OUT);
        if !marker.try_exists()? {
            return Ok(false);
        }
        for suffix in SQLITE_SUFFIXES {
            let from = self.root.join(format!("{STORE}{suffix}"));
            let to = self.root.join(format!("{STORE}{suffix}.logged-out"));
            match fs::rename(&from, &to) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        fs::remove_file(marker)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_process_cannot_use_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDir::open(dir.path()).unwrap();
        let error = DataDir::open(dir.path()).unwrap_err().to_string();
        assert!(error.contains("another process"), "{error}");
        // Released on drop: the next start works.
        drop(first);
        DataDir::open(dir.path()).unwrap();
    }

    #[test]
    fn different_directories_do_not_conflict() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _a = DataDir::open(a.path()).unwrap();
        let _b = DataDir::open(b.path()).unwrap();
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = DataDir::open(&dir.path().join("missing")).unwrap_err();
        assert!(error.to_string().contains("opening"), "{error}");
    }

    #[test]
    fn the_store_is_in_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::open(dir.path()).unwrap();
        assert_eq!(data.store(), dir.path().join("whatsapp.db"));
    }

    #[test]
    fn a_logged_out_session_is_moved_aside_once() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::open(dir.path()).unwrap();
        // Nothing marked: nothing happens, and the store stays.
        fs::write(dir.path().join("whatsapp.db"), "session").unwrap();
        assert!(!data.clear_logged_out_session().unwrap());
        assert!(dir.path().join("whatsapp.db").exists());

        data.mark_logged_out().unwrap();
        fs::write(dir.path().join("whatsapp.db-wal"), "wal").unwrap();
        assert!(data.clear_logged_out_session().unwrap());
        assert!(!dir.path().join("whatsapp.db").exists());
        assert!(!dir.path().join("whatsapp.db-wal").exists());
        assert!(!dir.path().join("logged-out").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("whatsapp.db.logged-out")).unwrap(),
            "session"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("whatsapp.db-wal.logged-out")).unwrap(),
            "wal"
        );
        // The marker is gone, so a second start keeps whatever store it finds.
        assert!(!data.clear_logged_out_session().unwrap());
    }

    #[test]
    fn a_logout_with_no_store_just_clears_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let data = DataDir::open(dir.path()).unwrap();
        data.mark_logged_out().unwrap();
        assert!(data.clear_logged_out_session().unwrap());
        assert!(!dir.path().join("logged-out").exists());
    }
}
