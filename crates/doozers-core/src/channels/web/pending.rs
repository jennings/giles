//! A table of things in flight that the server must remember between two requests of one
//! browser: a passkey ceremony ([`passkey`](super::passkey)) or an OpenID Connect sign-in
//! ([`oidc`](super::oidc)).
//!
//! An entry is kept in memory under a random id the page sends back, for a time to live, once:
//! [`Pending::finish`] takes the entry out whether or not it matches. An entry is bound to what
//! started it, a string the finishing request must repeat. The table holds at most a fixed number
//! of entries; past that the oldest go, so memory is bounded however many are started. A restart
//! forgets them, and the page says to try again.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use doozers_types::Timestamp;
use sha2::{Digest, Sha256};

use super::random_hex;

struct Entry<T> {
    /// When it stops being valid, in Unix milliseconds.
    expires_at: i64,
    bound: String,
    value: T,
}

/// The entries in flight ([module docs](self)).
pub(super) struct Pending<T> {
    entries: Mutex<HashMap<String, Entry<T>>>,
    ttl_ms: i64,
    max: usize,
}

impl<T> Pending<T> {
    /// A table whose entries live for `ttl`, and which holds at most `max` of them.
    pub(super) fn new(ttl: Duration, max: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl_ms: i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX),
            max,
        }
    }

    /// Keeps `value`, bound to `bound`, and returns the id to finish it with: 128 random bits as
    /// hex. Expired entries go first, then, at the cap, the oldest.
    pub(super) fn begin(&self, bound: &str, value: T, now: Timestamp) -> String {
        let now = now.as_millisecond();
        let id = random_hex(16);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.len() >= self.max {
            entries.retain(|_, entry| entry.expires_at > now);
        }
        while entries.len() >= self.max {
            let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            entries.remove(&oldest);
        }
        entries.insert(
            id.clone(),
            Entry {
                expires_at: now.saturating_add(self.ttl_ms),
                bound: bound.to_owned(),
                value,
            },
        );
        id
    }

    /// Takes the entry `id` out, if it is there, hasn't expired, and was bound to `bound`. The
    /// entry is gone either way, so a guess at an id costs the one it guessed.
    pub(super) fn finish(&self, id: &str, bound: &str, now: Timestamp) -> Option<T> {
        let now = now.as_millisecond();
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)?;
        (entry.expires_at > now && same(&entry.bound, bound)).then_some(entry.value)
    }

    /// How many entries are kept.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// Whether two bindings are equal, compared as digests so the time taken says nothing about how
/// much of one matched.
fn same(a: &str, b: &str) -> bool {
    Sha256::digest(a.as_bytes()) == Sha256::digest(b.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).unwrap()
    }

    const TTL: Duration = Duration::from_secs(360);

    #[test]
    fn an_entry_finishes_once_for_what_started_it() {
        let pending = Pending::new(TTL, 100);
        let id = pending.begin("ann", "value", at(0));
        assert_eq!(id.len(), 32);
        assert_eq!(pending.finish(&id, "ann", at(1)), Some("value"));
        assert_eq!(pending.finish(&id, "ann", at(2)), None, "used once");
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn an_entry_finished_by_another_binding_or_late_is_gone() {
        let pending = Pending::new(TTL, 100);
        let id = pending.begin("ann", "value", at(0));
        assert_eq!(pending.finish(&id, "bob", at(1)), None);
        assert_eq!(
            pending.finish(&id, "ann", at(2)),
            None,
            "the wrong guess spent it"
        );
        let id = pending.begin("ann", "value", at(0));
        let late = at(TTL.as_secs() as i64);
        assert_eq!(pending.finish(&id, "ann", late), None);
        assert_eq!(pending.finish("nope", "ann", at(1)), None);
    }

    #[test]
    fn the_table_stays_at_its_cap_dropping_what_expired_and_then_the_oldest() {
        let pending = Pending::new(Duration::from_secs(60), 3);
        let first = pending.begin("ann", 1, at(0));
        let second = pending.begin("ann", 2, at(10));
        let third = pending.begin("ann", 3, at(20));
        // Full, with none expired: the oldest goes.
        let fourth = pending.begin("ann", 4, at(30));
        assert_eq!(pending.len(), 3);
        assert_eq!(pending.finish(&first, "ann", at(31)), None);
        assert_eq!(pending.finish(&second, "ann", at(31)), Some(2));
        // Two are left and one fits. Everything old enough has expired: only live ones count.
        let fifth = pending.begin("ann", 5, at(32));
        assert_eq!(pending.len(), 3);
        let sixth = pending.begin("ann", 6, at(91));
        assert_eq!(
            pending.len(),
            2,
            "third and fourth expired and were dropped"
        );
        assert_eq!(pending.finish(&third, "ann", at(91)), None);
        assert_eq!(pending.finish(&fourth, "ann", at(91)), None);
        assert_eq!(pending.finish(&fifth, "ann", at(91)), Some(5));
        assert_eq!(pending.finish(&sixth, "ann", at(91)), Some(6));
    }
}
