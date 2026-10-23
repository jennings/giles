//! One-time login links (PLAN section 3.4): `giles admin login-link --user <user>` prints a URL
//! that logs its first visitor in, once, within [`LOGIN_LINK_TTL`].
//!
//! Links live in memory only, by the SHA-256 of their token: a restart invalidates every link
//! (they're short-lived, and the operator can create another), and nothing on disk can be replayed.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use giles_types::{Timestamp, UserId};
use sha2::{Digest, Sha256};

use super::{lock, random_hex};

/// How long a login link works.
pub const LOGIN_LINK_TTL: Duration = Duration::from_secs(10 * 60);

/// Whether `token` has the form of a login-link token (256 random bits as 64 lowercase hex).
pub(super) fn is_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Debug)]
struct Pending {
    user: UserId,
    expires_at: Timestamp,
}

/// The unused, unexpired login links.
#[derive(Debug, Default)]
pub(super) struct LoginLinks {
    links: Mutex<HashMap<[u8; 32], Pending>>,
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl LoginLinks {
    /// A new link for `user`: its token and when it expires.
    pub(super) fn create(&self, user: UserId, now: Timestamp) -> (String, Timestamp) {
        let token = random_hex(32);
        let expires_at = now
            .checked_add(LOGIN_LINK_TTL)
            .expect("ten minutes from now is a valid time");
        let mut links = lock(&self.links);
        links.retain(|_, link| link.expires_at > now);
        links.insert(digest(&token), Pending { user, expires_at });
        (token, expires_at)
    }

    /// When the link `token` expires, if it would log someone in now (asking doesn't use it up).
    pub(super) fn expires_at(&self, token: &str, now: Timestamp) -> Option<Timestamp> {
        lock(&self.links)
            .get(&digest(token))
            .map(|link| link.expires_at)
            .filter(|expires_at| *expires_at > now)
    }

    /// Uses up the link `token`: its user, unless it doesn't exist, was used, or expired.
    pub(super) fn redeem(&self, token: &str, now: Timestamp) -> Option<UserId> {
        let mut links = lock(&self.links);
        links.retain(|_, link| link.expires_at > now);
        links.remove(&digest(token)).map(|link| link.user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + secs).unwrap()
    }

    fn owner() -> UserId {
        UserId::new("owner").unwrap()
    }

    #[test]
    fn a_link_works_once_within_ten_minutes() {
        let links = LoginLinks::default();
        let (token, expires_at) = links.create(owner(), at(0));
        assert!(is_token(&token), "{token}");
        assert_eq!(expires_at, at(600));
        assert_eq!(links.expires_at(&token, at(599)), Some(at(600)));
        assert!(
            links.expires_at(&token, at(1)).is_some(),
            "asking doesn't use it up"
        );
        assert_eq!(links.redeem(&token, at(599)), Some(owner()));
        assert_eq!(links.expires_at(&token, at(599)), None);
        assert_eq!(links.redeem(&token, at(599)), None, "single use");
    }

    #[test]
    fn expired_and_unknown_links_log_nobody_in() {
        let links = LoginLinks::default();
        let (token, _) = links.create(owner(), at(0));
        assert_eq!(links.expires_at(&token, at(600)), None);
        assert_eq!(links.redeem(&token, at(600)), None);
        assert_eq!(links.redeem(&"0".repeat(64), at(1)), None);
        let (a, _) = links.create(owner(), at(0));
        let (b, _) = links.create(owner(), at(0));
        assert_ne!(a, b);
        // Expired links are dropped as new ones are made.
        links.create(owner(), at(700));
        assert_eq!(lock(&links.links).len(), 1);
        assert!(!is_token("ABC") && !is_token(&"A".repeat(64)));
    }
}
