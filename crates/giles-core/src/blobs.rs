//! The core's side of the blob store (PLAN section 11.5): attachment bytes for built-in
//! providers, and the periodic sweep of blobs nothing references.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use giles_store::{Blob, BlobStore, SWEEP_GRACE, SweepReport};
use giles_types::{Error, MediaRef, Result};
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::providers::MediaSource;
use crate::sessions::Clock;

/// How long after startup the first sweep runs: late enough not to compete with startup
/// recovery, early enough that a core that is restarted often still sweeps.
pub const SWEEP_FIRST_DELAY: Duration = Duration::from_secs(60);

/// The time between sweeps. Blobs have to be unreferenced for 48 hours first
/// ([`SWEEP_GRACE`]), so a blob lives at most this much longer; the scan reads every message,
/// which is why it doesn't run more often.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// When the sweep runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepSchedule {
    /// From start to the first sweep.
    pub first_delay: Duration,
    /// From the end of one sweep to the start of the next.
    pub interval: Duration,
}

impl Default for SweepSchedule {
    fn default() -> Self {
        Self {
            first_delay: SWEEP_FIRST_DELAY,
            interval: SWEEP_INTERVAL,
        }
    }
}

/// The background task that periodically runs [`BlobStore::sweep`].
#[derive(Debug)]
pub struct BlobSweep {
    tasks: TaskTracker,
}

impl BlobSweep {
    /// Starts sweeping `blobs` on `schedule`, with the [`SWEEP_GRACE`] period, until `shutdown`
    /// is cancelled. A sweep that fails is logged and the next one runs on schedule; the sweep
    /// removes nothing when it fails.
    pub fn start(
        blobs: BlobStore,
        clock: Arc<dyn Clock>,
        schedule: SweepSchedule,
        shutdown: CancellationToken,
    ) -> Self {
        let tasks = TaskTracker::new();
        tasks.spawn(async move {
            let mut wait = schedule.first_delay;
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                // A running sweep is let finish: it's short, and abandoning it would only leave
                // its files to the next one.
                match blobs.sweep(clock.now(), SWEEP_GRACE).await {
                    Ok(report) => log_report(report),
                    Err(error) => tracing::warn!(%error, "blob sweep failed; nothing was removed"),
                }
                wait = schedule.interval;
            }
        });
        tasks.close();
        Self { tasks }
    }

    /// Waits until the sweep task has stopped. Call it after cancelling the shutdown token.
    pub async fn closed(&self) {
        self.tasks.wait().await;
    }
}

fn log_report(report: SweepReport) {
    let SweepReport {
        blobs,
        orphan_files,
        bytes,
    } = report;
    if blobs + orphan_files == 0 {
        tracing::debug!("blob sweep: nothing to remove");
    } else {
        tracing::info!(
            blobs,
            orphan_files,
            bytes,
            "blob sweep removed unreferenced blobs"
        );
    }
}

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
    use tokio::time::timeout;

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

    struct FixedClock(Timestamp);

    impl Clock for FixedClock {
        fn now(&self) -> Timestamp {
            self.0
        }
    }

    const FAST: SweepSchedule = SweepSchedule {
        first_delay: Duration::from_millis(10),
        interval: Duration::from_millis(10),
    };

    #[tokio::test]
    async fn the_sweep_task_removes_old_unreferenced_blobs_and_stops_on_shutdown() {
        let (dir, blobs) = open().await;
        let old = blobs
            .put(&b"old"[..], 1024, meta(), Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        let young = blobs
            .put(
                &b"young"[..],
                1024,
                meta(),
                Timestamp::from_second(10_000_000).unwrap(),
            )
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let now = Timestamp::from_second(10_000_000).unwrap();
        let sweep = BlobSweep::start(
            blobs.clone(),
            Arc::new(FixedClock(now)),
            FAST,
            shutdown.clone(),
        );

        timeout(Duration::from_secs(10), async {
            while blob_file(&dir, &old.id).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the sweep removes the old blob");
        assert!(blobs.stat(&old.id).await.unwrap().is_none());
        assert!(blob_file(&dir, &young.id).exists());

        shutdown.cancel();
        timeout(Duration::from_secs(10), sweep.closed())
            .await
            .expect("the sweep task stops with the shutdown token");
    }

    #[tokio::test]
    async fn a_shutdown_before_the_first_sweep_removes_nothing() {
        let (dir, blobs) = open().await;
        let old = blobs
            .put(&b"old"[..], 1024, meta(), Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let now = Timestamp::from_second(10_000_000).unwrap();
        let schedule = SweepSchedule {
            first_delay: Duration::from_secs(3600),
            ..FAST
        };
        let sweep = BlobSweep::start(blobs, Arc::new(FixedClock(now)), schedule, shutdown.clone());
        shutdown.cancel();
        timeout(Duration::from_secs(10), sweep.closed())
            .await
            .unwrap();
        assert!(blob_file(&dir, &old.id).exists());
    }

    #[test]
    fn the_default_schedule_starts_soon_and_repeats_well_inside_the_grace_period() {
        let schedule = SweepSchedule::default();
        assert!(schedule.first_delay <= Duration::from_secs(5 * 60));
        assert!(schedule.interval * 4 <= SWEEP_GRACE);
    }
}
