//! Passkeys, the parts that need no request: whether they can be offered, the WebAuthn relying
//! party, the ceremonies in flight, and the ids that identify a user to an authenticator. The
//! setup pages are in [`passkey_pages`](super::passkey_pages). Design:
//! `Docs/technical/web-channel.md`.
//!
//! # Availability
//!
//! A passkey belongs to a relying party id (RP ID), which is the host of the web channel's
//! `public_url`, and to nothing else: change that host and every passkey stops working. So passkeys
//! are offered only when `public_url` is set, its host is a domain name (WebAuthn has no RP ID for
//! an IP address), and it is a secure origin: `https`, or a `localhost` name over `http`, which a
//! browser treats as secure ([`Unavailable`] says which is missing). The origin of the ceremonies is
//! that URL too: a browser at another address sends another origin, and the ceremony fails.
//!
//! # Ceremonies
//!
//! A registration is a ceremony of two requests, with state between them that must stay on the
//! server (the library's serialized form is behind a `danger-` feature, so there is none).
//! [`Ceremonies`] keeps it in memory under a random id the page sends back, for [`TTL`], once:
//! [`Ceremonies::finish`] takes the entry out whether or not it matches. An entry is bound to what
//! started it, a string the finishing request must repeat: for a registration, the user and their
//! CSRF token. The table holds at most [`MAX`]
//! entries; past that the oldest go, so memory is bounded however many ceremonies are started. A
//! restart forgets them, and the page says to try again.
//!
//! # Identifying a user
//!
//! A discoverable passkey names its user to the server by a user handle: [`user_handle`] is a UUID
//! made from the user id, so it is the same for every passkey of a user, reveals nothing, and
//! needs no column. Sign-in looks the passkey up by credential id, and refuses one whose handle
//! isn't its owner's.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;

use doozers_store::PasskeyKind;
use doozers_types::{Timestamp, UserId};
use sha2::{Digest, Sha256};
use url::{Host, Url};
use webauthn_rs::prelude::{PasskeyRegistration, Uuid, Webauthn, WebauthnBuilder};

use super::login::is_loopback_host;
use super::random_hex;

/// How long a ceremony may take: what the browser is told to allow (the library's default, 5
/// minutes), and the server keeps the state a little longer.
pub(super) const TTL: Duration = Duration::from_secs(6 * 60);

/// The most ceremonies kept at once.
pub(super) const MAX: usize = 10_000;

/// The name Doozers gives itself in a browser's passkey dialog.
const RP_NAME: &str = "Doozers";

/// Why passkeys can't be offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unavailable {
    /// `public_url` isn't set, so there is no RP ID.
    NoPublicUrl,
    /// `public_url`'s host is an IP address, which has no RP ID.
    NoDomain,
    /// `public_url` is plain `http` on a host that isn't `localhost`: a browser won't do WebAuthn
    /// there.
    Insecure,
}

impl Unavailable {
    /// What to tell a user: why, and what the person who runs Doozers does about it.
    pub(super) const fn explain(self) -> &'static str {
        match self {
            Self::NoPublicUrl => {
                "Passkeys aren't available because Doozers doesn't know its own address. The \
                 person who runs Doozers sets `public_url` in the `[channel.web.config]` table of \
                 `doozers.toml`."
            }
            Self::NoDomain => {
                "Passkeys aren't available because `public_url` is an IP address. A passkey \
                 belongs to a domain name, so the person who runs Doozers sets `public_url` to \
                 one, or to `localhost`."
            }
            Self::Insecure => {
                "Passkeys aren't available because `public_url` isn't secure. Browsers use \
                 passkeys only over HTTPS, or at `localhost`, so the person who runs Doozers sets \
                 `public_url` to an `https` address."
            }
        }
    }
}

/// The relying party: the WebAuthn library set up for `public_url`.
pub(super) struct RelyingParty {
    webauthn: Webauthn,
    /// The origin passkeys work at, as a browser writes it: `https://doozers.example.com`.
    origin: String,
}

impl fmt::Debug for RelyingParty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelyingParty")
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

impl RelyingParty {
    /// The library, which makes and checks the ceremonies.
    pub(super) fn webauthn(&self) -> &Webauthn {
        &self.webauthn
    }

    /// The origin a browser must be at to use a passkey.
    pub(super) fn origin(&self) -> &str {
        &self.origin
    }

