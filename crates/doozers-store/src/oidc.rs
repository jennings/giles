//! `oidc_providers` and `user_oidc_connections`: the OpenID Connect identity providers an admin
//! configures, and which provider account signs in as which user (PLAN 3.4).
//!
//! The store keeps what an admin typed and never contacts a provider: the endpoints, the keys,
//! and the algorithms come from the provider's discovery document, which the core fetches. A
//! provider's client secret isn't here either, only the name it has in the secret store.
//!
//! A connection says that the account `subject` of the provider `issuer` is a user. It is found
//! by that triple and nothing else, never by an email address or a name, because only the
//! subject is stable and never reassigned. The issuer is part of the key, and it is the
//! provider's issuer at the time of the sign-in that is looked up, so a provider whose issuer an
//! admin changes stops recognizing the subjects of the old one (and recognizes them again if the
//! issuer is changed back).

use doozers_types::{Timestamp, UserId};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::time::to_millis;
use crate::{Error, Result, Store, get_time, parse_id, query_all};

/// The longest provider id, in characters.
pub const MAX_PROVIDER_ID_LEN: usize = 32;

/// The longest subject the store accepts, in characters: the OpenID Connect limit (the
/// specification says 255 ASCII characters).
pub const MAX_SUBJECT_LEN: usize = 255;

/// Whether `id` can be a provider's id: 1 to [`MAX_PROVIDER_ID_LEN`] characters of `a-z`, `0-9`,
/// and `-`, starting with a letter or digit. It is in the URL a provider sends the browser back
/// to, so it has to be safe there as it is.
pub fn is_provider_id(id: &str) -> bool {
    let mut chars = id.chars();
    id.len() <= MAX_PROVIDER_ID_LEN
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// An identity provider, as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcProvider {
    /// The id, which is in the redirect URI the provider is told: `<public_url>/auth/oidc/<id>/callback`.
    pub id: String,
    /// What the login page and the setup page call it, such as `Google`.
    pub display_name: String,
    /// The issuer URL, whose `/.well-known/openid-configuration` is the provider's discovery
    /// document.
    pub issuer: String,
    /// The client id the provider gave Doozers.
    pub client_id: String,
    /// The name of the client secret in the secret store, or `None` for a client that has none.
    pub client_secret: Option<String>,
    /// The scopes to ask for, `openid` among them.
    pub scopes: Vec<String>,
    /// Whether the login and setup pages offer it, and sign-ins through it work.
    pub enabled: bool,
    /// When it was added.
    pub created_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
}

/// What an admin sets on a provider: what [`Store::oidc_provider_add`] stores and
/// [`Store::oidc_provider_update`] replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcProviderSettings {
    /// What pages call it.
    pub display_name: String,
    /// The issuer URL.
    pub issuer: String,
    /// The client id.
    pub client_id: String,
    /// The name of the client secret in the secret store, if there is one.
    pub client_secret: Option<String>,
    /// The scopes to ask for; they must include `openid`.
    pub scopes: Vec<String>,
    /// Whether it is offered.
    pub enabled: bool,
}

/// A user's connection to an account at a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcConnection {
    /// The provider's id.
    pub provider: String,
    /// The issuer the subject belongs to.
    pub issuer: String,
    /// The subject: the provider's stable id for the account.
    pub subject: String,
    /// The user it signs in as.
    pub user: UserId,
    /// When the user connected it.
    pub connected_at: Timestamp,
}

const PROVIDER_COLUMNS: &str = "id, display_name, issuer, client_id, client_secret, scopes, \
                                enabled, created_at, updated_at";

fn provider_from_row(row: &Row<'_>) -> Result<OidcProvider> {
    let scopes: String = row.get(5)?;
    Ok(OidcProvider {
        id: row.get(0)?,
        display_name: row.get(1)?,
        issuer: row.get(2)?,
        client_id: row.get(3)?,
        client_secret: row.get(4)?,
        scopes: scopes.split(' ').map(str::to_owned).collect(),
        enabled: row.get::<_, i64>(6)? != 0,
        created_at: get_time(row, 7)?,
        updated_at: get_time(row, 8)?,
    })
}

const CONNECTION_COLUMNS: &str = "provider_id, issuer, subject, user_id, connected_at";

fn connection_from_row(row: &Row<'_>) -> Result<OidcConnection> {
    Ok(OidcConnection {
        provider: row.get(0)?,
        issuer: row.get(1)?,
        subject: row.get(2)?,
        user: parse_id(row.get(3)?, UserId::new)?,
        connected_at: get_time(row, 4)?,
    })
}

