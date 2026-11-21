//! `users` and `user_identities`: who uses this Doozers, and which channel identities are theirs
//! (PLAN 1.1, 3.2), and the marker that says `[owner]` of `doozers.toml` has been imported.
//!
//! The store keeps the rows as given and checks only what the schema can: ids, usernames, and
//! identities are unique, and a default identity is one of the user's identities. What a username
//! may look like, which time zones exist, and who may do what is the core's business. One rule
//! about roles is the store's, because it must hold when changes race: the last active admin can't
//! be disabled, demoted, or deleted ([`Error::LastAdmin`], checked in the transaction of the
//! change).
//!
//! A user's id never changes. The store takes the id the caller gives and forgets it when
//! [`Store::user_delete`] deletes the user, so it can't tell a reused id from a new one: the
//! caller (the core) must never give a deleted user's id to another user, which random ids
//! guarantee, and must delete everything keyed by the id when it deletes a user.
//!
//! An identity is stored and compared in its [canonical form](canonical_identity), so one sender
//! can't belong to two users by differing in the case of an email domain.

use doozers_types::{Timestamp, UserId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::time::to_millis;
use crate::{Error, Result, Store, get_time, parse_id, query_all, query_opt};

/// The form identities are stored and compared in: the joined `<namespace>:<native id>`, with the
/// domain of an `email` identity (first segment `email`, domain after the last `@`) lowercased.
/// The comparison of other identities is exact. The local part of an email address stays
/// case-sensitive, as RFC 5321 lets a mail server treat it so.
pub fn canonical_identity(identity: &str) -> String {
    if identity.split(':').next() == Some("email")
        && let Some((local, domain)) = identity.rsplit_once('@')
    {
        return format!("{local}@{}", domain.to_ascii_lowercase());
    }
    identity.to_owned()
}

/// What a user may do. All admins are equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Manages the instance.
    Admin,
    /// An ordinary user.
    Member,
}

impl Role {
    /// The stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Member => "member",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "admin" => Ok(Role::Admin),
            "member" => Ok(Role::Member),
            other => Err(Error::corrupt(format!("user role {other:?}"))),
        }
    }
}

/// Whether a user can use Doozers now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserState {
    /// The user can sign in and their messages are handled.
    Active,
    /// The user can't sign in and their messages are dropped. Everything they own stays.
    Disabled,
}

impl UserState {
    /// The stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            UserState::Active => "active",
            UserState::Disabled => "disabled",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "active" => Ok(UserState::Active),
            "disabled" => Ok(UserState::Disabled),
            other => Err(Error::corrupt(format!("user state {other:?}"))),
        }
    }
}

/// How an identity was established as the user's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// The user sent a link code from the identity.
    Link,
    /// An admin or the operator set it: a proxy login identity, or the `web:<id>` identity a login
    /// link signs in as.
    Admin,
    /// It was in `[owner]` of `doozers.toml` when that was imported.
    Imported,
}

impl Verification {
    /// The stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Verification::Link => "link",
            Verification::Admin => "admin",
            Verification::Imported => "imported",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "link" => Ok(Verification::Link),
            "admin" => Ok(Verification::Admin),
            "imported" => Ok(Verification::Imported),
            other => Err(Error::corrupt(format!("identity verification {other:?}"))),
        }
    }
}

/// A user to create. It starts [`Active`](UserState::Active).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUser {
    /// The user's id: opaque, and never changed.
    pub id: UserId,
    /// The username, unique without regard to ASCII case.
    pub username: String,
    /// The name people see.
    pub display_name: String,
    /// What the user may do.
    pub role: Role,
    /// The user's IANA time zone name, if set.
    pub timezone: Option<String>,
    /// The `<namespace>:<native id>` of the identity jobs and proactive messages go to, which must
    /// be one of the identities created with the user.
    pub default_identity: Option<String>,
}

/// An identity to add to a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIdentity {
    /// The channel's identity namespace, e.g. `whatsapp` or `irc:libera.chat`.
    pub namespace: String,
    /// The sender's id in that namespace.
    pub native_id: String,
    /// How it was established.
    pub verification: Verification,
}

/// A stored channel identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredIdentity {
    /// The channel's identity namespace.
    pub namespace: String,
    /// The sender's id in that namespace.
    pub native_id: String,
    /// When it was established as the user's.
    pub verified_at: Timestamp,
    /// How it was established.
    pub verification: Verification,
}

impl StoredIdentity {
    /// `<namespace>:<native id>`.
    pub fn text(&self) -> String {
        format!("{}:{}", self.namespace, self.native_id)
    }
}

/// A stored user with their identities, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUser {
    /// The user's id.
    pub id: UserId,
    /// The username.
    pub username: String,
    /// The name people see.
    pub display_name: String,
    /// What the user may do.
    pub role: Role,
    /// Whether the user can use Doozers now.
    pub state: UserState,
    /// The user's IANA time zone name, if set.
    pub timezone: Option<String>,
    /// The `<namespace>:<native id>` of the identity jobs and proactive messages go to.
    pub default_identity: Option<String>,
    /// When the user was created.
    pub created_at: Timestamp,
    /// When the user last changed.
    pub updated_at: Timestamp,
    /// The user's identities, oldest first.
    pub identities: Vec<StoredIdentity>,
}

/// What [`Store::users_import`] imports: the owner of `doozers.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedUser {
    /// The user to create.
    pub user: NewUser,
    /// Their identities as `<namespace>:<native id>`, stored as [`Verification::Imported`]. The
    /// import can't tell where a namespace ends (`irc:libera.chat` contains a `:`), so it records
    /// the part before the first `:` as the namespace.
    pub identities: Vec<String>,
}

const USER_COLUMNS: &str = "id, username, display_name, role, state, timezone, default_identity, \
                            created_at, updated_at";

fn user_from_row(row: &Row<'_>) -> Result<StoredUser> {
    Ok(StoredUser {
        id: parse_id(row.get(0)?, UserId::new)?,
        username: row.get(1)?,
        display_name: row.get(2)?,
        role: Role::parse(&row.get::<_, String>(3)?)?,
        state: UserState::parse(&row.get::<_, String>(4)?)?,
        timezone: row.get(5)?,
        default_identity: row.get(6)?,
        created_at: get_time(row, 7)?,
        updated_at: get_time(row, 8)?,
        identities: Vec::new(),
    })
}

