//! Channel events the core hasn't acked yet, persisted before they're sent (PLAN section 8.5).
//!
//! One file per event, `events/<seq>.pb`, holding the encoded wire `ChannelEvent`: exactly what goes
//! on the stream. Sequence numbers keep the order across restarts. A file is written to `tmp/`,
//! synced, renamed into place, and the directory synced, before the event is offered to any
//! `Events` stream; it's deleted when the core acks the event.
//!
//! The spool tells [`Blobs`] which inbound blobs its events refer to, so a blob is deleted when
//! the last event that refers to it is acked, and blobs no event refers to are swept at startup.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use giles_proto::channel::v1::{ChannelEvent, channel_event};
use giles_types::MediaId;
use prost::Message as _;
use tokio::sync::watch;

use crate::blobs::{Blobs, write_synced};
use crate::dirs::{SdkDirs, remove_if_exists, sync_dir};

/// The persistent queue of unacked events.
pub(crate) struct Spool {
    dirs: Arc<SdkDirs>,
    blobs: Blobs,
    state: Mutex<State>,
    /// The highest sequence number pushed so far; streams wait on it for new events.
    pushed: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    next_seq: u64,
    pending: BTreeMap<u64, ChannelEvent>,
    /// Sequence number by event id, including events still being written.
    by_id: HashMap<String, u64>,
}

/// The inbound blobs a wire event refers to, one per attachment. Ids that aren't blob ids can't
/// be this SDK's blobs (the SDK only sends its own), so they're skipped.
fn attachments(event: &ChannelEvent) -> Vec<MediaId> {
    match &event.event {
        Some(channel_event::Event::Message(message)) => message
            .attachments
            .iter()
            .filter_map(|blob| MediaId::new(blob.id.as_str()).ok())
            .collect(),
        _ => Vec::new(),
    }
}

