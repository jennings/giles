//! Users and their channel identities (PLAN sections 1.1, 3.2, 11.2, 11.4): [`User`], [`UserList`],
//! the snapshot of every user that the [router](crate::router::Router) and the web login read for
//! each message, and [`Users`], which keeps the users in the database and replaces that snapshot
//! after every change. Design: `Docs/technical/identity-and-routing.md`.
//!
//! # Storage and liveness
//!
//! Users and identities live in the `users` and `user_identities` tables. [`Users`] also keeps them
//! as a [`UserList`] snapshot that the router shares, and replaces it after every successful
//! change (the [`RouteList`](crate::routes::RouteList) pattern), so the next inbound message is
//! resolved against the new users and nothing restarts. A snapshot, rather than a database read
//! per message, keeps [`Router::classify`](crate::router::Router::classify) synchronous and off
//! the database for every message, including the ones it drops. Changes are serialized by one lock
//! that spans reading the snapshot, validating the change, writing it, and replacing the snapshot,
//! so the snapshot always equals what is stored. Only this process changes the tables.
//!
//! # Roles
//!
//! A user is an `admin` or a `member` ([`Role`]); all admins are equal. [`User::is_admin`] (through
//! [`UserDirectory::is_admin`] or [`UserList::is_admin`]) says whether a user is an active admin,
//! read from the current snapshot. The last active admin can't be removed, disabled, or demoted
//! ([`UserError::LastAdmin`]): [`Users::set_state`], [`Users::set_role`], and [`Users::remove`]
//! refuse it, and the store checks it inside the transaction of the change, so the check and the
//! change can't be split by another one.
//!
//! # Invites
//!
//! [`Users::create_invite`] makes a one-time link that signs a user in: an `invite` sets up a new
//! account, and a `recovery` link gets a user who lost every sign-in method back in
//! (`doozers admin login-link`). Both last [`INVITE_TTL`] (72 hours), work once, and live in the
//! database by the hash of their token, so they survive a restart and nothing stored can be
//! replayed. Making one gives the user the `web:<id>` identity a web sign-in acts as. A recovery
//! link takes nothing away from the user.
//!
//! # Identities
//!
//! A user's identities are added by [`Users::redeem_link_code`] (a `/link` code sent from the
//! identity, verified as `link`, see [`links`](crate::links)), by [`Users::add_identity`] (an
//! admin or the operator), by [`Users::set_proxy_identity`] (an admin, for the trusted proxy's
//! login, as `admin` with no code), by an invite (the `web:<id>` a link signs in as), and by the
//! `[owner]` import. [`Users::remove_identity`] and [`Users::set_default_identity`] are the
//! user's own changes. Each replaces the snapshot.
//!
//! # Import
//!
//! `doozers.toml`'s `[owner]` is imported once, by [`Users::open`], when the import has never run
//! (the database records that it did). The owner becomes an `admin` user with the id
//! [`identity::OWNER`], so sessions, memories, SSH key secrets, and `users/owner/` need no
//! rewrite. After that `[owner]` is never read again; `open` warns while `doozers.toml` still
//! has it. A fresh install has no `[owner]`: it starts with no users, and
//! [`Users::login_user`] (`doozers admin login-link`) creates the first admin.
//!
//! The id `owner` is a data key. Config jobs (`[[jobs]]` of `agent.toml`, until
//! #115 replaces them) run as the user `owner`, so the first admin of a fresh install has that
//! id too.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use doozers_store::{
    ImportedUser, InviteKind, LinkRedeemed, NewIdentity, NewUser, Role, Store, StoredUser,
    UserState, Verification, canonical_identity,
};
use doozers_types::{InboundMessage, NativeId, Timestamp, UserId};
use jiff::tz::TimeZone;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::{Config, Identity, parse_timezone};
use crate::identity;

/// How long an invite or recovery link works.
pub const INVITE_TTL: Duration = Duration::from_secs(72 * 60 * 60);

/// Whether `token` has the form of an invite or recovery token: 256 random bits as 64 lowercase
/// hex digits.
pub fn is_invite_token(token: &str) -> bool {
    token.len() == 64
        && token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A link [`Users::create_invite`] made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInvite {
    /// The secret in the link. Only its hash is stored, so this is the one time it is known.
    pub token: String,
    /// When the link stops working.
    pub expires_at: Timestamp,
}

/// A user: a person who talks to the agents.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct User {
    /// The user's id: opaque and never changed. The id of the user `[owner]` described is
    /// [`identity::OWNER`].
    pub id: UserId,
    /// The username.
    pub username: String,
    /// The name people see.
    pub display_name: String,
    /// What the user may do.
    pub role: Role,
    /// Whether the user can use Doozers now. A disabled user's messages are dropped.
    pub state: UserState,
    /// The user's time zone, for message time prefixes and job schedules. `None`: the agent's
    /// `timezone`, else the system time zone.
    pub timezone: Option<TimeZone>,
    /// The identity jobs and proactive messages go to; one of `identities`. `None` if the user
    /// has no identity a channel can reach.
    pub default_identity: Option<Identity>,
    /// The identities that act as this user, oldest first.
    pub identities: Vec<UserIdentity>,
}

/// A channel identity of a user.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UserIdentity {
    /// `<namespace>:<native id>`.
    pub identity: Identity,
    /// When it was established as the user's.
    pub verified_at: Timestamp,
    /// How it was established.
    pub verification: Verification,
}

impl User {
    /// Whether the user can use Doozers now.
    pub fn is_active(&self) -> bool {
        self.state == UserState::Active
    }

    /// Whether the user may administer Doozers now: an active `admin`. This is the one test of
    /// privilege (PLAN section 3.1); a disabled admin has none.
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin && self.is_active()
    }
}

/// The directory of users one [`UserList`] snapshot holds.
#[derive(Debug, Default)]
pub struct UserDirectory {
    users: Vec<User>,
    by_id: HashMap<UserId, usize>,
    /// The index of each identity's user, by [`identity_key`].
    by_identity: HashMap<String, usize>,
}

impl UserDirectory {
    fn new(users: Vec<User>) -> Self {
        let mut by_id = HashMap::with_capacity(users.len());
        let mut by_identity = HashMap::new();
        for (index, user) in users.iter().enumerate() {
            by_id.insert(user.id.clone(), index);
            for identity in &user.identities {
                by_identity.insert(identity_key(identity.identity.as_str()), index);
            }
        }
        Self {
            users,
            by_id,
            by_identity,
        }
    }

    /// Every user, oldest first.
    pub fn users(&self) -> &[User] {
        &self.users
    }

    /// The user `id`, if there is one.
    pub fn user(&self, id: &UserId) -> Option<&User> {
        self.by_id.get(id).map(|&index| &self.users[index])
    }

    /// The user whose username is `username`, compared without regard to ASCII case, as the
    /// database keeps usernames unique.
    pub fn by_username(&self, username: &str) -> Option<&User> {
        self.users
            .iter()
            .find(|user| user.username.eq_ignore_ascii_case(username))
    }

    /// The user who has the identity `<namespace>:<id>`.
    ///
    /// The comparison is exact (case-sensitive) except in the `email` namespace (first segment
    /// `email`), where the domain, after the last `@`, is compared ASCII case-insensitively; the
    /// local part stays case-sensitive, as RFC 5321 allows mail servers to treat it so.
    pub fn resolve(&self, namespace: &str, id: &NativeId) -> Option<&User> {
        let key = identity_key(&format!("{namespace}:{id}"));
        self.by_identity.get(&key).map(|&index| &self.users[index])
    }

    /// Whether anyone has the identity `<namespace>:<native id>`, given joined.
    pub fn has_identity(&self, joined: &str) -> bool {
        self.by_identity.contains_key(&identity_key(joined))
    }

    /// Whether `id` is an active admin ([`User::is_admin`]). A user who doesn't exist is none.
    pub fn is_admin(&self, id: &UserId) -> bool {
        self.user(id).is_some_and(User::is_admin)
    }

    /// The user the message's sender is: the one who has the sender's identity or, if none does,
    /// the first alias that is anyone's.
    pub fn resolve_sender(&self, namespace: &str, message: &InboundMessage) -> Option<&User> {
        message
            .sender_ids()
            .find_map(|id| self.resolve(namespace, id))
    }
}

/// The key identities are compared by: the joined identity in its
/// [canonical form](doozers_store::canonical_identity), which the store keeps them in too.
fn identity_key(identity: &str) -> String {
    canonical_identity(identity)
}

/// The users the router and the web login read. Cheap to clone; clones share the list.
///
/// [`get`](Self::get) returns a snapshot, so a message is resolved against one set of users even if
/// they change meanwhile.
#[derive(Debug, Clone, Default)]
pub struct UserList {
    current: Arc<RwLock<Arc<UserDirectory>>>,
}

impl UserList {
    /// Starts with `users`.
    pub fn new(users: Vec<User>) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(UserDirectory::new(users)))),
        }
    }

    /// The users now.
    pub fn get(&self) -> Arc<UserDirectory> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the users; messages resolved after this returns use them.
    pub fn set(&self, users: Vec<User>) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) =
            Arc::new(UserDirectory::new(users));
    }

    /// Whether `id` is an active admin now ([`User::is_admin`]).
    pub fn is_admin(&self, id: &UserId) -> bool {
        self.get().is_admin(id)
    }

    /// The time zone of user `id`, if the user exists and has set one.
    pub fn timezone(&self, id: &UserId) -> Option<TimeZone> {
        self.get().user(id)?.timezone.clone()
    }
}

/// The longest username, in characters.
pub const MAX_USERNAME_CHARS: usize = 64;

