//! The blob sweep (PLAN 11.5): removes blobs nothing refers to.
//!
//! A blob is garbage when it is old (stored at least [`SWEEP_GRACE`] ago) and its id appears in
//! no message, summary, inbox row, outbox row, or job. References are found by scanning the
//! text of every column that can hold one for 64-character runs of lowercase hex, the form of a
//! [`MediaId`], instead of parsing each column's JSON. A reference nested anywhere in a JSON
//! document (a tool result inside a message, an attachment in an inbox event, a `$media`
//! input) is found the same way, and a column added later that holds a `MediaRef` only has to
//! be added to [`REFERENCING_COLUMNS`]. The scan can only over-approximate (any hex string equal
//! to a blob's id keeps it), which is the safe direction.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use giles_types::{MediaId, Timestamp};
use rusqlite::Connection;
use rusqlite::types::ValueRef;

use super::BlobStore;
use crate::Result;
use crate::time::to_millis;

/// How old a blob has to be before the sweep may remove it (PLAN 11.5): 48 hours.
pub const SWEEP_GRACE: Duration = Duration::from_secs(48 * 60 * 60);

/// Every column that can hold a [`MediaId`], as `(table, column)`. Adding a table or a column
/// that stores a `MediaRef` anywhere requires adding it here, or the sweep deletes its blobs.
const REFERENCING_COLUMNS: &[(&str, &str)] = &[
    // Blocks: attachments, nested tool-result content, `$media` tool inputs.
    ("messages", "content"),
    ("messages", "meta"),
    ("summaries", "content"),
    // Events with `attachments`, awaiting or past processing.
    ("inbox", "payload"),
    // Sends with `attachments`, awaiting delivery.
    ("outbox", "payload"),
    ("jobs", "spec"),
];

/// What a [`BlobStore::sweep`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Unreferenced old blobs removed: their rows, and their files where those existed.
    pub blobs: usize,
    /// Old files without a row removed: what interrupted puts left.
    pub orphan_files: usize,
    /// The size of the files removed.
    pub bytes: u64,
}

/// A file under `sha256/<shard>/` named by a valid id.
#[derive(Debug)]
struct DiskFile {
    path: PathBuf,
    size: u64,
    /// Modification time in Unix milliseconds, `i64::MAX` if it can't be read (kept).
    modified_ms: i64,
}

/// A blob the sweep may remove unless something refers to it.
struct Candidate {
    id: [u8; 64],
    had_row: bool,
    file: Option<DiskFile>,
}

impl BlobStore {
    /// Removes the blobs nothing refers to, as of `now`: rows stored more than `grace` ago that
    /// no message, summary, inbox row, outbox row, or job mentions, and files without a row
    /// whose mtime is more than `grace` ago (the leftovers of interrupted puts, PLAN 11.5).
    /// Production passes [`SWEEP_GRACE`] and the current time.
    ///
    /// Holds [`lock_for_sweep`](Self::lock_for_sweep) throughout, so no put commits between the
    /// decision and the deletion. The decision and the row deletions happen in one write
    /// transaction (no reference can appear between the scan and the deletion); files are
    /// removed after it commits. A crash in between leaves rowless files, which the next sweep
    /// removes.
    ///
    /// Fails without removing anything if the files can't be listed or a table can't be scanned.
    /// A file that can't be removed is logged and left for the next sweep. Files in the store
    /// whose names aren't blob ids, and the `tmp/` directory (puts in progress, cleared when the
    /// store opens), are left alone.
    pub async fn sweep(&self, now: Timestamp, grace: Duration) -> Result<SweepReport> {
        let cutoff =
            to_millis(now).saturating_sub(i64::try_from(grace.as_millis()).unwrap_or(i64::MAX));
        let _lock = self.lock_for_sweep().await;

        let root = self.root.clone();
        let disk = tokio::task::spawn_blocking(move || list_files(&root)).await??;
        let candidates = self
            .db
            .write(move |conn| {
                let candidates = pick(conn, disk, cutoff)?;
                if candidates.is_empty() {
                    return Ok(Vec::new());
                }
                let wanted: HashSet<[u8; 64]> = candidates.iter().map(|c| c.id).collect();
                let referenced = referenced_ids(conn, &wanted)?;
                let mut delete = conn.prepare_cached("DELETE FROM blobs WHERE id = ?1")?;
                let mut garbage = Vec::new();
                for candidate in candidates {
                    if referenced.contains(&candidate.id) {
                        continue;
                    }
                    if candidate.had_row {
                        delete.execute([id_str(&candidate.id)])?;
                    }
                    garbage.push(candidate);
                }
                Ok(garbage)
            })
            .await?;

        let mut report = SweepReport::default();
        for candidate in candidates {
            let mut removed = true;
            if let Some(file) = &candidate.file {
                match tokio::fs::remove_file(&file.path).await {
                    Ok(()) => report.bytes += file.size,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::warn!(path = %file.path.display(), %error, "cannot remove blob file");
                        removed = false;
                    }
                }
            }
            if removed {
                tracing::debug!(id = id_str(&candidate.id), "swept blob");
                if candidate.had_row {
                    report.blobs += 1;
                } else {
                    report.orphan_files += 1;
                }
            }
        }
        Ok(report)
    }
}

