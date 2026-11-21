//! Removing a sign-in method: a user's password, one of their passkeys, or their connection to
//! an identity provider. Each lives in its own table (`user_passwords`, `user_passkeys`,
//! `user_oidc_connections`); what they share is that a user keeps at least one that works.
//!
//! The rule is checked in the transaction that removes, so two removals at once can't each see
//! the other's method and take both. A method counts when it can sign the user in now: the
//! password always, a passkey only where the caller says passkeys work (they don't where the
//! install has no relying party: no `public_url`, an IP address, or plain `http` off `localhost`),
//! and a connection only while its provider is enabled and still has the issuer the connection was
//! made under (changing a provider's issuer, or disabling it, stops its connections from signing
//! anyone in). Removing a method that can't sign in now is
//! always allowed, because it takes nothing away. A user who has no method at all, such as one just
//! invited, is never asked to keep one. The trusted proxy's identity isn't a method: it is the
//! admin's, not the user's, and works only where a proxy is set up.

use doozers_types::UserId;
use rusqlite::{Connection, params};

use crate::user_passkeys::PasskeyId;
use crate::{Error, Result, Store};

/// One of a user's sign-in methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInMethodRef {
    /// The user's password.
    Password,
    /// The user's passkey with this row id.
    Passkey(PasskeyId),
    /// The user's connection to the identity provider with this id.
    Oidc {
        /// The provider's id.
        provider: String,
    },
}

/// How many of the user's methods can sign them in now. Passkeys count only if `passkeys_work`.
fn usable(conn: &Connection, user: &UserId, passkeys_work: bool) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "SELECT
                 (SELECT count(*) FROM user_passwords WHERE user_id = ?1)
               + (SELECT count(*) FROM user_passkeys WHERE user_id = ?1 AND ?2)
               + (SELECT count(*) FROM user_oidc_connections c
                  JOIN oidc_providers p
                    ON p.id = c.provider_id AND p.issuer = c.issuer AND p.enabled = 1
                  WHERE c.user_id = ?1)",
        )?
        .query_row(params![user.as_str(), passkeys_work], |row| row.get(0))?)
}

impl Store {
    /// How many of `user`'s sign-in methods can sign them in now: what
    /// [`user_sign_in_method_remove`](Self::user_sign_in_method_remove) counts to keep one.
    /// Passkeys count only if `passkeys_work`.
    pub async fn user_sign_in_methods_usable(
        &self,
        user: &UserId,
        passkeys_work: bool,
    ) -> Result<u64> {
        let user = user.clone();
        self.read(move |conn| {
            Ok(u64::try_from(usable(conn, &user, passkeys_work)?).unwrap_or_default())
        })
        .await
    }

