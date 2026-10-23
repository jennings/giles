//! The files the core hands a stdio server for `{"$media": "<id>"}` inputs (PLAN section 7.10):
//! copies of blobs in the server's `TMPDIR`, removed when the call ends.
//!
//! The server can write its `TMPDIR` and the data directory holding it, so it could put a link
//! where the core is about to write. Every operation therefore goes through one descriptor of the
//! directory, opened without following a link in its place: files are created in it with
//! `openat(O_CREAT | O_EXCL | O_NOFOLLOW)` and removed with `unlinkat`, which never follow a link
//! either. The worst a server can do is make its own copy fail.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use giles_store::BlobStore;
use giles_types::{Error, MediaId, MediaRef, Result};
use tokio::io::AsyncWriteExt as _;

use crate::media;

/// What every file the core hands over is named after: `giles-media-<random>-<name>`.
pub(crate) const FILE_PREFIX: &str = "giles-media-";

/// The longest part of a file name taken from the media's name, in bytes (all ASCII).
const MAX_NAME_LEN: usize = 100;

/// A stdio server's `TMPDIR`, where media inputs are copied.
#[derive(Debug, Clone)]
pub(crate) struct MediaDir {
    path: PathBuf,
}

impl MediaDir {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The directory itself, refusing a link in its place.
    fn open(&self) -> io::Result<File> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&self.path)
    }

    /// Removes every file an earlier run left behind (a crash before a call ended). Blocking. A
    /// missing directory has none.
    pub(crate) fn sweep(&self) -> io::Result<usize> {
        let dir = match self.open() {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut removed = 0;
        // The listing goes by path, but removal by the descriptor: if the server swapped the
        // directory since, only entries in the directory opened are removed.
        for entry in std::fs::read_dir(&self.path)? {
            let name = entry?.file_name();
            if name.as_bytes().starts_with(FILE_PREFIX.as_bytes()) {
                unlink_at(&dir, &name)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Copies each of `media` from `blobs` into a new file of its own and returns where each
    /// went. The files are removed when the returned [`HandedFiles`] is dropped, and so are those
    /// already made if a copy fails.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if a blob isn't stored, [`Error::Other`] if it can't be read or
    /// copied.
    pub(crate) async fn hand_over(
        &self,
        blobs: &BlobStore,
        media: &[MediaRef],
    ) -> Result<HandedFiles> {
        let opened = self.clone();
        let dir = tokio::task::spawn_blocking(move || opened.open())
            .await
            .map_err(Error::other)?
            .map_err(|error| {
                Error::other(format!(
                    "cannot open the server's temporary directory: {error}"
                ))
            })?;
        let mut handed = HandedFiles {
            dir: Arc::new(dir),
            names: Vec::with_capacity(media.len()),
            paths: HashMap::with_capacity(media.len()),
        };
        for item in media {
            let path = self.copy(blobs, item, &mut handed).await?;
            handed.paths.insert(item.id.clone(), path);
        }
        Ok(handed)
    }

    async fn copy(
        &self,
        blobs: &BlobStore,
        item: &MediaRef,
        handed: &mut HandedFiles,
    ) -> Result<PathBuf> {
        let failed = |error: &dyn std::fmt::Display| {
            Error::other(format!(
                "cannot hand media {} to the server: {error}",
                item.id
            ))
        };
        let mut blob = blobs
            .get(&item.id)
            .await
            .map_err(|error| failed(&error))?
            .ok_or_else(|| Error::NotFound(format!("media {} is not stored", item.id)))?;
        let name = file_name(item, &random_tag().map_err(|error| failed(&error))?);
        let dir = handed.dir.clone();
        let c_name = CString::new(name.as_bytes()).map_err(|error| failed(&error))?;
        let created = tokio::task::spawn_blocking(move || create_at(&dir, &c_name))
            .await
            .map_err(Error::other)?
            .map_err(|error| failed(&error))?;
        // Registered before the copy, so a failed copy is removed too.
        handed.names.push(name.clone().into());
        let mut file = tokio::fs::File::from_std(created);
        // The blob reader checks the content's hash at the end.
        tokio::io::copy(&mut blob.reader, &mut file)
            .await
            .map_err(|error| failed(&error))?;
        file.flush().await.map_err(|error| failed(&error))?;
        Ok(self.path.join(name))
    }
}

/// Media inputs of one call as files in the server's `TMPDIR`. Dropping it removes them.
#[derive(Debug)]
pub(crate) struct HandedFiles {
    dir: Arc<File>,
    names: Vec<std::ffi::OsString>,
    paths: HashMap<MediaId, PathBuf>,
}

impl HandedFiles {
    /// Where the media with `id` went.
    pub(crate) fn path(&self, id: &MediaId) -> Option<&Path> {
        self.paths.get(id).map(PathBuf::as_path)
    }
}

impl Drop for HandedFiles {
    fn drop(&mut self) {
        for name in &self.names {
            if let Err(error) = unlink_at(&self.dir, name)
                && error.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(
                    file = %name.to_string_lossy(),
                    %error,
                    "cannot remove a media file handed to an MCP server"
                );
            }
        }
    }
}

/// 16 random hex digits, so names can't be guessed ahead of the copy.
fn random_tag() -> io::Result<String> {
    use std::fmt::Write as _;
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let mut tag = String::with_capacity(2 * bytes.len());
    for byte in bytes {
        let _ = write!(tag, "{byte:02x}");
    }
    Ok(tag)
}

/// The file name for `item`: [`FILE_PREFIX`], `tag`, and a safe form of the media's name (its
/// last path component, with only ASCII letters, digits, `.`, `-`, and `_`, other characters as
/// `_`, cut to [`MAX_NAME_LEN`] bytes keeping a short extension), `media` without one. A name
/// without an extension gets the media type's, if Giles knows one, since servers often go by it.
fn file_name(item: &MediaRef, tag: &str) -> String {
    let given = item
        .name
        .as_deref()
        .and_then(|name| name.rsplit(['/', '\\']).next())
        .unwrap_or_default();
    let mut name: String = given
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.trim_matches(['.', '_']).is_empty() {
        name = "media".to_owned();
    }
    let extension = match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= 17 => Some(dot),
        _ => None,
    };
    match extension {
        None => {
            name.truncate(MAX_NAME_LEN);
            if let Some(extension) = media::extension_for(&item.media_type) {
                name.push('.');
                name.push_str(extension);
            }
        }
        Some(dot) if name.len() > MAX_NAME_LEN => {
            let extension = name.split_off(dot);
            name.truncate(MAX_NAME_LEN - extension.len());
            name.push_str(&extension);
        }
        Some(_) => {}
    }
    format!("{FILE_PREFIX}{tag}-{name}")
}

/// Creates `name` in `dir` for writing, mode 0600; an existing entry (a link included) is
/// refused.
fn create_at(dir: &File, name: &CString) -> io::Result<File> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `dir` is an open descriptor and `name` a NUL-terminated string, both alive for the
    // call; the mode is passed as the variadic `c_uint` `openat` reads with `O_CREAT`. On
    // success the returned descriptor is new and owned by nothing else, so `OwnedFd` may take
    // it.
    #[allow(unsafe_code)]
    let fd = unsafe {
        let fd = libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, 0o600 as libc::c_uint);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        OwnedFd::from_raw_fd(fd)
    };
    Ok(File::from(fd))
}