impl Spool {
    /// Loads the events left by earlier runs, then deletes the inbound blobs none of them refers
    /// to (an ack's deletion that a crash interrupted, or blobs put and never sent). A file that
    /// doesn't decode is renamed to `<name>.corrupt`, logged, and skipped; a second file with an
    /// id already loaded is deleted. Call it before the plugin can put blobs.
    pub(crate) fn open(dirs: Arc<SdkDirs>, blobs: Blobs) -> io::Result<Self> {
        let mut state = State {
            next_seq: 1,
            ..State::default()
        };
        let mut files: Vec<(u64, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(&dirs.events)? {
            let path = entry?.path();
            match seq_of(&path) {
                Some(seq) => files.push((seq, path)),
                None if path.extension().is_some_and(|ext| ext == "corrupt") => {}
                None => tracing::warn!(path = %path.display(), "ignoring a stray file"),
            }
        }
        files.sort_unstable();
        for (seq, path) in files {
            state.next_seq = state.next_seq.max(seq + 1);
            let event = std::fs::read(&path)
                .and_then(|bytes| ChannelEvent::decode(bytes.as_slice()).map_err(io::Error::other));
            match event {
                Ok(event) if state.by_id.contains_key(&event.event_id) => {
                    remove_if_exists(&path)?;
                }
                Ok(event) => {
                    blobs.claim(&attachments(&event));
                    state.by_id.insert(event.event_id.clone(), seq);
                    state.pending.insert(seq, event);
                }
                Err(e) => {
                    tracing::error!(path = %path.display(), "unreadable unacked event, set aside: {e}");
                    std::fs::rename(&path, path.with_extension("corrupt"))?;
                }
            }
        }
        let swept = blobs.sweep()?;
        if swept > 0 {
            tracing::info!(count = swept, "deleted blobs no unacked event refers to");
        }
        let last = state.next_seq - 1;
        Ok(Self {
            dirs,
            blobs,
            state: Mutex::new(state),
            pushed: watch::Sender::new(last),
        })
    }

    /// The blob store whose inbound blobs the spool's events refer to.
    pub(crate) fn blobs(&self) -> &Blobs {
        &self.blobs
    }

    /// Persists an event and offers it to the streams. Returns `false`, writing nothing, if an
    /// event with the same id is already pending (a channel sending it again); the repeat's
    /// attachments are then given up ([`Blobs::abandon`]), since the pending copy has its own.
    pub(crate) async fn push(&self, event: ChannelEvent) -> io::Result<bool> {
        let seq = {
            let mut state = self.lock();
            if state.by_id.contains_key(&event.event_id) {
                None
            } else {
                let seq = state.next_seq;
                state.next_seq += 1;
                state.by_id.insert(event.event_id.clone(), seq);
                Some(seq)
            }
        };
        let Some(seq) = seq else {
            // Only logged: the event is handled, and a retry would give the puts up twice.
            if let Err(e) = self.blobs.abandon(attachments(&event)).await {
                tracing::warn!(event_id = event.event_id, "deleting a repeat's blobs: {e}");
            }
            return Ok(false);
        };
        let bytes = event.encode_to_vec();
        let (dirs, path) = (Arc::clone(&self.dirs), self.path(seq));
        let written = tokio::task::spawn_blocking(move || {
            let tmp = dirs.tmp_path();
            write_synced(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)?;
            sync_dir(&dirs.events)
        })
        .await
        .map_err(io::Error::other)
        .flatten();
        let mut state = self.lock();
        if let Err(e) = written {
            state.by_id.remove(&event.event_id);
            return Err(e);
        }
        // Claimed before the event is visible, so its ack can't come first.
        self.blobs.claim(&attachments(&event));
        state.pending.insert(seq, event);
        drop(state);
        self.pushed.send_replace(seq);
        Ok(true)
    }

    /// Forgets an acked event and deletes the inbound blobs no other unacked event refers to.
    /// Returns whether it was pending.
    pub(crate) async fn ack(&self, event_id: &str) -> io::Result<bool> {
        let (seq, event) = {
            let mut state = self.lock();
            // An event still being written isn't pending yet: it can't have been sent, so it
            // can't be acked either.
            let Some(&seq) = state.by_id.get(event_id) else {
                return Ok(false);
            };
            let Some(event) = state.pending.remove(&seq) else {
                return Ok(false);
            };
            state.by_id.remove(event_id);
            (seq, event)
        };
        let path = self.path(seq);
        tokio::task::spawn_blocking(move || remove_if_exists(&path))
            .await
            .map_err(io::Error::other)
            .flatten()?;
        // A crash before this leaves the blobs to the next start's sweep.
        self.blobs.unclaim(attachments(&event)).await?;
        Ok(true)
    }

    /// The pending events after sequence number `seq`, in order.
    pub(crate) fn after(&self, seq: u64) -> Vec<(u64, ChannelEvent)> {
        self.lock()
            .pending
            .range(seq + 1..)
            .map(|(seq, event)| (*seq, event.clone()))
            .collect()
    }

    /// Changes whenever an event is pushed.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.pushed.subscribe()
    }

    fn path(&self, seq: u64) -> PathBuf {
        self.dirs.events.join(format!("{seq:020}.pb"))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("the spool lock is never poisoned")
    }
}

