//! The twilight client (`RealDiscord`) against a REST server of the test's own, on loopback: what
//! it asks for, what it makes of the answers, and of the errors. Nothing here reaches Discord, and
//! the token is not one. The server plays Discord from its documentation (issue #20 checks it).

mod loopback;

use std::time::Duration;

use giles_channel_discord::client::{Discord, RestError};
use loopback::rest::{Response, RestServer};
use loopback::{TOKEN, WAIT, rest_client};
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
    // Bind a port and drop the listener: nothing answers there.
    let host = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().to_string()
    };
    let client = rest_client(&host);
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
