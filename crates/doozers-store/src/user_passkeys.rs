//! `user_passkeys`: the passkeys users sign in with (PLAN 3.4), and the key the sign-in page makes
//! up stand-in credential ids with.
//!
//! The store keeps each passkey's credential as the opaque JSON the WebAuthn library serialized
//! and never reads it: the core (de)serializes it, checks every signature, and hands the store
//! the results. What the schema enforces is that a credential id belongs to one passkey of one
//! user, because a sign-in finds the user by it, and that removing a user removes their passkeys.

use doozers_types::{Timestamp, UserId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::time::to_millis;
use crate::{Error, Result, Store, get_opt_time, get_time, parse_id, query_all, row_id};

row_id!(
    /// A `user_passkeys` row id. Never reused.
    PasskeyId
);

/// How a passkey was made, which decides how the browser can ask for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasskeyKind {
    /// The authenticator keeps it (a discoverable credential), so the browser can offer it with no
    /// username.
    Discoverable,
    /// A credential the authenticator doesn't keep, for a hardware key with few slots: the browser
    /// can only use it when the server names it, which needs the username first.
    NonResident,
}

impl PasskeyKind {
    /// The stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            PasskeyKind::Discoverable => "discoverable",
            PasskeyKind::NonResident => "non_resident",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "discoverable" => Ok(PasskeyKind::Discoverable),
            "non_resident" => Ok(PasskeyKind::NonResident),
            other => Err(Error::corrupt(format!("passkey kind {other:?}"))),
        }
    }
}

/// A passkey, as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPasskey {
    /// The row id.
    pub id: PasskeyId,
    /// The user it signs in.
    pub user: UserId,
    /// The authenticator's id for the credential.
    pub credential_id: Vec<u8>,
    /// The serialized credential: public key, signature counter, backup flags, and transports.
    pub credential: String,
    /// How it was made.
    pub kind: PasskeyKind,
    /// What the user calls it.
    pub name: String,
    /// When it was added.
    pub created_at: Timestamp,
    /// When it last signed in, if it ever has.
    pub last_used_at: Option<Timestamp>,
}

/// What [`Store::user_passkey_add`] stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPasskey {
    /// The user it signs in.
    pub user: UserId,
    /// The authenticator's id for the credential.
    pub credential_id: Vec<u8>,
    /// The serialized credential.
    pub credential: String,
    /// How it was made.
    pub kind: PasskeyKind,
    /// What the user calls it.
    pub name: String,
}

const COLUMNS: &str =
    "id, user_id, credential_id, credential, kind, name, created_at, last_used_at";

fn from_row(row: &Row<'_>) -> Result<StoredPasskey> {
    Ok(StoredPasskey {
        id: row.get(0)?,
        user: parse_id(row.get(1)?, UserId::new)?,
        credential_id: row.get(2)?,
        credential: row.get(3)?,
        kind: PasskeyKind::parse(&row.get::<_, String>(4)?)?,
        name: row.get(5)?,
        created_at: get_time(row, 6)?,
        last_used_at: get_opt_time(row, 7)?,
    })
}

fn find(conn: &Connection, id: PasskeyId) -> Result<Option<StoredPasskey>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM user_passkeys WHERE id = ?1"
    ))?;
    stmt.query_row([id], |row| Ok(from_row(row)))
        .optional()?
        .transpose()
}

impl Store {
    /// Adds a passkey with `created_at` set to `now` and returns it.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`] if the user doesn't exist; [`Error::Conflict`] if a passkey (of any
    /// user) has this credential id; [`Error::InvalidRequest`] if the credential id, credential,
    /// or name is empty. Nothing changes.
    pub async fn user_passkey_add(&self, new: NewPasskey, now: Timestamp) -> Result<StoredPasskey> {
        if new.credential_id.is_empty() || new.credential.is_empty() || new.name.is_empty() {
            return Err(Error::invalid(
                "a passkey needs a credential id, a credential, and a name",
            ));
        }
        self.write(move |conn| {
            if !conn
                .prepare_cached("SELECT 1 FROM users WHERE id = ?1")?
                .exists([new.user.as_str()])?
            {
                return Err(Error::NoSuchUser);
            }
            if conn
                .prepare_cached("SELECT 1 FROM user_passkeys WHERE credential_id = ?1")?
                .exists([&new.credential_id])?
            {
                return Err(Error::Conflict(
                    "a passkey with this credential id exists".to_owned(),
                ));
            }
            conn.prepare_cached(
                "INSERT INTO user_passkeys
                     (user_id, credential_id, credential, kind, name, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                new.user.as_str(),
                new.credential_id,
                new.credential,
                new.kind.as_str(),
                new.name,
                to_millis(now)
            ])?;
            let id = PasskeyId::new(conn.last_insert_rowid());
            find(conn, id)?.ok_or_else(|| Error::corrupt("the passkey just inserted is missing"))
        })
        .await
    }

