//! The twilight client (`RealDiscord`) against a REST server of the test's own, on loopback: what
//! it asks for, what it makes of the answers, and of the errors. Nothing here reaches Discord, and
//! the token is not one. The server plays Discord from its documentation (issue #20 checks it).

mod loopback;

use std::time::Duration;

use giles_channel_discord::client::{Discord, RestError};
use loopback::rest::{Response, RestServer};
use loopback::{TOKEN, WAIT, refused_address, rest_client};
use serde_json::{Value, json};

const CHANNEL: u64 = 319_674_150_115_610_528;

/// A DM from a person, as Discord documents a message object.
fn message(id: u64, content: &str) -> Value {
    loopback::message_json(id, CHANNEL, 11_111_111_111_111_111, content)
}

async fn history(client: &impl Discord, after: u64, limit: u8) -> Result<Vec<u64>, RestError> {
    let page = tokio::time::timeout(WAIT, client.messages_after(CHANNEL, after, limit))
        .await
        .expect("an answer in time")?;
    Ok(page.messages.iter().map(|message| message.id).collect())
}

#[tokio::test]
async fn it_asks_for_the_page_after_a_message_and_hands_it_over_oldest_first() {
    let server = RestServer::start(|_, _| {
        // Discord sends a page newest first.
        Response::json(
            200,
            &json!([message(30, "c"), message(20, "b"), message(10, "a")]),
        )
    })
    .await;
    let client = rest_client(&server.host);

    assert_eq!(history(&client, 5, 100).await.unwrap(), [10, 20, 30]);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "GET");
    assert_eq!(
        request.target.split('?').next().unwrap(),
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    assert_eq!(request.query("after"), Some("5"));
    assert_eq!(request.query("limit"), Some("100"));
    // The token goes in `Authorization`, with the `Bot ` prefix, and nowhere else.
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bot {TOKEN}").as_str())
    );
    assert!(!request.target.contains(TOKEN));
    assert!(
        request
            .header("user-agent")
            .is_some_and(|agent| agent.starts_with("Discord")),
        "Discord asks for a `DiscordBot` user agent: {:?}",
        request.header("user-agent")
    );
}

#[tokio::test]
async fn an_id_of_zero_is_asked_for_as_one() {
    // Id 0 is not an id for the library; Discord treats 0 and 1 alike (the start of time).
    let server = RestServer::start(|_, _| Response::json(200, &json!([]))).await;
    let client = rest_client(&server.host);
    assert_eq!(history(&client, 0, 1).await.unwrap(), Vec::<u64>::new());
    assert_eq!(server.requests()[0].query("after"), Some("1"));
    assert_eq!(server.requests()[0].query("limit"), Some("1"));
}

#[tokio::test]
async fn api_errors_carry_their_status_and_code() {
    let server = RestServer::start(|_, n| match n {
        0 => Response::json(404, &json!({"message": "Unknown Channel", "code": 10003})),
        1 => Response::json(403, &json!({"message": "Missing Access", "code": 50001})),
        _ => Response::json(500, &json!({"message": "oops", "code": 0})),
    })
    .await;
    let client = rest_client(&server.host);
    assert_eq!(
        history(&client, 1, 100).await,
        Err(RestError::Api {
            status: 404,
            code: Some(10003)
        })
    );
    assert_eq!(
        history(&client, 1, 100).await,
        Err(RestError::Api {
            status: 403,
            code: Some(50001)
        })
    );
    assert_eq!(
        history(&client, 1, 100).await,
        Err(RestError::Api {
            status: 500,
            code: Some(0)
        })
    );
}

#[tokio::test]
async fn a_failure_with_no_json_body_is_a_transport_error() {
    // What a proxy in front of Discord sends: nothing the library can read.
    let server = RestServer::start(|_, _| Response {
        status: 502,
        headers: vec![],
        body: "<html>bad gateway</html>".into(),
    })
    .await;
    let client = rest_client(&server.host);
    assert_eq!(history(&client, 1, 100).await, Err(RestError::Transport));
}

