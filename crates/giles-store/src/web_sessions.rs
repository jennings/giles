//! `web_sessions`: web login sessions behind the session cookie (PLAN 3.4).
//!
//! The store keeps only the SHA-256 of each cookie token, so a copy of the database (a backup)
//! can't be replayed as a login. The token is random with at least 128 bits of entropy (the web
//! login code generates it), so a plain hash needs no salt or slow KDF.

use giles_types::{Timestamp, UserId};
use rusqlite::{ErrorCode, params};
use sha2::{Digest, Sha256};

use crate::time::to_millis;
use crate::{Error, Result, Store, get_time, parse_id, query_opt};

/// A live web login session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSession {
    /// The user it's logged in as.
    pub user: UserId,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it expires.
    pub expires_at: Timestamp,
}

fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

impl Store {
    /// Stores a session for the cookie `token`. [`Error::Conflict`] if a session with the same
    /// token exists; [`Error::InvalidRequest`] unless `expires_at` is after `now`.
    pub async fn web_session_create(
        &self,
        token: &str,
        user: UserId,
        now: Timestamp,
        expires_at: Timestamp,
    ) -> Result<()> {
        if expires_at <= now {
            return Err(Error::invalid(
                "a web session must expire after it's created",
            ));
        }
        let hash = token_hash(token);
        self.write(move |conn| {
            let inserted = conn
                .prepare_cached(
                    "INSERT INTO web_sessions (token_hash, user_id, created_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4)",
                )?
                .execute(params![
                    hash,
                    user.as_str(),
                    to_millis(now),
                    to_millis(expires_at)
                ]);
            match inserted {
                Ok(_) => Ok(()),
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == ErrorCode::ConstraintViolation =>
                {
                    Err(Error::conflict("a web session with this token exists"))
                }
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// The session for the cookie `token`, unless it doesn't exist or has expired
    /// (`expires_at <= now`).
    pub async fn web_session(&self, token: &str, now: Timestamp) -> Result<Option<WebSession>> {
        let hash = token_hash(token);
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT user_id, created_at, expires_at FROM web_sessions
                 WHERE token_hash = ?1 AND expires_at > ?2",
            )?;
            query_opt(&mut stmt, params![hash, to_millis(now)], |row| {
                Ok(WebSession {
                    user: parse_id(row.get(0)?, UserId::new)?,
                    created_at: get_time(row, 1)?,
                    expires_at: get_time(row, 2)?,
                })
            })
        })
        .await
    }

    /// Deletes the session for `token` (logout). Returns `false` if there was none.
    pub async fn web_session_delete(&self, token: &str) -> Result<bool> {
        let hash = token_hash(token);
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("DELETE FROM web_sessions WHERE token_hash = ?1")?
                .execute([hash])?
                == 1)
        })
        .await
    }

    /// Deletes every session with `expires_at <= now`; returns how many.
    pub async fn web_sessions_expire(&self, now: Timestamp) -> Result<u64> {
        self.write(move |conn| {
            let deleted = conn
                .prepare_cached("DELETE FROM web_sessions WHERE expires_at <= ?1")?
                .execute([to_millis(now)])?;
            Ok(deleted as u64)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::{at, store};

    fn owner() -> UserId {
        UserId::new("owner").unwrap()
    }

    #[tokio::test]
    async fn sessions_resolve_until_they_expire() {
        let (_dir, store) = store().await;
        store
            .web_session_create("tok-a", owner(), at(0), at(100))
            .await
            .unwrap();
        let session = store.web_session("tok-a", at(99)).await.unwrap().unwrap();
        assert_eq!(
            session,
            WebSession {
                user: owner(),
                created_at: at(0),
                expires_at: at(100)
            }
        );
        assert_eq!(store.web_session("tok-a", at(100)).await.unwrap(), None);
        assert_eq!(store.web_session("tok-b", at(1)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn only_the_token_hash_is_stored() {
        let (_dir, store) = store().await;
        store
            .web_session_create("secret-token", owner(), at(0), at(100))
            .await
            .unwrap();
        let (stored, matches_raw) = store
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT token_hash, token_hash = CAST('secret-token' AS BLOB) FROM web_sessions",
                    [],
                    |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, bool>(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(stored, token_hash("secret-token"));
        assert_eq!(stored.len(), 32);
        assert!(!matches_raw);
    }

    #[tokio::test]
    async fn duplicate_tokens_conflict_and_bad_expiry_is_rejected() {
        let (_dir, store) = store().await;
        store
            .web_session_create("t", owner(), at(0), at(100))
            .await
            .unwrap();
        let err = store
            .web_session_create("t", owner(), at(1), at(100))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        let err = store
            .web_session_create("u", owner(), at(5), at(5))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[tokio::test]
    async fn logout_and_expiry_delete_rows() {
        let (_dir, store) = store().await;
        store
            .web_session_create("a", owner(), at(0), at(10))
            .await
            .unwrap();
        store
            .web_session_create("b", owner(), at(0), at(20))
            .await
            .unwrap();
        store
            .web_session_create("c", owner(), at(0), at(30))
            .await
            .unwrap();

        assert!(store.web_session_delete("c").await.unwrap());
        assert!(!store.web_session_delete("c").await.unwrap());
        assert_eq!(store.web_sessions_expire(at(9)).await.unwrap(), 0);
        assert_eq!(store.web_sessions_expire(at(20)).await.unwrap(), 2);
        assert_eq!(store.web_sessions_expire(at(100)).await.unwrap(), 0);
    }
}
