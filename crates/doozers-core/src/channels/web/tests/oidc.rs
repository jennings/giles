//! Signing in with OpenID Connect through the real handlers, against a fake provider
//! ([`FakeIdp`]) that runs in this process: connecting an account from the setup page, signing in
//! with it later, and everything that must be refused: an account nobody connected, an ID token
//! that doesn't check out in any of the ways it can fail, and a callback that isn't the one of
//! the sign-in in flight.
//!
//! The users are the ones [`stored_users`] makes (`root`, an admin, and `mia`, a member) and
//! `newbie`, whom an invite sets up. The browser is [`Browser`]: it keeps cookies, and the test
//! carries it to the provider and back.

use std::collections::BTreeMap;

use axum::extract::ConnectInfo;
use axum::http::header::RETRY_AFTER;
use doozers_store::{OidcProviderSettings, UserState};
use url::Url;

use super::admin::{open_users, stored_users};
use super::fake_oidc::{FakeIdp, Tamper};
use super::*;
use crate::channels::web::oidc::{self, Oidc, Purpose};
use crate::config::Secret;
use crate::http::PeerAddr;
use crate::secrets::{KeyFile, LocalSecrets, SecretPath, SecretProvider};

/// The address the provider is told to send the browser back to starts with this.
const BASE: &str = "http://127.0.0.1:8080";
const CLIENT_ID: &str = "doozers-client";
const CLIENT_SECRET: &str = "s3cret-value";
const SECRET_PATH: &str = "oidc/idp/client-secret";

/// A browser: the cookies it keeps, and requests that carry them. A cookie goes only to the paths
/// its `Path` says, as a browser's does. It starts with the sign-in nonce cookie the login page
/// gives, which the links on the page carry ([`login_path`]).
struct Browser {
    cookies: BTreeMap<String, String>,
    /// The `Path` of each cookie a response set; a cookie put in `cookies` by hand has `/`.
    paths: BTreeMap<String, String>,
}

impl Default for Browser {
    fn default() -> Self {
        Self {
            cookies: BTreeMap::from([(NONCE_COOKIE.to_owned(), NONCE.to_owned())]),
            paths: BTreeMap::new(),
        }
    }
}

/// Whether a cookie for `cookie_path` goes to `request`'s path (RFC 6265 section 5.1.4).
fn path_matches(cookie_path: &str, request: &str) -> bool {
    let path = request.split('?').next().unwrap_or_default();
    path == cookie_path
        || (path.starts_with(cookie_path)
            && (cookie_path.ends_with('/') || path[cookie_path.len()..].starts_with('/')))
}

/// The link on the login page that signs in at provider `id`, with the page's nonce.
fn login_path(id: &str) -> String {
    format!("/login/oidc/{id}?nonce={NONCE}")
}

impl Browser {
    /// Keeps the cookies `response` sets and forgets the ones it deletes.
    fn absorb(&mut self, response: &Response) {
        for set in set_cookies(response) {
            let (pair, attributes) = set.split_once(';').unwrap_or((set, ""));
            let (name, value) = pair.split_once('=').unwrap();
            if value.is_empty() || attributes.contains("Max-Age=0") {
                self.cookies.remove(name);
                self.paths.remove(name);
            } else {
                self.cookies.insert(name.to_owned(), value.to_owned());
                let path = attributes
                    .split(';')
                    .find_map(|attribute| attribute.trim().strip_prefix("Path="))
                    .unwrap_or("/");
                self.paths.insert(name.to_owned(), path.to_owned());
            }
        }
    }

    async fn send(
        &mut self,
        router: &axum::Router,
        method: Method,
        path: &str,
        form: Option<&str>,
    ) -> Response {
        let cookie = self
            .cookies
            .iter()
            .filter(|(name, _)| {
                path_matches(self.paths.get(*name).map_or("/", String::as_str), path)
            })
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        let headers: Vec<(&str, &str)> = if cookie.is_empty() {
            Vec::new()
        } else {
            vec![("cookie", cookie.as_str())]
        };
        let response = call(router.clone(), method, path, &headers, form).await;
        self.absorb(&response);
        response
    }

    async fn get(&mut self, router: &axum::Router, path: &str) -> Response {
        self.send(router, Method::GET, path, None).await
    }

    /// The session cookie, if the browser has one.
    fn session(&self) -> Option<&str> {
        self.cookies.get(SESSION_COOKIE).map(String::as_str)
    }

    /// The CSRF token of the browser's login.
    fn csrf(&self) -> String {
        auth::cookie_csrf(self.session().expect("a login"))
    }
}

/// The provider, the web app wired to it, and a provider `idp` configured for it.
struct Site {
    env: Env,
    idp: FakeIdp,
    secrets: Arc<dyn SecretProvider>,
    router: axum::Router,
}

fn settings(issuer: &str) -> OidcProviderSettings {
    OidcProviderSettings {
        display_name: "Idp".to_owned(),
        issuer: issuer.to_owned(),
        client_id: CLIENT_ID.to_owned(),
        client_secret: Some(SECRET_PATH.to_owned()),
        scopes: vec!["openid".to_owned(), "profile".to_owned()],
        enabled: true,
    }
}

async fn site() -> Site {
    site_with(env().await).await
}

async fn site_with(env: Env) -> Site {
    let idp = FakeIdp::start(CLIENT_ID, Some(CLIENT_SECRET)).await;
    let root = env.root();
    let secrets: Arc<dyn SecretProvider> = Arc::new(
        LocalSecrets::open(
            env.store.clone(),
            &KeyFile::new(root, &root.secret_key_file()),
            Arc::new(SystemClock),
        )
        .await
        .unwrap(),
    );
    secrets
        .put(
            &SecretPath::new(SECRET_PATH).unwrap(),
            &Secret::new(CLIENT_SECRET),
            None,
        )
        .await
        .unwrap();
    let users = stored_users(&env).await;
    add_user(&users, "newbie", doozers_store::Role::Member).await;
    env.store
        .oidc_provider_add("idp", settings(&idp.issuer), Timestamp::now())
        .await
        .unwrap();
    let router = env
        .app
        .install_oidc(env.app.install(RouterBuilder::new()), secrets.clone())
        .build();
    Site {
        env,
        idp,
        secrets,
        router,
    }
}

fn id(user: &str) -> UserId {
    UserId::new(user).unwrap()
}

fn location(response: &Response) -> String {
    response.headers()[LOCATION].to_str().unwrap().to_owned()
}

/// The path of an absolute URL to this site.
fn path_of(url: &str) -> String {
    url.strip_prefix(BASE)
        .unwrap_or_else(|| panic!("{url}"))
        .to_owned()
}

impl Site {
    /// A browser that opened an invite link for `user`: it has their session.
    async fn invited(&self, user: &str) -> Browser {
        let token = invite_token(&self.env, &id(user), InviteKind::Invite).await;
        let response = redeem(self.router.clone(), &token, &[]).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{user}");
        let mut browser = Browser::default();
        browser.absorb(&response);
        browser
    }

    /// The user's account at the provider is connected, without the pages.
    async fn connected(&self, user: &str, subject: &str) {
        self.env
            .store
            .user_oidc_connect(
                "idp",
                &self.idp.issuer,
                subject,
                &id(user),
                Timestamp::now(),
            )
            .await
            .unwrap();
    }

    /// Opens `path` (a link on a page), goes to the provider as `subject`, and comes back: the
    /// answer to the callback.
    async fn through_idp(&self, browser: &mut Browser, path: &str, subject: &str) -> Response {
        let start = browser.get(&self.router, path).await;
        assert_eq!(start.status(), StatusCode::SEE_OTHER, "{path}");
        let back = self
            .idp
            .approve(&location(&start), subject)
            .unwrap_or_else(|why| panic!("{why}"));
        browser.get(&self.router, &path_of(&back)).await
    }

    /// A sign-in from the login page as `subject`.
    async fn sign_in(&self, browser: &mut Browser, subject: &str) -> Response {
        self.through_idp(browser, &login_path("idp"), subject).await
    }

    /// A connection from the setup page of a browser that has a login.
    async fn connect(&self, browser: &mut Browser, subject: &str) -> Response {
        let path = format!("/setup/oidc/idp?csrf={}", browser.csrf());
        self.through_idp(browser, &path, subject).await
    }

    async fn connection(&self, subject: &str) -> Option<UserId> {
        self.env
            .store
            .user_oidc_user("idp", &self.idp.issuer, subject)
            .await
            .unwrap()
    }
}