/// Removes the entry `name` of `dir` (a link itself, never its target).
fn unlink_at(dir: &File, name: &OsStr) -> io::Result<()> {
    let name = CString::new(name.as_bytes()).map_err(io::Error::other)?;
    // SAFETY: `dir` is an open descriptor and `name` a NUL-terminated string, both alive for the
    // call.
    #[allow(unsafe_code)]
    let result = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use giles_store::{BlobMeta, Store};
    use giles_types::Timestamp;

    use super::*;

    fn item(name: Option<&str>, media_type: &str) -> MediaRef {
        MediaRef {
            id: MediaId::from_digest([1; 32]),
            media_type: media_type.to_owned(),
            size: 1,
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn file_names_are_safe_and_keep_the_extension() {
        let name = |name: Option<&str>, media_type: &str| file_name(&item(name, media_type), "t");
        assert_eq!(
            name(Some("q3.pdf"), "application/pdf"),
            "giles-media-t-q3.pdf"
        );
        assert_eq!(
            name(Some("../../etc/pass wd"), "text/plain"),
            "giles-media-t-pass_wd.txt"
        );
        assert_eq!(
            name(Some("C:\\x\\a b.PNG"), "image/png"),
            "giles-media-t-a_b.PNG"
        );
        assert_eq!(name(Some("voice"), "audio/ogg"), "giles-media-t-voice.ogg");
        assert_eq!(name(None, "image/jpeg"), "giles-media-t-media.jpeg");
        assert_eq!(
            name(Some(".."), "application/x-what"),
            "giles-media-t-media"
        );
        assert_eq!(
            name(Some("café.txt"), "text/plain"),
            "giles-media-t-caf_.txt"
        );
        let long = format!("{}.tar", "x".repeat(300));
        let cut = name(Some(&long), "application/x-tar");
        assert_eq!(cut, format!("giles-media-t-{}.tar", "x".repeat(96)));
        let long = "y".repeat(300);
        assert_eq!(
            name(Some(&long), "text/plain"),
            format!("giles-media-t-{}.txt", "y".repeat(100))
        );
    }

    async fn blobs(dir: &Path) -> BlobStore {
        let db = Store::open(dir.join("giles.db")).await.unwrap();
        BlobStore::open(dir.join("blobs"), db).await.unwrap()
    }

    async fn put(blobs: &BlobStore, bytes: &'static [u8], name: &str) -> MediaRef {
        let meta = BlobMeta {
            media_type: "text/plain".to_owned(),
            name: Some(name.to_owned()),
        };
        blobs
            .put(bytes, 1024, meta, Timestamp::UNIX_EPOCH)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn media_is_copied_in_and_removed_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = blobs(tmp.path()).await;
        let notes = put(&blobs, b"the notes", "notes.txt").await;
        let todo = put(&blobs, b"the list", "todo.txt").await;
        let dir = tmp.path().join("server/tmp");
        std::fs::create_dir_all(&dir).unwrap();
        let media_dir = MediaDir::new(dir.clone());

        let handed = media_dir
            .hand_over(&blobs, &[notes.clone(), todo.clone()])
            .await
            .unwrap();
        let path = handed.path(&notes.id).unwrap().to_owned();
        assert_eq!(path.parent(), Some(dir.as_path()));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with(FILE_PREFIX) && name.ends_with("-notes.txt"),
            "{name}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"the notes");
        assert_eq!(
            std::fs::read(handed.path(&todo.id).unwrap()).unwrap(),
            b"the list"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        drop(handed);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_missing_blob_fails_and_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = blobs(tmp.path()).await;
        let notes = put(&blobs, b"the notes", "notes.txt").await;
        let missing = MediaRef {
            id: MediaId::from_digest([7; 32]),
            ..notes.clone()
        };
        let dir = tmp.path().join("tmp");
        std::fs::create_dir(&dir).unwrap();
        let result = MediaDir::new(dir.clone())
            .hand_over(&blobs, &[notes, missing])
            .await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_link_in_place_of_the_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = blobs(tmp.path()).await;
        let notes = put(&blobs, b"the notes", "notes.txt").await;
        let victim = tmp.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        let dir = tmp.path().join("tmp");
        symlink(&victim, &dir).unwrap();
        let media_dir = MediaDir::new(dir);
        let result = media_dir.hand_over(&blobs, &[notes]).await;
        assert!(matches!(result, Err(Error::Other(_))), "{result:?}");
        assert!(media_dir.sweep().is_err());
        assert_eq!(std::fs::read_dir(&victim).unwrap().count(), 0);
    }

    #[test]
    fn a_sweep_removes_only_handed_files_and_never_follows_links() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tmp");
        std::fs::create_dir(&dir).unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "keep").unwrap();
        std::fs::write(dir.join("giles-media-1-old.txt"), "old").unwrap();
        symlink(&outside, dir.join("giles-media-2-link")).unwrap();
        std::fs::write(dir.join("server-file"), "mine").unwrap();
        let media_dir = MediaDir::new(dir.clone());
        assert_eq!(media_dir.sweep().unwrap(), 2);
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["server-file"]);
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(MediaDir::new(tmp.path().join("nope")).sweep().unwrap(), 0);
    }
}
