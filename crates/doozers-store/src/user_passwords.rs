//! `user_passwords`: the password a user signs in with (PLAN 3.4).
//!
//! The store keeps the Argon2id hash in PHC string format and nothing else about the password. It
//! doesn't parse the string, and hashing and checking are the caller's: the work is slow and
//! memory-hungry on purpose, and doesn't belong on a database thread. A user has at most one
//! password, and removing the user removes it.

use doozers_types::{Timestamp, UserId};
use rusqlite::params;

use crate::time::to_millis;
use crate::{Error, Result, Store, get_time, query_opt};

/// A user's password, as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPassword {
    /// The Argon2id hash as a PHC string, salt and parameters included.
    pub hash: String,
    /// When the user last set it.
    pub changed_at: Timestamp,
}

impl Store {
    /// Sets `user`'s password to `hash` (a PHC string), replacing the one they had, at `now`.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`] if `user` doesn't exist; [`Error::InvalidRequest`] if `hash` is
    /// empty.
    pub async fn user_password_set(&self, user: &UserId, hash: &str, now: Timestamp) -> Result<()> {
        if hash.is_empty() {
            return Err(Error::invalid("a password hash can't be empty"));
        }
        let user = user.clone();
        let hash = hash.to_owned();
        self.write(move |conn| {
            if !conn
                .prepare_cached("SELECT 1 FROM users WHERE id = ?1")?
                .exists([user.as_str()])?
            {
                return Err(Error::NoSuchUser);
            }
            conn.prepare_cached(
                "INSERT INTO user_passwords (user_id, hash, changed_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (user_id) DO UPDATE
                 SET hash = excluded.hash, changed_at = excluded.changed_at",
            )?
            .execute(params![user.as_str(), hash, to_millis(now)])?;
            Ok(())
        })
        .await
    }

    /// The password of `user`, if they have one.
    pub async fn user_password(&self, user: &UserId) -> Result<Option<StoredPassword>> {
        let user = user.clone();
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT hash, changed_at FROM user_passwords WHERE user_id = ?1")?;
            query_opt(&mut stmt, [user.as_str()], |row| {
                Ok(StoredPassword {
                    hash: row.get(0)?,
                    changed_at: get_time(row, 1)?,
                })
            })
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

    #[tokio::test]
    async fn a_user_has_the_password_they_set_last() {
        let (_dir, store) = with_users().await;
        assert_eq!(store.user_password(&id("bob")).await.unwrap(), None);
        store
            .user_password_set(&id("bob"), "$argon2id$first", at(10))
            .await
            .unwrap();
        store
            .user_password_set(&id("ann"), "$argon2id$ann", at(11))
            .await
            .unwrap();
        assert_eq!(
            store.user_password(&id("bob")).await.unwrap(),
            Some(StoredPassword {
                hash: "$argon2id$first".into(),
                changed_at: at(10)
            })
        );
        store
            .user_password_set(&id("bob"), "$argon2id$second", at(20))
            .await
            .unwrap();
        assert_eq!(
            store.user_password(&id("bob")).await.unwrap(),
            Some(StoredPassword {
                hash: "$argon2id$second".into(),
                changed_at: at(20)
            })
        );
        let ann = store.user_password(&id("ann")).await.unwrap().unwrap();
        assert_eq!(ann.hash, "$argon2id$ann", "another user's password stays");
    }

    #[tokio::test]
    async fn only_a_user_who_exists_can_have_a_password() {
        let (_dir, store) = with_users().await;
        let err = store
            .user_password_set(&id("nobody"), "$argon2id$x", at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSuchUser), "{err}");
        let err = store
            .user_password_set(&id("bob"), "", at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        assert_eq!(store.user_password(&id("nobody")).await.unwrap(), None);
    }

    #[tokio::test]
    async fn removing_a_user_removes_their_password() {
        let (_dir, store) = with_users().await;
        store
            .user_password_set(&id("bob"), "$argon2id$bob", at(1))
            .await
            .unwrap();
        store
            .user_password_set(&id("ann"), "$argon2id$ann", at(1))
            .await
            .unwrap();
        store.user_delete(&id("bob")).await.unwrap();
        assert_eq!(store.user_password(&id("bob")).await.unwrap(), None);
        let rows = store
            .read(|conn| {
                Ok(
                    conn.query_row("SELECT count(*) FROM user_passwords", [], |row| {
                        row.get::<_, i64>(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(rows, 1);
        assert!(store.user_password(&id("ann")).await.unwrap().is_some());
    }
}
