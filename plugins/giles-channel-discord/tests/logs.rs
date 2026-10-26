//! What the plugin logs: with a fake Discord client behind the real plugin SDK, and with the real
//! client against a loopback Gateway. A test binary of its own: `tracing` caches whether a call
//! site is enabled, which makes a subscriber installed for one test unreliable next to tests that
//! run without one.

mod loopback;
mod support;

use std::io;
use std::sync::{Arc, Mutex};

use giles_channel_discord::client::Discord;
use giles_channel_discord::client::GatewayEvent;
use giles_channel_discord::fake::direct_message;
use loopback::{BOT, Gateway, client, next};
use serde_json::json;
use support::{Served, TOKEN, next_status};
use tracing_subscriber::fmt::MakeWriter;

/// Collects what `tracing` writes.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

/// Logs everything on this thread into the returned capture, until the guard is dropped: the
/// `tracing` events of the plugin and of twilight, and the `log` records of `hyper` and `reqwest`
/// (which the plugin's `main` also turns into the same output).
fn capture_logs() -> (Capture, tracing::subscriber::DefaultGuard) {
    // Once for the process; the records go to the subscriber of the thread that logs them.
    let _ = tracing_log::LogTracer::init();
    tracing_log::log::set_max_level(tracing_log::log::LevelFilter::Trace);
    let capture = Capture::default();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish(),
    );
    (capture, guard)
}

#[tokio::test]
async fn the_token_is_in_no_log_line_at_any_level() {
    let (capture, _logging) = capture_logs();
    let served = Served::start().await;
    // A rejected configuration logs too.
    served
        .configure(serde_json::json!({"token": format!("Bot {TOKEN}")}))
        .await
        .unwrap_err();
    served
        .configure(serde_json::json!({"token": TOKEN}))
        .await
        .unwrap();
    let (_acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    next_status(&mut events).await;
    served
        .client
        .push(GatewayEvent::Closed { code: Some(4004) });
    next_status(&mut events).await;
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("Discord channel configured"), "{logs}");
    assert!(logs.contains("connected to Discord"), "{logs}");
    assert!(!logs.contains("not-a-real-token"), "{logs}");
}

#[tokio::test]
async fn the_real_client_never_logs_the_token() {
    let (capture, _logging) = capture_logs();
    let gateway = Gateway::bind().await;
    let client = client(&gateway.url);
    let server = tokio::spawn(async move {
        // Identify, then a connection that drops, a resume, and a rejected token.
        let mut connection = gateway.accept(100).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        drop(connection);
        let mut connection = gateway.accept(100).await;
        let resume = connection.recv_json().await;
        assert_eq!(resume["op"], 6, "{resume}");
        connection
            .send(json!({"op": 0, "s": 2, "t": "RESUMED", "d": {}}))
            .await;
        connection.close(4004).await;
    });
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Ready { user_id: BOT })
    );
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Closed { code: None })
    );
    assert_eq!(next(&client).await, Some(GatewayEvent::Resumed));
    assert_eq!(
        next(&client).await,
        Some(GatewayEvent::Closed { code: Some(4004) })
    );
    assert_eq!(next(&client).await, None);
    client.shutdown().await;
    server.await.unwrap();

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    // The capture saw the library: it logs sending the Identify and the Resume at debug.
    assert!(logs.contains("sending identify"), "{logs}");
    assert!(!logs.contains("not-a-real-token"), "{logs}");
    assert!(!logs.contains(loopback::TOKEN), "{logs}");
}

/// A message id created now, so catch-up's age limit has nothing to say about it.
fn now_id(n: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    giles_channel_discord::snowflake::at_unix_ms(u64::try_from(now).unwrap()) + n
}

/// Text that would show if anything logged a message, a name, or a file name.
const MARKER: &str = "xyzzy-private-words";

