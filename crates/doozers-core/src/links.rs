//! Linking channel identities to users with `/link` (PLAN section 3.2). Design:
//! `Docs/technical/identity-and-routing.md#linking-an-identity`.
//!
//! A signed-in user asks for a code for a channel's identity namespace ([`Links::start`]), then sends
//! `/link <code>` from the account they want to add. The [router](crate::router::Router) lets that
//! message through even though its sender has no user, and the session pump hands it to
//! [`Links::link`], which answers on the channel. The code never reaches an agent, a session, or the
//! inbox.
//!
//! # The code
//!
//! A code is [`CODE_LEN`] characters of the RFC 4648 base32 alphabet, 40 bits, from the operating
//! system's random number generator. It lives [`CODE_TTL`], is for one namespace, and is stored only
//! as a salted hash (`user_link_codes`). A user has one code per namespace, so asking again replaces
//! it.
//!
//! # The limits
//!
//! The code is short, so the limits do the work of the missing bits:
//!
//! - A sender gets [`SENDER_ATTEMPTS`] wrong tries per [`SENDER_WINDOW`]. The last one locks the
//!   sender out for [`LOCKOUT`]: the answer says so once, and while the lock holds `/link` from
//!   that sender is dropped without an answer, so a sender can't make Doozers talk. The count is in
//!   memory, so a restart forgets it, and bounded ([`MAX_TRACKED`] senders).
//! - A code survives [`CODE_ATTEMPTS`] wrong tries by anyone while it is pending, then stops
//!   working. A wrong code can't be told apart from a guess at any pending code of its channel,
//!   so each counts against all of them, but not against the codes of other channels. That count is
//!   in the database. It is higher than a sender's own limit so
//!   that a few strangers can't use up every code, and low enough that guessing 40 bits is out of
//!   reach: the chance of hitting a code is [`CODE_ATTEMPTS`] in 2^40 for its whole life.
//!
//! The answer to a wrong code is the same whether or not any code is pending, and says only how
//! many tries the sender has left.
//!
//! # A sender who has a user already
//!
//! `/link` from an identity that is a user's already is refused, with no count, and the code isn't
//! looked at. An identity moves to another user only after it is removed from the first, so no code
//! can take an identity from a user.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use doozers_store::{LINK_CODE_SALT_LEN, Store};
use doozers_types::{Timestamp, UserId};
use tracing::{info, warn};

use crate::channels::chat_ids;
use crate::config::Identity;
use crate::registry::Registry;
use crate::router::LinkRequest;
use crate::sessions::{Clock, Linker};
use crate::users::{User, UserError, Users};

/// How many characters a link code has.
pub const CODE_LEN: usize = 8;

/// How long a link code works.
pub const CODE_TTL: Duration = Duration::from_secs(10 * 60);

/// How many wrong codes, from anyone, a pending code survives.
pub const CODE_ATTEMPTS: u32 = 20;

/// How many wrong codes one sender may send per [`SENDER_WINDOW`] before it is locked out.
pub const SENDER_ATTEMPTS: u32 = 5;

/// How long a sender's wrong codes are counted, from the first.
pub const SENDER_WINDOW: Duration = Duration::from_secs(60 * 60);

/// How long a sender that used up its tries is locked out.
pub const LOCKOUT: Duration = Duration::from_secs(60 * 60);

/// How long an expired code stays on the page, so the page can say it expired.
pub const EXPIRED_KEPT: Duration = Duration::from_secs(24 * 60 * 60);

/// The most senders whose wrong codes are counted at once. Past it, counts whose window ended go
/// first, then the oldest counts that aren't locks; when every one is a lock, a new sender is
/// treated as locked, so memory stays bounded however many identities someone writes from.
const MAX_TRACKED: usize = 10_000;

/// The RFC 4648 base32 alphabet: no `0`, `1`, `8`, or `9` to mistake for letters.
const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The namespaces that exist: those the running channels report.
pub trait ChannelNamespaces: Send + Sync {
    /// The identity namespace of every channel that is registered now.
    fn namespaces(&self) -> BTreeSet<String>;
}

impl ChannelNamespaces for Registry {
    fn namespaces(&self) -> BTreeSet<String> {
        self.channels()
            .into_iter()
            .map(|(_, channel)| channel.info().identity_namespace.clone())
            .collect()
    }
}

/// A new link code, to show once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLinkCode {
    /// The namespace it is for.
    pub namespace: String,
    /// The code, which isn't stored and can't be shown again.
    pub code: String,
    /// When it stops working.
    pub expires_at: Timestamp,
}

