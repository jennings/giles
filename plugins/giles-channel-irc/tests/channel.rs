//! The IRC channel against a scripted fake server on 127.0.0.1: registration, CAP and SASL,
//! identities, conversations, echo dropping, sending, recipient resolution, keepalive, and
//! reconnecting.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt as _;
use giles_channel_irc::{HOST_LEN, Irc, Timing};
use giles_plugin_sdk::giles_types::{
    Channel as _, ChannelEvent, ChannelStatus, ConversationId, ConversationKind, Error, MediaId,
    MediaRef, MessageId, NativeId, OutboundMessage, Result, SendTarget,
};
use serde_json::json;

use self::support::{
    CLIENT_CERT, CLIENT_KEY, Client, FakeServer, MODERN_CAPS, MODERN_REQ, WAIT, settings, timing,
};

fn to_conversation(id: &str, markdown: &str) -> OutboundMessage {
    OutboundMessage {
        target: SendTarget::Conversation(ConversationId::new(id).unwrap()),
        markdown: markdown.into(),
        attachments: vec![],
        reply_to: None,
    }
}

fn to_recipient(id: &str, markdown: &str) -> OutboundMessage {
    OutboundMessage {
        target: SendTarget::Recipient(NativeId::new(id).unwrap()),
        markdown: markdown.into(),
        attachments: vec![],
        reply_to: None,
    }
}

/// Sends `msg` in the background (the fake server must read the lines meanwhile).
fn send(client: &Client, msg: OutboundMessage) -> tokio::task::JoinHandle<Result<MessageId>> {
    let irc = Arc::clone(&client.irc);
    tokio::spawn(async move { irc.send(msg).await })
}

async fn result(task: tokio::task::JoinHandle<Result<MessageId>>) -> Result<MessageId> {
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap()
}

/// A registered client with modern capabilities, joined to nothing.
async fn connected(extra: serde_json::Value) -> (FakeServer, support::Peer, Client) {
    let server = FakeServer::plain().await;
    let mut client = Client::start(settings(server.port(), extra), timing()).await;
    let mut peer = server.accept().await;
    peer.register(MODERN_CAPS, Some(MODERN_REQ)).await;
    client.connected().await;
    (server, peer, client)
}

#[tokio::test]
async fn registers_with_capabilities_joins_and_reports_connected() {
    let server = FakeServer::plain().await;
    let mut client = Client::start(
        settings(server.port(), json!({ "channels": ["#giles", "#ops"] })),
        timing(),
    )
    .await;
    let mut peer = server.accept().await;
    // Only the capabilities the plugin uses are requested.
    peer.register(
        &format!("{MODERN_CAPS} sasl=PLAIN chghost"),
        Some(MODERN_REQ),
    )
    .await;
    peer.expect("JOIN #giles").await;
    peer.expect("JOIN #ops").await;
    client.connected().await;
    // Server pings are answered.
    peer.sync("abc").await;
}

#[tokio::test]
async fn a_server_without_capabilities_still_registers() {
    let server = FakeServer::plain().await;
    let mut client = Client::start(settings(server.port(), json!({})), timing()).await;
    let mut peer = server.accept().await;
    peer.expect_hello("giles").await;
    // An old server ignores `CAP LS` and welcomes the client.
    peer.welcome("giles").await;
    client.connected().await;
    // Without account-tag, an account tag can't be trusted.
    peer.send("@account=stephen :stephen!sj@host PRIVMSG giles :hi")
        .await;
    let (_, msg) = client.message().await;
    assert_eq!(msg.sender.as_str(), "mask:stephen!sj@host");
    assert!(!msg.sender_authenticated);
}

#[tokio::test]
async fn sasl_plain_logs_in_before_registration() {
    let server = FakeServer::plain().await;
    let mut client = Client::start(
        settings(server.port(), json!({ "sasl": { "password": "hunter22" } })),
        timing(),
    )
    .await;
    let mut peer = server.accept().await;
    peer.expect_hello("giles").await;
    peer.send(":irc.test CAP * LS * :account-tag sasl=PLAIN,EXTERNAL")
        .await;
    peer.send(":irc.test CAP * LS :server-time").await;
    peer.expect("CAP REQ :account-tag server-time sasl").await;
    peer.send(":irc.test CAP * ACK :account-tag server-time sasl")
        .await;
    peer.expect("AUTHENTICATE PLAIN").await;
    peer.send("AUTHENTICATE +").await;
    let line = peer.recv().await;
    let payload = line.strip_prefix("AUTHENTICATE ").unwrap();
    assert_eq!(BASE64.decode(payload).unwrap(), b"giles\0giles\0hunter22");
    peer.send(":irc.test 900 giles giles!giles@host giles :You are now logged in as giles")
        .await;
    peer.send(":irc.test 903 giles :SASL authentication successful")
        .await;
    peer.expect("CAP END").await;
    peer.welcome("giles").await;
    client.connected().await;
}

