//! The core's side of the blob store (PLAN section 11.5): attachment bytes for built-in
//! providers.

use async_trait::async_trait;
use bytes::Bytes;
use giles_store::{Blob, BlobStore};
use giles_types::{Error, MediaRef, Result};
use tokio::io::AsyncReadExt;

use crate::providers::MediaSource;

/// A [`MediaSource`] over the blob store. Each read loads the whole blob into memory, at most
/// the size recorded when it was stored, and verifies its SHA-256.
#[derive(Debug, Clone)]
pub struct BlobMedia {
    blobs: BlobStore,
}

impl BlobMedia {
    /// Reads from `blobs`.
    pub fn new(blobs: BlobStore) -> Self {
        Self { blobs }
    }
}

#[async_trait]
impl MediaSource for BlobMedia {
    async fn read(&self, media: &MediaRef) -> Result<Bytes> {
        let blob = self
            .blobs
            .get(&media.id)
            .await
            .map_err(Error::other)?
            .ok_or_else(|| Error::NotFound(format!("blob {}", media.id)))?;
        read_blob(blob, media).await
    }
}

/// Reads all of `blob`, which must be exactly its recorded size.
async fn read_blob(blob: Blob, media: &MediaRef) -> Result<Bytes> {
    let size = blob.media.size;
    let too_large = || Error::other(format!("blob {} is larger than its {size} bytes", media.id));
    let mut bytes = Vec::with_capacity(usize::try_from(size).map_err(|_| too_large())?);
    // One byte past the size: enough to reach end of file (where the reader checks the hash) or
    // to see that the file grew, without reading an unbounded amount.
    blob.reader
        .take(size.saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(Error::other)?;
    if bytes.len() as u64 != size {
        return Err(too_large());
    }
    Ok(Bytes::from(bytes))
}

#[cfg(test)]
mod tests {
    use giles_store::{BlobMeta, Store};
    use giles_types::{MediaId, Timestamp};

    use super::*;

    async fn open() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(dir.path().join("giles.db")).await.unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs"), db).await.unwrap();
        (dir, blobs)
    }

    fn meta() -> BlobMeta {
        BlobMeta {
            media_type: "image/png".into(),
            name: None,
        }
    }

    fn blob_file(dir: &tempfile::TempDir, id: &MediaId) -> std::path::PathBuf {
        let id = id.as_str();
        dir.path().join("blobs/sha256").join(&id[..2]).join(id)
    }

    #[tokio::test]
    async fn reads_stored_bytes() {
        let (_dir, blobs) = open().await;
        let media = blobs
            .put(&b"png bytes"[..], 1024, meta(), Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        let bytes = BlobMedia::new(blobs).read(&media).await.unwrap();
        assert_eq!(&bytes[..], b"png bytes");
    }

    #[tokio::test]
    async fn missing_blobs_are_not_found() {
        let (_dir, blobs) = open().await;
        let media = MediaRef {
            id: MediaId::from_digest([7; 32]),
            media_type: "image/png".into(),
            size: 3,
            name: None,
        };
        let err = BlobMedia::new(blobs).read(&media).await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }

    #[tokio::test]
    async fn damaged_content_is_an_error() {
        let (dir, blobs) = open().await;
        let media = blobs
            .put(&b"original"[..], 1024, meta(), Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        // Same length, different bytes: only the hash check catches it.
        std::fs::write(blob_file(&dir, &media.id), b"tampered").unwrap();
        let err = BlobMedia::new(blobs).read(&media).await.unwrap_err();
        assert!(err.to_string().contains("SHA-256"), "{err}");
    }

    #[tokio::test]
    async fn a_blob_longer_than_its_recorded_size_is_an_error() {
        let (dir, blobs) = open().await;
        let media = blobs
            .put(&b"original"[..], 1024, meta(), Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        let blob = blobs.get(&media.id).await.unwrap().unwrap();
        // The size check in `get` has passed; the file grows before it is read.
        std::fs::OpenOptions::new()
            .append(true)
            .open(blob_file(&dir, &media.id))
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"!"))
            .unwrap();
        let err = read_blob(blob, &media).await.unwrap_err();
        assert!(err.to_string().contains("larger than its 8 bytes"), "{err}");
    }
}
