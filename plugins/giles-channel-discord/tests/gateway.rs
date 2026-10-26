//! The twilight client (`RealDiscord`) against a Gateway server of the test's own, on loopback:
//! the handshake, the intents, heartbeats, resume, invalid sessions, close codes, and the clean
//! close at shutdown. Nothing here reaches Discord, and the token is not one.
//!
//! The server plays Discord from its documentation, so these tests show that the glue maps what
//! the library does onto the plugin's events, not that Discord behaves this way (issue #20).
//! `twilight-gateway` sleeps a second before every connect, so each test takes a few seconds.

mod loopback;

use std::time::Duration;

use giles_channel_discord::client::{Discord, GatewayEvent};
use loopback::{BOT, Frame, Gateway, TOKEN, WAIT, client, message_create, message_json, next};
use serde_json::json;

#[tokio::test]
async fn it_identifies_heartbeats_and_closes_with_1000() {
    let gateway = Gateway::bind().await;
    let client = client(&gateway.url);
    let server = tokio::spawn(async move {
        let mut connection = gateway.accept(100).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        // Heartbeats carry the sequence of the last dispatch, and each gets an ACK.
        let mut heartbeats = 0;
        let close = loop {
            match connection.recv().await {
                Frame::Json(payload) if payload["op"] == 1 => {
                    assert_eq!(payload["d"], 1, "{payload}");
                    heartbeats += 1;
                    connection.send(json!({"op": 11})).await;
                }
                Frame::Json(payload) => panic!("unexpected {payload}"),
                Frame::Close(code) => break code,
            }
        };
        // The server's WebSocket library answers the close, as Discord does.
        (heartbeats, close)
    });

    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Ready { user_id: BOT })
    );
    // Heartbeats only go out while the client is polled.
    assert!(
        tokio::time::timeout(Duration::from_millis(700), client.next_event())
            .await
            .is_err()
    );
    tokio::time::timeout(WAIT, client.shutdown())
        .await
        .expect("shutdown finishes once Discord answers the close");
    let (heartbeats, close) = server.await.unwrap();
    assert!(heartbeats >= 2, "{heartbeats} heartbeats");
    assert_eq!(close, 1000, "a clean close ends the session");
    // The stream is over: no polling after a close, and a second shutdown is a no-op.
    assert_eq!(next(&client).await, None);
    tokio::time::timeout(WAIT, client.shutdown()).await.unwrap();
}

#[tokio::test]
async fn a_rejected_token_closes_with_4004_and_ends_the_stream() {
    let gateway = Gateway::bind().await;
    let client = client(&gateway.url);
    let server = tokio::spawn(async move {
        let mut connection = gateway.accept(41_250).await;
        connection.expect_identify().await;
        connection.close(4004).await;
        // The client never connects again: this server would hear it.
        tokio::time::timeout(Duration::from_secs(3), gateway.listener.accept())
            .await
            .is_err()
    });

    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Closed { code: Some(4004) })
    );
    assert_eq!(next(&client).await, None);
    assert_eq!(next(&client).await, None);
    assert!(server.await.unwrap(), "the client connected again");
    // With no session, shutdown has nothing to close.
    tokio::time::timeout(WAIT, client.shutdown()).await.unwrap();
}

#[tokio::test]
async fn a_dropped_connection_is_resumed_with_the_same_session() {
    let gateway = Gateway::bind().await;
    let client = client(&gateway.url);
    let server = tokio::spawn(async move {
        let mut connection = gateway.accept(41_250).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        // Gone without a close frame.
        drop(connection);

        let mut connection = gateway.accept(41_250).await;
        let resume = connection.recv_json().await;
        connection
            .send(json!({"op": 0, "s": 2, "t": "RESUMED", "d": {}}))
            .await;
        resume
    });

    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Ready { user_id: BOT })
    );
    // A plain TCP close has no close frame to report a code for. The shard reconnects without
    // saying so, which the client has to notice (the plugin delays and reports from this).
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Closed { code: None })
    );
    assert_eq!(next(&client).await, Some(GatewayEvent::Resumed));
    let resume = server.await.unwrap();
    assert_eq!(resume["op"], 6, "{resume}");
    assert_eq!(resume["d"]["token"], format!("Bot {TOKEN}"));
    assert_eq!(resume["d"]["session_id"], "session-1");
    assert_eq!(resume["d"]["seq"], 1);
}