/// Asserts that `response` is the refusal of a sign-in, and that the browser has no login.
async fn assert_refused(response: Response, browser: &Browser, what: &str) {
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "{what}");
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{what}");
    assert!(browser.session().is_none(), "{what}: logged in");
    let html = body_text(response).await;
    assert!(
        html.contains("Couldn&#39;t sign you in") || html.contains("Couldn't sign you in"),
        "{what}: {html}"
    );
}

#[tokio::test]
async fn an_invited_user_connects_an_account_and_signs_in_with_it_later() {
    let site = site().await;

    // The invite signs the browser in, and the setup page offers the provider, with a link that
    // starts the connection.
    let mut browser = site.invited("newbie").await;
    let response = browser.get(&site.router, "/setup").await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    let link = format!("/setup/oidc/idp?csrf={}", browser.csrf());
    assert!(html.contains(&format!(r#"href="{link}""#)), "{html}");
    assert!(html.contains("Sign in with your Idp account"), "{html}");

    // The link sends the browser to the provider with a PKCE challenge, a state, a nonce, and the
    // callback Doozers registered, and binds the sign-in to this browser.
    let start = browser.get(&site.router, &link).await;
    assert_eq!(start.status(), StatusCode::SEE_OTHER);
    assert_eq!(start.headers()[CACHE_CONTROL], "no-store");
    let url = Url::parse(&location(&start)).unwrap();
    assert_eq!(url.path(), "/authorize");
    let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query["response_type"], "code");
    assert_eq!(query["client_id"], CLIENT_ID);
    assert_eq!(
        query["redirect_uri"],
        format!("{BASE}/auth/oidc/idp/callback")
    );
    assert_eq!(query["scope"], "openid profile");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["code_challenge"].len(), 43);
    assert_eq!(query["state"].len(), 32);
    assert!(query["nonce"].len() >= 16);
    assert!(!location(&start).contains(CLIENT_SECRET));
    let binding = set_cookies(&start)
        .into_iter()
        .find(|cookie| cookie.starts_with("doozers_oidc="))
        .unwrap();
    assert!(
        binding.ends_with("; Path=/; Max-Age=600; HttpOnly; SameSite=Lax"),
        "{binding}"
    );

    // Back from the provider, the account is the user's, and the page they land on says so.
    let back = site.idp.approve(&location(&start), "newbie-sub").unwrap();
    let done = browser.get(&site.router, &path_of(&back)).await;
    assert_eq!(done.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&done), "/setup");
    assert!(
        browser.cookies.contains_key("doozers_oidc"),
        "the binding stays for another tab"
    );
    assert_eq!(site.connection("newbie-sub").await, Some(id("newbie")));
    let html = body_text(browser.get(&site.router, "/setup").await).await;
    assert!(html.contains("You are connected"), "{html}");

    // Later, another browser: the login page offers the provider, and signing in with it logs
    // the user in as OpenID Connect.
    let mut later = Browser::default();
    let login = body_text(later.get(&site.router, "/login").await).await;
    assert!(
        login.contains(&format!(
            r#"<a href="/login/oidc/idp?nonce={NONCE}">Sign in with Idp</a>"#
        )),
        "{login}"
    );
    let done = site.sign_in(&mut later, "newbie-sub").await;
    assert_eq!(done.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&done), "/");
    assert_eq!(done.headers()[CACHE_CONTROL], "no-store");
    let token = later.session().unwrap().to_owned();
    let session = site
        .env
        .store
        .web_session(&token, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.user, id("newbie"));
    assert_eq!(session.method, SignInMethod::Oidc);
    // The session is a login: the chat list opens.
    assert_eq!(later.get(&site.router, "/").await.status(), StatusCode::OK);
    // A session from OpenID Connect doesn't add ways to sign in.
    let again = later
        .get(
            &site.router,
            &format!("/setup/oidc/idp?csrf={}", later.csrf()),
        )
        .await;
    assert_eq!(again.status(), StatusCode::FORBIDDEN);
    assert!(body_text(again).await.contains("Open a link first"));
}

#[tokio::test]
async fn an_account_nobody_connected_is_refused_whatever_its_claims_say() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    let mut browser = Browser::default();
    // The ID token carries an email address, and a name that is a user's username: neither is
    // ever looked at, and a new account isn't made.
    for subject in ["stranger", "MIA-SUB", "mia", "root"] {
        let response = site.sign_in(&mut browser, subject).await;
        assert_refused(response, &browser, subject).await;
    }
    assert_eq!(site.connection("stranger").await, None);
    open_users(&site.env).await;
    assert!(
        site.env
            .channel
            .users()
            .get()
            .by_username("stranger")
            .is_none()
    );
    // The one who is connected gets in.
    let response = site.sign_in(&mut browser, "mia-sub").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(browser.session().is_some());
}

#[tokio::test]
async fn a_user_who_cannot_sign_in_is_refused_like_an_account_nobody_connected() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    let users = open_users(&site.env).await;
    users
        .set_state(&id("mia"), UserState::Disabled, Timestamp::now())
        .await
        .unwrap();
    let mut browser = Browser::default();
    let disabled = site.sign_in(&mut browser, "mia-sub").await;
    let unknown = site.sign_in(&mut browser, "nobody").await;
    assert_eq!(disabled.status(), unknown.status());
    let (disabled, unknown) = (body_text(disabled).await, body_text(unknown).await);
    assert_eq!(
        disabled, unknown,
        "one answer says nothing about who is connected"
    );
    assert!(browser.session().is_none());
    users
        .set_state(&id("mia"), UserState::Active, Timestamp::now())
        .await
        .unwrap();
    let response = site.sign_in(&mut browser, "mia-sub").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn signing_in_replaces_the_browsers_old_session() {
    let site = site().await;
    site.connected("root", "root-sub").await;
    let mut browser = site.invited("mia").await;
    let old = browser.session().unwrap().to_owned();
    // The invite took the browser's nonce; the login page gives it another, which is this one.
    browser
        .cookies
        .insert(NONCE_COOKIE.to_owned(), NONCE.to_owned());
    let response = site.sign_in(&mut browser, "root-sub").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let new = browser.session().unwrap().to_owned();
    assert_ne!(old, new);
    let store = &site.env.store;
    assert!(
        store
            .web_session(&old, Timestamp::now())
            .await
            .unwrap()
            .is_none()
    );
    let session = store
        .web_session(&new, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(session.user, id("root"));
}

/// Each way an ID token can fail to check out, and one that is fine.
#[tokio::test]
async fn an_id_token_that_does_not_check_out_signs_nobody_in() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    site.idp.add_key("rogue", false);
    type Change = fn(&mut Tamper);
    let bad: [(&str, Change); 16] = [
        ("a bad signature", |t| t.corrupt_signature = true),
        ("a key the provider doesn't publish", |t| {
            t.signer = Some("rogue".into());
        }),
        ("another key's signature under a published kid", |t| {
            t.signer = Some("rogue".into());
            t.kid = Some("k1".into());
        }),
        ("an unknown kid", |t| t.kid = Some("nope".into())),
        ("another issuer", |t| {
            t.issuer = Some("https://evil.example.com".into());
        }),
        ("another audience", |t| {
            t.audience = Some(serde_json::json!("someone-else"));
        }),
        ("several audiences", |t| {
            t.audience = Some(serde_json::json!([CLIENT_ID, "someone-else"]));
        }),
        ("another authorized party", |t| {
            t.azp = Some("someone-else".into())
        }),
        ("an expired token", |t| t.expires_in = Some(-120)),
        ("a token issued in the future", |t| t.issued_in = Some(600)),
        ("no nonce", |t| t.nonce = Some(None)),
        ("another nonce", |t| {
            t.nonce = Some(Some("not-the-nonce".into()));
        }),
        ("no signature (alg none)", |t| t.alg = Some("none".into())),
        ("an HMAC with the client secret", |t| {
            t.alg = Some("HS256".into())
        }),
        ("no ID token", |t| t.no_id_token = true),
        ("an empty subject", |t| t.subject = Some(String::new())),
    ];
    for (what, change) in bad {
        site.idp.behave();
        site.idp.tamper(change);
        let mut browser = Browser::default();
        let response = site.sign_in(&mut browser, "mia-sub").await;
        assert_refused(response, &browser, what).await;
    }
    // The same flow with a good token works, so each of those was refused for what it was.
    // Small differences are fine: a clock a little off, one audience in a list, our own `azp`.
    let fine: [(&str, Change); 4] = [
        ("a good token", |_| {}),
        ("a token that expired a moment ago", |t| {
            t.expires_in = Some(-30)
        }),
        ("one audience in a list", |t| {
            t.audience = Some(serde_json::json!([CLIENT_ID]));
        }),
        ("our own authorized party", |t| {
            t.azp = Some(CLIENT_ID.into())
        }),
    ];
    for (what, change) in fine {
        site.idp.behave();
        site.idp.tamper(change);
        let mut browser = Browser::default();
        let response = site.sign_in(&mut browser, "mia-sub").await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{what}");
        assert!(browser.session().is_some(), "{what}");
    }
}