#[tokio::test]
async fn a_sasl_failure_is_reported_and_retried() {
    let server = FakeServer::plain().await;
    let mut client = Client::start(
        settings(
            server.port(),
            json!({ "sasl": { "password": "wrong-password" } }),
        ),
        timing(),
    )
    .await;
    let mut peer = server.accept().await;
    peer.expect_hello("giles").await;
    peer.send(":irc.test CAP * LS :sasl").await;
    peer.expect("CAP REQ :sasl").await;
    peer.send(":irc.test CAP * ACK :sasl").await;
    peer.expect("AUTHENTICATE PLAIN").await;
    peer.send("AUTHENTICATE +").await;
    peer.recv().await;
    peer.send(":irc.test 904 giles :SASL authentication failed")
        .await;
    let error = client.error().await;
    assert_eq!(
        error,
        "SASL authentication failed: SASL authentication failed"
    );
    assert!(!error.contains("wrong-password"));
    // The connection is closed and tried again.
    assert_eq!(peer.try_recv().await, None);
    server.accept().await.expect("CAP LS 302").await;
}

#[tokio::test]
async fn configured_sasl_is_never_skipped() {
    let server = FakeServer::plain().await;
    let sasl = json!({ "sasl": { "password": "hunter22" } });
    let mut client = Client::start(settings(server.port(), sasl.clone()), timing()).await;

    // Not offered: the client gives up on this connection rather than register without it.
    let mut peer = server.accept().await;
    peer.expect_hello("giles").await;
    peer.send(":irc.test CAP * LS :account-tag").await;
    assert_eq!(client.error().await, "the server doesn't offer SASL");
    assert_eq!(peer.try_recv().await, None);

    // The mechanism isn't offered.
    let mut peer = server.accept().await;
    peer.expect_hello("giles").await;
    peer.send(":irc.test CAP * LS :sasl=EXTERNAL").await;
    assert_eq!(
        client.error().await,
        "the server doesn't offer SASL PLAIN (only EXTERNAL)"
    );
    assert_eq!(peer.try_recv().await, None);

    // A server that ignores CAP and welcomes the client anyway.
    let mut peer = server.accept().await;
    peer.expect_hello("giles").await;
    peer.welcome("giles").await;
    assert_eq!(
        client.error().await,
        "registered without SASL authentication"
    );
}

#[tokio::test]
async fn account_tags_identify_senders() {
    let (_server, mut peer, mut client) = connected(json!({})).await;
    peer.send(
        "@account=Stephen;msgid=m1;time=2026-10-23T12:00:00.000Z \
         :Stephen!~sj@user/stephen PRIVMSG giles :hello there",
    )
    .await;
    let (id, msg) = client.message().await;
    assert_eq!(id.as_str(), "msgid:m1");
    assert_eq!(msg.sender.as_str(), "account:stephen");
    assert!(msg.sender_authenticated);
    assert!(msg.sender_aliases.is_empty());
    assert_eq!(msg.text, "hello there");
    assert_eq!(msg.conversation.kind, ConversationKind::Direct);
    assert_eq!(msg.conversation.id.as_str(), "stephen");
    assert_eq!(msg.sent_at, Some("2026-10-23T12:00:00Z".parse().unwrap()));
    assert_eq!(msg.message_id.as_ref().map(MessageId::as_str), Some("m1"));

    // No account: reported, but unverified (the core drops it).
    peer.send(":mallory!m@evil.example PRIVMSG giles :I am stephen")
        .await;
    let (id, msg) = client.message().await;
    assert!(id.as_str().starts_with("sha256:"), "{id}");
    assert_eq!(msg.sender.as_str(), "mask:mallory!m@evil.example");
    assert!(!msg.sender_authenticated);
}