fn identities_of(conn: &Connection, user: &str) -> Result<Vec<StoredIdentity>> {
    let mut stmt = conn.prepare_cached(
        "SELECT namespace, native_id, verified_at, verification FROM user_identities
         WHERE user_id = ?1 ORDER BY verified_at, rowid",
    )?;
    query_all(&mut stmt, [user], |row| {
        Ok(StoredIdentity {
            namespace: row.get(0)?,
            native_id: row.get(1)?,
            verified_at: get_time(row, 2)?,
            verification: Verification::parse(&row.get::<_, String>(3)?)?,
        })
    })
}

fn list(conn: &Connection) -> Result<Vec<StoredUser>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {USER_COLUMNS} FROM users ORDER BY created_at, rowid"
    ))?;
    let mut users = query_all(&mut stmt, [], user_from_row)?;
    for user in &mut users {
        user.identities = identities_of(conn, user.id.as_str())?;
    }
    Ok(users)
}

pub(crate) fn find(conn: &Connection, id: &UserId) -> Result<Option<StoredUser>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1"))?;
    let Some(mut user) = query_opt(&mut stmt, [id.as_str()], user_from_row)? else {
        return Ok(None);
    };
    user.identities = identities_of(conn, id.as_str())?;
    Ok(Some(user))
}

fn count(conn: &Connection, table: &str) -> Result<i64> {
    Ok(conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
}

fn insert_identity(
    conn: &Connection,
    user: &UserId,
    identity: &NewIdentity,
    now: Timestamp,
) -> Result<()> {
    let joined = canonical_identity(&format!("{}:{}", identity.namespace, identity.native_id));
    let native_id = joined
        .strip_prefix(&identity.namespace)
        .and_then(|rest| rest.strip_prefix(':'))
        .ok_or_else(|| Error::invalid(format!("the identity {joined} changed its namespace")))?;
    let taken: bool = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM user_identities WHERE identity = ?1)")?
        .query_row([&joined], |row| row.get(0))?;
    if taken {
        return Err(Error::IdentityTaken { identity: joined });
    }
    conn.prepare_cached(
        "INSERT INTO user_identities (namespace, native_id, user_id, verified_at, verification)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![
        identity.namespace,
        native_id,
        user.as_str(),
        to_millis(now),
        identity.verification.as_str()
    ])?;
    Ok(())
}

/// Inserts `user` and `identities` (a default identity must be among them).
fn insert_user(
    conn: &Connection,
    user: &NewUser,
    identities: &[NewIdentity],
    now: Timestamp,
) -> Result<()> {
    let default = user.default_identity.as_deref().map(canonical_identity);
    if let Some(default) = &default
        && !identities
            .iter()
            .any(|i| canonical_identity(&format!("{}:{}", i.namespace, i.native_id)) == *default)
    {
        return Err(Error::invalid(format!(
            "the default identity {default} is not one of the user's identities"
        )));
    }
    let id_taken: bool = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM users WHERE id = ?1)")?
        .query_row([user.id.as_str()], |row| row.get(0))?;
    if id_taken {
        return Err(Error::UserIdTaken);
    }
    let name_taken: bool = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM users WHERE username = ?1)")?
        .query_row([&user.username], |row| row.get(0))?;
    if name_taken {
        return Err(Error::UsernameTaken);
    }
    conn.prepare_cached(
        "INSERT INTO users (id, username, display_name, role, state, timezone, default_identity,
                            created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?7)",
    )?
    .execute(params![
        user.id.as_str(),
        user.username,
        user.display_name,
        user.role.as_str(),
        user.timezone,
        default,
        to_millis(now)
    ])?;
    for identity in identities {
        insert_identity(conn, &user.id, identity, now)?;
    }
    Ok(())
}

/// Fails with [`Error::LastAdmin`] if `id` is an active admin and no other user is: the change
/// the caller is about to make (removing, disabling, or demoting `id`) would leave none. Called
/// inside the write transaction of that change, so another change can't slip between the check
/// and the write.
fn keep_an_admin(conn: &Connection, id: &UserId) -> Result<()> {
    let active_admin = "role = 'admin' AND state = 'active'";
    let is_one: bool = conn
        .prepare_cached(&format!(
            "SELECT EXISTS (SELECT 1 FROM users WHERE id = ?1 AND {active_admin})"
        ))?
        .query_row([id.as_str()], |row| row.get(0))?;
    if !is_one {
        return Ok(());
    }
    let others: bool = conn
        .prepare_cached(&format!(
            "SELECT EXISTS (SELECT 1 FROM users WHERE id <> ?1 AND {active_admin})"
        ))?
        .query_row([id.as_str()], |row| row.get(0))?;
    if others {
        Ok(())
    } else {
        Err(Error::LastAdmin)
    }
}

fn touch(conn: &Connection, user: &UserId, now: Timestamp) -> Result<()> {
    conn.prepare_cached("UPDATE users SET updated_at = ?2 WHERE id = ?1")?
        .execute(params![user.as_str(), to_millis(now)])?;
    Ok(())
}

/// Adds `identity` to user `id` (inside a write transaction). A user with no default identity
/// takes it as their default.
pub(crate) fn add_identity(
    conn: &Connection,
    id: &UserId,
    identity: &NewIdentity,
    now: Timestamp,
) -> Result<()> {
    let user = find(conn, id)?.ok_or(Error::NoSuchUser)?;
    insert_identity(conn, id, identity, now)?;
    if user.default_identity.is_none() {
        conn.prepare_cached("UPDATE users SET default_identity = ?2 WHERE id = ?1")?
            .execute(params![
                id.as_str(),
                canonical_identity(&format!("{}:{}", identity.namespace, identity.native_id))
            ])?;
    }
    touch(conn, id, now)
}

/// Removes `identity`, already in its canonical form, from user `id` (inside a write
/// transaction). If it was the default identity, the oldest one left becomes the default.
fn remove_identity(conn: &Connection, id: &UserId, identity: &str, now: Timestamp) -> Result<()> {
    let user = find(conn, id)?.ok_or(Error::NoSuchUser)?;
    let removed = conn
        .prepare_cached("DELETE FROM user_identities WHERE user_id = ?1 AND identity = ?2")?
        .execute(params![id.as_str(), identity])?;
    if removed == 0 {
        return Err(Error::not_found(format!(
            "identity {identity} of user {id}"
        )));
    }
    if user.default_identity.as_deref() == Some(identity) {
        let next: Option<String> = conn
            .prepare_cached(
                "SELECT identity FROM user_identities WHERE user_id = ?1
                 ORDER BY verified_at, rowid LIMIT 1",
            )?
            .query_row([id.as_str()], |row| row.get(0))
            .optional()?;
        conn.prepare_cached("UPDATE users SET default_identity = ?2 WHERE id = ?1")?
            .execute(params![id.as_str(), next])?;
    }
    touch(conn, id, now)
}

