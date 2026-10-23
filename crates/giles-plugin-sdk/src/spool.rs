//! Channel events the core hasn't acked yet, persisted before they're sent (PLAN section 8.5).
//!
//! One file per event, `events/<seq>.pb`, holding the encoded wire `ChannelEvent`: exactly what goes
//! on the stream. Sequence numbers keep the order across restarts. A file is written to `tmp/`,
//! synced, renamed into place, and the directory synced, before the event is offered to any
//! `Events` stream; it's deleted when the core acks the event.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use giles_proto::channel::v1::ChannelEvent;
use prost::Message as _;
use tokio::sync::watch;

use crate::blobs::write_synced;
use crate::dirs::{SdkDirs, remove_if_exists, sync_dir};

/// The persistent queue of unacked events.
pub(crate) struct Spool {
    dirs: Arc<SdkDirs>,
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

impl Spool {
    /// Loads the events left by earlier runs. A file that doesn't decode is renamed to
    /// `<name>.corrupt`, logged, and skipped; a second file with an id already loaded is deleted.
    pub(crate) fn open(dirs: Arc<SdkDirs>) -> io::Result<Self> {
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
                    state.by_id.insert(event.event_id.clone(), seq);
                    state.pending.insert(seq, event);
                }
                Err(e) => {
                    tracing::error!(path = %path.display(), "unreadable unacked event, set aside: {e}");
                    std::fs::rename(&path, path.with_extension("corrupt"))?;
                }
            }
        }
        let last = state.next_seq - 1;
        Ok(Self {
            dirs,
            state: Mutex::new(state),
            pushed: watch::Sender::new(last),
        })
    }

    /// Persists an event and offers it to the streams. Returns `false`, writing nothing, if an
    /// event with the same id is already pending (a channel sending it again).
    pub(crate) async fn push(&self, event: ChannelEvent) -> io::Result<bool> {
        let seq = {
            let mut state = self.lock();
            if state.by_id.contains_key(&event.event_id) {
                return Ok(false);
            }
            let seq = state.next_seq;
            state.next_seq += 1;
            state.by_id.insert(event.event_id.clone(), seq);
            seq
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
        state.pending.insert(seq, event);
        drop(state);
        self.pushed.send_replace(seq);
        Ok(true)
    }

    /// Forgets an acked event. Returns whether it was pending.
    pub(crate) async fn ack(&self, event_id: &str) -> io::Result<bool> {
        let seq = {
            let mut state = self.lock();
            // An event still being written isn't pending yet: it can't have been sent, so it
            // can't be acked either.
            let Some(&seq) = state.by_id.get(event_id) else {
                return Ok(false);
            };
            if state.pending.remove(&seq).is_none() {
                return Ok(false);
            }
            state.by_id.remove(event_id);
            seq
        };
        let path = self.path(seq);
        tokio::task::spawn_blocking(move || remove_if_exists(&path))
            .await
            .map_err(io::Error::other)
            .flatten()?;
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
    use giles_proto::channel::v1::{ChannelStatus, Connected, channel_event, channel_status};

    use super::*;

    fn event(id: &str) -> ChannelEvent {
        ChannelEvent {
            event_id: id.into(),
            event: Some(channel_event::Event::Status(ChannelStatus {
                state: Some(channel_status::State::Connected(Connected {})),
            })),
        }
    }

    fn open(data: &Path) -> Spool {
        Spool::open(Arc::new(SdkDirs::open(data).unwrap())).unwrap()
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

    #[test]
    fn only_numbered_pb_files_are_events() {
        assert_eq!(seq_of(Path::new("/e/00000000000000000042.pb")), Some(42));
        for name in ["42.json", "x.pb", "-1.pb", "42", "42.pb.corrupt", "+4.pb"] {
            assert_eq!(seq_of(&Path::new("/e").join(name)), None, "{name}");
        }
    }
}
