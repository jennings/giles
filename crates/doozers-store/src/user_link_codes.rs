//! `user_link_codes`: the codes a user sends from a channel identity to prove it is theirs (PLAN
//! 3.2).
//!
//! A code is for one user and one identity namespace, and a user has at most one per namespace:
//! [`Store::user_link_code_create`] replaces the one they had. The store keeps only
//! SHA-256(salt ‖ code), with a salt of its own for each code, so the code can't be read from
//! the database. That is all the hash guarantees: the code has 40 bits, so someone with a copy of the
//! database could find it by brute force in minutes. A backup is safe because a code expires within
//! 10 minutes. What keeps a code safe is how little it lives and how few guesses it gets: it works
//! until its expiry, once, and not after `max_attempts` wrong codes have been sent while it was
//! pending.
//!
//! [`Store::user_link_code_redeem`] does everything one `/link <code>` message needs in one
//! transaction: it finds the code, adds the identity to the code's user, and deletes the code, or
//! counts the wrong code against every pending code in the sender's namespace. So two messages with the same code
//! can't both link, and a code is never used up by a link that failed.

use doozers_types::{Timestamp, UserId};
use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::time::to_millis;
use crate::users::{add_identity, find};
use crate::{
    Error, NewIdentity, Result, Store, StoredUser, Verification, get_time, parse_id, query_all,
};

/// How long a link code's salt is, in bytes.
pub const LINK_CODE_SALT_LEN: usize = 16;

/// A user's link code for a namespace, as the page that made it shows it. The code itself isn't
/// stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCodeRow {
    /// The identity namespace the code is for.
    pub namespace: String,
    /// When it was made.
    pub created_at: Timestamp,
    /// When it stops working.
    pub expires_at: Timestamp,
    /// How many wrong codes were sent while it was pending.
    pub attempts: u32,
}

/// What [`Store::user_link_code_redeem`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkRedeemed {
    /// The code was right: the identity now belongs to the user, who is returned as they are now.
    /// The code is gone.
    Linked(StoredUser),
    /// No pending code matched the code, the namespace, and an active user. The wrong code was
    /// counted against every pending code in the namespace.
    Refused,
}

fn code_hash(salt: &[u8], code: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(code.as_bytes());
    hasher.finalize().to_vec()
}

