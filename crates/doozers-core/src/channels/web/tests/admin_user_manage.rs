//! The user management pages (`/admin/invite-user`, `/admin/users/<id>` and what it posts to)
//! through the real handlers: requests carry a session cookie, as a browser's do, so the role of
//! the one asking is read from the stored users, and a link is spent the way its user would.

use super::admin::session_of;
use super::admin_users::Pages;
use super::agents::guest;
use super::ssh_keys::url_encode;
use super::*;

/// What a request got back.
struct Reply {
    status: StatusCode,
    body: String,
    location: Option<String>,
}

impl Pages {
    /// `method path` from the browser logged in with `token`: its cookie and, for a POST, its CSRF
    /// token and `pairs`, URL-encoded.
    async fn browse(
        &self,
        token: &str,
        method: Method,
        path: &str,
        pairs: &[(&str, &str)],
    ) -> Reply {
        let mut form = format!("csrf={}", auth::cookie_csrf(token));
        for (name, value) in pairs {
            form.push_str(&format!("&{name}={}", url_encode(value)));
        }
        let cookie = session(token);
        let post = method == Method::POST;
        let response = call(
            self.router(None),
            method,
            path,
            &[("cookie", &cookie)],
            post.then_some(form.as_str()),
        )
        .await;
        let status = response.status();
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{path}");
        let location = response
            .headers()
            .get(LOCATION)
            .map(|l| l.to_str().unwrap().to_owned());
        Reply {
            status,
            body: body_text(response).await,
            location,
        }
    }

    async fn page(&self, token: &str, path: &str) -> Reply {
        self.browse(token, Method::GET, path, &[]).await
    }

    async fn submit(&self, token: &str, path: &str, pairs: &[(&str, &str)]) -> Reply {
        self.browse(token, Method::POST, path, pairs).await
    }

    /// Spends the link at the end of `html` as its user's browser: the session token it starts.
    async fn spend(&self, html: &str) -> String {
        let response = redeem(self.router(None), &link_token(html), &[]).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[LOCATION], "/setup");
        cookie_token(set_cookies(&response)[0])
    }
}

/// The token in the link a page shows.
fn link_token(html: &str) -> String {
    let at = html.find("/invite/").expect("the page shows a link") + "/invite/".len();
    html[at..at + 64].to_owned()
}

/// The user with `username`, whose id is a random one.
fn by_username(p: &Pages, username: &str) -> crate::users::User {
    p.users
        .list()
        .get()
        .by_username(username)
        .unwrap_or_else(|| panic!("no user {username}"))
        .clone()
}

fn role_of(p: &Pages, name: &str) -> UserRole {
    p.user(name).unwrap().role
}

use doozers_store::Role as UserRole;

