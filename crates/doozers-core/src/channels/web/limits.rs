//! Limits on password guessing: who a client is, and how many wrong passwords one username and one
//! client address get. Design: `Docs/technical/web-channel.md`.
//!
//! The limits are in memory, so a restart forgets them, and they are per process. A guess costs the
//! attacker one request and the server an Argon2 hash, so the limits sit in front of the hash:
//! an attempt over a limit is refused without hashing anything.
//!
//! - Per username: [`USERNAME_ATTEMPTS`] attempts per [`WINDOW`]. A username that exists and one
//!   that doesn't count alike, so the limit doesn't say which is which.
//! - Per client address: [`ADDRESS_ATTEMPTS`] attempts per [`WINDOW`], shared by every username.
//!   IPv6 addresses count by their /64, the smallest network an ISP gives out, so one subscriber
//!   can't take a fresh address for every guess. A connection from a loopback address to the main
//!   listener is a reverse proxy on this machine, whose peer address is every visitor's, so it has
//!   no address limit (the username limit still applies); the trusted-proxy listener tells visitors
//!   apart by `X-Forwarded-For`.
//!
//! A window starts at the first attempt after the previous one ended and doesn't move: a refused
//! attempt isn't counted, so someone who keeps trying can't keep themselves locked out, or anyone
//! else. A successful sign-in forgets the username's count and gives the address its attempt back.
//! While a username is over its limit not even its right password gets in, which is the cost of a
//! limit that doesn't depend on guessing right: whoever knows a username can lock it for the rest of
//! the window, and the address limit makes that cost the attacker more than the user. The user can
//! still sign in with an invite or recovery link.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::Duration;

use axum::http::HeaderMap;
use doozers_types::Timestamp;

use crate::http::PeerAddr;

/// How many password attempts one username gets per [`WINDOW`].
pub(super) const USERNAME_ATTEMPTS: u32 = 10;

/// How many password attempts one client address gets per [`WINDOW`].
pub(super) const ADDRESS_ATTEMPTS: u32 = 30;

/// How long a count lasts.
pub(super) const WINDOW: Duration = Duration::from_secs(15 * 60);

/// The most usernames or addresses counted at once. Past it, windows that ended go first, then the
/// entries with the fewest attempts, and never one that is at its limit: whoever sends enough fresh
/// names can't push a locked username out. When every entry is at its limit a new key is refused,
/// so memory stays bounded however many names someone tries.
const MAX_TRACKED: usize = 10_000;

/// Which client a request is from, for counting attempts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ClientKey {
    /// An IPv4 address.
    V4(std::net::Ipv4Addr),
    /// The first 64 bits of an IPv6 address.
    V6([u8; 8]),
    /// The request didn't say: every such client counts together.
    Unknown,
    /// A loopback peer of the main listener: a reverse proxy on this machine. It has no limit.
    Loopback,
}

impl ClientKey {
    fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(ip) => Self::V4(ip),
            IpAddr::V6(ip) => {
                let mut prefix = [0; 8];
                prefix.copy_from_slice(&ip.octets()[..8]);
                Self::V6(prefix)
            }
        }
    }
}

impl std::fmt::Display for ClientKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::V4(ip) => ip.fmt(f),
            Self::V6(prefix) => {
                let mut octets = [0; 16];
                octets[..8].copy_from_slice(prefix);
                write!(f, "{}/64", Ipv6Addr::from(octets))
            }
            Self::Unknown => f.write_str("unknown"),
            Self::Loopback => f.write_str("loopback"),
        }
    }
}