    /// Sets up the library for `public_url` (already an origin, from the channel's config), or
    /// says why it can't be.
    fn new(public_url: Option<&str>) -> Result<Self, Unavailable> {
        let origin = public_url
            .and_then(|url| Url::parse(url).ok())
            .ok_or(Unavailable::NoPublicUrl)?;
        let Some(Host::Domain(domain)) = origin.host() else {
            return Err(Unavailable::NoDomain);
        };
        if origin.scheme() != "https" && !is_loopback_host(domain) {
            return Err(Unavailable::Insecure);
        }
        let webauthn = WebauthnBuilder::new(domain, &origin)
            .and_then(|builder| builder.rp_name(RP_NAME).build())
            .map_err(|_| Unavailable::NoDomain)?;
        Ok(Self {
            webauthn,
            origin: origin.origin().ascii_serialization(),
        })
    }
}

/// What a ceremony in flight holds.
pub(super) enum Ceremony {
    /// A registration of a passkey of this kind.
    Register {
        kind: PasskeyKind,
        state: PasskeyRegistration,
    },
}

struct Entry {
    /// When it stops being valid, in Unix milliseconds.
    expires_at: i64,
    bound: String,
    ceremony: Ceremony,
}

/// The ceremonies in flight ([module docs](self)).
pub(super) struct Ceremonies {
    entries: Mutex<HashMap<String, Entry>>,
    ttl_ms: i64,
    max: usize,
}

impl Default for Ceremonies {
    fn default() -> Self {
        Self::new(TTL, MAX)
    }
}

impl Ceremonies {
    pub(super) fn new(ttl: Duration, max: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl_ms: i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX),
            max,
        }
    }

    /// Keeps `ceremony`, bound to `bound`, and returns the id to finish it with: 128 random bits as
    /// hex. Expired ceremonies go first, then, at the cap, the oldest.
    pub(super) fn begin(&self, bound: &str, ceremony: Ceremony, now: Timestamp) -> String {
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
                ceremony,
            },
        );
        id
    }

    /// Takes the ceremony `id` out, if it is there, hasn't expired, and was bound to `bound`. The
    /// entry is gone either way, so a guess at an id costs the one it guessed.
    pub(super) fn finish(&self, id: &str, bound: &str, now: Timestamp) -> Option<Ceremony> {
        let now = now.as_millisecond();
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)?;
        (entry.expires_at > now && same(&entry.bound, bound)).then_some(entry.ceremony)
    }

    /// How many ceremonies are kept.
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

/// What a registration is bound to: the user and the CSRF token of their login.
pub(super) fn registration_binding(user: &UserId, csrf: &str) -> String {
    format!("register\0{user}\0{csrf}")
}

/// The user handle of `user`: the 16 bytes of a UUID made from their id, the same for all their
/// passkeys.
pub(super) fn user_handle(user: &UserId) -> Uuid {
    let digest = Sha256::new()
        .chain_update(b"doozers passkey user handle\0")
        .chain_update(user.as_str().as_bytes())
        .finalize();
    Uuid::from_slice(&digest[..16]).expect("16 bytes make a UUID")
}

/// The passkey side of a web app: the relying party, if passkeys can be offered, and the
/// ceremonies in flight.
pub(super) struct Passkeys {
    relying_party: Result<RelyingParty, Unavailable>,
    pub(super) ceremonies: Ceremonies,
}

impl fmt::Debug for Passkeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Passkeys")
            .field("relying_party", &self.relying_party)
            .finish_non_exhaustive()
    }
}

impl Passkeys {
    /// Passkeys for the web channel whose `public_url` is `public_url`.
    pub(super) fn new(public_url: Option<&str>) -> Self {
        Self {
            relying_party: RelyingParty::new(public_url),
            ceremonies: Ceremonies::default(),
        }
    }