/// The longest display name, in characters.
pub const MAX_DISPLAY_NAME_CHARS: usize = 100;

/// The username and display name of the first admin `doozers admin login-link` creates.
const FIRST_ADMIN: (&str, &str) = ("admin", "Admin");

/// Startup failed to import `[owner]` or load the users.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UsersError {
    /// `[owner]` in `doozers.toml` is invalid, so it can't be imported. Fix it or remove it.
    #[error("{file}: {key}: {reason}")]
    Import {
        /// The configuration file.
        file: String,
        /// The key at fault, such as `owner.identities[1]`.
        key: String,
        /// What is wrong.
        reason: String,
    },
    /// The database failed.
    #[error("cannot load the users: {0}")]
    Store(#[from] doozers_store::Error),
    /// A stored user is malformed.
    #[error("the stored users are damaged: {0}")]
    Corrupt(String),
}

/// A change to the users was refused or failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UserError {
    /// The change names something that isn't valid.
    #[error("{0}")]
    Invalid(String),
    /// A user has this id already.
    #[error("a user with that id already exists")]
    IdTaken,
    /// A user has this username already, in any case.
    #[error("a user with that username already exists")]
    UsernameTaken,
    /// The identity belongs to a user already.
    #[error("the identity {0} belongs to a user already")]
    IdentityTaken(String),
    /// There is no such user.
    #[error("there is no user `{0}`")]
    NoSuchUser(UserId),
    /// The change would leave no active admin: it removes, disables, or demotes the last one.
    #[error("the last active admin can't be removed, disabled, or demoted")]
    LastAdmin,
    /// The database failed.
    #[error("database error: {0}")]
    Store(doozers_store::Error),
    /// A stored user is malformed.
    #[error("the stored users are damaged: {0}")]
    Corrupt(String),
}

impl UserError {
    fn from_store(error: doozers_store::Error, user: &UserId) -> Self {
        use doozers_store::Error;
        match error {
            Error::UserIdTaken => Self::IdTaken,
            Error::UsernameTaken => Self::UsernameTaken,
            Error::IdentityTaken { identity } => Self::IdentityTaken(identity),
            Error::NoSuchUser => Self::NoSuchUser(user.clone()),
            Error::LastAdmin => Self::LastAdmin,
            Error::InvalidRequest(reason) => Self::Invalid(reason),
            other => Self::Store(other),
        }
    }

    /// Whether the change was refused for what it asked, not because something broke.
    pub fn is_invalid_input(&self) -> bool {
        !matches!(self, Self::Store(_) | Self::Corrupt(_))
    }
}

impl From<UsersError> for UserError {
    fn from(error: UsersError) -> Self {
        match error {
            UsersError::Store(error) => Self::Store(error),
            UsersError::Corrupt(reason) | UsersError::Import { reason, .. } => {
                Self::Corrupt(reason)
            }
        }
    }
}

/// Why [`Users::login_user`] found no user for a link, or [`Users::create_invite`] couldn't make
/// one.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoginUserError {
    /// There are several users and none was named. Carries the message, which lists them.
    #[error("{0}")]
    NeedUser(String),
    /// There is no such user.
    #[error("there is no user `{0}`")]
    NoSuchUser(UserId),
    /// The user is disabled.
    #[error("user `{0}` is disabled")]
    Disabled(UserId),
    /// The `web:<id>` identity a link signs in as belongs to another user.
    #[error(
        "`web:{user}` is an identity of user `{owner}`, so `{user}` can't log in to the web chat"
    )]
    WebIdentityTaken {
        /// The user to log in.
        user: UserId,
        /// The user who has `web:<user>`.
        owner: UserId,
    },
    /// The users couldn't be changed.
    #[error(transparent)]
    Users(UserError),
}

/// What `[owner]` of `doozers.toml` says. Unknown keys are reported and ignored.
#[derive(Debug, Deserialize)]
struct LegacyOwner {
    identities: Vec<String>,
    default: Option<String>,
    timezone: Option<String>,
}

/// Validates `[owner]` of the configuration file `file` and turns it into the user to import. It
/// checks what `doozers.toml` always did, except that identities differing only in the case of an
/// email domain, which it didn't tell apart, are one: the first spelling stays.
fn import_owner(value: &toml::Value, file: &str) -> Result<ImportedUser, UsersError> {
    let err = |key: &str, reason: String| UsersError::Import {
        file: file.to_owned(),
        key: key.to_owned(),
        reason,
    };
    let mut unknown = Vec::new();
    let raw: LegacyOwner = serde_ignored::deserialize(value.clone(), |path| {
        unknown.push(path.to_string());
    })
    .map_err(|e: toml::de::Error| err("owner", e.message().to_owned()))?;
    for key in unknown {
        warn!("{file}: unknown key `owner.{key}` (ignored)");
    }
    if raw.identities.is_empty() {
        return Err(err(
            "owner.identities",
            "must list at least one identity".into(),
        ));
    }
    let mut identities = Vec::with_capacity(raw.identities.len());
    let mut seen = BTreeSet::new();
    let mut spelled = BTreeSet::new();
    for (i, text) in raw.identities.iter().enumerate() {
        let key = format!("owner.identities[{i}]");
        let identity = Identity::new(text.as_str()).map_err(|r| err(&key, r))?;
        if !spelled.insert(identity.clone()) {
            return Err(err(&key, format!("{identity} is listed twice")));
        }
        let canonical = canonical_identity(identity.as_str());
        if !seen.insert(canonical.clone()) {
            warn!(
                "{file}: {key}: {identity} is the same identity as an earlier one, since an email \
                 domain ignores case; keeping the earlier spelling"
            );
            continue;
        }
        identities.push(canonical);
    }
    let default = match &raw.default {
        Some(text) => {
            let identity = Identity::new(text.as_str()).map_err(|r| err("owner.default", r))?;
            let canonical = canonical_identity(identity.as_str());
            if !seen.contains(&canonical) {
                return Err(err(
                    "owner.default",
                    format!("{identity} is not one of `identities`"),
                ));
            }
            canonical
        }
        None if identities.len() == 1 => identities[0].clone(),
        None => {
            return Err(err(
                "owner.default",
                "missing: with several identities, name the one jobs and proactive messages go to"
                    .into(),
            ));
        }
    };
    if let Some(name) = &raw.timezone {
        parse_timezone(name).map_err(|r| err("owner.timezone", r))?;
    }
    Ok(ImportedUser {
        user: NewUser {
            id: identity::owner(),
            username: identity::OWNER.to_owned(),
            display_name: "Owner".to_owned(),
            role: Role::Admin,
            timezone: raw.timezone,
            default_identity: Some(default),
        },
        identities,
    })
}

/// A stored user as the snapshot holds it.
fn to_user(stored: StoredUser) -> Result<User, UsersError> {
    let corrupt = |what: String| UsersError::Corrupt(format!("user {}: {what}", stored.id));
    let identity = |text: &str| Identity::new(text).map_err(corrupt);
    let timezone = stored
        .timezone
        .as_deref()
        .map(|name| parse_timezone(name).map_err(corrupt))
        .transpose()?;
    let default_identity = stored
        .default_identity
        .as_deref()
        .map(identity)
        .transpose()?;
    let identities = stored
        .identities
        .iter()
        .map(|i| {
            Ok(UserIdentity {
                identity: identity(&i.text())?,
                verified_at: i.verified_at,
                verification: i.verification,
            })
        })
        .collect::<Result<_, UsersError>>()?;
    Ok(User {
        id: stored.id,
        username: stored.username,
        display_name: stored.display_name,
        role: stored.role,
        state: stored.state,
        timezone,
        default_identity,
        identities,
    })
}

/// Checks a username: 1 to [`MAX_USERNAME_CHARS`] letters, digits, `.`, `_`, and `-`.
fn check_username(username: &str) -> Result<(), UserError> {
    let ok = !username.is_empty()
        && username.chars().count() <= MAX_USERNAME_CHARS
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(UserError::Invalid(format!(
            "a username is 1 to {MAX_USERNAME_CHARS} letters, digits, `.`, `_`, or `-`"
        )))
    }
}

/// Checks a display name: 1 to [`MAX_DISPLAY_NAME_CHARS`] characters, none of them control
/// characters.
fn check_display_name(name: &str) -> Result<(), UserError> {
    let ok = !name.trim().is_empty()
        && name.chars().count() <= MAX_DISPLAY_NAME_CHARS
        && !name.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(UserError::Invalid(format!(
            "a display name is 1 to {MAX_DISPLAY_NAME_CHARS} characters, without control characters"
        )))
    }
}

/// Checks an identity's parts: together they are a valid [`Identity`].
fn check_identity(namespace: &str, native_id: &str) -> Result<Identity, UserError> {
    Identity::new(format!("{namespace}:{native_id}")).map_err(UserError::Invalid)
}

/// The users, in the database, and the [`UserList`] the router reads. See the module
/// documentation. Share it in an `Arc`.
#[derive(Debug)]
pub struct Users {
    store: Store,
    list: UserList,
    /// Held across read, validate, write, and replacing the snapshot.
    changing: Mutex<()>,
}