impl Store {
    /// Every user with their identities, oldest first.
    pub async fn users(&self) -> Result<Vec<StoredUser>> {
        self.read(list).await
    }

    /// User `id`, if there is one.
    pub async fn user(&self, id: &UserId) -> Result<Option<StoredUser>> {
        let id = id.clone();
        self.read(move |conn| find(conn, &id)).await
    }

    /// Creates an active user with `identities`, all verified at `now`, and returns it.
    ///
    /// # Errors
    ///
    /// [`Error::UserIdTaken`], [`Error::UsernameTaken`], or [`Error::IdentityTaken`], with nothing
    /// stored; [`Error::InvalidRequest`] if the default identity isn't one of `identities`.
    pub async fn user_create(
        &self,
        user: NewUser,
        identities: Vec<NewIdentity>,
        now: Timestamp,
    ) -> Result<StoredUser> {
        self.write(move |conn| {
            insert_user(conn, &user, &identities, now)?;
            find(conn, &user.id)?.ok_or_else(|| Error::corrupt("the user just inserted is missing"))
        })
        .await
    }

    /// Sets the state of user `id` and returns the user.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`]; [`Error::LastAdmin`] if the user is the last active admin and
    /// `state` is [`Disabled`](UserState::Disabled).
    pub async fn user_set_state(
        &self,
        id: &UserId,
        state: UserState,
        now: Timestamp,
    ) -> Result<StoredUser> {
        let id = id.clone();
        self.write(move |conn| {
            if state == UserState::Disabled {
                keep_an_admin(conn, &id)?;
            }
            let changed = conn
                .prepare_cached("UPDATE users SET state = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![id.as_str(), state.as_str(), to_millis(now)])?;
            if changed == 0 {
                return Err(Error::NoSuchUser);
            }
            find(conn, &id)?.ok_or_else(|| Error::corrupt("the user just updated is missing"))
        })
        .await
    }

    /// Sets the role of user `id` and returns the user.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`]; [`Error::LastAdmin`] if the user is the last active admin and
    /// `role` is [`Member`](Role::Member).
    pub async fn user_set_role(
        &self,
        id: &UserId,
        role: Role,
        now: Timestamp,
    ) -> Result<StoredUser> {
        let id = id.clone();
        self.write(move |conn| {
            if role == Role::Member {
                keep_an_admin(conn, &id)?;
            }
            let changed = conn
                .prepare_cached("UPDATE users SET role = ?2, updated_at = ?3 WHERE id = ?1")?
                .execute(params![id.as_str(), role.as_str(), to_millis(now)])?;
            if changed == 0 {
                return Err(Error::NoSuchUser);
            }
            find(conn, &id)?.ok_or_else(|| Error::corrupt("the user just updated is missing"))
        })
        .await
    }

    /// Sets the display name and time zone of user `id`, which a user edits on their own account
    /// page, and returns the user. `timezone` is an IANA name, or `None` to clear it; the store
    /// doesn't know which names exist.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`]; [`Error::InvalidRequest`] if `display_name` is empty.
    pub async fn user_profile_set(
        &self,
        id: &UserId,
        display_name: &str,
        timezone: Option<&str>,
        now: Timestamp,
    ) -> Result<StoredUser> {
        if display_name.is_empty() {
            return Err(Error::invalid("a display name can't be empty"));
        }
        let (id, display_name, timezone) = (
            id.clone(),
            display_name.to_owned(),
            timezone.map(str::to_owned),
        );
        self.write(move |conn| {
            let changed = conn
                .prepare_cached(
                    "UPDATE users SET display_name = ?2, timezone = ?3, updated_at = ?4
                     WHERE id = ?1",
                )?
                .execute(params![id.as_str(), display_name, timezone, to_millis(now)])?;
            if changed == 0 {
                return Err(Error::NoSuchUser);
            }
            find(conn, &id)?.ok_or_else(|| Error::corrupt("the user just updated is missing"))
        })
        .await
    }

    /// Deletes the account of user `id` and, by cascade, its identities, invite links, link codes,
    /// password, passkeys, and OpenID Connect connections, and returns the user as it was. Links
    /// the user made stay, with no creator. It deletes nothing else: what the user owns
    /// (web login sessions, conversations, memories, secrets, workspaces) stays, and is the caller's
    /// to delete first ([`user_data_forget`](Store::user_data_forget) does the database's part).
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`]; [`Error::LastAdmin`] if the user is the last active admin.
    pub async fn user_delete(&self, id: &UserId) -> Result<StoredUser> {
        let id = id.clone();
        self.write(move |conn| {
            let user = find(conn, &id)?.ok_or(Error::NoSuchUser)?;
            keep_an_admin(conn, &id)?;
            conn.prepare_cached("DELETE FROM users WHERE id = ?1")?
                .execute([id.as_str()])?;
            Ok(user)
        })
        .await
    }

    /// Adds `identity` to user `id`, verified at `now`, and returns the user. A user with no
    /// default identity takes it as their default.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`], or [`Error::IdentityTaken`] if any user has it.
    pub async fn user_identity_add(
        &self,
        id: &UserId,
        identity: NewIdentity,
        now: Timestamp,
    ) -> Result<StoredUser> {
        let id = id.clone();
        self.write(move |conn| {
            add_identity(conn, &id, &identity, now)?;
            find(conn, &id)?.ok_or_else(|| Error::corrupt("the user just updated is missing"))
        })
        .await
    }

    /// Removes `identity` (`<namespace>:<native id>`) from user `id` and returns the user. If it
    /// was the default identity, the oldest identity left becomes the default, or there is none.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`], or [`Error::NotFound`] if the user doesn't have the identity.
    pub async fn user_identity_remove(
        &self,
        id: &UserId,
        identity: &str,
        now: Timestamp,
    ) -> Result<StoredUser> {
        let id = id.clone();
        let identity = canonical_identity(identity);
        self.write(move |conn| {
            remove_identity(conn, &id, &identity, now)?;
            find(conn, &id)?.ok_or_else(|| Error::corrupt("the user just updated is missing"))
        })
        .await
    }