#[tokio::test]
async fn a_callback_that_is_not_the_sign_in_in_flight_is_refused() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    let mut victim = Browser::default();
    let response = victim.get(&site.router, &login_path("idp")).await;
    let back = path_of(&site.idp.approve(&location(&response), "mia-sub").unwrap());

    // Another browser opening the callback (a login CSRF) has no binding cookie: refused, and
    // the sign-in is still there for its own browser.
    let mut attacker = Browser::default();
    let refused = attacker.get(&site.router, &back).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert!(attacker.session().is_none());
    let done = victim.get(&site.router, &back).await;
    assert_eq!(done.status(), StatusCode::SEE_OTHER);
    assert!(victim.session().is_some());

    // The callback of a sign-in that finished is no use again, even in the right browser.
    let mut replay = Browser {
        cookies: victim
            .cookies
            .iter()
            .filter(|(name, _)| name.as_str() != SESSION_COOKIE)
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        ..Browser::default()
    };
    replay.cookies.insert(
        "doozers_oidc".into(),
        set_cookies(&response)
            .iter()
            .find_map(|cookie| cookie.strip_prefix("doozers_oidc="))
            .map(|rest| rest.split(';').next().unwrap().to_owned())
            .unwrap(),
    );
    let replayed = replay.get(&site.router, &back).await;
    assert_eq!(replayed.status(), StatusCode::BAD_REQUEST);
    assert!(replay.session().is_none());
    assert!(body_text(replayed).await.contains("Sign-in expired"));
}

#[tokio::test]
async fn a_callback_with_another_browsers_binding_spends_the_sign_in() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    let mut browser = Browser::default();
    let start = browser.get(&site.router, &login_path("idp")).await;
    let back = path_of(&site.idp.approve(&location(&start), "mia-sub").unwrap());
    let mut other = Browser::default();
    other.cookies.insert("doozers_oidc".into(), "0".repeat(32));
    assert_eq!(
        other.get(&site.router, &back).await.status(),
        StatusCode::BAD_REQUEST
    );
    // A wrong guess costs the sign-in it guessed.
    assert_eq!(
        browser.get(&site.router, &back).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert!(browser.session().is_none());
}

#[tokio::test]
async fn a_state_nobody_made_or_made_for_another_provider_is_refused() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    site.env
        .store
        .oidc_provider_add("other", settings(&site.idp.issuer), Timestamp::now())
        .await
        .unwrap();
    let mut browser = Browser::default();
    let start = browser.get(&site.router, &login_path("idp")).await;
    let back = site.idp.approve(&location(&start), "mia-sub").unwrap();
    let query = back.split_once('?').unwrap().1;

    let forged = format!("/auth/oidc/idp/callback?code=x&state={}", "a".repeat(32));
    assert_eq!(
        browser.get(&site.router, &forged).await.status(),
        StatusCode::BAD_REQUEST
    );
    let no_state = browser
        .get(&site.router, "/auth/oidc/idp/callback?code=x")
        .await;
    assert_eq!(no_state.status(), StatusCode::BAD_REQUEST);

    // The state of one provider's sign-in, at another provider's callback.
    let elsewhere = browser
        .get(&site.router, &format!("/auth/oidc/other/callback?{query}"))
        .await;
    assert_eq!(elsewhere.status(), StatusCode::BAD_REQUEST);
    assert!(browser.session().is_none());
    // It was spent by the attempt.
    let at_home = browser
        .get(&site.router, &format!("/auth/oidc/idp/callback?{query}"))
        .await;
    assert_eq!(at_home.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_provider_that_refuses_or_names_another_issuer_signs_nobody_in() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    let mut browser = Browser::default();

    // The user said no at the provider.
    let start = browser.get(&site.router, &login_path("idp")).await;
    let state = Url::parse(&location(&start))
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    let denied = browser
        .get(
            &site.router,
            &format!("/auth/oidc/idp/callback?error=access_denied&state={state}"),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
    assert!(
        body_text(denied)
            .await
            .contains("Idp didn&#39;t sign you in")
    );
    // That used the sign-in up.
    let back = site.idp.approve(&location(&start), "mia-sub").unwrap();
    assert_eq!(
        browser.get(&site.router, &path_of(&back)).await.status(),
        StatusCode::BAD_REQUEST
    );

    // A callback that says another issuer answered (RFC 9207).
    let start = browser.get(&site.router, &login_path("idp")).await;
    let back = site.idp.approve(&location(&start), "mia-sub").unwrap();
    let mix_up = format!("{}&iss=https%3A%2F%2Fevil.example.com", path_of(&back));
    let response = browser.get(&site.router, &mix_up).await;
    assert_refused(response, &browser, "another issuer in the callback").await;
}

#[tokio::test]
async fn only_a_login_that_came_from_a_link_connects_an_account() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    // A login that came from the provider itself.
    let mut browser = Browser::default();
    site.sign_in(&mut browser, "mia-sub").await;
    let path = format!("/setup/oidc/idp?csrf={}", browser.csrf());
    let response = browser.get(&site.router, &path).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(body_text(response).await.contains("Open a link first"));
    let setup = body_text(browser.get(&site.router, "/setup").await).await;
    assert!(!setup.contains("/setup/oidc/"), "{setup}");

    // Nobody logged in.
    let mut nobody = Browser::default();
    let response = nobody.get(&site.router, "/setup/oidc/idp?csrf=x").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // A link's login, but not the page's CSRF token.
    let mut invited = site.invited("newbie").await;
    for path in ["/setup/oidc/idp", "/setup/oidc/idp?csrf=nope"] {
        let response = invited.get(&site.router, path).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(site.connection("newbie-sub").await, None);
}

#[tokio::test]
async fn a_connection_finishes_only_in_the_login_that_began_it() {
    let site = site().await;
    let mut newbie = site.invited("newbie").await;
    let path = format!("/setup/oidc/idp?csrf={}", newbie.csrf());
    let start = newbie.get(&site.router, &path).await;
    let back = path_of(&site.idp.approve(&location(&start), "stolen").unwrap());

    // Someone else's login, with newbie's binding cookie (the worst case for the binding), can't
    // finish it, and it is gone.
    let mut mia = site.invited("mia").await;
    mia.cookies.insert(
        "doozers_oidc".into(),
        newbie.cookies["doozers_oidc"].clone(),
    );
    let response = mia.get(&site.router, &back).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(site.connection("stolen").await, None);
    let response = newbie.get(&site.router, &back).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(site.connection("stolen").await, None);

    // And no login at all: the callback of a connection needs the browser's session.
    let start = newbie.get(&site.router, &path).await;
    let back = path_of(&site.idp.approve(&location(&start), "stolen").unwrap());
    let mut anonymous = Browser::default();
    anonymous.cookies.insert(
        "doozers_oidc".into(),
        newbie.cookies["doozers_oidc"].clone(),
    );
    let response = anonymous.get(&site.router, &back).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(site.connection("stolen").await, None);
}

#[tokio::test]
async fn an_account_is_one_users_and_connecting_another_replaces_the_users_account() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;

    // Another user's account can't be connected.
    let mut newbie = site.invited("newbie").await;
    let response = site.connect(&mut newbie, "mia-sub").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        body_text(response)
            .await
            .contains("connected to another user")
    );
    assert_eq!(site.connection("mia-sub").await, Some(id("mia")));

    // Connecting another account replaces the one the user had.
    let response = site.connect(&mut newbie, "first").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let response = site.connect(&mut newbie, "second").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(site.connection("first").await, None);
    assert_eq!(site.connection("second").await, Some(id("newbie")));
    let mut later = Browser::default();
    let response = site.sign_in(&mut later, "first").await;
    assert_refused(response, &later, "the replaced account").await;
    let response = site.sign_in(&mut later, "second").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_disabled_or_missing_provider_is_not_offered_or_used() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    let mut browser = Browser::default();
    let start = browser.get(&site.router, &login_path("idp")).await;
    let back = path_of(&site.idp.approve(&location(&start), "mia-sub").unwrap());

    let store = &site.env.store;
    assert!(
        store
            .oidc_provider_set_enabled("idp", false, Timestamp::now())
            .await
            .unwrap()
    );
    let login = body_text(browser.get(&site.router, "/login").await).await;
    assert!(!login.contains("/login/oidc/"), "{login}");
    let mut invited = site.invited("newbie").await;
    let setup = body_text(invited.get(&site.router, "/setup").await).await;
    assert!(!setup.contains("/setup/oidc/"), "{setup}");
    for path in [
        login_path("idp"),
        login_path("nope"),
        login_path("Not%20An%20Id"),
        format!("/setup/oidc/idp?csrf={}", invited.csrf()),
    ] {
        let response = if path.starts_with("/setup") {
            invited.get(&site.router, &path).await
        } else {
            browser.get(&site.router, &path).await
        };
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    // A sign-in begun before it was disabled doesn't finish.
    let response = browser.get(&site.router, &back).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(browser.session().is_none());
    assert_eq!(site.idp.requests().token, 0);

    // Enabled again, it works.
    store
        .oidc_provider_set_enabled("idp", true, Timestamp::now())
        .await
        .unwrap();
    let response = site.sign_in(&mut browser, "mia-sub").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_client_secret_authenticates_the_client_at_the_token_endpoint_only() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    let mut browser = Browser::default();
    let response = site.sign_in(&mut browser, "mia-sub").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    // HTTP Basic is what the provider's document allows: the secret isn't in the form.
    let form = site.idp.last_token_form();
    assert!(!form.contains_key("client_secret"), "{form:?}");
    assert_eq!(form["code_verifier"].len(), 43);

    // A provider that takes the credentials in the form gets them there.
    site.idp.form_credentials(true);
    let mut browser = Browser::default();
    let response = site
        .through_idp_fresh(&mut browser, &login_path("idp"), "mia-sub")
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(site.idp.last_token_form()["client_secret"], CLIENT_SECRET);

    // A secret that is gone from the store, or is wrong, signs nobody in.
    let path = SecretPath::new(SECRET_PATH).unwrap();
    site.idp.form_credentials(false);
    site.secrets
        .put(&path, &Secret::new("wrong"), None)
        .await
        .unwrap();
    let mut browser = Browser::default();
    let response = site.sign_in(&mut browser, "mia-sub").await;
    assert_refused(response, &browser, "a wrong client secret").await;
    assert!(site.secrets.delete(&path).await.unwrap());
    let response = site.sign_in(&mut browser, "mia-sub").await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(browser.session().is_none());
}

