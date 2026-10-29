//! A Gateway server of the test's own on loopback, playing Discord from its documentation, and a
//! `RealDiscord` pointed at it. Shared by the test binaries that drive the real client.

// Each test binary uses part of it.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use giles_channel_discord::client::{Discord, GatewayEvent};
use giles_channel_discord::config::{Config, Settings};
use giles_channel_discord::real::RealDiscord;
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_websockets::{CloseCode, Message, ServerBuilder, WebSocketStream};

pub mod rest;

/// A token-shaped string for tests. Not a real credential.
pub const TOKEN: &str = "MTIzNDU2.Gabcde.not-a-real-token-1234567890";

/// Upper bound for anything in a test that must not hang.
pub const WAIT: Duration = Duration::from_secs(30);

/// The bot's user id in the fake READY.
pub const BOT: u64 = 82_198_898_841_029_460;

/// A loopback address that refuses connections for as long as the returned socket lives.
///
/// The socket is bound but never listened on, so connects to the address are refused, and no
/// other socket can take the port while it is held. Keep the socket alive until the test is done.
pub fn refused_address() -> (socket2::Socket, SocketAddr) {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    let addr = socket.local_addr().unwrap().as_socket().unwrap();
    (socket, addr)
}

/// A Gateway listening on loopback.
pub struct Gateway {
    pub listener: TcpListener,
    pub url: String,
}

impl Gateway {
    pub async fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        Self { listener, url }
    }

    /// Accepts a connection and sends Hello, with `heartbeat_ms` between heartbeats.
    pub async fn accept(&self, heartbeat_ms: u64) -> Connection {
        let (tcp, _) = tokio::time::timeout(WAIT, self.listener.accept())
            .await
            .expect("the client connects")
            .unwrap();
        let (_request, socket) = ServerBuilder::new().accept(tcp).await.unwrap();
        let mut connection = Connection(socket);
        connection
            .send(json!({"op": 10, "d": {"heartbeat_interval": heartbeat_ms}}))
            .await;
        connection
    }

    /// The READY dispatch, which points the client's resume at this server.
    pub fn ready(&self) -> Value {
        json!({
            "op": 0, "s": 1, "t": "READY",
            "d": {
                "v": 10,
                "user": {
                    "id": BOT.to_string(), "username": "Giles", "discriminator": "0",
                    "global_name": null, "avatar": null, "bot": true, "mfa_enabled": true,
                    "verified": true, "flags": 0
                },
                "guilds": [],
                "session_id": "session-1",
                "resume_gateway_url": self.url,
                "application": { "id": "82198898841029461", "flags": 8_978_432 },
                "shard": [0, 1]
            }
        })
    }
}

/// What the client sent.
#[derive(Debug)]
pub enum Frame {
    /// A JSON payload.
    Json(Value),
    /// A close frame with this code.
    Close(u16),
}

pub struct Connection(WebSocketStream<TcpStream>);

impl Connection {
    pub async fn send(&mut self, payload: Value) {
        self.0
            .send(Message::text(payload.to_string()))
            .await
            .unwrap();
    }

    pub async fn close(&mut self, code: u16) {
        let code = CloseCode::try_from(code).unwrap();
        self.0.send(Message::close(Some(code), "")).await.unwrap();
    }

    /// The next frame from the client.
    pub async fn recv(&mut self) -> Frame {
        let message = tokio::time::timeout(WAIT, self.0.next())
            .await
            .expect("a frame in time")
            .expect("the client is connected")
            .unwrap();
        if let Some((code, _)) = message.as_close() {
            // The library has queued the answering close frame, as Discord sends one.
            self.0.flush().await.unwrap();
            return Frame::Close(code.into());
        }
        Frame::Json(serde_json::from_str(message.as_text().expect("text")).unwrap())
    }

    /// The next frame, which must be a close, read without letting the library answer it: the
    /// connection is dropped by the caller.
    pub async fn recv_close_unanswered(&mut self) -> u16 {
        let message = tokio::time::timeout(WAIT, self.0.next())
            .await
            .expect("a frame in time")
            .expect("the client is connected")
            .unwrap();
        let (code, _) = message.as_close().expect("a close frame");
        code.into()
    }

    /// The next payload, which must be a JSON one.
    pub async fn recv_json(&mut self) -> Value {
        match self.recv().await {
            Frame::Json(payload) => payload,
            other => panic!("not a payload: {other:?}"),
        }
    }

    /// Reads the client's Identify and checks it: the bot's token and only the DM intent.
    pub async fn expect_identify(&mut self) {
        // The shard's identify queue can hold the Identify back for seconds, and a heartbeat
        // (op 1) may go out first.
        let identify = loop {
            let payload = self.recv_json().await;
            if payload["op"] != 1 {
                break payload;
            }
        };
        assert_eq!(identify["op"], 2, "{identify}");
        // `twilight-gateway` adds the `Bot ` prefix.
        assert_eq!(identify["d"]["token"], format!("Bot {TOKEN}"));
        // DIRECT_MESSAGES (1 << 12) and nothing else: not MESSAGE_CONTENT (1 << 15), which
        // would need a switch in the Developer Portal.
        assert_eq!(identify["d"]["intents"], 1 << 12);
    }
}

pub fn settings() -> Settings {
    let config: Config = serde_json::from_value(json!({ "token": TOKEN })).unwrap();
    Settings::new(config).unwrap()
}

/// A client pointed at the Gateway at `url`.
pub fn client(url: &str) -> RealDiscord {
    RealDiscord::with_gateway_url(&settings(), url)
}

/// A client whose REST calls go to `rest_host` (`127.0.0.1:<port>`) and whose Gateway is not used.
pub fn rest_client(rest_host: &str) -> RealDiscord {
    RealDiscord::with_urls(&settings(), "ws://127.0.0.1:9", rest_host)
}

pub async fn next(client: &RealDiscord) -> Option<GatewayEvent> {
    tokio::time::timeout(WAIT, client.next_event())
        .await
        .expect("an event in time")
}

/// A message object as Discord documents it, from a person in a DM: `author` writes `content` in
/// `channel`. The time is the one the id encodes.
pub fn message_json(id: u64, channel: u64, author: u64, content: &str) -> Value {
    let ms = giles_channel_discord::snowflake::unix_ms(id);
    let seconds = i64::try_from(ms / 1000).unwrap();
    let timestamp = rfc3339(seconds, ms % 1000);
    json!({
        "id": id.to_string(), "channel_id": channel.to_string(), "type": 0,
        "content": content, "timestamp": timestamp,
        "edited_timestamp": null, "tts": false, "mention_everyone": false, "mentions": [],
        "mention_roles": [], "pinned": false, "flags": 0, "embeds": [], "attachments": [],
        "author": {
            "id": author.to_string(), "username": format!("user{author}"), "discriminator": "0",
            "global_name": null, "avatar": null
        }
    })
}

/// A `MESSAGE_CREATE` dispatch with sequence number `seq`.
pub fn message_create(seq: u64, message: Value) -> Value {
    json!({"op": 0, "s": seq, "t": "MESSAGE_CREATE", "d": message})
}

/// `seconds` since the Unix epoch and `millis` as an RFC 3339 timestamp in UTC.
fn rfc3339(seconds: i64, millis: u64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}000+00:00",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}
