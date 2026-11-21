//! Password sign-in through the real handlers: setting a password through an invite, logging in
//! with it, and what must hold when it goes wrong: one answer for every failure, the same work for
//! an unknown username as for a wrong password, limits on guessing, and no session fixation.
//!
//! The users are the ones [`stored_users`] makes (`root`, an admin, and `mia`, a member), each of
//! whom can sign in, and `newbie`, whom an invite sets up.

use std::sync::atomic::Ordering;

use axum::extract::ConnectInfo;
use axum::http::header::RETRY_AFTER;
use doozers_store::UserState;

use super::admin::{open_users, stored_users};
use super::*;
use crate::channels::web::limits::{ADDRESS_ATTEMPTS, USERNAME_ATTEMPTS};
use crate::channels::web::password::{HASH_PREFIX, check_new};
use crate::http::PeerAddr;

const GOOD: &str = "correct horse battery staple";

/// Like [`call`], from the client `peer`, as the server tells the handlers.
async fn call_from(
    app: axum::Router,
    peer: &str,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    form: Option<&str>,
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match form {
        Some(form) => {
            request = request.header(CONTENT_TYPE, "application/x-www-form-urlencoded");
            Body::from(form.to_owned())
        }
        None => Body::empty(),
    };
    let mut request = request.body(body).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(PeerAddr(Some(peer.parse().unwrap()))));
    app.oneshot(request).await.unwrap()
}

/// `text` as a form value.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Gives `user` the password `password`, without going through the pages.
async fn set_password(env: &Env, user: &str, password: &str) {
    let normalized = check_new(password, user).unwrap();
    let hash = env.app.inner.passwords.hash(normalized).await.unwrap();
    env.store
        .user_password_set(&UserId::new(user).unwrap(), &hash, Timestamp::now())
        .await
        .unwrap();
}

/// The login form's POST as a browser sends it from `peer`, with the nonce of the form it loaded.
async fn log_in_from(
    env: &Env,
    peer: &str,
    username: &str,
    password: &str,
    headers: &[(&str, &str)],
) -> Response {
    let cookie = format!("{NONCE_COOKIE}={NONCE}");
    let form = format!(
        "username={}&password={}&nonce={NONCE}",
        encode(username),
        encode(password)
    );
    let mut sent = vec![("cookie", cookie.as_str())];
    sent.extend_from_slice(headers);
    call_from(
        env.main_app(),
        peer,
        Method::POST,
        "/login/password",
        &sent,
        Some(&form),
    )
    .await
}

async fn log_in_as(env: &Env, username: &str, password: &str) -> Response {
    log_in_from(env, "192.0.2.1", username, password, &[]).await
}

/// Asserts that `response` signs nobody in: any cookie it sets is the sign-in nonce's.
fn assert_no_session(response: &Response, what: &str) {
    for cookie in set_cookies(response) {
        assert!(
            cookie.starts_with(&format!("{NONCE_COOKIE}=")),
            "{what}: {cookie}"
        );
    }
}

/// How many password checks have run, and how many of them were against the dummy hash.
fn checks(env: &Env) -> (usize, usize) {
    let ran = &env.app.inner.passwords.ran;
    (
        ran.checked.load(Ordering::SeqCst),
        ran.checked_dummy.load(Ordering::SeqCst),
    )
}