impl NewLinkCode {
    /// What to send from the identity: `/link <code>`.
    pub fn command(&self) -> String {
        format!("/link {}", self.code)
    }
}

/// Where a user's code for a namespace stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeState {
    /// It works until `expires_at`.
    Pending,
    /// It ran out of time.
    Expired,
    /// Too many wrong codes were sent while it was pending, so it was cancelled.
    Cancelled,
}

/// A code a user made, without the code, as the page lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCodeInfo {
    /// The namespace it is for.
    pub namespace: String,
    /// Where it stands.
    pub state: CodeState,
    /// When it stops (or stopped) working.
    pub expires_at: Timestamp,
}

/// Why a link code couldn't be made, or an identity couldn't be changed, on behalf of a user.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LinkError {
    /// No running channel reports the namespace. Carries the namespaces that exist.
    #[error("{}", unknown_namespace(.namespace, .available))]
    UnknownNamespace {
        /// What was asked for.
        namespace: String,
        /// The namespaces a link can be made for, which exist now.
        available: Vec<String>,
    },
    /// The namespace is the web chat's, whose identities aren't linked from a channel.
    #[error(
        "Identities in the web namespace aren't linked from a channel: they are how you sign in. \
         An admin sets the identity of a trusted proxy."
    )]
    WebNamespace,
    /// The user doesn't exist or is disabled.
    #[error("the user can't link identities")]
    UserUnavailable,
    /// The change was refused or failed.
    #[error(transparent)]
    Users(#[from] UserError),
    /// The database failed.
    #[error("database error: {0}")]
    Store(#[from] doozers_store::Error),
}

fn unknown_namespace(namespace: &str, available: &[String]) -> String {
    let mut text = format!("No running channel uses the identity namespace `{namespace}`.");
    if available.is_empty() {
        text.push_str(" No channel besides the web chat is running.");
    } else {
        let list = available
            .iter()
            .map(|ns| format!("`{ns}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(text, " The namespaces are {list}.");
    }
    text
}

/// A sender's wrong tries, in Unix milliseconds.
#[derive(Debug, Clone, Copy)]
struct Entry {
    /// When the first wrong try of this window was.
    since: i64,
    failures: u32,
    /// Until when the sender is locked out.
    locked_until: Option<i64>,
}

impl Entry {
    fn locked(&self, now: i64) -> bool {
        self.locked_until.is_some_and(|until| until > now)
    }

    fn over(&self, now: i64) -> bool {
        match self.locked_until {
            Some(until) => until <= now,
            None => now - self.since >= millis(SENDER_WINDOW),
        }
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// What one more wrong try did to a sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// The sender may try this many more times.
    TriesLeft(u32),
    /// That was the last try: the sender is locked out now.
    LockedNow,
    /// The table filled with locks since the sender was found unlocked ([`SenderLimits::locked`]
    /// asks first): it is treated as locked, silently.
    Full,
}

/// The wrong tries of senders, by their identity.
#[derive(Debug, Default)]
struct SenderLimits {
    senders: HashMap<String, Entry>,
}

impl SenderLimits {
    /// Whether `sender` is locked out at `now` (Unix milliseconds). A sender the table doesn't
    /// know is locked too while the table is full of locks, which leaves no room to count it, so it
    /// is refused before its code is looked at.
    fn locked(&self, sender: &str, now: i64) -> bool {
        match self.senders.get(sender) {
            Some(entry) => entry.locked(now),
            None => {
                self.senders.len() >= MAX_TRACKED
                    && self.senders.values().all(|entry| entry.locked(now))
            }
        }
    }

    /// Counts a wrong try by `sender`.
    fn failed(&mut self, sender: &str, now: i64) -> Failure {
        if let Some(entry) = self.senders.get_mut(sender) {
            if entry.over(now) {
                *entry = Entry {
                    since: now,
                    failures: 0,
                    locked_until: None,
                };
            }
        } else {
            if self.senders.len() >= MAX_TRACKED {
                self.senders.retain(|_, entry| !entry.over(now));
            }
            if self.senders.len() >= MAX_TRACKED {
                let oldest = self
                    .senders
                    .iter()
                    .filter(|(_, entry)| !entry.locked(now))
                    .min_by_key(|(_, entry)| entry.since)
                    .map(|(key, _)| key.clone());
                match oldest {
                    Some(key) => {
                        self.senders.remove(&key);
                    }
                    None => return Failure::Full,
                }
            }
            self.senders.insert(
                sender.to_owned(),
                Entry {
                    since: now,
                    failures: 0,
                    locked_until: None,
                },
            );
        }
        let entry = self
            .senders
            .get_mut(sender)
            .expect("the sender was just counted");
        entry.failures += 1;
        if entry.failures >= SENDER_ATTEMPTS {
            entry.locked_until = Some(now.saturating_add(millis(LOCKOUT)));
            Failure::LockedNow
        } else {
            Failure::TriesLeft(SENDER_ATTEMPTS - entry.failures)
        }
    }

    /// Forgets `sender`'s wrong tries.
    fn succeeded(&mut self, sender: &str) {
        self.senders.remove(sender);
    }
}

/// The identity links of a daemon: makes link codes for the web page, answers `/link`, and changes
/// the identities users keep themselves. See the [module documentation](self).
pub struct Links {
    store: Store,
    users: Arc<Users>,
    channels: Arc<dyn ChannelNamespaces>,
    clock: Arc<dyn Clock>,
    senders: Mutex<SenderLimits>,
}

impl std::fmt::Debug for Links {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Links").finish_non_exhaustive()
    }
}

/// A random link code.
fn new_code() -> String {
    let mut bytes = [0u8; CODE_LEN];
    getrandom::fill(&mut bytes).expect("the OS random number generator failed");
    // 256 is a multiple of 32, so the low five bits of a random byte are uniform.
    bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(b & 31)]))
        .collect()
}

