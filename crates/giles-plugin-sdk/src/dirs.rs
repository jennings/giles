//! The SDK's own state inside the plugin's data directory.
//!
//! ```text
//! <GILES_PLUGIN_DATA>/giles-sdk/
//!   blobs/<sha256>       blobs the plugin put, until the core releases them
//!   blobs/<sha256>.json  their media type and file name
//!   outbound/<sha256>    blobs the core put, until the Send that uses them ends
//!   events/<seq>.pb      events the core hasn't acked yet
//!   tmp/                 partial writes
//! ```

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The name of the SDK's directory inside the data directory. Plugin authors keep out of it.
pub(crate) const SDK_DIR: &str = "giles-sdk";

/// Paths of the SDK's state, created by [`SdkDirs::open`].
#[derive(Debug)]
pub(crate) struct SdkDirs {
    pub(crate) blobs: PathBuf,
    pub(crate) outbound: PathBuf,
    pub(crate) events: PathBuf,
    tmp: PathBuf,
    next_tmp: AtomicU64,
}

impl SdkDirs {
    /// Creates the layout under `data_dir`, and empties `outbound/` and `tmp/`: a blob the core
    /// put is valid only until the RPC using it ends, which can't outlive the process, and a
    /// temporary file left behind is an interrupted write.
    pub(crate) fn open(data_dir: &Path) -> io::Result<Self> {
        let root = data_dir.join(SDK_DIR);
        let dirs = Self {
            blobs: root.join("blobs"),
            outbound: root.join("outbound"),
            events: root.join("events"),
            tmp: root.join("tmp"),
            next_tmp: AtomicU64::new(0),
        };
        for dir in [&dirs.outbound, &dirs.tmp] {
            match std::fs::remove_dir_all(dir) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        for dir in [&dirs.blobs, &dirs.outbound, &dirs.events, &dirs.tmp] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(dirs)
    }

    /// A fresh path in `tmp/`, on the same file system as the other directories so a finished
    /// file can be renamed into place.
    pub(crate) fn tmp_path(&self) -> PathBuf {
        let n = self.next_tmp.fetch_add(1, Ordering::Relaxed);
        self.tmp.join(n.to_string())
    }
}

/// Makes the entries of `dir` (a rename or a new file) durable.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Removes a file; one that's already gone is fine.
pub(crate) fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_keeps_blobs_but_clears_transient_files() {
        let data = tempfile::tempdir().unwrap();
        let dirs = SdkDirs::open(data.path()).unwrap();
        for (dir, name) in [(&dirs.blobs, "b"), (&dirs.outbound, "o")] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let tmp = dirs.tmp_path();
        std::fs::write(&tmp, "x").unwrap();
        assert_ne!(dirs.tmp_path(), tmp);

        let dirs = SdkDirs::open(data.path()).unwrap();
        assert!(dirs.blobs.join("b").exists());
        assert!(!dirs.outbound.join("o").exists());
        assert!(dirs.outbound.is_dir());
        assert!(!tmp.exists());
        assert!(tmp.parent().unwrap().is_dir());
    }

    #[test]
    fn remove_if_exists_ignores_missing_files() {
        let data = tempfile::tempdir().unwrap();
        let path = data.path().join("f");
        remove_if_exists(&path).unwrap();
        std::fs::write(&path, "").unwrap();
        remove_if_exists(&path).unwrap();
        assert!(!path.exists());
    }
}