    /// The relying party, or why there is none.
    pub(super) fn relying_party(&self) -> Result<&RelyingParty, Unavailable> {
        self.relying_party.as_ref().map_err(|why| *why)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).unwrap()
    }

    fn rp(url: Option<&str>) -> Result<String, Unavailable> {
        RelyingParty::new(url).map(|rp| rp.origin().to_owned())
    }

    #[test]
    fn passkeys_need_a_public_url_with_a_domain_and_a_secure_origin() {
        assert_eq!(rp(None).unwrap_err(), Unavailable::NoPublicUrl);
        assert_eq!(
            rp(Some("https://doozers.example.com")).unwrap(),
            "https://doozers.example.com"
        );
        assert_eq!(
            rp(Some("https://doozers.example.com:8443")).unwrap(),
            "https://doozers.example.com:8443"
        );
        // A browser treats localhost as secure over plain HTTP.
        assert_eq!(
            rp(Some("http://localhost:8080")).unwrap(),
            "http://localhost:8080"
        );
        assert_eq!(
            rp(Some("http://doozers.localhost:8080")).unwrap(),
            "http://doozers.localhost:8080"
        );
        assert_eq!(
            rp(Some("http://doozers.example.com")).unwrap_err(),
            Unavailable::Insecure
        );
        // An IP address is no RP ID, however secure the origin.
        for url in [
            "https://203.0.113.5",
            "http://127.0.0.1:8080",
            "http://[::1]:8080",
        ] {
            assert_eq!(rp(Some(url)).unwrap_err(), Unavailable::NoDomain, "{url}");
        }
    }

    #[test]
    fn each_reason_tells_whoever_runs_doozers_what_to_set() {
        for why in [
            Unavailable::NoPublicUrl,
            Unavailable::NoDomain,
            Unavailable::Insecure,
        ] {
            assert!(why.explain().contains("public_url"), "{why:?}");
        }
    }

    #[test]
    fn a_user_has_one_handle_and_no_two_users_share_it() {
        let ann = UserId::new("ann").unwrap();
        let bob = UserId::new("bob").unwrap();
        assert_eq!(user_handle(&ann), user_handle(&ann));
        assert_ne!(user_handle(&ann), user_handle(&bob));
        assert_eq!(user_handle(&ann).as_bytes().len(), 16);
    }

    /// A registration ceremony, for the tests of the table that holds them.
    fn ceremony() -> Ceremony {
        let rp = RelyingParty::new(Some("https://doozers.example.com")).unwrap();
        let user = UserId::new("ann").unwrap();
        let (_, state) = rp
            .webauthn()
            .start_passkey_registration(user_handle(&user), "ann", "Ann", None)
            .unwrap();
        Ceremony::Register {
            kind: PasskeyKind::Discoverable,
            state,
        }
    }

    fn bound(user: &str, csrf: &str) -> String {
        registration_binding(&UserId::new(user).unwrap(), csrf)
    }

    #[test]
    fn a_ceremony_finishes_once_for_what_started_it() {
        let ceremonies = Ceremonies::default();
        let id = ceremonies.begin(&bound("ann", "csrf"), ceremony(), at(0));
        assert_eq!(id.len(), 32);
        assert!(matches!(
            ceremonies.finish(&id, &bound("ann", "csrf"), at(1)),
            Some(Ceremony::Register { .. })
        ));
        assert!(
            ceremonies
                .finish(&id, &bound("ann", "csrf"), at(2))
                .is_none(),
            "used once"
        );
        assert_eq!(ceremonies.len(), 0);
    }

    #[test]
    fn a_ceremony_finished_by_someone_else_or_late_is_gone() {
        let ceremonies = Ceremonies::default();
        // Another login's CSRF token and another user don't match, and the entry doesn't survive
        // the attempt.
        for (user, csrf) in [("ann", "other"), ("bob", "csrf")] {
            let id = ceremonies.begin(&bound("ann", "csrf"), ceremony(), at(0));
            assert!(ceremonies.finish(&id, &bound(user, csrf), at(1)).is_none());
            assert!(
                ceremonies
                    .finish(&id, &bound("ann", "csrf"), at(2))
                    .is_none()
            );
        }
        let id = ceremonies.begin(&bound("ann", "csrf"), ceremony(), at(0));
        let late = at(TTL.as_secs() as i64);
        assert!(
            ceremonies
                .finish(&id, &bound("ann", "csrf"), late)
                .is_none()
        );
        assert!(
            ceremonies
                .finish("nope", &bound("ann", "csrf"), at(1))
                .is_none()
        );
    }

    #[test]
    fn the_table_stays_at_its_cap_dropping_what_expired_and_then_the_oldest() {
        let ceremonies = Ceremonies::new(Duration::from_secs(60), 3);
        let bound = bound("ann", "csrf");
        let first = ceremonies.begin(&bound, ceremony(), at(0));
        let second = ceremonies.begin(&bound, ceremony(), at(10));
        let third = ceremonies.begin(&bound, ceremony(), at(20));
        // Full, with none expired: the oldest goes.
        let fourth = ceremonies.begin(&bound, ceremony(), at(30));
        assert_eq!(ceremonies.len(), 3);
        assert!(ceremonies.finish(&first, &bound, at(31)).is_none());
        assert!(ceremonies.finish(&second, &bound, at(31)).is_some());
        // Two are left and one fits. Everything old enough has expired: only live ones count.
        let fifth = ceremonies.begin(&bound, ceremony(), at(32));
        assert_eq!(ceremonies.len(), 3);
        let sixth = ceremonies.begin(&bound, ceremony(), at(91));
        assert_eq!(
            ceremonies.len(),
            2,
            "third and fourth expired and were dropped"
        );
        assert!(ceremonies.finish(&third, &bound, at(91)).is_none());
        assert!(ceremonies.finish(&fourth, &bound, at(91)).is_none());
        assert!(ceremonies.finish(&fifth, &bound, at(91)).is_some());
        assert!(ceremonies.finish(&sixth, &bound, at(91)).is_some());
    }
}