#[tokio::test]
async fn a_rejected_token_is_unauthorized_and_nothing_more_is_sent() {
    let server = RestServer::start(|_, _| {
        Response::json(401, &json!({"message": "401: Unauthorized", "code": 0}))
    })
    .await;
    let client = rest_client(&server.host);
    assert_eq!(history(&client, 1, 100).await, Err(RestError::Unauthorized));
    // Discord asks apps to stop after a 401, and the library refuses every later request: the
    // server hears nothing more, however often the plugin asks.
    for _ in 0..3 {
        assert_eq!(history(&client, 1, 100).await, Err(RestError::Unauthorized));
    }
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn a_429_is_waited_out_and_the_request_sent_again() {
    let server = RestServer::start(|_, n| {
        if n == 0 {
            Response::json(
                429,
                &json!({"message": "You are being rate limited.", "retry_after": 0.2, "global": false}),
            )
            .header("retry-after", "1")
            .header("x-ratelimit-scope", "user")
            .header("x-ratelimit-bucket", "abcd")
            .header("x-ratelimit-limit", "5")
            .header("x-ratelimit-remaining", "0")
            .header("x-ratelimit-reset-after", "0.2")
        } else {
            Response::json(200, &json!([message(10, "a")]))
        }
    })
    .await;
    let client = rest_client(&server.host);
    let started = tokio::time::Instant::now();
    assert_eq!(history(&client, 1, 100).await.unwrap(), [10]);
    assert_eq!(server.requests().len(), 2, "the same request, sent again");
    assert!(
        started.elapsed() >= Duration::from_millis(150),
        "the limit's reset-after was waited: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_message_the_library_cannot_read_costs_only_itself() {
    let server = RestServer::start(|_, _| {
        Response::json(
            200,
            &json!([message(30, "c"), {"id": "20", "type": 0, "content": "no author"}, message(10, "a")]),
        )
    })
    .await;
    let client = rest_client(&server.host);
    assert_eq!(history(&client, 1, 100).await.unwrap(), [10, 30]);
    // The count and the highest id are of what Discord sent, the message that can't be read
    // included, so a full page is still seen as full.
    let page = client.messages_after(CHANNEL, 1, 100).await.unwrap();
    assert_eq!((page.received, page.last), (3, Some(30)));
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_transport_error() {
    let (_reserved, addr) = refused_address();
    let client = rest_client(&addr.to_string());
    assert_eq!(history(&client, 1, 100).await, Err(RestError::Transport));
}

#[tokio::test]
async fn a_message_has_the_time_its_json_says() {
    let id = giles_channel_discord::snowflake::at_unix_ms(1_792_843_200_123) + 5;
    let server =
        RestServer::start(move |_, _| Response::json(200, &json!([message(id, "x")]))).await;
    let client = rest_client(&server.host);
    let page = client.messages_after(CHANNEL, 1, 1).await.unwrap().messages;
    // The fixture writes the time its id encodes, to the millisecond, and the plugin reads it back.
    assert_eq!(page[0].timestamp_ms, 1_792_843_200_123);
    assert_eq!(page[0].author.id, 11_111_111_111_111_111);
    assert_eq!(page[0].content, "x");
}

// Sending: `create_message` and `create_dm`.

const BOT_USER: u64 = 82_198_898_841_029_460;

async fn post(client: &impl Discord, content: &str) -> Result<u64, RestError> {
    tokio::time::timeout(
        WAIT,
        client.create_message(
            CHANNEL,
            giles_channel_discord::client::NewMessage {
                content: content.to_owned(),
                files: Vec::new(),
            },
        ),
    )
    .await
    .expect("an answer in time")
}

fn body_of(request: &loopback::rest::Request) -> Value {
    serde_json::from_slice(&request.body).expect("a JSON body")
}

#[tokio::test]
async fn a_message_is_posted_with_no_mention_and_no_embed() {
    // Only an id in the response: nothing else of the message model is needed.
    let server =
        RestServer::start(|_, _| Response::json(200, &json!({"id": "1234567890123456789"}))).await;
    let client = rest_client(&server.host);
    assert_eq!(
        post(&client, "hello **world**").await,
        Ok(1_234_567_890_123_456_789)
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.target,
        format!("/api/v10/channels/{CHANNEL}/messages")
    );
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bot {TOKEN}").as_str())
    );
    assert_eq!(request.header("content-type"), Some("application/json"));
    // `parse: []` is "no mention pings", which is not the same as leaving the field out (Discord's
    // default parses users, roles, and everyone). `flags: 4` is SUPPRESS_EMBEDS.
    assert_eq!(
        body_of(request),
        json!({
            "content": "hello **world**",
            "allowed_mentions": {"parse": []},
            "flags": 4,
        })
    );
    // The token is the header's and nobody else's.
    assert!(!String::from_utf8_lossy(&request.body).contains(TOKEN));
}

#[tokio::test]
async fn a_dm_channel_is_created_for_a_user() {
    let server = RestServer::start(|_, _| {
        Response::json(
            200,
            &json!({"id": CHANNEL.to_string(), "type": 1, "recipients": []}),
        )
    })
    .await;
    let client = rest_client(&server.host);
    assert_eq!(client.create_dm(BOT_USER).await, Ok(CHANNEL));
    let request = &server.requests()[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/api/v10/users/@me/channels");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bot {TOKEN}").as_str())
    );
    // Snowflakes are strings on the wire.
    assert_eq!(
        body_of(request),
        json!({"recipient_id": BOT_USER.to_string()})
    );
}