/// The client of a request.
///
/// On the main listener it is the peer of the connection, or [`ClientKey::Loopback`] if that is
/// a loopback address: a reverse proxy on this machine stands for every visitor, so counting by its
/// address would let anyone lock everyone out. On the trusted-proxy listener the peer is
/// the proxy, so, as with the identity header and `X-Forwarded-Proto`, the proxy is believed: the
/// client is the last address of `X-Forwarded-For`, the one the proxy added itself (anything before
/// it is whatever the client sent). A trusted request without a usable header falls back to the
/// peer, so a proxy that doesn't forward addresses makes all its clients count together.
pub(super) fn client_key(trusted: bool, headers: &HeaderMap, peer: Option<&PeerAddr>) -> ClientKey {
    let peer = peer.and_then(|peer| peer.0).map(|ip| ip.to_canonical());
    if !trusted {
        return match peer {
            Some(ip) if ip.is_loopback() => ClientKey::Loopback,
            Some(ip) => ClientKey::of(ip),
            None => ClientKey::Unknown,
        };
    }
    headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .and_then(|last| last.trim().parse::<IpAddr>().ok())
        .or(peer)
        .map_or(ClientKey::Unknown, ClientKey::of)
}

/// The count for one key.
#[derive(Debug, Clone, Copy)]
struct Count {
    /// When the window began, in Unix milliseconds.
    since: i64,
    attempts: u32,
    /// Whether an attempt was refused for being at the limit, which has been logged.
    refused: bool,
}

#[derive(Debug, Default)]
struct Counts {
    usernames: HashMap<String, Count>,
    clients: HashMap<ClientKey, Count>,
}

/// Counts password attempts ([module docs](self)).
#[derive(Debug)]
pub(super) struct LoginLimits {
    counts: Mutex<Counts>,
    username_attempts: u32,
    address_attempts: u32,
    window: Duration,
}

impl Default for LoginLimits {
    fn default() -> Self {
        Self::new(USERNAME_ATTEMPTS, ADDRESS_ATTEMPTS, WINDOW)
    }
}

/// An attempt [`LoginLimits::begin`] allowed. Report how it ended with
/// [`succeeded`](LoginLimits::succeeded) or [`cancel`](LoginLimits::cancel); one that is dropped
/// stays counted, which is a failed guess.
#[derive(Debug)]
#[must_use = "report how the attempt ended"]
pub(super) struct Attempt {
    username: String,
    client: ClientKey,
    username_since: i64,
    client_since: i64,
}

/// An attempt a limit refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Limited {
    /// How long until the limit that refused it lets another attempt in.
    pub retry_after: Duration,
    /// Whether this is the first refusal since a key reached its limit, which is the one worth a
    /// warning in the log: the rest of a flood is only noise.
    pub first: bool,
}