/// `typed` as a link code: ASCII case folded, if it has the form of one.
fn normalize(typed: &str) -> Option<String> {
    let code = typed.trim().to_ascii_uppercase();
    (code.len() == CODE_LEN && code.bytes().all(|b| ALPHABET.contains(&b))).then_some(code)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Links {
    /// The links of `users`, with codes in `store`, for the channels `channels` reports (the
    /// registry, in the daemon).
    pub fn new(
        store: Store,
        users: Arc<Users>,
        channels: Arc<dyn ChannelNamespaces>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            users,
            channels,
            clock,
            senders: Mutex::default(),
        }
    }

    /// The namespaces a code can be made for: those the running channels report, except the web
    /// chat's. By name.
    pub fn namespaces(&self) -> Vec<String> {
        self.channels
            .namespaces()
            .into_iter()
            .filter(|ns| !is_web_identity(ns))
            .collect()
    }

    /// The user `id` as the router sees them now, if there is one.
    pub fn user(&self, id: &UserId) -> Option<User> {
        self.users.list().get().user(id).cloned()
    }

    /// Makes a code for user `user` and `namespace`, replacing the code they had for it, and
    /// returns it. This is the only time the code is known.
    ///
    /// # Errors
    ///
    /// [`LinkError::UnknownNamespace`] (with the namespaces that exist) if no running channel
    /// reports `namespace`, [`LinkError::WebNamespace`], [`LinkError::UserUnavailable`] if the
    /// user doesn't exist or is disabled, or a database error.
    pub async fn start(&self, user: &UserId, namespace: &str) -> Result<NewLinkCode, LinkError> {
        if is_web_identity(namespace) {
            return Err(LinkError::WebNamespace);
        }
        let available = self.namespaces();
        if !available.iter().any(|ns| ns == namespace) {
            return Err(LinkError::UnknownNamespace {
                namespace: namespace.to_owned(),
                available,
            });
        }
        if !self.user(user).is_some_and(|u| u.is_active()) {
            return Err(LinkError::UserUnavailable);
        }
        let now = self.clock.now();
        let expires_at = now
            .checked_add(CODE_TTL)
            .expect("ten minutes from now is a valid time");
        let code = new_code();
        let mut salt = [0u8; LINK_CODE_SALT_LEN];
        getrandom::fill(&mut salt).expect("the OS random number generator failed");
        self.store
            .user_link_code_create(user, namespace, &code, salt, now, expires_at)
            .await?;
        if let Ok(cutoff) = now.checked_sub(EXPIRED_KEPT) {
            self.store.user_link_codes_prune(cutoff).await?;
        }
        info!(%user, namespace, "made a link code");
        Ok(NewLinkCode {
            namespace: namespace.to_owned(),
            code,
            expires_at,
        })
    }

    /// The codes `user` has made that weren't used, by namespace, with where each stands now.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn codes(&self, user: &UserId) -> Result<Vec<LinkCodeInfo>, LinkError> {
        let now = self.clock.now();
        Ok(self
            .store
            .user_link_codes(user)
            .await?
            .into_iter()
            .map(|row| LinkCodeInfo {
                state: if row.expires_at <= now {
                    CodeState::Expired
                } else if row.attempts >= CODE_ATTEMPTS {
                    CodeState::Cancelled
                } else {
                    CodeState::Pending
                },
                namespace: row.namespace,
                expires_at: row.expires_at,
            })
            .collect())
    }

    /// Removes `identity` from user `user`. The identities of the web chat can't be removed here:
    /// they are how the user signs in (the `web:<id>` of a login link) or the admin's trusted proxy.
    ///
    /// # Errors
    ///
    /// [`LinkError::WebNamespace`], or the [`UserError`] of removing it.
    pub async fn remove_identity(
        &self,
        user: &UserId,
        identity: &Identity,
    ) -> Result<User, LinkError> {
        if is_web_identity(identity.as_str()) {
            return Err(LinkError::WebNamespace);
        }
        Ok(self
            .users
            .remove_identity(user, identity, self.clock.now())
            .await?)
    }

    /// Makes `identity` the default identity of `user`: where jobs and proactive messages go.
    ///
    /// # Errors
    ///
    /// The [`UserError`] of setting it.
    pub async fn set_default_identity(
        &self,
        user: &UserId,
        identity: &Identity,
    ) -> Result<User, LinkError> {
        Ok(self
            .users
            .set_default_identity(user, identity, self.clock.now())
            .await?)
    }

    /// Counts a wrong try by `sender` and says what to tell it, or nothing.
    fn failed(&self, request: &LinkRequest, sender: &str, now: Timestamp) -> Option<String> {
        let failure = lock(&self.senders).failed(sender, now.as_millisecond());
        match failure {
            Failure::TriesLeft(left) => Some(wrong_code(left)),
            Failure::LockedNow => {
                warn!(
                    namespace = request.namespace,
                    sender = %request.sender,
                    "locked a sender out of /link after too many wrong codes"
                );
                Some(locked_out())
            }
            Failure::Full => None,
        }
    }
}