#[tokio::test]
async fn no_message_text_or_name_is_in_any_log_line_through_the_plugin() {
    let (capture, _logging) = capture_logs();
    let served = Served::start().await;
    served
        .configure(serde_json::json!({"token": TOKEN}))
        .await
        .unwrap();
    let (_acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    next_status(&mut events).await;

    let channel = 319_674_150_115_610_528;
    let marked = |id: u64, content: &str| {
        let mut message = direct_message(id, channel, 1234, content);
        message.author.username = format!("{MARKER}-name");
        message.author.global_name = Some(format!("{MARKER}-display"));
        message
            .attachments
            .push(giles_channel_discord::client::Attachment {
                url: format!(
                    "https://cdn.discordapp.com/x/{MARKER}.png?ex=1&is=2&hm=secret-signature"
                ),
                filename: format!("{MARKER}.png"),
                content_type: None,
                size: 1,
            });
        message.stickers.push(format!("{MARKER}-sticker"));
        message
    };
    // A history that fails, then succeeds, with a message in it; a live message; a bot's; an
    // empty one; and one from a server.
    served
        .client
        .fail_history(giles_channel_discord::client::RestError::Transport);
    served
        .client
        .add_history(marked(now_id(1), &format!("{MARKER} from history")));
    served.client.push(GatewayEvent::Message(marked(
        now_id(2),
        &format!("{MARKER} live"),
    )));
    let mut bot = marked(now_id(3), &format!("{MARKER} bot"));
    bot.author.bot = true;
    served.client.push(GatewayEvent::Message(bot));
    served.client.push(GatewayEvent::Message(direct_message(
        now_id(4),
        channel,
        1234,
        "",
    )));
    let mut guild = marked(now_id(5), &format!("{MARKER} guild"));
    guild.guild = true;
    served.client.push(GatewayEvent::Message(guild));
    for _ in 0..2 {
        loop {
            if let giles_plugin_sdk::giles_types::ChannelEvent::Message { .. } =
                support::next_event(&mut events).await
            {
                break;
            }
        }
    }

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("catch-up"), "the pass was logged: {logs}");
    assert!(!logs.contains(MARKER), "{logs}");
    assert!(!logs.contains("secret-signature"), "{logs}");
    assert!(!logs.contains(TOKEN), "{logs}");
}

#[tokio::test]
async fn no_message_text_is_in_any_log_line_through_the_real_client() {
    let (capture, _logging) = capture_logs();
    let gateway = Gateway::bind().await;
    let channel = 319_674_150_115_610_528;
    let rest = loopback::rest::RestServer::start(move |_, n| {
        if n == 0 {
            loopback::rest::Response::json(
                200,
                &json!([
                    loopback::message_json(1_000_000_000_000_000_001, channel, 1234, MARKER),
                    {"id": "1000000000000000000", "content": MARKER},
                ]),
            )
        } else {
            loopback::rest::Response {
                status: 502,
                headers: vec![],
                body: format!("<html>{MARKER}</html>"),
            }
        }
    })
    .await;
    let client = loopback::rest_client(&rest.host);
    let gateway_client = loopback::client(&gateway.url);
    let server = tokio::spawn(async move {
        let mut connection = gateway.accept(41_250).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        // One that reads, and one that doesn't (no author) with the marker in its body.
        connection
            .send(loopback::message_create(
                2,
                loopback::message_json(1_000_000_000_000_000_002, channel, 1234, MARKER),
            ))
            .await;
        connection
            .send(loopback::message_create(
                3,
                json!({"id": "1000000000000000003", "content": MARKER}),
            ))
            .await;
        connection
            .send(loopback::message_create(
                4,
                loopback::message_json(1_000_000_000_000_000_004, channel, 1234, "last"),
            ))
            .await;
        let _ = connection.recv_close_unanswered().await;
    });
    assert!(matches!(
        next(&gateway_client).await,
        Some(GatewayEvent::Ready { .. })
    ));
    for _ in 0..2 {
        assert!(matches!(
            next(&gateway_client).await,
            Some(GatewayEvent::Message(_))
        ));
    }
    assert_eq!(
        client
            .messages_after(channel, 1, 100)
            .await
            .unwrap()
            .messages
            .len(),
        1
    );
    assert!(client.messages_after(channel, 1, 100).await.is_err());
    gateway_client.shutdown().await;
    server.await.unwrap();

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("couldn't be deserialized"), "{logs}");
    assert!(!logs.contains(MARKER), "{logs}");
}