/// The username as counted: ASCII case folded, as usernames are compared, and cut to
/// `crate::users::MAX_USERNAME_CHARS` characters (nobody has a longer one, and the count mustn't
/// grow with what someone sends).
pub(super) fn username_key(username: &str) -> String {
    username
        .trim()
        .chars()
        .take(crate::users::MAX_USERNAME_CHARS)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

impl LoginLimits {
    pub(super) fn new(username_attempts: u32, address_attempts: u32, window: Duration) -> Self {
        Self {
            counts: Mutex::new(Counts::default()),
            username_attempts,
            address_attempts,
            window,
        }
    }

    fn window_ms(&self) -> i64 {
        i64::try_from(self.window.as_millis()).unwrap_or(i64::MAX)
    }

    /// Counts an attempt to sign in as `username` (already [`username_key`]ed) from `client` at
    /// `now`, unless either has used up its attempts.
    ///
    /// The attempt is counted before the password is checked, so requests that arrive together
    /// can't all get through on the last attempt.
    pub(super) fn begin(
        &self,
        username: &str,
        client: &ClientKey,
        now: Timestamp,
    ) -> Result<Attempt, Limited> {
        let now = now.as_millisecond();
        let window = self.window_ms();
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let counts = &mut *counts;
        let tracked = *client != ClientKey::Loopback;
        let live = |count: Option<&Count>| {
            count
                .copied()
                .filter(|count| now.saturating_sub(count.since) < window)
        };
        let user = live(counts.usernames.get(username));
        let address = live(counts.clients.get(client)).filter(|_| tracked);

        let retry_in = |count: &Count| count.since.saturating_add(window).saturating_sub(now);
        let mut retry_ms = 0;
        let mut first = false;
        if let Some(count) = user.filter(|count| count.attempts >= self.username_attempts) {
            retry_ms = retry_ms.max(retry_in(&count));
            first |= mark_refused(&mut counts.usernames, &username.to_owned());
        }
        if let Some(count) = address.filter(|count| count.attempts >= self.address_attempts) {
            retry_ms = retry_ms.max(retry_in(&count));
            first |= mark_refused(&mut counts.clients, client);
        }
        if retry_ms > 0 {
            return Err(limited(retry_ms, first));
        }

        let fresh = Count {
            since: now,
            attempts: 0,
            refused: false,
        };
        let user = user.unwrap_or(fresh);
        let address = address.unwrap_or(fresh);
        let username = username.to_owned();
        if let Err(retry_ms) = make_room(
            &mut counts.usernames,
            &username,
            self.username_attempts,
            now,
            window,
        ) {
            return Err(limited(retry_ms, false));
        }
        if tracked
            && let Err(retry_ms) = make_room(
                &mut counts.clients,
                client,
                self.address_attempts,
                now,
                window,
            )
        {
            return Err(limited(retry_ms, false));
        }
        counts.usernames.insert(
            username.clone(),
            Count {
                attempts: user.attempts + 1,
                ..user
            },
        );
        if tracked {
            counts.clients.insert(
                client.clone(),
                Count {
                    attempts: address.attempts + 1,
                    ..address
                },
            );
        }
        Ok(Attempt {
            username,
            client: client.clone(),
            username_since: user.since,
            client_since: address.since,
        })
    }

    /// The attempt signed in: the username starts over, and the address gets the attempt back (a
    /// household where one person mistypes now and then isn't held to a count of its mistakes).
    pub(super) fn succeeded(&self, attempt: Attempt) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if counts
            .usernames
            .get(&attempt.username)
            .is_some_and(|count| count.since == attempt.username_since)
        {
            counts.usernames.remove(&attempt.username);
        }
        give_back(&mut counts.clients, &attempt.client, attempt.client_since);
    }

    /// The attempt never checked a password (the server was too busy, or failed): neither the
    /// username nor the address pays for it.
    pub(super) fn cancel(&self, attempt: Attempt) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        give_back(
            &mut counts.usernames,
            &attempt.username,
            attempt.username_since,
        );
        give_back(&mut counts.clients, &attempt.client, attempt.client_since);
    }
}

/// Takes one attempt off `key`'s count, if the window it was counted in is still the current one.
fn give_back<K: std::hash::Hash + Eq>(counts: &mut HashMap<K, Count>, key: &K, since: i64) {
    if let Some(count) = counts.get_mut(key)
        && count.since == since
    {
        count.attempts = count.attempts.saturating_sub(1);
        if count.attempts == 0 {
            counts.remove(key);
        }
    }
}

/// A refusal that says to wait `retry_ms` milliseconds (at least a second, so that `Retry-After`
/// is never 0).
fn limited(retry_ms: i64, first: bool) -> Limited {
    Limited {
        retry_after: Duration::from_millis(u64::try_from(retry_ms).unwrap_or(0).max(1000)),
        first,
    }
}

/// Records that `key` refused an attempt; whether it was the first to.
fn mark_refused<K: std::hash::Hash + Eq>(counts: &mut HashMap<K, Count>, key: &K) -> bool {
    counts
        .get_mut(key)
        .is_some_and(|count| !std::mem::replace(&mut count.refused, true))
}