/// Whether `text` is in the web chat's namespace (`web`, or `web:...`). `text` may be an
/// identity.
pub fn is_web_identity(text: &str) -> bool {
    text.split(':').next() == Some(chat_ids::COMPONENT)
}

fn wrong_code(left: u32) -> String {
    format!(
        "That code didn't work. Send /link and the 8-character code from the Identities page, and \
         make a new code there if it ran out of time. You have {left} {} left before /link is \
         locked for an hour.",
        if left == 1 { "try" } else { "tries" }
    )
}

fn locked_out() -> String {
    "Too many wrong codes. /link is locked for this account for an hour.".to_owned()
}

#[async_trait]
impl Linker for Links {
    async fn link(
        &self,
        request: &LinkRequest,
        now: Timestamp,
    ) -> Result<Option<String>, doozers_store::Error> {
        if request.linked {
            info!(
                namespace = request.namespace,
                sender = %request.sender,
                "refused /link: the identity is a user's already"
            );
            return Ok(Some(
                "This account is already linked to a user, so it can't be linked again. To \
                 link it to another user, remove it from the Identities page of the user it \
                 belongs to first."
                    .to_owned(),
            ));
        }
        let sender = format!("{}:{}", request.namespace, request.sender);
        if lock(&self.senders).locked(&sender, now.as_millisecond()) {
            info!(
                namespace = request.namespace,
                sender = %request.sender,
                "dropped /link: the sender is locked out"
            );
            return Ok(None);
        }
        let Some(code) = request.code.as_deref().and_then(normalize) else {
            return Ok(self.failed(request, &sender, now));
        };
        let redeemed = self
            .users
            .redeem_link_code(
                &request.namespace,
                request.sender.as_str(),
                &code,
                now,
                CODE_ATTEMPTS,
            )
            .await;
        match redeemed {
            Ok(Some(user)) => {
                lock(&self.senders).succeeded(&sender);
                info!(
                    user = %user.id,
                    identity = sender,
                    "linked an identity with /link"
                );
                Ok(Some(format!(
                    "Done. This account is now linked to {}. Messages you send from here go to \
                     your agents.",
                    user.display_name
                )))
            }
            // A wrong code, and the rare ones that can't be told from it: a sender that isn't a
            // valid identity, or one that a user got since the router looked.
            Ok(None) | Err(UserError::Invalid(_) | UserError::IdentityTaken(_)) => {
                Ok(self.failed(request, &sender, now))
            }
            Err(UserError::Store(error)) => Err(error),
            Err(other) => Err(doozers_store::Error::Corrupt(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests;