impl Site {
    /// [`through_idp`](Self::through_idp), after the provider's document changed: the one Doozers
    /// has is dropped first by the start of a new site state, so use a fresh router.
    async fn through_idp_fresh(
        &self,
        browser: &mut Browser,
        path: &str,
        subject: &str,
    ) -> Response {
        let router = self
            .env
            .app
            .install_oidc(
                self.env.app.install(RouterBuilder::new()),
                self.secrets.clone(),
            )
            .build();
        let start = browser.get(&router, path).await;
        assert_eq!(start.status(), StatusCode::SEE_OTHER, "{path}");
        let back = self.idp.approve(&location(&start), subject).unwrap();
        browser.get(&router, &path_of(&back)).await
    }
}

#[tokio::test]
async fn a_provider_with_no_client_secret_signs_in_with_pkce_alone() {
    let site = site().await;
    // A public client: the provider knows no secret for it, and Doozers has none to send.
    let idp = FakeIdp::start("public-client", None).await;
    let mut public = settings(&idp.issuer);
    public.client_id = "public-client".into();
    public.client_secret = None;
    site.env
        .store
        .oidc_provider_add("public", public, Timestamp::now())
        .await
        .unwrap();
    site.env
        .store
        .user_oidc_connect("public", &idp.issuer, "p-sub", &id("mia"), Timestamp::now())
        .await
        .unwrap();
    let mut browser = Browser::default();
    let start = browser.get(&site.router, &login_path("public")).await;
    let back = idp.approve(&location(&start), "p-sub").unwrap();
    let response = browser
        .get(
            &site.router,
            &format!(
                "/auth/oidc/public/callback?{}",
                back.split_once('?').unwrap().1
            ),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(browser.session().is_some());
    let form = idp.last_token_form();
    assert_eq!(form["client_id"], "public-client");
    assert!(!form.contains_key("client_secret"));
}

#[tokio::test]
async fn an_https_public_url_is_the_redirect_uri_and_makes_the_cookie_secure() {
    let config = WebConfig::from_json(&serde_json::json!({
        "public_url": "https://doozers.example.com"
    }))
    .unwrap();
    let site = site_with(env_with(config, &["web:owner"]).await).await;
    let mut browser = Browser::default();
    let start = browser.get(&site.router, &login_path("idp")).await;
    let url = Url::parse(&location(&start)).unwrap();
    let uri = url
        .query_pairs()
        .find(|(name, _)| name == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    assert_eq!(uri, "https://doozers.example.com/auth/oidc/idp/callback");
    let binding = set_cookies(&start)
        .into_iter()
        .find(|cookie| cookie.starts_with("doozers_oidc="))
        .unwrap();
    assert!(
        binding.ends_with("; HttpOnly; SameSite=Lax; Secure"),
        "{binding}"
    );
}

/// Like [`call`], from the client `peer`, as the server tells the handlers, with the sign-in
/// nonce cookie if `nonce` says so.
async fn call_from(app: axum::Router, peer: &str, path: &str, nonce: bool) -> Response {
    let mut request = Request::builder().method(Method::GET).uri(path);
    if nonce {
        request = request.header("cookie", format!("{NONCE_COOKIE}={NONCE}"));
    }
    let mut request = request.body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(PeerAddr(Some(peer.parse().unwrap()))));
    app.oneshot(request).await.unwrap()
}

#[tokio::test]
async fn a_client_address_can_only_start_so_many_sign_ins() {
    let site = site().await;
    let path = login_path("idp");
    for _ in 0..oidc::START_ATTEMPTS {
        let response = call_from(site.router.clone(), "192.0.2.1", &path, true).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }
    let response = call_from(site.router.clone(), "192.0.2.1", &path, true).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(RETRY_AFTER));
    // Another address is another count.
    let response = call_from(site.router.clone(), "192.0.2.2", &path, true).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_start_needs_the_login_pages_nonce_and_costs_nothing_without_it() {
    let site = site().await;
    let peer = "192.0.2.1";
    // Another site can send a visitor here, but not with the nonce cookie, which is `Strict`, and
    // not with the nonce in the link either, which only the login page has.
    let wrong = format!("/login/oidc/idp?nonce={}", "f".repeat(32));
    for (what, path, nonce) in [
        ("no nonce in the link", "/login/oidc/idp".to_owned(), true),
        ("another nonce in the link", wrong, true),
        ("no nonce cookie", login_path("idp"), false),
    ] {
        for _ in 0..oidc::START_ATTEMPTS + 5 {
            let response = call_from(site.router.clone(), peer, &path, nonce).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{what}");
            assert!(!response.headers().contains_key(LOCATION), "{what}");
            assert!(
                set_cookies(&response)
                    .iter()
                    .all(|cookie| !cookie.starts_with("doozers_oidc=")),
                "{what}"
            );
        }
    }
    // None of that was counted, or kept: the visitor's own start still works.
    let response = call_from(site.router.clone(), peer, &login_path("idp"), true).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        site.idp.requests().discovery,
        1,
        "only the real start went on"
    );
}