/// The sequence number in an event file's name, `<seq>.pb`.
fn seq_of(path: &Path) -> Option<u64> {
    if path.extension()? != "pb" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    stem.bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| stem.parse().ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use giles_proto::channel::v1::{ChannelStatus, Connected, InboundMessage, channel_status};
    use giles_proto::types::v1::BlobRef;

    use super::*;

    fn event(id: &str) -> ChannelEvent {
        ChannelEvent {
            event_id: id.into(),
            event: Some(channel_event::Event::Status(ChannelStatus {
                state: Some(channel_status::State::Connected(Connected {})),
            })),
        }
    }

    /// A message event with these attachments.
    fn message(id: &str, attachments: &[&MediaId]) -> ChannelEvent {
        ChannelEvent {
            event_id: id.into(),
            event: Some(channel_event::Event::Message(InboundMessage {
                attachments: attachments
                    .iter()
                    .map(|id| BlobRef {
                        id: id.to_string(),
                        media_type: "text/plain".into(),
                        size: 1,
                        name: None,
                    })
                    .collect(),
                ..InboundMessage::default()
            })),
        }
    }

    fn open_with_blobs(data: &Path) -> (Spool, Blobs) {
        let dirs = Arc::new(SdkDirs::open(data).unwrap());
        let blobs = Blobs::new(Arc::clone(&dirs));
        (Spool::open(dirs, blobs.clone()).unwrap(), blobs)
    }

    fn open(data: &Path) -> Spool {
        open_with_blobs(data).0
    }

    fn ids(events: &[(u64, ChannelEvent)]) -> Vec<&str> {
        events.iter().map(|(_, e)| e.event_id.as_str()).collect()
    }

    fn files(data: &Path) -> usize {
        std::fs::read_dir(data.join("giles-sdk/events"))
            .unwrap()
            .count()
    }

    #[tokio::test]
    async fn pushed_events_are_on_disk_in_order_until_acked() {
        let data = tempfile::tempdir().unwrap();
        let spool = open(data.path());
        let mut pushed = spool.subscribe();
        assert!(spool.push(event("a")).await.unwrap());
        assert!(pushed.has_changed().unwrap());
        assert_eq!(*pushed.borrow_and_update(), 1);
        assert!(spool.push(event("b")).await.unwrap());
        assert!(spool.push(event("c")).await.unwrap());
        assert_eq!(files(data.path()), 3);

        let all = spool.after(0);
        assert_eq!(ids(&all), ["a", "b", "c"]);
        assert_eq!(ids(&spool.after(all[0].0)), ["b", "c"]);

        assert!(spool.ack("b").await.unwrap());
        assert!(!spool.ack("b").await.unwrap(), "acking twice is harmless");
        assert!(!spool.ack("zzz").await.unwrap());
        assert_eq!(ids(&spool.after(0)), ["a", "c"]);
        assert_eq!(files(data.path()), 2);
    }

    #[tokio::test]
    async fn a_pending_id_is_not_stored_twice_but_an_acked_one_is_again() {
        let data = tempfile::tempdir().unwrap();
        let spool = open(data.path());
        assert!(spool.push(event("a")).await.unwrap());
        assert!(!spool.push(event("a")).await.unwrap());
        assert_eq!(files(data.path()), 1);
        spool.ack("a").await.unwrap();
        assert!(spool.push(event("a")).await.unwrap());
        assert_eq!(ids(&spool.after(0)), ["a"]);
    }

    #[tokio::test]
    async fn reopening_restores_pending_events_and_continues_the_sequence() {
        let data = tempfile::tempdir().unwrap();
        let spool = open(data.path());
        for id in ["a", "b", "c"] {
            spool.push(event(id)).await.unwrap();
        }
        spool.ack("a").await.unwrap();
        drop(spool);

        let spool = open(data.path());
        let pending = spool.after(0);
        assert_eq!(ids(&pending), ["b", "c"]);
        assert_eq!(*spool.subscribe().borrow(), 3);
        spool.push(event("d")).await.unwrap();
        let after = spool.after(pending[1].0);
        assert_eq!(ids(&after), ["d"]);
        assert_eq!(after[0].0, 4);
        assert!(
            !spool.push(event("b")).await.unwrap(),
            "still pending after the restart"
        );
    }

    #[tokio::test]
    async fn corrupt_and_duplicate_files_are_set_aside_on_open() {
        let data = tempfile::tempdir().unwrap();
        let spool = open(data.path());
        spool.push(event("a")).await.unwrap();
        drop(spool);
        let events = data.path().join("giles-sdk/events");
        std::fs::write(
            events.join("00000000000000000007.pb"),
            b"\xff\xff not protobuf",
        )
        .unwrap();
        std::fs::copy(
            events.join("00000000000000000001.pb"),
            events.join("00000000000000000009.pb"),
        )
        .unwrap();
        std::fs::write(events.join("notes.txt"), "").unwrap();

        let spool = open(data.path());
        assert_eq!(ids(&spool.after(0)), ["a"]);
        assert!(events.join("00000000000000000007.corrupt").exists());
        assert!(!events.join("00000000000000000009.pb").exists());
        spool.push(event("b")).await.unwrap();
        assert_eq!(
            spool.after(1)[0].0,
            10,
            "numbering continues past every file seen"
        );
    }

    #[tokio::test]
    async fn an_ack_deletes_blobs_only_the_acked_event_used() {
        let data = tempfile::tempdir().unwrap();
        let (spool, blobs) = open_with_blobs(data.path());
        let shared = blobs.put("shared", "text/plain", None).await.unwrap().id;
        let own = blobs.put("own", "text/plain", None).await.unwrap().id;
        spool.push(message("a", &[&shared, &own])).await.unwrap();
        let shared_again = blobs.put("shared", "text/plain", None).await.unwrap().id;
        spool.push(message("b", &[&shared_again])).await.unwrap();

        assert!(spool.ack("a").await.unwrap());
        assert!(blobs.inbound(&own).await.unwrap().is_none());
        assert!(
            blobs.inbound(&shared).await.unwrap().is_some(),
            "b still uses it"
        );
        // However long b waits (the core's inbox is down), a release doesn't take it.
        blobs.release(&shared).await.unwrap();
        assert!(blobs.inbound(&shared).await.unwrap().is_some());

        assert!(spool.ack("b").await.unwrap());
        assert!(blobs.inbound(&shared).await.unwrap().is_none());
        // A second ack of the same event changes nothing.
        assert!(!spool.ack("b").await.unwrap());
    }

    #[tokio::test]
    async fn a_repeated_event_gives_its_puts_up() {
        let data = tempfile::tempdir().unwrap();
        let (spool, blobs) = open_with_blobs(data.path());
        let first = blobs.put("hello", "text/plain", None).await.unwrap().id;
        assert!(spool.push(message("a", &[&first])).await.unwrap());
        // The channel emits the same event again, storing its attachment again.
        let again = blobs.put("hello", "text/plain", None).await.unwrap().id;
        assert!(!spool.push(message("a", &[&again])).await.unwrap());
        assert!(
            blobs.inbound(&first).await.unwrap().is_some(),
            "a is still pending"
        );

        assert!(spool.ack("a").await.unwrap());
        assert!(blobs.inbound(&first).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn abandoning_a_put_deletes_only_unused_blobs() {
        let data = tempfile::tempdir().unwrap();
        let (spool, blobs) = open_with_blobs(data.path());
        let spooled = blobs.put("spooled", "text/plain", None).await.unwrap().id;
        spool.push(message("a", &[&spooled])).await.unwrap();
        // Dropped events (the protocol can't carry them) with the same and other bytes.
        let same = blobs.put("spooled", "text/plain", None).await.unwrap().id;
        let other = blobs.put("other", "text/plain", None).await.unwrap().id;
        spool
            .blobs()
            .abandon(vec![same.clone(), other.clone()])
            .await
            .unwrap();
        assert!(blobs.inbound(&spooled).await.unwrap().is_some());
        assert!(blobs.inbound(&other).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_crash_between_an_ack_and_the_deletion_leaks_nothing() {
        let data = tempfile::tempdir().unwrap();
        let (spool, blobs) = open_with_blobs(data.path());
        let acked = blobs.put("acked", "text/plain", None).await.unwrap().id;
        let pending = blobs.put("pending", "text/plain", None).await.unwrap().id;
        spool.push(message("a", &[&acked])).await.unwrap();
        spool.push(message("b", &[&pending])).await.unwrap();
        // The ack removed a's file, then the plugin died before deleting its blob.
        let first = spool.after(0)[0].0;
        std::fs::remove_file(spool.path(first)).unwrap();
        drop(spool);

        let (spool, blobs) = open_with_blobs(data.path());
        assert_eq!(ids(&spool.after(0)), ["b"]);
        assert!(blobs.inbound(&acked).await.unwrap().is_none());
        assert!(blobs.inbound(&pending).await.unwrap().is_some());
        // The restored event still owns its blob.
        blobs.release(&pending).await.unwrap();
        assert!(blobs.inbound(&pending).await.unwrap().is_some());
        spool.ack("b").await.unwrap();
        assert!(blobs.inbound(&pending).await.unwrap().is_none());
    }

    #[test]
    fn only_numbered_pb_files_are_events() {
        assert_eq!(seq_of(Path::new("/e/00000000000000000042.pb")), Some(42));
        for name in ["42.json", "x.pb", "-1.pb", "42", "42.pb.corrupt", "+4.pb"] {
            assert_eq!(seq_of(&Path::new("/e").join(name)), None, "{name}");
        }
    }
}
