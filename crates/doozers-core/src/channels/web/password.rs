//! Passwords: the rules for a new one, and hashing and checking with Argon2id. Design:
//! `Docs/technical/web-channel.md`.
//!
//! # Rules
//!
//! [`check_new`] follows NIST SP 800-63B-4 for a memorized secret that is the only factor: at least
//! [`MIN_CHARS`] and at most [`MAX_CHARS`] characters, and no composition rules (no required digits
//! or symbols), no hints, and no expiry. A character is a Unicode scalar value of the password after
//! NFKC normalization, so a password counts the same however a keyboard or an operating system
//! composes it, and anything may be in it, spaces included. Normalizing applies when a password is
//! set and when it is checked, so one typed on another device still matches. The password is
//! refused if it is a single character repeated, or contains the username or the name of this
//! service, which are the guesses an attacker makes first. There is no list of leaked passwords:
//! at 15 characters, the passwords on such lists are all too short.
//!
//! # Hashing
//!
//! Argon2id with the OWASP Password Storage Cheat Sheet's first choice: 19 MiB of memory, 2
//! iterations, 1 lane, a random 16-byte salt, and a 32-byte hash, stored as a PHC string
//! ([`HASH_PREFIX`]). That costs about 20 ms and 19 MiB, so [`Passwords`] runs it on a blocking
//! thread, and lets at most [`MAX_CONCURRENT`] run at once. Anyone can start a hash by posting the
//! login form, so a flood must wait its turn (and give up after [`QUEUE_TIMEOUT`], with
//! [`Error::Busy`]) rather than take all the memory.
//!
//! # Unknown usernames
//!
//! [`Passwords::verify`] with no stored hash checks the password against [`DUMMY_HASH`], a hash
//! with the same parameters of a password nobody knows, and answers `false` whatever the result.
//! So a username that doesn't exist, one with no password, and one that can't sign in cost the same
//! as a wrong password.

use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use tokio::sync::Semaphore;
use tracing::error;
use unicode_normalization::UnicodeNormalization;

/// The fewest characters a new password has.
pub(super) const MIN_CHARS: usize = 15;

/// The most characters a password has. A longer one is refused when set, and can't match when
/// checked.
pub(super) const MAX_CHARS: usize = 128;

/// How many hashes run at once. Each takes [`MEMORY_KIB`], so this is the most memory the logins
/// of a flood take: about 76 MiB.
pub(super) const MAX_CONCURRENT: usize = 4;

/// How long a password waits for one of the [`MAX_CONCURRENT`] places before it is [`Error::Busy`].
pub(super) const QUEUE_TIMEOUT: Duration = Duration::from_secs(5);

/// Argon2's memory cost in KiB: 19 MiB.
const MEMORY_KIB: u32 = 19 * 1024;

/// Argon2's number of iterations.
const ITERATIONS: u32 = 2;

/// Argon2's degree of parallelism.
const LANES: u32 = 1;

/// What every hash this code makes starts with: the algorithm, the Argon2 version (19 is 0x13), and
/// the cost parameters.
#[cfg(test)]
pub(super) const HASH_PREFIX: &str = "$argon2id$v=19$m=19456,t=2,p=1$";

/// A hash of a password nobody knows, made with the parameters above, that stands in for the hash of
/// a user who has none. A test makes sure it still has them.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$ZG9vemVycyBkdW1teSBzYWx0$f+8tMkIJmQEmdjHZ7xvFh5K3b0kM4i1cVYz2zjX8mYc";

/// The name of this service, which a password may not contain.
const SERVICE_NAME: &str = "doozers";

/// The shortest username a password is checked for containing: a short one would turn away too
/// many ordinary passphrases.
const MIN_CONTEXT_CHARS: usize = 4;

/// Why a new password isn't acceptable. The messages are shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum Problem {
    /// Fewer than [`MIN_CHARS`] characters.
    #[error("Use at least 15 characters. A few random words make a good password.")]
    TooShort,
    /// More than [`MAX_CHARS`] characters.
    #[error("Use at most 128 characters.")]
    TooLong,
    /// One character repeated.
    #[error("Don't use one character repeated.")]
    Repeated,
    /// It has the username in it.
    #[error("Don't put your username in your password.")]
    HasUsername,
    /// It has the name of the service in it.
    #[error("Don't put the name Doozers in your password.")]
    HasServiceName,
}

