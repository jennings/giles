//! The content-addressed blob store (PLAN 11.5): attachments and tool media, named by SHA-256.
//!
//! Layout under the blobs directory (`DataRoot::blobs_dir()`, `data/blobs`):
//!
//! ```text
//! sha256/
//!   tmp/               puts in progress; emptied when the store opens
//!   3f/
//!     3fa9...e1        one file per blob, named by its full hex digest
//! ```
//!
//! Files are sharded by the first two hex digits (256 directories), which keeps directories
//! small for listing and the sweep (4.2) without deep trees. Metadata (size, media type, name,
//! times) is a `blobs` row in the database, written after the file is in place.
//!
//! Contract for the sweep (task 4.2):
//!
//! - It holds [`BlobStore::lock_for_sweep`] while it picks and deletes blobs. A put holds the
//!   shared side of the same lock from just before it renames its file into place until its row
//!   is written, so the sweep never sees a put half-committed: it can't delete a file whose row
//!   is about to be written, nor a file a put has just decided to keep.
//! - A file without a row is an interrupted put (a crash, or a put cancelled between the rename
//!   and the row), and garbage only once its mtime is older than the grace period (48 hours,
//!   like unreferenced blobs, PLAN 11.5). A put always renames a fresh file into place, so the
//!   mtime is the time of the latest put.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};

use giles_types::{MediaId, MediaRef, Timestamp};
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};
use tokio::sync::{OwnedRwLockWriteGuard, RwLock};

use crate::time::to_millis;
use crate::{Error, Result, Store};

/// The read buffer for puts.
const CHUNK: usize = 64 * 1024;

/// Metadata for [`BlobStore::put`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobMeta {
    /// The media type, e.g. `image/png`.
    pub media_type: String,
    /// The file name, if the source had one.
    pub name: Option<String>,
}

/// A stored blob opened for reading: its metadata and a verifying reader.
#[derive(Debug)]
pub struct Blob {
    /// The blob's metadata as first stored.
    pub media: MediaRef,
    /// Its bytes.
    pub reader: BlobReader,
}

/// Reads a blob's file and checks its SHA-256 at the end. If the content doesn't match the id,
/// the read that reaches the end fails with [`io::ErrorKind::InvalidData`] (and so does every
/// read after it), so a consumer that reads to the end never accepts damaged data silently.
#[derive(Debug)]
pub struct BlobReader {
    file: tokio::fs::File,
    id: MediaId,
    hasher: Sha256,
    failed: bool,
    done: bool,
}

impl AsyncRead for BlobReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.failed {
            return Poll::Ready(Err(damaged_data(&this.id)));
        }
        if buf.remaining() == 0 || this.done {
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        ready!(Pin::new(&mut this.file).poll_read(cx, buf))?;
        let read = &buf.filled()[before..];
        if !read.is_empty() {
            this.hasher.update(read);
            return Poll::Ready(Ok(()));
        }
        // End of file: check the digest.
        this.done = true;
        let digest: [u8; 32] = std::mem::take(&mut this.hasher).finalize().into();
        if MediaId::from_digest(digest) != this.id {
            this.failed = true;
            return Poll::Ready(Err(damaged_data(&this.id)));
        }
        Poll::Ready(Ok(()))
    }
}

fn damaged_data(id: &MediaId) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("blob {id} does not match its SHA-256"),
    )
}

/// The blob store. Cheap to clone. Open it once per process: opening empties the temp
/// directory, which would break another instance's puts in progress.
#[derive(Debug, Clone)]
pub struct BlobStore {
    /// `<blobs dir>/sha256`.
    root: PathBuf,
    db: Store,
    /// Shared by puts while they commit, exclusive for the sweep (see the module docs).
    sweep_lock: Arc<RwLock<()>>,
}

/// Held by the sweep while it deletes blobs; puts wait to commit until it's dropped. From
/// [`BlobStore::lock_for_sweep`].
#[derive(Debug)]
pub struct SweepGuard {
    _guard: OwnedRwLockWriteGuard<()>,
}

async fn create_private_dir(path: &Path) -> io::Result<()> {
    tokio::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .await
}

