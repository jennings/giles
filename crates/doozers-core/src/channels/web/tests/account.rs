//! The Account page (`/account`) through the real handlers: the profile, the summary of channel
//! identities, and the sidebar. Requests are authenticated as a browser's are, with a session
//! cookie (a recovery link's), so the footer's display name comes from the stored users.

use super::admin::{session_of, stored_users};
use super::ssh_keys::url_encode;
use super::*;
use crate::users::Users;

pub(super) struct Page {
    pub(super) env: Env,
    pub(super) users: Arc<Users>,
}

/// What a request got back.
pub(super) struct Answer {
    pub(super) status: StatusCode,
    pub(super) body: String,
    pub(super) location: Option<String>,
}

/// A browser logged in as one user: the session cookie and the CSRF token it derives.
pub(super) struct Browser {
    pub(super) token: String,
    cookie: String,
    pub(super) csrf: String,
}

impl Browser {
    pub(super) fn new(token: String) -> Self {
        Self {
            cookie: session(&token),
            csrf: auth::cookie_csrf(&token),
            token,
        }
    }
}

impl Page {
    /// The owner (an admin), `root` (an admin), and `mia` (a member), each with `web:<id>`.
    pub(super) async fn new() -> Self {
        Self::with(env().await).await
    }

    /// [`new`](Self::new) over `env`.
    pub(super) async fn with(env: Env) -> Self {
        let users = stored_users(&env).await;
        Self { env, users }
    }

    pub(super) fn router(&self) -> axum::Router {
        self.env
            .app
            .install_account(
                self.env.app.install(RouterBuilder::new()),
                self.users.clone(),
            )
            .build()
    }

    /// A browser of `user`, logged in with a recovery link.
    pub(super) async fn login(&self, user: &str) -> Browser {
        Browser::new(session_of(&self.env, user).await)
    }

    pub(super) async fn send(
        &self,
        browser: Option<&Browser>,
        method: Method,
        path: &str,
        form: Option<&str>,
    ) -> Answer {
        let headers: Vec<(&str, &str)> = browser
            .map(|b| vec![("cookie", b.cookie.as_str())])
            .unwrap_or_default();
        let response = call(self.router(), method, path, &headers, form).await;
        let status = response.status();
        if status != StatusCode::UNAUTHORIZED {
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{path}");
        }
        let location = response
            .headers()
            .get(LOCATION)
            .map(|l| l.to_str().unwrap().to_owned());
        Answer {
            status,
            body: body_text(response).await,
            location,
        }
    }

    pub(super) async fn get(&self, browser: &Browser, path: &str) -> Answer {
        self.send(Some(browser), Method::GET, path, None).await
    }

    /// A POST of `pairs` and the browser's CSRF token, URL-encoded.
    pub(super) async fn post(
        &self,
        browser: &Browser,
        path: &str,
        pairs: &[(&str, &str)],
    ) -> Answer {
        let mut form = format!("csrf={}", browser.csrf);
        for (name, value) in pairs {
            form.push_str(&format!("&{name}={}", url_encode(value)));
        }
        self.send(Some(browser), Method::POST, path, Some(&form))
            .await
    }
}