#[tokio::test]
async fn a_second_tab_or_a_restart_keeps_the_first_sign_in_working() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    let mut browser = Browser::default();
    let first = browser.get(&site.router, &login_path("idp")).await;
    let binding = |response: &Response| {
        set_cookies(response)
            .into_iter()
            .find_map(|cookie| cookie.strip_prefix("doozers_oidc="))
            .map(|rest| rest.split(';').next().unwrap().to_owned())
            .unwrap()
    };
    // The start sees the cookie the first one set, so the second start keeps the binding.
    let second = browser.get(&site.router, &login_path("idp")).await;
    assert_eq!(binding(&first), binding(&second));
    let first = path_of(&site.idp.approve(&location(&first), "mia-sub").unwrap());
    let second = path_of(&site.idp.approve(&location(&second), "mia-sub").unwrap());
    // Both finish, in either order.
    assert_eq!(
        browser.get(&site.router, &second).await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        browser.get(&site.router, &first).await.status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn a_provider_that_cannot_be_reached_says_so_without_a_sign_in() {
    let site = site().await;
    let mut dead = settings("http://127.0.0.1:9");
    dead.display_name = "Dead".into();
    site.env
        .store
        .oidc_provider_add("dead", dead, Timestamp::now())
        .await
        .unwrap();
    let mut browser = Browser::default();
    let response = browser.get(&site.router, &login_path("dead")).await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!response.headers().contains_key(LOCATION));
    assert!(!browser.cookies.contains_key("doozers_oidc"));
    assert!(
        body_text(response)
            .await
            .contains("Couldn&#39;t reach the provider")
    );
}

// The protocol, with the time the test chooses.

fn provider(site: &Site) -> doozers_store::OidcProvider {
    let now = Timestamp::now();
    doozers_store::OidcProvider {
        id: "idp".into(),
        display_name: "Idp".into(),
        issuer: site.idp.issuer.clone(),
        client_id: CLIENT_ID.into(),
        client_secret: Some(SECRET_PATH.into()),
        scopes: vec!["openid".into()],
        enabled: true,
        created_at: now,
        updated_at: now,
    }
}

fn after(base: Timestamp, seconds: i64) -> Timestamp {
    Timestamp::from_second(base.as_second() + seconds).unwrap()
}

/// A whole sign-in through `oidc` at `now`.
async fn login(
    oidc: &Oidc,
    site: &Site,
    provider: &doozers_store::OidcProvider,
    subject: &str,
    now: Timestamp,
) -> Result<oidc::Identity, oidc::Error> {
    let redirect = format!("{BASE}/auth/oidc/idp/callback");
    let url = oidc
        .begin(provider, &redirect, "binding", Purpose::SignIn, now)
        .await?;
    let back = site.idp.approve(url.as_str(), subject).unwrap();
    let query: BTreeMap<_, _> = Url::parse(&back)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    let flow = oidc.take_flow(&query["state"], "binding", now).unwrap();
    oidc.complete(
        provider,
        Some(&Secret::new(CLIENT_SECRET)),
        &redirect,
        flow,
        oidc::Returned {
            code: &query["code"],
            issuer: None,
        },
        now,
    )
    .await
}

#[tokio::test]
async fn the_identity_is_the_issuer_and_subject_the_provider_vouches_for() {
    let site = site().await;
    let oidc = Oidc::default();
    let now = Timestamp::now();
    let identity = login(&oidc, &site, &provider(&site), "the-subject", now)
        .await
        .unwrap();
    assert_eq!(
        identity,
        oidc::Identity {
            issuer: site.idp.issuer.clone(),
            subject: "the-subject".into()
        }
    );
}

#[tokio::test]
async fn a_sign_in_is_forgotten_after_its_time_and_cannot_be_taken_twice() {
    let oidc = Oidc::default();
    let site = site().await;
    let provider = provider(&site);
    let now = Timestamp::now();
    let mut states = Vec::new();
    for _ in 0..2 {
        let url = oidc
            .begin(
                &provider,
                &format!("{BASE}/auth/oidc/idp/callback"),
                "b",
                Purpose::SignIn,
                now,
            )
            .await
            .unwrap();
        let state = url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .into_owned();
        states.push(state);
    }
    assert!(oidc.take_flow(&states[0], "b", after(now, 1)).is_some());
    assert!(oidc.take_flow(&states[0], "b", after(now, 2)).is_none());
    let late = after(now, oidc::FLOW_TTL.as_secs() as i64 + 1);
    assert!(oidc.take_flow(&states[1], "b", late).is_none());
}

#[tokio::test]
async fn a_rotated_key_is_found_by_refreshing_the_keys_but_not_more_than_once_a_minute() {
    let site = site().await;
    let oidc = Oidc::default();
    let provider = provider(&site);
    let t0 = Timestamp::now();
    login(&oidc, &site, &provider, "s", t0).await.unwrap();
    assert_eq!(
        site.idp.requests().jwks,
        1,
        "fetched with the discovery document"
    );

    // The provider rotates: a new key signs, and the old one stays published.
    site.idp.add_key("k2", true);
    site.idp.tamper(|t| t.signer = Some("k2".into()));
    // Too soon after the keys were fetched: no refresh, so the token isn't checked.
    let err = login(&oidc, &site, &provider, "s", after(t0, 30))
        .await
        .unwrap_err();
    assert!(matches!(err, oidc::Error::Refused(_)), "{err}");
    assert_eq!(site.idp.requests().jwks, 1);
    // A minute on, an unknown kid fetches the keys again, and the token is accepted.
    login(&oidc, &site, &provider, "s", after(t0, 90))
        .await
        .unwrap();
    assert_eq!(site.idp.requests().jwks, 2);
    // From then on k2 is known, so signing in costs no request.
    login(&oidc, &site, &provider, "s", after(t0, 95))
        .await
        .unwrap();
    assert_eq!(site.idp.requests().jwks, 2);

    // Tokens with a kid nobody has refresh the keys at most once a minute.
    site.idp.tamper(|t| t.kid = Some("forged".into()));
    for seconds in [100, 120, 149] {
        login(&oidc, &site, &provider, "s", after(t0, seconds))
            .await
            .unwrap_err();
    }
    assert_eq!(site.idp.requests().jwks, 2, "refreshed 5 seconds ago");
    login(&oidc, &site, &provider, "s", after(t0, 160))
        .await
        .unwrap_err();
    assert_eq!(site.idp.requests().jwks, 3);
    login(&oidc, &site, &provider, "s", after(t0, 161))
        .await
        .unwrap_err();
    assert_eq!(site.idp.requests().jwks, 3);
}

#[tokio::test]
async fn tokens_with_the_same_new_kid_all_find_the_key_one_of_them_fetched() {
    let site = site().await;
    let oidc = Oidc::default();
    let provider = provider(&site);
    let t0 = Timestamp::now();
    login(&oidc, &site, &provider, "s", t0).await.unwrap();
    assert_eq!(site.idp.requests().jwks, 1);

    // The provider rotates, and several people sign in at once with tokens of the new key. The
    // first to look refreshes the keys; the others find them already refreshed, instead of being
    // refused because the keys were fetched a moment ago.
    site.idp.add_key("k2", true);
    site.idp.tamper(|t| t.signer = Some("k2".into()));
    let later = after(t0, 90);
    let (a, b, c) = tokio::join!(
        login(&oidc, &site, &provider, "a", later),
        login(&oidc, &site, &provider, "b", later),
        login(&oidc, &site, &provider, "c", later),
    );
    for (who, result) in [("a", a), ("b", b), ("c", c)] {
        assert!(result.is_ok(), "{who}: {:?}", result.err());
    }
    assert_eq!(site.idp.requests().jwks, 2, "one fetch for all of them");
}

#[tokio::test]
async fn a_token_that_expired_is_judged_by_the_time_of_the_sign_in() {
    let site = site().await;
    let oidc = Oidc::default();
    let provider = provider(&site);
    let t0 = Timestamp::now();
    // The token is good for 300 seconds, and the clock may be a minute off.
    login(&oidc, &site, &provider, "s", after(t0, 330))
        .await
        .unwrap();
    let err = login(&oidc, &site, &provider, "s", after(t0, 400))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("xpired"), "{err}");
}

#[tokio::test]
async fn the_discovery_document_is_kept_for_an_hour_unless_a_fresh_one_is_fetched() {
    let site = site().await;
    let oidc = Oidc::default();
    let t0 = Timestamp::now();
    oidc.discover(&site.idp.issuer, t0).await.unwrap();
    oidc.discover(&site.idp.issuer, after(t0, 3000))
        .await
        .unwrap();
    assert_eq!(site.idp.requests().discovery, 1);
    // A fetch asks again whatever is kept, and what it got is kept from then on.
    oidc.fetch(&site.idp.issuer, after(t0, 3000)).await.unwrap();
    assert_eq!(site.idp.requests().discovery, 2);
    oidc.discover(&site.idp.issuer, after(t0, 6500))
        .await
        .unwrap();
    assert_eq!(site.idp.requests().discovery, 2);
    oidc.discover(&site.idp.issuer, after(t0, 7000))
        .await
        .unwrap();
    assert_eq!(site.idp.requests().discovery, 3);
}