#[tokio::test]
async fn an_invalid_session_is_a_close_and_a_new_identify() {
    let gateway = Gateway::bind().await;
    let client = client(&gateway.url);
    let server = tokio::spawn(async move {
        let mut connection = gateway.accept(41_250).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        // Not resumable: the client closes, and Discord answers the close.
        connection.send(json!({"op": 9, "d": false})).await;
        let Frame::Close(code) = connection.recv().await else {
            panic!("the client didn't close");
        };

        // A new connection identifies again instead of resuming. (`twilight-gateway`'s
        // identify queue spaces Identifies five seconds apart.)
        let mut connection = gateway.accept(41_250).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        code
    });

    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Ready { user_id: BOT })
    );
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Closed { code: Some(1000) })
    );
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Ready { user_id: BOT })
    );
    assert_eq!(server.await.unwrap(), 1000);
}

#[tokio::test]
async fn a_reconnect_request_is_a_close_and_a_resume() {
    let gateway = Gateway::bind().await;
    let client = client(&gateway.url);
    let server = tokio::spawn(async move {
        let mut connection = gateway.accept(41_250).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        connection.send(json!({"op": 7, "d": null})).await;
        let Frame::Close(code) = connection.recv().await else {
            panic!("the client didn't close");
        };

        let mut connection = gateway.accept(41_250).await;
        let resume = connection.recv_json().await;
        connection
            .send(json!({"op": 0, "s": 2, "t": "RESUMED", "d": {}}))
            .await;
        (code, resume["op"].clone())
    });

    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Ready { user_id: BOT })
    );
    // Not 1000 or 1001, which would end the session: it is kept for the resume.
    let Some(GatewayEvent::Closed { code: Some(code) }) = next(&client).await else {
        panic!("no close");
    };
    assert!(code != 1000 && code != 1001, "{code}");
    assert_eq!(next(&client).await, Some(GatewayEvent::Resumed));
    let (sent, op) = server.await.unwrap();
    assert_eq!(sent, code);
    assert_eq!(op, 6);
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_close_without_a_code() {
    // Nothing listens: bind to learn a free port, then release it.
    let url = Gateway::bind().await.url;
    let client = client(&url);
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Closed { code: None })
    );
    // And it keeps trying when polled again, which the session paces.
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Closed { code: None })
    );
}

#[tokio::test]
async fn shutdown_does_not_reconnect_when_the_close_is_never_answered() {
    let gateway = Gateway::bind().await;
    let client = client(&gateway.url);
    let server = tokio::spawn(async move {
        let mut connection = gateway.accept(41_250).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        let code = connection.recv_close_unanswered().await;
        // Gone without answering: the shard sees a plain TCP close and would reconnect and
        // Identify, which would put the bot back online after it was shut down.
        drop(connection);
        let reconnected = tokio::time::timeout(Duration::from_secs(3), gateway.listener.accept())
            .await
            .is_ok();
        (code, reconnected)
    });

    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Ready { user_id: BOT })
    );
    let started = std::time::Instant::now();
    tokio::time::timeout(WAIT, client.shutdown())
        .await
        .expect("shutdown finishes without an answer");
    assert!(started.elapsed() < Duration::from_millis(1_500));
    let (code, reconnected) = server.await.unwrap();
    assert_eq!(code, 1000);
    assert!(!reconnected, "the client connected again after shutdown");
}

#[tokio::test]
async fn a_dm_dispatch_is_a_message_and_edits_deletes_and_broken_events_are_skipped() {
    let gateway = Gateway::bind().await;
    let client = client(&gateway.url);
    let user = 11_111_111_111_111_111;
    let channel = 319_674_150_115_610_528;
    let id = 1_234_567_890_123_456_789;
    let server = tokio::spawn(async move {
        let mut connection = gateway.accept(41_250).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        // Edits and deletes of a DM arrive with the DM intent, and the plugin never asks for
        // them to be parsed. (The shard skips events not in its wanted set.)
        connection
            .send(json!({"op": 0, "s": 2, "t": "MESSAGE_UPDATE", "d": message_json(id, channel, user, "edited")}))
            .await;
        connection
            .send(json!({"op": 0, "s": 3, "t": "MESSAGE_DELETE", "d": {"id": id.to_string(), "channel_id": channel.to_string()}}))
            .await;
        // A message the library's models can't read (no author): logged by type, skipped, and
        // the connection and the stream go on.
        connection
            .send(json!({"op": 0, "s": 4, "t": "MESSAGE_CREATE", "d": {"id": "1", "content": "no author"}}))
            .await;
        connection
            .send(message_create(5, message_json(id, channel, user, "hello")))
            .await;
        connection.recv_close_unanswered().await;
    });

    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Ready { user_id: BOT })
    );
    let Some(GatewayEvent::Message(message)) = next(&client).await else {
        panic!("not a message");
    };
    assert_eq!(
        (message.id, message.channel_id, message.author.id),
        (id, channel, user)
    );
    assert_eq!(message.content, "hello");
    assert!(!message.guild && !message.webhook && !message.author.bot);
    tokio::time::timeout(WAIT, client.shutdown()).await.unwrap();
    server.await.unwrap();
}