impl Store {
    /// Stores the link code `code` for `user` and `namespace`, made at `now` and good until
    /// `expires_at`, replacing the code the user had for the namespace. `salt` is random and
    /// belongs to this code alone.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`]; [`Error::InvalidRequest`] unless `expires_at` is after `now`, or if
    /// the namespace or the code is empty.
    pub async fn user_link_code_create(
        &self,
        user: &UserId,
        namespace: &str,
        code: &str,
        salt: [u8; LINK_CODE_SALT_LEN],
        now: Timestamp,
        expires_at: Timestamp,
    ) -> Result<()> {
        if expires_at <= now {
            return Err(Error::invalid("a link code must expire after it's made"));
        }
        if namespace.is_empty() || code.is_empty() {
            return Err(Error::invalid("a link code needs a namespace and a code"));
        }
        let hash = code_hash(&salt, code);
        let user = user.clone();
        let namespace = namespace.to_owned();
        self.write(move |conn| {
            if !conn
                .prepare_cached("SELECT 1 FROM users WHERE id = ?1")?
                .exists([user.as_str()])?
            {
                return Err(Error::NoSuchUser);
            }
            conn.prepare_cached(
                "INSERT OR REPLACE INTO user_link_codes
                     (user_id, namespace, salt, code_hash, created_at, expires_at, attempts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
            )?
            .execute(params![
                user.as_str(),
                namespace,
                salt.as_slice(),
                hash,
                to_millis(now),
                to_millis(expires_at)
            ])?;
            Ok(())
        })
        .await
    }

    /// The link codes `user` has made that haven't been used, expired ones included, by
    /// namespace.
    pub async fn user_link_codes(&self, user: &UserId) -> Result<Vec<LinkCodeRow>> {
        let user = user.clone();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT namespace, created_at, expires_at, attempts FROM user_link_codes
                 WHERE user_id = ?1 ORDER BY namespace",
            )?;
            query_all(&mut stmt, [user.as_str()], |row| {
                Ok(LinkCodeRow {
                    namespace: row.get(0)?,
                    created_at: get_time(row, 1)?,
                    expires_at: get_time(row, 2)?,
                    attempts: u32::try_from(row.get::<_, i64>(3)?)
                        .map_err(|_| Error::corrupt("negative link code attempts"))?,
                })
            })
        })
        .await
    }

    /// Handles a `/link <code>` message: the sender is the identity `native_id` in `namespace`.
    ///
    /// A pending code that matches `code` and `namespace`, belongs to an active user, and has
    /// had fewer than `max_attempts` wrong codes sent against it links the identity to that user,
    /// verified as [`Verification::Link`], and is deleted, all in one transaction. Otherwise
    /// nothing is linked and one wrong attempt is counted against every pending code in `namespace`,
    /// since a wrong code can't be told apart from a guess at any of them. A code for another
    /// namespace can't be what the sender guessed at, so it isn't counted: an identity on one
    /// channel can't use up the codes of another.
    ///
    /// # Errors
    ///
    /// [`Error::IdentityTaken`] if the code was right but a user has the identity already; nothing
    /// changes and the code stays.
    pub async fn user_link_code_redeem(
        &self,
        namespace: &str,
        native_id: &str,
        code: &str,
        now: Timestamp,
        max_attempts: u32,
    ) -> Result<LinkRedeemed> {
        let namespace = namespace.to_owned();
        let native_id = native_id.to_owned();
        let code = code.to_owned();
        self.write(move |conn| {
            let now_ms = to_millis(now);
            let pending: Vec<(String, String, Vec<u8>, Vec<u8>)> = {
                let mut stmt = conn.prepare_cached(
                    "SELECT c.user_id, c.namespace, c.salt, c.code_hash
                     FROM user_link_codes c JOIN users u ON u.id = c.user_id
                     WHERE c.expires_at > ?1 AND c.attempts < ?2 AND u.state = 'active'",
                )?;
                query_all(&mut stmt, params![now_ms, max_attempts], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
            };
            let found = pending
                .into_iter()
                .find(|(_, ns, salt, hash)| *ns == namespace && code_hash(salt, &code) == *hash);
            let Some((user, ..)) = found else {
                conn.prepare_cached(
                    "UPDATE user_link_codes SET attempts = attempts + 1
                     WHERE expires_at > ?1 AND attempts < ?2 AND namespace = ?3",
                )?
                .execute(params![now_ms, max_attempts, namespace])?;
                return Ok(LinkRedeemed::Refused);
            };
            let user = parse_id(user, UserId::new)?;
            add_identity(
                conn,
                &user,
                &NewIdentity {
                    namespace: namespace.clone(),
                    native_id,
                    verification: Verification::Link,
                },
                now,
            )?;
            conn.prepare_cached(
                "DELETE FROM user_link_codes WHERE user_id = ?1 AND namespace = ?2",
            )?
            .execute(params![user.as_str(), namespace])?;
            let linked = find(conn, &user)?
                .ok_or_else(|| Error::corrupt("the user just linked is missing"))?;
            Ok(LinkRedeemed::Linked(linked))
        })
        .await
    }

    /// Deletes every link code that expired at or before `cutoff`; returns how many.
    pub async fn user_link_codes_prune(&self, cutoff: Timestamp) -> Result<u64> {
        self.write(move |conn| {
            let deleted = conn
                .prepare_cached("DELETE FROM user_link_codes WHERE expires_at <= ?1")?
                .execute([to_millis(cutoff)])?;
            Ok(deleted as u64)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::{at, store};
    use crate::{NewUser, Role, UserState};

    fn id(text: &str) -> UserId {
        UserId::new(text).unwrap()
    }

    const SALT: [u8; LINK_CODE_SALT_LEN] = [7; LINK_CODE_SALT_LEN];

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

    async fn make(store: &Store, user: &str, namespace: &str, code: &str) {
        store
            .user_link_code_create(&id(user), namespace, code, SALT, at(0), at(600))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_right_code_links_the_identity_once() {
        let (_dir, store) = with_users().await;
        make(&store, "bob", "whatsapp", "ABCD2345").await;
        let Ok(LinkRedeemed::Linked(bob)) = store
            .user_link_code_redeem("whatsapp", "+15551234567", "ABCD2345", at(10), 5)
            .await
        else {
            panic!("the right code didn't link");
        };
        assert_eq!(bob.id, id("bob"));
        assert_eq!(bob.identities.len(), 1);
        assert_eq!(bob.identities[0].text(), "whatsapp:+15551234567");
        assert_eq!(bob.identities[0].verification, Verification::Link);
        assert_eq!(bob.identities[0].verified_at, at(10));
        // The first identity is the default.
        assert_eq!(
            bob.default_identity.as_deref(),
            Some("whatsapp:+15551234567")
        );
        // The code is gone, so a second message with it is refused and the page lists no code.
        assert!(store.user_link_codes(&id("bob")).await.unwrap().is_empty());
        assert_eq!(
            store
                .user_link_code_redeem("whatsapp", "+15557654321", "ABCD2345", at(11), 5)
                .await
                .unwrap(),
            LinkRedeemed::Refused
        );
    }

    #[tokio::test]
    async fn only_the_hash_is_stored() {
        let (_dir, store) = with_users().await;
        make(&store, "bob", "whatsapp", "ABCD2345").await;
        let text: String = store
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT hex(salt) || hex(code_hash) FROM user_link_codes",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert!(!text.contains("ABCD2345"));
        assert_eq!(text.len(), 2 * (LINK_CODE_SALT_LEN + 32));
        // The salt is part of the hash: the same code with another salt hashes differently.
        assert_ne!(
            code_hash(&[1; 16], "ABCD2345"),
            code_hash(&[2; 16], "ABCD2345")
        );
    }

    #[tokio::test]
    async fn a_wrong_expired_or_misplaced_code_links_nothing() {
        let (_dir, store) = with_users().await;
        make(&store, "bob", "whatsapp", "ABCD2345").await;
        let redeem = |namespace: &'static str, code: &'static str, secs: i64| {
            let store = store.clone();
            async move {
                store
                    .user_link_code_redeem(namespace, "+15551234567", code, at(secs), 100)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            redeem("whatsapp", "ZZZZ2222", 10).await,
            LinkRedeemed::Refused
        );
        // The right code in another namespace, and one second at or after the expiry.
        assert_eq!(
            redeem("discord", "ABCD2345", 10).await,
            LinkRedeemed::Refused
        );
        assert_eq!(
            redeem("whatsapp", "ABCD2345", 600).await,
            LinkRedeemed::Refused
        );
        assert_eq!(
            redeem("whatsapp", "abcd2345", 10).await,
            LinkRedeemed::Refused
        );
        let bob = store.user(&id("bob")).await.unwrap().unwrap();
        assert!(bob.identities.is_empty());
        // The code still works just before it expires.
        assert!(matches!(
            redeem("whatsapp", "ABCD2345", 599).await,
            LinkRedeemed::Linked(_)
        ));
    }

    #[tokio::test]
    async fn wrong_codes_count_against_every_pending_code_in_the_namespace_until_the_limit() {
        let (_dir, store) = with_users().await;
        make(&store, "ann", "whatsapp", "AAAA2222").await;
        make(&store, "bob", "discord", "BBBB3333").await;
        make(&store, "bob", "whatsapp", "DDDD5555").await;
        for n in 0..3 {
            let got = store
                .user_link_code_redeem("whatsapp", "+1555", "WRONG222", at(n), 3)
                .await
                .unwrap();
            assert_eq!(got, LinkRedeemed::Refused);
        }
        let attempts = |user: &'static str, namespace: &'static str| {
            let store = store.clone();
            async move {
                let codes = store.user_link_codes(&id(user)).await.unwrap();
                codes
                    .iter()
                    .find(|c| c.namespace == namespace)
                    .unwrap()
                    .attempts
            }
        };
        // Both codes of the namespace counted the guesses; the discord code didn't.
        assert_eq!(
            (
                attempts("ann", "whatsapp").await,
                attempts("bob", "whatsapp").await
            ),
            (3, 3)
        );
        assert_eq!(attempts("bob", "discord").await, 0);
        // At the limit a code is dead, even the right one, and the count stops.
        let got = store
            .user_link_code_redeem("whatsapp", "+1555", "AAAA2222", at(5), 3)
            .await
            .unwrap();
        assert_eq!(got, LinkRedeemed::Refused);
        assert_eq!(attempts("ann", "whatsapp").await, 3);
        // A new code replaces the dead one.
        make(&store, "ann", "whatsapp", "CCCC4444").await;
        assert_eq!(attempts("ann", "whatsapp").await, 0);
        assert!(matches!(
            store
                .user_link_code_redeem("whatsapp", "+1555", "CCCC4444", at(6), 3)
                .await
                .unwrap(),
            LinkRedeemed::Linked(_)
        ));
    }

    #[tokio::test]
    async fn a_user_has_one_code_per_namespace_and_a_new_one_replaces_the_old() {
        let (_dir, store) = with_users().await;
        make(&store, "bob", "whatsapp", "AAAA2222").await;
        make(&store, "bob", "discord", "BBBB3333").await;
        make(&store, "bob", "whatsapp", "CCCC4444").await;
        let rows = store.user_link_codes(&id("bob")).await.unwrap();
        let namespaces: Vec<_> = rows.iter().map(|r| r.namespace.as_str()).collect();
        assert_eq!(namespaces, ["discord", "whatsapp"]);
        assert_eq!(
            store
                .user_link_code_redeem("whatsapp", "+1555", "AAAA2222", at(1), 5)
                .await
                .unwrap(),
            LinkRedeemed::Refused,
            "the replaced code is dead"
        );
        assert!(matches!(
            store
                .user_link_code_redeem("whatsapp", "+1555", "CCCC4444", at(2), 5)
                .await
                .unwrap(),
            LinkRedeemed::Linked(_)
        ));
    }

    #[tokio::test]
    async fn a_code_cant_claim_an_identity_another_user_has() {
        let (_dir, store) = with_users().await;
        store
            .user_identity_add(
                &id("ann"),
                NewIdentity {
                    namespace: "whatsapp".into(),
                    native_id: "+15551234567".into(),
                    verification: Verification::Imported,
                },
                at(0),
            )
            .await
            .unwrap();
        make(&store, "bob", "whatsapp", "ABCD2345").await;
        let err = store
            .user_link_code_redeem("whatsapp", "+15551234567", "ABCD2345", at(1), 5)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::IdentityTaken { .. }), "{err}");
        // Nothing changed: the code is still there and still unused.
        let rows = store.user_link_codes(&id("bob")).await.unwrap();
        assert_eq!((rows.len(), rows[0].attempts), (1, 0));
        assert!(
            store
                .user(&id("bob"))
                .await
                .unwrap()
                .unwrap()
                .identities
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_disabled_user_cant_link_and_removing_a_user_removes_their_codes() {
        let (_dir, store) = with_users().await;
        make(&store, "bob", "whatsapp", "ABCD2345").await;
        store
            .user_set_state(&id("bob"), UserState::Disabled, at(1))
            .await
            .unwrap();
        assert_eq!(
            store
                .user_link_code_redeem("whatsapp", "+1555", "ABCD2345", at(2), 5)
                .await
                .unwrap(),
            LinkRedeemed::Refused
        );
        store.user_delete(&id("bob")).await.unwrap();
        assert!(store.user_link_codes(&id("bob")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pruning_deletes_codes_that_expired_before_the_cutoff() {
        let (_dir, store) = with_users().await;
        make(&store, "ann", "whatsapp", "AAAA2222").await;
        store
            .user_link_code_create(&id("bob"), "whatsapp", "BBBB3333", SALT, at(0), at(5000))
            .await
            .unwrap();
        assert_eq!(store.user_link_codes_prune(at(599)).await.unwrap(), 0);
        assert_eq!(store.user_link_codes_prune(at(600)).await.unwrap(), 1);
        assert_eq!(store.user_link_codes(&id("ann")).await.unwrap(), vec![]);
        assert_eq!(store.user_link_codes(&id("bob")).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn making_a_code_checks_its_arguments() {
        let (_dir, store) = with_users().await;
        let create = async |user: &str, namespace: &str, code: &str, expires: i64| {
            store
                .user_link_code_create(&id(user), namespace, code, SALT, at(0), at(expires))
                .await
        };
        assert!(matches!(
            create("nobody", "whatsapp", "ABCD2345", 600).await,
            Err(Error::NoSuchUser)
        ));
        for (namespace, code, expires) in [
            ("", "ABCD2345", 600),
            ("whatsapp", "", 600),
            ("whatsapp", "A", 0),
        ] {
            assert!(matches!(
                create("bob", namespace, code, expires).await,
                Err(Error::InvalidRequest(_))
            ));
        }
    }
}