#[tokio::test]
async fn mask_identities_are_verified_only_when_opted_in() {
    let (_server, mut peer, mut client) = connected(json!({ "mask_identities": true })).await;
    peer.send(":Stephen!~sj@Home.Example PRIVMSG giles :hi")
        .await;
    let (_, msg) = client.message().await;
    assert_eq!(msg.sender.as_str(), "mask:stephen!~sj@home.example");
    assert!(msg.sender_authenticated);
    peer.send("@account=sj :Stephen!~sj@home.example PRIVMSG giles :hi")
        .await;
    let (_, msg) = client.message().await;
    assert_eq!(msg.sender.as_str(), "account:sj");
    assert_eq!(
        msg.sender_aliases,
        [NativeId::new("mask:stephen!~sj@home.example").unwrap()]
    );
}

#[tokio::test]
async fn queries_are_direct_and_channel_messages_are_group() {
    let (_server, mut peer, mut client) = connected(json!({ "channels": ["#Giles"] })).await;
    peer.expect("JOIN #Giles").await;
    peer.send("@account=stephen :stephen!sj@host PRIVMSG #Giles :hi all")
        .await;
    let (_, msg) = client.message().await;
    assert_eq!(msg.conversation.kind, ConversationKind::Group);
    assert_eq!(msg.conversation.id.as_str(), "#giles");
    assert_eq!(msg.conversation.title.as_deref(), Some("#Giles"));
    peer.send("@account=stephen :Stephen!sj@host PRIVMSG Giles :just you")
        .await;
    let (_, msg) = client.message().await;
    assert_eq!(msg.conversation.kind, ConversationKind::Direct);
    assert_eq!(msg.conversation.id.as_str(), "stephen");
}

#[tokio::test]
async fn echoes_and_other_noise_are_dropped() {
    let (_server, mut peer, mut client) = connected(json!({})).await;
    // echo-message: our own reply comes back.
    peer.send("@msgid=e1 :giles!giles@host PRIVMSG stephen :my reply")
        .await;
    peer.send(":stephen!sj@host NOTICE giles :a notice").await;
    peer.send(":stephen!sj@host PRIVMSG giles :\x01VERSION\x01")
        .await;
    peer.send(":irc.test PRIVMSG giles :from the server").await;
    peer.send("this is not irc").await;
    peer.send(":stephen!sj@host PRIVMSG giles :\x01ACTION waves\x01")
        .await;
    let (_, msg) = client.message().await;
    assert_eq!(msg.text, "/me waves");
}

#[tokio::test]
async fn long_messages_are_split_to_fit_and_rate_limited() {
    let (_server, mut peer, client) =
        connected(json!({ "send_burst": 2, "send_interval_ms": 150 })).await;
    let words: Vec<String> = (0..400).map(|n| format!("w{n}")).collect();
    let text = words.join(" ");
    let task = send(&client, to_conversation("stephen", &format!("**{text}**")));
    let mut lines = Vec::new();
    let mut times = Vec::new();
    while lines
        .iter()
        .map(|l: &String| l.split(' ').count())
        .sum::<usize>()
        < 400 + 2 * lines.len()
    {
        let line = peer.recv().await;
        times.push(Instant::now());
        lines.push(line);
    }
    let id = result(task).await.unwrap();
    assert!(id.as_str().starts_with("sent:"));
    let mut received = Vec::new();
    for line in &lines {
        let text = line.strip_prefix("PRIVMSG stephen :").unwrap();
        // Bold reopened on every line.
        assert!(text.starts_with('\x02'), "{text:?}");
        // As relayed, with the longest host, each line fits in 512 bytes.
        let relayed = format!(":giles!~giles@{} {line}\r\n", "h".repeat(HOST_LEN));
        assert!(relayed.len() <= 512, "{}", relayed.len());
        received.extend(text.replace('\x02', "").split(' ').map(str::to_owned));
    }
    assert_eq!(received, words);
    assert!(lines.len() >= 4, "{}", lines.len());
    // Two at once, then one per interval.
    assert!(times[2] - times[0] >= Duration::from_millis(120));
    assert!(times[3] - times[2] >= Duration::from_millis(120));
}

#[tokio::test]
async fn max_lines_and_max_line_bytes_limit_a_message() {
    let (_server, mut peer, client) =
        connected(json!({ "max_lines": 3, "max_line_bytes": 20 })).await;
    let task = send(
        &client,
        to_conversation("#giles", &"0123456789 ".repeat(20)),
    );
    for _ in 0..2 {
        let line = peer.recv().await;
        let text = line.strip_prefix("PRIVMSG #giles :").unwrap();
        assert!(text.len() <= 20, "{text:?}");
    }
    assert_eq!(
        peer.recv().await,
        "PRIVMSG #giles :[18 more lines not sent]"
    );
    result(task).await.unwrap();
}