/// Flushes a directory entry change (a rename into it) to disk.
async fn sync_dir(path: PathBuf) -> io::Result<()> {
    tokio::task::spawn_blocking(move || std::fs::File::open(path)?.sync_all())
        .await
        .map_err(io::Error::other)?
}

impl BlobStore {
    /// Opens the store under `blobs_dir` (normally `DataRoot::blobs_dir()`), creating
    /// `sha256/` and `sha256/tmp/` (mode 0700) if needed and deleting leftover temp files from
    /// puts a crash interrupted. `db` holds the metadata.
    pub async fn open(blobs_dir: impl Into<PathBuf>, db: Store) -> Result<Self> {
        let root = blobs_dir.into().join("sha256");
        let tmp = root.join("tmp");
        create_private_dir(&tmp).await?;
        let mut entries = tokio::fs::read_dir(&tmp).await?;
        while let Some(entry) = entries.next_entry().await? {
            tracing::debug!(path = %entry.path().display(), "removing leftover blob temp file");
            tokio::fs::remove_file(entry.path()).await?;
        }
        Ok(Self {
            root,
            db,
            sweep_lock: Arc::new(RwLock::new(())),
        })
    }

    /// Waits for puts that are committing to finish and keeps new ones from committing until
    /// the guard is dropped. Puts keep streaming into temp files meanwhile. The blob sweep
    /// (task 4.2) holds this while it decides what to delete and deletes it.
    pub async fn lock_for_sweep(&self) -> SweepGuard {
        SweepGuard {
            _guard: self.sweep_lock.clone().write_owned().await,
        }
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    fn shard_dir(&self, id: &MediaId) -> PathBuf {
        self.root.join(&id.as_str()[..2])
    }

    /// The file holding a blob.
    fn blob_path(&self, id: &MediaId) -> PathBuf {
        self.shard_dir(id).join(id.as_str())
    }

    /// Streams `reader` into the store and returns its [`MediaRef`].
    ///
    /// The bytes go to a temp file in `sha256/tmp/` while being hashed; more than `limit` bytes
    /// aborts with [`Error::TooLarge`]. The file is fsynced and renamed into place atomically,
    /// replacing any existing copy of the same content (so a damaged copy is repaired, and
    /// readers that have the old file open keep reading it). A failed or cancelled put leaves no
    /// temp file behind. `media_type` and `name` are recorded the first time some content is
    /// stored; the returned `MediaRef` always carries the caller's `meta`, since it describes
    /// this attachment. `now` is recorded as the latest put time, which keeps the blob from
    /// being swept while it's in flight.
    pub async fn put<R>(
        &self,
        mut reader: R,
        limit: u64,
        meta: BlobMeta,
        now: Timestamp,
    ) -> Result<MediaRef>
    where
        R: AsyncRead + Unpin + Send,
    {
        let tmp_dir = self.tmp_dir();
        let temp = tokio::task::spawn_blocking(move || {
            tempfile::Builder::new().prefix("put-").tempfile_in(tmp_dir)
        })
        .await??;
        // `temp_path` deletes the file when dropped, including when this future is cancelled.
        let (file, temp_path) = temp.into_parts();
        let mut file = tokio::fs::File::from_std(file);

        let mut hasher = Sha256::new();
        let mut size: u64 = 0;
        let mut buf = vec![0; CHUNK];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            size += n as u64;
            if size > limit {
                return Err(Error::TooLarge { limit });
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        let digest: [u8; 32] = hasher.finalize().into();
        let id = MediaId::from_digest(digest);
        let dest = self.blob_path(&id);

        // Commit: file into place, then its row, with the sweep kept out.
        let _commit = self.sweep_lock.read().await;
        let shard = self.shard_dir(&id);
        match tokio::fs::DirBuilder::new()
            .mode(0o700)
            .create(&shard)
            .await
        {
            // A new shard directory: make its entry in `sha256/` durable too.
            Ok(()) => sync_dir(self.root.clone()).await?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        tokio::task::spawn_blocking(move || temp_path.persist(dest))
            .await?
            .map_err(|e| e.error)?;
        sync_dir(shard).await?;

        // At most `limit`, and SQLite integers hold up to i64::MAX.
        let row_size = i64::try_from(size).map_err(|_| Error::TooLarge { limit })?;
        let row_id = id.as_str().to_owned();
        let row_meta = meta.clone();
        self.db
            .write(move |conn| {
                conn.prepare_cached(
                    "INSERT INTO blobs (id, size, media_type, name, created_at, stored_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                     ON CONFLICT (id) DO UPDATE SET stored_at = max(stored_at, excluded.stored_at)",
                )?
                .execute(params![
                    row_id,
                    row_size,
                    row_meta.media_type,
                    row_meta.name,
                    to_millis(now)
                ])?;
                Ok(())
            })
            .await?;

        Ok(MediaRef {
            id,
            media_type: meta.media_type,
            size,
            name: meta.name,
        })
    }

    /// The blob's metadata as first stored, or `None` if it isn't stored. Doesn't touch the
    /// file.
    pub async fn stat(&self, id: &MediaId) -> Result<Option<MediaRef>> {
        let id = id.clone();
        self.db
            .read(move |conn| {
                let row = conn
                    .prepare_cached("SELECT size, media_type, name FROM blobs WHERE id = ?1")?
                    .query_row([id.as_str()], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    })
                    .optional()?;
                row.map(|(size, media_type, name)| {
                    Ok(MediaRef {
                        size: u64::try_from(size)
                            .map_err(|_| Error::corrupt(format!("blob size {size}")))?,
                        id,
                        media_type,
                        name,
                    })
                })
                .transpose()
            })
            .await
    }

    /// Opens a blob: `None` if it isn't stored, [`Error::BlobDamaged`] if its file is missing or
    /// has the wrong size. The reader verifies the content's hash at the end.
    pub async fn get(&self, id: &MediaId) -> Result<Option<Blob>> {
        let Some(media) = self.stat(id).await? else {
            return Ok(None);
        };
        let damaged = |reason: String| Error::BlobDamaged {
            id: id.clone(),
            reason,
        };
        let file = match tokio::fs::File::open(self.blob_path(id)).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(damaged("file missing".into()));
            }
            Err(e) => return Err(e.into()),
        };
        let len = file.metadata().await?.len();
        if len != media.size {
            return Err(damaged(format!(
                "file has {len} bytes, expected {}",
                media.size
            )));
        }
        Ok(Some(Blob {
            reader: BlobReader {
                file,
                id: id.clone(),
                hasher: Sha256::new(),
                failed: false,
                done: false,
            },
            media,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::at;

    async fn open() -> (tempfile::TempDir, Store, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(dir.path().join("giles.db")).await.unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs"), db.clone())
            .await
            .unwrap();
        (dir, db, blobs)
    }

    fn meta(media_type: &str, name: Option<&str>) -> BlobMeta {
        BlobMeta {
            media_type: media_type.into(),
            name: name.map(Into::into),
        }
    }

    async fn read_all(blob: Blob) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut reader = blob.reader;
        reader.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }

    /// Every file under `sha256/`, relative to it, sorted.
    fn files(blobs: &BlobStore) -> Vec<String> {
        let mut out = Vec::new();
        for dir in std::fs::read_dir(&blobs.root).unwrap() {
            let dir = dir.unwrap().path();
            for file in std::fs::read_dir(&dir).unwrap() {
                let path = file.unwrap().path();
                out.push(
                    path.strip_prefix(&blobs.root)
                        .unwrap()
                        .display()
                        .to_string(),
                );
            }
        }
        out.sort();
        out
    }

    async fn blob_rows(db: &Store) -> i64 {
        db.read(|conn| Ok(conn.query_row("SELECT count(*) FROM blobs", [], |r| r.get(0))?))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn put_and_get_round_trip_bytes_and_metadata() {
        let (_dir, _db, blobs) = open().await;
        let media = blobs
            .put(
                &b"hello"[..],
                1024,
                meta("text/plain", Some("hi.txt")),
                at(0),
            )
            .await
            .unwrap();
        // sha256("hello")
        let id = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(media.id.as_str(), id);
        assert_eq!(
            (media.size, media.media_type.as_str(), media.name.as_deref()),
            (5, "text/plain", Some("hi.txt"))
        );
        assert_eq!(files(&blobs), [format!("2c/{id}")]);

        assert_eq!(blobs.stat(&media.id).await.unwrap(), Some(media.clone()));
        let blob = blobs.get(&media.id).await.unwrap().unwrap();
        assert_eq!(blob.media, media);
        assert_eq!(read_all(blob).await.unwrap(), b"hello");

        let unknown = MediaId::from_digest([7; 32]);
        assert!(blobs.get(&unknown).await.unwrap().is_none());
        assert!(blobs.stat(&unknown).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_and_multi_chunk_blobs() {
        let (_dir, _db, blobs) = open().await;
        let empty = blobs
            .put(&b""[..], 0, meta("a/b", None), at(0))
            .await
            .unwrap();
        assert_eq!(empty.size, 0);
        assert_eq!(
            read_all(blobs.get(&empty.id).await.unwrap().unwrap())
                .await
                .unwrap(),
            b""
        );

        let big: Vec<u8> = (0..(3 * CHUNK + 17)).map(|i| (i % 251) as u8).collect();
        let media = blobs
            .put(&big[..], big.len() as u64, meta("a/b", None), at(0))
            .await
            .unwrap();
        assert_eq!(media.size, big.len() as u64);
        let back = read_all(blobs.get(&media.id).await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(back, big);
    }

    #[tokio::test]
    async fn same_content_is_stored_once_and_keeps_its_first_metadata() {
        let (_dir, db, blobs) = open().await;
        let first = blobs
            .put(&b"photo"[..], 100, meta("image/png", Some("a.png")), at(0))
            .await
            .unwrap();
        let second = blobs
            .put(
                &b"photo"[..],
                100,
                meta("image/jpeg", Some("b.jpg")),
                at(50),
            )
            .await
            .unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(
            second.name.as_deref(),
            Some("b.jpg"),
            "the caller's metadata"
        );
        assert_eq!(files(&blobs).len(), 1);
        assert_eq!(blob_rows(&db).await, 1);
        assert_eq!(blobs.stat(&first.id).await.unwrap(), Some(first.clone()));

        let times: (i64, i64) = db
            .read(|conn| {
                Ok(
                    conn.query_row("SELECT created_at, stored_at FROM blobs", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(times, (to_millis(at(0)), to_millis(at(50))));
    }

    #[tokio::test]
    async fn exceeding_the_limit_stores_nothing() {
        let (_dir, db, blobs) = open().await;
        let err = blobs
            .put(&b"123456"[..], 5, meta("a/b", None), at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::TooLarge { limit: 5 }), "{err}");
        let big = vec![1u8; 2 * CHUNK];
        let err = blobs
            .put(&big[..], CHUNK as u64, meta("a/b", None), at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::TooLarge { .. }), "{err}");
        assert!(files(&blobs).is_empty(), "{:?}", files(&blobs));
        assert_eq!(blob_rows(&db).await, 0);

        // Exactly at the limit is fine.
        blobs
            .put(&b"12345"[..], 5, meta("a/b", None), at(0))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_cancelled_put_leaves_no_temp_file() {
        let (_dir, db, blobs) = open().await;
        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"partial").await.unwrap();
        // The writer stays open, so the put waits for more bytes until the timeout drops it.
        let put = blobs.put(reader, 100, meta("a/b", None), at(0));
        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(100), put).await;
        assert!(timed_out.is_err());
        assert!(files(&blobs).is_empty(), "{:?}", files(&blobs));
        assert_eq!(blob_rows(&db).await, 0);
        drop(writer);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_puts_of_the_same_content_agree() {
        let (_dir, db, blobs) = open().await;
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 7) as u8).collect();
        let content = std::sync::Arc::new(content);
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..16 {
            let (blobs, content) = (blobs.clone(), content.clone());
            tasks.spawn(async move {
                blobs
                    .put(&content[..], 1 << 20, meta("a/b", None), at(i))
                    .await
            });
        }
        let mut ids = std::collections::HashSet::new();
        while let Some(result) = tasks.join_next().await {
            ids.insert(result.unwrap().unwrap().id);
        }
        assert_eq!(ids.len(), 1);
        assert_eq!(files(&blobs).len(), 1);
        assert_eq!(blob_rows(&db).await, 1);
        let id = ids.into_iter().next().unwrap();
        let back = read_all(blobs.get(&id).await.unwrap().unwrap())
            .await
            .unwrap();
        assert_eq!(&back, &*content);
    }

    #[tokio::test]
    async fn a_missing_or_truncated_file_is_reported_and_repaired_by_a_new_put() {
        let (_dir, _db, blobs) = open().await;
        let media = blobs
            .put(&b"precious"[..], 100, meta("a/b", None), at(0))
            .await
            .unwrap();
        let path = blobs.blob_path(&media.id);

        std::fs::remove_file(&path).unwrap();
        let err = blobs.get(&media.id).await.unwrap_err();
        assert!(matches!(err, Error::BlobDamaged { .. }), "{err}");
        assert!(blobs.stat(&media.id).await.unwrap().is_some());

        blobs
            .put(&b"precious"[..], 100, meta("a/b", None), at(1))
            .await
            .unwrap();
        assert_eq!(
            read_all(blobs.get(&media.id).await.unwrap().unwrap())
                .await
                .unwrap(),
            b"precious"
        );

        std::fs::write(&path, b"prec").unwrap();
        let err = blobs.get(&media.id).await.unwrap_err();
        assert!(matches!(err, Error::BlobDamaged { .. }), "{err}");
        blobs
            .put(&b"precious"[..], 100, meta("a/b", None), at(2))
            .await
            .unwrap();
        assert_eq!(
            read_all(blobs.get(&media.id).await.unwrap().unwrap())
                .await
                .unwrap(),
            b"precious"
        );
    }

    #[tokio::test]
    async fn corrupted_content_fails_at_the_end_of_the_read() {
        let (_dir, _db, blobs) = open().await;
        let media = blobs
            .put(&b"precious"[..], 100, meta("a/b", None), at(0))
            .await
            .unwrap();
        std::fs::write(blobs.blob_path(&media.id), b"PRECIOUS").unwrap();

        let blob = blobs.get(&media.id).await.unwrap().unwrap();
        let err = read_all(blob).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let mut reader = blobs.get(&media.id).await.unwrap().unwrap().reader;
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf).await.unwrap();
        assert!(reader.read(&mut buf).await.is_err());
        assert!(reader.read(&mut buf).await.is_err(), "the error sticks");

        // Storing the content again replaces the damaged copy.
        blobs
            .put(&b"precious"[..], 100, meta("a/b", None), at(1))
            .await
            .unwrap();
        let blob = blobs.get(&media.id).await.unwrap().unwrap();
        assert_eq!(read_all(blob).await.unwrap(), b"precious");
    }

    #[tokio::test]
    async fn the_sweep_lock_holds_puts_at_their_commit() {
        let (_dir, db, blobs) = open().await;
        let guard = blobs.lock_for_sweep().await;
        let put = {
            let blobs = blobs.clone();
            tokio::spawn(async move {
                blobs
                    .put(&b"later"[..], 100, meta("a/b", None), at(0))
                    .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!put.is_finished());
        // The bytes were streamed, but nothing is committed: no blob file, no row.
        assert!(
            files(&blobs).iter().all(|f| f.starts_with("tmp/")),
            "{:?}",
            files(&blobs)
        );
        assert_eq!(blob_rows(&db).await, 0);

        drop(guard);
        let media = put.await.unwrap().unwrap();
        assert!(blobs.get(&media.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn opening_clears_leftover_temp_files() {
        let (dir, db, blobs) = open().await;
        let leftover = blobs.tmp_dir().join("put-crashed");
        std::fs::write(&leftover, b"half").unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs"), db).await.unwrap();
        assert!(!leftover.exists());
        assert!(blobs.tmp_dir().is_dir());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn directories_and_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, _db, blobs) = open().await;
        let media = blobs
            .put(&b"x"[..], 10, meta("a/b", None), at(0))
            .await
            .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&blobs.root), 0o700);
        assert_eq!(mode(&blobs.shard_dir(&media.id)), 0o700);
        assert_eq!(mode(&blobs.blob_path(&media.id)), 0o600);
    }
}
