//! `web_sessions`: web login sessions behind the session cookie (PLAN 3.4).
//!
//! The store keeps only the SHA-256 of each cookie token, so a copy of the database (a backup)
//! can't be replayed as a login. The token is random with at least 128 bits of entropy (the web
//! login code generates it), so a plain hash needs no salt or slow KDF.

use doozers_types::{Timestamp, UserId};
use rusqlite::{ErrorCode, Row, params};
use sha2::{Digest, Sha256};

use crate::time::to_millis;
use crate::{Error, Result, Store, get_opt_time, get_time, parse_id, query_all, query_opt};

/// How a web session signed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInMethod {
    /// A one-time invite link, which sets up a new account.
    Invite,
    /// A one-time recovery link: the operator's `doozers admin login-link`, or one an admin made.
    Recovery,
    /// A username and password.
    Password,
    /// A passkey.
    Passkey,
    /// An OpenID Connect identity provider.
    Oidc,
}

impl SignInMethod {
    /// The stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            SignInMethod::Invite => "invite",
            SignInMethod::Recovery => "recovery",
            SignInMethod::Password => "password",
            SignInMethod::Passkey => "passkey",
            SignInMethod::Oidc => "oidc",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "invite" => Ok(SignInMethod::Invite),
            "recovery" => Ok(SignInMethod::Recovery),
            "password" => Ok(SignInMethod::Password),
            "passkey" => Ok(SignInMethod::Passkey),
            "oidc" => Ok(SignInMethod::Oidc),
            other => Err(Error::corrupt(format!("web session method {other:?}"))),
        }
    }
}

/// A live web login session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSession {
    /// The session's opaque id, which the account page lists and revokes it by. It is random and
    /// has nothing to do with the cookie token, so showing it shows nothing that logs anyone in.
    pub id: String,
    /// The user it's logged in as.
    pub user: UserId,
    /// How it signed in.
    pub method: SignInMethod,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it expires.
    pub expires_at: Timestamp,
    /// When a request last used it, as often as the core records that; `None` before the first.
    pub last_seen_at: Option<Timestamp>,
    /// When the user last proved who they are again in this session
    /// ([`Store::web_session_reverified`]), if they have.
    pub reverified_at: Option<Timestamp>,
}

const COLUMNS: &str = "id, user_id, method, created_at, expires_at, last_seen_at, reverified_at";

fn from_row(row: &Row<'_>) -> Result<WebSession> {
    Ok(WebSession {
        id: row.get(0)?,
        user: parse_id(row.get(1)?, UserId::new)?,
        method: SignInMethod::parse(&row.get::<_, String>(2)?)?,
        created_at: get_time(row, 3)?,
        expires_at: get_time(row, 4)?,
        last_seen_at: get_opt_time(row, 5)?,
        reverified_at: get_opt_time(row, 6)?,
    })
}

/// The SHA-256 of a cookie or link token, which is what the tables keep.
pub(crate) fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

