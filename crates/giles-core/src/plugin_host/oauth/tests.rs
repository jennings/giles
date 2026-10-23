//! The token client against a fake token endpoint on `127.0.0.1`, and the response parsing.

use std::sync::Mutex;
use std::time::Instant;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use base64::Engine as _;

use super::*;

/// One request the fake endpoint got: its `authorization` header and body.
#[derive(Debug, Clone)]
struct Seen {
    authorization: Option<String>,
    content_type: Option<String>,
    body: String,
    at: Instant,
}

/// A fake token endpoint: answers each request with `status` and `body(n)` for the `n`th request
/// (from 1), and records them.
struct Endpoint {
    url: url::Url,
    seen: Arc<Mutex<Vec<Seen>>>,
}

async fn endpoint(
    status: StatusCode,
    body: impl Fn(usize) -> String + Send + Sync + 'static,
) -> Endpoint {
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let body = Arc::new(body);
    let app = Router::new().route(
        "/token",
        post({
            let seen = Arc::clone(&seen);
            move |headers: HeaderMap, request: String| {
                let (seen, body) = (Arc::clone(&seen), Arc::clone(&body));
                async move {
                    let header =
                        |name: &str| headers.get(name).map(|v| v.to_str().unwrap().to_owned());
                    let n = {
                        let mut seen = seen.lock().unwrap();
                        seen.push(Seen {
                            authorization: header("authorization"),
                            content_type: header("content-type"),
                            body: request,
                            at: Instant::now(),
                        });
                        seen.len()
                    };
                    (status, body(n))
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Endpoint {
        url: format!("http://{addr}/token").parse().unwrap(),
        seen,
    }
}

fn client(url: &url::Url, scopes: &[&str]) -> TokenClient {
    TokenClient::new(OAuthClientCredentials {
        token_url: url.clone(),
        client_id: "giles core".into(),
        client_secret: Secret::new("s3cr:et&"),
        scopes: scopes.iter().map(|s| (*s).to_owned()).collect(),
    })
    .unwrap()
}

#[tokio::test]
async fn a_token_request_uses_basic_auth_and_the_form_body() {
    let fake = endpoint(StatusCode::OK, |n| {
        format!(r#"{{"access_token":"tok-{n}","token_type":"Bearer","expires_in":3600}}"#)
    })
    .await;
    let token = client(&fake.url, &["channel", "read"])
        .fetch()
        .await
        .unwrap();
    assert_eq!(token.value.expose(), "tok-1");
    assert_eq!(token.expires_in, Some(Duration::from_secs(3600)));
    assert_eq!(token.header().to_str().unwrap(), "Bearer tok-1");

    let seen = fake.seen.lock().unwrap()[0].clone();
    let basic = seen.authorization.unwrap();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(basic.strip_prefix("Basic ").unwrap())
        .unwrap();
    // RFC 6749 section 2.3.1: both parts form-encoded before Basic encoding.
    assert_eq!(
        String::from_utf8(decoded).unwrap(),
        "giles+core:s3cr%3Aet%26"
    );
    assert_eq!(
        seen.content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    assert_eq!(
        seen.body,
        "grant_type=client_credentials&scope=channel+read"
    );

    // No scopes: no scope parameter.
    client(&fake.url, &[]).fetch().await.unwrap();
    assert_eq!(
        fake.seen.lock().unwrap()[1].body,
        "grant_type=client_credentials"
    );
}

#[tokio::test]
async fn a_refused_request_names_the_status_and_the_oauth_error() {
    let fake = endpoint(StatusCode::UNAUTHORIZED, |_| {
        r#"{"error":"invalid_client","error_description":"unknown\nclient"}"#.into()
    })
    .await;
    let error = client(&fake.url, &[]).fetch().await.unwrap_err();
    assert_eq!(
        error,
        "the token endpoint answered 401 Unauthorized: invalid_client (unknownclient)"
    );

    let fake = endpoint(StatusCode::INTERNAL_SERVER_ERROR, |_| "oops".into()).await;
    let error = client(&fake.url, &[]).fetch().await.unwrap_err();
    assert_eq!(
        error,
        "the token endpoint answered 500 Internal Server Error"
    );

    let fake = endpoint(StatusCode::OK, |_| "x".repeat(MAX_RESPONSE + 1)).await;
    let error = client(&fake.url, &[]).fetch().await.unwrap_err();
    assert!(error.contains("longer than"), "{error}");

    let error = client(&"http://127.0.0.1:9/token".parse().unwrap(), &[])
        .fetch()
        .await
        .unwrap_err();
    assert!(
        error.starts_with("the token endpoint can't be reached"),
        "{error}"
    );
}

#[test]
fn token_responses_are_checked() {
    let ok = |body: &str| parse_token(body.as_bytes()).unwrap();
    let token = ok(r#"{"access_token":"a.b-c_d~e+f/g=","token_type":"bearer"}"#);
    assert_eq!(token.value.expose(), "a.b-c_d~e+f/g=");
    assert_eq!(token.expires_in, None);
    assert_eq!(
        ok(r#"{"access_token":"t","expires_in":"60"}"#).expires_in,
        Some(Duration::from_secs(60))
    );
    assert_eq!(
        ok(r#"{"access_token":"t","expires_in":null,"scope":"x"}"#).expires_in,
        None
    );

    for (body, why) in [
        ("not json", "not a JSON object"),
        ("[]", "not a JSON object"),
        (r#"{"token_type":"Bearer"}"#, "no access_token"),
        (r#"{"access_token":""}"#, "not visible ASCII"),
        (r#"{"access_token":"a b"}"#, "not visible ASCII"),
        (r#"{"access_token":"t\u00e9"}"#, "not visible ASCII"),
        (
            r#"{"access_token":"t","token_type":"mac"}"#,
            "is not Bearer",
        ),
        (r#"{"access_token":"t","expires_in":-5}"#, "expires_in"),
        (r#"{"access_token":"t","expires_in":"soon"}"#, "expires_in"),
    ] {
        let error = parse_token(body.as_bytes()).unwrap_err();
        assert!(error.contains(why), "{body}: {error}");
        assert!(!error.contains("a b"), "the token isn't echoed: {error}");
    }
}

#[test]
fn tokens_are_refreshed_before_they_expire() {
    let s = Duration::from_secs;
    assert_eq!(refresh_delay(s(3600)), s(3540));
    assert_eq!(refresh_delay(s(120)), s(60));
    assert_eq!(refresh_delay(s(60)), s(30));
    assert_eq!(refresh_delay(s(2)), s(1));
    assert_eq!(refresh_delay(s(1)), s(1));
    assert_eq!(refresh_delay(s(0)), s(1));
}

#[tokio::test]
async fn the_header_follows_each_refreshed_token() {
    // Each token is valid for 2 s, so it is refreshed after 1 s.
    let fake = endpoint(StatusCode::OK, |n| {
        format!(r#"{{"access_token":"tok-{n}","expires_in":2}}"#)
    })
    .await;
    let client = client(&fake.url, &[]);
    let first = client.fetch().await.unwrap();
    let header: SharedHeader = Arc::new(RwLock::new(first.header()));
    let secrets = Secrets::new(vec![first.value.clone()]);
    let refresh = keep_fresh(
        client,
        Arc::clone(&header),
        first.expires_in,
        first.value,
        secrets.clone(),
    );
    let current = || header.read().unwrap().to_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(10);
    while current() != "Bearer tok-3" {
        assert!(Instant::now() < deadline, "still {}", current());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let seen = fake.seen.lock().unwrap().clone();
    let gap = seen[2].at - seen[1].at;
    assert!(gap >= Duration::from_millis(900), "refreshed after {gap:?}");
    // Each new token is scrubbed; the oldest has been dropped.
    assert_eq!(
        secrets.scrub("tok-1 tok-2 tok-3"),
        "tok-1 <redacted> <redacted>"
    );

    // Dropping the handle stops the refreshes.
    drop(refresh);
    let count = fake.seen.lock().unwrap().len();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(fake.seen.lock().unwrap().len(), count);
}

#[tokio::test]
async fn a_failed_refresh_keeps_the_old_token_and_retries() {
    // The first refresh fails, the retry a second later succeeds.
    let fake = endpoint(StatusCode::OK, |n| match n {
        2 => "not json".into(),
        _ => format!(r#"{{"access_token":"tok-{n}","expires_in":2}}"#),
    })
    .await;
    let client = client(&fake.url, &[]);
    let first = client.fetch().await.unwrap();
    let header: SharedHeader = Arc::new(RwLock::new(first.header()));
    let _refresh = keep_fresh(
        client,
        Arc::clone(&header),
        first.expires_in,
        first.value.clone(),
        Secrets::new(vec![first.value]),
    );
    let current = || header.read().unwrap().to_str().unwrap().to_owned();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fake.seen.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(current(), "Bearer tok-1", "kept after the failure");
    while current() != "Bearer tok-3" {
        assert!(Instant::now() < deadline, "still {}", current());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
