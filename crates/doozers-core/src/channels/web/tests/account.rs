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
        let env = env().await;
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
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{path}");
        let status = response.status();
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