    /// Removes `method` of `user`. `false` if the user doesn't have it: another user's passkey id
    /// is no passkey of this user's. `passkeys_work` says whether passkeys can sign anyone in on
    /// this install, which the store can't know: when it is `false` they don't count as methods to
    /// keep.
    ///
    /// # Errors
    ///
    /// [`Error::LastSignInMethod`] if `method` can sign the user in and is the only one that can.
    /// Nothing changes.
    pub async fn user_sign_in_method_remove(
        &self,
        user: &UserId,
        method: &SignInMethodRef,
        passkeys_work: bool,
    ) -> Result<bool> {
        let (user, method) = (user.clone(), method.clone());
        self.write(move |conn| {
            // Whether the method exists, and whether it counts towards what the user keeps.
            let (exists, counts) = match &method {
                SignInMethodRef::Password => {
                    let has = conn
                        .prepare_cached("SELECT 1 FROM user_passwords WHERE user_id = ?1")?
                        .exists([user.as_str()])?;
                    (has, has)
                }
                SignInMethodRef::Passkey(id) => {
                    let has = conn
                        .prepare_cached(
                            "SELECT 1 FROM user_passkeys WHERE id = ?1 AND user_id = ?2",
                        )?
                        .exists(params![id, user.as_str()])?;
                    (has, has && passkeys_work)
                }
                SignInMethodRef::Oidc { provider } => {
                    let exists = conn
                        .prepare_cached(
                            "SELECT 1 FROM user_oidc_connections
                             WHERE provider_id = ?1 AND user_id = ?2",
                        )?
                        .exists(params![provider, user.as_str()])?;
                    let counts = conn
                        .prepare_cached(
                            "SELECT 1 FROM user_oidc_connections c
                             JOIN oidc_providers p
                               ON p.id = c.provider_id AND p.issuer = c.issuer AND p.enabled = 1
                             WHERE c.provider_id = ?1 AND c.user_id = ?2",
                        )?
                        .exists(params![provider, user.as_str()])?;
                    (exists, counts)
                }
            };
            if !exists {
                return Ok(false);
            }
            if counts && usable(conn, &user, passkeys_work)? <= 1 {
                return Err(Error::LastSignInMethod);
            }
            match &method {
                SignInMethodRef::Password => {
                    conn.prepare_cached("DELETE FROM user_passwords WHERE user_id = ?1")?
                        .execute([user.as_str()])?;
                }
                SignInMethodRef::Passkey(id) => {
                    conn.prepare_cached(
                        "DELETE FROM user_passkeys WHERE id = ?1 AND user_id = ?2",
                    )?
                    .execute(params![id, user.as_str()])?;
                }
                SignInMethodRef::Oidc { provider } => {
                    conn.prepare_cached(
                        "DELETE FROM user_oidc_connections
                         WHERE provider_id = ?1 AND user_id = ?2",
                    )?
                    .execute(params![provider, user.as_str()])?;
                }
            }
            Ok(true)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::tests::{at, store};
    use crate::{NewPasskey, NewUser, OidcProviderSettings, PasskeyKind, Role};

    const ISSUER: &str = "https://idp.example.com";

    fn id(text: &str) -> UserId {
        UserId::new(text).unwrap()
    }

    async fn with_users() -> (tempfile::TempDir, Store) {
        let (dir, store) = store().await;
        for user in ["ann", "bob"] {
            let new = NewUser {
                id: id(user),
                username: user.to_owned(),
                display_name: user.to_uppercase(),
                role: Role::Member,
                timezone: None,
                default_identity: None,
            };
            store.user_create(new, vec![], at(0)).await.unwrap();
        }
        store
            .oidc_provider_add(
                "idp",
                OidcProviderSettings {
                    display_name: "Idp".to_owned(),
                    issuer: ISSUER.to_owned(),
                    client_id: "client".to_owned(),
                    client_secret: None,
                    scopes: vec!["openid".to_owned()],
                    enabled: true,
                },
                at(0),
            )
            .await
            .unwrap();
        (dir, store)
    }

    async fn passkey(store: &Store, user: &str, credential: u8) -> crate::StoredPasskey {
        store
            .user_passkey_add(
                NewPasskey {
                    user: id(user),
                    credential_id: vec![credential],
                    credential: "{}".to_owned(),
                    kind: PasskeyKind::Discoverable,
                    name: format!("key {credential}"),
                },
                at(1),
            )
            .await
            .unwrap()
    }

    async fn remove(store: &Store, user: &str, method: SignInMethodRef) -> Result<bool> {
        store
            .user_sign_in_method_remove(&id(user), &method, true)
            .await
    }

    fn oidc() -> SignInMethodRef {
        SignInMethodRef::Oidc {
            provider: "idp".to_owned(),
        }
    }

    #[tokio::test]
    async fn any_method_goes_but_the_last_one_that_works_stays() {
        let (_dir, store) = with_users().await;
        store
            .user_password_set(&id("ann"), "$argon2id$x", at(1))
            .await
            .unwrap();
        let key = passkey(&store, "ann", 1).await;
        store
            .user_oidc_connect("idp", ISSUER, "sub-ann", &id("ann"), at(1))
            .await
            .unwrap();

        assert!(remove(&store, "ann", oidc()).await.unwrap());
        assert!(!remove(&store, "ann", oidc()).await.unwrap());
        assert!(
            remove(&store, "ann", SignInMethodRef::Password)
                .await
                .unwrap()
        );
        assert!(
            !remove(&store, "ann", SignInMethodRef::Password)
                .await
                .unwrap()
        );
        let err = remove(&store, "ann", SignInMethodRef::Passkey(key.id))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LastSignInMethod), "{err}");
        assert_eq!(store.user_passkeys(&id("ann")).await.unwrap().len(), 1);

        // A second method makes the first removable again.
        store
            .user_password_set(&id("ann"), "$argon2id$y", at(2))
            .await
            .unwrap();
        assert!(
            remove(&store, "ann", SignInMethodRef::Passkey(key.id))
                .await
                .unwrap()
        );
        let err = remove(&store, "ann", SignInMethodRef::Password)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LastSignInMethod), "{err}");
    }

    #[tokio::test]
    async fn a_user_removes_only_their_own_methods() {
        let (_dir, store) = with_users().await;
        let anns = passkey(&store, "ann", 1).await;
        passkey(&store, "ann", 2).await;
        passkey(&store, "bob", 3).await;
        store
            .user_oidc_connect("idp", ISSUER, "sub-ann", &id("ann"), at(1))
            .await
            .unwrap();
        store
            .user_password_set(&id("ann"), "$argon2id$x", at(1))
            .await
            .unwrap();

        // Bob names Ann's passkey, and has no connection or password: nothing is his to remove.
        assert!(
            !remove(&store, "bob", SignInMethodRef::Passkey(anns.id))
                .await
                .unwrap()
        );
        assert!(
            !remove(
                &store,
                "bob",
                SignInMethodRef::Oidc {
                    provider: "idp".to_owned()
                }
            )
            .await
            .unwrap()
        );
        assert!(
            !remove(&store, "bob", SignInMethodRef::Password)
                .await
                .unwrap()
        );
        assert_eq!(store.user_passkeys(&id("ann")).await.unwrap().len(), 2);
        assert_eq!(
            store
                .user_oidc_user("idp", ISSUER, "sub-ann")
                .await
                .unwrap(),
            Some(id("ann"))
        );
        assert!(store.user_password(&id("ann")).await.unwrap().is_some());
        assert_eq!(store.user_passkeys(&id("bob")).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_connection_that_cannot_sign_in_does_not_count_and_can_always_go() {
        let (_dir, store) = with_users().await;
        store
            .user_password_set(&id("ann"), "$argon2id$x", at(1))
            .await
            .unwrap();
        store
            .user_oidc_connect("idp", ISSUER, "sub-ann", &id("ann"), at(1))
            .await
            .unwrap();
        // Disabled, the connection signs nobody in, so the password is the only way.
        store
            .oidc_provider_set_enabled("idp", false, at(2))
            .await
            .unwrap();
        let err = remove(&store, "ann", SignInMethodRef::Password)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LastSignInMethod), "{err}");
        assert!(
            remove(
                &store,
                "ann",
                SignInMethodRef::Oidc {
                    provider: "idp".to_owned()
                }
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn a_user_with_nothing_can_still_have_a_dead_method_removed() {
        let (_dir, store) = with_users().await;
        store
            .user_oidc_connect("idp", ISSUER, "sub-bob", &id("bob"), at(1))
            .await
            .unwrap();
        store
            .oidc_provider_set_enabled("idp", false, at(2))
            .await
            .unwrap();
        assert!(
            remove(
                &store,
                "bob",
                SignInMethodRef::Oidc {
                    provider: "idp".to_owned()
                }
            )
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn the_methods_that_can_sign_in_are_counted_as_removal_counts_them() {
        let (_dir, store) = with_users().await;
        let ann = id("ann");
        let count = |works: bool| store.user_sign_in_methods_usable(&ann, works);
        assert_eq!(count(true).await.unwrap(), 0);
        store
            .user_password_set(&id("ann"), "$argon2id$x", at(1))
            .await
            .unwrap();
        passkey(&store, "ann", 1).await;
        passkey(&store, "ann", 2).await;
        store
            .user_oidc_connect("idp", ISSUER, "sub-ann", &id("ann"), at(1))
            .await
            .unwrap();
        assert_eq!(count(true).await.unwrap(), 4);
        assert_eq!(count(false).await.unwrap(), 2);
        store
            .oidc_provider_set_enabled("idp", false, at(2))
            .await
            .unwrap();
        assert_eq!(count(true).await.unwrap(), 3);
        // Another user's methods are not counted.
        assert_eq!(
            store
                .user_sign_in_methods_usable(&id("bob"), true)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn passkeys_count_only_where_they_can_sign_in() {
        let (_dir, store) = with_users().await;
        store
            .user_password_set(&id("ann"), "$argon2id$x", at(1))
            .await
            .unwrap();
        let key = passkey(&store, "ann", 1).await;
        // Where passkeys don't work, the password is the only way in, so it stays.
        let err = store
            .user_sign_in_method_remove(&id("ann"), &SignInMethodRef::Password, false)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LastSignInMethod), "{err}");
        assert!(store.user_password(&id("ann")).await.unwrap().is_some());
        // A passkey that can't sign in takes nothing away, so it can go.
        assert!(
            store
                .user_sign_in_method_remove(&id("ann"), &SignInMethodRef::Passkey(key.id), false)
                .await
                .unwrap()
        );
        // Where they work, the password can go once there is a passkey.
        let key = passkey(&store, "ann", 2).await;
        assert!(
            remove(&store, "ann", SignInMethodRef::Password)
                .await
                .unwrap()
        );
        let err = remove(&store, "ann", SignInMethodRef::Passkey(key.id))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LastSignInMethod), "{err}");
    }

    #[tokio::test]
    async fn a_working_connection_counts_until_its_provider_changes_issuer() {
        let (_dir, store) = with_users().await;
        store
            .user_password_set(&id("ann"), "$argon2id$x", at(1))
            .await
            .unwrap();
        store
            .user_oidc_connect("idp", ISSUER, "sub-ann", &id("ann"), at(1))
            .await
            .unwrap();
        // The connection works, so the password can go; then the connection is the last way in.
        assert!(
            remove(&store, "ann", SignInMethodRef::Password)
                .await
                .unwrap()
        );
        let err = remove(&store, "ann", oidc()).await.unwrap_err();
        assert!(matches!(err, Error::LastSignInMethod), "{err}");

        // With the password back, an admin moves the provider to another issuer: the old
        // connection signs nobody in, so the password is the last way in, and the dead
        // connection can go.
        store
            .user_password_set(&id("ann"), "$argon2id$y", at(2))
            .await
            .unwrap();
        store
            .oidc_provider_update(
                "idp",
                OidcProviderSettings {
                    display_name: "Idp".to_owned(),
                    issuer: "https://other.example.com".to_owned(),
                    client_id: "client".to_owned(),
                    client_secret: None,
                    scopes: vec!["openid".to_owned()],
                    enabled: true,
                },
                at(3),
            )
            .await
            .unwrap();
        let err = remove(&store, "ann", SignInMethodRef::Password)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::LastSignInMethod), "{err}");
        assert!(remove(&store, "ann", oidc()).await.unwrap());
    }

    #[tokio::test]
    async fn removing_a_connection_leaves_the_other_users_of_the_provider() {
        let (_dir, store) = with_users().await;
        for user in ["ann", "bob"] {
            store
                .user_password_set(&id(user), "$argon2id$x", at(1))
                .await
                .unwrap();
            store
                .user_oidc_connect("idp", ISSUER, &format!("sub-{user}"), &id(user), at(1))
                .await
                .unwrap();
        }
        assert!(remove(&store, "bob", oidc()).await.unwrap());
        assert!(!remove(&store, "bob", oidc()).await.unwrap());
        assert_eq!(
            store
                .user_oidc_user("idp", ISSUER, "sub-bob")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .user_oidc_user("idp", ISSUER, "sub-ann")
                .await
                .unwrap(),
            Some(id("ann"))
        );
    }
}