/// Makes room for a new `key` in `counts`, which holds [`MAX_TRACKED`] at most, with a limit of
/// `limit` attempts: drops the windows that ended and, if that isn't enough, the entry with the
/// fewest attempts (the oldest of those), but never one at its limit. If every entry is at its
/// limit there is no room, and the error is how many milliseconds until the first window ends.
fn make_room<K: std::hash::Hash + Eq + Clone>(
    counts: &mut HashMap<K, Count>,
    key: &K,
    limit: u32,
    now: i64,
    window: i64,
) -> Result<(), i64> {
    if counts.len() < MAX_TRACKED || counts.contains_key(key) {
        return Ok(());
    }
    counts.retain(|_, count| now.saturating_sub(count.since) < window);
    if counts.len() < MAX_TRACKED {
        return Ok(());
    }
    let evict = counts
        .iter()
        .filter(|(_, count)| count.attempts < limit)
        .min_by_key(|(_, count)| (count.attempts, count.since))
        .map(|(key, _)| key.clone());
    match evict {
        Some(evict) => {
            counts.remove(&evict);
            Ok(())
        }
        None => {
            let first_end = counts
                .values()
                .map(|count| count.since.saturating_add(window).saturating_sub(now))
                .min()
                .unwrap_or(0);
            Err(first_end)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(seconds).unwrap()
    }

    fn v4(last: u8) -> ClientKey {
        ClientKey::V4(std::net::Ipv4Addr::new(192, 0, 2, last))
    }

    fn limits() -> LoginLimits {
        LoginLimits::new(3, 5, Duration::from_secs(60))
    }

    #[test]
    fn a_username_gets_its_attempts_and_then_waits_for_the_window_to_end() {
        let limits = limits();
        for second in 0..3 {
            let _ = limits.begin("ann", &v4(1), at(second)).unwrap();
        }
        // Refused from the fourth attempt on, and told how long is left of the window that began
        // at the first attempt. Refusing doesn't move the window.
        for second in [3, 30, 59] {
            let limited = limits.begin("ann", &v4(1), at(second)).unwrap_err();
            assert_eq!(limited.retry_after, Duration::from_secs(60 - second as u64));
        }
        // Another username, from another address, is unaffected.
        let _ = limits.begin("bob", &v4(2), at(3)).unwrap();
        // The window ended: a fresh count.
        let _ = limits.begin("ann", &v4(1), at(60)).unwrap();
    }

    #[test]
    fn an_address_gets_its_attempts_across_usernames() {
        let limits = limits();
        for (second, name) in ["a", "b", "c", "d", "e"].into_iter().enumerate() {
            let _ = limits.begin(name, &v4(1), at(second as i64)).unwrap();
        }
        let limited = limits.begin("f", &v4(1), at(10)).unwrap_err();
        assert_eq!(limited.retry_after, Duration::from_secs(50));
        // The same name from another address is fine, and counts there only.
        let _ = limits.begin("f", &v4(2), at(10)).unwrap();
        let _ = limits.begin("a", &v4(2), at(10)).unwrap();
    }

    #[test]
    fn the_longer_wait_of_the_two_limits_is_reported() {
        let limits = LoginLimits::new(1, 1, Duration::from_secs(60));
        let _ = limits.begin("ann", &v4(1), at(0)).unwrap();
        let _ = limits.begin("bob", &v4(2), at(30)).unwrap();
        // ann from bob's address: ann's window ends in 30 seconds, bob's address's in 60.
        let limited = limits.begin("ann", &v4(2), at(30)).unwrap_err();
        assert_eq!(limited.retry_after, Duration::from_secs(60));
    }

    #[test]
    fn success_starts_the_username_over_and_returns_the_address_its_attempt() {
        let limits = limits();
        for second in 0..2 {
            let _ = limits.begin("ann", &v4(1), at(second)).unwrap();
        }
        let attempt = limits.begin("ann", &v4(1), at(2)).unwrap();
        limits.succeeded(attempt);
        // Three more attempts for ann; before, she had none left.
        for second in 3..6 {
            let _ = limits.begin("ann", &v4(1), at(second)).unwrap();
        }
        assert!(limits.begin("ann", &v4(1), at(6)).is_err());
        // The address paid for 2 failures and 3 more attempts, not for the success: it has used
        // all 5, where it would have been out a place sooner if the success had counted.
        assert!(limits.begin("bob", &v4(1), at(6)).is_err());
    }

    #[test]
    fn a_cancelled_attempt_costs_nothing() {
        let limits = limits();
        for second in 0..3 {
            let attempt = limits.begin("ann", &v4(1), at(second)).unwrap();
            limits.cancel(attempt);
        }
        for second in 3..6 {
            let _ = limits.begin("ann", &v4(1), at(second)).unwrap();
        }
        assert!(limits.begin("ann", &v4(1), at(6)).is_err());
    }

    #[test]
    fn simultaneous_attempts_can_not_all_take_the_last_place() {
        let limits = limits();
        let outcomes: Vec<_> = (0..6)
            .map(|_| limits.begin("ann", &v4(1), at(0)).is_ok())
            .collect();
        assert_eq!(outcomes.iter().filter(|ok| **ok).count(), 3);
    }

    #[test]
    fn what_is_tracked_stays_bounded() {
        let limits = LoginLimits::new(10, 10, Duration::from_secs(3600));
        for n in 0..MAX_TRACKED + 50 {
            let client = ClientKey::V4(std::net::Ipv4Addr::from(u32::try_from(n).unwrap()));
            let now = Timestamp::from_millisecond(i64::try_from(n).unwrap()).unwrap();
            let _ = limits.begin(&format!("user{n}"), &client, now).unwrap();
        }
        let counts = limits.counts.lock().unwrap();
        assert_eq!(counts.usernames.len(), MAX_TRACKED);
        assert_eq!(counts.clients.len(), MAX_TRACKED);
        // The oldest were dropped, the newest are kept.
        assert!(!counts.usernames.contains_key("user0"));
        assert!(
            counts
                .usernames
                .contains_key(&format!("user{}", MAX_TRACKED + 49))
        );
    }

    fn nth_client(n: usize) -> ClientKey {
        ClientKey::V4(std::net::Ipv4Addr::from(u32::try_from(n).unwrap()))
    }

    #[test]
    fn a_locked_username_survives_any_number_of_fresh_names() {
        let limits = LoginLimits::new(3, 1_000_000, Duration::from_secs(3600));
        for _ in 0..3 {
            let _ = limits.begin("ann", &v4(1), at(0)).unwrap();
        }
        for n in 0..MAX_TRACKED + 50 {
            let now = Timestamp::from_millisecond(1000 + i64::try_from(n).unwrap()).unwrap();
            let _ = limits
                .begin(&format!("fresh{n}"), &nth_client(n), now)
                .unwrap();
        }
        assert_eq!(limits.counts.lock().unwrap().usernames.len(), MAX_TRACKED);
        assert!(
            limits.begin("ann", &v4(1), at(10)).is_err(),
            "ann is still locked"
        );
        // So is a username that has used some of its attempts: fresh names go first.
        let limits = LoginLimits::new(3, 1_000_000, Duration::from_secs(3600));
        let _ = limits.begin("bob", &v4(1), at(0)).unwrap();
        let _ = limits.begin("bob", &v4(1), at(0)).unwrap();
        for n in 0..MAX_TRACKED + 50 {
            let now = Timestamp::from_millisecond(1000 + i64::try_from(n).unwrap()).unwrap();
            let _ = limits
                .begin(&format!("fresh{n}"), &nth_client(n), now)
                .unwrap();
        }
        let _ = limits.begin("bob", &v4(1), at(10)).unwrap();
        assert!(limits.begin("bob", &v4(1), at(10)).is_err());
    }

    #[test]
    fn when_every_entry_is_at_its_limit_a_new_name_is_refused_until_a_window_ends() {
        let limits = LoginLimits::new(1, 1_000_000, Duration::from_secs(3600));
        for n in 0..MAX_TRACKED {
            let _ = limits
                .begin(&format!("name{n}"), &nth_client(n), at(0))
                .unwrap();
        }
        let limited = limits.begin("one-more", &v4(250), at(10)).unwrap_err();
        assert_eq!(limited.retry_after, Duration::from_secs(3590));
        assert_eq!(limits.counts.lock().unwrap().usernames.len(), MAX_TRACKED);
        // A name that is tracked is answered as before, and once the windows end there is room.
        assert!(limits.begin("name5", &v4(251), at(10)).is_err());
        let _ = limits.begin("one-more", &v4(250), at(3600)).unwrap();
    }

    #[test]
    fn only_the_first_refusal_since_a_limit_was_reached_is_first() {
        let limits = limits();
        for _ in 0..3 {
            let _ = limits.begin("ann", &v4(1), at(0)).unwrap();
        }
        assert!(limits.begin("ann", &v4(1), at(1)).unwrap_err().first);
        assert!(!limits.begin("ann", &v4(1), at(2)).unwrap_err().first);
        assert!(!limits.begin("ann", &v4(2), at(3)).unwrap_err().first);
        // A new window that reaches the limit has a first refusal again.
        for _ in 0..3 {
            let _ = limits.begin("ann", &v4(1), at(60)).unwrap();
        }
        assert!(limits.begin("ann", &v4(1), at(61)).unwrap_err().first);
    }

    #[test]
    fn a_loopback_client_has_no_address_limit_but_its_usernames_do() {
        let limits = limits();
        for n in 0..50 {
            let _ = limits
                .begin(&format!("name{n}"), &ClientKey::Loopback, at(0))
                .unwrap();
        }
        assert!(limits.counts.lock().unwrap().clients.is_empty());
        for _ in 0..3 {
            let _ = limits.begin("ann", &ClientKey::Loopback, at(0)).unwrap();
        }
        assert!(limits.begin("ann", &ClientKey::Loopback, at(1)).is_err());
        // Another address isn't affected by the proxy's attempts.
        let _ = limits.begin("bob", &v4(1), at(1)).unwrap();
    }

    #[test]
    fn loopback_peers_of_the_main_listener_are_the_proxy_and_the_rest_are_clients() {
        let none = HeaderMap::new();
        for peer in ["127.0.0.1", "127.8.9.1", "::1", "::ffff:127.0.0.1"] {
            let peer = PeerAddr(Some(peer.parse().unwrap()));
            assert_eq!(client_key(false, &none, Some(&peer)), ClientKey::Loopback);
        }
        let peer = PeerAddr(Some("::ffff:192.0.2.5".parse().unwrap()));
        assert_eq!(client_key(false, &none, Some(&peer)), v4(5));
        // On the proxy listener the forwarded address decides, a loopback peer or not.
        let peer = PeerAddr(Some("127.0.0.1".parse().unwrap()));
        let forwarded = headers(&["192.0.2.9"]);
        assert_eq!(client_key(true, &forwarded, Some(&peer)), v4(9));
        assert_eq!(client_key(true, &forwarded, Some(&PeerAddr(None))), v4(9));
    }

    #[test]
    fn usernames_count_without_regard_to_case_or_length() {
        assert_eq!(username_key("  Ann "), "ann");
        let long = "X".repeat(500);
        assert_eq!(username_key(&long).len(), crate::users::MAX_USERNAME_CHARS);
    }

    fn headers(forwarded: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in forwarded {
            headers.append("x-forwarded-for", value.parse().unwrap());
        }
        headers
    }

    #[test]
    fn the_client_is_the_peer_except_behind_the_trusted_proxy() {
        let peer = PeerAddr(Some("192.0.2.7".parse().unwrap()));
        let spoofed = headers(&["198.51.100.1"]);
        // The main listener never reads the header.
        assert_eq!(client_key(false, &spoofed, Some(&peer)), v4(7));
        assert_eq!(
            client_key(false, &HeaderMap::new(), None),
            ClientKey::Unknown
        );
        // The proxy's own header is the client, and its last entry is the one it added.
        let proxied = headers(&["203.0.113.5, 198.51.100.1", "10.0.0.9, 192.0.2.99"]);
        assert_eq!(client_key(true, &proxied, Some(&peer)), v4(99));
        // Without a usable header the peer stands in, else nobody.
        assert_eq!(client_key(true, &headers(&["junk"]), Some(&peer)), v4(7));
        assert_eq!(client_key(true, &HeaderMap::new(), Some(&peer)), v4(7));
        assert_eq!(
            client_key(true, &HeaderMap::new(), Some(&PeerAddr(None))),
            ClientKey::Unknown
        );
    }

    #[test]
    fn ipv6_clients_count_by_their_64_bit_prefix_and_mapped_ipv4_is_ipv4() {
        let a = ClientKey::of("2001:db8:1:2::1".parse().unwrap());
        let b = ClientKey::of("2001:db8:1:2:ffff::9".parse().unwrap());
        let c = ClientKey::of("2001:db8:1:3::1".parse().unwrap());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.to_string(), "2001:db8:1:2::/64");
        assert_eq!(ClientKey::of("::ffff:192.0.2.4".parse().unwrap()), v4(4));
    }
}
