//! `user_invites`: one-time links that sign a user in (PLAN 3.4).
//!
//! An [`Invite`](InviteKind::Invite) sets up a new account and a [`Recovery`](InviteKind::Recovery)
//! link lets a user who lost every sign-in method back in. The store keeps only the SHA-256 of
//! each token, so a copy of the database (a backup) can't be used to sign in. The token is random
//! with 256 bits of entropy (the core generates it), so a plain hash needs no salt or slow KDF.
//!
//! A link works until its expiry and once: [`Store::user_invite_redeem`] marks it used in the one
//! statement that finds it, so two requests with the same token can't both succeed.

use doozers_types::{Timestamp, UserId};
use rusqlite::{ErrorCode, params};

use crate::time::to_millis;
use crate::web_sessions::token_hash;
use crate::{Error, Result, Store, get_time, parse_id, query_opt};

/// What a one-time link is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteKind {
    /// Sets up a new account.
    Invite,
    /// Gets a user who lost every sign-in method back in. Existing methods stay.
    Recovery,
}

impl InviteKind {
    /// The stored name.
    pub const fn as_str(self) -> &'static str {
        match self {
            InviteKind::Invite => "invite",
            InviteKind::Recovery => "recovery",
        }
    }

    fn parse(text: &str) -> Result<Self> {
        match text {
            "invite" => Ok(InviteKind::Invite),
            "recovery" => Ok(InviteKind::Recovery),
            other => Err(Error::corrupt(format!("user invite kind {other:?}"))),
        }
    }
}

/// A one-time link, as the store holds it: who it is for, what for, who made it, and until when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInvite {
    /// The user it signs in.
    pub user: UserId,
    /// What it is for.
    pub kind: InviteKind,
    /// The admin who made it; `None` for the operator's `doozers admin login-link`, or when that
    /// admin is gone.
    pub created_by: Option<UserId>,
    /// When it was made.
    pub created_at: Timestamp,
    /// When it stops working.
    pub expires_at: Timestamp,
}

const COLUMNS: &str = "user_id, kind, created_by, created_at, expires_at";

fn invite_from_row(row: &rusqlite::Row<'_>) -> Result<UserInvite> {
    Ok(UserInvite {
        user: parse_id(row.get(0)?, UserId::new)?,
        kind: InviteKind::parse(&row.get::<_, String>(1)?)?,
        created_by: row
            .get::<_, Option<String>>(2)?
            .map(|id| parse_id(id, UserId::new))
            .transpose()?,
        created_at: get_time(row, 3)?,
        expires_at: get_time(row, 4)?,
    })
}

