//! Router-level tests of the Identities page (`/account/identities`) over stored users, a `Links`
//! service with a fixed set of channel namespaces, and the real session pump behind `/link`.

use std::collections::BTreeSet;

use doozers_store::{UserState, Verification};

use super::admin::stored_users;
use super::ssh_keys::url_encode;
use super::*;
use crate::config::Identity;
use crate::links::{CODE_ATTEMPTS, CODE_TTL, ChannelNamespaces, Links};
use crate::router::LinkRequest;
use crate::sessions::Linker;
use crate::users::Users;

const PAGE: &str = "/account/identities";

struct Namespaces;

impl ChannelNamespaces for Namespaces {
    fn namespaces(&self) -> BTreeSet<String> {
        ["web", "whatsapp", "discord", "irc:libera.chat"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
}

struct Page {
    env: Env,
    users: Arc<Users>,
    links: Arc<Links>,
}

/// What a request got back.
struct Answer {
    status: StatusCode,
    body: String,
    location: Option<String>,
}

fn member(id: &str) -> WebUser {
    WebUser {
        user: UserId::new(id).unwrap(),
        role: doozers_store::Role::Member,
        sender: NativeId::new(id).unwrap(),
        csrf: format!("csrf-of-{id}"),
        display_name: "Test user".into(),
        login: auth::Login::Test,
    }
}

impl Page {
    /// The owner (an admin), `root` (an admin), and `mia` (a member), each with `web:<id>`.
    async fn new() -> Self {
        let env = env().await;
        let users = stored_users(&env).await;
        let links = Arc::new(Links::new(
            env.store.clone(),
            users.clone(),
            Arc::new(Namespaces),
            Arc::new(SystemClock),
        ));
        Self { env, users, links }
    }

    fn router(&self, user: Option<WebUser>) -> axum::Router {
        let app = self
            .env
            .app
            .install_identities(
                self.env.app.install(RouterBuilder::new()),
                self.links.clone(),
            )
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    async fn send(&self, user: &WebUser, method: Method, path: &str, form: Option<&str>) -> Answer {
        let response = call(self.router(Some(user.clone())), method, path, &[], form).await;
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
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

    async fn get(&self, user: &WebUser, path: &str) -> Answer {
        self.send(user, Method::GET, path, None).await
    }

    /// A POST of `pairs` and the user's CSRF token, URL-encoded.
    async fn post(&self, user: &WebUser, path: &str, pairs: &[(&str, &str)]) -> Answer {
        let mut form = format!("csrf={}", user.csrf);
        for (name, value) in pairs {
            form.push_str(&format!("&{name}={}", url_encode(value)));
        }
        self.send(user, Method::POST, path, Some(&form)).await
    }

    /// `user` makes a code for `namespace` on the page: the code, from the page it was shown on.
    async fn make_code(&self, user: &WebUser, namespace: &str) -> String {
        let answer = self
            .post(
                user,
                "/account/identities/link",
                &[("namespace", namespace)],
            )
            .await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        command_of(&answer.body)
            .strip_prefix("/link ")
            .unwrap()
            .to_owned()
    }

    /// `sender` sends `/link <code>` from `namespace`, as the core would handle it.
    async fn send_link(&self, namespace: &str, sender: &str, code: &str) -> String {
        let request = LinkRequest {
            namespace: namespace.to_owned(),
            sender: NativeId::new(sender).unwrap(),
            conversation: ConversationId::new(sender).unwrap(),
            code: Some(code.to_owned()),
            linked: false,
        };
        self.links
            .link(&request, Timestamp::now())
            .await
            .unwrap()
            .unwrap()
    }

    /// Adds `mia` the identity `<namespace>:<sender>` by the whole flow.
    async fn link(&self, namespace: &str, sender: &str) {
        let mia = member("mia");
        let code = self.make_code(&mia, namespace).await;
        let reply = self.send_link(namespace, sender, &code).await;
        assert!(reply.starts_with("Done."), "{reply}");
    }
}

/// The message the page shows after making a code: the value of the read-only field.
fn command_of(body: &str) -> String {
    let start = body
        .find("id=\"link-command\"")
        .expect("no code on the page");
    let rest = &body[start..];
    let value = &rest[rest.find("value=\"").unwrap() + 7..];
    value[..value.find('"').unwrap()].to_owned()
}

#[tokio::test]
async fn the_page_needs_a_login_and_lists_only_the_users_own_identities() {
    let p = Page::new().await;
    let response = call(p.router(None), Method::GET, PAGE, &[], None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let mia = member("mia");
    let answer = p.get(&mia, PAGE).await;
    assert_eq!(answer.status, StatusCode::OK);
    assert!(
        answer.body.contains("<code>web:mia</code>"),
        "{}",
        answer.body
    );
    assert!(answer.body.contains("Signs you in to the web chat"));
    // Nobody else's identity, and nothing to remove: the web sign-in stays.
    assert!(!answer.body.contains("web:root"));
    assert!(!answer.body.contains("web:owner"));
    assert!(!answer.body.contains("/account/identities/remove?"));
    // The sidebar marks the Account link current: the page belongs to the account area, and
    // says so with a link back.
    assert!(
        answer
            .body
            .contains("href=\"/account\" aria-current=\"page\"")
    );
    assert!(answer.body.contains(r#"<a class="back" href="/account">"#));
    // The form offers the channels that run, never the web chat, and nothing is pending yet.
    for namespace in ["whatsapp", "discord", "irc:libera.chat"] {
        assert!(
            answer
                .body
                .contains(&format!("<option value=\"{namespace}\"")),
            "{namespace}"
        );
    }
    assert!(!answer.body.contains("<option value=\"web\""));
    assert!(!answer.body.contains("<h2>Codes</h2>"));
}

#[tokio::test]
async fn a_code_is_shown_once_and_adding_an_identity_with_it_puts_it_on_the_page() {
    let p = Page::new().await;
    let mia = member("mia");
    let answer = p
        .post(
            &mia,
            "/account/identities/link",
            &[("namespace", "whatsapp")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    assert_eq!(answer.location, None, "the code is in no redirect");
    let command = command_of(&answer.body);
    let code = command.strip_prefix("/link ").unwrap();
    assert_eq!(code.len(), 8);
    assert!(answer.body.contains("Send this from your whatsapp account"));
    assert!(answer.body.contains(" UTC"), "the page says when it stops");
    // The list says a code is pending, without the code, and the code is shown nowhere again.
    let again = p.get(&mia, PAGE).await;
    assert!(!again.body.contains(code), "{}", again.body);
    assert!(again.body.contains("<h2>Codes</h2>"));
    assert!(again.body.contains("Pending"));
    assert!(again.body.contains("works until"));

    // Sending the message from a phone adds the identity, as a link, and the page lists it.
    let reply = p.send_link("whatsapp", "+15551234567", code).await;
    assert!(reply.starts_with("Done."), "{reply}");
    let after = p.get(&mia, PAGE).await;
    assert!(
        after.body.contains("<code>whatsapp:+15551234567</code>"),
        "{}",
        after.body
    );
    assert!(after.body.contains("Linked by you with a code"));
    assert!(
        after
            .body
            .contains("/account/identities/remove?identity=whatsapp%3A%2B15551234567")
    );
    assert!(
        !after.body.contains("<h2>Codes</h2>"),
        "a used code is gone"
    );
    assert_eq!(
        p.env
            .channel
            .users()
            .get()
            .resolve("whatsapp", &NativeId::new("+15551234567").unwrap())
            .unwrap()
            .id,
        mia.user
    );
    // It was a link, not an admin's or an import.
    let stored = p.env.store.user(&mia.user).await.unwrap().unwrap();
    let linked = stored
        .identities
        .iter()
        .find(|i| i.namespace == "whatsapp")
        .unwrap();
    assert_eq!(linked.verification, Verification::Link);
    // Another user's page doesn't have it.
    let root = p.get(&member("root"), PAGE).await;
    assert!(!root.body.contains("+15551234567"));
}

#[tokio::test]
async fn a_new_code_replaces_the_old_and_the_old_stops_working() {
    let p = Page::new().await;
    let mia = member("mia");
    let first = p.make_code(&mia, "discord").await;
    let second = p.make_code(&mia, "discord").await;
    assert_ne!(first, second);
    let reply = p.send_link("discord", "82198898841029460", &first).await;
    assert!(reply.starts_with("That code didn't work."), "{reply}");
    let reply = p.send_link("discord", "82198898841029460", &second).await;
    assert!(reply.starts_with("Done."), "{reply}");
}

#[tokio::test]
async fn a_namespace_no_channel_reports_is_refused_with_the_ones_that_exist() {
    let p = Page::new().await;
    let mia = member("mia");
    for namespace in ["discord:discord", "", "slack"] {
        let answer = p
            .post(
                &mia,
                "/account/identities/link",
                &[("namespace", namespace)],
            )
            .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{namespace:?}");
        assert!(
            answer
                .body
                .contains("No running channel uses the identity namespace"),
            "{}",
            answer.body
        );
        // The hint is the list of namespaces that exist, and the form is still there to fix.
        assert!(
            answer
                .body
                .contains("`discord`, `irc:libera.chat`, `whatsapp`"),
            "{}",
            answer.body
        );
        assert!(answer.body.contains("action=\"/account/identities/link\""));
        assert!(!answer.body.contains("id=\"link-command\""));
    }
    let answer = p
        .post(&mia, "/account/identities/link", &[("namespace", "web")])
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(answer.body.contains("Identities in the web namespace aren"));
    // No code was made.
    assert!(p.links.codes(&mia.user).await.unwrap().is_empty());
}

#[tokio::test]
async fn expired_and_cancelled_codes_are_shown_as_such() {
    let p = Page::new().await;
    let mia = member("mia");
    let past = Timestamp::now().checked_sub(CODE_TTL * 2).unwrap();
    p.env
        .store
        .user_link_code_create(
            &mia.user,
            "discord",
            "ABCD2345",
            [3; 16],
            past,
            past.checked_add(CODE_TTL).unwrap(),
        )
        .await
        .unwrap();
    p.make_code(&mia, "whatsapp").await;
    for _ in 0..CODE_ATTEMPTS {
        p.users
            .redeem_link_code(
                "whatsapp",
                "+1555",
                "ZZZZ2222",
                Timestamp::now(),
                CODE_ATTEMPTS,
            )
            .await
            .unwrap();
    }
    let answer = p.get(&mia, PAGE).await;
    assert!(answer.body.contains("Expired"), "{}", answer.body);
    assert!(answer.body.contains("ran out at"));
    assert!(answer.body.contains("Cancelled"));
    assert!(answer.body.contains("too many wrong codes were sent"));
    assert!(!answer.body.contains("ABCD2345"));
}

#[tokio::test]
async fn every_change_needs_the_csrf_token() {
    let p = Page::new().await;
    p.link("whatsapp", "+15551234567").await;
    let mia = member("mia");
    let before = p.env.store.user(&mia.user).await.unwrap().unwrap();
    for (path, form) in [
        ("/account/identities/link", "namespace=whatsapp"),
        ("/account/identities/default", "identity=web%3Amia"),
        (
            "/account/identities/remove",
            "identity=whatsapp%3A%2B15551234567",
        ),
    ] {
        for body in [form.to_owned(), format!("csrf=wrong&{form}")] {
            let answer = p.send(&mia, Method::POST, path, Some(&body)).await;
            assert_eq!(answer.status, StatusCode::FORBIDDEN, "{path}");
        }
    }
    assert_eq!(p.env.store.user(&mia.user).await.unwrap().unwrap(), before);
    assert!(p.links.codes(&mia.user).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_default_identity_is_changed_for_the_user_and_nobody_else() {
    let p = Page::new().await;
    p.link("whatsapp", "+15551234567").await;
    let mia = member("mia");
    let answer = p.get(&mia, PAGE).await;
    // `web:mia` was the first, so it is the default and the phone has the button.
    assert!(
        answer
            .body
            .contains("<code>web:mia</code> <span class=\"pill pill-ok\">default</span>")
    );
    assert!(
        answer
            .body
            .contains("Make whatsapp:+15551234567 the default")
    );

    let answer = p
        .post(
            &mia,
            "/account/identities/default",
            &[("identity", "whatsapp:+15551234567")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some("/account/identities?notice=default")
    );
    let stored = p.env.store.user(&mia.user).await.unwrap().unwrap();
    assert_eq!(
        stored.default_identity.as_deref(),
        Some("whatsapp:+15551234567")
    );
    let answer = p.get(&mia, "/account/identities?notice=default").await;
    assert!(answer.body.contains("Changed your default identity."));
    assert!(answer.body.contains(
        "<code>whatsapp:+15551234567</code> <span class=\"pill pill-ok\">default</span>"
    ));
    assert!(answer.body.contains("Make web:mia the default"));

    // Another user's identity, one nobody has, and nonsense are 404s that change nothing.
    for identity in ["web:root", "whatsapp:+19990000000", "no-namespace", ""] {
        let answer = p
            .post(
                &mia,
                "/account/identities/default",
                &[("identity", identity)],
            )
            .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{identity:?}");
    }
    let root = p
        .env
        .store
        .user(&UserId::new("root").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root.default_identity.as_deref(), Some("web:root"));
    assert_eq!(
        p.env
            .store
            .user(&mia.user)
            .await
            .unwrap()
            .unwrap()
            .default_identity
            .as_deref(),
        Some("whatsapp:+15551234567")
    );
}

#[tokio::test]
async fn removing_an_identity_asks_first_and_never_touches_another_users_or_the_web_sign_in() {
    let p = Page::new().await;
    p.link("whatsapp", "+15551234567").await;
    p.link("discord", "82198898841029460").await;
    let mia = member("mia");
    p.post(
        &mia,
        "/account/identities/default",
        &[("identity", "discord:82198898841029460")],
    )
    .await;

    let ask = p
        .get(
            &mia,
            "/account/identities/remove?identity=discord%3A82198898841029460",
        )
        .await;
    assert_eq!(ask.status, StatusCode::OK);
    assert!(
        ask.body.contains("Remove discord:82198898841029460?"),
        "{}",
        ask.body
    );
    assert!(ask.body.contains("It is your default identity"));
    // Asking changes nothing.
    assert!(
        p.env
            .channel
            .users()
            .get()
            .resolve("discord", &NativeId::new("82198898841029460").unwrap())
            .is_some()
    );

    let answer = p
        .post(
            &mia,
            "/account/identities/remove",
            &[("identity", "discord:82198898841029460")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some("/account/identities?notice=removed")
    );
    assert!(
        p.env
            .channel
            .users()
            .get()
            .resolve("discord", &NativeId::new("82198898841029460").unwrap())
            .is_none()
    );
    let stored = p.env.store.user(&mia.user).await.unwrap().unwrap();
    // The oldest identity left is the default.
    assert_eq!(stored.default_identity.as_deref(), Some("web:mia"));
    let page = p.get(&mia, "/account/identities?notice=removed").await;
    assert!(page.body.contains("Removed the identity."));

    // Another user's identity, the web sign-in, an identity nobody has, and nonsense: 404 on the
    // question and the post, and nothing is removed.
    for identity in ["web:root", "web:mia", "discord:82198898841029460", "x", ""] {
        let query = format!(
            "/account/identities/remove?identity={}",
            url_encode(identity)
        );
        assert_eq!(
            p.get(&mia, &query).await.status,
            StatusCode::NOT_FOUND,
            "{identity:?}"
        );
        let answer = p
            .post(
                &mia,
                "/account/identities/remove",
                &[("identity", identity)],
            )
            .await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND, "{identity:?}");
    }
    let directory = p.env.channel.users().get();
    assert_eq!(
        directory
            .resolve("web", &NativeId::new("root").unwrap())
            .unwrap()
            .id
            .as_str(),
        "root"
    );
    assert_eq!(
        directory
            .resolve("web", &NativeId::new("mia").unwrap())
            .unwrap()
            .id
            .as_str(),
        "mia"
    );
    assert!(
        directory
            .resolve("whatsapp", &NativeId::new("+15551234567").unwrap())
            .is_some()
    );
}

#[tokio::test]
async fn what_an_identity_says_is_escaped_and_each_kind_says_how_it_was_added() {
    let p = Page::new().await;
    let mia = UserId::new("mia").unwrap();
    let now = Timestamp::now();
    p.users
        .add_identity(
            &mia,
            "email",
            "<b>x</b>@example.com",
            Verification::Imported,
            now,
        )
        .await
        .unwrap();
    p.users
        .add_identity(
            &mia,
            "irc:libera.chat",
            "account:mia",
            Verification::Admin,
            now,
        )
        .await
        .unwrap();
    p.users
        .set_proxy_identity(&mia, Some("mia@proxy.example"), now)
        .await
        .unwrap();
    let answer = p.get(&member("mia"), PAGE).await;
    assert!(!answer.body.contains("<b>x</b>"), "{}", answer.body);
    assert!(answer.body.contains("&#60;b&#62;x&#60;"));
    assert!(answer.body.contains("Imported from doozers.toml"));
    assert!(answer.body.contains("Set by an admin<"));
    assert!(
        answer
            .body
            .contains("Set by an admin for the trusted proxy")
    );
    // The proxy identity is the admin's, so the page offers no way to remove it.
    assert!(!answer.body.contains("mia%40proxy.example"));
    // The escaped identity can be removed through the encoded link.
    let ask = p
        .get(
            &member("mia"),
            "/account/identities/remove?identity=email%3A%3Cb%3Ex%3C%2Fb%3E%40example.com",
        )
        .await;
    assert_eq!(ask.status, StatusCode::OK);
    assert!(!ask.body.contains("<b>x</b>"));
    let identity = Identity::new("email:<b>x</b>@example.com").unwrap();
    assert!(p.links.remove_identity(&mia, &identity).await.is_ok());
}

#[tokio::test]
async fn a_disabled_user_cant_make_a_code() {
    let p = Page::new().await;
    p.users
        .set_state(
            &UserId::new("mia").unwrap(),
            UserState::Disabled,
            Timestamp::now(),
        )
        .await
        .unwrap();
    let answer = p
        .post(
            &member("mia"),
            "/account/identities/link",
            &[("namespace", "whatsapp")],
        )
        .await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN, "{}", answer.body);
}