/// Hashing or checking a password failed.
#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    /// All [`MAX_CONCURRENT`] places were taken for [`QUEUE_TIMEOUT`].
    #[error("too many passwords are being checked right now")]
    Busy,
    /// The hash function or the thread that ran it failed.
    #[error("cannot hash a password: {0}")]
    Failed(String),
}

/// The password after normalization, which is what gets hashed.
fn normalize(password: &str) -> String {
    password.nfkc().collect()
}

/// Checks a new password for `username` against the rules, and returns it normalized, which is
/// what [`Passwords::hash`] wants.
pub(super) fn check_new(password: &str, username: &str) -> Result<String, Problem> {
    let password = normalize(password);
    let chars = password.chars().count();
    if chars < MIN_CHARS {
        return Err(Problem::TooShort);
    }
    if chars > MAX_CHARS {
        return Err(Problem::TooLong);
    }
    let first = password.chars().next();
    if password.chars().all(|c| Some(c) == first) {
        return Err(Problem::Repeated);
    }
    let lower = password.to_lowercase();
    let username = normalize(username).to_lowercase();
    if username.chars().count() >= MIN_CONTEXT_CHARS && lower.contains(&username) {
        return Err(Problem::HasUsername);
    }
    if lower.contains(SERVICE_NAME) {
        return Err(Problem::HasServiceName);
    }
    Ok(password)
}

/// The Argon2id instance every hash is made with.
fn argon2() -> Argon2<'static> {
    let params = Params::new(MEMORY_KIB, ITERATIONS, LANES, None).expect("fixed valid parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hashes and checks passwords, a few at a time ([module docs](self)).
#[derive(Debug)]
pub(super) struct Passwords {
    places: Arc<Semaphore>,
    /// How many hashes (of any kind) ran, and how many of them checked [`DUMMY_HASH`].
    #[cfg(test)]
    pub(super) ran: Arc<Ran>,
}

/// What a test can see of the hashes that ran.
#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct Ran {
    pub(super) hashed: std::sync::atomic::AtomicUsize,
    pub(super) checked: std::sync::atomic::AtomicUsize,
    pub(super) checked_dummy: std::sync::atomic::AtomicUsize,
}

impl Default for Passwords {
    fn default() -> Self {
        Self::new(MAX_CONCURRENT)
    }
}

impl Passwords {
    pub(super) fn new(concurrent: usize) -> Self {
        Self {
            places: Arc::new(Semaphore::new(concurrent)),
            #[cfg(test)]
            ran: Arc::default(),
        }
    }

    /// Waits for one of the places, then runs `work` on a blocking thread that holds it.
    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, Error> {
        let place = tokio::time::timeout(QUEUE_TIMEOUT, self.places.clone().acquire_owned())
            .await
            .map_err(|_| Error::Busy)?
            .map_err(|e| Error::Failed(e.to_string()))?;
        tokio::task::spawn_blocking(move || {
            let result = work();
            drop(place);
            result
        })
        .await
        .map_err(|e| Error::Failed(e.to_string()))
    }

    /// The PHC string of `password`, which [`check_new`] returned, with a new random salt.
    pub(super) async fn hash(&self, password: String) -> Result<String, Error> {
        #[cfg(test)]
        self.ran
            .hashed
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.run(move || {
            argon2()
                .hash_password(password.as_bytes())
                .map(|hash| hash.to_string())
                .map_err(|e| Error::Failed(e.to_string()))
        })
        .await?
    }

