//! Plugin-side blob handles (PLAN section 8.3).
//!
//! Attachments never travel inside messages. A plugin stores an inbound attachment with
//! [`Blobs::put`] and puts the returned [`MediaRef`] in [`InboundMessage::attachments`]; the core
//! fetches the bytes with `GetBlob` and frees them with `ReleaseBlob`. In the other direction, the
//! core uploads an outbound attachment with `PutBlob` before `Send`, and the channel's `send` reads
//! it with [`Blobs::open`] or [`Blobs::read`].
//!
//! Handles are the SHA-256 of the content, as lowercase hex: the same id as the core's blob store,
//! so a `MediaRef` names the same bytes on both sides. Several uploads of the same bytes share one
//! file in `outbound/`, counted: each `PutBlob` adds a reference, each `Send` that ends drops one.
//!
//! [`InboundMessage::attachments`]: giles_types::InboundMessage::attachments

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use giles_proto::types::v1::BlobRef;
use giles_types::{Error, MediaId, MediaRef, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};

use crate::dirs::{SdkDirs, remove_if_exists, sync_dir};

/// The plugin's blob store, in its data directory.
///
/// Cheap to clone. Get it from the `Configure` context.
#[derive(Debug, Clone)]
pub struct Blobs {
    dirs: Arc<SdkDirs>,
    /// References to each blob in `outbound/`: `PutBlob`s whose `Send` hasn't ended. File
    /// operations on `outbound/` happen under this lock, so a count and its file agree.
    outbound_refs: Arc<Mutex<HashMap<MediaId, usize>>>,
}

/// What a blob's `.json` sidecar holds.
#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

impl Blobs {
    pub(crate) fn new(dirs: Arc<SdkDirs>) -> Self {
        Self {
            dirs,
            outbound_refs: Arc::default(),
        }
    }

    /// Stores an inbound attachment and returns its reference, for
    /// [`InboundMessage::attachments`](giles_types::InboundMessage::attachments).
    ///
    /// The blob is on disk (synced) when this returns, so put attachments before emitting the
    /// event that refers to them. It stays until the core releases it; nothing frees it
    /// implicitly. Storing the same bytes twice gives the same id; the last media type and name
    /// win.
    pub async fn put(
        &self,
        bytes: impl AsRef<[u8]>,
        media_type: impl Into<String>,
        name: Option<String>,
    ) -> Result<MediaRef> {
        self.put_reader(bytes.as_ref(), media_type, name).await
    }

    /// Like [`Blobs::put`], streaming the bytes from `reader`.
    pub async fn put_reader(
        &self,
        mut reader: impl AsyncRead + Unpin,
        media_type: impl Into<String>,
        name: Option<String>,
    ) -> Result<MediaRef> {
        let mut writer = BlobWriter::create(&self.dirs).await?;
        let mut buf = vec![0; 64 * 1024];
        loop {
            let len = reader.read(&mut buf).await.map_err(Error::other)?;
            if len == 0 {
                break;
            }
            writer.write(&buf[..len]).await?;
        }
        let media = MediaRef {
            id: writer.id(),
            media_type: media_type.into(),
            size: writer.size(),
            name,
        };
        let meta = serde_json::to_vec(&Meta {
            media_type: media.media_type.clone(),
            name: media.name.clone(),
        })
        .map_err(Error::other)?;
        let (dirs, id) = (Arc::clone(&self.dirs), media.id.clone());
        // The sidecar first: a blob file without one never exists.
        tokio::task::spawn_blocking(move || {
            let tmp = dirs.tmp_path();
            write_synced(&tmp, &meta)?;
            std::fs::rename(&tmp, meta_path(&dirs.blobs, &id))
        })
        .await
        .map_err(Error::other)?
        .map_err(Error::other)?;
        writer
            .commit(&self.dirs.blobs.join(media.id.as_str()))
            .await?;
        Ok(media)
    }