impl Users {
    /// Opens the users of `store`: imports `[owner]` of `config`, read from the file `file`, if
    /// the import has never run, loads the stored users into `list`, and logs that `[owner]` is
    /// ignored while the file still has it. `list` is the one the router and the web login hold.
    ///
    /// # Errors
    ///
    /// [`UsersError::Import`] if the import is due and `[owner]` is invalid (nothing is
    /// imported), or a database error.
    pub async fn open(
        store: Store,
        list: UserList,
        config: &Config,
        file: &Path,
    ) -> Result<Self, UsersError> {
        let legacy = config.legacy_owner.as_ref();
        let mut import = None;
        if !store.users_imported().await? {
            let file = file.display().to_string();
            let owner = legacy.map(|owner| import_owner(owner, &file)).transpose()?;
            import = store.users_import(owner, jiff::Timestamp::now()).await?;
        }
        if legacy.is_some() {
            match import {
                Some(true) => info!(
                    "imported the [owner] of doozers.toml as the admin `{}`: it was imported once \
                     and is not read again; remove it from doozers.toml",
                    identity::OWNER
                ),
                Some(false) => warn!(
                    "doozers.toml has [owner], which is ignored: users already existed when it \
                     would have been imported, and users are managed by Doozers; remove it from \
                     doozers.toml"
                ),
                None => warn!(
                    "doozers.toml has [owner], which is ignored: users and their identities are \
                     stored by Doozers, and [owner] was imported when they were first stored; \
                     remove it from doozers.toml"
                ),
            }
        }
        let users = Self {
            store,
            list,
            changing: Mutex::new(()),
        };
        users.load().await?;
        Ok(users)
    }

    /// The list the router reads. It follows every change made through this object.
    pub fn list(&self) -> UserList {
        self.list.clone()
    }

    /// Reads the stored users and makes them the list the router uses.
    async fn load(&self) -> Result<(), UsersError> {
        let users = self
            .store
            .users()
            .await?
            .into_iter()
            .map(to_user)
            .collect::<Result<Vec<_>, _>>()?;
        self.list.set(users);
        Ok(())
    }

    /// Replaces the snapshot after a write, as a change did.
    async fn publish(&self) -> Result<(), UserError> {
        Ok(self.load().await?)
    }

    /// Creates an active user with `identities`, each verified at `now`. The caller picks a fresh
    /// random id: the store forgets a removed user's id, and nothing may be keyed by an id that
    /// once was someone else's.
    ///
    /// # Errors
    ///
    /// [`UserError`]: the id, username, or an identity is taken, something isn't valid, or the
    /// database failed. Nothing changes.
    pub async fn create(
        &self,
        user: NewUser,
        identities: Vec<NewIdentity>,
        now: Timestamp,
    ) -> Result<User, UserError> {
        check_username(&user.username)?;
        check_display_name(&user.display_name)?;
        if let Some(name) = &user.timezone {
            parse_timezone(name).map_err(UserError::Invalid)?;
        }
        for identity in &identities {
            check_identity(&identity.namespace, &identity.native_id)?;
        }
        let _changing = self.changing.lock().await;
        let directory = self.list.get();
        for identity in &identities {
            let joined = format!("{}:{}", identity.namespace, identity.native_id);
            if directory.has_identity(&joined) {
                return Err(UserError::IdentityTaken(joined));
            }
        }
        let id = user.id.clone();
        let stored = self
            .store
            .user_create(user, identities, now)
            .await
            .map_err(|e| UserError::from_store(e, &id))?;
        self.publish().await?;
        Ok(to_user(stored)?)
    }

    /// Sets the state of user `id`.
    ///
    /// # Errors
    ///
    /// [`UserError::NoSuchUser`], or [`UserError::LastAdmin`] if the user is the last active
    /// admin and the state is `disabled`.
    pub async fn set_state(
        &self,
        id: &UserId,
        state: UserState,
        now: Timestamp,
    ) -> Result<User, UserError> {
        let _changing = self.changing.lock().await;
        let stored = self
            .store
            .user_set_state(id, state, now)
            .await
            .map_err(|e| UserError::from_store(e, id))?;
        self.publish().await?;
        Ok(to_user(stored)?)
    }

    /// Sets the role of user `id`: promotes a member to admin or demotes an admin to member.
    ///
    /// # Errors
    ///
    /// [`UserError::NoSuchUser`], or [`UserError::LastAdmin`] if the user is the last active
    /// admin and the role is `member`.
    pub async fn set_role(
        &self,
        id: &UserId,
        role: Role,
        now: Timestamp,
    ) -> Result<User, UserError> {
        let _changing = self.changing.lock().await;
        let stored = self
            .store
            .user_set_role(id, role, now)
            .await
            .map_err(|e| UserError::from_store(e, id))?;
        self.publish().await?;
        Ok(to_user(stored)?)
    }

    /// Sets the display name and time zone of user `id`: the profile a user edits on their account
    /// page. The display name is checked as when a user is created, and `timezone`, an IANA name
    /// or `None` for none, is checked as a time zone in `doozers.toml` is.
    ///
    /// # Errors
    ///
    /// [`UserError::Invalid`] for a display name or time zone that isn't valid, and
    /// [`UserError::NoSuchUser`].
    pub async fn set_profile(
        &self,
        id: &UserId,
        display_name: &str,
        timezone: Option<&str>,
        now: Timestamp,
    ) -> Result<User, UserError> {
        let display_name = display_name.trim();
        check_display_name(display_name)?;
        if let Some(name) = timezone {
            parse_timezone(name).map_err(UserError::Invalid)?;
        }
        let _changing = self.changing.lock().await;
        let stored = self
            .store
            .user_profile_set(id, display_name, timezone, now)
            .await
            .map_err(|e| UserError::from_store(e, id))?;
        self.publish().await?;
        Ok(to_user(stored)?)
    }

    /// Deletes the account of user `id`, with its identities, and returns the user as it was.
    /// Everything else the user owns stays: forgetting that data (PLAN section 11.4) is built on
    /// this and does it in the same operation.
    ///
    /// # Errors
    ///
    /// [`UserError::NoSuchUser`], or [`UserError::LastAdmin`] if the user is the last active
    /// admin.
    pub async fn remove(&self, id: &UserId) -> Result<User, UserError> {
        let _changing = self.changing.lock().await;
        let stored = self
            .store
            .user_delete(id)
            .await
            .map_err(|e| UserError::from_store(e, id))?;
        self.publish().await?;
        Ok(to_user(stored)?)
    }

    /// Adds the identity `<namespace>:<native_id>` to user `id`, verified at `now`.
    ///
    /// # Errors
    ///
    /// [`UserError::NoSuchUser`], [`UserError::IdentityTaken`] if any user has it, or
    /// [`UserError::Invalid`] if it isn't a valid identity.
    pub async fn add_identity(
        &self,
        id: &UserId,
        namespace: &str,
        native_id: &str,
        verification: Verification,
        now: Timestamp,
    ) -> Result<User, UserError> {
        check_identity(namespace, native_id)?;
        let _changing = self.changing.lock().await;
        self.add_identity_locked(id, namespace, native_id, verification, now)
            .await
    }

    async fn add_identity_locked(
        &self,
        id: &UserId,
        namespace: &str,
        native_id: &str,
        verification: Verification,
        now: Timestamp,
    ) -> Result<User, UserError> {
        let joined = format!("{namespace}:{native_id}");
        if self.list.get().has_identity(&joined) {
            return Err(UserError::IdentityTaken(joined));
        }
        let stored = self
            .store
            .user_identity_add(
                id,
                NewIdentity {
                    namespace: namespace.to_owned(),
                    native_id: native_id.to_owned(),
                    verification,
                },
                now,
            )
            .await
            .map_err(|e| UserError::from_store(e, id))?;
        self.publish().await?;
        Ok(to_user(stored)?)
    }

    /// Removes `identity` from user `id`.
    ///
    /// # Errors
    ///
    /// [`UserError::NoSuchUser`], or [`UserError::Invalid`] if the user doesn't have it.
    pub async fn remove_identity(
        &self,
        id: &UserId,
        identity: &Identity,
        now: Timestamp,
    ) -> Result<User, UserError> {
        let _changing = self.changing.lock().await;
        let stored = self
            .store
            .user_identity_remove(id, identity.as_str(), now)
            .await
            .map_err(|e| match e {
                doozers_store::Error::NotFound(_) => {
                    UserError::Invalid(format!("user {id} doesn't have the identity {identity}"))
                }
                other => UserError::from_store(other, id),
            })?;
        self.publish().await?;
        Ok(to_user(stored)?)
    }

    /// Makes `identity` the default identity of user `id`: where jobs and proactive messages go.
    ///
    /// # Errors
    ///
    /// [`UserError::NoSuchUser`], or [`UserError::Invalid`] if the user doesn't have it.
    pub async fn set_default_identity(
        &self,
        id: &UserId,
        identity: &Identity,
        now: Timestamp,
    ) -> Result<User, UserError> {
        let _changing = self.changing.lock().await;
        let stored = self
            .store
            .user_default_identity_set(id, identity.as_str(), now)
            .await
            .map_err(|e| match e {
                doozers_store::Error::NotFound(_) => {
                    UserError::Invalid(format!("user {id} doesn't have the identity {identity}"))
                }
                other => UserError::from_store(other, id),
            })?;
        self.publish().await?;
        Ok(to_user(stored)?)
    }

    /// Sets the identity a trusted proxy logs user `id` in as (PLAN section 3.4): `web:<native_id>`,
    /// verified as `admin`, or none if `native_id` is `None`. It replaces the user's other web
    /// identities, except `web:<id>`, the one a link sign-in acts as, so a user has one proxy
    /// identity. An admin sets it on the user's page; the user can't, and no `/link` is involved,
    /// because the proxy is the admin's own infrastructure.
    ///
    /// # Errors
    ///
    /// [`UserError::NoSuchUser`]; [`UserError::Invalid`] if `native_id` isn't a valid identity
    /// or is `web:<id>` itself; [`UserError::IdentityTaken`] if another user has it. Nothing
    /// changes then, so the user keeps the identity they had.
    pub async fn set_proxy_identity(
        &self,
        id: &UserId,
        native_id: Option<&str>,
        now: Timestamp,
    ) -> Result<User, UserError> {
        if let Some(native_id) = native_id {
            check_identity(web_namespace(), native_id)?;
            if native_id == id.as_str() {
                return Err(UserError::Invalid(format!(
                    "`web:{id}` is the identity a link sign-in acts as, not a proxy identity"
                )));
            }
        }
        let _changing = self.changing.lock().await;
        let stored = self
            .store
            .user_proxy_identity_set(id, web_namespace(), id.as_str(), native_id, now)
            .await
            .map_err(|e| UserError::from_store(e, id))?;
        self.publish().await?;
        Ok(to_user(stored)?)
    }