/// Checks what an admin typed: every text is set and has no control characters, the scopes are
/// valid scope tokens and include `openid`, and a secret name isn't empty.
fn check(settings: &OidcProviderSettings) -> Result<()> {
    let text = |what: &str, value: &str| {
        if value.trim().is_empty() || value.chars().any(char::is_control) {
            Err(Error::invalid(format!(
                "a provider's {what} must be set, without control characters"
            )))
        } else {
            Ok(())
        }
    };
    text("display name", &settings.display_name)?;
    text("issuer", &settings.issuer)?;
    text("client id", &settings.client_id)?;
    if let Some(secret) = &settings.client_secret {
        text("client secret name", secret)?;
    }
    // A scope token is one or more printable ASCII characters other than space, `"`, and `\`
    // (RFC 6749 section 3.3), so a list of them can be space-separated in one column.
    let token = |scope: &str| {
        !scope.is_empty()
            && scope
                .bytes()
                .all(|b| b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b))
    };
    if !settings.scopes.iter().all(|scope| token(scope)) {
        return Err(Error::invalid(
            "a provider's scopes must be valid scope tokens",
        ));
    }
    if !settings.scopes.iter().any(|scope| scope == "openid") {
        return Err(Error::invalid("a provider's scopes must include openid"));
    }
    Ok(())
}

fn find_provider(conn: &Connection, id: &str) -> Result<Option<OidcProvider>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {PROVIDER_COLUMNS} FROM oidc_providers WHERE id = ?1"
    ))?;
    stmt.query_row([id], |row| Ok(provider_from_row(row)))
        .optional()?
        .transpose()
}