    /// Opens a blob: one the core put for the current `Send`, or one the plugin put.
    ///
    /// A blob the core put is valid until the `Send` that uses it returns.
    pub async fn open(&self, media: &MediaRef) -> Result<tokio::fs::File> {
        let path = self.path(media).await?;
        tokio::fs::File::open(&path)
            .await
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => not_found(&media.id),
                _ => Error::other(e),
            })
    }

    /// Reads a whole blob into memory. See [`Blobs::open`].
    pub async fn read(&self, media: &MediaRef) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open(media)
            .await?
            .read_to_end(&mut bytes)
            .await
            .map_err(Error::other)?;
        Ok(bytes)
    }

    /// The path of a blob's file, for libraries that want one. See [`Blobs::open`]. Don't modify
    /// or delete the file.
    pub async fn path(&self, media: &MediaRef) -> Result<PathBuf> {
        for dir in [&self.dirs.outbound, &self.dirs.blobs] {
            let path = dir.join(media.id.as_str());
            if tokio::fs::try_exists(&path).await.map_err(Error::other)? {
                return Ok(path);
            }
        }
        Err(not_found(&media.id))
    }

    /// Starts receiving a blob from the core (`PutBlob`).
    pub(crate) async fn writer(&self) -> Result<BlobWriter> {
        BlobWriter::create(&self.dirs).await
    }

    /// A blob the plugin put, for `GetBlob`: its reference and open file. `None` if there's no
    /// such blob (never put, or released).
    pub(crate) async fn inbound(
        &self,
        id: &MediaId,
    ) -> io::Result<Option<(BlobRef, tokio::fs::File)>> {
        let file = match tokio::fs::File::open(self.dirs.blobs.join(id.as_str())).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let size = file.metadata().await?.len();
        // The sidecar can be missing only if a release raced a new put of the same bytes; the
        // event that refers to the blob carries its media type anyway.
        let meta = match tokio::fs::read(meta_path(&self.dirs.blobs, id)).await {
            Ok(meta) => serde_json::from_slice(&meta).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Meta {
                media_type: "application/octet-stream".into(),
                name: None,
            },
            Err(e) => return Err(e),
        };
        let blob = BlobRef {
            id: id.to_string(),
            media_type: meta.media_type,
            size,
            name: meta.name,
        };
        Ok(Some((blob, file)))
    }

    /// Whether a blob exists, in either direction.
    pub(crate) async fn exists(&self, id: &MediaId) -> io::Result<bool> {
        for dir in [&self.dirs.outbound, &self.dirs.blobs] {
            if tokio::fs::try_exists(dir.join(id.as_str())).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Frees a blob in both directions (`ReleaseBlob`), outstanding references to a blob the core
    /// put included: the core said it's done with it. An unknown id is fine.
    pub(crate) async fn release(&self, id: &MediaId) -> io::Result<()> {
        let (dirs, refs) = (Arc::clone(&self.dirs), Arc::clone(&self.outbound_refs));
        let id = id.clone();
        tokio::task::spawn_blocking(move || {
            // The blob before its sidecar, the reverse of `put_reader`.
            remove_if_exists(&dirs.blobs.join(id.as_str()))?;
            remove_if_exists(&meta_path(&dirs.blobs, &id))?;
            let mut refs = lock(&refs);
            refs.remove(&id);
            remove_if_exists(&dirs.outbound.join(id.as_str()))
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Drops one reference to each of `ids` (distinct handles from the core's `PutBlob`) when the
    /// `Send` that used them has ended, deleting a blob when its last reference goes. Ids without
    /// references (the plugin's own blobs, or released ones) are left alone. Runs in the
    /// background; the handle is for tests.
    pub(crate) fn drop_outbound(&self, ids: Vec<MediaId>) -> tokio::task::JoinHandle<()> {
        let (dirs, refs) = (Arc::clone(&self.dirs), Arc::clone(&self.outbound_refs));
        tokio::task::spawn_blocking(move || {
            let mut refs = lock(&refs);
            for id in ids {
                let Some(count) = refs.get_mut(&id) else {
                    continue;
                };
                *count -= 1;
                if *count > 0 {
                    continue;
                }
                refs.remove(&id);
                if let Err(e) = remove_if_exists(&dirs.outbound.join(id.as_str())) {
                    tracing::warn!(%id, "deleting an outbound blob: {e}");
                }
            }
        })
    }
}

fn lock(
    refs: &Mutex<HashMap<MediaId, usize>>,
) -> std::sync::MutexGuard<'_, HashMap<MediaId, usize>> {
    refs.lock().expect("never poisoned")
}

fn meta_path(dir: &Path, id: &MediaId) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn not_found(id: &MediaId) -> Error {
    Error::NotFound(format!("blob {id}"))
}

/// Writes a new file and syncs it.
pub(crate) fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create_new(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// A blob being written to a temporary file, hashed as it goes. Dropped without
/// [`commit`](BlobWriter::commit), it deletes the temporary file.
pub(crate) struct BlobWriter {
    file: tokio::fs::File,
    path: PathBuf,
    hasher: Sha256,
    size: u64,
    committed: bool,
}

impl BlobWriter {
    async fn create(dirs: &SdkDirs) -> Result<Self> {
        let path = dirs.tmp_path();
        let file = tokio::fs::File::create_new(&path)
            .await
            .map_err(Error::other)?;
        Ok(Self {
            file,
            path,
            hasher: Sha256::new(),
            size: 0,
            committed: false,
        })
    }

    pub(crate) async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.file.write_all(data).await.map_err(Error::other)?;
        self.hasher.update(data);
        self.size += u64::try_from(data.len()).expect("fits in u64");
        Ok(())
    }

    /// Bytes written so far.
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    /// The id of the bytes written so far.
    pub(crate) fn id(&self) -> MediaId {
        MediaId::from_digest(self.hasher.clone().finalize().into())
    }

    /// Syncs the blob, moves it to `dest`, and syncs the rename.
    async fn commit(mut self, dest: &Path) -> Result<()> {
        self.file.flush().await.map_err(Error::other)?;
        self.file.sync_all().await.map_err(Error::other)?;
        tokio::fs::rename(&self.path, dest)
            .await
            .map_err(Error::other)?;
        self.committed = true;
        let dir = dest.parent().expect("blob paths have a parent").to_owned();
        tokio::task::spawn_blocking(move || sync_dir(&dir))
            .await
            .map_err(Error::other)?
            .map_err(Error::other)
    }

    /// Moves a blob the core put into `outbound/`, adds a reference to it, and returns its id. Not
    /// synced: it only lives until the `Send` that uses it ends.
    pub(crate) async fn commit_outbound(mut self, blobs: &Blobs) -> Result<MediaId> {
        let id = self.id();
        self.file.flush().await.map_err(Error::other)?;
        let (tmp, dest) = (self.path.clone(), blobs.dirs.outbound.join(id.as_str()));
        let (refs, counted) = (Arc::clone(&blobs.outbound_refs), id.clone());
        // Under the lock, so a `Send` dropping its reference can't delete the file in between.
        tokio::task::spawn_blocking(move || {
            let mut refs = lock(&refs);
            std::fs::rename(tmp, dest)?;
            *refs.entry(counted).or_default() += 1;
            Ok::<_, io::Error>(())
        })
        .await
        .map_err(Error::other)?
        .map_err(Error::other)?;
        self.committed = true;
        Ok(id)
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn store() -> (tempfile::TempDir, Blobs) {
        let data = tempfile::tempdir().unwrap();
        let dirs = SdkDirs::open(data.path()).unwrap();
        (data, Blobs::new(Arc::new(dirs)))
    }

    #[tokio::test]
    async fn put_returns_a_content_addressed_reference_that_survives_a_restart() {
        let (data, blobs) = store();
        let media = blobs
            .put("hello", "text/plain", Some("hi.txt".into()))
            .await
            .unwrap();
        assert_eq!(media.id.as_str(), HELLO_SHA256);
        assert_eq!(media.size, 5);
        assert_eq!(media.media_type, "text/plain");
        assert_eq!(media.name.as_deref(), Some("hi.txt"));
        assert_eq!(blobs.read(&media).await.unwrap(), b"hello");

        // Reopening (a plugin restart) keeps inbound blobs, sidecar included.
        let blobs = Blobs::new(Arc::new(SdkDirs::open(data.path()).unwrap()));
        let (blob, _file) = blobs.inbound(&media.id).await.unwrap().unwrap();
        assert_eq!(
            blob,
            BlobRef {
                id: HELLO_SHA256.into(),
                media_type: "text/plain".into(),
                size: 5,
                name: Some("hi.txt".into()),
            }
        );
        assert!(blobs.path(&media).await.unwrap().ends_with(HELLO_SHA256));
    }

    #[tokio::test]
    async fn a_blob_without_its_sidecar_is_still_served() {
        let (_data, blobs) = store();
        let media = blobs.put("hello", "text/plain", None).await.unwrap();
        std::fs::remove_file(meta_path(&blobs.dirs.blobs, &media.id)).unwrap();
        let (blob, _file) = blobs.inbound(&media.id).await.unwrap().unwrap();
        assert_eq!(blob.media_type, "application/octet-stream");
        assert_eq!(blob.size, 5);
    }

    #[tokio::test]
    async fn same_bytes_share_an_id_and_the_last_metadata_wins() {
        let (_data, blobs) = store();
        let first = blobs.put("hello", "text/plain", None).await.unwrap();
        let second = blobs
            .put_reader(&b"hello"[..], "text/markdown", Some("a.md".into()))
            .await
            .unwrap();
        assert_eq!(first.id, second.id);
        let (blob, _) = blobs.inbound(&first.id).await.unwrap().unwrap();
        assert_eq!(blob.media_type, "text/markdown");
        assert_eq!(blob.name.as_deref(), Some("a.md"));
    }

    #[tokio::test]
    async fn release_frees_both_directions_and_is_idempotent() {
        let (_data, blobs) = store();
        let media = blobs.put("hello", "text/plain", None).await.unwrap();
        let mut writer = blobs.writer().await.unwrap();
        writer.write(b"hello").await.unwrap();
        assert_eq!(writer.commit_outbound(&blobs).await.unwrap(), media.id);

        blobs.release(&media.id).await.unwrap();
        assert!(blobs.inbound(&media.id).await.unwrap().is_none());
        assert!(!blobs.exists(&media.id).await.unwrap());
        assert!(matches!(blobs.read(&media).await, Err(Error::NotFound(_))));
        blobs.release(&media.id).await.unwrap();
        let left = std::fs::read_dir(blobs.dirs.blobs.as_path())
            .unwrap()
            .count();
        assert_eq!(left, 0, "no sidecar left behind");
    }

    #[tokio::test]
    async fn outbound_blobs_are_readable_until_dropped_and_not_served_back() {
        let (_data, blobs) = store();
        let mut writer = blobs.writer().await.unwrap();
        writer.write(b"hel").await.unwrap();
        writer.write(b"lo").await.unwrap();
        assert_eq!(writer.size(), 5);
        let id = writer.commit_outbound(&blobs).await.unwrap();
        assert_eq!(id.as_str(), HELLO_SHA256);
        let media = MediaRef {
            id: id.clone(),
            media_type: "text/plain".into(),
            size: 5,
            name: None,
        };
        assert_eq!(blobs.read(&media).await.unwrap(), b"hello");
        assert!(blobs.exists(&id).await.unwrap());
        // GetBlob serves only blobs the plugin put.
        assert!(blobs.inbound(&id).await.unwrap().is_none());

        blobs.drop_outbound(vec![id.clone()]);
        for _ in 0..100 {
            if !blobs.exists(&id).await.unwrap() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the outbound blob was not deleted");
    }

    async fn put_outbound(blobs: &Blobs, bytes: &[u8]) -> MediaId {
        let mut writer = blobs.writer().await.unwrap();
        writer.write(bytes).await.unwrap();
        writer.commit_outbound(blobs).await.unwrap()
    }

    #[tokio::test]
    async fn outbound_blobs_are_counted_per_upload() {
        let (_data, blobs) = store();
        let id = put_outbound(&blobs, b"hello").await;
        assert_eq!(put_outbound(&blobs, b"hello").await, id);

        blobs.drop_outbound(vec![id.clone()]).await.unwrap();
        assert!(blobs.exists(&id).await.unwrap(), "one upload still uses it");
        blobs.drop_outbound(vec![id.clone()]).await.unwrap();
        assert!(!blobs.exists(&id).await.unwrap());
        // Uncounted: dropping again, or dropping the plugin's own blob, does nothing.
        blobs.drop_outbound(vec![id.clone()]).await.unwrap();
        let own = blobs.put("mine", "text/plain", None).await.unwrap();
        blobs.drop_outbound(vec![own.id.clone()]).await.unwrap();
        assert!(blobs.inbound(&own.id).await.unwrap().is_some());

        // A release drops every reference at once, and a later upload starts counting afresh.
        put_outbound(&blobs, b"hello").await;
        put_outbound(&blobs, b"hello").await;
        blobs.release(&id).await.unwrap();
        assert!(!blobs.exists(&id).await.unwrap());
        put_outbound(&blobs, b"hello").await;
        blobs.drop_outbound(vec![id.clone()]).await.unwrap();
        assert!(!blobs.exists(&id).await.unwrap());
    }

    #[tokio::test]
    async fn an_abandoned_writer_leaves_nothing_behind() {
        let (data, blobs) = store();
        let mut writer = blobs.writer().await.unwrap();
        writer.write(b"partial").await.unwrap();
        drop(writer);
        let tmp = data.path().join("giles-sdk/tmp");
        assert_eq!(std::fs::read_dir(tmp).unwrap().count(), 0);
    }
}