#[tokio::test]
async fn an_id_that_cant_exist_is_not_sent() {
    let server = RestServer::start(|_, _| Response::json(200, &json!({"id": "1"}))).await;
    let client = rest_client(&server.host);
    assert_eq!(
        client
            .create_message(
                0,
                giles_channel_discord::client::NewMessage {
                    content: "x".into(),
                    files: Vec::new(),
                }
            )
            .await,
        Err(RestError::Api {
            status: 404,
            code: Some(10_003)
        })
    );
    assert_eq!(
        client.create_dm(0).await,
        Err(RestError::Api {
            status: 404,
            code: Some(10_013)
        })
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn sending_errors_keep_their_status_and_code() {
    let error = |status, code: u32, message: &str| {
        Response::json(status, &json!({"code": code, "message": message}))
    };
    let server = RestServer::start(move |_, n| match n {
        0 => error(403, 50_007, "Cannot send messages to this user"),
        1 => error(404, 10_013, "Unknown User"),
        2 => error(404, 10_003, "Unknown Channel"),
        3 => Response::json(
            400,
            &json!({"code": 50_035, "message": "Invalid Form Body", "errors": {
                "content": {"_errors": [{"code": "BASE_TYPE_MAX_LENGTH", "message": "x"}]}
            }}),
        ),
        4 => error(413, 40_005, "Request entity too large"),
        5 => error(500, 0, "Internal Server Error"),
        // Cloudflare or a proxy: not Discord's JSON.
        _ => Response {
            status: 502,
            headers: vec![],
            body: "<html>Bad gateway</html>".into(),
        },
    })
    .await;
    let client = rest_client(&server.host);
    let mut got = Vec::new();
    for text in ["a", "b", "c", "d", "e", "f", "g"] {
        got.push(post(&client, text).await.unwrap_err());
    }
    let api = |status, code| RestError::Api {
        status,
        code: Some(code),
    };
    assert_eq!(
        got,
        [
            api(403, 50_007),
            api(404, 10_013),
            api(404, 10_003),
            api(400, 50_035),
            RestError::TooLarge,
            api(500, 0),
            RestError::Transport,
        ]
    );
    // None of them locked the client: they are not 401s.
    assert_eq!(server.requests().len(), 7);
}

#[tokio::test]
async fn a_burst_waits_for_the_bucket_and_arrives_in_order() {
    // Every answer says the bucket is empty for 0.3 s: the client holds the next request back.
    let times = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&times);
    let server = RestServer::start(move |_, n| {
        seen.lock().unwrap().push(tokio::time::Instant::now());
        Response::json(200, &json!({"id": (n + 1).to_string()}))
            .header("x-ratelimit-scope", "user")
            .header("x-ratelimit-bucket", "abcd1234")
            .header("x-ratelimit-limit", "5")
            .header("x-ratelimit-remaining", "0")
            .header("x-ratelimit-reset-after", "0.3")
    })
    .await;
    let client = rest_client(&server.host);
    for (i, text) in ["one", "two", "three"].into_iter().enumerate() {
        assert_eq!(post(&client, text).await, Ok(i as u64 + 1));
    }
    let texts: Vec<_> = server
        .requests()
        .iter()
        .map(|request| body_of(request)["content"].clone())
        .collect();
    assert_eq!(texts, [json!("one"), json!("two"), json!("three")]);
    let times = times.lock().unwrap().clone();
    for pair in times.windows(2) {
        assert!(
            pair[1] - pair[0] >= Duration::from_millis(250),
            "{:?} between two sends of an empty bucket",
            pair[1] - pair[0]
        );
    }
}

#[tokio::test]
async fn after_a_401_a_send_asks_nothing_more_of_discord() {
    let server = RestServer::start(|_, _| {
        Response::json(401, &json!({"message": "401: Unauthorized", "code": 0}))
    })
    .await;
    let client = rest_client(&server.host);
    assert_eq!(post(&client, "a").await, Err(RestError::Unauthorized));
    // The library remembers: Discord asks that an invalid token isn't tried again.
    assert_eq!(post(&client, "b").await, Err(RestError::Unauthorized));
    assert_eq!(
        client.create_dm(BOT_USER).await,
        Err(RestError::Unauthorized)
    );
    assert_eq!(server.requests().len(), 1);
}