fn id_str(id: &[u8; 64]) -> &str {
    // Candidates are built from valid ids: lowercase hex.
    std::str::from_utf8(id).expect("a media id is ASCII")
}

/// Every blob file under `root`, which holds the shard directories and `tmp/`. Entries that
/// aren't regular files named by a valid id in their own shard are skipped.
fn list_files(root: &Path) -> io::Result<HashMap<[u8; 64], DiskFile>> {
    let mut files = HashMap::new();
    for shard in std::fs::read_dir(root)? {
        let shard = shard?;
        let shard_name = shard.file_name();
        let Some(shard_name) = shard_name.to_str() else {
            continue;
        };
        let is_shard = shard_name.len() == 2 && shard_name.bytes().all(is_hex);
        if !is_shard || !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(shard.path())? {
            let entry = entry?;
            let name = entry.file_name();
            let valid = name.to_str().and_then(|n| {
                let id = MediaId::new(n).ok()?;
                n.starts_with(shard_name).then_some(id)
            });
            let Some(id) = valid else {
                tracing::debug!(path = %entry.path().display(), "not a blob file; leaving it");
                continue;
            };
            if !entry.file_type()?.is_file() {
                continue;
            }
            let meta = entry.metadata()?;
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|m| Timestamp::try_from(m).ok())
                .map_or(i64::MAX, to_millis);
            let id: [u8; 64] = id.as_str().as_bytes().try_into().expect("64 digits");
            files.insert(
                id,
                DiskFile {
                    path: entry.path(),
                    size: meta.len(),
                    modified_ms,
                },
            );
        }
    }
    Ok(files)
}

fn is_hex(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

/// The old blobs: rows stored before `cutoff` (with their files, if present), and old files
/// without a row. Rows younger than `cutoff` and their files are not candidates.
fn pick(
    conn: &Connection,
    mut disk: HashMap<[u8; 64], DiskFile>,
    cutoff: i64,
) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::new();
    let mut old = conn.prepare_cached("SELECT id FROM blobs WHERE stored_at < ?1")?;
    let mut rows = old.query([cutoff])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let Ok(id) = <[u8; 64]>::try_from(id.as_bytes()) else {
            // Can't be an id this store wrote; keep it rather than guess.
            tracing::warn!(id, "blob row with a malformed id; leaving it");
            continue;
        };
        let file = disk.remove(&id);
        candidates.push(Candidate {
            id,
            had_row: true,
            file,
        });
    }
    // What's left on disk has no old row: either a young row (keep) or no row (an interrupted
    // put, garbage once its file is old).
    let mut has_row = conn.prepare_cached("SELECT 1 FROM blobs WHERE id = ?1")?;
    for (id, file) in disk {
        if file.modified_ms >= cutoff || has_row.exists([id_str(&id)])? {
            continue;
        }
        candidates.push(Candidate {
            id,
            had_row: false,
            file: Some(file),
        });
    }
    Ok(candidates)
}

