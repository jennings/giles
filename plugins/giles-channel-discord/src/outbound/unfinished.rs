//! What the sends that haven't finished got through, so a retry carries on instead of posting the
//! messages that are already in the DM again.
//!
//! Discord has no idempotency key for a message, and the outbox retries a whole send until it
//! succeeds or its row expires (24 hours). Without this, a part that fails every time (Discord's
//! harmful-links filter, 240000, blocks a link in part 3) would make every retry post parts 1 and
//! 2 again, over and over, and a reply that takes longer than the outbox's 60 seconds would be cut
//! at the same part every time and repeat its start.
//!
//! A send is identified by where it goes and a hash of the messages it posts (each one's text and
//! the files it carries, by their content ids, media types, and names); the entry counts the
//! messages Discord accepted (a message with files counts like any other: one request, one
//! part), is updated after each one (so a send the outbox drops at its timeout leaves its progress
//! too), and is removed when the last one is accepted, so a genuine second reply with the same
//! text is sent in full. Only unfinished sends are remembered, at most [`MAX_UNFINISHED`] of them,
//! each for [`UNFINISHED_TTL`] after its last progress. In memory: a restart forgets them, and
//! the retry then posts the messages again once. `Outbound` keeps only sends to a user: nothing
//! retries a reply into a conversation, so an entry for one would only hold a slot for a day.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher as _};
use std::time::Duration;

use tokio::time::Instant;

use super::Destination;

/// The most unfinished sends remembered; the one that made progress longest ago is forgotten for a
/// new one. A handful is normal: one per message stuck in the outbox.
pub const MAX_UNFINISHED: usize = 64;

/// How long a send is remembered after its last progress: as long as the outbox keeps a row.
pub const UNFINISHED_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Which send an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    destination: Destination,
    /// A hash of the posts. 64 bits of `SipHash`: a collision would need two different
    /// unfinished sends to one destination to hash alike.
    digest: u64,
}

impl Key {
    pub fn new(destination: Destination, parts: &[impl Hash]) -> Self {
        let mut hasher = DefaultHasher::new();
        parts.hash(&mut hasher);
        Self {
            destination,
            digest: hasher.finish(),
        }
    }
}

struct Entry {
    /// The parts Discord has accepted.
    sent: usize,
    /// When the last one was.
    at: Instant,
}

/// The unfinished sends.
#[derive(Default)]
pub struct Unfinished {
    entries: HashMap<Key, Entry>,
}

impl Unfinished {
    /// How many parts of the send `key` Discord has already accepted: where to carry on.
    pub fn resume(&mut self, key: &Key) -> usize {
        let Some(entry) = self.entries.get(key) else {
            return 0;
        };
        if entry.at.elapsed() < UNFINISHED_TTL {
            return entry.sent;
        }
        self.entries.remove(key);
        0
    }

    /// Notes that `sent` of `total` parts of the send `key` are accepted. All of them ends the
    /// send: nothing is remembered.
    pub fn record(&mut self, key: Key, sent: usize, total: usize) {
        if sent >= total {
            self.entries.remove(&key);
            return;
        }
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.at) < UNFINISHED_TTL);
        if !self.entries.contains_key(&key) && self.entries.len() >= MAX_UNFINISHED {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, Entry { sent, at: now });
    }

    /// Forgets the send `key`: it failed in a way a retry won't fix, and the outbox drops it, so
    /// the same text sent later is a new message.
    pub fn forget(&mut self, key: &Key) {
        self.entries.remove(key);
    }

    /// How many sends are remembered.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u64) -> Key {
        Key::new(Destination::Channel(7), &[format!("send {n}")])
    }

    #[tokio::test(start_paused = true)]
    async fn progress_is_kept_until_the_last_part_and_then_forgotten() {
        let mut unfinished = Unfinished::default();
        assert_eq!(unfinished.resume(&key(1)), 0);
        unfinished.record(key(1), 1, 3);
        assert_eq!(unfinished.resume(&key(1)), 1);
        unfinished.record(key(1), 2, 3);
        assert_eq!(unfinished.resume(&key(1)), 2);
        unfinished.record(key(1), 3, 3);
        assert_eq!(
            unfinished.resume(&key(1)),
            0,
            "a finished send leaves nothing"
        );
        assert_eq!(unfinished.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_send_is_told_apart_by_destination_and_text() {
        let mut unfinished = Unfinished::default();
        let parts = vec!["a".to_owned(), "b".to_owned()];
        let here = Key::new(Destination::Channel(1), &parts);
        unfinished.record(here, 1, 2);
        assert_eq!(unfinished.resume(&here), 1);
        assert_eq!(
            unfinished.resume(&Key::new(Destination::Channel(2), &parts)),
            0
        );
        assert_eq!(
            unfinished.resume(&Key::new(Destination::User(1), &parts)),
            0
        );
        let other = vec!["a".to_owned(), "c".to_owned()];
        assert_eq!(
            unfinished.resume(&Key::new(Destination::Channel(1), &other)),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_oldest_is_forgotten_when_the_map_is_full() {
        let mut unfinished = Unfinished::default();
        for n in 0..MAX_UNFINISHED as u64 {
            unfinished.record(key(n), 1, 2);
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert_eq!(unfinished.len(), MAX_UNFINISHED);
        // New progress for a remembered send replaces its entry; nothing is evicted.
        unfinished.record(key(0), 1, 2);
        assert_eq!(unfinished.len(), MAX_UNFINISHED);
        assert_eq!(unfinished.resume(&key(1)), 1);

        // A new send makes room by dropping the one that made progress longest ago: key 1, since
        // key 0 has just been refreshed.
        let new = key(1_000);
        unfinished.record(new, 1, 2);
        assert_eq!(unfinished.len(), MAX_UNFINISHED);
        assert_eq!(unfinished.resume(&new), 1);
        assert_eq!(unfinished.resume(&key(1)), 0, "the oldest was evicted");
        assert_eq!(unfinished.resume(&key(0)), 1, "a refreshed send was kept");
        assert_eq!(unfinished.resume(&key(2)), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn many_sends_never_grow_the_map_past_its_bound() {
        let mut unfinished = Unfinished::default();
        for n in 0..10_000 {
            unfinished.record(key(n), 1, 2);
            assert!(unfinished.len() <= MAX_UNFINISHED);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn progress_expires_a_day_after_the_last_part() {
        let mut unfinished = Unfinished::default();
        unfinished.record(key(1), 1, 3);
        tokio::time::advance(UNFINISHED_TTL - Duration::from_secs(1)).await;
        assert_eq!(unfinished.resume(&key(1)), 1);
        // Progress moves the expiry.
        unfinished.record(key(1), 2, 3);
        tokio::time::advance(UNFINISHED_TTL - Duration::from_secs(1)).await;
        assert_eq!(unfinished.resume(&key(1)), 2);
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            unfinished.resume(&key(1)),
            0,
            "the outbox has long given up"
        );
        assert_eq!(unfinished.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_entries_make_room_before_anything_live_is_evicted() {
        let mut unfinished = Unfinished::default();
        for n in 0..MAX_UNFINISHED as u64 {
            unfinished.record(key(n), 1, 2);
        }
        tokio::time::advance(UNFINISHED_TTL + Duration::from_secs(1)).await;
        unfinished.record(key(1_000), 1, 2);
        assert_eq!(unfinished.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_forgotten_send_starts_over() {
        let mut unfinished = Unfinished::default();
        unfinished.record(key(1), 2, 3);
        unfinished.forget(&key(1));
        assert_eq!(unfinished.resume(&key(1)), 0);
    }
}