#[tokio::test]
async fn attachments_are_named_not_sent() {
    let (_server, mut peer, client) = connected(json!({})).await;
    let mut msg = to_conversation("stephen", "see attached");
    msg.attachments.push(MediaRef {
        id: MediaId::from_digest([7; 32]),
        media_type: "image/png".into(),
        size: 10,
        name: Some("cat.png".into()),
    });
    let task = send(&client, msg);
    peer.expect("PRIVMSG stephen :see attached").await;
    peer.expect("PRIVMSG stephen :[attachment not sent: cat.png]")
        .await;
    result(task).await.unwrap();
}

#[tokio::test]
async fn an_account_recipient_is_resolved_with_whois() {
    let (_server, mut peer, client) = connected(json!({})).await;
    let task = send(
        &client,
        to_recipient("account:Stephen", "your job finished"),
    );
    peer.expect("WHOIS Stephen").await;
    peer.send(":irc.test 311 giles Stephen ~sj user/stephen * :Stephen J")
        .await;
    peer.send(":irc.test 330 giles Stephen stephen :is logged in as")
        .await;
    peer.send(":irc.test 318 giles Stephen :End of /WHOIS list.")
        .await;
    peer.expect("PRIVMSG Stephen :your job finished").await;
    result(task).await.unwrap();
}

#[tokio::test]
async fn a_recipient_lookup_waits_for_queued_lines() {
    let (_server, mut peer, client) =
        connected(json!({ "send_burst": 1, "send_interval_ms": 200 })).await;
    let reply = send(&client, to_conversation("#giles", "one\n\ntwo\n\nthree"));
    peer.expect("PRIVMSG #giles :one").await;
    // `two` and `three` are queued: the WHOIS comes after them, so the confirmed nick's message
    // isn't stuck behind them.
    let job = send(&client, to_recipient("account:stephen", "done"));
    peer.expect("PRIVMSG #giles :two").await;
    peer.expect("PRIVMSG #giles :three").await;
    result(reply).await.unwrap();
    let asked = Instant::now();
    peer.expect("WHOIS stephen").await;
    assert!(asked.elapsed() >= Duration::from_millis(120));
    peer.send(":irc.test 311 giles stephen ~sj host * :S").await;
    peer.send(":irc.test 330 giles stephen stephen :is logged in as")
        .await;
    peer.send(":irc.test 318 giles stephen :End of /WHOIS list.")
        .await;
    peer.expect("PRIVMSG stephen :done").await;
    result(job).await.unwrap();
}

#[tokio::test]
async fn the_nick_last_seen_on_the_account_is_tried_first() {
    let (_server, mut peer, mut client) = connected(json!({})).await;
    peer.send("@account=stephen :sj_away!~sj@host PRIVMSG giles :brb")
        .await;
    client.message().await;
    // A nick change is followed.
    peer.send(":sj_away!~sj@host NICK sj_phone").await;
    peer.sync("1").await;
    let task = send(&client, to_recipient("account:stephen", "ping"));
    peer.expect("WHOIS sj_phone").await;
    peer.send(":irc.test 311 giles sj_phone ~sj host * :Stephen")
        .await;
    peer.send(":irc.test 330 giles sj_phone stephen :is logged in as")
        .await;
    peer.send(":irc.test 318 giles sj_phone :End of /WHOIS list.")
        .await;
    peer.expect("PRIVMSG sj_phone :ping").await;
    result(task).await.unwrap();
}

#[tokio::test]
async fn a_stale_nick_is_never_messaged() {
    let (_server, mut peer, mut client) = connected(json!({})).await;
    peer.send("@account=stephen :sj!~sj@host PRIVMSG giles :hi")
        .await;
    client.message().await;
    let task = send(&client, to_recipient("account:stephen", "secret code 1234"));
    // `sj` now belongs to someone else.
    peer.expect("WHOIS sj").await;
    peer.send(":irc.test 311 giles sj ~x other.host * :Someone Else")
        .await;
    peer.send(":irc.test 330 giles sj mallory :is logged in as")
        .await;
    peer.send(":irc.test 318 giles sj :End of /WHOIS list.")
        .await;
    // Then the account's own name: not online.
    peer.expect("WHOIS stephen").await;
    peer.send(":irc.test 401 giles stephen :No such nick/channel")
        .await;
    peer.send(":irc.test 318 giles stephen :End of /WHOIS list.")
        .await;
    let error = result(task).await.unwrap_err();
    assert!(
        matches!(&error, Error::Unavailable { message, .. } if message == "account stephen is not online"),
        "{error}"
    );
    // Nothing was sent to anyone.
    peer.sync("2").await;

    // A nick that's online but not logged in at all.
    let task = send(&client, to_recipient("account:stephen", "x"));
    peer.expect("WHOIS stephen").await;
    peer.send(":irc.test 311 giles stephen ~s host * :S").await;
    peer.send(":irc.test 318 giles stephen :End of /WHOIS list.")
        .await;
    assert!(matches!(result(task).await, Err(Error::Unavailable { .. })));
    peer.sync("3").await;
}