impl Store {
    /// Stores a session for the cookie `token`, which signed in with `method`, under a new random
    /// id, and records `now` as the time the user last signed in
    /// ([`StoredUser::last_sign_in_at`](crate::StoredUser::last_sign_in_at)).
    /// [`Error::Conflict`] if a session with the same token exists; [`Error::InvalidRequest`]
    /// unless `expires_at` is after `now`.
    pub async fn web_session_create(
        &self,
        token: &str,
        user: UserId,
        method: SignInMethod,
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
                    "INSERT INTO web_sessions
                         (token_hash, id, user_id, method, created_at, expires_at)
                     VALUES (?1, lower(hex(randomblob(16))), ?2, ?3, ?4, ?5)",
                )?
                .execute(params![
                    hash,
                    user.as_str(),
                    method.as_str(),
                    to_millis(now),
                    to_millis(expires_at)
                ]);
            match inserted {
                Ok(_) => {
                    conn.prepare_cached("UPDATE users SET last_sign_in_at = ?2 WHERE id = ?1")?
                        .execute(params![user.as_str(), to_millis(now)])?;
                    Ok(())
                }
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
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM web_sessions WHERE token_hash = ?1 AND expires_at > ?2"
            ))?;
            query_opt(&mut stmt, params![hash, to_millis(now)], from_row)
        })
        .await
    }

    /// `user`'s sessions that haven't expired at `now`, newest first. Never another user's.
    pub async fn web_sessions_of(&self, user: &UserId, now: Timestamp) -> Result<Vec<WebSession>> {
        let user = user.clone();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM web_sessions WHERE user_id = ?1 AND expires_at > ?2
                 ORDER BY created_at DESC, id"
            ))?;
            query_all(&mut stmt, params![user.as_str(), to_millis(now)], from_row)
        })
        .await
    }

    /// Deletes `user`'s session with id `id`. `false` if `user` has no such session: another
    /// user's session id is no session at all, so one user can't end another's.
    pub async fn web_session_revoke(&self, user: &UserId, id: &str) -> Result<bool> {
        let (user, id) = (user.clone(), id.to_owned());
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("DELETE FROM web_sessions WHERE user_id = ?1 AND id = ?2")?
                .execute(params![user.as_str(), id])?
                == 1)
        })
        .await
    }

    /// Records that the session for `token` was used at `now`. `false` if there is no such
    /// session.
    pub async fn web_session_seen(&self, token: &str, now: Timestamp) -> Result<bool> {
        let hash = token_hash(token);
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("UPDATE web_sessions SET last_seen_at = ?2 WHERE token_hash = ?1")?
                .execute(params![hash, to_millis(now)])?
                == 1)
        })
        .await
    }

    /// Records that the user of the session for `token` proved who they are again at `now`, with
    /// one of their own sign-in methods. `false` if there is no such session.
    pub async fn web_session_reverified(&self, token: &str, now: Timestamp) -> Result<bool> {
        let hash = token_hash(token);
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("UPDATE web_sessions SET reverified_at = ?2 WHERE token_hash = ?1")?
                .execute(params![hash, to_millis(now)])?
                == 1)
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
            .web_session_create("tok-a", owner(), SignInMethod::Passkey, at(0), at(100))
            .await
            .unwrap();
        let session = store.web_session("tok-a", at(99)).await.unwrap().unwrap();
        assert_eq!(session.id.len(), 32);
        assert_eq!(
            session,
            WebSession {
                id: session.id.clone(),
                user: owner(),
                method: SignInMethod::Passkey,
                created_at: at(0),
                expires_at: at(100),
                last_seen_at: None,
                reverified_at: None,
            }
        );
        assert_eq!(store.web_session("tok-a", at(100)).await.unwrap(), None);
        assert_eq!(store.web_session("tok-b", at(1)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn only_the_token_hash_is_stored() {
        let (_dir, store) = store().await;
        store
            .web_session_create(
                "secret-token",
                owner(),
                SignInMethod::Invite,
                at(0),
                at(100),
            )
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
            .web_session_create("t", owner(), SignInMethod::Password, at(0), at(100))
            .await
            .unwrap();
        let err = store
            .web_session_create("t", owner(), SignInMethod::Password, at(1), at(100))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        let err = store
            .web_session_create("u", owner(), SignInMethod::Password, at(5), at(5))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[tokio::test]
    async fn logout_and_expiry_delete_rows() {
        let (_dir, store) = store().await;
        store
            .web_session_create("a", owner(), SignInMethod::Oidc, at(0), at(10))
            .await
            .unwrap();
        store
            .web_session_create("b", owner(), SignInMethod::Oidc, at(0), at(20))
            .await
            .unwrap();
        store
            .web_session_create("c", owner(), SignInMethod::Oidc, at(0), at(30))
            .await
            .unwrap();

        assert!(store.web_session_delete("c").await.unwrap());
        assert!(!store.web_session_delete("c").await.unwrap());
        assert_eq!(store.web_sessions_expire(at(9)).await.unwrap(), 0);
        assert_eq!(store.web_sessions_expire(at(20)).await.unwrap(), 2);
        assert_eq!(store.web_sessions_expire(at(100)).await.unwrap(), 0);
    }

    fn user(name: &str) -> UserId {
        UserId::new(name).unwrap()
    }

    #[tokio::test]
    async fn a_user_lists_their_own_live_sessions_by_an_id_that_is_not_the_token() {
        let (_dir, store) = store().await;
        for (token, who, created, expires) in [
            ("t-old", "ann", 1, 100),
            ("t-new", "ann", 5, 100),
            ("t-gone", "ann", 2, 10),
            ("t-bob", "bob", 3, 100),
        ] {
            store
                .web_session_create(
                    token,
                    user(who),
                    SignInMethod::Password,
                    at(created),
                    at(expires),
                )
                .await
                .unwrap();
        }
        let listed = store.web_sessions_of(&user("ann"), at(20)).await.unwrap();
        let created: Vec<_> = listed.iter().map(|s| s.created_at).collect();
        assert_eq!(
            created,
            [at(5), at(1)],
            "newest first, without the expired one"
        );
        assert!(listed.iter().all(|s| s.user == user("ann")));
        for session in &listed {
            assert_eq!(session.id.len(), 32);
            assert!(session.id.bytes().all(|b| b.is_ascii_hexdigit()));
            // Not the token, and not its hash in hex.
            let hashes: Vec<String> = ["t-old", "t-new"]
                .iter()
                .map(|token| {
                    token_hash(token)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect()
                })
                .collect();
            assert!(!["t-old", "t-new"].contains(&session.id.as_str()));
            assert!(hashes.iter().all(|hash| !hash.starts_with(&session.id)));
        }
        assert_ne!(listed[0].id, listed[1].id);
        assert_eq!(
            store
                .web_sessions_of(&user("bob"), at(20))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn revoking_is_scoped_to_the_user_and_the_id() {
        let (_dir, store) = store().await;
        for (token, who) in [("a", "ann"), ("b", "bob")] {
            store
                .web_session_create(token, user(who), SignInMethod::Passkey, at(0), at(100))
                .await
                .unwrap();
        }
        let bobs = store.web_sessions_of(&user("bob"), at(1)).await.unwrap()[0]
            .id
            .clone();
        // Ann can't revoke Bob's session, and an id nobody has revokes nothing.
        assert!(!store.web_session_revoke(&user("ann"), &bobs).await.unwrap());
        assert!(
            !store
                .web_session_revoke(&user("bob"), &"0".repeat(32))
                .await
                .unwrap()
        );
        assert!(store.web_session("b", at(1)).await.unwrap().is_some());
        assert!(store.web_session_revoke(&user("bob"), &bobs).await.unwrap());
        assert!(store.web_session("b", at(1)).await.unwrap().is_none());
        assert!(store.web_session("a", at(1)).await.unwrap().is_some());
        assert!(!store.web_session_revoke(&user("bob"), &bobs).await.unwrap());
    }

    #[tokio::test]
    async fn a_session_remembers_when_it_was_seen_and_re_verified() {
        let (_dir, store) = store().await;
        store
            .web_session_create("t", owner(), SignInMethod::Oidc, at(0), at(100))
            .await
            .unwrap();
        assert!(store.web_session_seen("t", at(10)).await.unwrap());
        assert!(store.web_session_reverified("t", at(20)).await.unwrap());
        let session = store.web_session("t", at(30)).await.unwrap().unwrap();
        assert_eq!(session.last_seen_at, Some(at(10)));
        assert_eq!(session.reverified_at, Some(at(20)));
        assert!(!store.web_session_seen("nope", at(10)).await.unwrap());
        assert!(!store.web_session_reverified("nope", at(10)).await.unwrap());
    }
}