    /// Whether `password` is the one `stored` (a PHC string) is the hash of; `None` or a string
    /// that isn't a hash gets the same work done against [`DUMMY_HASH`] and is always `false`. A
    /// password too long to be one is checked as an empty one, for the same cost.
    pub(super) async fn verify(&self, password: &str, stored: Option<&str>) -> Result<bool, Error> {
        let parsed = stored.and_then(|stored| match PasswordHash::new(stored) {
            Ok(hash) => Some(hash),
            Err(e) => {
                error!(error = %e, "a stored password hash can't be read");
                None
            }
        });
        let real = parsed.is_some();
        let hash = parsed
            .unwrap_or_else(|| PasswordHash::new(DUMMY_HASH).expect("the dummy hash is valid"));
        let mut password = normalize(password);
        if password.chars().count() > MAX_CHARS {
            password.clear();
        }
        #[cfg(test)]
        let ran = self.ran.clone();
        let matched = self
            .run(move || {
                #[cfg(test)]
                {
                    ran.checked
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if !real {
                        ran.checked_dummy
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                argon2().verify_password(password.as_bytes(), &hash).is_ok()
            })
            .await?;
        Ok(matched && real)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn a_new_password_has_between_15_and_128_characters_and_no_other_rule() {
        let user = "bob";
        assert_eq!(check_new(&"x1".repeat(7), user), Err(Problem::TooShort));
        assert_eq!(check_new("", user), Err(Problem::TooShort));
        // The 15th character is the first that is enough.
        assert_eq!(
            check_new("correct horse b", user).as_deref(),
            Ok("correct horse b")
        );
        assert_eq!(
            check_new("correct horse ", user),
            Err(Problem::TooShort),
            "14 characters"
        );
        // No composition rules: lowercase letters alone, digits alone, spaces and all.
        for ok in [
            "abcdefghijklmnopqrstuvwxyz",
            "123456789012345",
            "   a b c d e f g   ",
        ] {
            assert_eq!(check_new(ok, user).as_deref(), Ok(ok), "{ok:?}");
        }
        let longest: String = ('a'..='z').cycle().take(MAX_CHARS).collect();
        assert!(check_new(&longest, user).is_ok());
        assert_eq!(
            check_new(&format!("{longest}a"), user),
            Err(Problem::TooLong)
        );
    }

    #[test]
    fn characters_are_unicode_scalar_values_after_normalization() {
        let user = "bob";
        // 15 emoji are 15 characters, though 60 bytes and 30 UTF-16 units.
        let emoji = "🦀".repeat(14) + "🐙";
        assert_eq!(emoji.chars().count(), 15);
        assert!(check_new(&emoji, user).is_ok());
        assert_eq!(check_new(&"🦀".repeat(14), user), Err(Problem::TooShort));
        // 128 emoji are fine, 129 aren't.
        let at_most = ["🦀", "🐙"]
            .iter()
            .cycle()
            .take(MAX_CHARS)
            .copied()
            .collect::<String>();
        assert!(check_new(&at_most, user).is_ok());
        assert_eq!(check_new(&(at_most + "🦀"), user), Err(Problem::TooLong));
        // `é` typed as `e` and a combining accent is the same password as the composed one.
        let composed = "caf\u{e9} au lait noir";
        let decomposed = "cafe\u{301} au lait noir";
        assert_eq!(check_new(decomposed, user).unwrap(), composed);
        // A fullwidth letter is its plain form.
        assert_eq!(
            check_new("ｃorrect horse battery", user).unwrap(),
            "correct horse battery"
        );
        // Counting happens after normalizing: 14 composed characters plus a ligature that
        // normalizes to two make 16, though they were 15 before.
        assert_eq!(
            check_new("abcdefghijklmn\u{fb01}", user)
                .unwrap()
                .chars()
                .count(),
            16
        );
    }

    #[test]
    fn the_guesses_an_attacker_makes_first_are_refused() {
        assert_eq!(check_new(&"a".repeat(20), "bob"), Err(Problem::Repeated));
        assert_eq!(
            check_new("my password is Alexander!!", "alexander"),
            Err(Problem::HasUsername)
        );
        assert_eq!(
            check_new("I love DOOZERS so much", "bob"),
            Err(Problem::HasServiceName)
        );
        // A short username is no reason to turn away a passphrase that happens to contain it.
        assert!(check_new("a long bobsled ride home", "bob").is_ok());
    }

    #[test]
    fn problems_say_what_to_do() {
        assert_eq!(
            Problem::TooShort.to_string(),
            "Use at least 15 characters. A few random words make a good password."
        );
        assert_eq!(Problem::TooLong.to_string(), "Use at most 128 characters.");
    }

    #[tokio::test]
    async fn a_password_hashes_to_the_owasp_parameters_and_checks_against_its_hash() {
        let passwords = Passwords::default();
        let hash = passwords
            .hash("correct horse battery staple".into())
            .await
            .unwrap();
        assert!(hash.starts_with(HASH_PREFIX), "{hash}");
        let other = passwords
            .hash("correct horse battery staple".into())
            .await
            .unwrap();
        assert_ne!(hash, other, "each hash has its own salt");

        assert!(
            passwords
                .verify("correct horse battery staple", Some(&hash))
                .await
                .unwrap()
        );
        assert!(
            !passwords
                .verify("correct horse battery stapl", Some(&hash))
                .await
                .unwrap()
        );
        assert!(!passwords.verify("", Some(&hash)).await.unwrap());
        // Another spelling of the same characters is the same password.
        let hash = passwords
            .hash(check_new("caf\u{e9} au lait noir", "bob").unwrap())
            .await
            .unwrap();
        assert!(
            passwords
                .verify("cafe\u{301} au lait noir", Some(&hash))
                .await
                .unwrap()
        );
        // Too long to have been set: wrong, and cheap to refuse by the same path.
        assert!(
            !passwords
                .verify(&"a".repeat(100_000), Some(&hash))
                .await
                .unwrap()
        );
    }

    #[test]
    fn the_dummy_hash_has_the_parameters_of_a_real_one() {
        let dummy = PasswordHash::new(DUMMY_HASH).unwrap();
        assert!(DUMMY_HASH.starts_with(HASH_PREFIX));
        let real = argon2().hash_password(b"whatever it is").unwrap();
        assert_eq!(dummy.algorithm, real.algorithm);
        assert_eq!(dummy.version, real.version);
        assert_eq!(dummy.params, real.params);
        assert_eq!(
            dummy.hash.as_ref().map(|h| h.len()),
            real.hash.as_ref().map(|h| h.len())
        );
        assert!(dummy.salt.as_ref().is_some_and(|salt| salt.len() >= 8));
    }

    #[tokio::test]
    async fn no_hash_is_checked_against_the_dummy_and_never_matches() {
        let passwords = Passwords::default();
        for stored in [None, Some("not a hash"), Some("")] {
            let before = passwords.ran.checked_dummy.load(Ordering::SeqCst);
            assert!(
                !passwords
                    .verify("anything at all here", stored)
                    .await
                    .unwrap()
            );
            assert_eq!(
                passwords.ran.checked_dummy.load(Ordering::SeqCst),
                before + 1,
                "{stored:?} ran the dummy check"
            );
        }
        // A real hash never does.
        let hash = passwords
            .hash("correct horse battery staple".into())
            .await
            .unwrap();
        let before = passwords.ran.checked_dummy.load(Ordering::SeqCst);
        assert!(
            !passwords
                .verify("wrong horse battery staple", Some(&hash))
                .await
                .unwrap()
        );
        assert_eq!(passwords.ran.checked_dummy.load(Ordering::SeqCst), before);
        // And nothing can make the dummy itself match.
        assert!(!passwords.verify("", None).await.unwrap());
    }

    #[tokio::test]
    async fn only_so_many_hashes_run_at_once_and_the_rest_wait() {
        let passwords = Passwords::new(1);
        let running = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let most = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tasks: Vec<_> = (0..6)
            .map(|_| {
                let (running, most) = (running.clone(), most.clone());
                passwords.run(move || {
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    running.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for result in futures::future::join_all(tasks).await {
            result.unwrap();
        }
        assert_eq!(most.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_password_that_waits_too_long_for_a_place_is_busy() {
        let passwords = Passwords::new(1);
        let _held = passwords.places.clone().acquire_owned().await.unwrap();
        let outcome = passwords.verify("anything at all here", None).await;
        assert!(matches!(outcome, Err(Error::Busy)), "{outcome:?}");
    }
}