#[tokio::test]
async fn the_page_needs_a_login_and_shows_only_the_users_own_profile_and_identities() {
    let p = Page::new().await;
    let answer = p.send(None, Method::GET, "/account", None).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);

    let mia = p.login("mia").await;
    let answer = p.get(&mia, "/account").await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
    let html = &answer.body;
    assert!(html.contains("<h1>Account</h1>"), "{html}");
    assert!(html.contains("<code>mia</code>"), "{html}");
    assert!(html.contains("<dd>Member</dd>"), "{html}");
    assert!(
        html.contains(r#"name="display_name" value="MIA""#),
        "{html}"
    );
    assert!(html.contains(r#"name="timezone" value="""#), "{html}");
    assert!(html.contains("<code>web:mia</code>"), "{html}");
    assert!(
        html.contains(r#"<span class="pill pill-ok">default</span>"#),
        "{html}"
    );
    assert!(html.contains(r#"href="/account/identities""#), "{html}");
    // Nothing of another user's.
    assert!(!html.contains("web:root"), "{html}");
    assert!(!html.contains("web:owner"), "{html}");

    // An admin sees their own role.
    let root = p.login("root").await;
    let html = p.get(&root, "/account").await.body;
    assert!(html.contains("<dd>Admin</dd>"), "{html}");
}

#[tokio::test]
async fn the_sidebar_links_the_account_and_the_footer_shows_the_display_name() {
    let p = Page::new().await;
    let mia = p.login("mia").await;
    let html = p.get(&mia, "/account").await.body;
    assert!(
        html.contains(r#"<a href="/account" aria-current="page">Account</a>"#),
        "{html}"
    );
    // Not the id, but the name the user goes by, in the footer of the page.
    assert!(
        html.contains(r#"<span class="sr-only">Signed in as </span>MIA</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="avatar" aria-hidden="true">M</span>"#),
        "{html}"
    );

    // Another page of the user's has the same link, not marked current.
    let response = call(
        p.router(),
        Method::GET,
        "/jobs",
        &[("cookie", &session(&mia.token))],
        None,
    )
    .await;
    let html = body_text(response).await;
    assert!(html.contains(r#"<a href="/account">Account</a>"#), "{html}");
    assert!(!html.contains(r#"href="/account/identities""#), "{html}");
}

#[tokio::test]
async fn saving_the_profile_changes_the_name_and_time_zone_the_next_request_shows() {
    let p = Page::new().await;
    let mia = p.login("mia").await;
    let answer = p
        .post(
            &mia,
            "/account/profile",
            &[
                ("display_name", "  Mia Example  "),
                ("timezone", " America/Los_Angeles "),
            ],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.location.as_deref(), Some("/account?notice=saved"));

    let stored = p
        .env
        .store
        .user(&UserId::new("mia").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.display_name, "Mia Example");
    assert_eq!(stored.timezone.as_deref(), Some("America/Los_Angeles"));
    let user = p
        .env
        .channel
        .users()
        .get()
        .user(&UserId::new("mia").unwrap())
        .cloned()
        .unwrap();
    assert_eq!(user.display_name, "Mia Example");

    let html = p.get(&mia, "/account?notice=saved").await.body;
    assert!(html.contains("Saved your profile."), "{html}");
    assert!(
        html.contains(r#"name="display_name" value="Mia Example""#),
        "{html}"
    );
    assert!(
        html.contains(r#"name="timezone" value="America/Los_Angeles""#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="sr-only">Signed in as </span>Mia Example</span>"#),
        "{html}"
    );

    // An empty time zone clears it.
    let answer = p
        .post(
            &mia,
            "/account/profile",
            &[("display_name", "Mia"), ("timezone", "  ")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let stored = p
        .env
        .store
        .user(&UserId::new("mia").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (stored.display_name.as_str(), stored.timezone),
        ("Mia", None)
    );

    // Only the user's own profile changed.
    let root = p
        .env
        .store
        .user(&UserId::new("root").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.display_name, "ROOT");
}

#[tokio::test]
async fn a_profile_that_is_not_valid_is_refused_with_why_and_what_was_typed() {
    let p = Page::new().await;
    let mia = p.login("mia").await;
    for (name, zone, why) in [
        ("Mia", "Mars/Olympus", "unknown time zone"),
        ("   ", "", "a display name is 1 to"),
        ("Mi\u{7}a", "", "without control characters"),
    ] {
        let answer = p
            .post(
                &mia,
                "/account/profile",
                &[("display_name", name), ("timezone", zone)],
            )
            .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{name:?} {zone:?}");
        assert!(answer.body.contains(why), "{}", answer.body);
        assert!(answer.body.contains(r#"role="alert""#), "{}", answer.body);
        if !zone.is_empty() {
            assert!(
                answer
                    .body
                    .contains(&format!(r#"name="timezone" value="{zone}""#)),
                "{}",
                answer.body
            );
        }
        // The name typed comes back, not the stored one (`MIA`).
        if name == "Mia" {
            assert!(
                answer.body.contains(r#"name="display_name" value="Mia""#),
                "{}",
                answer.body
            );
        }
    }
    let stored = p
        .env
        .store
        .user(&UserId::new("mia").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (stored.display_name.as_str(), stored.timezone),
        ("MIA", None)
    );
}

#[tokio::test]
async fn changing_the_profile_needs_the_csrf_token_and_a_login() {
    let p = Page::new().await;
    let mia = p.login("mia").await;
    let answer = p
        .send(
            Some(&mia),
            Method::POST,
            "/account/profile",
            Some("display_name=Evil&timezone="),
        )
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    let answer = p
        .send(
            Some(&mia),
            Method::POST,
            "/account/profile",
            Some("csrf=wrong&display_name=Evil&timezone="),
        )
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    let answer = p
        .send(
            None,
            Method::POST,
            "/account/profile",
            Some("csrf=x&display_name=Evil"),
        )
        .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    let stored = p
        .env
        .store
        .user(&UserId::new("mia").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.display_name, "MIA");
}

#[tokio::test]
async fn what_a_user_wrote_in_their_profile_is_escaped() {
    let p = Page::new().await;
    let mia = p.login("mia").await;
    let answer = p
        .post(
            &mia,
            "/account/profile",
            &[("display_name", r#"<b>"x"</b>"#), ("timezone", "")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let html = p.get(&mia, "/account").await.body;
    assert!(!html.contains("<b>"), "{html}");
    assert!(html.contains("&#60;b&#62;"), "{html}");
}

// --- Sign-in methods ---------------------------------------------------------------------------

use super::passkeys::{ORIGIN, env_at};
use super::reverify::{GOOD, minutes};
use doozers_store::{NewPasskey, OidcProviderSettings, PasskeyKind};

fn id(user: &str) -> UserId {
    UserId::new(user).unwrap()
}

impl Page {
    /// Gives `user` a passkey named `name`, without the ceremony; its id.
    async fn give_passkey(&self, user: &str, credential: u8, name: &str) -> i64 {
        self.env
            .store
            .user_passkey_add(
                NewPasskey {
                    user: id(user),
                    credential_id: vec![credential],
                    credential: "{}".to_owned(),
                    kind: PasskeyKind::Discoverable,
                    name: name.to_owned(),
                },
                Timestamp::now(),
            )
            .await
            .unwrap()
            .id
            .get()
    }

    /// Adds provider `idp` (and `off`, disabled) and connects `user` to `idp`.
    async fn give_connection(&self, user: &str) {
        let settings = |name: &str, issuer: &str, enabled| OidcProviderSettings {
            display_name: name.to_owned(),
            issuer: issuer.to_owned(),
            client_id: "client".to_owned(),
            client_secret: None,
            scopes: vec!["openid".to_owned()],
            enabled,
        };
        let now = Timestamp::now();
        let store = &self.env.store;
        if store.oidc_provider("idp").await.unwrap().is_none() {
            store
                .oidc_provider_add("idp", settings("Idp", "https://idp.example.com", true), now)
                .await
                .unwrap();
            store
                .oidc_provider_add(
                    "off",
                    settings("Off", "https://off.example.com", false),
                    now,
                )
                .await
                .unwrap();
        }
        store
            .user_oidc_connect(
                "idp",
                "https://idp.example.com",
                &format!("sub-{user}"),
                &id(user),
                now,
            )
            .await
            .unwrap();
    }

    async fn methods_left(&self, user: &str) -> (bool, usize, bool) {
        let store = &self.env.store;
        (
            store.user_password(&id(user)).await.unwrap().is_some(),
            store.user_passkeys(&id(user)).await.unwrap().len(),
            !store
                .user_oidc_connections(&id(user))
                .await
                .unwrap()
                .is_empty(),
        )
    }
}

/// `mia` with a password, a passkey, and an account at a provider, on an install where passkeys
/// work; `root` has a passkey of their own.
async fn with_every_method() -> (Page, i64) {
    let p = Page::with(env_at(Some(ORIGIN)).await).await;
    p.give_password("mia", GOOD).await;
    let key = p.give_passkey("mia", 1, "Phone").await;
    p.give_passkey("root", 2, "Roots secret laptop").await;
    p.give_connection("mia").await;
    (p, key)
}

#[tokio::test]
async fn the_page_lists_the_users_methods_and_offers_changes_only_to_a_login_that_may() {
    let (p, key) = with_every_method().await;
    let remove_password = "/account/sign-in/remove?method=password";
    let remove_passkey = format!("/account/sign-in/remove?method=passkey:{key}");

    // A recovery login may change them as it is.
    let recovery = p.login("mia").await;
    let html = p.get(&recovery, "/account").await.body;
    assert!(html.contains("You signed in with a link"), "{html}");
    assert!(html.contains("You set your password"), "{html}");
    assert!(
        html.contains(r#"href="/setup/password">Change your password"#),
        "{html}"
    );
    assert!(
        html.contains(&format!(r#"href="{remove_password}""#)),
        "{html}"
    );
    assert!(
        html.contains("<td data-label=\"Passkey\" class=\"m-title\">Phone<br>"),
        "{html}"
    );
    assert!(html.contains(">never</td>"), "{html}");
    assert!(
        html.contains(&format!(r#"href="{remove_passkey}""#)),
        "{html}"
    );
    assert!(
        html.contains(r#"href="/setup/passkey">Add a passkey"#),
        "{html}"
    );
    assert!(html.contains("Idp:"), "{html}");
    assert!(html.contains("Connect another account"), "{html}");
    assert!(
        html.contains(r#"href="/account/sign-in/remove?method=oidc:idp""#),
        "{html}"
    );
    assert!(
        html.contains(&format!(r#"href="/setup/oidc/idp?csrf={}""#, recovery.csrf)),
        "{html}"
    );
    // A provider that is off, and has no connection, isn't offered, and nobody else's passkey is
    // shown.
    assert!(!html.contains("Off:"), "{html}");
    assert!(!html.contains("Roots secret laptop"), "{html}");

    // A login that came from a password may rename, and has to confirm before it may change.
    let stolen = p.password_login("mia").await;
    let html = p.get(&stolen, "/account").await.body;
    assert!(
        html.contains(r#"href="/account/reverify">Confirm it's you"#),
        "{html}"
    );
    assert!(html.contains("/account/sign-in/passkey/rename"), "{html}");
    for gone in [
        "/account/sign-in/remove",
        "/setup/password",
        "/setup/passkey",
        "/setup/oidc/",
    ] {
        assert!(!html.contains(gone), "{gone}: {html}");
    }

    // After confirming, it may, and says until when.
    p.reverified_ago(&stolen, minutes(1)).await;
    let html = p.get(&stolen, "/account").await.body;
    assert!(html.contains("You confirmed it's you."), "{html}");
    assert!(
        html.contains(&format!(r#"href="{remove_password}""#)),
        "{html}"
    );
    assert!(
        html.contains(r#"href="/setup/passkey">Add a passkey"#),
        "{html}"
    );
}

#[tokio::test]
async fn a_user_removes_each_kind_of_method_but_not_the_last_one_that_works() {
    let (p, key) = with_every_method().await;
    let mia = p.login("mia").await;
    let remove = |method: String| {
        let (p, mia) = (&p, &mia);
        async move {
            p.post(mia, "/account/sign-in/remove", &[("method", &method)])
                .await
        }
    };

    // The page asks first, and says what it is.
    let ask = p.get(&mia, "/account/sign-in/remove?method=oidc:idp").await;
    assert_eq!(ask.status, StatusCode::OK, "{}", ask.body);
    assert!(
        ask.body.contains("<h1>Remove your Idp account?</h1>"),
        "{}",
        ask.body
    );
    assert!(
        ask.body.contains(r#"name="method" value="oidc:idp""#),
        "{}",
        ask.body
    );
    let ask = p
        .get(
            &mia,
            &format!("/account/sign-in/remove?method=passkey:{key}"),
        )
        .await;
    assert!(
        ask.body.contains("<h1>Remove the passkey Phone?</h1>"),
        "{}",
        ask.body
    );

    let answer = remove("oidc:idp".into()).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.location.as_deref(), Some("/account?notice=removed"));
    assert_eq!(p.methods_left("mia").await, (true, 1, false));
    let answer = remove("password".into()).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(p.methods_left("mia").await, (false, 1, false));

    // The passkey is all that is left: refused, and said so on the page.
    let answer = remove(format!("passkey:{key}")).await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert!(
        answer.body.contains("only way to sign in"),
        "{}",
        answer.body
    );
    assert_eq!(p.methods_left("mia").await, (false, 1, false));
    let html = p.get(&mia, "/account").await.body;
    assert!(html.contains("You have one way to sign in."), "{html}");

    // Adding another makes it removable.
    p.give_password("mia", GOOD).await;
    let answer = remove(format!("passkey:{key}")).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(p.methods_left("mia").await, (true, 0, false));
    let answer = remove("password".into()).await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert_eq!(p.methods_left("mia").await, (true, 0, false));
}

#[tokio::test]
async fn removing_a_method_needs_the_same_confirmation_as_adding_one() {
    let (p, key) = with_every_method().await;
    let stolen = p.password_login("mia").await;
    let method = format!("passkey:{key}");

    let answer = p
        .post(&stolen, "/account/sign-in/remove", &[("method", &method)])
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert!(
        answer.body.contains("Confirm it's you first"),
        "{}",
        answer.body
    );
    let ask = p
        .get(&stolen, &format!("/account/sign-in/remove?method={method}"))
        .await;
    assert_eq!(ask.status, StatusCode::FORBIDDEN);
    assert_eq!(p.methods_left("mia").await, (true, 1, true));

    // It is the CSRF token's job too, and a login's.
    let recovery = p.login("mia").await;
    let answer = p
        .send(
            Some(&recovery),
            Method::POST,
            "/account/sign-in/remove",
            Some(&format!("method={method}")),
        )
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    let answer = p
        .send(
            None,
            Method::POST,
            "/account/sign-in/remove",
            Some("csrf=x&method=password"),
        )
        .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(p.methods_left("mia").await, (true, 1, true));

    // Confirmed, the same session removes it; and the confirmation runs out.
    p.reverified_ago(&stolen, minutes(1)).await;
    let answer = p
        .post(&stolen, "/account/sign-in/remove", &[("method", &method)])
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    p.reverified_ago(&stolen, minutes(11)).await;
    let answer = p
        .post(
            &stolen,
            "/account/sign-in/remove",
            &[("method", "password")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(p.methods_left("mia").await, (true, 0, true));
}

#[tokio::test]
async fn another_users_method_or_one_that_is_not_there_is_a_404() {
    let (p, _) = with_every_method().await;
    let roots = p.env.store.user_passkeys(&id("root")).await.unwrap()[0]
        .id
        .get();
    let mia = p.login("mia").await;
    for method in [
        format!("passkey:{roots}"),
        "passkey:99999".to_owned(),
        "passkey:x".to_owned(),
        "oidc:off".to_owned(),
        "oidc:nope".to_owned(),
        "oidc:Bad Id".to_owned(),
        "telepathy".to_owned(),
        String::new(),
    ] {
        let answer = p
            .post(&mia, "/account/sign-in/remove", &[("method", &method)])
            .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{method:?}");
        let ask = p
            .get(
                &mia,
                &format!("/account/sign-in/remove?method={}", url_encode(&method)),
            )
            .await;
        assert_eq!(ask.status, StatusCode::NOT_FOUND, "{method:?}");
    }
    // A user without a password has none to remove.
    let root = p.login("root").await;
    let answer = p
        .post(&root, "/account/sign-in/remove", &[("method", "password")])
        .await;
    assert_eq!(answer.status, StatusCode::NOT_FOUND);
    assert_eq!(p.methods_left("mia").await, (true, 1, true));
    assert_eq!(p.methods_left("root").await, (false, 1, false));
}

#[tokio::test]
async fn passkeys_that_cannot_sign_in_here_do_not_keep_the_password_from_being_the_last_way() {
    // No public_url: a passkey stored before the address was lost can't sign anyone in.
    let p = Page::with(env_at(None).await).await;
    p.give_password("mia", GOOD).await;
    let key = p.give_passkey("mia", 1, "Phone").await;
    let mia = p.login("mia").await;
    let html = p.get(&mia, "/account").await.body;
    assert!(html.contains("Passkeys aren&#39;t available"), "{html}");
    assert!(!html.contains("Add a passkey"), "{html}");
    assert!(html.contains("You have one way to sign in."), "{html}");

    let answer = p
        .post(&mia, "/account/sign-in/remove", &[("method", "password")])
        .await;
    assert_eq!(answer.status, StatusCode::CONFLICT, "{}", answer.body);
    assert_eq!(p.methods_left("mia").await, (true, 1, false));
    // The passkey doesn't count, so it can go.
    let answer = p
        .post(
            &mia,
            "/account/sign-in/remove",
            &[("method", &format!("passkey:{key}"))],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_connection_to_a_provider_that_is_off_does_not_count() {
    let p = Page::new().await;
    p.give_password("mia", GOOD).await;
    p.give_connection("mia").await;
    p.env
        .store
        .oidc_provider_set_enabled("idp", false, Timestamp::now())
        .await
        .unwrap();
    let mia = p.login("mia").await;
    let html = p.get(&mia, "/account").await.body;
    assert!(html.contains("an admin turned this provider off"), "{html}");
    assert!(!html.contains("Connect another account"), "{html}");
    // The password is the only way in now, so it stays, and the dead connection can go.
    let answer = p
        .post(&mia, "/account/sign-in/remove", &[("method", "password")])
        .await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    let answer = p
        .post(&mia, "/account/sign-in/remove", &[("method", "oidc:idp")])
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_passkey_is_renamed_by_its_user_without_confirming_and_nobody_elses_is() {
    let (p, key) = with_every_method().await;
    let roots = p.env.store.user_passkeys(&id("root")).await.unwrap()[0]
        .id
        .get();
    let stolen = p.password_login("mia").await;
    let rename = |id: String, name: &'static str| {
        let (p, stolen) = (&p, &stolen);
        async move {
            p.post(
                stolen,
                "/account/sign-in/passkey/rename",
                &[("id", &id), ("name", name)],
            )
            .await
        }
    };
    let answer = rename(key.to_string(), "  Work\u{7} laptop  ").await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.location.as_deref(), Some("/account?notice=renamed"));
    let stored = p.env.store.user_passkeys(&id("mia")).await.unwrap();
    assert_eq!(stored[0].name, "Work laptop");
    let html = p.get(&stolen, "/account?notice=renamed").await.body;
    assert!(html.contains("Renamed the passkey."), "{html}");

    // An empty name changes nothing; another user's passkey, or none, is a 404.
    let answer = rename(key.to_string(), "   ").await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(
        answer.body.contains("Type a name for the passkey."),
        "{}",
        answer.body
    );
    for id_ in [roots.to_string(), "99999".to_owned(), "x".to_owned()] {
        let answer = rename(id_.clone(), "Mine").await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{id_}");
    }
    let roots_key = &p.env.store.user_passkeys(&id("root")).await.unwrap()[0];
    assert_eq!(roots_key.name, "Roots secret laptop");
    assert_eq!(
        p.env.store.user_passkeys(&id("mia")).await.unwrap()[0].name,
        "Work laptop"
    );
    // The CSRF token is needed.
    let answer = p
        .send(
            Some(&stolen),
            Method::POST,
            "/account/sign-in/passkey/rename",
            Some(&format!("id={key}&name=Evil")),
        )
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
}