#[tokio::test]
async fn mask_recipients_must_still_match() {
    let (_server, mut peer, client) = connected(json!({ "mask_identities": true })).await;
    let task = send(&client, to_recipient("mask:sj!~sj@home.example", "hello"));
    peer.expect("WHOIS sj").await;
    peer.send(":irc.test 311 giles SJ ~sj Home.Example * :S")
        .await;
    peer.send(":irc.test 318 giles SJ :End of /WHOIS list.")
        .await;
    peer.expect("PRIVMSG SJ :hello").await;
    result(task).await.unwrap();

    let task = send(&client, to_recipient("mask:sj!~sj@home.example", "hello"));
    peer.expect("WHOIS sj").await;
    peer.send(":irc.test 311 giles sj ~sj elsewhere.example * :S")
        .await;
    peer.send(":irc.test 318 giles sj :End of /WHOIS list.")
        .await;
    assert!(matches!(result(task).await, Err(Error::Unavailable { .. })));
}

#[tokio::test]
async fn unanswered_whois_is_unavailable() {
    let server = FakeServer::plain().await;
    let timing = Timing {
        lookup_timeout: Duration::from_millis(200),
        ..timing()
    };
    let mut client = Client::start(settings(server.port(), json!({})), timing).await;
    let mut peer = server.accept().await;
    peer.register(MODERN_CAPS, Some(MODERN_REQ)).await;
    client.connected().await;
    let task = send(&client, to_recipient("account:stephen", "x"));
    peer.expect("WHOIS stephen").await;
    assert!(matches!(result(task).await, Err(Error::Unavailable { .. })));
}

