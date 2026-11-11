//! `read` and `write`: whole-file access.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

use serde_json::json;

use crate::find::write_failed;
use crate::{Failure, Outcome};

/// Prints `{"size":N}` and a newline, then up to `max` bytes of the file.
pub fn read(path: &OsString, max: u64) -> Outcome<()> {
    let path = Path::new(path);
    let mut file = std::fs::File::open(path).map_err(|e| Failure::io(path, &e))?;
    let metadata = file.metadata().map_err(|e| Failure::io(path, &e))?;
    if metadata.is_dir() {
        return Err(Failure::other(format!(
            "{}: is a directory",
            path.display()
        )));
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", json!({ "size": metadata.len() })).map_err(write_failed)?;
    std::io::copy(&mut (&mut file).take(max), &mut out).map_err(|e| Failure::io(path, &e))?;
    out.flush().map_err(write_failed)
}

/// Writes standard input to the file, creating missing parent directories. An existing file keeps
/// its permissions (and a symlink is followed); a new one gets the default for the process's umask.
pub fn write(path: &OsString) -> Outcome<()> {
    let path = Path::new(path);
    let mut content = Vec::new();
    std::io::stdin()
        .lock()
        .read_to_end(&mut content)
        .map_err(|e| Failure::other(format!("reading standard input: {e}")))?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Failure::io(parent, &e))?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| Failure::io(path, &e))?;
    file.write_all(&content).map_err(|e| Failure::io(path, &e))
}

/// Prints `{"free_bytes":N}`: the space available to unprivileged users where `path` is.
pub fn free_space(path: &OsString) -> Outcome<()> {
    let path = Path::new(path);
    let stat = rustix::fs::statvfs(path)
        .map_err(|e| Failure::io(path, &std::io::Error::from_raw_os_error(e.raw_os_error())))?;
    let free = stat.f_bavail.saturating_mul(stat.f_frsize);
    writeln!(std::io::stdout(), "{}", json!({ "free_bytes": free })).map_err(write_failed)
}