    /// Handles a `/link <code>` message from the identity `native_id` in `namespace`: if `code` is
    /// a pending code for that namespace, links the identity to the code's user, verified as
    /// `link`, and returns the user; otherwise counts a wrong guess against the pending codes
    /// (at most `max_attempts` wrong codes kill a code) and returns `None`. The snapshot has the
    /// identity before this returns, so the next message from it is the user's.
    ///
    /// # Errors
    ///
    /// [`UserError::Invalid`] if the sender isn't a valid identity, [`UserError::IdentityTaken`]
    /// if a user has it already (the code stays), or a database error.
    pub async fn redeem_link_code(
        &self,
        namespace: &str,
        native_id: &str,
        code: &str,
        now: Timestamp,
        max_attempts: u32,
    ) -> Result<Option<User>, UserError> {
        check_identity(namespace, native_id)?;
        let _changing = self.changing.lock().await;
        let redeemed = self
            .store
            .user_link_code_redeem(namespace, native_id, code, now, max_attempts)
            .await
            .map_err(|e| match e {
                doozers_store::Error::IdentityTaken { identity } => {
                    UserError::IdentityTaken(identity)
                }
                other => UserError::Store(other),
            })?;
        match redeemed {
            LinkRedeemed::Linked(stored) => {
                self.publish().await?;
                Ok(Some(to_user(stored)?))
            }
            LinkRedeemed::Refused => Ok(None),
        }
    }

    /// The user `doozers admin login-link` makes a recovery link for, and the first admin if there
    /// are no users yet.
    ///
    /// - With no `requested` user and no users at all, it creates the first admin, the id
    ///   [`identity::OWNER`] (the id an imported owner has, and the user config jobs run as), and
    ///   returns it.
    /// - With no `requested` user and exactly one user, it is that user, as before there were
    ///   several.
    /// - With no `requested` user and several users, it asks for one
    ///   ([`LoginUserError::NeedUser`]).
    /// - With a `requested` user, who must exist and be active, it returns the user.
    ///
    /// # Errors
    ///
    /// [`LoginUserError`], with a message for the operator.
    pub async fn login_user(
        &self,
        requested: Option<&UserId>,
        now: Timestamp,
    ) -> Result<UserId, LoginUserError> {
        let _changing = self.changing.lock().await;
        let directory = self.list.get();
        let id = match (requested, directory.users()) {
            (Some(id), _) => id,
            (None, [only]) => &only.id,
            (None, []) => {
                return self.create_first_admin(now).await;
            }
            (None, several) => {
                let list = several
                    .iter()
                    .map(|user| format!("{} ({})", user.id, user.username))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(LoginUserError::NeedUser(format!(
                    "there are {} users, so say whose link you want with --user <id>: {list}",
                    several.len()
                )));
            }
        };
        let user = directory
            .user(id)
            .ok_or_else(|| LoginUserError::NoSuchUser(id.clone()))?;
        if !user.is_active() {
            return Err(LoginUserError::Disabled(id.clone()));
        }
        Ok(id.clone())
    }

    /// Makes a one-time link for `user`, valid from `now` for [`INVITE_TTL`], made by `created_by`
    /// (`None`: the operator). An `invite` sets up a new account and a `recovery` link gets a user
    /// back in; both sign in whoever spends them first, and neither removes a way to sign in.
    ///
    /// The user gets the identity `web:<id>`, verified as `admin`, if they lack it: a web
    /// sign-in acts as `web:<id>`, so without it the router would drop their messages.
    ///
    /// # Errors
    ///
    /// [`LoginUserError::NoSuchUser`] or [`LoginUserError::Disabled`] for a user who doesn't exist
    /// or can't sign in, [`LoginUserError::WebIdentityTaken`] if another user has `web:<id>`, or a
    /// database error.
    pub async fn create_invite(
        &self,
        user: &UserId,
        kind: InviteKind,
        created_by: Option<&UserId>,
        now: Timestamp,
    ) -> Result<NewInvite, LoginUserError> {
        let _changing = self.changing.lock().await;
        let directory = self.list.get();
        let found = directory
            .user(user)
            .ok_or_else(|| LoginUserError::NoSuchUser(user.clone()))?;
        if !found.is_active() {
            return Err(LoginUserError::Disabled(user.clone()));
        }
        let web = NativeId::new(user.as_str()).map_err(|e| {
            LoginUserError::Users(UserError::Invalid(format!("user id {user}: {e}")))
        })?;
        match directory.resolve(web_namespace(), &web) {
            Some(owner) if owner.id == *user => {}
            Some(owner) => {
                return Err(LoginUserError::WebIdentityTaken {
                    user: user.clone(),
                    owner: owner.id.clone(),
                });
            }
            None => {
                self.add_identity_locked(
                    user,
                    web_namespace(),
                    user.as_str(),
                    Verification::Admin,
                    now,
                )
                .await
                .map_err(LoginUserError::Users)?;
            }
        }
        let token = crate::channels::chat_ids::random_hex(32);
        let expires_at = now
            .checked_add(INVITE_TTL)
            .expect("72 hours from now is a valid time");
        self.store
            .user_invite_create(&token, user, kind, created_by, now, expires_at)
            .await
            .map_err(|e| LoginUserError::Users(UserError::from_store(e, user)))?;
        info!(%user, kind = kind.as_str(), "created a one-time link");
        Ok(NewInvite { token, expires_at })
    }

    /// Creates the first admin, with the changes lock held.
    async fn create_first_admin(&self, now: Timestamp) -> Result<UserId, LoginUserError> {
        let user = NewUser {
            id: identity::owner(),
            username: FIRST_ADMIN.0.to_owned(),
            display_name: FIRST_ADMIN.1.to_owned(),
            role: Role::Admin,
            timezone: None,
            default_identity: Some(format!("{}:{}", web_namespace(), identity::OWNER)),
        };
        let web = NewIdentity {
            namespace: web_namespace().to_owned(),
            native_id: identity::OWNER.to_owned(),
            verification: Verification::Admin,
        };
        self.store
            .user_create(user, vec![web], now)
            .await
            .map_err(|e| LoginUserError::Users(UserError::from_store(e, &identity::owner())))?;
        self.publish().await.map_err(LoginUserError::Users)?;
        info!("created the first admin `{}`", identity::OWNER);
        Ok(identity::owner())
    }
}

/// The identity namespace of the web channel.
fn web_namespace() -> &'static str {
    crate::channels::chat_ids::COMPONENT
}

