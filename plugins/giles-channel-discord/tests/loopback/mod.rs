//! A Gateway server of the test's own on loopback, playing Discord from its documentation, and a
//! `RealDiscord` pointed at it. Shared by the test binaries that drive the real client.

// Each test binary uses part of it.
#![allow(dead_code)]

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