impl Store {
    /// Adds a provider with `settings` at `now` and returns it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if `id` isn't a [provider id](is_provider_id) or `settings` are
    /// invalid; [`Error::Conflict`] if a provider has this id. Nothing changes.
    pub async fn oidc_provider_add(
        &self,
        id: &str,
        settings: OidcProviderSettings,
        now: Timestamp,
    ) -> Result<OidcProvider> {
        if !is_provider_id(id) {
            return Err(Error::invalid("not a valid provider id"));
        }
        check(&settings)?;
        let id = id.to_owned();
        self.write(move |conn| {
            if find_provider(conn, &id)?.is_some() {
                return Err(Error::Conflict(format!("a provider {id} exists")));
            }
            conn.prepare_cached(
                "INSERT INTO oidc_providers
                     (id, display_name, issuer, client_id, client_secret, scopes, enabled,
                      created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            )?
            .execute(params![
                id,
                settings.display_name.trim(),
                settings.issuer.trim(),
                settings.client_id.trim(),
                settings.client_secret.as_deref().map(str::trim),
                settings.scopes.join(" "),
                i64::from(settings.enabled),
                to_millis(now),
            ])?;
            find_provider(conn, &id)?
                .ok_or_else(|| Error::corrupt("the provider just inserted is missing"))
        })
        .await
    }

    /// Replaces the settings of provider `id` and sets `updated_at` to `now`. `None` if there is
    /// no such provider.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if `settings` are invalid.
    pub async fn oidc_provider_update(
        &self,
        id: &str,
        settings: OidcProviderSettings,
        now: Timestamp,
    ) -> Result<Option<OidcProvider>> {
        check(&settings)?;
        let id = id.to_owned();
        self.write(move |conn| {
            let changed = conn
                .prepare_cached(
                    "UPDATE oidc_providers
                     SET display_name = ?2, issuer = ?3, client_id = ?4, client_secret = ?5,
                         scopes = ?6, enabled = ?7, updated_at = ?8
                     WHERE id = ?1",
                )?
                .execute(params![
                    id,
                    settings.display_name.trim(),
                    settings.issuer.trim(),
                    settings.client_id.trim(),
                    settings.client_secret.as_deref().map(str::trim),
                    settings.scopes.join(" "),
                    i64::from(settings.enabled),
                    to_millis(now),
                ])?;
            if changed == 0 {
                return Ok(None);
            }
            find_provider(conn, &id)
        })
        .await
    }

    /// Enables or disables provider `id` and sets `updated_at` to `now`. `false` if there is no
    /// such provider.
    pub async fn oidc_provider_set_enabled(
        &self,
        id: &str,
        enabled: bool,
        now: Timestamp,
    ) -> Result<bool> {
        let id = id.to_owned();
        self.write(move |conn| {
            Ok(conn
                .prepare_cached(
                    "UPDATE oidc_providers SET enabled = ?2, updated_at = ?3 WHERE id = ?1",
                )?
                .execute(params![id, i64::from(enabled), to_millis(now)])?
                == 1)
        })
        .await
    }

    /// Removes provider `id` and every connection to it. `false` if there is no such provider.
    pub async fn oidc_provider_remove(&self, id: &str) -> Result<bool> {
        let id = id.to_owned();
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("DELETE FROM oidc_providers WHERE id = ?1")?
                .execute([id])?
                == 1)
        })
        .await
    }

    /// Provider `id`, if there is one.
    pub async fn oidc_provider(&self, id: &str) -> Result<Option<OidcProvider>> {
        let id = id.to_owned();
        self.read(move |conn| find_provider(conn, &id)).await
    }

    /// Every provider, ordered by display name and then id.
    pub async fn oidc_providers(&self) -> Result<Vec<OidcProvider>> {
        self.read(|conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {PROVIDER_COLUMNS} FROM oidc_providers
                 ORDER BY display_name COLLATE NOCASE, id"
            ))?;
            query_all(&mut stmt, [], provider_from_row)
        })
        .await
    }

    /// Connects the account `subject` of `issuer` at provider `provider` to `user` at `now`,
    /// replacing the connection the user had to that provider, if any: a user has one account
    /// at a provider. Connecting an account the user already has connected changes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::NoSuchUser`] if the user doesn't exist; [`Error::NotFound`] if the provider
    /// doesn't; [`Error::Conflict`] if the account is connected to another user;
    /// [`Error::InvalidRequest`] if the issuer or subject is empty, or the subject is longer than
    /// [`MAX_SUBJECT_LEN`] characters. Nothing changes.
    pub async fn user_oidc_connect(
        &self,
        provider: &str,
        issuer: &str,
        subject: &str,
        user: &UserId,
        now: Timestamp,
    ) -> Result<OidcConnection> {
        if issuer.is_empty() || subject.is_empty() || subject.chars().count() > MAX_SUBJECT_LEN {
            return Err(Error::invalid(
                "an OpenID Connect subject must be 1 to 255 characters, with an issuer",
            ));
        }
        let (provider, issuer, subject, user) = (
            provider.to_owned(),
            issuer.to_owned(),
            subject.to_owned(),
            user.clone(),
        );
        self.write(move |conn| {
            if !conn
                .prepare_cached("SELECT 1 FROM users WHERE id = ?1")?
                .exists([user.as_str()])?
            {
                return Err(Error::NoSuchUser);
            }
            if find_provider(conn, &provider)?.is_none() {
                return Err(Error::NotFound(format!("provider {provider}")));
            }
            let existing = conn
                .prepare_cached(&format!(
                    "SELECT {CONNECTION_COLUMNS} FROM user_oidc_connections
                     WHERE provider_id = ?1 AND issuer = ?2 AND subject = ?3"
                ))?
                .query_row(params![provider, issuer, subject], |row| {
                    Ok(connection_from_row(row))
                })
                .optional()?
                .transpose()?;
            if let Some(existing) = existing {
                return if existing.user == user {
                    Ok(existing)
                } else {
                    Err(Error::Conflict(
                        "this account is connected to another user".to_owned(),
                    ))
                };
            }
            conn.prepare_cached(
                "DELETE FROM user_oidc_connections WHERE provider_id = ?1 AND user_id = ?2",
            )?
            .execute(params![provider, user.as_str()])?;
            conn.prepare_cached(
                "INSERT INTO user_oidc_connections
                     (provider_id, issuer, subject, user_id, connected_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                provider,
                issuer,
                subject,
                user.as_str(),
                to_millis(now)
            ])?;
            // As stored, so the time is the millisecond the column keeps.
            conn.prepare_cached(&format!(
                "SELECT {CONNECTION_COLUMNS} FROM user_oidc_connections
                 WHERE provider_id = ?1 AND issuer = ?2 AND subject = ?3"
            ))?
            .query_row(params![provider, issuer, subject], |row| {
                Ok(connection_from_row(row))
            })?
        })
        .await
    }

    /// The user the account `subject` of `issuer` at provider `provider` is connected to, if any.
    pub async fn user_oidc_user(
        &self,
        provider: &str,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<UserId>> {
        let (provider, issuer, subject) =
            (provider.to_owned(), issuer.to_owned(), subject.to_owned());
        self.read(move |conn| {
            let user: Option<String> = conn
                .prepare_cached(
                    "SELECT user_id FROM user_oidc_connections
                     WHERE provider_id = ?1 AND issuer = ?2 AND subject = ?3",
                )?
                .query_row(params![provider, issuer, subject], |row| row.get(0))
                .optional()?;
            user.map(|user| parse_id(user, UserId::new)).transpose()
        })
        .await
    }

    /// `user`'s connections, oldest first.
    pub async fn user_oidc_connections(&self, user: &UserId) -> Result<Vec<OidcConnection>> {
        let user = user.clone();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {CONNECTION_COLUMNS} FROM user_oidc_connections
                 WHERE user_id = ?1 ORDER BY connected_at, provider_id"
            ))?;
            query_all(&mut stmt, [user.as_str()], connection_from_row)
        })
        .await
    }

    /// Removes `user`'s connection to provider `provider`. `false` if they had none.
    pub async fn user_oidc_disconnect(&self, user: &UserId, provider: &str) -> Result<bool> {
        let (user, provider) = (user.clone(), provider.to_owned());
        self.write(move |conn| {
            Ok(conn
                .prepare_cached(
                    "DELETE FROM user_oidc_connections WHERE user_id = ?1 AND provider_id = ?2",
                )?
                .execute(params![user.as_str(), provider])?
                > 0)
        })
        .await
    }

    /// How many accounts are connected to provider `provider` with its issuer `issuer`: the
    /// connections that can sign in now.
    pub async fn oidc_connection_count(&self, provider: &str, issuer: &str) -> Result<u64> {
        let (provider, issuer) = (provider.to_owned(), issuer.to_owned());
        self.read(move |conn| {
            let count: i64 = conn
                .prepare_cached(
                    "SELECT count(*) FROM user_oidc_connections
                     WHERE provider_id = ?1 AND issuer = ?2",
                )?
                .query_row(params![provider, issuer], |row| row.get(0))?;
            Ok(u64::try_from(count).unwrap_or_default())
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

    fn settings(name: &str, issuer: &str) -> OidcProviderSettings {
        OidcProviderSettings {
            display_name: name.to_owned(),
            issuer: issuer.to_owned(),
            client_id: "client".to_owned(),
            client_secret: Some("oidc/idp/client-secret".to_owned()),
            scopes: vec!["openid".to_owned(), "profile".to_owned()],
            enabled: true,
        }
    }

    async fn with_provider() -> (tempfile::TempDir, Store) {
        let (dir, store) = with_users().await;
        store
            .oidc_provider_add("idp", settings("Idp", "https://idp.example.com"), at(1))
            .await
            .unwrap();
        (dir, store)
    }

    #[test]
    fn a_provider_id_is_a_short_slug() {
        for ok in ["google", "a", "0", "my-idp-2", &"a".repeat(32)] {
            assert!(is_provider_id(ok), "{ok}");
        }
        for bad in [
            "",
            "-a",
            "A",
            "a b",
            "a/b",
            "a_b",
            "é",
            "a.b",
            &"a".repeat(33),
        ] {
            assert!(!is_provider_id(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn providers_are_added_changed_listed_and_removed() {
        let (_dir, store) = with_users().await;
        let added = store
            .oidc_provider_add("zed", settings("Zed", "https://zed.example.com"), at(5))
            .await
            .unwrap();
        assert_eq!(
            (
                added.id.as_str(),
                added.display_name.as_str(),
                added.enabled
            ),
            ("zed", "Zed", true)
        );
        assert_eq!(added.scopes, ["openid", "profile"]);
        assert_eq!(
            added.client_secret.as_deref(),
            Some("oidc/idp/client-secret")
        );
        assert_eq!((added.created_at, added.updated_at), (at(5), at(5)));
        store
            .oidc_provider_add("amy", settings("amy", "https://amy.example.com"), at(6))
            .await
            .unwrap();
        let names: Vec<_> = store
            .oidc_providers()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(names, ["amy", "zed"], "by display name, ignoring case");

        let err = store
            .oidc_provider_add("zed", settings("Other", "https://x.example.com"), at(7))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");

        let mut changed = settings("  Zed Corp ", "https://zed.example.com/");
        changed.client_secret = None;
        changed.enabled = false;
        let after = store
            .oidc_provider_update("zed", changed, at(9))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.display_name, "Zed Corp");
        assert_eq!(after.issuer, "https://zed.example.com/");
        assert_eq!(after.client_secret, None);
        assert!(!after.enabled);
        assert_eq!((after.created_at, after.updated_at), (at(5), at(9)));
        assert_eq!(
            store
                .oidc_provider_update("nope", settings("N", "https://n"), at(9))
                .await
                .unwrap(),
            None
        );

        assert!(
            store
                .oidc_provider_set_enabled("zed", true, at(10))
                .await
                .unwrap()
        );
        let zed = store.oidc_provider("zed").await.unwrap().unwrap();
        assert!(zed.enabled);
        assert_eq!(zed.updated_at, at(10));
        assert!(
            !store
                .oidc_provider_set_enabled("nope", true, at(10))
                .await
                .unwrap()
        );

        assert!(store.oidc_provider_remove("zed").await.unwrap());
        assert!(!store.oidc_provider_remove("zed").await.unwrap());
        assert_eq!(store.oidc_provider("zed").await.unwrap(), None);
    }

    #[tokio::test]
    async fn invalid_providers_are_refused() {
        let (_dir, store) = with_users().await;
        let err = store
            .oidc_provider_add("Bad Id", settings("X", "https://x"), at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        let blank = |edit: fn(&mut OidcProviderSettings)| {
            let mut s = settings("X", "https://x");
            edit(&mut s);
            s
        };
        for bad in [
            blank(|s| s.display_name = " ".into()),
            blank(|s| s.issuer = String::new()),
            blank(|s| s.client_id = "a\nb".into()),
            blank(|s| s.client_secret = Some(String::new())),
            blank(|s| s.scopes = vec!["profile".into()]),
            blank(|s| s.scopes = vec!["openid".into(), "two words".into()]),
            blank(|s| s.scopes = vec!["openid".into(), String::new()]),
        ] {
            let err = store
                .oidc_provider_add("idp", bad.clone(), at(1))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::InvalidRequest(_)), "{bad:?}: {err}");
            let err = store
                .oidc_provider_update("idp", bad, at(1))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        }
        assert!(store.oidc_providers().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_account_is_found_by_its_issuer_and_subject_only() {
        let (_dir, store) = with_provider().await;
        let connected = store
            .user_oidc_connect(
                "idp",
                "https://idp.example.com",
                "sub-1",
                &id("bob"),
                at(10),
            )
            .await
            .unwrap();
        assert_eq!(connected.connected_at, at(10));
        let found = |issuer: &'static str, subject: &'static str| {
            let store = store.clone();
            async move { store.user_oidc_user("idp", issuer, subject).await.unwrap() }
        };
        assert_eq!(
            found("https://idp.example.com", "sub-1").await,
            Some(id("bob"))
        );
        assert_eq!(found("https://idp.example.com", "sub-2").await, None);
        assert_eq!(found("https://other.example.com", "sub-1").await, None);
        assert_eq!(found("https://idp.example.com", "SUB-1").await, None);
        assert_eq!(
            store
                .user_oidc_user("nope", "https://idp.example.com", "sub-1")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store.user_oidc_connections(&id("bob")).await.unwrap(),
            [connected]
        );
        assert!(
            store
                .user_oidc_connections(&id("ann"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .oidc_connection_count("idp", "https://idp.example.com")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .oidc_connection_count("idp", "https://moved.example.com")
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn an_account_belongs_to_one_user_and_a_user_has_one_account_per_provider() {
        let (_dir, store) = with_provider().await;
        let issuer = "https://idp.example.com";
        store
            .user_oidc_connect("idp", issuer, "sub-1", &id("bob"), at(10))
            .await
            .unwrap();
        // The same account again changes nothing, and the other user can't take it.
        let again = store
            .user_oidc_connect("idp", issuer, "sub-1", &id("bob"), at(20))
            .await
            .unwrap();
        assert_eq!(again.connected_at, at(10));
        let err = store
            .user_oidc_connect("idp", issuer, "sub-1", &id("ann"), at(21))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        assert_eq!(
            store.user_oidc_user("idp", issuer, "sub-1").await.unwrap(),
            Some(id("bob"))
        );
        // Another account replaces the user's connection to the provider.
        store
            .user_oidc_connect("idp", issuer, "sub-2", &id("bob"), at(30))
            .await
            .unwrap();
        assert_eq!(
            store.user_oidc_user("idp", issuer, "sub-1").await.unwrap(),
            None
        );
        assert_eq!(
            store.user_oidc_user("idp", issuer, "sub-2").await.unwrap(),
            Some(id("bob"))
        );
        assert_eq!(
            store.user_oidc_connections(&id("bob")).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn connecting_needs_a_user_a_provider_and_a_subject_that_fits() {
        let (_dir, store) = with_provider().await;
        let issuer = "https://idp.example.com";
        let err = store
            .user_oidc_connect("idp", issuer, "s", &id("nobody"), at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NoSuchUser), "{err}");
        let err = store
            .user_oidc_connect("nope", issuer, "s", &id("bob"), at(1))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
        for (issuer, subject) in [
            (issuer, ""),
            ("", "s"),
            (issuer, &"s".repeat(MAX_SUBJECT_LEN + 1)),
        ] {
            let err = store
                .user_oidc_connect("idp", issuer, subject, &id("bob"), at(1))
                .await
                .unwrap_err();
            assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
        }
        store
            .user_oidc_connect(
                "idp",
                issuer,
                &"s".repeat(MAX_SUBJECT_LEN),
                &id("bob"),
                at(1),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn disconnecting_forgets_only_that_connection() {
        let (_dir, store) = with_provider().await;
        let issuer = "https://idp.example.com";
        store
            .user_oidc_connect("idp", issuer, "sub-1", &id("bob"), at(1))
            .await
            .unwrap();
        store
            .user_oidc_connect("idp", issuer, "sub-2", &id("ann"), at(1))
            .await
            .unwrap();
        assert!(store.user_oidc_disconnect(&id("bob"), "idp").await.unwrap());
        assert!(!store.user_oidc_disconnect(&id("bob"), "idp").await.unwrap());
        assert_eq!(
            store.user_oidc_user("idp", issuer, "sub-1").await.unwrap(),
            None
        );
        assert_eq!(
            store.user_oidc_user("idp", issuer, "sub-2").await.unwrap(),
            Some(id("ann"))
        );
    }

    #[tokio::test]
    async fn removing_a_provider_or_a_user_removes_their_connections() {
        let (_dir, store) = with_provider().await;
        let issuer = "https://idp.example.com";
        store
            .oidc_provider_add(
                "other",
                settings("Other", "https://other.example.com"),
                at(1),
            )
            .await
            .unwrap();
        for (provider, subject, user) in [
            ("idp", "sub-1", "bob"),
            ("idp", "sub-2", "ann"),
            ("other", "sub-3", "bob"),
        ] {
            let issuer = if provider == "idp" {
                issuer
            } else {
                "https://other.example.com"
            };
            store
                .user_oidc_connect(provider, issuer, subject, &id(user), at(2))
                .await
                .unwrap();
        }
        assert_eq!(
            store.user_oidc_connections(&id("bob")).await.unwrap().len(),
            2
        );
        assert!(store.oidc_provider_remove("other").await.unwrap());
        assert_eq!(
            store.user_oidc_connections(&id("bob")).await.unwrap().len(),
            1
        );
        store
            .write(|conn| Ok(conn.execute("DELETE FROM users WHERE id = 'bob'", [])?))
            .await
            .unwrap();
        assert_eq!(
            store.user_oidc_user("idp", issuer, "sub-1").await.unwrap(),
            None
        );
        assert_eq!(
            store.user_oidc_user("idp", issuer, "sub-2").await.unwrap(),
            Some(id("ann"))
        );
    }

    #[tokio::test]
    async fn a_secret_name_is_stored_trimmed_and_a_connection_time_as_stored() {
        let (_dir, store) = with_users().await;
        let mut padded = settings("Zed", "https://zed.example.com");
        padded.client_secret = Some(" oidc/zed/client-secret ".to_owned());
        let added = store
            .oidc_provider_add("zed", padded.clone(), at(1))
            .await
            .unwrap();
        assert_eq!(
            added.client_secret.as_deref(),
            Some("oidc/zed/client-secret")
        );
        let updated = store
            .oidc_provider_update("zed", padded, at(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            updated.client_secret.as_deref(),
            Some("oidc/zed/client-secret")
        );

        // The column keeps milliseconds, and what connecting returns is what is stored.
        let now = Timestamp::new(1_800_000_010, 123_456_789).unwrap();
        let connected = store
            .user_oidc_connect("zed", "https://zed.example.com", "s", &id("bob"), now)
            .await
            .unwrap();
        assert_eq!(connected.connected_at.subsec_nanosecond(), 123_000_000);
        assert_eq!(
            store.user_oidc_connections(&id("bob")).await.unwrap(),
            [connected]
        );
    }
}