/// The text of a message and the token must stay out of the logs when a send goes wrong: the
/// plugin's own and the REST library's, at every level.
#[tokio::test]
async fn a_send_never_logs_its_text_or_the_token() {
    use giles_channel_discord::channel::{DiscordChannel, Inbound};
    use giles_channel_discord::client::{NewMessage, RestError};
    use giles_channel_discord::fake::{FakeDiscord, FakeMedia};
    use giles_channel_discord::ingest::Clock;
    use giles_channel_discord::session::Timing;
    use giles_channel_discord::state::StateFile;
    use giles_plugin_sdk::giles_types::{
        Channel as _, ConversationId, OutboundMessage, SendTarget,
    };
    use loopback::rest::{Response, RestServer};

    const TEXT: &str = "the-secret-words-of-the-reply";
    let message = || OutboundMessage {
        target: SendTarget::Conversation(ConversationId::new("319674150115610528").unwrap()),
        markdown: TEXT.to_owned(),
        attachments: Vec::new(),
        reply_to: None,
    };

    let (capture, _logging) = capture_logs();
    // Through the plugin, with the failures it handles: a refusal, no answer, a rejected token.
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    let channel = DiscordChannel::start(
        client.clone(),
        Timing::default(),
        Inbound {
            state: StateFile::open(dir.path()),
            catch_up_max_age: std::time::Duration::from_secs(3600),
            clock: Clock::system(),
            media: Arc::new(FakeMedia::default()),
            max_blob: 50 * 1024 * 1024,
        },
    );
    client.script_messages([
        Err(RestError::Api {
            status: 403,
            code: Some(50_007),
        }),
        Err(RestError::Transport),
        Ok(1),
        Err(RestError::Unauthorized),
    ]);
    for _ in 0..4 {
        let _ = channel.send(message()).await;
    }
    // Through the real client, against a server that refuses and then rejects.
    let server = RestServer::start(|_, n| {
        if n == 0 {
            Response::json(
                403,
                &json!({"code": 50_007, "message": "Cannot send messages to this user"}),
            )
        } else {
            Response::json(401, &json!({"code": 0, "message": "401: Unauthorized"}))
        }
    })
    .await;
    let real = loopback::rest_client(&server.host);
    for _ in 0..2 {
        let _ = real
            .create_message(
                1,
                NewMessage {
                    content: TEXT.to_owned(),
                    files: Vec::new(),
                },
            )
            .await;
    }

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("sending to Discord failed"), "{logs}");
    assert!(!logs.contains(TEXT), "{logs}");
    assert!(!logs.contains("not-a-real-token"), "{logs}");
}

/// A signed URL's credential: nothing may log it.
const SIGNATURE: &str = "hm=secret-signature-0123456789";