#[tokio::test]
async fn only_an_admin_manages_users_and_every_post_needs_the_csrf_token() {
    let p = Pages::new().await;
    let mia = session_of(&p.env, "mia").await;
    let owner = session_of(&p.env, "owner").await;
    let routes = [
        (Method::GET, "/admin/invite-user"),
        (Method::POST, "/admin/invite-user"),
        (Method::GET, "/admin/users/mia"),
        (Method::POST, "/admin/users/mia/details"),
        (Method::POST, "/admin/users/mia/role"),
        (Method::POST, "/admin/users/mia/proxy-identity"),
        (Method::POST, "/admin/users/mia/recovery-link"),
    ];
    for (method, path) in &routes {
        let (status, ..) = p.send(None, method.clone(), path, true).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        let (status, body, _) = p.send(Some(guest()), method.clone(), path, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(body.contains("Only an admin"), "{body}");
        // A member's real session is refused too, whatever the form says.
        let pairs = [
            ("username", "taken-by-a-member"),
            ("display_name", "Member"),
            ("role", "admin"),
            ("proxy_identity", "m@example.com"),
        ];
        let reply = p.browse(&mia, method.clone(), path, &pairs).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {path}");
        // An admin's post without the token is refused before anything is read.
        if *method == Method::POST {
            let (status, ..) = p
                .send(Some(owner_user()), method.clone(), path, false)
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        }
    }
    let mia = p.user("mia").unwrap();
    assert_eq!((mia.role, mia.username.as_str()), (UserRole::Member, "mia"));
    assert!(p.user("taken-by-a-member").is_none());

    // An id that isn't a user is a 404, in a path that is shaped like one or not.
    for id in ["nobody", "a%20b"] {
        let reply = p.page(&owner, &format!("/admin/users/{id}")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{id}");
        for action in ["details", "role", "proxy-identity", "recovery-link"] {
            let reply = p
                .submit(
                    &owner,
                    &format!("/admin/users/{id}/{action}"),
                    &[("role", "admin")],
                )
                .await;
            assert_eq!(reply.status, StatusCode::NOT_FOUND, "{id} {action}");
        }
    }
}

fn owner_user() -> WebUser {
    owner()
}

#[tokio::test]
async fn the_list_and_the_page_show_when_a_user_last_signed_in() {
    let p = Pages::new().await;
    let owner = session_of(&p.env, "owner").await;
    let html = p.page(&owner, "/admin/users/mia").await.body;
    assert!(html.contains("<dt>Last sign-in</dt>"), "{html}");
    assert!(html.contains("<dd>Never</dd>"), "{html}");
    let list = p.page(&owner, "/admin/users").await.body;
    assert!(list.contains(r#"href="/admin/invite-user""#), "{list}");

    // Signing in with the link records it, and the page reads the stored time.
    let _mia = session_of(&p.env, "mia").await;
    let html = p.page(&owner, "/admin/users/mia").await.body;
    assert!(!html.contains("<dd>Never</dd>"), "{html}");
    assert!(html.contains(" UTC</dd>"), "{html}");
    let list = p.page(&owner, "/admin/users").await.body;
    assert!(list.contains(r#"href="/admin/users/mia""#), "{list}");
    let row = |name: &str| {
        let start = list.find(&format!(r#"<tr id="user-{name}">"#)).unwrap();
        let end = start + list[start..].find("</tr>").unwrap();
        list[start..end].to_owned()
    };
    assert!(row("mia").contains(" UTC</td>"), "{list}");
    assert!(row("root").contains(">Never</td>"), "{list}");
}

#[tokio::test]
async fn an_invited_user_sets_up_through_the_link_and_the_page_shows_it_once() {
    let p = Pages::new().await;
    let owner = session_of(&p.env, "owner").await;
    let form = p.page(&owner, "/admin/invite-user").await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.body);
    assert!(form.body.contains(r#"name="username""#), "{}", form.body);

    let created = p
        .submit(
            &owner,
            "/admin/invite-user",
            &[
                ("username", "newbie"),
                ("display_name", "New Bie"),
                ("role", "member"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    assert!(
        created.body.contains("<h1>Invite link</h1>"),
        "{}",
        created.body
    );
    assert!(
        created.body.contains("<code>newbie</code>"),
        "{}",
        created.body
    );
    let newbie = by_username(&p, "newbie");
    assert_eq!((newbie.role, newbie.is_active()), (UserRole::Member, true));
    assert_eq!(newbie.display_name, "New Bie");
    // An opaque random id, not the username, and the identity a web login acts as.
    let id = newbie.id.to_string();
    assert!(id.len() == 32 && id != "newbie", "{id}");
    assert_eq!(
        newbie
            .identities
            .iter()
            .map(|i| i.identity.as_str())
            .collect::<Vec<_>>(),
        [format!("web:{id}")]
    );
    // The link is on the answer, with a way to copy it; the user's page doesn't show it again.
    assert!(
        created.body.contains(r#"data-copy-from="link""#),
        "{}",
        created.body
    );
    let page = p.page(&owner, &format!("/admin/users/{id}")).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("<code>newbie</code>"), "{}", page.body);
    assert!(!page.body.contains("/invite/"), "{}", page.body);

    // The invited user opens the link and lands where they set up how they sign in.
    let token = p.spend(&created.body).await;
    let setup = p.page(&token, "/setup").await;
    assert_eq!(setup.status, StatusCode::OK, "{}", setup.body);
    assert!(
        setup
            .body
            .contains(r#"<a href="/setup/password">Password</a>"#),
        "{}",
        setup.body
    );
    let stored = p
        .env
        .store
        .web_session(&token, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.user, newbie.id);
    assert_eq!(stored.method, doozers_store::SignInMethod::Invite);
    // A member, so not the admin pages, and the link works once.
    assert_eq!(
        p.page(&token, "/admin/users").await.status,
        StatusCode::FORBIDDEN
    );
    let again = redeem(p.router(None), &link_token(&created.body), &[]).await;
    assert_eq!(again.status(), StatusCode::GONE);

    // An invited admin has the admin pages, and every user gets an id of their own.
    let boss = p
        .submit(
            &owner,
            "/admin/invite-user",
            &[
                ("username", "boss"),
                ("display_name", "The Boss"),
                ("role", "admin"),
            ],
        )
        .await;
    assert_eq!(boss.status, StatusCode::OK, "{}", boss.body);
    let boss_token = p.spend(&boss.body).await;
    assert_eq!(
        p.page(&boss_token, "/admin/users").await.status,
        StatusCode::OK
    );
    assert_ne!(by_username(&p, "boss").id, newbie.id);
}

#[tokio::test]
async fn an_invite_that_is_refused_shows_the_form_again_with_what_was_typed() {
    let p = Pages::new().await;
    let owner = session_of(&p.env, "owner").await;
    let before = p.users.list().get().users().len();
    let cases = [
        // Another user's username, in any case.
        (
            "MIA",
            "Dup",
            "member",
            StatusCode::CONFLICT,
            "Another user has that username",
        ),
        (
            "bad name",
            "Bad",
            "member",
            StatusCode::BAD_REQUEST,
            "a username is 1 to",
        ),
        (
            "",
            "Empty",
            "member",
            StatusCode::BAD_REQUEST,
            "a username is 1 to",
        ),
        (
            "ok",
            "",
            "member",
            StatusCode::BAD_REQUEST,
            "a display name is 1 to",
        ),
        (
            "ok",
            "Ok",
            "emperor",
            StatusCode::BAD_REQUEST,
            "Choose Member or Admin",
        ),
    ];
    for (username, display, role, status, why) in cases {
        let reply = p
            .submit(
                &owner,
                "/admin/invite-user",
                &[
                    ("username", username),
                    ("display_name", display),
                    ("role", role),
                ],
            )
            .await;
        assert_eq!(reply.status, status, "{username:?}: {}", reply.body);
        assert!(reply.body.contains(why), "{username:?}: {}", reply.body);
        assert!(
            reply
                .body
                .contains(&format!(r#"name="username" value="{username}""#)),
            "{}",
            reply.body
        );
        assert!(
            reply
                .body
                .contains(&format!(r#"name="display_name" value="{display}""#)),
            "{}",
            reply.body
        );
        assert!(!reply.body.contains("/invite/"), "{}", reply.body);
    }
    assert_eq!(
        p.users.list().get().users().len(),
        before,
        "nobody was created"
    );
}

#[tokio::test]
async fn an_admin_edits_a_users_details_and_a_refused_edit_changes_nothing() {
    let p = Pages::new().await;
    let owner = session_of(&p.env, "owner").await;
    let page = p.page(&owner, "/admin/users/mia").await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert!(page.body.contains("<h1>MIA</h1>"), "{}", page.body);
    assert!(
        page.body.contains(r#"name="username" value="mia""#),
        "{}",
        page.body
    );
    assert!(
        page.body.contains(r#"href="/admin/users/mia/remove""#),
        "{}",
        page.body
    );

    let saved = p
        .submit(
            &owner,
            "/admin/users/mia/details",
            &[
                ("username", " mia.b "),
                ("display_name", "Mia <b>B</b>"),
                ("timezone", "Europe/London"),
            ],
        )
        .await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);
    assert_eq!(
        saved.location.as_deref(),
        Some("/admin/users/mia?notice=saved")
    );
    let mia = p.user("mia").unwrap();
    assert_eq!(
        (mia.username.as_str(), mia.display_name.as_str()),
        ("mia.b", "Mia <b>B</b>")
    );
    assert_eq!(
        mia.timezone.as_ref().and_then(|zone| zone.iana_name()),
        Some("Europe/London")
    );
    // Found by the new username, under the id it always had, and everything is escaped.
    let directory = p.users.list().get();
    assert_eq!(directory.by_username("MIA.B").unwrap().id, mia.id);
    assert!(directory.by_username("mia").is_none());
    let html = p.page(&owner, "/admin/users/mia?notice=saved").await.body;
    assert!(html.contains("Saved the details."), "{html}");
    assert!(
        html.contains(r#"name="timezone" value="Europe/London""#),
        "{html}"
    );
    assert!(!html.contains("<b>B</b>"), "{html}");

    // A refused edit shows the page again with why and what was typed, and writes nothing, not
    // even the fields that were fine.
    let cases = [
        (
            "ROOT",
            "Changed",
            "Asia/Tokyo",
            StatusCode::CONFLICT,
            "Another user has that username",
        ),
        (
            "a b",
            "Changed",
            "Asia/Tokyo",
            StatusCode::BAD_REQUEST,
            "a username is 1 to",
        ),
        (
            "mia.b",
            "",
            "Asia/Tokyo",
            StatusCode::BAD_REQUEST,
            "a display name is 1 to",
        ),
        (
            "mia.b",
            "Changed",
            "Mars/Olympus",
            StatusCode::BAD_REQUEST,
            "Mars/Olympus",
        ),
    ];
    for (username, display, zone, status, why) in cases {
        let reply = p
            .submit(
                &owner,
                "/admin/users/mia/details",
                &[
                    ("username", username),
                    ("display_name", display),
                    ("timezone", zone),
                ],
            )
            .await;
        assert_eq!(reply.status, status, "{username:?}: {}", reply.body);
        assert!(reply.body.contains(why), "{username:?}: {}", reply.body);
        assert!(
            reply
                .body
                .contains(&format!(r#"name="username" value="{username}""#)),
            "{}",
            reply.body
        );
        assert!(
            reply
                .body
                .contains(&format!(r#"name="timezone" value="{zone}""#)),
            "{}",
            reply.body
        );
        let now = p.user("mia").unwrap();
        assert_eq!(
            (now.username.as_str(), now.display_name.as_str()),
            ("mia.b", "Mia <b>B</b>"),
            "{username:?}"
        );
        assert_eq!(
            now.timezone.as_ref().and_then(|zone| zone.iana_name()),
            Some("Europe/London")
        );
    }
    // The user's own username can change case, an empty time zone clears it, and the old username
    // is free for another user.
    let recase = p
        .submit(
            &owner,
            "/admin/users/mia/details",
            &[
                ("username", "MIA.B"),
                ("display_name", "Mia"),
                ("timezone", ""),
            ],
        )
        .await;
    assert_eq!(recase.status, StatusCode::SEE_OTHER, "{}", recase.body);
    assert!(p.user("mia").unwrap().timezone.is_none());
    let take = p
        .submit(
            &owner,
            "/admin/users/root/details",
            &[
                ("username", "mia.b2"),
                ("display_name", "Root"),
                ("timezone", ""),
            ],
        )
        .await;
    assert_eq!(take.status, StatusCode::SEE_OTHER, "{}", take.body);
}

#[tokio::test]
async fn an_admin_promotes_and_demotes_and_the_last_active_admin_stays() {
    let p = Pages::new().await;
    let owner = session_of(&p.env, "owner").await;
    let root = session_of(&p.env, "root").await;
    let mia = session_of(&p.env, "mia").await;
    let set = async |token: &str, user: &str, role: &str| {
        p.submit(
            token,
            &format!("/admin/users/{user}/role"),
            &[("role", role)],
        )
        .await
    };

    // Promoting takes effect on the member's next request, and demoting on the admin's.
    assert_eq!(
        p.page(&mia, "/admin/users").await.status,
        StatusCode::FORBIDDEN
    );
    let promoted = set(&owner, "mia", "admin").await;
    assert_eq!(promoted.status, StatusCode::SEE_OTHER, "{}", promoted.body);
    assert_eq!(
        promoted.location.as_deref(),
        Some("/admin/users/mia?notice=role")
    );
    assert_eq!(role_of(&p, "mia"), UserRole::Admin);
    assert_eq!(p.page(&mia, "/admin/users").await.status, StatusCode::OK);
    assert_eq!(
        set(&owner, "root", "member").await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        p.page(&root, "/admin/users").await.status,
        StatusCode::FORBIDDEN
    );

    // A role that isn't one is refused with the page and nothing changes.
    let bad = set(&owner, "mia", "emperor").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    assert!(bad.body.contains("Choose Member or Admin"), "{}", bad.body);
    assert_eq!(role_of(&p, "mia"), UserRole::Admin);

    // The owner and mia are the admins: either can demote the other, and the last can't be.
    assert_eq!(
        set(&mia, "owner", "member").await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(role_of(&p, "owner"), UserRole::Member);
    let last = set(&mia, "mia", "member").await;
    assert_eq!(last.status, StatusCode::CONFLICT, "{}", last.body);
    assert!(last.body.contains("last active admin"), "{}", last.body);
    assert_eq!(role_of(&p, "mia"), UserRole::Admin);

    // An admin demoting themselves while another remains goes to the home page, and has lost the
    // admin pages.
    assert_eq!(
        set(&mia, "root", "admin").await.status,
        StatusCode::SEE_OTHER
    );
    let own = set(&mia, "mia", "member").await;
    assert_eq!(own.status, StatusCode::SEE_OTHER, "{}", own.body);
    assert_eq!(own.location.as_deref(), Some("/"));
    assert_eq!(
        p.page(&mia, "/admin/users").await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(p.page(&root, "/admin/users").await.status, StatusCode::OK);
}

#[tokio::test]
async fn an_admin_sets_shows_and_clears_a_proxy_identity() {
    let p = Pages::new().await;
    let owner = session_of(&p.env, "owner").await;
    let set = async |user: &str, value: &str| {
        p.submit(
            &owner,
            &format!("/admin/users/{user}/proxy-identity"),
            &[("proxy_identity", value)],
        )
        .await
    };
    let identities = |name: &str| -> Vec<String> {
        p.user(name)
            .unwrap()
            .identities
            .iter()
            .map(|i| i.identity.as_str().to_owned())
            .collect()
    };
    assert!(
        p.page(&owner, "/admin/users/mia")
            .await
            .body
            .contains("They have none now.")
    );

    let saved = set("mia", " mia@corp.example ").await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER, "{}", saved.body);
    assert_eq!(
        saved.location.as_deref(),
        Some("/admin/users/mia?notice=proxy")
    );
    assert_eq!(identities("mia"), ["web:mia", "web:mia@corp.example"]);
    let html = p.page(&owner, "/admin/users/mia?notice=proxy").await.body;
    assert!(
        html.contains(r#"name="proxy_identity" value="mia@corp.example""#),
        "{html}"
    );
    assert!(
        html.contains("<code>web:mia@corp.example</code> now."),
        "{html}"
    );
    // The admin verified it: no link code was involved.
    let stored = p
        .env
        .store
        .user(&UserId::new("mia").unwrap())
        .await
        .unwrap()
        .unwrap();
    let proxy = stored
        .identities
        .iter()
        .find(|i| i.native_id == "mia@corp.example")
        .unwrap();
    assert_eq!(proxy.verification, doozers_store::Verification::Admin);

    // Another user's, the login identity, and one that isn't an identity are refused with the
    // value typed, and mia keeps hers.
    let cases = [
        (
            "mia@corp.example",
            "root",
            StatusCode::CONFLICT,
            "Another user has the identity",
        ),
        ("a b", "mia", StatusCode::BAD_REQUEST, "identity"),
        (
            "web:mia@corp.example",
            "mia",
            StatusCode::BAD_REQUEST,
            "Leave off web:",
        ),
        (
            "mia",
            "mia",
            StatusCode::BAD_REQUEST,
            "link sign-in acts as",
        ),
    ];
    for (value, user, status, why) in cases {
        let reply = set(user, value).await;
        assert_eq!(reply.status, status, "{value:?}: {}", reply.body);
        assert!(reply.body.contains(why), "{value:?}: {}", reply.body);
        assert!(
            reply
                .body
                .contains(&format!(r#"name="proxy_identity" value="{value}""#)),
            "{}",
            reply.body
        );
    }
    assert_eq!(identities("mia"), ["web:mia", "web:mia@corp.example"]);
    assert_eq!(identities("root"), ["web:root"]);

    // Saving it empty removes it.
    let cleared = set("mia", "").await;
    assert_eq!(cleared.status, StatusCode::SEE_OTHER, "{}", cleared.body);
    assert_eq!(
        cleared.location.as_deref(),
        Some("/admin/users/mia?notice=proxy-cleared")
    );
    assert_eq!(identities("mia"), ["web:mia"]);
}

#[tokio::test]
async fn a_recovery_link_from_the_users_page_signs_the_user_in_once() {
    let p = Pages::new().await;
    let owner = session_of(&p.env, "owner").await;
    let link = p
        .submit(&owner, "/admin/users/mia/recovery-link", &[])
        .await;
    assert_eq!(link.status, StatusCode::OK, "{}", link.body);
    assert!(
        link.body.contains("<h1>Recovery link</h1>"),
        "{}",
        link.body
    );
    assert!(link.body.contains("<code>mia</code>"), "{}", link.body);
    assert!(
        link.body.contains("Doozers shows the link only now"),
        "{}",
        link.body
    );
    assert!(
        link.body
            .contains(&p.env.channel.invite_url(&link_token(&link.body)))
    );

    let token = p.spend(&link.body).await;
    let stored = p
        .env
        .store
        .web_session(&token, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.user, UserId::new("mia").unwrap());
    assert_eq!(stored.method, doozers_store::SignInMethod::Recovery);
    let again = redeem(p.router(None), &link_token(&link.body), &[]).await;
    assert_eq!(again.status(), StatusCode::GONE);
}

#[tokio::test]
async fn a_disabled_user_gets_no_recovery_link() {
    let p = Pages::new().await;
    let owner = session_of(&p.env, "owner").await;
    let mia = UserId::new("mia").unwrap();
    p.users
        .set_state(&mia, doozers_store::UserState::Disabled, Timestamp::now())
        .await
        .unwrap();
    let refused = p
        .submit(&owner, "/admin/users/mia/recovery-link", &[])
        .await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert!(refused.body.contains("is disabled"), "{}", refused.body);
    assert!(!refused.body.contains("/invite/"), "{}", refused.body);
    assert_eq!(p.env.store.user_data(&mia).await.unwrap().invites, 0);
    // The page offers none either.
    let page = p.page(&owner, "/admin/users/mia").await;
    assert!(page.body.contains("is disabled"), "{}", page.body);
    assert!(
        !page
            .body
            .contains(r#"action="/admin/users/mia/recovery-link""#),
        "{}",
        page.body
    );
}