/// The page of a failed login, with the username the browser sent made the same in every one.
async fn failed_page(response: Response, username: &str) -> String {
    body_text(response)
        .await
        .replace(&format!(r#"value="{username}""#), r#"value="USER""#)
}

#[tokio::test]
async fn a_user_sets_a_password_through_an_invite_and_signs_in_with_it() {
    let env = env().await;
    let users = open_users(&env).await;
    let newbie = add_user(&users, "newbie", doozers_store::Role::Member).await;
    let invite = invite_token(&env, &newbie, InviteKind::Invite).await;

    // The invite signs the browser in, and the setup page offers a password.
    let response = redeem(env.main_app(), &invite, &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let token = cookie_token(set_cookies(&response)[0]);
    let cookie = session(&token);
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    let html = body_text(response).await;
    assert!(
        html.contains(r#"<a href="/setup/password">Password</a>"#),
        "{html}"
    );
    assert!(html.contains("at least 15 characters"), "{html}");

    // The form names the user, and says what a password has to be.
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup/password",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    let html = body_text(response).await;
    assert!(html.contains("<h1>Set a password</h1>"), "{html}");
    assert!(html.contains(r#"value="newbie""#), "{html}");
    assert!(html.contains(r#"autocomplete="new-password""#), "{html}");
    assert!(html.contains("Use 15 to 128 characters"), "{html}");
    assert!(!html.contains("isn't encrypted"), "loopback: {html}");
    assert!(env.store.user_password(&newbie).await.unwrap().is_none());

    // Setting it: 303 to the page that says so, and only the hash is stored.
    let csrf = auth::cookie_csrf(&token);
    let form = format!(
        "csrf={csrf}&password={}&confirm={}",
        encode(GOOD),
        encode(GOOD)
    );
    let response = call(
        env.main_app(),
        Method::POST,
        "/setup/password",
        &[("cookie", &cookie)],
        Some(&form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/setup/password?saved=1");
    let stored = env.store.user_password(&newbie).await.unwrap().unwrap();
    assert!(stored.hash.starts_with(HASH_PREFIX), "{}", stored.hash);
    assert!(!stored.hash.contains("horse"));
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup/password?saved=1",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    let html = body_text(response).await;
    assert!(html.contains("Your password is saved"), "{html}");
    assert!(html.contains("<h1>Change your password</h1>"), "{html}");

    // Another browser logs in with it. The login page hands out the nonce its form carries.
    let response = call(env.main_app(), Method::GET, "/login", &[], None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let nonce = nonce_of(set_cookie(&response));
    let html = body_text(response).await;
    assert!(
        html.contains(r#"<form class="form form-flush" method="post" action="/login/password">"#),
        "{html}"
    );
    assert!(
        html.contains(&format!(r#"name="nonce" value="{nonce}""#)),
        "{html}"
    );
    assert!(
        html.contains(r#"autocomplete="current-password""#),
        "{html}"
    );
    assert!(
        !html.contains("<details"),
        "no passkeys, so the form is shown: {html}"
    );

    let cookie = format!("{NONCE_COOKIE}={nonce}");
    let form = format!("username=NewBie&password={}&nonce={nonce}", encode(GOOD));
    let response = call(
        env.main_app(),
        Method::POST,
        "/login/password",
        &[("cookie", &cookie)],
        Some(&form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/");
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    let cookies = set_cookies(&response);
    assert_eq!(cookies.len(), 2, "{cookies:?}");
    assert!(
        cookies[0].ends_with("; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax"),
        "{cookies:?}"
    );
    assert_eq!(
        cookies[1],
        format!("{NONCE_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict")
    );
    let token = cookie_token(cookies[0]);
    let stored = env
        .store
        .web_session(&token, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.user, newbie);
    assert_eq!(stored.method, SignInMethod::Password);
    // The session works: the chat list answers, as a user whose page is theirs.
    let response = call(
        env.main_app(),
        Method::GET,
        "/",
        &[("cookie", &session(&token))],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (hashed, checked) = (
        env.app.inner.passwords.ran.hashed.load(Ordering::SeqCst),
        env.app.inner.passwords.ran.checked.load(Ordering::SeqCst),
    );
    assert_eq!((hashed, checked), (1, 1));
}

#[tokio::test]
async fn a_password_sign_in_needs_the_nonce_of_its_browsers_form() {
    let env = env().await;
    stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    let form = format!("username=mia&password={}&nonce={NONCE}", encode(GOOD));
    let other = NONCE.replace('0', "f");
    let cookie = format!("{NONCE_COOKIE}={NONCE}");
    let wrong_cookie = format!("{NONCE_COOKIE}={other}");
    let no_nonce = format!("username=mia&password={}", encode(GOOD));
    for (what, headers, form) in [
        ("no cookie", vec![], form.as_str()),
        (
            "another browser's nonce",
            vec![("cookie", wrong_cookie.as_str())],
            form.as_str(),
        ),
        (
            "no field",
            vec![("cookie", cookie.as_str())],
            no_nonce.as_str(),
        ),
    ] {
        let response = call_from(
            env.main_app(),
            "192.0.2.1",
            Method::POST,
            "/login/password",
            &headers,
            Some(form),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{what}");
        assert!(!response.headers().contains_key(SET_COOKIE), "{what}");
    }
    assert_eq!(checks(&env), (0, 0), "nothing was hashed or counted");
}

#[tokio::test]
async fn every_way_to_fail_is_the_same_answer_with_the_same_work() {
    let env = env().await;
    let users = stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    set_password(&env, "root", GOOD).await;
    users
        .set_state(
            &UserId::new("root").unwrap(),
            UserState::Disabled,
            Timestamp::now(),
        )
        .await
        .unwrap();

    // (what, username, password, does it check the dummy hash)
    let cases = [
        (
            "a wrong password",
            "mia",
            "correct horse battery stapl",
            false,
        ),
        ("an unknown user", "ghost", GOOD, true),
        ("a user with no password", "newbie-less", GOOD, true),
        ("a disabled user's right password", "root", GOOD, true),
    ];
    // A user without a password: one who exists, with no password row.
    add_user(&users, "newbie-less", doozers_store::Role::Member).await;
    let mut pages = Vec::new();
    for (what, username, password, dummy) in cases {
        let (before, before_dummy) = checks(&env);
        let response = log_in_as(&env, username, password).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{what}");
        assert_no_session(&response, what);
        assert!(!response.headers().contains_key(RETRY_AFTER), "{what}");
        let (after, after_dummy) = checks(&env);
        assert_eq!(after, before + 1, "{what} ran one check");
        assert_eq!(after_dummy, before_dummy + usize::from(dummy), "{what}");
        pages.push((what, failed_page(response, username).await));
    }
    let (_, first) = &pages[0];
    assert!(first.contains("Wrong username or password."), "{first}");
    for (what, page) in &pages {
        assert_eq!(page, first, "{what}");
    }
    // None of them signed anyone in, and the right password of an active user still does.
    let response = log_in_as(&env, "mia", GOOD).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    // A password isn't the only check: the disabled user is let in when they are active again.
    users
        .set_state(
            &UserId::new("root").unwrap(),
            UserState::Active,
            Timestamp::now(),
        )
        .await
        .unwrap();
    let response = log_in_as(&env, "root", GOOD).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_username_gets_ten_attempts_and_then_waits() {
    let env = env().await;
    stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    set_password(&env, "root", GOOD).await;
    for attempt in 0..USERNAME_ATTEMPTS {
        let response = log_in_as(&env, "mia", &format!("not the password {attempt}")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{attempt}");
    }
    let (checked, _) = checks(&env);
    // The next attempt is refused, the right password and any case of the name included, and
    // nothing is hashed for it.
    for username in ["mia", "MIA"] {
        let response = log_in_as(&env, username, GOOD).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let wait: u64 = response.headers()[RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((890..=900).contains(&wait), "{wait}");
        assert_no_session(&response, "limited");
        let html = body_text(response).await;
        assert!(
            html.contains("Too many attempts. Try again in 15 minutes."),
            "{html}"
        );
    }
    assert_eq!(checks(&env).0, checked);
    // Another user, from the same address, isn't held up.
    let response = log_in_as(&env, "root", GOOD).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn an_unknown_username_is_limited_like_a_real_one() {
    let env = env().await;
    stored_users(&env).await;
    for attempt in 0..USERNAME_ATTEMPTS {
        let response = log_in_as(&env, "ghost", &format!("not the password {attempt}")).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let response = log_in_as(&env, "ghost", GOOD).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_client_address_gets_thirty_attempts_across_usernames_and_then_waits() {
    let env = env().await;
    stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    for attempt in 0..ADDRESS_ATTEMPTS {
        let response =
            log_in_from(&env, "192.0.2.50", &format!("ghost{attempt}"), "x y z", &[]).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{attempt}");
    }
    let (checked, _) = checks(&env);
    let response = log_in_from(&env, "192.0.2.50", "mia", GOOD, &[]).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(checks(&env).0, checked, "nothing was hashed");
    // Another address is another client.
    let response = log_in_from(&env, "192.0.2.51", "mia", GOOD, &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn an_ipv6_client_counts_by_its_64_bit_prefix() {
    let env = env().await;
    stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    for attempt in 0..ADDRESS_ATTEMPTS {
        let peer = format!("2001:db8:1:2::{attempt:x}");
        let response = log_in_from(&env, &peer, &format!("ghost{attempt}"), "x y z", &[]).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{attempt}");
    }
    // Another address of the same /64 is the same client, the next /64 is not.
    let response = log_in_from(&env, "2001:db8:1:2:ffff::9", "mia", GOOD, &[]).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let response = log_in_from(&env, "2001:db8:1:3::1", "mia", GOOD, &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn behind_a_proxy_on_the_main_listener_only_usernames_are_limited() {
    let env = env().await;
    stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    set_password(&env, "root", GOOD).await;
    // The proxy is on this machine, so its peer address is every visitor's: more wrong passwords
    // than an address gets, for different usernames, lock nobody out.
    for peer in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
        for attempt in 0..ADDRESS_ATTEMPTS {
            let name = format!("ghost{peer}-{attempt}");
            let response = log_in_from(&env, peer, &name, "x y z", &[]).await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{peer} {attempt}"
            );
        }
    }
    let response = log_in_from(&env, "127.0.0.1", "mia", GOOD, &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    // A username is still limited, whoever the proxy says is asking.
    for attempt in 0..USERNAME_ATTEMPTS {
        let response = log_in_from(
            &env,
            "127.0.0.1",
            "root",
            &format!("not {attempt} {attempt}"),
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{attempt}");
    }
    let response = log_in_from(&env, "127.0.0.1", "root", GOOD, &[]).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    // A forged forwarding header changes nothing on this listener.
    let response = log_in_from(
        &env,
        "127.0.0.1",
        "root",
        GOOD,
        &[("x-forwarded-for", "198.51.100.1")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn behind_the_proxy_the_forwarded_address_is_the_client_and_elsewhere_it_is_ignored() {
    let config = proxy_config(serde_json::json!({ "identity_header": "X-User" }));
    let env = env_with(config, &["web:owner"]).await;
    stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    let cookie = format!("{NONCE_COOKIE}={NONCE}");

    // On the main listener a forged `X-Forwarded-For` doesn't hide the client: 30 attempts, each
    // claiming to be someone else, use up the real address.
    for attempt in 0..ADDRESS_ATTEMPTS {
        let forged = format!("198.51.100.{attempt}");
        let response = log_in_from(
            &env,
            "192.0.2.60",
            &format!("ghost{attempt}"),
            "x y z",
            &[("x-forwarded-for", &forged)],
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{attempt}");
    }
    let response = log_in_from(
        &env,
        "192.0.2.60",
        "mia",
        GOOD,
        &[("x-forwarded-for", "198.51.100.200")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    // On the proxy listener the peer is the proxy, and the client is the address the proxy
    // appended: one proxied client using up its attempts doesn't shut the others out.
    let proxied = |forwarded: &'static str| {
        let env = &env;
        let cookie = cookie.clone();
        async move {
            let form = format!("username=ghost&password=x+y+z&nonce={NONCE}");
            call_from(
                env.proxy_app(),
                "127.0.0.1",
                Method::POST,
                "/login/password",
                &[("cookie", &cookie), ("x-forwarded-for", forwarded)],
                Some(&form),
            )
            .await
        }
    };
    // `ghost` is limited per username too, so use fresh names for the address's count.
    for attempt in 0..ADDRESS_ATTEMPTS {
        let form = format!("username=g{attempt}&password=x+y+z&nonce={NONCE}");
        let response = call_from(
            env.proxy_app(),
            "127.0.0.1",
            Method::POST,
            "/login/password",
            &[
                ("cookie", &cookie),
                ("x-forwarded-for", "203.0.113.9, 192.0.2.77"),
            ],
            Some(&form),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{attempt}");
    }
    let limited = proxied("1.2.3.4, 192.0.2.77").await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let other = proxied("203.0.113.9, 192.0.2.78").await;
    assert_eq!(other.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn signing_in_replaces_the_session_the_browser_had() {
    let env = env().await;
    stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    let old = session_of_user(&env, "root").await;
    assert!(
        env.store
            .web_session(&old, Timestamp::now())
            .await
            .unwrap()
            .is_some()
    );

    let response = log_in_from(
        &env,
        "192.0.2.1",
        "mia",
        GOOD,
        &[("cookie", &session(&old))],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let new = cookie_token(set_cookies(&response)[0]);
    assert_ne!(new, old);
    assert!(
        env.store
            .web_session(&old, Timestamp::now())
            .await
            .unwrap()
            .is_none(),
        "the old session is gone, so its token can't be replayed"
    );
    let now = env
        .store
        .web_session(&new, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(now.user, UserId::new("mia").unwrap());
    assert_eq!(now.method, SignInMethod::Password);
    // Two sign-ins never share a session.
    let again = log_in_as(&env, "mia", GOOD).await;
    assert_ne!(cookie_token(set_cookies(&again)[0]), new);
}

/// A recovery session of `user` (a second `cookie` in the request would be the nonce).
async fn session_of_user(env: &Env, user: &str) -> String {
    super::admin::session_of(env, user).await
}

#[tokio::test]
async fn a_new_password_must_follow_the_rules_and_changes_nothing_when_it_does_not() {
    let env = env().await;
    stored_users(&env).await;
    let token = session_of_user(&env, "root").await;
    let csrf = auth::cookie_csrf(&token);
    let cookie = session(&token);
    let root = UserId::new("root").unwrap();
    let post = |password: &str, confirm: &str, csrf: &str| {
        let form = format!(
            "csrf={csrf}&password={}&confirm={}",
            encode(password),
            encode(confirm)
        );
        let (env, cookie) = (&env, cookie.clone());
        async move {
            call(
                env.main_app(),
                Method::POST,
                "/setup/password",
                &[("cookie", &cookie)],
                Some(&form),
            )
            .await
        }
    };
    for (what, password, confirm, said) in [
        (
            "short",
            "too short",
            "too short",
            "Use at least 15 characters",
        ),
        (
            "different",
            GOOD,
            "correct horse battery stapl",
            "The two passwords differ",
        ),
        (
            "long",
            &"ab".repeat(65),
            &"ab".repeat(65),
            "Use at most 128 characters",
        ),
        (
            "repeated",
            &"z".repeat(20),
            &"z".repeat(20),
            "one character repeated",
        ),
        (
            "the username",
            "my name is root root root",
            "my name is root root root",
            "username",
        ),
        (
            "the service",
            "i like doozers a lot",
            "i like doozers a lot",
            "Doozers",
        ),
    ] {
        let response = post(password, confirm, &csrf).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{what}");
        let html = body_text(response).await;
        assert!(html.contains(said), "{what}: {html}");
        assert!(html.contains(r#"role="alert""#), "{what}: {html}");
        assert!(
            !html.contains(password),
            "the form never echoes a password: {what}"
        );
        assert!(
            env.store.user_password(&root).await.unwrap().is_none(),
            "{what}"
        );
    }
    // The CSRF token is checked first.
    let response = post(GOOD, GOOD, "wrong").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(env.store.user_password(&root).await.unwrap().is_none());
    // No composition rules: lowercase letters and spaces are a password.
    let response = post(GOOD, GOOD, &csrf).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(env.store.user_password(&root).await.unwrap().is_some());
}

#[tokio::test]
async fn only_a_login_that_came_from_a_link_may_set_a_password() {
    let env = env().await;
    stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    let mia = UserId::new("mia").unwrap();
    let before = env.store.user_password(&mia).await.unwrap().unwrap();

    // A login with the password can't change it, and the setup page doesn't offer to.
    let response = log_in_as(&env, "mia", GOOD).await;
    let token = cookie_token(set_cookies(&response)[0]);
    let cookie = session(&token);
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    let html = body_text(response).await;
    assert!(!html.contains("/setup/password"), "{html}");
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup/password",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let html = body_text(response).await;
    assert!(html.contains("Confirm it's you first"), "{html}");
    let form = format!(
        "csrf={}&password=a+completely+new+password&confirm=a+completely+new+password",
        auth::cookie_csrf(&token)
    );
    let response = call(
        env.main_app(),
        Method::POST,
        "/setup/password",
        &[("cookie", &cookie)],
        Some(&form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        env.store.user_password(&mia).await.unwrap().unwrap(),
        before
    );

    // A recovery link can: the new password replaces the old one.
    let token = session_of_user(&env, "mia").await;
    let cookie = session(&token);
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    let html = body_text(response).await;
    assert!(html.contains("Change your password"), "{html}");
    let form = format!(
        "csrf={}&password=a+completely+new+password&confirm=a+completely+new+password",
        auth::cookie_csrf(&token)
    );
    let response = call(
        env.main_app(),
        Method::POST,
        "/setup/password",
        &[("cookie", &cookie)],
        Some(&form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let after = env.store.user_password(&mia).await.unwrap().unwrap();
    assert_ne!(after.hash, before.hash);
    assert_eq!(
        log_in_as(&env, "mia", GOOD).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        log_in_as(&env, "mia", "a completely new password")
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn the_proxy_login_has_no_password_to_set() {
    let config = proxy_config(serde_json::json!({ "identity_header": "X-User" }));
    let env = env_with(config, &["web:owner"]).await;
    let response = call(
        env.proxy_app(),
        Method::GET,
        "/setup/password",
        &[("x-user", "owner")],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn pages_with_a_password_warn_when_they_are_plain_http_off_this_machine() {
    let env = env().await;
    stored_users(&env).await;
    set_password(&env, "mia", GOOD).await;
    let warns = |html: &str| html.contains("This page isn't encrypted.");
    let login = |host: Option<&'static str>, extra: Vec<(&'static str, &'static str)>| {
        let env = &env;
        async move {
            let mut headers = extra;
            if let Some(host) = host {
                headers.push(("host", host));
            }
            body_text(call(env.main_app(), Method::GET, "/login", &headers, None).await).await
        }
    };
    // Without a Host header the page is where the server listens: loopback.
    assert!(!warns(&login(None, vec![]).await));
    for host in [
        "localhost:8080",
        "127.0.0.1:8080",
        "[::1]:8080",
        "chat.localhost",
    ] {
        assert!(!warns(&login(Some(host), vec![]).await), "{host}");
    }
    for host in [
        "doozers.lan:8080",
        "192.168.1.20:8080",
        "10.0.0.5",
        "localhost.example.com",
    ] {
        let html = login(Some(host), vec![]).await;
        assert!(warns(&html), "{host}");
        assert!(html.contains("read your password"), "{html}");
    }
    // The failed login's page, which holds the form again, warns too.
    let response = log_in_from(
        &env,
        "192.0.2.1",
        "mia",
        "wrong wrong wrong",
        &[("host", "doozers.lan")],
    )
    .await;
    assert!(warns(&body_text(response).await));
    // So does the page that sets a password.
    let token = session_of_user(&env, "mia").await;
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup/password",
        &[("cookie", &session(&token)), ("host", "doozers.lan")],
        None,
    )
    .await;
    assert!(warns(&body_text(response).await));
    // A trusted proxy that says the browser used HTTPS makes it encrypted; nobody else can say so.
    let config = proxy_config(serde_json::json!({ "identity_header": "X-User" }));
    let proxied = env_with(config, &["web:owner"]).await;
    let page = |app: axum::Router, headers: Vec<(&'static str, &'static str)>| async move {
        body_text(call(app, Method::GET, "/login", &headers, None).await).await
    };
    let tls = vec![("host", "doozers.lan"), ("x-forwarded-proto", "https")];
    assert!(!warns(&page(proxied.proxy_app(), tls.clone()).await));
    assert!(warns(&page(proxied.main_app(), tls).await));
    // An https public URL does too.
    let https =
        WebConfig::from_json(&serde_json::json!({ "public_url": "https://doozers.example.com" }))
            .unwrap();
    let secure = env_with(https, &["web:owner"]).await;
    let html = page(secure.main_app(), vec![("host", "doozers.lan")]).await;
    assert!(!warns(&html));
}

#[tokio::test]
async fn the_username_comes_back_in_the_form_escaped_and_the_password_never_does() {
    let env = env().await;
    let response = log_in_as(
        &env,
        r#""><script>alert(1)</script>"#,
        "hunter2 hunter2 hunter2",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let html = body_text(response).await;
    assert!(!html.contains("<script>alert"), "{html}");
    assert!(html.contains("&#34;&#62;&#60;script&#62;"), "{html}");
    assert!(!html.contains("hunter2"), "{html}");
}

#[tokio::test]
async fn usernames_are_found_without_regard_to_ascii_case() {
    let env = env().await;
    stored_users(&env).await;
    let directory = env.channel.users().get();
    for name in ["mia", "MIA", "Mia"] {
        assert_eq!(
            directory.by_username(name).map(|user| user.id.as_str()),
            Some("mia")
        );
    }
    assert!(directory.by_username("mi").is_none());
    assert!(directory.by_username("").is_none());
}