#[tokio::test]
async fn downloads_never_log_a_url_a_file_name_or_a_signature_through_the_plugin() {
    use giles_channel_discord::client::Attachment;
    use giles_channel_discord::fake::Served as Cdn;

    let (capture, _logging) = capture_logs();
    let served = Served::start().await;
    served
        .configure(serde_json::json!({"token": TOKEN, "max_blob": "1 MiB"}))
        .await
        .unwrap();
    let (_acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    next_status(&mut events).await;

    let url =
        |name: &str| format!("https://cdn.discordapp.com/a/{MARKER}/{name}?ex=1&is=2&{SIGNATURE}");
    let file = |name: &str, size: u64| Attachment {
        url: url(name),
        filename: format!("{MARKER}-{name}"),
        content_type: Some("image/png".to_owned()),
        size,
    };
    served.client.serve(&url("ok.png"), Cdn::file(b"bytes"));
    served.client.serve(&url("gone.png"), Cdn::status(403));
    served.client.serve(
        &url("broken.png"),
        Cdn::Refused(giles_channel_discord::client::RestError::BadUrl),
    );
    let mut message = direct_message(now_id(1), 319_674_150_115_610_528, 1234, "files");
    message.attachments = vec![
        file("ok.png", 5),
        file("gone.png", 5),
        file("broken.png", 5),
        file("huge.png", 10 * 1024 * 1024),
    ];
    served.client.push(GatewayEvent::Message(message));
    loop {
        if let giles_plugin_sdk::giles_types::ChannelEvent::Message { message, .. } =
            support::next_event(&mut events).await
        {
            // One arrived, three are notes, and the notes are the only place the names are.
            assert_eq!(message.attachments.len(), 1);
            assert_eq!(message.text.lines().count(), 4, "{}", message.text);
            break;
        }
    }

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("an attachment was not received"), "{logs}");
    assert!(
        logs.contains("cdn.discordapp.com"),
        "the host is logged: {logs}"
    );
    for private in [MARKER, SIGNATURE, "ex=1", "/a/", ".png", TOKEN] {
        assert!(!logs.contains(private), "{private} is in the logs: {logs}");
    }
}

#[tokio::test]
async fn downloads_never_log_a_url_a_file_name_or_a_signature_through_the_real_client() {
    use futures::StreamExt as _;
    use giles_channel_discord::real::CdnPolicy;

    let (capture, _logging) = capture_logs();
    let server = loopback::rest::RestServer::start(|request, _| {
        let path = request.target.split('?').next().unwrap();
        match path {
            "/ok" => loopback::rest::Response {
                status: 200,
                headers: vec![],
                body: format!("the file says {MARKER}"),
            },
            "/moved" => loopback::rest::Response {
                status: 302,
                headers: vec![(
                    "location".into(),
                    format!("http://localhost:1/{MARKER}?{SIGNATURE}"),
                )],
                body: String::new(),
            },
            "/big" => loopback::rest::Response {
                status: 200,
                headers: vec![],
                body: "x".repeat(2 * 1024 * 1024),
            },
            _ => loopback::rest::Response {
                status: 403,
                headers: vec![],
                body: format!("<html>{MARKER}</html>"),
            },
        }
    })
    .await;
    let client = loopback::rest_client("127.0.0.1:9").with_cdn(CdnPolicy::loopback(&["127.0.0.1"]));
    for path in ["ok", "moved", "big", "expired", MARKER] {
        let url = format!("http://{}/{path}?ex=1&is=2&{SIGNATURE}", server.host);
        if let Ok(body) = client.download(&url, 1024 * 1024).await {
            let mut body = giles_channel_discord::client::limited(body, 1024 * 1024);
            while body.next().await.is_some_and(|chunk| chunk.is_ok()) {}
        }
    }
    // A URL that is refused before anything is requested.
    for url in [
        format!("https://evil.example/{MARKER}?{SIGNATURE}"),
        format!("http://localhost:{}/{MARKER}?{SIGNATURE}", 9),
    ] {
        assert!(client.download(&url, 1024).await.is_err());
    }

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("the CDN refused a download"), "{logs}");
    assert!(logs.contains("refused to download from a URL"), "{logs}");
    // The capture saw the HTTP libraries too (their `log` records), so their silence about URLs
    // is a finding and not a blind spot.
    assert!(logs.contains("starting new connection"), "{logs}");
    assert!(logs.contains(&server.host), "the host is logged: {logs}");
    for private in [MARKER, SIGNATURE, "ex=1", "evil.example", TOKEN] {
        assert!(!logs.contains(private), "{private} is in the logs: {logs}");
    }
}