#[tokio::test]
async fn a_provider_whose_document_cannot_be_used_is_refused_with_the_reason() {
    let site = site().await;
    let now = Timestamp::now();
    let expect = |change: fn(&mut serde_json::Value), reason: &'static str| {
        let site = &site;
        async move {
            site.idp.edit_discovery(change);
            let err = Oidc::default()
                .discover(&site.idp.issuer, now)
                .await
                .err()
                .unwrap_or_else(|| panic!("{reason}: accepted"));
            assert!(matches!(err, oidc::Error::Provider(_)), "{reason}: {err}");
            assert!(err.to_string().contains(reason), "{reason}: {err}");
            site.idp.edit_discovery(|doc| {
                *doc = serde_json::json!({
                    "issuer": site.idp.issuer,
                    "authorization_endpoint": format!("{}/authorize", site.idp.issuer),
                    "token_endpoint": format!("{}/token", site.idp.issuer),
                    "jwks_uri": format!("{}/jwks", site.idp.issuer),
                    "response_types_supported": ["code"],
                    "subject_types_supported": ["public"],
                    "id_token_signing_alg_values_supported": ["ES256"],
                });
            });
        }
    };
    expect(
        |doc| doc["issuer"] = "https://another.example.com".into(),
        "unexpected issuer",
    )
    .await;
    expect(
        |doc| doc["response_types_supported"] = serde_json::json!(["id_token"]),
        "authorization code flow",
    )
    .await;
    expect(
        |doc| doc["code_challenge_methods_supported"] = serde_json::json!(["plain"]),
        "PKCE with S256",
    )
    .await;
    expect(
        |doc| doc["id_token_signing_alg_values_supported"] = serde_json::json!(["HS256", "none"]),
        "no algorithm Doozers accepts",
    )
    .await;
    expect(
        |doc| {
            doc.as_object_mut().unwrap().remove("token_endpoint");
        },
        "token endpoint",
    )
    .await;
    // The default document, with no PKCE method listed, is fine: it's a provider that doesn't say.
    Oidc::default()
        .discover(&site.idp.issuer, now)
        .await
        .unwrap();
}

#[tokio::test]
async fn an_issuer_that_is_not_https_or_not_a_url_is_refused_before_any_request() {
    let site = site().await;
    let oidc = Oidc::default();
    let now = Timestamp::now();
    for issuer in [
        "http://idp.example.com",
        "ftp://idp.example.com",
        "not a url",
        "https://idp.example.com/?a=b",
        "https://idp.example.com/#frag",
        "",
    ] {
        let err = oidc.discover(issuer, now).await.err().expect(issuer);
        assert!(matches!(err, oidc::Error::Provider(_)), "{issuer}: {err}");
    }
    assert_eq!(site.idp.requests().discovery, 0);
}

// The admin pages.

/// The form of a provider, as the browser posts it.
fn provider_form(csrf: &str, issuer: &str, extra: &[(&str, &str)]) -> String {
    let mut pairs = vec![
        ("csrf", csrf),
        ("id", "idp"),
        ("display_name", "Idp"),
        ("issuer", issuer),
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("scopes", "profile"),
        ("enabled", "on"),
    ];
    for (name, value) in extra {
        pairs.retain(|(have, _)| have != name);
        pairs.push((name, value));
    }
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

impl Site {
    /// A browser logged in as `user` with a recovery link.
    async fn recovered(&self, user: &str) -> Browser {
        let token = invite_token(&self.env, &id(user), InviteKind::Recovery).await;
        let response = redeem(self.router.clone(), &token, &[]).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{user}");
        let mut browser = Browser::default();
        browser.absorb(&response);
        browser
    }

    async fn post(&self, browser: &mut Browser, path: &str, form: &str) -> Response {
        browser
            .send(&self.router, Method::POST, path, Some(form))
            .await
    }

    async fn stored_secret(&self, name: &str) -> Option<String> {
        self.secrets
            .get(&SecretPath::new(name).unwrap())
            .await
            .unwrap()
            .map(|secret| secret.expose().to_owned())
    }
}

/// A site with no provider yet.
async fn bare_site() -> Site {
    let site = site().await;
    assert!(site.env.store.oidc_provider_remove("idp").await.unwrap());
    site.secrets
        .delete(&SecretPath::new(SECRET_PATH).unwrap())
        .await
        .unwrap();
    site
}

#[tokio::test]
async fn an_admin_adds_a_provider_and_an_invited_user_signs_up_with_it_and_signs_in_later() {
    let site = bare_site().await;
    let mut admin = site.recovered("root").await;

    // The form says what to register at the provider.
    let page = body_text(
        admin
            .get(&site.router, "/admin/identity-providers/new")
            .await,
    )
    .await;
    assert!(page.contains("<h1>New identity provider</h1>"), "{page}");
    assert!(page.contains("/auth/oidc/&#60;id&#62;/callback"), "{page}");
    let page = body_text(admin.get(&site.router, "/admin/identity-providers").await).await;
    assert!(page.contains("No identity providers yet"), "{page}");

    // Adding one reads the provider's document and keeps the secret in the secret store.
    let form = provider_form(&admin.csrf(), &site.idp.issuer, &[]);
    let response = site
        .post(&mut admin, "/admin/identity-providers", &form)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        location(&response),
        "/admin/identity-providers?added=Idp#provider-idp"
    );
    assert_eq!(site.idp.requests().discovery, 1);
    let stored = site.env.store.oidc_provider("idp").await.unwrap().unwrap();
    assert_eq!(stored.issuer, site.idp.issuer);
    assert_eq!(stored.client_id, CLIENT_ID);
    assert_eq!(stored.scopes, ["openid", "profile"]);
    assert!(stored.enabled);
    assert_eq!(stored.client_secret.as_deref(), Some(SECRET_PATH));
    assert_eq!(
        site.stored_secret(SECRET_PATH).await.as_deref(),
        Some(CLIENT_SECRET)
    );

    // The list shows it with the redirect URI to register, and never the secret.
    let page = body_text(
        admin
            .get(&site.router, "/admin/identity-providers?added=Idp")
            .await,
    )
    .await;
    assert!(page.contains("Added Idp."), "{page}");
    assert!(page.contains(&site.idp.issuer), "{page}");
    assert!(
        page.contains(&format!("{BASE}/auth/oidc/idp/callback")),
        "{page}"
    );
    assert!(page.contains("secret stored"), "{page}");
    assert!(page.contains("enabled"), "{page}");
    assert!(!page.contains(CLIENT_SECRET), "{page}");
    let edit = body_text(
        admin
            .get(&site.router, "/admin/identity-providers/idp/edit")
            .await,
    )
    .await;
    assert!(!edit.contains(CLIENT_SECRET), "{edit}");
    assert!(edit.contains("stored; leave blank to keep it"), "{edit}");

    // An invited user connects an account at it, and signs in with it later.
    let mut newbie = site.invited("newbie").await;
    let response = site.connect(&mut newbie, "newbie-sub").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/setup");
    let mut later = Browser::default();
    let response = site.sign_in(&mut later, "newbie-sub").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let session = site
        .env
        .store
        .web_session(later.session().unwrap(), Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (session.user, session.method),
        (id("newbie"), SignInMethod::Oidc)
    );

    // An account nobody connected is refused.
    let mut stranger = Browser::default();
    let response = site.sign_in(&mut stranger, "stranger").await;
    assert_refused(response, &stranger, "an unconnected account").await;

    // The list counts the connected account.
    let page = body_text(admin.get(&site.router, "/admin/identity-providers").await).await;
    assert!(page.contains("1 account"), "{page}");
}