#[cfg(test)]
impl User {
    /// An active member `id` with `identities` and no time zone.
    pub(crate) fn member(id: &str, identities: &[&str]) -> Self {
        Self {
            id: UserId::new(id).unwrap(),
            username: id.to_owned(),
            display_name: id.to_owned(),
            role: Role::Member,
            state: UserState::Active,
            timezone: None,
            default_identity: None,
            identities: identities
                .iter()
                .map(|identity| UserIdentity {
                    identity: Identity::new(*identity).unwrap(),
                    verified_at: Timestamp::UNIX_EPOCH,
                    verification: Verification::Imported,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
impl User {
    /// An active admin `id` with `identities` and no time zone.
    pub(crate) fn admin(id: &str, identities: &[&str]) -> Self {
        Self {
            role: Role::Admin,
            ..Self::member(id, identities)
        }
    }
}

/// The users of the `[owner]` of `config`, as importing it would make them, for tests that build
/// a registry without opening the users.
#[cfg(test)]
pub(crate) fn legacy_owner_users(config: &Config) -> Vec<User> {
    let imported = import_owner(config.legacy_owner.as_ref().unwrap(), "doozers.toml").unwrap();
    let identities = imported
        .identities
        .iter()
        .map(|text| {
            let (namespace, native_id) = text.split_once(':').unwrap();
            doozers_store::StoredIdentity {
                namespace: namespace.to_owned(),
                native_id: native_id.to_owned(),
                verified_at: Timestamp::UNIX_EPOCH,
                verification: Verification::Imported,
            }
        })
        .collect();
    let user = imported.user;
    vec![
        to_user(StoredUser {
            id: user.id,
            username: user.username,
            display_name: user.display_name,
            role: user.role,
            state: UserState::Active,
            timezone: user.timezone,
            default_identity: user.default_identity,
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            identities,
        })
        .unwrap(),
    ]
}

#[cfg(test)]
impl UserList {
    /// Active admins `admins` and active members `members`, with no identities.
    pub(crate) fn with_roles(admins: &[&str], members: &[&str]) -> Self {
        Self::new(
            admins
                .iter()
                .map(|id| User::admin(id, &[]))
                .chain(members.iter().map(|id| User::member(id, &[])))
                .collect(),
        )
    }

    /// The owner, an admin with `identities`, the first of them the default, and `timezone` if
    /// set.
    pub(crate) fn owner(identities: &[&str], timezone: Option<&str>) -> Self {
        let mut owner = User::member(identity::OWNER, identities);
        owner.role = Role::Admin;
        owner.display_name = "Owner".to_owned();
        owner.default_identity = Some(Identity::new(identities[0]).unwrap());
        owner.timezone = timezone.map(|name| TimeZone::get(name).unwrap());
        Self::new(vec![owner])
    }
}

#[cfg(test)]
mod tests {
    use doozers_types::{ConversationId, ConversationKind, ConversationRef};

    use super::*;

    fn native(id: &str) -> NativeId {
        NativeId::new(id).unwrap()
    }

    fn resolves(list: &UserList, namespace: &str, id: &str) -> bool {
        list.get().resolve(namespace, &native(id)).is_some()
    }

    fn owner_ids(ids: &[&str]) -> UserList {
        UserList::owner(ids, None)
    }

    #[test]
    fn identities_match_exactly_across_the_joined_namespace() {
        let list = owner_ids(&["whatsapp:+15551234567", "irc:libera.chat:account:stephen"]);
        assert!(resolves(&list, "whatsapp", "+15551234567"));
        assert!(resolves(&list, "irc:libera.chat", "account:stephen"));
        // Not another number, another namespace, or another case.
        assert!(!resolves(&list, "whatsapp", "+15550000000"));
        assert!(!resolves(&list, "irc:other.net", "account:stephen"));
        assert!(!resolves(&list, "irc:libera.chat", "account:Stephen"));
        // A namespace may contain `:`, so only the whole joined string matters.
        assert!(!resolves(&list, "irc", "libera.chat"));
        assert!(!resolves(&list, "irc:libera.chat:account", "stephen-x"));
    }

    #[test]
    fn an_email_domain_matches_in_any_case_and_the_local_part_does_not() {
        let list = owner_ids(&["email:me@example.com", "email:Case@example.org"]);
        assert!(resolves(&list, "email", "me@example.com"));
        assert!(resolves(&list, "email", "me@Example.COM"));
        assert!(!resolves(&list, "email", "Me@example.com"));
        assert!(resolves(&list, "email", "Case@EXAMPLE.org"));
        assert!(!resolves(&list, "email", "case@example.org"));
        // The configured domain's case doesn't matter either.
        let list = owner_ids(&["email:me@Example.com"]);
        assert!(resolves(&list, "email", "me@example.com"));
        // Only the `email` namespace gets this.
        let list = owner_ids(&["other:me@example.com"]);
        assert!(!resolves(&list, "other", "me@EXAMPLE.com"));
        // A sub-namespace of `email` does too: the first segment names the namespace.
        let list = owner_ids(&["email:work:me@example.com"]);
        assert!(resolves(&list, "email:work", "me@EXAMPLE.com"));
    }

    #[test]
    fn matching() {
        let owner = owner_ids(&[
            "whatsapp:+15551234567",
            "irc:libera.chat:account:stephen",
            "email:Me@Example.com",
            "web:owner",
        ]);
        let yes = [
            ("whatsapp", "+15551234567"),
            ("irc:libera.chat", "account:stephen"),
            ("web", "owner"),
            ("email", "Me@Example.com"),
            ("email", "Me@example.COM"),
        ];
        for (namespace, id) in yes {
            assert!(resolves(&owner, namespace, id), "{namespace}:{id}");
        }
        let no = [
            // Another namespace or a near miss on the namespace boundary.
            ("whatsapp:x", "+15551234567"),
            ("whats", "app:+15551234567"),
            ("irc:oftc", "account:stephen"),
            ("irc:libera.chat", "account:Stephen"),
            ("irc:libera.chat", "account:stephe"),
            ("irc:libera.chat", "account:stephenx"),
            ("WEB", "owner"),
            ("web", "Owner"),
            // Email local parts stay case-sensitive.
            ("email", "me@example.com"),
            ("email", "Me@Example.org"),
            ("email", "Me"),
            // The domain rule is only for the email namespace.
            ("whatsapp", "+15551234567@S"),
        ];
        for (namespace, id) in no {
            assert!(!resolves(&owner, namespace, id), "{namespace}:{id}");
        }
    }

    #[test]
    fn email_without_at_sign_is_exact() {
        let owner = owner_ids(&["email:postmaster"]);
        assert!(resolves(&owner, "email", "postmaster"));
        assert!(!resolves(&owner, "email", "Postmaster"));
    }

    #[test]
    fn the_sender_wins_over_its_aliases_and_an_alias_resolves_when_the_sender_does_not() {
        let list = UserList::new(vec![
            user("a", &["whatsapp:+1"]),
            user("b", &["whatsapp:123@lid", "whatsapp:+2"]),
        ]);
        let mut message = message("+2");
        message.sender_aliases = vec![native("+1")];
        let directory = list.get();
        assert_eq!(
            directory.resolve_sender("whatsapp", &message).unwrap().id,
            UserId::new("b").unwrap()
        );
        message.sender = native("+9");
        assert_eq!(
            directory.resolve_sender("whatsapp", &message).unwrap().id,
            UserId::new("a").unwrap()
        );
        message.sender_aliases.clear();
        assert!(directory.resolve_sender("whatsapp", &message).is_none());
    }

    fn message(sender: &str) -> InboundMessage {
        InboundMessage {
            conversation: ConversationRef {
                id: ConversationId::new(sender).unwrap(),
                kind: ConversationKind::Direct,
                title: None,
            },
            sender: native(sender),
            sender_aliases: Vec::new(),
            sender_authenticated: true,
            sender_display_name: None,
            text: "hi".to_owned(),
            attachments: Vec::new(),
            message_id: None,
            reply_to: None,
            sent_at: None,
        }
    }

    fn user(id: &str, identities: &[&str]) -> User {
        User::member(id, identities)
    }

    #[test]
    fn a_replaced_list_applies_to_the_next_lookup_and_snapshots_stay_whole() {
        let list = owner_ids(&["web:owner"]);
        let clone = list.clone();
        let before = list.get();
        clone.set(vec![user("ann", &["web:ann"])]);
        assert!(resolves(&list, "web", "ann"));
        assert!(!resolves(&list, "web", "owner"));
        // A snapshot taken earlier still sees the users it was made with.
        assert!(before.resolve("web", &native("owner")).is_some());
        assert!(before.user(&UserId::new("ann").unwrap()).is_none());
        assert_eq!(
            list.timezone(&UserId::new("ann").unwrap()),
            None,
            "no time zone set"
        );
    }

    #[test]
    fn the_owner_list_has_the_owner_as_an_active_admin_with_its_time_zone() {
        let list = UserList::owner(&["web:owner"], Some("America/Los_Angeles"));
        let directory = list.get();
        let owner = directory.user(&identity::owner()).unwrap();
        assert_eq!(owner.role, Role::Admin);
        assert!(owner.is_active());
        assert_eq!(
            owner.default_identity.as_ref().map(Identity::as_str),
            Some("web:owner")
        );
        assert_eq!(
            list.timezone(&identity::owner()).unwrap().iana_name(),
            Some("America/Los_Angeles")
        );
    }

    // The stored users.

    use crate::config::LoadOptions;
    use crate::paths::DataRoot;

    /// A data root and database for `doozers.toml` with `toml`.
    struct Home {
        dir: tempfile::TempDir,
        store: Store,
    }

    impl Home {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path().join("doozers.db")).await.unwrap();
            Self { dir, store }
        }

        fn config(&self, toml: &str) -> Config {
            let root = DataRoot::new(self.dir.path(), &self.dir.path().join("tmp")).unwrap();
            std::fs::write(root.config_file(), toml).unwrap();
            Config::load_with(
                &root,
                &LoadOptions {
                    env: &|_| None,
                    doozers_exe: None,
                    ..LoadOptions::default()
                },
            )
            .unwrap_or_else(|e| panic!("{e}"))
        }

        /// Starts the users as `serve` does, with `toml` as `doozers.toml`.
        async fn open(&self, toml: &str) -> Result<(Users, UserList), UsersError> {
            let list = UserList::default();
            let config = self.config(toml);
            let file = self.dir.path().join("doozers.toml");
            let users = Users::open(self.store.clone(), list.clone(), &config, &file).await?;
            Ok((users, list))
        }
    }

    fn id(text: &str) -> UserId {
        UserId::new(text).unwrap()
    }

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + secs).unwrap()
    }

    fn member(id: &str) -> NewUser {
        NewUser {
            id: UserId::new(id).unwrap(),
            username: id.to_owned(),
            display_name: id.to_owned(),
            role: Role::Member,
            timezone: None,
            default_identity: None,
        }
    }

    const OWNER: &str = "[owner]\nidentities = [\"whatsapp:+15551234567\", \
        \"irc:libera.chat:account:stephen\", \"email:Me@Example.com\"]\n\
        default = \"irc:libera.chat:account:stephen\"\ntimezone = \"America/Los_Angeles\"\n";

    #[tokio::test]
    async fn the_owner_is_imported_as_the_admin_owner_with_everything_it_declared() {
        let home = Home::new().await;
        let (_users, list) = home.open(OWNER).await.unwrap();
        let directory = list.get();
        let [owner] = directory.users() else {
            panic!("{:?}", directory.users());
        };
        assert_eq!(owner.id, identity::owner());
        assert_eq!(owner.username, "owner");
        assert_eq!(owner.role, Role::Admin);
        assert!(owner.is_active());
        assert_eq!(
            owner.timezone.as_ref().unwrap().iana_name(),
            Some("America/Los_Angeles")
        );
        assert_eq!(
            owner.default_identity.as_ref().map(Identity::as_str),
            Some("irc:libera.chat:account:stephen")
        );
        let identities: Vec<_> = owner
            .identities
            .iter()
            .map(|i| (i.identity.as_str(), i.verification))
            .collect();
        assert_eq!(
            identities,
            [
                ("whatsapp:+15551234567", Verification::Imported),
                ("irc:libera.chat:account:stephen", Verification::Imported),
                ("email:Me@example.com", Verification::Imported),
            ]
        );
        // The list resolves the owner's identities as the router will, whatever the split.
        assert!(
            directory
                .resolve("irc:libera.chat", &native("account:stephen"))
                .is_some()
        );
        assert!(
            directory
                .resolve("email", &native("Me@EXAMPLE.com"))
                .is_some()
        );
        assert!(home.store.users_imported().await.unwrap());
    }

    #[tokio::test]
    async fn a_single_identity_is_the_default_and_the_time_zone_is_optional() {
        let home = Home::new().await;
        let (_users, list) = home
            .open("[owner]\nidentities = [\"web:owner\"]\n")
            .await
            .unwrap();
        let owner = list.get().user(&identity::owner()).unwrap().clone();
        assert_eq!(owner.default_identity.unwrap().as_str(), "web:owner");
        assert!(owner.timezone.is_none());
    }

    #[tokio::test]
    async fn the_import_runs_once_and_what_changes_later_is_ignored() {
        let home = Home::new().await;
        home.open(OWNER).await.unwrap();
        // A changed `[owner]`, and then none at all, change nothing: the database is the truth.
        let (_users, list) = home
            .open("[owner]\nidentities = [\"web:other\"]\n")
            .await
            .unwrap();
        let directory = list.get();
        assert_eq!(directory.users().len(), 1);
        assert!(
            directory
                .resolve("whatsapp", &native("+15551234567"))
                .is_some()
        );
        assert!(directory.resolve("web", &native("other")).is_none());
        let (_users, list) = home.open("").await.unwrap();
        assert!(
            list.get()
                .resolve("whatsapp", &native("+15551234567"))
                .is_some()
        );
        // Not even an invalid `[owner]` stops a start after the import.
        let (_users, list) = home.open("[owner]\nidentities = []\n").await.unwrap();
        assert_eq!(list.get().users().len(), 1);
    }

    #[tokio::test]
    async fn a_fresh_install_has_no_users_and_a_later_owner_is_not_imported() {
        let home = Home::new().await;
        let (_users, list) = home.open("").await.unwrap();
        assert!(list.get().users().is_empty());
        assert!(home.store.users_imported().await.unwrap());
        let (_users, list) = home.open(OWNER).await.unwrap();
        assert!(list.get().users().is_empty());
    }

    #[tokio::test]
    async fn an_invalid_owner_is_refused_with_its_key_and_imports_nothing() {
        let cases = [
            ("[owner]\nnmae = 1", "owner", "missing field"),
            (
                "[owner]\nidentities = \"web:owner\"",
                "owner",
                "invalid type",
            ),
            (
                "[owner]\nidentities = []",
                "owner.identities",
                "at least one",
            ),
            (
                "[owner]\nidentities = [\"whatsapp\"]",
                "owner.identities[0]",
                "<namespace>:<native id>",
            ),
            (
                "[owner]\nidentities = [\"a:b\", \"a:b\"]",
                "owner.identities[1]",
                "listed twice",
            ),
            (
                "[owner]\nidentities = [\"a:b\", \"c:d\"]",
                "owner.default",
                "missing",
            ),
            (
                "[owner]\nidentities = [\"a:b\", \"c:d\"]\ndefault = \"e:f\"",
                "owner.default",
                "not one of",
            ),
            (
                "[owner]\nidentities = [\"a:b\"]\ndefault = \"nodefault\"",
                "owner.default",
                "<namespace>:<native id>",
            ),
            (
                "[owner]\nidentities = [\"a:b\"]\ntimezone = \"PST\"",
                "owner.timezone",
                "unknown time zone",
            ),
        ];
        for (toml, key, reason) in cases {
            let home = Home::new().await;
            let err = home.open(toml).await.map(|_| ()).unwrap_err();
            let UsersError::Import {
                file,
                key: got,
                reason: why,
            } = &err
            else {
                panic!("{toml}: {err}");
            };
            assert_eq!(got, key, "{toml}");
            assert!(why.contains(reason), "{toml}: {why}");
            assert!(file.ends_with("doozers.toml"), "{file}");
            assert!(
                err.to_string().starts_with(&format!("{file}: owner")),
                "{err}"
            );
            assert!(!home.store.users_imported().await.unwrap(), "{toml}");
            assert!(home.store.users().await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn owner_identities_differing_in_an_email_domain_are_one_and_the_first_spelling_stays() {
        let home = Home::new().await;
        let (_users, list) = home
            .open(
                "[owner]\nidentities = [\"email:a@Example.com\", \"web:owner\", \"email:a@example.COM\"]\n\
                 default = \"email:a@EXAMPLE.com\"\n",
            )
            .await
            .unwrap();
        let directory = list.get();
        let owner = directory.user(&identity::owner()).unwrap();
        let identities: Vec<_> = owner
            .identities
            .iter()
            .map(|i| i.identity.as_str())
            .collect();
        assert_eq!(identities, ["email:a@example.com", "web:owner"]);
        assert_eq!(
            owner.default_identity.as_ref().unwrap().as_str(),
            "email:a@example.com"
        );
        // The same text twice is still a mistake.
        let other = Home::new().await;
        let err = other
            .open("[owner]\nidentities = [\"email:a@x.com\", \"email:a@x.com\"]\n")
            .await
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("listed twice"), "{err}");
    }

    #[tokio::test]
    async fn every_change_replaces_the_snapshot() {
        let home = Home::new().await;
        let (users, list) = home.open("").await.unwrap();
        let before = list.get();
        let ann = users
            .create(
                member("ann"),
                vec![NewIdentity {
                    namespace: "whatsapp".into(),
                    native_id: "+15557654321".into(),
                    verification: Verification::Link,
                }],
                at(0),
            )
            .await
            .unwrap();
        assert_eq!(ann.default_identity, None);
        assert!(resolves(&list, "whatsapp", "+15557654321"));
        // A snapshot taken earlier is unchanged.
        assert!(before.users().is_empty());

        users
            .add_identity(
                &id("ann"),
                "irc:libera.chat",
                "account:ann",
                Verification::Admin,
                at(1),
            )
            .await
            .unwrap();
        assert!(resolves(&list, "irc:libera.chat", "account:ann"));
        let ann = list.get().user(&id("ann")).unwrap().clone();
        assert_eq!(ann.identities.len(), 2);
        users
            .remove_identity(
                &id("ann"),
                &Identity::new("whatsapp:+15557654321").unwrap(),
                at(2),
            )
            .await
            .unwrap();
        assert!(!resolves(&list, "whatsapp", "+15557654321"));
        assert!(resolves(&list, "irc:libera.chat", "account:ann"));

        users
            .set_state(&id("ann"), UserState::Disabled, at(3))
            .await
            .unwrap();
        assert!(!list.get().user(&id("ann")).unwrap().is_active());
        users
            .set_state(&id("ann"), UserState::Active, at(4))
            .await
            .unwrap();
        assert!(list.get().user(&id("ann")).unwrap().is_active());
        // A fresh start sees the same users.
        let (_again, reloaded) = home.open("").await.unwrap();
        assert_eq!(reloaded.get().users(), list.get().users());
    }

    #[tokio::test]
    async fn a_profile_change_replaces_the_snapshot_and_is_checked_first() {
        let home = Home::new().await;
        let (users, list) = home.open("").await.unwrap();
        users.create(member("ann"), vec![], at(0)).await.unwrap();

        let ann = users
            .set_profile(
                &id("ann"),
                "  Ann Example ",
                Some("America/Los_Angeles"),
                at(1),
            )
            .await
            .unwrap();
        assert_eq!(ann.display_name, "Ann Example");
        // Padding doesn't count towards the length: a name that fits once trimmed is kept.
        let padded = format!(" {} ", "x".repeat(MAX_DISPLAY_NAME_CHARS));
        let long = users
            .set_profile(&id("ann"), &padded, None, at(1))
            .await
            .unwrap();
        assert_eq!(long.display_name.chars().count(), MAX_DISPLAY_NAME_CHARS);
        users
            .set_profile(
                &id("ann"),
                "Ann Example",
                Some("America/Los_Angeles"),
                at(1),
            )
            .await
            .unwrap();
        let seen = list.get().user(&id("ann")).unwrap().clone();
        assert_eq!(seen.display_name, "Ann Example");
        assert_eq!(
            seen.timezone.as_ref().and_then(|zone| zone.iana_name()),
            Some("America/Los_Angeles")
        );
        users
            .set_profile(&id("ann"), "Ann", None, at(2))
            .await
            .unwrap();
        assert!(list.get().user(&id("ann")).unwrap().timezone.is_none());

        for (name, zone) in [
            ("", None),
            ("   ", None),
            ("Ann\u{7}", None),
            (&"x".repeat(MAX_DISPLAY_NAME_CHARS + 1), None),
            ("Ann", Some("Mars/Olympus")),
            ("Ann", Some("")),
        ] {
            let err = users
                .set_profile(&id("ann"), name, zone, at(3))
                .await
                .unwrap_err();
            assert!(
                matches!(err, UserError::Invalid(_)),
                "{name:?} {zone:?}: {err}"
            );
        }
        assert_eq!(list.get().user(&id("ann")).unwrap().display_name, "Ann");
        let err = users
            .set_profile(&id("nobody"), "Nobody", None, at(3))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::NoSuchUser(_)), "{err}");
    }

    #[tokio::test]
    async fn changes_are_checked_before_they_are_made() {
        let home = Home::new().await;
        let (users, list) = home.open("").await.unwrap();
        users.create(member("ann"), vec![], at(0)).await.unwrap();
        let web = |id: &str| (id.to_owned(), Verification::Admin);
        users
            .add_identity(&id("ann"), "email", "Ann@Example.com", web("").1, at(1))
            .await
            .unwrap();

        let bad_names = ["", "a b", "ann!", &"x".repeat(MAX_USERNAME_CHARS + 1)];
        for name in bad_names {
            let new = NewUser {
                username: name.to_owned(),
                ..member("bob")
            };
            let err = users.create(new, vec![], at(2)).await.unwrap_err();
            assert!(matches!(err, UserError::Invalid(_)), "{name:?}: {err}");
        }
        let blank = NewUser {
            display_name: "  ".into(),
            ..member("bob")
        };
        assert!(matches!(
            users.create(blank, vec![], at(2)).await,
            Err(UserError::Invalid(_))
        ));
        let zone = NewUser {
            timezone: Some("Mars/Olympus".into()),
            ..member("bob")
        };
        assert!(matches!(
            users.create(zone, vec![], at(2)).await,
            Err(UserError::Invalid(_))
        ));
        let taken = |namespace: &str, native: &str| NewIdentity {
            namespace: namespace.to_owned(),
            native_id: native.to_owned(),
            verification: Verification::Link,
        };
        // Another user can't have the same identity, whatever the case of an email domain.
        for native in ["Ann@Example.com", "Ann@EXAMPLE.COM"] {
            let err = users
                .create(member("bob"), vec![taken("email", native)], at(2))
                .await
                .unwrap_err();
            assert!(
                matches!(err, UserError::IdentityTaken(_)),
                "{native}: {err}"
            );
        }
        let err = users
            .create(member("ANN"), vec![], at(2))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::UsernameTaken), "{err}");
        let same_id = NewUser {
            username: "other".into(),
            ..member("ann")
        };
        let err = users.create(same_id, vec![], at(2)).await.unwrap_err();
        assert!(matches!(err, UserError::IdTaken), "{err}");
        let err = users
            .add_identity(&id("nobody"), "web", "x", Verification::Admin, at(2))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::NoSuchUser(_)), "{err}");
        let err = users
            .add_identity(&id("ann"), "web", "a b", Verification::Admin, at(2))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::Invalid(_)), "{err}");
        let err = users
            .remove_identity(&id("ann"), &Identity::new("web:none").unwrap(), at(2))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::Invalid(_)), "{err}");
        // Nothing of that was stored.
        assert_eq!(list.get().users().len(), 1);
        assert_eq!(home.store.users().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_last_active_admin_stays_active() {
        let home = Home::new().await;
        let (users, list) = home
            .open("[owner]\nidentities = [\"web:owner\"]\n")
            .await
            .unwrap();
        users.create(member("ann"), vec![], at(0)).await.unwrap();
        let err = users
            .set_state(&identity::owner(), UserState::Disabled, at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::LastAdmin), "{err}");
        // A member can be disabled, and the sole admin stays.
        users
            .set_state(&id("ann"), UserState::Disabled, at(2))
            .await
            .unwrap();
        assert!(list.get().user(&identity::owner()).unwrap().is_active());
        let admin = NewUser {
            role: Role::Admin,
            ..member("bob")
        };
        users.create(admin, vec![], at(3)).await.unwrap();
        users
            .set_state(&identity::owner(), UserState::Disabled, at(4))
            .await
            .unwrap();
        let err = users
            .set_state(&id("bob"), UserState::Disabled, at(5))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::LastAdmin), "{err}");
        let err = users
            .set_state(&id("nobody"), UserState::Disabled, at(5))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::NoSuchUser(_)), "{err}");
    }

    #[tokio::test]
    async fn the_last_active_admin_can_be_neither_demoted_nor_removed() {
        let home = Home::new().await;
        let (users, list) = home
            .open("[owner]\nidentities = [\"web:owner\"]\n")
            .await
            .unwrap();
        users.create(member("ann"), vec![], at(0)).await.unwrap();
        let owner = identity::owner();
        let is_admin = |user: &UserId| list.is_admin(user);
        assert!(is_admin(&owner));
        assert!(!is_admin(&id("ann")));
        assert!(!is_admin(&id("nobody")));

        for err in [
            users
                .set_role(&owner, Role::Member, at(1))
                .await
                .unwrap_err(),
            users.remove(&owner).await.unwrap_err(),
        ] {
            assert!(matches!(err, UserError::LastAdmin), "{err}");
        }
        assert!(is_admin(&owner), "refused changes leave the admin");
        assert_eq!(home.store.users().await.unwrap().len(), 2);

        // A member promoted is an admin at once, and then the owner can go.
        let ann = users
            .set_role(&id("ann"), Role::Admin, at(2))
            .await
            .unwrap();
        assert!(ann.is_admin());
        assert!(is_admin(&id("ann")));
        users.set_role(&owner, Role::Member, at(3)).await.unwrap();
        assert!(!is_admin(&owner));
        // Now ann is the last: demoting, disabling, and removing her are all refused.
        for err in [
            users
                .set_role(&id("ann"), Role::Member, at(4))
                .await
                .unwrap_err(),
            users
                .set_state(&id("ann"), UserState::Disabled, at(4))
                .await
                .unwrap_err(),
            users.remove(&id("ann")).await.unwrap_err(),
        ] {
            assert!(matches!(err, UserError::LastAdmin), "{err}");
        }
        assert!(is_admin(&id("ann")));

        // A disabled admin has no privilege and doesn't count as one to keep.
        users.set_role(&owner, Role::Admin, at(5)).await.unwrap();
        users
            .set_state(&owner, UserState::Disabled, at(6))
            .await
            .unwrap();
        assert!(!is_admin(&owner));
        let err = users.remove(&id("ann")).await.unwrap_err();
        assert!(matches!(err, UserError::LastAdmin), "{err}");
        users
            .set_state(&owner, UserState::Active, at(7))
            .await
            .unwrap();
        assert!(is_admin(&owner));

        // Removing a user takes them from the list and the database, and they are no admin.
        let removed = users.remove(&id("ann")).await.unwrap();
        assert_eq!(removed.id, id("ann"));
        assert!(list.get().user(&id("ann")).is_none());
        assert!(!is_admin(&id("ann")));
        assert_eq!(home.store.users().await.unwrap().len(), 1);
        let err = users.remove(&id("ann")).await.unwrap_err();
        assert!(matches!(err, UserError::NoSuchUser(_)), "{err}");
        let err = users
            .set_role(&id("ann"), Role::Admin, at(8))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::NoSuchUser(_)), "{err}");
    }

    #[tokio::test]
    async fn changes_made_at_once_can_not_remove_every_admin() {
        let home = Home::new().await;
        let (users, list) = home.open("").await.unwrap();
        let users = Arc::new(users);
        for name in ["ann", "bob"] {
            let admin = NewUser {
                role: Role::Admin,
                ..member(name)
            };
            users.create(admin, vec![], at(0)).await.unwrap();
        }
        let tasks: Vec<_> = ["ann", "bob"]
            .into_iter()
            .enumerate()
            .map(|(i, name)| {
                let users = users.clone();
                tokio::spawn(async move {
                    if i == 0 {
                        users.set_role(&id(name), Role::Member, at(1)).await
                    } else {
                        users.set_state(&id(name), UserState::Disabled, at(1)).await
                    }
                })
            })
            .collect();
        let mut refused = 0;
        for task in tasks {
            match task.await.unwrap() {
                Ok(_) => {}
                Err(UserError::LastAdmin) => refused += 1,
                Err(other) => panic!("{other}"),
            }
        }
        assert_eq!(refused, 1);
        let directory = list.get();
        assert_eq!(
            directory
                .users()
                .iter()
                .filter(|user| user.is_admin())
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn login_with_no_users_creates_the_first_admin_once() {
        let home = Home::new().await;
        let (users, list) = home.open("").await.unwrap();
        let first = users.login_user(None, at(0)).await.unwrap();
        assert_eq!(first, identity::owner());
        let directory = list.get();
        let admin = directory.user(&first).unwrap();
        assert_eq!(admin.role, Role::Admin);
        assert!(admin.is_active());
        assert_eq!(admin.username, "admin");
        assert_eq!(
            admin.default_identity.as_ref().unwrap().as_str(),
            "web:owner"
        );
        // The web login resolves to them, as messages from it will.
        assert_eq!(
            directory.resolve("web", &native("owner")).unwrap().id,
            first
        );
        // It's stored.
        assert_eq!(home.store.users().await.unwrap().len(), 1);
        // With one user, no name is needed, and naming them changes nothing.
        assert_eq!(users.login_user(None, at(1)).await.unwrap(), first);
        assert_eq!(users.login_user(Some(&first), at(2)).await.unwrap(), first);
        assert_eq!(list.get().user(&first).unwrap().identities.len(), 1);
        assert_eq!(home.store.users().await.unwrap().len(), 1);
        // With several, one must be named, and the message lists them.
        users.create(member("ann"), vec![], at(3)).await.unwrap();
        let err = users.login_user(None, at(4)).await.unwrap_err();
        let LoginUserError::NeedUser(message) = &err else {
            panic!("{err}");
        };
        assert!(
            message.contains("owner (admin)")
                && message.contains("ann (ann)")
                && message.contains("--user"),
            "{message}"
        );
        assert_eq!(list.get().user(&id("ann")).unwrap().identities.len(), 0);
    }

    #[tokio::test]
    async fn login_for_a_named_user_is_that_active_user_and_changes_nothing() {
        let home = Home::new().await;
        let (users, list) = home
            .open("[owner]\nidentities = [\"whatsapp:+15551234567\"]\n")
            .await
            .unwrap();
        assert_eq!(
            users
                .login_user(Some(&identity::owner()), at(0))
                .await
                .unwrap(),
            identity::owner()
        );
        // Making the link is what gives the user an identity to sign in as.
        assert!(!resolves(&list, "web", "owner"));

        let err = users
            .login_user(Some(&id("nobody")), at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, LoginUserError::NoSuchUser(_)), "{err}");
        users.create(member("ann"), vec![], at(2)).await.unwrap();
        users
            .set_state(&id("ann"), UserState::Disabled, at(3))
            .await
            .unwrap();
        let err = users.login_user(Some(&id("ann")), at(4)).await.unwrap_err();
        assert!(matches!(err, LoginUserError::Disabled(_)), "{err}");
    }

    #[tokio::test]
    async fn an_invite_gives_the_user_the_web_identity_a_link_signs_in_as() {
        let home = Home::new().await;
        let (users, list) = home
            .open("[owner]\nidentities = [\"whatsapp:+15551234567\"]\n")
            .await
            .unwrap();
        // The imported owner has no `web:` identity yet.
        assert!(!resolves(&list, "web", "owner"));
        users
            .create_invite(&identity::owner(), InviteKind::Recovery, None, at(0))
            .await
            .unwrap();
        let directory = list.get();
        let owner = directory.user(&identity::owner()).unwrap();
        assert_eq!(
            directory.resolve("web", &native("owner")).unwrap().id,
            owner.id
        );
        assert_eq!(owner.identities[1].verification, Verification::Admin);
        // The default identity stays where jobs go, and a second link adds nothing.
        assert_eq!(
            owner.default_identity.as_ref().unwrap().as_str(),
            "whatsapp:+15551234567"
        );
        users
            .create_invite(&identity::owner(), InviteKind::Recovery, None, at(1))
            .await
            .unwrap();
        assert_eq!(
            list.get()
                .user(&identity::owner())
                .unwrap()
                .identities
                .len(),
            2
        );

        let err = users
            .create_invite(&id("nobody"), InviteKind::Invite, None, at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, LoginUserError::NoSuchUser(_)), "{err}");
        users.create(member("ann"), vec![], at(2)).await.unwrap();
        users
            .set_state(&id("ann"), UserState::Disabled, at(3))
            .await
            .unwrap();
        let err = users
            .create_invite(&id("ann"), InviteKind::Invite, None, at(4))
            .await
            .unwrap_err();
        assert!(matches!(err, LoginUserError::Disabled(_)), "{err}");
        // `web:bob` belongs to ann, so bob can't be signed in as `bob`.
        users
            .add_identity(&id("ann"), "web", "bob", Verification::Admin, at(5))
            .await
            .unwrap();
        users.create(member("bob"), vec![], at(6)).await.unwrap();
        let err = users
            .create_invite(&id("bob"), InviteKind::Invite, None, at(7))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, LoginUserError::WebIdentityTaken { owner, .. } if owner == &id("ann")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn an_invite_works_once_for_72_hours_and_only_its_hash_is_stored() {
        let home = Home::new().await;
        let (users, _) = home.open("").await.unwrap();
        let ann = users.create(member("ann"), vec![], at(0)).await.unwrap();
        let root = users.create(member("root"), vec![], at(0)).await.unwrap();
        let invite = users
            .create_invite(&ann.id, InviteKind::Invite, Some(&root.id), at(10))
            .await
            .unwrap();
        assert!(is_invite_token(&invite.token), "{}", invite.token);
        assert_eq!(invite.expires_at, at(10 + 72 * 60 * 60));
        let other = users
            .create_invite(&ann.id, InviteKind::Recovery, None, at(10))
            .await
            .unwrap();
        assert_ne!(invite.token, other.token);

        let stored = home
            .store
            .user_invite(&invite.token, at(11))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.user, ann.id);
        assert_eq!(stored.kind, InviteKind::Invite);
        assert_eq!(stored.created_by, Some(root.id));
        assert_eq!(stored.expires_at, invite.expires_at);
        let kind = home
            .store
            .user_invite(&other.token, at(11))
            .await
            .unwrap()
            .unwrap()
            .kind;
        assert_eq!(kind, InviteKind::Recovery);
        assert!(
            home.store
                .user_invite(&invite.token, invite.expires_at)
                .await
                .unwrap()
                .is_none(),
            "expired"
        );
        assert!(!is_invite_token("ABC") && !is_invite_token(&"A".repeat(64)));
        assert!(!is_invite_token(&"a".repeat(63)));
    }

    #[tokio::test]
    async fn an_admin_sets_a_users_proxy_identity_without_a_link_code() {
        let home = Home::new().await;
        let (users, list) = home.open("").await.unwrap();
        let ann = NewUser {
            default_identity: Some("web:ann".to_owned()),
            ..member("ann")
        };
        let web = NewIdentity {
            namespace: "web".to_owned(),
            native_id: "ann".to_owned(),
            verification: Verification::Admin,
        };
        users.create(ann, vec![web], at(0)).await.unwrap();
        users.create(member("bob"), vec![], at(0)).await.unwrap();

        let set = users
            .set_proxy_identity(&id("ann"), Some("ann@example.com"), at(1))
            .await
            .unwrap();
        let texts: Vec<_> = set.identities.iter().map(|i| i.identity.as_str()).collect();
        assert_eq!(texts, ["web:ann", "web:ann@example.com"]);
        assert_eq!(set.identities[1].verification, Verification::Admin);
        // The router resolves the header value to ann at once.
        assert_eq!(
            list.get()
                .resolve("web", &native("ann@example.com"))
                .unwrap()
                .id,
            id("ann")
        );
        // A second value replaces the first, and the login identity stays.
        users
            .set_proxy_identity(&id("ann"), Some("ann@corp.example"), at(2))
            .await
            .unwrap();
        assert!(!resolves(&list, "web", "ann@example.com"));
        assert!(resolves(&list, "web", "ann@corp.example"));
        assert!(resolves(&list, "web", "ann"));

        // Another user's identity is refused and ann keeps theirs.
        users
            .set_proxy_identity(&id("bob"), Some("bob@example.com"), at(3))
            .await
            .unwrap();
        let err = users
            .set_proxy_identity(&id("ann"), Some("bob@example.com"), at(4))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::IdentityTaken(_)), "{err}");
        assert!(resolves(&list, "web", "ann@corp.example"));
        // The login identity isn't a proxy identity, a blank or spaced value isn't an identity,
        // and the user must exist.
        for bad in ["ann", "", "a b"] {
            let err = users
                .set_proxy_identity(&id("ann"), Some(bad), at(5))
                .await
                .unwrap_err();
            assert!(matches!(err, UserError::Invalid(_)), "{bad:?}: {err}");
        }
        let err = users
            .set_proxy_identity(&id("nobody"), Some("x"), at(5))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::NoSuchUser(_)), "{err}");
        // None clears it.
        users
            .set_proxy_identity(&id("ann"), None, at(6))
            .await
            .unwrap();
        assert!(!resolves(&list, "web", "ann@corp.example"));
        assert!(resolves(&list, "web", "ann"));
    }

    #[tokio::test]
    async fn the_default_identity_is_set_to_one_the_user_has() {
        let home = Home::new().await;
        let (users, list) = home.open("").await.unwrap();
        users.create(member("ann"), vec![], at(0)).await.unwrap();
        users
            .add_identity(&id("ann"), "whatsapp", "+1", Verification::Link, at(1))
            .await
            .unwrap();
        users
            .add_identity(&id("ann"), "discord", "7", Verification::Link, at(2))
            .await
            .unwrap();
        let discord = Identity::new("discord:7").unwrap();
        users
            .set_default_identity(&id("ann"), &discord, at(3))
            .await
            .unwrap();
        let directory = list.get();
        let ann = directory.user(&id("ann")).unwrap();
        assert_eq!(ann.default_identity.as_ref(), Some(&discord));
        let err = users
            .set_default_identity(&id("ann"), &Identity::new("discord:8").unwrap(), at(4))
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::Invalid(_)), "{err}");
    }

    #[tokio::test]
    async fn redeeming_a_code_replaces_the_snapshot_and_checks_the_sender() {
        let home = Home::new().await;
        let (users, list) = home.open("").await.unwrap();
        users.create(member("ann"), vec![], at(0)).await.unwrap();
        home.store
            .user_link_code_create(&id("ann"), "whatsapp", "ABCD2345", [1; 16], at(0), at(600))
            .await
            .unwrap();
        // A wrong code links nothing; the right one links and the snapshot has it.
        assert!(
            users
                .redeem_link_code("whatsapp", "+15551234567", "WRONG222", at(1), 20)
                .await
                .unwrap()
                .is_none()
        );
        assert!(!resolves(&list, "whatsapp", "+15551234567"));
        let ann = users
            .redeem_link_code("whatsapp", "+15551234567", "ABCD2345", at(2), 20)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ann.id, id("ann"));
        assert!(resolves(&list, "whatsapp", "+15551234567"));
        // A sender that isn't an identity is invalid, and a taken one is refused.
        let err = users
            .redeem_link_code("whatsapp", "a b", "ABCD2345", at(3), 20)
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::Invalid(_)), "{err}");
        home.store
            .user_link_code_create(&id("ann"), "whatsapp", "EFGH6789", [1; 16], at(3), at(600))
            .await
            .unwrap();
        let err = users
            .redeem_link_code("whatsapp", "+15551234567", "EFGH6789", at(4), 20)
            .await
            .unwrap_err();
        assert!(matches!(err, UserError::IdentityTaken(_)), "{err}");
    }
}