#[tokio::test]
async fn bad_recipients_are_refused() {
    let (_server, _peer, client) = connected(json!({})).await;
    for (recipient, why) in [
        ("nick:stephen", "not an IRC identity"),
        ("mask:sj!~sj@host", "mask identities are off"),
    ] {
        let error = client
            .irc
            .send(to_recipient(recipient, "x"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::InvalidRequest(message) if message.contains(why)),
            "{error}"
        );
    }
    let error = client
        .irc
        .send(to_conversation("two words", "x"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error}");
}

#[tokio::test]
async fn sends_while_disconnected_are_unavailable() {
    // Nothing listens on the port once the listener is gone.
    let port = FakeServer::plain().await.port();
    let timing = Timing {
        send_wait: Duration::from_millis(200),
        ..timing()
    };
    let mut client = Client::start(settings(port, json!({})), timing).await;
    assert!(
        client
            .error()
            .await
            .starts_with(&format!("connecting to 127.0.0.1:{port}: "))
    );
    let error = client
        .irc
        .send(to_recipient("account:stephen", "x"))
        .await
        .unwrap_err();
    assert!(error.is_retryable(), "{error}");
    let started = Instant::now();
    let error = client
        .irc
        .send(to_conversation("stephen", "x"))
        .await
        .unwrap_err();
    assert!(error.is_retryable(), "{error}");
    assert!(started.elapsed() >= Duration::from_millis(150));
}

#[tokio::test]
async fn a_reply_waits_for_the_connection_to_come_back() {
    let server = FakeServer::plain().await;
    let mut client = Client::start(settings(server.port(), json!({})), timing()).await;
    let task = send(&client, to_conversation("stephen", "late reply"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut peer = server.accept().await;
    peer.register(MODERN_CAPS, Some(MODERN_REQ)).await;
    client.connected().await;
    peer.expect("PRIVMSG stephen :late reply").await;
    result(task).await.unwrap();
}

#[tokio::test]
async fn reconnects_after_the_connection_drops() {
    let (server, mut peer, mut client) = connected(json!({ "channels": ["#giles"] })).await;
    peer.expect("JOIN #giles").await;
    peer.send("ERROR :Closing Link: (Excess Flood)").await;
    drop(peer);
    assert_eq!(
        client.error().await,
        "the server closed the connection: Closing Link: (Excess Flood)"
    );
    let mut peer = server.accept().await;
    peer.register(MODERN_CAPS, Some(MODERN_REQ)).await;
    peer.expect("JOIN #giles").await;
    client.connected().await;

    // A plain close.
    drop(peer);
    assert_eq!(client.error().await, "the server closed the connection");
    let mut peer = server.accept().await;
    peer.register(MODERN_CAPS, Some(MODERN_REQ)).await;
    client.connected().await;
}

#[tokio::test]
async fn a_silent_server_is_pinged_then_given_up() {
    let server = FakeServer::plain().await;
    let timing = Timing {
        ping_after: Duration::from_millis(300),
        ping_timeout: Duration::from_millis(300),
        ..timing()
    };
    let mut client = Client::start(settings(server.port(), json!({})), timing).await;
    let mut peer = server.accept().await;
    peer.register(MODERN_CAPS, Some(MODERN_REQ)).await;
    client.connected().await;
    peer.expect("PING :giles").await;
    // Any line resets the clock; then silence.
    peer.send(":irc.test PONG irc.test :giles").await;
    peer.expect("PING :giles").await;
    assert!(
        client
            .error()
            .await
            .starts_with("no reply from the server for ")
    );
    assert_eq!(peer.try_recv().await, None);
    server.accept().await.expect("CAP LS 302").await;
}

#[tokio::test]
async fn a_taken_nick_gets_an_underscore() {
    let server = FakeServer::plain().await;
    let mut client = Client::start(settings(server.port(), json!({})), timing()).await;
    let mut peer = server.accept().await;
    peer.expect_hello("giles").await;
    peer.send(":irc.test CAP * LS :account-tag").await;
    peer.expect("CAP REQ :account-tag").await;
    peer.send(":irc.test CAP * ACK :account-tag").await;
    peer.expect("CAP END").await;
    peer.send(":irc.test 433 * giles :Nickname is already in use")
        .await;
    peer.expect("NICK giles_").await;
    peer.welcome("giles_").await;
    client.connected().await;
    // Queries to the new nick are direct; our own messages under it are echoes.
    peer.send(":giles_!giles@host PRIVMSG stephen :echo").await;
    peer.send(":stephen!sj@host PRIVMSG giles_ :hi").await;
    let (_, msg) = client.message().await;
    assert_eq!(msg.text, "hi");
    assert_eq!(msg.conversation.kind, ConversationKind::Direct);
}

#[tokio::test]
async fn dropping_the_channel_quits() {
    let (_server, mut peer, client) = connected(json!({})).await;
    drop(client);
    peer.expect("QUIT :Giles is shutting down").await;
}

#[tokio::test]
async fn tls_with_a_client_certificate_and_sasl_external() {
    let server = FakeServer::tls(true).await;
    let settings = settings(
        server.port(),
        json!({
            "tls": true,
            "client_cert": CLIENT_CERT,
            "client_key": CLIENT_KEY,
            "sasl": { "mechanism": "external" },
        }),
    );
    let mut client = Client::start(settings, timing()).await;
    let mut peer = server.accept().await;
    assert_eq!(peer.client_certs, 1);
    peer.expect_hello("giles").await;
    peer.send(":irc.test CAP * LS :sasl=PLAIN,EXTERNAL").await;
    peer.expect("CAP REQ :sasl").await;
    peer.send(":irc.test CAP * ACK :sasl").await;
    peer.expect("AUTHENTICATE EXTERNAL").await;
    peer.send("AUTHENTICATE +").await;
    peer.expect("AUTHENTICATE +").await;
    peer.send(":irc.test 903 giles :SASL authentication successful")
        .await;
    peer.expect("CAP END").await;
    peer.welcome("giles").await;
    client.connected().await;
}

#[tokio::test]
async fn tls_refuses_a_server_the_bundled_roots_dont_trust() {
    let server = FakeServer::tls(false).await;
    let settings = settings(server.port(), json!({ "tls": true }));
    let tls = settings
        .tls_config(giles_plugin_sdk::tls::root_store())
        .unwrap();
    let irc = Irc::start(settings, tls, timing());
    let mut events = irc.events().await.unwrap();
    server.accept_failing().await;
    let event = tokio::time::timeout(WAIT, events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ChannelEvent::Status {
        status: ChannelStatus::Error { message },
        ..
    } = event
    else {
        panic!("{event:?}");
    };
    assert!(message.starts_with("TLS with 127.0.0.1:"), "{message}");
}