impl Store {
    /// Stores a one-time link for `user` for the token `token`, made at `now` by `created_by`
    /// (`None`: the operator) and good until `expires_at`.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`] if `user` or `created_by` doesn't exist; [`Error::Conflict`] if the
    /// token exists; [`Error::InvalidRequest`] unless `expires_at` is after `now`.
    pub async fn user_invite_create(
        &self,
        token: &str,
        user: &UserId,
        kind: InviteKind,
        created_by: Option<&UserId>,
        now: Timestamp,
        expires_at: Timestamp,
    ) -> Result<()> {
        if expires_at <= now {
            return Err(Error::invalid("a user invite must expire after it's made"));
        }
        let hash = token_hash(token);
        let user = user.clone();
        let created_by = created_by.cloned();
        self.write(move |conn| {
            let exists = |id: &UserId| -> Result<bool> {
                Ok(conn
                    .prepare_cached("SELECT 1 FROM users WHERE id = ?1")?
                    .exists([id.as_str()])?)
            };
            if !exists(&user)? {
                return Err(Error::NoSuchUser);
            }
            if let Some(by) = &created_by
                && !exists(by)?
            {
                return Err(Error::NoSuchUser);
            }
            let inserted = conn
                .prepare_cached(
                    "INSERT INTO user_invites
                         (token_hash, user_id, kind, created_by, created_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?
                .execute(params![
                    hash,
                    user.as_str(),
                    kind.as_str(),
                    created_by.as_ref().map(UserId::as_str),
                    to_millis(now),
                    to_millis(expires_at),
                ]);
            match inserted {
                Ok(_) => Ok(()),
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if e.code == ErrorCode::ConstraintViolation =>
                {
                    Err(Error::conflict("a user invite with this token exists"))
                }
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// The link for `token`, if it can sign someone in at `now`: it exists, hasn't been used, and
    /// hasn't expired (`expires_at <= now`). Asking doesn't use it up.
    pub async fn user_invite(&self, token: &str, now: Timestamp) -> Result<Option<UserInvite>> {
        let hash = token_hash(token);
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM user_invites
                 WHERE token_hash = ?1 AND used_at IS NULL AND expires_at > ?2"
            ))?;
            query_opt(&mut stmt, params![hash, to_millis(now)], invite_from_row)
        })
        .await
    }

    /// Uses up the link for `token` at `now` and returns it, or `None` if it doesn't exist, was
    /// used, or expired. Of any number of calls with the same token, at most one gets the link.
    pub async fn user_invite_redeem(
        &self,
        token: &str,
        now: Timestamp,
    ) -> Result<Option<UserInvite>> {
        let hash = token_hash(token);
        self.write(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "UPDATE user_invites SET used_at = ?2
                 WHERE token_hash = ?1 AND used_at IS NULL AND expires_at > ?2
                 RETURNING {COLUMNS}"
            ))?;
            query_opt(&mut stmt, params![hash, to_millis(now)], invite_from_row)
        })
        .await
    }

    /// Deletes every link that has expired (`expires_at <= now`), used or not; returns how many.
    pub async fn user_invites_expire(&self, now: Timestamp) -> Result<u64> {
        self.write(move |conn| {
            let deleted = conn
                .prepare_cached("DELETE FROM user_invites WHERE expires_at <= ?1")?
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
    async fn a_link_works_once_until_it_expires() {
        let (_dir, store) = with_users().await;
        store
            .user_invite_create(
                "tok",
                &id("bob"),
                InviteKind::Invite,
                Some(&id("ann")),
                at(10),
                at(100),
            )
            .await
            .unwrap();
        let expected = UserInvite {
            user: id("bob"),
            kind: InviteKind::Invite,
            created_by: Some(id("ann")),
            created_at: at(10),
            expires_at: at(100),
        };
        assert_eq!(
            store.user_invite("tok", at(99)).await.unwrap(),
            Some(expected.clone())
        );
        assert_eq!(
            store.user_invite("tok", at(99)).await.unwrap(),
            Some(expected.clone()),
            "asking doesn't use it up"
        );
        assert_eq!(store.user_invite("other", at(99)).await.unwrap(), None);
        assert_eq!(
            store.user_invite_redeem("tok", at(99)).await.unwrap(),
            Some(expected)
        );
        assert_eq!(store.user_invite("tok", at(99)).await.unwrap(), None);
        assert_eq!(store.user_invite_redeem("tok", at(99)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_expired_link_can_be_neither_seen_nor_used() {
        let (_dir, store) = with_users().await;
        store
            .user_invite_create(
                "tok",
                &id("bob"),
                InviteKind::Recovery,
                None,
                at(0),
                at(100),
            )
            .await
            .unwrap();
        assert_eq!(store.user_invite("tok", at(100)).await.unwrap(), None);
        assert_eq!(
            store.user_invite_redeem("tok", at(100)).await.unwrap(),
            None
        );
        // It was never used, so a clock that goes back finds it again.
        let found = store.user_invite("tok", at(99)).await.unwrap().unwrap();
        assert_eq!(found.kind, InviteKind::Recovery);
        assert_eq!(found.created_by, None);
    }

    #[tokio::test]
    async fn only_one_of_many_redeems_wins() {
        let (_dir, store) = with_users().await;
        store
            .user_invite_create("tok", &id("bob"), InviteKind::Invite, None, at(0), at(100))
            .await
            .unwrap();
        let attempts = (0..8).map(|_| {
            let store = store.clone();
            tokio::spawn(async move { store.user_invite_redeem("tok", at(1)).await.unwrap() })
        });
        let mut won = 0;
        for attempt in attempts.collect::<Vec<_>>() {
            won += usize::from(attempt.await.unwrap().is_some());
        }
        assert_eq!(won, 1);
    }

    #[tokio::test]
    async fn only_the_token_hash_is_stored() {
        let (_dir, store) = with_users().await;
        store
            .user_invite_create(
                "secret-token",
                &id("bob"),
                InviteKind::Invite,
                None,
                at(0),
                at(100),
            )
            .await
            .unwrap();
        let (stored, matches_raw) = store
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT token_hash, token_hash = CAST('secret-token' AS BLOB)
                     FROM user_invites",
                    [],
                    |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, bool>(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(stored, token_hash("secret-token"));
        assert!(!matches_raw);
    }

    #[tokio::test]
    async fn bad_links_are_refused() {
        let (_dir, store) = with_users().await;
        let create = |token: &'static str, user: &str, by: Option<&str>, expires: i64| {
            let store = store.clone();
            let user = id(user);
            let by = by.map(id);
            async move {
                store
                    .user_invite_create(
                        token,
                        &user,
                        InviteKind::Invite,
                        by.as_ref(),
                        at(5),
                        at(expires),
                    )
                    .await
            }
        };
        assert!(matches!(
            create("a", "nobody", None, 10).await,
            Err(Error::NoSuchUser)
        ));
        assert!(matches!(
            create("a", "bob", Some("nobody"), 10).await,
            Err(Error::NoSuchUser)
        ));
        assert!(matches!(
            create("a", "bob", None, 5).await,
            Err(Error::InvalidRequest(_))
        ));
        create("a", "bob", None, 10).await.unwrap();
        assert!(matches!(
            create("a", "ann", None, 10).await,
            Err(Error::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn removing_a_user_removes_their_links_and_forgets_who_made_others() {
        let (_dir, store) = with_users().await;
        store
            .user_invite_create(
                "for-ann",
                &id("ann"),
                InviteKind::Recovery,
                None,
                at(0),
                at(100),
            )
            .await
            .unwrap();
        store
            .user_invite_create(
                "by-bob",
                &id("ann"),
                InviteKind::Recovery,
                Some(&id("bob")),
                at(0),
                at(100),
            )
            .await
            .unwrap();
        store
            .user_invite_create(
                "for-bob",
                &id("bob"),
                InviteKind::Invite,
                Some(&id("ann")),
                at(0),
                at(100),
            )
            .await
            .unwrap();
        store.user_delete(&id("bob")).await.unwrap();
        assert_eq!(store.user_invite("for-bob", at(1)).await.unwrap(), None);
        let kept = store.user_invite("by-bob", at(1)).await.unwrap().unwrap();
        assert_eq!(kept.created_by, None);
        assert!(store.user_invite("for-ann", at(1)).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn expiring_deletes_expired_links_used_or_not() {
        let (_dir, store) = with_users().await;
        for (token, expires) in [("a", 10), ("b", 20), ("c", 30)] {
            store
                .user_invite_create(
                    token,
                    &id("bob"),
                    InviteKind::Invite,
                    None,
                    at(0),
                    at(expires),
                )
                .await
                .unwrap();
        }
        store.user_invite_redeem("a", at(1)).await.unwrap();
        assert_eq!(store.user_invites_expire(at(9)).await.unwrap(), 0);
        assert_eq!(store.user_invites_expire(at(20)).await.unwrap(), 2);
        assert!(store.user_invite("c", at(21)).await.unwrap().is_some());
        assert_eq!(store.user_invites_expire(at(100)).await.unwrap(), 1);
    }
}