    /// Makes `identity` (`<namespace>:<native id>`) the default identity of user `id`, where jobs
    /// and proactive messages go, and returns the user.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`], or [`Error::NotFound`] if the user doesn't have the identity.
    pub async fn user_default_identity_set(
        &self,
        id: &UserId,
        identity: &str,
        now: Timestamp,
    ) -> Result<StoredUser> {
        let id = id.clone();
        let identity = canonical_identity(identity);
        self.write(move |conn| {
            let user = find(conn, &id)?.ok_or(Error::NoSuchUser)?;
            if !user
                .identities
                .iter()
                .any(|i| canonical_identity(&i.text()) == identity)
            {
                return Err(Error::not_found(format!(
                    "identity {identity} of user {id}"
                )));
            }
            conn.prepare_cached("UPDATE users SET default_identity = ?2 WHERE id = ?1")?
                .execute(params![id.as_str(), identity])?;
            touch(conn, &id, now)?;
            find(conn, &id)?.ok_or_else(|| Error::corrupt("the user just updated is missing"))
        })
        .await
    }

    /// Sets the identity user `id` is logged in as by a trusted proxy: `native_id` in `namespace`
    /// (`web`), verified as [`Verification::Admin`] at `now`, or none if `native_id` is `None`.
    /// Every other identity of the user in `namespace` goes, except `login`, the one a link
    /// sign-in acts as (`web:<id>`), which is not a proxy identity. Everything happens in one
    /// transaction, so a user is never left without the proxy identity they had because the new one
    /// was refused. If the default identity goes, the oldest one left becomes the default.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`], or [`Error::IdentityTaken`] if another user has the new identity;
    /// nothing changes then.
    pub async fn user_proxy_identity_set(
        &self,
        id: &UserId,
        namespace: &str,
        login: &str,
        native_id: Option<&str>,
        now: Timestamp,
    ) -> Result<StoredUser> {
        let id = id.clone();
        let namespace = namespace.to_owned();
        let login = login.to_owned();
        let native_id = native_id.map(str::to_owned);
        self.write(move |conn| {
            let user = find(conn, &id)?.ok_or(Error::NoSuchUser)?;
            let wanted = native_id
                .as_deref()
                .map(|native| canonical_identity(&format!("{namespace}:{native}")));
            let is_wanted = |identity: &StoredIdentity| {
                wanted.as_deref() == Some(canonical_identity(&identity.text()).as_str())
            };
            if let Some(native_id) = &native_id {
                match user.identities.iter().find(|i| is_wanted(i)) {
                    Some(have) if have.verification != Verification::Admin => {
                        conn.prepare_cached(
                            "UPDATE user_identities SET verification = 'admin', verified_at = ?3
                             WHERE user_id = ?1 AND identity = ?2",
                        )?
                        .execute(params![
                            id.as_str(),
                            canonical_identity(&have.text()),
                            to_millis(now)
                        ])?;
                    }
                    Some(_) => {}
                    None => insert_identity(
                        conn,
                        &id,
                        &NewIdentity {
                            namespace: namespace.clone(),
                            native_id: native_id.clone(),
                            verification: Verification::Admin,
                        },
                        now,
                    )?,
                }
            }
            for stale in user
                .identities
                .iter()
                .filter(|i| i.namespace == namespace && i.native_id != login && !is_wanted(i))
            {
                remove_identity(conn, &id, &canonical_identity(&stale.text()), now)?;
            }
            // A user who had no default identity takes the new one, as with any added identity.
            let user = find(conn, &id)?.ok_or_else(|| Error::corrupt("the user is missing"))?;
            if user.default_identity.is_none()
                && let Some(wanted) = &wanted
            {
                conn.prepare_cached("UPDATE users SET default_identity = ?2 WHERE id = ?1")?
                    .execute(params![id.as_str(), wanted])?;
            }
            touch(conn, &id, now)?;
            find(conn, &id)?.ok_or_else(|| Error::corrupt("the user just updated is missing"))
        })
        .await
    }

    /// Whether the import of `[owner]` of `doozers.toml` has run
    /// ([`users_import`](Self::users_import)).
    pub async fn users_imported(&self) -> Result<bool> {
        self.read(|conn| Ok(count(conn, "users_imported")? > 0))
            .await
    }

