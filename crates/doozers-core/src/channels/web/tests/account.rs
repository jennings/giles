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

// --- Sessions ----------------------------------------------------------------------------------

impl Page {
    /// The ids of `user`'s sessions, newest first.
    async fn session_ids(&self, user: &str) -> Vec<String> {
        self.env
            .store
            .web_sessions_of(&id(user), Timestamp::now())
            .await
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect()
    }

    async fn revoke(&self, browser: &Browser, session: &str) -> Answer {
        self.post(browser, &format!("/account/sessions/{session}/revoke"), &[])
            .await
    }

    /// Whether the browser's session still logs in.
    async fn logged_in(&self, browser: &Browser) -> bool {
        self.send(Some(browser), Method::GET, "/account", None)
            .await
            .status
            == StatusCode::OK
    }
}

#[tokio::test]
async fn the_page_lists_the_users_own_logins_marks_this_one_and_shows_no_token_or_hash() {
    let p = Page::new().await;
    let recovery = p.login("mia").await;
    let password = p.password_login("mia").await;
    let roots = p.login("root").await;
    // Used once, so it has a time.
    p.get(&recovery, "/account").await;

    let html = p.get(&password, "/account").await.body;
    assert!(html.contains("<h2 id=\"sessions\">Logins</h2>"), "{html}");
    assert_eq!(html.matches(r#"pill-ok">this login"#).count(), 1, "{html}");
    assert!(html.contains("Recovery link"), "{html}");
    assert!(html.contains(">Password"), "{html}");
    // Two logins of Mia's, and not Root's.
    let mine = p.session_ids("mia").await;
    assert_eq!(mine.len(), 2);
    assert_eq!(html.matches("/revoke\"").count(), 2, "{html}");
    for session in &mine {
        assert!(
            html.contains(&format!("/account/sessions/{session}/revoke")),
            "{html}"
        );
    }
    for session in p.session_ids("root").await {
        assert!(!html.contains(&session), "{html}");
    }
    // The current login's own row says so and offers to log out.
    let current_row = html
        .split("<tr>")
        .find(|row| row.contains(r#"pill-ok">this login"#))
        .unwrap();
    assert!(current_row.contains(">Password"), "{current_row}");
    assert!(current_row.contains("Log out"), "{current_row}");

    // Never a token, or the hash of one, which would be as good as the token for anyone with a
    // copy of the database, and not the id of the session.
    use sha2::{Digest, Sha256};
    for browser in [&recovery, &password, &roots] {
        let hash: String = Sha256::digest(browser.token.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(!html.contains(&browser.token), "token on the page");
        assert!(!html.contains(&hash), "hash on the page");
        assert!(!mine.contains(&browser.token));
        assert!(!mine.contains(&hash));
    }
}

#[tokio::test]
async fn a_user_ends_one_of_their_other_logins() {
    let p = Page::new().await;
    let here = p.login("mia").await;
    let there = p.password_login("mia").await;
    let ids = p.session_ids("mia").await;
    let mine = p.session_id_of(&here).await;
    let theirs = ids
        .iter()
        .find(|session| **session != mine)
        .unwrap()
        .clone();

    assert!(p.logged_in(&there).await);
    let answer = p.revoke(&here, &theirs).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER, "{}", answer.body);
    assert_eq!(answer.location.as_deref(), Some("/account?notice=revoked"));
    assert!(!p.logged_in(&there).await, "the other login ended");
    assert!(p.logged_in(&here).await, "this one stays");
    assert_eq!(p.session_ids("mia").await.len(), 1);
    let html = p.get(&here, "/account?notice=revoked").await.body;
    assert!(html.contains("Ended that login."), "{html}");
}

impl Page {
    async fn session_id_of(&self, browser: &Browser) -> String {
        self.env
            .store
            .web_session(&browser.token, Timestamp::now())
            .await
            .unwrap()
            .unwrap()
            .id
    }
}

#[tokio::test]
async fn ending_this_login_logs_out_and_deletes_the_cookie() {
    let p = Page::new().await;
    let here = p.login("mia").await;
    let me = p.session_id_of(&here).await;
    let response = call(
        p.router(),
        Method::POST,
        &format!("/account/sessions/{me}/revoke"),
        &[("cookie", &session(&here.token))],
        Some(&format!("csrf={}", here.csrf)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/");
    let cookie = set_cookie(&response);
    assert!(
        cookie.starts_with("doozers_session=;") && cookie.contains("Max-Age=0"),
        "{cookie}"
    );
    assert!(!p.logged_in(&here).await);
    assert!(p.session_ids("mia").await.is_empty());
}

#[tokio::test]
async fn another_users_login_or_none_at_all_is_a_404_and_ending_needs_the_csrf_token() {
    let p = Page::new().await;
    let mia = p.login("mia").await;
    let roots = p.login("root").await;
    let root_session = p.session_ids("root").await[0].clone();

    for target in [
        root_session.clone(),
        "0".repeat(32),
        "nope".to_owned(),
        "A".repeat(32),
    ] {
        let answer = p.revoke(&mia, &target).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{target}");
    }
    assert!(p.logged_in(&roots).await, "Root's login is untouched");
    assert_eq!(
        p.session_ids("root").await,
        std::slice::from_ref(&root_session)
    );

    let answer = p
        .send(
            Some(&mia),
            Method::POST,
            &format!("/account/sessions/{}/revoke", p.session_id_of(&mia).await),
            Some("csrf=wrong"),
        )
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    let answer = p
        .send(
            None,
            Method::POST,
            &format!("/account/sessions/{root_session}/revoke"),
            Some("csrf=x"),
        )
        .await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert!(p.logged_in(&mia).await);
    assert!(p.logged_in(&roots).await);
}

#[tokio::test]
async fn a_login_remembers_when_it_was_last_used_but_not_on_every_request() {
    let p = Page::new().await;
    let mia = p.login("mia").await;
    let seen = || async {
        p.env
            .store
            .web_session(&mia.token, Timestamp::now())
            .await
            .unwrap()
            .unwrap()
            .last_seen_at
    };
    assert_eq!(seen().await, None);
    // A login that was never used says so, and the one asking has just been used.
    let unused = p.password_login("mia").await;
    let html = p.get(&mia, "/account").await.body;
    assert_eq!(html.matches("not since it began").count(), 1, "{html}");
    assert!(p.logged_in(&unused).await);
    let html = p.get(&mia, "/account").await.body;
    assert!(!html.contains("not since it began"), "{html}");
    let first = seen().await.expect("recorded by the first request");
    p.get(&mia, "/account").await;
    assert_eq!(seen().await, Some(first), "not written again so soon");

    // Eleven minutes after the last time, the next request records it again.
    let old = Timestamp::now()
        .checked_sub(super::reverify::minutes(11))
        .unwrap();
    p.env.store.web_session_seen(&mia.token, old).await.unwrap();
    p.get(&mia, "/account").await;
    assert!(seen().await.unwrap() > old);
    let html = p.get(&mia, "/account").await.body;
    assert!(!html.contains("not since it began"), "{html}");
}

// --- Logins that are not a plain cookie session -------------------------------------------------

/// Mia's login through the trusted proxy: no session of its own.
fn proxy_mia() -> WebUser {
    WebUser {
        user: id("mia"),
        role: doozers_store::Role::Member,
        display_name: "MIA".into(),
        sender: NativeId::new("mia").unwrap(),
        csrf: "csrf-of-proxy".into(),
        login: auth::Login::Proxy,
    }
}

impl Page {
    async fn as_proxy(&self, method: Method, path: &str, form: Option<&str>) -> Response {
        call(
            self.router().layer(Extension(proxy_mia())),
            method,
            path,
            &[],
            form,
        )
        .await
    }
}

#[tokio::test]
async fn a_proxy_login_is_told_why_it_cannot_change_methods_and_marks_no_login_as_its_own() {
    let (p, key) = with_every_method().await;
    let cookie_login = p.login("mia").await;
    let cookie_id = p.session_id_of(&cookie_login).await;

    let html = body_text(p.as_proxy(Method::GET, "/account", None).await).await;
    assert!(html.contains("proxy in front of Doozers"), "{html}");
    assert!(!html.contains("Confirm it's you"), "{html}");
    assert!(!html.contains("/account/sign-in/remove"), "{html}");
    // Its cookie logins are listed, and none is this one.
    assert!(
        html.contains(&format!("/account/sessions/{cookie_id}/revoke")),
        "{html}"
    );
    assert!(!html.contains(r#"pill-ok">this login"#), "{html}");

    let response = p
        .as_proxy(
            Method::POST,
            "/account/sign-in/remove",
            Some(&format!("csrf=csrf-of-proxy&method=passkey:{key}")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        body_text(response)
            .await
            .contains("proxy in front of Doozers")
    );
    assert_eq!(p.methods_left("mia").await, (true, 1, true));

    // Revoking a cookie login ends it, and there is no cookie of the proxy's to delete.
    let response = p
        .as_proxy(
            Method::POST,
            &format!("/account/sessions/{cookie_id}/revoke"),
            Some("csrf=csrf-of-proxy"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/account?notice=revoked");
    assert!(response.headers().get(SET_COOKIE).is_none());
    assert!(!p.logged_in(&cookie_login).await);
}

#[tokio::test]
async fn a_links_session_older_than_thirty_minutes_is_sent_to_confirm_like_any_other() {
    let (p, key) = with_every_method().await;
    let old = p
        .aged_login("mia", SignInMethod::Recovery, minutes(31))
        .await;
    let html = p.get(&old, "/account").await.body;
    assert!(
        html.contains(r#"href="/account/reverify">Confirm it's you"#),
        "{html}"
    );
    assert!(!html.contains("You signed in with a link"), "{html}");
    assert!(!html.contains("/account/sign-in/remove"), "{html}");
    let answer = p
        .post(
            &old,
            "/account/sign-in/remove",
            &[("method", &format!("passkey:{key}"))],
        )
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    // The page it sends to leads back to the account page.
    assert!(
        answer
            .body
            .contains(r#"href="/account/reverify?next=/account""#),
        "{}",
        answer.body
    );
    assert_eq!(p.methods_left("mia").await, (true, 1, true));
}

#[tokio::test]
async fn a_connection_whose_provider_changed_issuer_says_it_no_longer_signs_in() {
    let (p, _) = with_every_method().await;
    let mia = p.login("mia").await;
    let html = p.get(&mia, "/account").await.body;
    assert!(!html.contains("no longer signs you in"), "{html}");
    p.env
        .store
        .oidc_provider_update(
            "idp",
            OidcProviderSettings {
                display_name: "Idp".to_owned(),
                issuer: "https://moved.example.com".to_owned(),
                client_id: "client".to_owned(),
                client_secret: None,
                scopes: vec!["openid".to_owned()],
                enabled: true,
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    let html = p.get(&mia, "/account").await.body;
    assert!(
        html.contains("this account no longer signs you in"),
        "{html}"
    );
    assert!(html.contains("Connect another account"), "{html}");
}

#[tokio::test]
async fn ending_a_login_ends_the_live_streams_it_opened() {
    use std::time::Duration;
    let p = Page::new().await;
    let here = p.login("mia").await;
    let there = p.password_login("mia").await;
    let chat = "0123456789abcdef0123456789abcdef";
    let open = |browser: &Browser| {
        let path = format!("/chats/{chat}/events");
        let cookie = session(&browser.token);
        let router = p.router();
        async move {
            let response = call(router, Method::GET, &path, &[("cookie", &cookie)], None).await;
            assert_eq!(response.status(), StatusCode::OK);
            response.into_body().into_data_stream()
        }
    };
    let ended = |mut stream: axum::body::BodyDataStream| async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            while stream.next().await.is_some() {}
        })
        .await
        .is_ok()
    };

    // Revoked from another browser.
    let mut revoked = open(&there).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(400), revoked.next())
            .await
            .is_err(),
        "the stream stays open"
    );
    let theirs = p.session_id_of(&there).await;
    assert_eq!(p.revoke(&here, &theirs).await.status, StatusCode::SEE_OTHER);
    assert!(ended(revoked).await, "revoking ends the stream");

    // Logged out.
    let third = p.password_login("mia").await;
    let logging_out = open(&third).await;
    let response = call(
        p.router(),
        Method::POST,
        "/logout",
        &[("cookie", &session(&third.token))],
        Some(&format!("csrf={}", third.csrf)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(ended(logging_out).await, "logging out ends the stream");

    // A login that wasn't ended keeps its stream.
    let mut kept = open(&here).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(400), kept.next())
            .await
            .is_err()
    );
}