#[tokio::test]
async fn only_an_admin_gets_the_provider_pages_and_every_change_needs_the_csrf_token() {
    let site = site().await;
    let mut member = site.recovered("mia").await;
    let csrf = member.csrf();
    for path in [
        "/admin/identity-providers",
        "/admin/identity-providers/new",
        "/admin/identity-providers/idp/edit",
        "/admin/identity-providers/idp/remove",
    ] {
        let response = member.get(&site.router, path).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let form = provider_form(&csrf, &site.idp.issuer, &[("id", "second")]);
    for (path, body) in [
        ("/admin/identity-providers", form.clone()),
        ("/admin/identity-providers/idp", form.clone()),
        (
            "/admin/identity-providers/idp/enabled",
            format!("csrf={csrf}&enabled=false"),
        ),
        (
            "/admin/identity-providers/idp/remove",
            format!("csrf={csrf}"),
        ),
    ] {
        let response = site.post(&mut member, path, &body).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let mut nobody = Browser::default();
    let response = nobody.get(&site.router, "/admin/identity-providers").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let nav = body_text(site.recovered("mia").await.get(&site.router, "/").await).await;
    assert!(!nav.contains("Identity providers"), "{nav}");

    // An admin's post without the token, or with another's, changes nothing.
    let mut admin = site.recovered("root").await;
    let nav = body_text(admin.get(&site.router, "/").await).await;
    assert!(
        nav.contains(r#"<a href="/admin/identity-providers">Identity providers</a>"#),
        "{nav}"
    );
    let stolen = member.csrf();
    for (path, body) in [
        (
            "/admin/identity-providers",
            provider_form("", &site.idp.issuer, &[("id", "second")]),
        ),
        (
            "/admin/identity-providers/idp",
            provider_form(&stolen, &site.idp.issuer, &[("display_name", "Hacked")]),
        ),
        (
            "/admin/identity-providers/idp/enabled",
            "enabled=false".to_owned(),
        ),
        ("/admin/identity-providers/idp/remove", String::new()),
    ] {
        let response = site.post(&mut admin, path, &body).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    let stored = site.env.store.oidc_providers().await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].display_name, "Idp");
    assert!(stored[0].enabled);
}

#[tokio::test]
async fn a_provider_that_cannot_be_used_is_refused_with_the_reason_and_nothing_is_stored() {
    let site = bare_site().await;
    let mut admin = site.recovered("root").await;
    let csrf = admin.csrf();
    /// The form fields to change, the status, and what the page says.
    type Attempt<'a> = (&'a [(&'a str, &'a str)], StatusCode, &'a str);
    let attempts: [Attempt; 7] = [
        (
            &[("id", "Not An Id")],
            StatusCode::BAD_REQUEST,
            "lowercase letters",
        ),
        (&[("id", "")], StatusCode::BAD_REQUEST, "lowercase letters"),
        (&[("display_name", "  ")], StatusCode::BAD_REQUEST, "name"),
        (&[("issuer", "")], StatusCode::BAD_REQUEST, "issuer"),
        (&[("client_id", "")], StatusCode::BAD_REQUEST, "client id"),
        (
            &[("issuer", "http://idp.example.com")],
            StatusCode::UNPROCESSABLE_ENTITY,
            "must be an https URL",
        ),
        (
            &[("issuer", "http://127.0.0.1:9")],
            StatusCode::UNPROCESSABLE_ENTITY,
            "Doozers can&#39;t use this provider",
        ),
    ];
    for (extra, status, reason) in attempts {
        let body = provider_form(&csrf, &site.idp.issuer, extra);
        let response = site
            .post(&mut admin, "/admin/identity-providers", &body)
            .await;
        assert_eq!(response.status(), status, "{extra:?}");
        let page = body_text(response).await;
        assert!(page.contains(reason), "{extra:?}: {page}");
        assert!(page.contains(r#"role="alert""#), "{page}");
        // The secret doesn't come back.
        assert!(!page.contains(CLIENT_SECRET), "{page}");
    }
    // A document that names another issuer.
    site.idp
        .edit_discovery(|doc| doc["issuer"] = "https://another.example.com".into());
    let body = provider_form(&csrf, &site.idp.issuer, &[]);
    let response = site
        .post(&mut admin, "/admin/identity-providers", &body)
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body_text(response).await.contains("unexpected issuer"));
    assert!(site.env.store.oidc_providers().await.unwrap().is_empty());
    assert_eq!(site.stored_secret("oidc/idp/client-secret").await, None);
}

#[tokio::test]
async fn a_provider_id_is_taken_once_and_scopes_are_made_tidy() {
    let site = bare_site().await;
    let mut admin = site.recovered("root").await;
    let csrf = admin.csrf();
    let form = provider_form(
        &csrf,
        &site.idp.issuer,
        &[
            ("scopes", "email  profile openid email"),
            ("client_secret", ""),
        ],
    );
    let response = site
        .post(&mut admin, "/admin/identity-providers", &form)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let stored = site.env.store.oidc_provider("idp").await.unwrap().unwrap();
    assert_eq!(stored.scopes, ["openid", "email", "profile"]);
    assert_eq!(stored.client_secret, None, "no secret typed, none stored");

    let again = provider_form(&csrf, &site.idp.issuer, &[("display_name", "Other")]);
    let response = site
        .post(&mut admin, "/admin/identity-providers", &again)
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(body_text(response).await.contains("exists already"));
    let stored = site.env.store.oidc_provider("idp").await.unwrap().unwrap();
    assert_eq!(stored.display_name, "Idp");
    assert_eq!(site.stored_secret("oidc/idp/client-secret").await, None);
}

#[tokio::test]
async fn editing_keeps_the_secret_unless_a_new_one_is_typed_or_removal_is_ticked() {
    let site = site().await;
    let mut admin = site.recovered("root").await;
    let csrf = admin.csrf();
    let edit = |extra: &[(&str, &str)]| provider_form(&csrf, &site.idp.issuer, extra);

    // A blank secret keeps the one stored; the id in the form is ignored, the path's is used.
    let body = edit(&[
        ("client_secret", ""),
        ("display_name", "Renamed"),
        ("client_id", "new-client"),
        ("scopes", "email"),
        ("id", "other"),
    ]);
    let response = site
        .post(&mut admin, "/admin/identity-providers/idp", &body)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        location(&response),
        "/admin/identity-providers?saved=Renamed#provider-idp"
    );
    let stored = site.env.store.oidc_provider("idp").await.unwrap().unwrap();
    assert_eq!(stored.display_name, "Renamed");
    assert_eq!(stored.client_id, "new-client");
    assert_eq!(stored.scopes, ["openid", "email"]);
    assert_eq!(stored.client_secret.as_deref(), Some(SECRET_PATH));
    assert_eq!(
        site.stored_secret(SECRET_PATH).await.as_deref(),
        Some(CLIENT_SECRET)
    );
    assert!(
        site.env
            .store
            .oidc_provider("other")
            .await
            .unwrap()
            .is_none()
    );
    assert!(stored.updated_at >= stored.created_at);

    // A typed one replaces it.
    let body = edit(&[("client_secret", "rotated-secret")]);
    let response = site
        .post(&mut admin, "/admin/identity-providers/idp", &body)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        site.stored_secret(SECRET_PATH).await.as_deref(),
        Some("rotated-secret")
    );

    // A tick removes it, and its name from the provider.
    let body = edit(&[("client_secret", ""), ("clear_secret", "on")]);
    let response = site
        .post(&mut admin, "/admin/identity-providers/idp", &body)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let stored = site.env.store.oidc_provider("idp").await.unwrap().unwrap();
    assert_eq!(stored.client_secret, None);
    assert_eq!(site.stored_secret(SECRET_PATH).await, None);
    let page = body_text(
        admin
            .get(&site.router, "/admin/identity-providers/idp/edit")
            .await,
    )
    .await;
    assert!(page.contains(r#"placeholder="none""#), "{page}");
    assert!(!page.contains("Remove the stored secret"), "{page}");

    // A change the provider can't take leaves the stored provider as it was, and the page shows
    // what was typed, except the secret.
    let body = edit(&[
        ("client_secret", "typed-secret"),
        ("issuer", "http://idp.example.com"),
        ("display_name", "Typed"),
    ]);
    let response = site
        .post(&mut admin, "/admin/identity-providers/idp", &body)
        .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let page = body_text(response).await;
    assert!(page.contains("http://idp.example.com"), "{page}");
    assert!(page.contains(r#"value="Typed""#), "{page}");
    assert!(!page.contains("typed-secret"), "{page}");
    let stored = site.env.store.oidc_provider("idp").await.unwrap().unwrap();
    assert_eq!(
        (stored.display_name.as_str(), stored.issuer.as_str()),
        ("Idp", site.idp.issuer.as_str())
    );
    assert_eq!(site.stored_secret(SECRET_PATH).await, None);

    // No such provider.
    for path in [
        "/admin/identity-providers/nope",
        "/admin/identity-providers/Not%20An%20Id",
    ] {
        let response = site.post(&mut admin, path, &edit(&[])).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let response = admin
        .get(&site.router, "/admin/identity-providers/nope/edit")
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn changing_the_issuer_disconnects_the_accounts_of_the_old_one() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    let mut admin = site.recovered("root").await;
    let other = FakeIdp::start(CLIENT_ID, Some(CLIENT_SECRET)).await;
    let body = provider_form(&admin.csrf(), &other.issuer, &[("client_secret", "")]);
    let response = site
        .post(&mut admin, "/admin/identity-providers/idp", &body)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    // The account belongs to the old issuer, which the provider no longer has.
    let mut browser = Browser::default();
    let start = browser.get(&site.router, &login_path("idp")).await;
    let back = path_of(&other.approve(&location(&start), "mia-sub").unwrap());
    let response = browser.get(&site.router, &back).await;
    assert_refused(response, &browser, "an account of the old issuer").await;
}

#[tokio::test]
async fn disabling_takes_a_provider_off_the_pages_and_enabling_puts_it_back() {
    let site = site().await;
    let mut admin = site.recovered("root").await;
    let csrf = admin.csrf();
    let toggle = |value: &str| format!("csrf={csrf}&enabled={value}");

    let response = site
        .post(
            &mut admin,
            "/admin/identity-providers/idp/enabled",
            &toggle("false"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        location(&response),
        "/admin/identity-providers#provider-idp"
    );
    let page = body_text(admin.get(&site.router, "/admin/identity-providers").await).await;
    assert!(page.contains("disabled"), "{page}");
    assert!(page.contains(r#"name="enabled" value="true""#), "{page}");
    let login = body_text(Browser::default().get(&site.router, "/login").await).await;
    assert!(!login.contains("/login/oidc/idp"), "{login}");
    // The discovery document isn't asked for: a provider that is down can be turned off.
    assert_eq!(site.idp.requests().discovery, 0);

    let response = site
        .post(
            &mut admin,
            "/admin/identity-providers/idp/enabled",
            &toggle("true"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let login = body_text(Browser::default().get(&site.router, "/login").await).await;
    assert!(login.contains("/login/oidc/idp"), "{login}");

    for (path, body) in [
        ("/admin/identity-providers/idp/enabled", toggle("maybe")),
        ("/admin/identity-providers/nope/enabled", toggle("true")),
    ] {
        let response = site.post(&mut admin, path, &body).await;
        assert!(response.status().is_client_error(), "{path}");
    }
}

#[tokio::test]
async fn removing_a_provider_asks_first_then_forgets_its_connections_and_its_secret() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    site.connected("root", "root-sub").await;
    let mut admin = site.recovered("root").await;

    let page = body_text(
        admin
            .get(&site.router, "/admin/identity-providers/idp/remove")
            .await,
    )
    .await;
    assert!(page.contains("Remove Idp?"), "{page}");
    assert!(page.contains("2 accounts"), "{page}");
    // Asking changed nothing.
    assert!(site.env.store.oidc_provider("idp").await.unwrap().is_some());
    assert!(site.stored_secret(SECRET_PATH).await.is_some());

    let body = format!("csrf={}", admin.csrf());
    let response = site
        .post(&mut admin, "/admin/identity-providers/idp/remove", &body)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/admin/identity-providers?removed=Idp");
    assert!(site.env.store.oidc_provider("idp").await.unwrap().is_none());
    assert_eq!(site.connection("mia-sub").await, None);
    assert!(
        site.env
            .store
            .user_oidc_connections(&id("mia"))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(site.stored_secret(SECRET_PATH).await, None);
    let page = body_text(
        admin
            .get(&site.router, "/admin/identity-providers?removed=Idp")
            .await,
    )
    .await;
    assert!(page.contains("Removed Idp."), "{page}");
    let login = body_text(Browser::default().get(&site.router, "/login").await).await;
    assert!(!login.contains("/login/oidc/"), "{login}");

    let response = site
        .post(&mut admin, "/admin/identity-providers/idp/remove", &body)
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn what_an_admin_or_a_provider_wrote_is_escaped() {
    let site = site().await;
    let mut admin = site.recovered("root").await;
    let xss = "<script>alert(1)</script>";
    let body = provider_form(
        &admin.csrf(),
        &site.idp.issuer,
        &[
            ("display_name", xss),
            ("client_secret", ""),
            ("client_id", "<b>x</b>"),
        ],
    );
    let response = site
        .post(&mut admin, "/admin/identity-providers/idp", &body)
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    for path in [
        "/admin/identity-providers",
        "/admin/identity-providers/idp/edit",
        "/admin/identity-providers/idp/remove",
        "/",
    ] {
        let page = body_text(admin.get(&site.router, path).await).await;
        assert!(!page.contains(xss), "{path}: {page}");
        assert!(!page.contains("<b>x</b>"), "{path}: {page}");
    }
    // The name is on the login page and the setup page, escaped too.
    let login = body_text(Browser::default().get(&site.router, "/login").await).await;
    assert!(!login.contains(xss), "{login}");
    assert!(login.contains("&#60;script&#62;alert(1)"), "{login}");
    let mut newbie = site.invited("newbie").await;
    let setup = body_text(newbie.get(&site.router, "/setup").await).await;
    assert!(!setup.contains(xss), "{setup}");
}

#[tokio::test]
async fn an_edit_the_store_would_refuse_changes_nothing_not_even_the_secret() {
    let site = site().await;
    let mut admin = site.recovered("root").await;
    let csrf = admin.csrf();
    // A scope with a character a scope can't have: only printable ASCII, which the form check
    // shares with the store, so the refusal comes before anything is written.
    for scopes in ["openid é", "openid \u{7f}x", "openid a\"b", "openid a\\b"] {
        let body = provider_form(
            &csrf,
            &site.idp.issuer,
            &[("client_secret", "typed-secret"), ("scopes", scopes)],
        );
        let response = site
            .post(&mut admin, "/admin/identity-providers/idp", &body)
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{scopes:?}");
        let page = body_text(response).await;
        assert!(
            page.contains("A provider&#39;s scopes must be valid scope tokens."),
            "{page}"
        );
        assert!(!page.contains("typed-secret"), "{page}");
        assert_eq!(
            site.stored_secret(SECRET_PATH).await.as_deref(),
            Some(CLIENT_SECRET),
            "{scopes:?}"
        );
        let stored = site.env.store.oidc_provider("idp").await.unwrap().unwrap();
        assert_eq!(stored.scopes, ["openid", "profile"], "{scopes:?}");
    }
    // The same for a new provider: no row and no secret.
    let body = provider_form(
        &csrf,
        &site.idp.issuer,
        &[("id", "second"), ("scopes", "openid é")],
    );
    let response = site
        .post(&mut admin, "/admin/identity-providers", &body)
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        site.env
            .store
            .oidc_provider("second")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(site.stored_secret("oidc/second/client-secret").await, None);
}

#[tokio::test]
async fn an_edit_of_a_provider_removed_meanwhile_stores_no_secret() {
    let site = site().await;
    let mut admin = site.recovered("root").await;
    // The form was shown, then the provider (and its secret) went.
    let path = SecretPath::new(SECRET_PATH).unwrap();
    assert!(site.env.store.oidc_provider_remove("idp").await.unwrap());
    site.secrets.delete(&path).await.unwrap();
    let body = provider_form(
        &admin.csrf(),
        &site.idp.issuer,
        &[("client_secret", "typed")],
    );
    let response = site
        .post(&mut admin, "/admin/identity-providers/idp", &body)
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(site.stored_secret(SECRET_PATH).await, None);
}

#[tokio::test]
async fn the_removal_page_counts_every_connection_the_removal_forgets() {
    let site = site().await;
    site.connected("mia", "mia-sub").await;
    // A connection under an issuer the provider no longer has goes with the provider too.
    site.env
        .store
        .user_oidc_connect(
            "idp",
            "https://old.example.com",
            "old-sub",
            &id("root"),
            Timestamp::now(),
        )
        .await
        .unwrap();
    let mut admin = site.recovered("root").await;
    let list = body_text(admin.get(&site.router, "/admin/identity-providers").await).await;
    assert!(
        list.contains("1 account"),
        "only the one that can sign in: {list}"
    );
    let page = body_text(
        admin
            .get(&site.router, "/admin/identity-providers/idp/remove")
            .await,
    )
    .await;
    assert!(page.contains("2 accounts"), "{page}");
    assert!(
        page.contains("stay logged in") && page.contains("disable the user"),
        "says that logins made with the provider last: {page}"
    );
}