/// The members of `wanted` that appear in any of [`REFERENCING_COLUMNS`]. Any error aborts, so
/// the caller never acts on a partial answer.
fn referenced_ids(conn: &Connection, wanted: &HashSet<[u8; 64]>) -> Result<HashSet<[u8; 64]>> {
    let mut found = HashSet::new();
    for (table, column) in REFERENCING_COLUMNS {
        let mut stmt = conn.prepare(&format!("SELECT {column} FROM {table}"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            match row.get_ref(0)? {
                ValueRef::Text(text) | ValueRef::Blob(text) => find_ids(text, wanted, &mut found),
                ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => {}
            }
        }
        if found.len() == wanted.len() {
            break;
        }
    }
    Ok(found)
}

/// Adds to `found` every 64-byte window of `text` made of lowercase hex digits that is in
/// `wanted`. Windows overlap inside longer hex runs, so an id glued to other hex digits counts.
fn find_ids(text: &[u8], wanted: &HashSet<[u8; 64]>, found: &mut HashSet<[u8; 64]>) {
    let mut run = 0;
    for (i, &b) in text.iter().enumerate() {
        if !is_hex(b) {
            run = 0;
            continue;
        }
        run += 1;
        if run >= 64 {
            let window: &[u8; 64] = text[i + 1 - 64..=i].try_into().expect("64 bytes");
            if wanted.contains(window) {
                found.insert(*window);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use giles_types::{Block, MediaRef};
    use serde_json::json;

    use super::*;
    use crate::{BlobMeta, Store};

    fn ts(millis: i64) -> Timestamp {
        Timestamp::from_millisecond(millis).unwrap()
    }

    const HOUR: i64 = 3_600_000;
    const GRACE_MS: i64 = 48 * HOUR;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: Store,
        blobs: BlobStore,
    }

    async fn open() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = Store::open(dir.path().join("giles.db")).await.unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs"), db.clone())
            .await
            .unwrap();
        Fixture {
            _dir: dir,
            db,
            blobs,
        }
    }

    impl Fixture {
        /// Stores `content` as of `stored_ms` (milliseconds after the epoch).
        async fn put(&self, content: &str, stored_ms: i64) -> MediaRef {
            self.blobs
                .put(
                    content.as_bytes(),
                    1024,
                    BlobMeta {
                        media_type: "text/plain".into(),
                        name: None,
                    },
                    ts(stored_ms),
                )
                .await
                .unwrap()
        }

        async fn sweep(&self, now_ms: i64) -> Result<SweepReport> {
            self.blobs.sweep(ts(now_ms), SWEEP_GRACE).await
        }

        async fn exec(&self, sql: &'static str, params: Vec<String>) {
            self.db
                .write(move |conn| {
                    conn.execute(sql, rusqlite::params_from_iter(params))?;
                    Ok(())
                })
                .await
                .unwrap();
        }

        /// A session, the parent of messages and summaries.
        async fn session(&self) {
            self.exec(
                "INSERT INTO sessions (id, agent, kind, channel, conversation, started_at)
                 VALUES (1, 'a', 'direct', 'c', 'x', 0)",
                vec![],
            )
            .await;
        }

        async fn has_row(&self, media: &MediaRef) -> bool {
            self.blobs.stat(&media.id).await.unwrap().is_some()
        }

        fn has_file(&self, media: &MediaRef) -> bool {
            self.blobs.blob_path(&media.id).exists()
        }

        /// A file as an interrupted put leaves it, with its mtime `age_ms` before now.
        fn orphan(&self, content: &str, age_ms: i64) -> PathBuf {
            use sha2::{Digest, Sha256};
            let digest: [u8; 32] = Sha256::digest(content.as_bytes()).into();
            let id = MediaId::from_digest(digest);
            std::fs::create_dir_all(self.blobs.shard_dir(&id)).unwrap();
            let path = self.blobs.blob_path(&id);
            std::fs::write(&path, content).unwrap();
            let mtime = SystemTime::now() - Duration::from_millis(age_ms as u64);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            path
        }
    }

    /// "Now" for the sweep: a long time after the epoch the test blobs are stored at, so a blob
    /// stored at `ts(0)` is old and one stored at `NOW - HOUR` is young.
    const NOW: i64 = 1_000 * HOUR;

    fn block_json(media: &MediaRef) -> String {
        serde_json::to_string(&[Block::Image {
            media: media.clone(),
        }])
        .unwrap()
    }

    #[tokio::test]
    async fn unreferenced_old_blobs_are_removed_and_young_ones_kept() {
        let f = open().await;
        let old = f.put("old", 0).await;
        let young = f.put("young", NOW - HOUR).await;
        // Exactly at the grace boundary is still kept; one millisecond older goes.
        let edge = f.put("edge", NOW - GRACE_MS).await;
        let past_edge = f.put("past edge", NOW - GRACE_MS - 1).await;

        let report = f.sweep(NOW).await.unwrap();
        assert_eq!(
            report,
            SweepReport {
                blobs: 2,
                orphan_files: 0,
                bytes: (3 + 9) as u64
            }
        );
        for gone in [&old, &past_edge] {
            assert!(!f.has_row(gone).await && !f.has_file(gone));
        }
        for kept in [&young, &edge] {
            assert!(f.has_row(kept).await && f.has_file(kept));
        }
        assert_eq!(f.sweep(NOW).await.unwrap(), SweepReport::default());
    }

    #[tokio::test]
    async fn a_blob_put_again_is_young_again() {
        let f = open().await;
        let media = f.put("again", 0).await;
        f.put("again", NOW - HOUR).await;
        assert_eq!(f.sweep(NOW).await.unwrap(), SweepReport::default());
        assert!(f.has_row(&media).await);
    }

    #[tokio::test]
    async fn blocks_in_messages_keep_their_blobs() {
        let f = open().await;
        f.session().await;
        let top = f.put("top-level image", 0).await;
        let nested = f.put("tool result image", 0).await;
        let tool_input = f.put("media input", 0).await;
        let in_summary = f.put("summarized", 0).await;
        let unused = f.put("unused", 0).await;

        let result = serde_json::to_string(&[
            Block::ToolUse {
                id: "t1".into(),
                name: "x__y".into(),
                input: json!({"file": {"$media": tool_input.id.as_str()}}),
            },
            Block::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![Block::Image {
                    media: nested.clone(),
                }],
                is_error: false,
            },
        ])
        .unwrap();
        for (seq, content) in [(1, block_json(&top)), (2, result)] {
            f.exec(
                "INSERT INTO messages (session_id, seq, role, content, meta, created_at)
                 VALUES (1, ?1, 'user', ?2, '{}', 0)",
                vec![seq.to_string(), content],
            )
            .await;
        }
        f.exec(
            "INSERT INTO summaries (session_id, from_seq, through_seq, content, created_at)
             VALUES (1, 1, 1, ?1, 0)",
            vec![format!("they sent {}", in_summary.id)],
        )
        .await;

        let report = f.sweep(NOW).await.unwrap();
        assert_eq!(report.blobs, 1);
        for kept in [&top, &nested, &tool_input, &in_summary] {
            assert!(f.has_row(kept).await && f.has_file(kept));
        }
        assert!(!f.has_row(&unused).await && !f.has_file(&unused));
    }

    #[tokio::test]
    async fn inbox_outbox_and_job_rows_keep_their_blobs() {
        let f = open().await;
        let inbound = f.put("received", 0).await;
        let outbound = f.put("to send", 0).await;
        let job = f.put("job attachment", 0).await;
        let unused = f.put("unused", 0).await;

        f.exec(
            "INSERT INTO inbox (instance, event_id, payload, state, received_at)
             VALUES ('wa', 'e1', ?1, 'done', 0)",
            vec![json!({"attachments": [inbound]}).to_string()],
        )
        .await;
        f.exec(
            "INSERT INTO outbox
               (instance, recipient, kind, payload, created_at, next_attempt_at, expires_at)
             VALUES ('wa', 'wa:1', 'job_reply', ?1, 0, 0, 0)",
            vec![json!({"markdown": "hi", "attachments": [outbound]}).to_string()],
        )
        .await;
        f.exec(
            "INSERT INTO jobs (agent, creator, spec, created_at) VALUES ('a', 'u', ?1, 0)",
            vec![json!({"prompt": "send it", "attachments": [job]}).to_string()],
        )
        .await;

        assert_eq!(f.sweep(NOW).await.unwrap().blobs, 1);
        for kept in [&inbound, &outbound, &job] {
            assert!(f.has_row(kept).await && f.has_file(kept));
        }
        assert!(!f.has_file(&unused));
    }

    #[tokio::test]
    async fn a_blob_is_collected_once_its_last_reference_is_gone() {
        let f = open().await;
        let media = f.put("short lived", 0).await;
        f.exec(
            "INSERT INTO outbox
               (instance, recipient, kind, payload, created_at, next_attempt_at, expires_at)
             VALUES ('wa', 'wa:1', 'notice', ?1, 0, 0, 0)",
            vec![json!({"attachments": [media]}).to_string()],
        )
        .await;
        assert_eq!(f.sweep(NOW).await.unwrap(), SweepReport::default());
        f.exec("DELETE FROM outbox", vec![]).await;
        assert_eq!(f.sweep(NOW).await.unwrap().blobs, 1);
    }

    #[tokio::test]
    async fn an_id_inside_a_longer_hex_run_still_counts_as_a_reference() {
        let f = open().await;
        let media = f.put("glued", 0).await;
        f.exec(
            "INSERT INTO inbox (instance, event_id, payload, state, received_at)
             VALUES ('wa', 'e1', ?1, 'done', 0)",
            vec![format!("\"ff{}ff\"", media.id)],
        )
        .await;
        assert_eq!(f.sweep(NOW).await.unwrap(), SweepReport::default());
    }

    #[tokio::test]
    async fn a_scan_error_removes_nothing() {
        let f = open().await;
        let old = f.put("old", 0).await;
        let orphan = f.orphan("orphan", 2 * GRACE_MS);
        // A column the sweep must read is gone.
        f.exec("DROP TABLE outbox", vec![]).await;

        f.blobs
            .sweep(Timestamp::now(), SWEEP_GRACE)
            .await
            .unwrap_err();
        assert!(f.has_row(&old).await && f.has_file(&old));
        assert!(orphan.exists());
    }

    #[tokio::test]
    async fn an_unlistable_store_removes_nothing() {
        let f = open().await;
        let old = f.put("old", 0).await;
        // The store's directory can't be listed.
        std::fs::remove_dir_all(&f.blobs.root).unwrap();
        std::fs::write(&f.blobs.root, b"not a directory").unwrap();
        f.sweep(NOW).await.unwrap_err();
        assert!(f.has_row(&old).await);
    }

    #[tokio::test]
    async fn old_files_without_a_row_are_removed_young_ones_kept() {
        let f = open().await;
        let old = f.orphan("interrupted long ago", 3 * GRACE_MS);
        let young = f.orphan("interrupted just now", HOUR);

        let report = f.blobs.sweep(Timestamp::now(), SWEEP_GRACE).await.unwrap();
        assert_eq!(
            report,
            SweepReport {
                blobs: 0,
                orphan_files: 1,
                bytes: "interrupted long ago".len() as u64
            }
        );
        assert!(!old.exists());
        assert!(young.exists());
    }

    #[tokio::test]
    async fn a_referenced_file_without_a_row_is_kept() {
        let f = open().await;
        let path = f.orphan("rowless but used", 3 * GRACE_MS);
        let id = path.file_name().unwrap().to_str().unwrap().to_owned();
        f.exec(
            "INSERT INTO outbox
               (instance, recipient, kind, payload, created_at, next_attempt_at, expires_at)
             VALUES ('wa', 'wa:1', 'notice', ?1, 0, 0, 0)",
            vec![json!({"attachments": [{"id": id}]}).to_string()],
        )
        .await;
        assert_eq!(
            f.blobs.sweep(Timestamp::now(), SWEEP_GRACE).await.unwrap(),
            SweepReport::default()
        );
        assert!(path.exists());
    }

    #[tokio::test]
    async fn a_row_whose_file_is_missing_is_removed_when_unreferenced() {
        let f = open().await;
        let lost = f.put("lost file", 0).await;
        let kept = f.put("lost but used", 0).await;
        std::fs::remove_file(f.blobs.blob_path(&lost.id)).unwrap();
        std::fs::remove_file(f.blobs.blob_path(&kept.id)).unwrap();
        f.exec(
            "INSERT INTO inbox (instance, event_id, payload, state, received_at)
             VALUES ('wa', 'e1', ?1, 'done', 0)",
            vec![json!({"attachments": [kept]}).to_string()],
        )
        .await;

        let report = f.sweep(NOW).await.unwrap();
        assert_eq!(
            report,
            SweepReport {
                blobs: 1,
                orphan_files: 0,
                bytes: 0
            }
        );
        assert!(!f.has_row(&lost).await);
        assert!(f.has_row(&kept).await);
    }

    #[tokio::test]
    async fn temp_files_and_foreign_files_are_left_alone() {
        let f = open().await;
        let tmp = f.blobs.tmp_dir().join("put-in-flight");
        std::fs::write(&tmp, b"streaming").unwrap();
        let old = f.put("old", 0).await;
        let stray = f.blobs.shard_dir(&old.id).join("notes.txt");
        std::fs::write(&stray, b"mine").unwrap();
        // A real id in the wrong shard.
        let misplaced = f.blobs.root.join("00").join(old.id.as_str());
        std::fs::create_dir_all(misplaced.parent().unwrap()).unwrap();
        std::fs::write(&misplaced, b"x").unwrap();

        let report = f
            .blobs
            .sweep(Timestamp::now(), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(report.blobs, 1);
        assert!(tmp.exists() && stray.exists() && misplaced.exists());
    }

    #[tokio::test]
    async fn a_put_waits_for_the_sweep_and_survives_it() {
        let f = open().await;
        let old = f.put("same bytes", 0).await;
        let sweep = {
            let (blobs, now) = (f.blobs.clone(), ts(NOW));
            tokio::spawn(async move { blobs.sweep(now, SWEEP_GRACE).await })
        };
        let again = f.put("same bytes", NOW).await;
        let report = sweep.await.unwrap().unwrap();
        // Either order is consistent: the sweep ran first and the put recreated the blob, or
        // the put came first and made it young.
        assert_eq!(again.id, old.id);
        assert!(report.blobs <= 1);
        assert!(f.has_row(&again).await && f.has_file(&again));
        assert_eq!(
            f.blobs.get(&again.id).await.unwrap().map(|b| b.media.size),
            Some(10)
        );
    }

    #[test]
    fn find_ids_matches_only_whole_windows_of_lowercase_hex() {
        let id = [b'a'; 64];
        let wanted = HashSet::from([id]);
        let scan = |text: &[u8]| {
            let mut found = HashSet::new();
            find_ids(text, &wanted, &mut found);
            found.len()
        };
        let id_text = "a".repeat(64);
        assert_eq!(scan(format!("{{\"id\":\"{id_text}\"}}").as_bytes()), 1);
        assert_eq!(scan(format!("0{id_text}9").as_bytes()), 1);
        assert_eq!(scan("a".repeat(63).as_bytes()), 0);
        assert_eq!(
            scan(format!("{}-{}", "a".repeat(32), "a".repeat(32)).as_bytes()),
            0
        );
        // Uppercase is not an id (MediaId is lowercase).
        assert_eq!(scan("A".repeat(64).as_bytes()), 0);
    }
}