    /// Imports `owner`, the `[owner]` of `doozers.toml` (`None`: it has none), once: if the import
    /// has never run, it creates the user unless users exist already (then they stay) and records
    /// that it ran. Returns whether it created the user, or `None` if the import had run before,
    /// in which case nothing changes, whatever `owner` holds and even if every user has since
    /// been removed. Nothing is stored if the import fails.
    pub async fn users_import(
        &self,
        owner: Option<ImportedUser>,
        now: Timestamp,
    ) -> Result<Option<bool>> {
        self.write(move |conn| {
            if count(conn, "users_imported")? > 0 {
                return Ok(None);
            }
            conn.execute(
                "INSERT INTO users_imported (id, at) VALUES (1, ?1)",
                [to_millis(now)],
            )?;
            let Some(owner) = owner else {
                return Ok(Some(false));
            };
            if count(conn, "users")? > 0 {
                return Ok(Some(false));
            }
            let identities: Vec<NewIdentity> = owner
                .identities
                .into_iter()
                .map(|identity| {
                    let (namespace, native_id) = identity.split_once(':').ok_or_else(|| {
                        Error::invalid(format!("identity {identity:?} has no namespace"))
                    })?;
                    Ok(NewIdentity {
                        namespace: namespace.to_owned(),
                        native_id: native_id.to_owned(),
                        verification: Verification::Imported,
                    })
                })
                .collect::<Result<_>>()?;
            insert_user(conn, &owner.user, &identities, now)?;
            Ok(Some(true))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SCHEMA_VERSION;
    use crate::migrate::{MIGRATIONS, migrate};
    use crate::sessions::tests::{at, store};

    fn user(id: &str, username: &str, role: Role) -> NewUser {
        NewUser {
            id: UserId::new(id).unwrap(),
            username: username.to_owned(),
            display_name: username.to_uppercase(),
            role,
            timezone: None,
            default_identity: None,
        }
    }

    fn identity(namespace: &str, native_id: &str) -> NewIdentity {
        NewIdentity {
            namespace: namespace.to_owned(),
            native_id: native_id.to_owned(),
            verification: Verification::Link,
        }
    }

    fn id(text: &str) -> UserId {
        UserId::new(text).unwrap()
    }

    #[tokio::test]
    async fn a_created_user_comes_back_with_every_field_and_identity() {
        let (_dir, store) = store().await;
        assert!(store.users().await.unwrap().is_empty());
        let new = NewUser {
            timezone: Some("America/Los_Angeles".into()),
            default_identity: Some("irc:libera.chat:account:ann".into()),
            ..user("u1", "ann", Role::Admin)
        };
        let created = store
            .user_create(
                new,
                vec![
                    identity("whatsapp", "+15551234567"),
                    identity("irc:libera.chat", "account:ann"),
                ],
                at(1),
            )
            .await
            .unwrap();
        assert_eq!(created.id, id("u1"));
        assert_eq!(created.username, "ann");
        assert_eq!(created.display_name, "ANN");
        assert_eq!(created.role, Role::Admin);
        assert_eq!(created.state, UserState::Active);
        assert_eq!(created.timezone.as_deref(), Some("America/Los_Angeles"));
        assert_eq!(
            created.default_identity.as_deref(),
            Some("irc:libera.chat:account:ann")
        );
        assert_eq!((created.created_at, created.updated_at), (at(1), at(1)));
        let texts: Vec<_> = created
            .identities
            .iter()
            .map(StoredIdentity::text)
            .collect();
        assert_eq!(
            texts,
            ["whatsapp:+15551234567", "irc:libera.chat:account:ann"]
        );
        assert_eq!(created.identities[0].verified_at, at(1));
        assert_eq!(created.identities[0].verification, Verification::Link);
        assert_eq!(store.user(&id("u1")).await.unwrap(), Some(created.clone()));
        assert_eq!(store.users().await.unwrap(), [created]);
        assert_eq!(store.user(&id("nobody")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn ids_usernames_and_identities_are_unique() {
        let (_dir, store) = store().await;
        store
            .user_create(
                user("u1", "ann", Role::Member),
                vec![identity("web", "ann")],
                at(0),
            )
            .await
            .unwrap();
        let again = store
            .user_create(user("u1", "other", Role::Member), vec![], at(1))
            .await;
        assert!(matches!(again, Err(Error::UserIdTaken)), "{again:?}");
        let same_name = store
            .user_create(user("u2", "ANN", Role::Member), vec![], at(1))
            .await;
        assert!(
            matches!(same_name, Err(Error::UsernameTaken)),
            "{same_name:?}"
        );
        // `web:x` and `ann` make a different identity than `web` and `ann`.
        let same_identity = store
            .user_create(
                user("u3", "bob", Role::Member),
                vec![identity("web:x", "ann")],
                at(1),
            )
            .await;
        assert!(same_identity.is_ok());
        let taken = store
            .user_create(
                user("u4", "cy", Role::Member),
                vec![identity("web", "ann")],
                at(2),
            )
            .await;
        assert!(
            matches!(&taken, Err(Error::IdentityTaken { identity }) if identity == "web:ann"),
            "{taken:?}"
        );
        // The same string split differently is the same identity.
        let split = store
            .user_identity_add(&id("u1"), identity("web", "x:ann"), at(3))
            .await;
        assert!(
            matches!(split, Err(Error::IdentityTaken { .. })),
            "{split:?}"
        );
        // A failed create stores nothing.
        let ids: Vec<_> = store
            .users()
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.id.to_string())
            .collect();
        assert_eq!(ids, ["u1", "u3"]);
    }

    #[tokio::test]
    async fn the_default_identity_must_be_one_of_the_users() {
        let (_dir, store) = store().await;
        let new = NewUser {
            default_identity: Some("web:bob".into()),
            ..user("u1", "ann", Role::Member)
        };
        let err = store
            .user_create(new, vec![identity("web", "ann")], at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        assert!(store.users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_profile_change_is_stored_and_touches_only_the_profile() {
        let (_dir, store) = store().await;
        store
            .user_create(
                user("u1", "ann", Role::Admin),
                vec![identity("web", "ann")],
                at(0),
            )
            .await
            .unwrap();
        store
            .user_create(user("u2", "bob", Role::Member), vec![], at(0))
            .await
            .unwrap();
        let changed = store
            .user_profile_set(&id("u1"), "Ann Example", Some("Europe/London"), at(5))
            .await
            .unwrap();
        assert_eq!(changed.display_name, "Ann Example");
        assert_eq!(changed.timezone.as_deref(), Some("Europe/London"));
        assert_eq!(
            (changed.username.as_str(), changed.role),
            ("ann", Role::Admin)
        );
        assert_eq!(changed.identities.len(), 1);
        assert_eq!((changed.created_at, changed.updated_at), (at(0), at(5)));
        let bob = store.user(&id("u2")).await.unwrap().unwrap();
        assert_ne!(bob.display_name, "Ann Example");
        let cleared = store
            .user_profile_set(&id("u1"), "Ann", None, at(6))
            .await
            .unwrap();
        assert_eq!(cleared.timezone, None);
        let empty = store.user_profile_set(&id("u1"), "", None, at(7)).await;
        assert!(matches!(empty, Err(Error::InvalidRequest(_))), "{empty:?}");
        let missing = store
            .user_profile_set(&id("nobody"), "N", None, at(7))
            .await;
        assert!(matches!(missing, Err(Error::NoSuchUser)), "{missing:?}");
    }

    #[tokio::test]
    async fn state_changes_are_stored() {
        let (_dir, store) = store().await;
        store
            .user_create(user("u1", "ann", Role::Member), vec![], at(0))
            .await
            .unwrap();
        let disabled = store
            .user_set_state(&id("u1"), UserState::Disabled, at(5))
            .await
            .unwrap();
        assert_eq!(disabled.state, UserState::Disabled);
        assert_eq!((disabled.created_at, disabled.updated_at), (at(0), at(5)));
        let active = store
            .user_set_state(&id("u1"), UserState::Active, at(6))
            .await
            .unwrap();
        assert_eq!(active.state, UserState::Active);
        let missing = store
            .user_set_state(&id("nobody"), UserState::Disabled, at(7))
            .await;
        assert!(matches!(missing, Err(Error::NoSuchUser)), "{missing:?}");
    }

    #[tokio::test]
    async fn the_last_active_admin_can_be_neither_removed_nor_disabled_nor_demoted() {
        let (_dir, store) = store().await;
        store
            .user_create(
                user("a1", "ann", Role::Admin),
                vec![identity("web", "a1")],
                at(0),
            )
            .await
            .unwrap();
        store
            .user_create(user("m1", "mia", Role::Member), vec![], at(0))
            .await
            .unwrap();
        let refused = |result: Result<StoredUser>| {
            assert!(matches!(result, Err(Error::LastAdmin)), "{result:?}");
        };
        refused(
            store
                .user_set_state(&id("a1"), UserState::Disabled, at(1))
                .await,
        );
        refused(store.user_set_role(&id("a1"), Role::Member, at(1)).await);
        refused(store.user_delete(&id("a1")).await);
        let ann = store.user(&id("a1")).await.unwrap().unwrap();
        assert_eq!(
            (ann.role, ann.state, ann.updated_at, ann.identities.len()),
            (Role::Admin, UserState::Active, at(0), 1),
            "a refused change leaves the user as it was"
        );

        // A member is no admin to keep, and making one gives the first a way out.
        let mia = store
            .user_set_role(&id("m1"), Role::Admin, at(2))
            .await
            .unwrap();
        assert_eq!((mia.role, mia.updated_at), (Role::Admin, at(2)));
        let ann = store
            .user_set_role(&id("a1"), Role::Member, at(3))
            .await
            .unwrap();
        assert_eq!(ann.role, Role::Member);
        // Now mia is the last one.
        refused(store.user_set_role(&id("m1"), Role::Member, at(4)).await);
        refused(store.user_delete(&id("m1")).await);
        // A disabled admin isn't an active one: ann, promoted again but disabled, doesn't count.
        store
            .user_set_role(&id("a1"), Role::Admin, at(5))
            .await
            .unwrap();
        store
            .user_set_state(&id("a1"), UserState::Disabled, at(6))
            .await
            .unwrap();
        refused(
            store
                .user_set_state(&id("m1"), UserState::Disabled, at(7))
                .await,
        );
        refused(store.user_delete(&id("m1")).await);
        // Re-enabling is never refused; then either admin can go while the other stays.
        store
            .user_set_state(&id("a1"), UserState::Active, at(8))
            .await
            .unwrap();
        let removed = store.user_delete(&id("a1")).await.unwrap();
        assert_eq!(removed.identities.len(), 1);
        assert_eq!(store.users().await.unwrap().len(), 1);
        // Gone is gone.
        let missing = store.user_delete(&id("a1")).await;
        assert!(matches!(missing, Err(Error::NoSuchUser)), "{missing:?}");
        let missing = store.user_set_role(&id("a1"), Role::Admin, at(9)).await;
        assert!(matches!(missing, Err(Error::NoSuchUser)), "{missing:?}");
    }

    #[tokio::test]
    async fn the_first_identity_becomes_the_default_and_removing_it_moves_the_default() {
        let (_dir, store) = store().await;
        store
            .user_create(user("u1", "ann", Role::Member), vec![], at(0))
            .await
            .unwrap();
        let one = store
            .user_identity_add(&id("u1"), identity("whatsapp", "+1"), at(1))
            .await
            .unwrap();
        assert_eq!(one.default_identity.as_deref(), Some("whatsapp:+1"));
        let two = store
            .user_identity_add(&id("u1"), identity("irc:net", "account:ann"), at(2))
            .await
            .unwrap();
        assert_eq!(two.default_identity.as_deref(), Some("whatsapp:+1"));
        assert_eq!(two.updated_at, at(2));

        let after = store
            .user_identity_remove(&id("u1"), "whatsapp:+1", at(3))
            .await
            .unwrap();
        assert_eq!(
            after.default_identity.as_deref(),
            Some("irc:net:account:ann")
        );
        let none = store
            .user_identity_remove(&id("u1"), "irc:net:account:ann", at(4))
            .await
            .unwrap();
        assert!(none.identities.is_empty());
        assert_eq!(none.default_identity, None);

        let missing = store
            .user_identity_remove(&id("u1"), "irc:net:account:ann", at(5))
            .await;
        assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");
        let nobody = store
            .user_identity_add(&id("nobody"), identity("web", "x"), at(6))
            .await;
        assert!(matches!(nobody, Err(Error::NoSuchUser)), "{nobody:?}");
    }

    fn owner_import() -> ImportedUser {
        ImportedUser {
            user: NewUser {
                default_identity: Some("web:owner".into()),
                timezone: Some("UTC".into()),
                ..user("owner", "owner", Role::Admin)
            },
            identities: vec!["web:owner".into()],
        }
    }

    #[tokio::test]
    async fn the_owner_is_imported_once() {
        let (_dir, store) = store().await;
        assert!(!store.users_imported().await.unwrap());
        assert_eq!(
            store
                .users_import(Some(owner_import()), at(1))
                .await
                .unwrap(),
            Some(true)
        );
        assert!(store.users_imported().await.unwrap());
        let owner = store.user(&id("owner")).await.unwrap().unwrap();
        assert_eq!(owner.role, Role::Admin);
        assert_eq!(owner.timezone.as_deref(), Some("UTC"));
        assert_eq!(owner.identities.len(), 1);
        assert_eq!(owner.identities[0].verification, Verification::Imported);
        assert_eq!(owner.identities[0].verified_at, at(1));

        // Never again, even after the user is gone or with a different owner.
        store
            .write(|conn| {
                conn.execute("DELETE FROM users", [])?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .users_import(Some(owner_import()), at(2))
                .await
                .unwrap(),
            None
        );
        assert!(store.users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn importing_nothing_still_records_the_import() {
        let (_dir, store) = store().await;
        assert_eq!(store.users_import(None, at(1)).await.unwrap(), Some(false));
        assert!(store.users_imported().await.unwrap());
        assert_eq!(
            store
                .users_import(Some(owner_import()), at(2))
                .await
                .unwrap(),
            None
        );
        assert!(store.users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_import_leaves_existing_users_alone() {
        let (_dir, store) = store().await;
        store
            .user_create(user("u1", "ann", Role::Admin), vec![], at(0))
            .await
            .unwrap();
        assert_eq!(
            store
                .users_import(Some(owner_import()), at(1))
                .await
                .unwrap(),
            Some(false)
        );
        assert_eq!(store.users().await.unwrap().len(), 1);
        assert!(store.users_imported().await.unwrap());
    }

    #[tokio::test]
    async fn the_import_records_namespaces_with_colons_and_removal_goes_by_the_joined_identity() {
        let (_dir, store) = store().await;
        let mut owner = owner_import();
        owner.user.default_identity = Some("irc:libera.chat:account:stephen".into());
        owner.identities = vec!["web:owner".into(), "irc:libera.chat:account:stephen".into()];
        store.users_import(Some(owner), at(1)).await.unwrap();
        let imported = store.user(&id("owner")).await.unwrap().unwrap();
        let texts: Vec<_> = imported
            .identities
            .iter()
            .map(StoredIdentity::text)
            .collect();
        assert_eq!(texts, ["web:owner", "irc:libera.chat:account:stephen"]);
        assert_eq!(imported.identities[1].namespace, "irc");

        // The channel reports `irc:libera.chat`; the import's split doesn't matter.
        let taken = store
            .user_create(
                user("u2", "ann", Role::Member),
                vec![identity("irc:libera.chat", "account:stephen")],
                at(2),
            )
            .await;
        assert!(
            matches!(taken, Err(Error::IdentityTaken { .. })),
            "{taken:?}"
        );
        let after = store
            .user_identity_remove(&id("owner"), "irc:libera.chat:account:stephen", at(3))
            .await
            .unwrap();
        assert_eq!(after.default_identity.as_deref(), Some("web:owner"));
        assert_eq!(after.identities.len(), 1);
    }

    #[tokio::test]
    async fn removing_another_users_identity_finds_nothing_and_changes_nothing() {
        let (_dir, store) = store().await;
        for (user, name) in [("u1", "ann"), ("u2", "bob")] {
            store
                .user_create(
                    self::user(user, name, Role::Member),
                    vec![identity("web", name)],
                    at(0),
                )
                .await
                .unwrap();
        }
        let err = store
            .user_identity_remove(&id("u2"), "web:ann", at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
        let ann = store.user(&id("u1")).await.unwrap().unwrap();
        assert_eq!(ann.identities.len(), 1);
        assert_eq!(ann.updated_at, at(0));
        assert_eq!(
            store
                .user(&id("u2"))
                .await
                .unwrap()
                .unwrap()
                .identities
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn an_email_domain_is_stored_lowercase_so_one_sender_has_one_user() {
        let (_dir, store) = store().await;
        let ann = store
            .user_create(
                NewUser {
                    default_identity: Some("email:Ann@Example.COM".into()),
                    ..user("u1", "ann", Role::Member)
                },
                vec![identity("email", "Ann@Example.COM")],
                at(0),
            )
            .await
            .unwrap();
        assert_eq!(ann.identities[0].text(), "email:Ann@example.com");
        assert_eq!(
            ann.default_identity.as_deref(),
            Some("email:Ann@example.com")
        );
        // The domain's case doesn't make another identity; the local part's does.
        for same in ["Ann@example.com", "Ann@EXAMPLE.com"] {
            let taken = store
                .user_identity_add(&id("u1"), identity("email", same), at(1))
                .await;
            assert!(
                matches!(taken, Err(Error::IdentityTaken { .. })),
                "{same}: {taken:?}"
            );
        }
        let other = store
            .user_create(
                user("u2", "bob", Role::Member),
                vec![identity("email", "ann@example.com")],
                at(2),
            )
            .await;
        assert!(other.is_ok(), "{other:?}");
        // Removal finds it in any spelling of the domain.
        let after = store
            .user_identity_remove(&id("u1"), "email:Ann@EXAMPLE.com", at(3))
            .await
            .unwrap();
        assert!(after.identities.is_empty());
        // Other namespaces are exact.
        store
            .user_identity_add(&id("u1"), identity("xmpp", "ann@Example.com"), at(4))
            .await
            .unwrap();
        let exact = store
            .user_identity_add(&id("u2"), identity("xmpp", "ann@example.com"), at(5))
            .await;
        assert!(exact.is_ok(), "{exact:?}");
    }

    #[test]
    fn canonical_identities() {
        for (identity, canonical) in [
            ("email:me@Example.COM", "email:me@example.com"),
            ("email:Me@example.com", "email:Me@example.com"),
            ("email:work:Me@EXAMPLE.com", "email:work:Me@example.com"),
            ("email:postmaster", "email:postmaster"),
            ("xmpp:me@Example.com", "xmpp:me@Example.com"),
            ("irc:libera.chat:account:Me", "irc:libera.chat:account:Me"),
        ] {
            assert_eq!(canonical_identity(identity), canonical, "{identity}");
        }
    }

    #[tokio::test]
    async fn deleting_a_user_deletes_their_identities() {
        let (_dir, store) = store().await;
        store
            .user_create(
                user("u1", "ann", Role::Member),
                vec![identity("web", "ann"), identity("whatsapp", "+1")],
                at(0),
            )
            .await
            .unwrap();
        store
            .user_create(
                user("u2", "bob", Role::Member),
                vec![identity("web", "bob")],
                at(0),
            )
            .await
            .unwrap();
        let rows = |store: &Store| {
            let store = store.clone();
            async move {
                store
                    .read(|conn| {
                        Ok(
                            conn.query_row("SELECT count(*) FROM user_identities", [], |r| {
                                r.get::<_, i64>(0)
                            })?,
                        )
                    })
                    .await
                    .unwrap()
            }
        };
        assert_eq!(rows(&store).await, 3);
        store
            .write(|conn| {
                conn.execute("DELETE FROM users WHERE id = 'u1'", [])?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(rows(&store).await, 1);
        assert_eq!(
            store
                .user(&id("u2"))
                .await
                .unwrap()
                .unwrap()
                .identities
                .len(),
            1
        );
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
            == 1
    }

    #[tokio::test]
    async fn an_existing_database_gains_the_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doozers.db");
        let before = MIGRATIONS
            .iter()
            .position(|m| m.name == "0015_users")
            .unwrap();
        {
            let mut conn = Connection::open(&path).unwrap();
            migrate(&mut conn, &MIGRATIONS[..before], None).unwrap();
            for table in ["users", "user_identities", "users_imported"] {
                assert!(!table_exists(&conn, table), "{table}");
            }
        }
        let store = Store::open(&path).await.unwrap();
        let (tables, version) = store
            .read(|conn| {
                Ok((
                    ["users", "user_identities", "users_imported"]
                        .map(|table| table_exists(conn, table)),
                    conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(tables, [true; 3]);
        assert_eq!(version, SCHEMA_VERSION);
        assert!(store.users().await.unwrap().is_empty());
        assert!(!store.users_imported().await.unwrap());
    }

    #[tokio::test]
    async fn a_failed_import_stores_nothing() {
        let (_dir, store) = store().await;
        let mut bad = owner_import();
        bad.user.default_identity = Some("web:nobody".into());
        let err = store.users_import(Some(bad), at(1)).await.unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        assert!(!store.users_imported().await.unwrap());
        assert!(store.users().await.unwrap().is_empty());
    }

    async fn two_users() -> (tempfile::TempDir, Store) {
        let (dir, store) = store().await;
        for (u, name) in [("u1", "ann"), ("u2", "bob")] {
            store
                .user_create(user(u, name, Role::Member), vec![], at(0))
                .await
                .unwrap();
        }
        (dir, store)
    }

    #[tokio::test]
    async fn the_default_identity_is_set_to_one_the_user_has() {
        let (_dir, store) = two_users().await;
        store
            .user_identity_add(&id("u1"), identity("whatsapp", "+1"), at(1))
            .await
            .unwrap();
        store
            .user_identity_add(&id("u1"), identity("email", "ann@Example.com"), at(2))
            .await
            .unwrap();
        let set = store
            .user_default_identity_set(&id("u1"), "email:ann@EXAMPLE.com", at(3))
            .await
            .unwrap();
        assert_eq!(
            set.default_identity.as_deref(),
            Some("email:ann@example.com")
        );
        assert_eq!(set.updated_at, at(3));
        // Another user's identity, an identity nobody has, and a user who doesn't exist.
        store
            .user_identity_add(&id("u2"), identity("whatsapp", "+2"), at(4))
            .await
            .unwrap();
        for identity in ["whatsapp:+2", "whatsapp:+3"] {
            let err = store
                .user_default_identity_set(&id("u1"), identity, at(5))
                .await;
            assert!(matches!(err, Err(Error::NotFound(_))), "{err:?}");
        }
        let err = store
            .user_default_identity_set(&id("nobody"), "whatsapp:+1", at(5))
            .await;
        assert!(matches!(err, Err(Error::NoSuchUser)), "{err:?}");
        let ann = store.user(&id("u1")).await.unwrap().unwrap();
        assert_eq!(
            ann.default_identity.as_deref(),
            Some("email:ann@example.com")
        );
    }

    fn admin_identity(native_id: &str) -> NewIdentity {
        NewIdentity {
            verification: Verification::Admin,
            ..identity("web", native_id)
        }
    }

    #[tokio::test]
    async fn the_proxy_identity_replaces_the_last_and_keeps_the_login_identity() {
        let (_dir, store) = two_users().await;
        store
            .user_identity_add(&id("u1"), admin_identity("u1"), at(1))
            .await
            .unwrap();
        store
            .user_identity_add(&id("u1"), identity("whatsapp", "+1"), at(2))
            .await
            .unwrap();
        let set = |native: Option<&'static str>, secs| {
            let store = store.clone();
            async move {
                store
                    .user_proxy_identity_set(&id("u1"), "web", "u1", native, at(secs))
                    .await
            }
        };
        let first = set(Some("ann@example.com"), 3).await.unwrap();
        let texts: Vec<_> = first.identities.iter().map(StoredIdentity::text).collect();
        assert_eq!(texts, ["web:u1", "whatsapp:+1", "web:ann@example.com"]);
        assert_eq!(first.identities[2].verification, Verification::Admin);
        assert_eq!(first.identities[2].verified_at, at(3));
        // A second one replaces it. The login identity and other namespaces stay.
        let second = set(Some("ann@corp.example"), 4).await.unwrap();
        let texts: Vec<_> = second.identities.iter().map(StoredIdentity::text).collect();
        assert_eq!(texts, ["web:u1", "whatsapp:+1", "web:ann@corp.example"]);
        // The same one again changes no identity.
        let same = set(Some("ann@corp.example"), 5).await.unwrap();
        assert_eq!(same.identities, second.identities);
        // None clears it.
        let cleared = set(None, 6).await.unwrap();
        let texts: Vec<_> = cleared
            .identities
            .iter()
            .map(StoredIdentity::text)
            .collect();
        assert_eq!(texts, ["web:u1", "whatsapp:+1"]);
        assert_eq!(cleared.updated_at, at(6));
    }

    #[tokio::test]
    async fn a_refused_proxy_identity_leaves_the_one_the_user_had() {
        let (_dir, store) = two_users().await;
        store
            .user_identity_add(&id("u2"), admin_identity("bob@example.com"), at(1))
            .await
            .unwrap();
        store
            .user_proxy_identity_set(&id("u1"), "web", "u1", Some("ann@example.com"), at(2))
            .await
            .unwrap();
        let err = store
            .user_proxy_identity_set(&id("u1"), "web", "u1", Some("bob@example.com"), at(3))
            .await;
        assert!(matches!(err, Err(Error::IdentityTaken { .. })), "{err:?}");
        let ann = store.user(&id("u1")).await.unwrap().unwrap();
        let texts: Vec<_> = ann.identities.iter().map(StoredIdentity::text).collect();
        assert_eq!(texts, ["web:ann@example.com"]);
        let err = store
            .user_proxy_identity_set(&id("nobody"), "web", "nobody", None, at(4))
            .await;
        assert!(matches!(err, Err(Error::NoSuchUser)), "{err:?}");
    }

    #[tokio::test]
    async fn an_imported_proxy_identity_is_replaced_and_one_the_user_has_becomes_an_admin_one() {
        let (_dir, store) = store().await;
        let imported = ImportedUser {
            user: user("owner", "owner", Role::Admin),
            identities: vec!["web:alice@example.com".to_owned(), "web:owner".to_owned()],
        };
        store.users_import(Some(imported), at(0)).await.unwrap();
        let same = store
            .user_proxy_identity_set(
                &id("owner"),
                "web",
                "owner",
                Some("alice@example.com"),
                at(1),
            )
            .await
            .unwrap();
        let alice = same
            .identities
            .iter()
            .find(|i| i.native_id == "alice@example.com")
            .unwrap();
        assert_eq!(
            (alice.verification, alice.verified_at),
            (Verification::Admin, at(1))
        );
        // Replacing the imported one removes it, and the default moves off it.
        let other = store
            .user_proxy_identity_set(&id("owner"), "web", "owner", Some("al@example.com"), at(2))
            .await
            .unwrap();
        let texts: Vec<_> = other.identities.iter().map(StoredIdentity::text).collect();
        assert_eq!(texts, ["web:owner", "web:al@example.com"]);
    }
}