    /// `user`'s passkeys, oldest first.
    pub async fn user_passkeys(&self, user: &UserId) -> Result<Vec<StoredPasskey>> {
        let user = user.clone();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM user_passkeys WHERE user_id = ?1 ORDER BY created_at, id"
            ))?;
            query_all(&mut stmt, [user.as_str()], from_row)
        })
        .await
    }

    /// The passkey with this credential id, whoever it belongs to.
    pub async fn user_passkey_by_credential(
        &self,
        credential_id: &[u8],
    ) -> Result<Option<StoredPasskey>> {
        let credential_id = credential_id.to_vec();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM user_passkeys WHERE credential_id = ?1"
            ))?;
            stmt.query_row([credential_id], |row| Ok(from_row(row)))
                .optional()?
                .transpose()
        })
        .await
    }

    /// Records a sign-in with passkey `id`: its `last_used_at` is `now`, and its serialized
    /// credential becomes `credential` (the signature counter moved). `false` if there is no such
    /// passkey.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if `credential` is empty.
    pub async fn user_passkey_used(
        &self,
        id: PasskeyId,
        credential: &str,
        now: Timestamp,
    ) -> Result<bool> {
        if credential.is_empty() {
            return Err(Error::invalid("a passkey credential can't be empty"));
        }
        let credential = credential.to_owned();
        self.write(move |conn| {
            Ok(conn
                .prepare_cached(
                    "UPDATE user_passkeys SET credential = ?2, last_used_at = ?3 WHERE id = ?1",
                )?
                .execute(params![id, credential, to_millis(now)])?
                == 1)
        })
        .await
    }

    /// Renames `user`'s passkey `id` to `name`. `false` if `user` has no such passkey: another
    /// user's passkey id is no passkey at all.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if `name` is empty.
    pub async fn user_passkey_rename(
        &self,
        user: &UserId,
        id: PasskeyId,
        name: &str,
    ) -> Result<bool> {
        if name.is_empty() {
            return Err(Error::invalid("a passkey's name can't be empty"));
        }
        let (user, name) = (user.clone(), name.to_owned());
        self.write(move |conn| {
            Ok(conn
                .prepare_cached(
                    "UPDATE user_passkeys SET name = ?3 WHERE id = ?1 AND user_id = ?2",
                )?
                .execute(params![id, user.as_str(), name])?
                == 1)
        })
        .await
    }

    /// The key stand-in credential ids are made with: `candidate` if none is stored yet (which it
    /// stores), else the one that was. Callers pass fresh random bytes every time, so the first
    /// call after a database is made decides, and every later one gets the same key back.
    pub async fn passkey_fake_key(&self, candidate: [u8; 32]) -> Result<[u8; 32]> {
        self.write(move |conn| {
            conn.prepare_cached(
                "INSERT INTO passkey_fake_key (id, key) VALUES (1, ?1) ON CONFLICT (id) DO NOTHING",
            )?
            .execute([candidate.as_slice()])?;
            let key: Vec<u8> = conn
                .prepare_cached("SELECT key FROM passkey_fake_key WHERE id = 1")?
                .query_row([], |row| row.get(0))?;
            <[u8; 32]>::try_from(key.as_slice())
                .map_err(|_| Error::corrupt("the passkey stand-in key isn't 32 bytes"))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::{at, store};
    use crate::{NewUser, Role};

    fn id(text: &str) -> UserId {
        UserId::new(text).unwrap()
    }

    async fn with_users() -> (tempfile::TempDir, Store) {
        let (dir, store) = store().await;
        for (user, role) in [("ann", Role::Admin), ("bob", Role::Member)] {
            let new = NewUser {
                id: id(user),
                username: user.to_owned(),
                display_name: user.to_uppercase(),
                role,
                timezone: None,
                default_identity: None,
            };
            store.user_create(new, vec![], at(0)).await.unwrap();
        }
        (dir, store)
    }

    fn new(user: &str, credential_id: &[u8], kind: PasskeyKind, name: &str) -> NewPasskey {
        NewPasskey {
            user: id(user),
            credential_id: credential_id.to_vec(),
            credential: format!(r#"{{"cred_id":"{name}"}}"#),
            kind,
            name: name.to_owned(),
        }
    }

    #[tokio::test]
    async fn a_user_has_the_passkeys_they_added_oldest_first() {
        let (_dir, store) = with_users().await;
        assert!(store.user_passkeys(&id("bob")).await.unwrap().is_empty());
        let phone = store
            .user_passkey_add(
                new("bob", b"one", PasskeyKind::Discoverable, "Phone"),
                at(10),
            )
            .await
            .unwrap();
        let key = store
            .user_passkey_add(new("bob", b"two", PasskeyKind::NonResident, "Key"), at(20))
            .await
            .unwrap();
        store
            .user_passkey_add(
                new("ann", b"three", PasskeyKind::Discoverable, "Laptop"),
                at(5),
            )
            .await
            .unwrap();
        assert_eq!(phone.kind, PasskeyKind::Discoverable);
        assert_eq!(phone.created_at, at(10));
        assert_eq!(phone.last_used_at, None);
        assert_ne!(phone.id, key.id);
        assert_eq!(
            store.user_passkeys(&id("bob")).await.unwrap(),
            vec![phone.clone(), key.clone()]
        );
        assert_eq!(store.user_passkeys(&id("ann")).await.unwrap().len(), 1);
        assert_eq!(
            store.user_passkey_by_credential(b"two").await.unwrap(),
            Some(key)
        );
        assert_eq!(
            store.user_passkey_by_credential(b"none").await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_passkey_is_renamed_by_its_user_only() {
        let (_dir, store) = with_users().await;
        let key = store
            .user_passkey_add(
                new("ann", b"one", PasskeyKind::Discoverable, "Phone"),
                at(1),
            )
            .await
            .unwrap();
        assert!(
            store
                .user_passkey_rename(&id("ann"), key.id, "Work laptop")
                .await
                .unwrap()
        );
        assert!(
            !store
                .user_passkey_rename(&id("bob"), key.id, "Mine now")
                .await
                .unwrap()
        );
        let err = store
            .user_passkey_rename(&id("ann"), key.id, "")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        let kept = &store.user_passkeys(&id("ann")).await.unwrap()[0];
        assert_eq!(kept.name, "Work laptop");
    }

    #[tokio::test]
    async fn a_credential_id_belongs_to_one_passkey_and_a_passkey_to_a_real_user() {
        let (_dir, store) = with_users().await;
        store
            .user_passkey_add(
                new("bob", b"one", PasskeyKind::Discoverable, "Phone"),
                at(1),
            )
            .await
            .unwrap();
        // Not twice for the same user, and not for another user either.
        for user in ["bob", "ann"] {
            let err = store
                .user_passkey_add(new(user, b"one", PasskeyKind::NonResident, "Copy"), at(2))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::Conflict(_)), "{user}: {err}");
        }
        let err = store
            .user_passkey_add(new("nobody", b"x", PasskeyKind::Discoverable, "X"), at(2))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSuchUser), "{err}");
        for bad in [
            new("bob", b"", PasskeyKind::Discoverable, "X"),
            new("bob", b"y", PasskeyKind::Discoverable, ""),
            NewPasskey {
                credential: String::new(),
                ..new("bob", b"y", PasskeyKind::Discoverable, "Y")
            },
        ] {
            let err = store.user_passkey_add(bad, at(2)).await.unwrap_err();
            assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        }
        assert_eq!(store.user_passkeys(&id("bob")).await.unwrap().len(), 1);
        assert!(store.user_passkeys(&id("ann")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn using_a_passkey_stores_its_credential_and_when() {
        let (_dir, store) = with_users().await;
        let passkey = store
            .user_passkey_add(
                new("bob", b"one", PasskeyKind::Discoverable, "Phone"),
                at(1),
            )
            .await
            .unwrap();
        assert!(
            store
                .user_passkey_used(passkey.id, r#"{"counter":2}"#, at(30))
                .await
                .unwrap()
        );
        let after = store
            .user_passkey_by_credential(b"one")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.credential, r#"{"counter":2}"#);
        assert_eq!(after.last_used_at, Some(at(30)));
        assert_eq!(after.created_at, at(1));
        assert_eq!(after.name, "Phone");
        assert!(
            !store
                .user_passkey_used(PasskeyId::new(999), "{}", at(31))
                .await
                .unwrap()
        );
        let err = store
            .user_passkey_used(passkey.id, "", at(31))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[tokio::test]
    async fn removing_a_user_removes_their_passkeys() {
        let (_dir, store) = with_users().await;
        store
            .user_passkey_add(
                new("bob", b"one", PasskeyKind::Discoverable, "Phone"),
                at(1),
            )
            .await
            .unwrap();
        store
            .user_passkey_add(
                new("ann", b"two", PasskeyKind::Discoverable, "Laptop"),
                at(1),
            )
            .await
            .unwrap();
        store.user_delete(&id("bob")).await.unwrap();
        assert_eq!(
            store.user_passkey_by_credential(b"one").await.unwrap(),
            None
        );
        assert!(
            store
                .user_passkey_by_credential(b"two")
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_stand_in_key_is_the_first_one_stored_and_survives_a_reopen() {
        let (dir, store) = store().await;
        let first = store.passkey_fake_key([1; 32]).await.unwrap();
        assert_eq!(first, [1; 32]);
        assert_eq!(store.passkey_fake_key([2; 32]).await.unwrap(), [1; 32]);
        drop(store);
        let store = Store::open(dir.path().join("doozers.db")).await.unwrap();
        assert_eq!(store.passkey_fake_key([3; 32]).await.unwrap(), [1; 32]);
    }
}
