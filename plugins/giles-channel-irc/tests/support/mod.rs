//! A scripted fake IRC server on 127.0.0.1 (plain or TLS), and helpers to run the channel
//! against it.
//!
//! The TLS fixtures in `tests/fixtures/` are a test CA and a server (127.0.0.1, localhost) and
//! client certificate it signed, valid for 100 years, made with:
//!
//! ```sh
//! openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ca.key
//! openssl req -x509 -new -key ca.key -config ca.cnf -extensions v3_ca -days 36500 -out ca.pem
//! openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out server.key
//! openssl req -new -key server.key -subj /CN=giles-irc-test-server -out server.csr
//! openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -days 36500 -extfile ca.cnf \
//!     -extensions server -out server.pem   # likewise client.pem with `-extensions client`
//! ```
//!
//! where `ca.cnf` has `basicConstraints = critical, CA:TRUE` for `v3_ca`, `subjectAltName =
//! IP:127.0.0.1, DNS:localhost` and `extendedKeyUsage = serverAuth` for `server`, and
//! `extendedKeyUsage = clientAuth` for `client`.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use giles_channel_irc::{Config, Irc, Settings, Timing};
use giles_plugin_sdk::giles_types::{
    Channel as _, ChannelEvent, ChannelStatus, EventId, InboundMessage,
};
use giles_plugin_sdk::tls::rustls::pki_types::pem::PemObject as _;
use giles_plugin_sdk::tls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use giles_plugin_sdk::tls::rustls::server::WebPkiClientVerifier;
use giles_plugin_sdk::tls::rustls::{self, RootCertStore, ServerConfig};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader, ReadHalf, WriteHalf,
};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Upper bound for anything that must not hang.
pub const WAIT: Duration = Duration::from_secs(10);

/// The test CA.
pub const CA: &str = include_str!("../fixtures/ca.pem");
/// The client certificate the CA signed.
pub const CLIENT_CERT: &str = include_str!("../fixtures/client.pem");
/// Its key.
pub const CLIENT_KEY: &str = include_str!("../fixtures/client.key");
const SERVER_CERT: &str = include_str!("../fixtures/server.pem");
const SERVER_KEY: &str = include_str!("../fixtures/server.key");

/// The capabilities a modern server offers.
pub const MODERN_CAPS: &str =
    "account-notify account-tag echo-message extended-join message-tags multi-prefix server-time";
/// What the plugin requests of [`MODERN_CAPS`].
pub const MODERN_REQ: &str =
    "account-notify account-tag echo-message extended-join message-tags server-time";

trait Stream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Stream for T {}

/// A listening fake server.
pub struct FakeServer {
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
}

impl FakeServer {
    /// A plain TCP server.
    pub async fn plain() -> Self {
        Self {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            tls: None,
        }
    }

    /// A TLS server with the fixture certificate, requiring a client certificate from the test CA
    /// if `client_auth`.
    pub async fn tls(client_auth: bool) -> Self {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let builder = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap();
        let builder = if client_auth {
            let verifier =
                WebPkiClientVerifier::builder_with_provider(Arc::new(ca_roots()), provider)
                    .build()
                    .unwrap();
            builder.with_client_cert_verifier(verifier)
        } else {
            builder.with_no_client_auth()
        };
        let chain = CertificateDer::pem_slice_iter(SERVER_CERT.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(SERVER_KEY.as_bytes()).unwrap();
        let config = builder.with_single_cert(chain, key).unwrap();
        Self {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
            tls: Some(TlsAcceptor::from(Arc::new(config))),
        }
    }

    /// Accepts a connection whose TLS handshake must fail, and returns the server's error.
    pub async fn accept_failing(&self) -> String {
        let (tcp, _) = tokio::time::timeout(WAIT, self.listener.accept())
            .await
            .expect("no connection")
            .unwrap();
        let acceptor = self.tls.as_ref().expect("a TLS server");
        match tokio::time::timeout(WAIT, acceptor.accept(tcp))
            .await
            .unwrap()
        {
            Ok(_) => panic!("the handshake succeeded"),
            Err(e) => e.to_string(),
        }
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }

    /// The next connection.
    pub async fn accept(&self) -> Peer {
        let (tcp, _) = tokio::time::timeout(WAIT, self.listener.accept())
            .await
            .expect("no connection")
            .unwrap();
        let (stream, client_certs): (Box<dyn Stream>, _) = match &self.tls {
            None => (Box::new(tcp), 0),
            Some(acceptor) => {
                let stream = tokio::time::timeout(WAIT, acceptor.accept(tcp))
                    .await
                    .unwrap()
                    .unwrap();
                let certs = stream.get_ref().1.peer_certificates().map_or(0, <[_]>::len);
                (Box::new(stream), certs)
            }
        };
        let (reader, writer) = tokio::io::split(stream);
        Peer {
            reader: BufReader::new(reader),
            writer,
            client_certs,
        }
    }
}

/// The CA as a root store.
pub fn ca_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(CA.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    roots
}

/// One client connection to the fake server.
pub struct Peer {
    reader: BufReader<ReadHalf<Box<dyn Stream>>>,
    writer: WriteHalf<Box<dyn Stream>>,
    /// How many certificates the client presented (TLS).
    pub client_certs: usize,
}

impl Peer {
    /// The next line from the client, without CRLF. Panics at end of stream.
    pub async fn recv(&mut self) -> String {
        self.try_recv()
            .await
            .expect("the client closed the connection")
    }

    /// The next line, or `None` at end of stream.
    pub async fn try_recv(&mut self) -> Option<String> {
        let mut line = String::new();
        let read = tokio::time::timeout(WAIT, self.reader.read_line(&mut line))
            .await
            .expect("no line from the client")
            .ok()?;
        if read == 0 {
            return None;
        }
        assert!(line.ends_with("\r\n"), "{line:?}");
        line.truncate(line.len() - 2);
        Some(line)
    }

    /// Asserts the next line is `want`.
    pub async fn expect(&mut self, want: &str) {
        assert_eq!(self.recv().await, want);
    }

    /// Sends `line` (CRLF added).
    pub async fn send(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .unwrap();
    }

    /// Expects the registration lines, then offers `offered` capabilities and acks `req` (the
    /// request it expects; `None`: none), and welcomes the client as `giles`.
    pub async fn register(&mut self, offered: &str, req: Option<&str>) {
        self.expect_hello("giles").await;
        self.send(&format!(":irc.test CAP * LS :{offered}")).await;
        if let Some(req) = req {
            self.expect(&format!("CAP REQ :{req}")).await;
            self.send(&format!(":irc.test CAP * ACK :{req}")).await;
        }
        self.expect("CAP END").await;
        self.welcome("giles").await;
    }

    /// Expects `CAP LS 302`, `NICK <nick>`, and `USER`.
    pub async fn expect_hello(&mut self, nick: &str) {
        self.expect("CAP LS 302").await;
        self.expect(&format!("NICK {nick}")).await;
        self.expect(&format!("USER {nick} 0 * :Giles")).await;
    }

    /// Sends the welcome and ISUPPORT numerics for `nick`.
    pub async fn welcome(&mut self, nick: &str) {
        self.send(&format!(
            ":irc.test 001 {nick} :Welcome to the test network"
        ))
        .await;
        self.send(&format!(
            ":irc.test 005 {nick} CASEMAPPING=rfc1459 CHANTYPES=# STATUSMSG=@+ :are supported"
        ))
        .await;
    }

    /// Proves nothing else was sent before now: a `PING` the client must answer next.
    pub async fn sync(&mut self, token: &str) {
        self.send(&format!("PING :{token}")).await;
        self.expect(&format!("PONG :{token}")).await;
    }
}

/// Settings for the fake server at `port`: plain TCP, nick `giles`, network `testnet`, no rate
/// limit, with `extra` keys merged in.
pub fn settings(port: u16, extra: Value) -> Settings {
    let mut config = json!({
        "server": "127.0.0.1",
        "port": port,
        "tls": false,
        "nick": "giles",
        "network": "testnet",
        "send_interval_ms": 0,
    });
    for (key, value) in extra.as_object().unwrap() {
        config[key] = value.clone();
    }
    let config: Config = serde_json::from_value(config).unwrap();
    Settings::new(config).unwrap()
}

/// Short timeouts and reconnect delays.
pub fn timing() -> Timing {
    Timing {
        reconnect_min: Duration::from_millis(50),
        reconnect_max: Duration::from_millis(200),
        lookup_timeout: Duration::from_secs(5),
        send_wait: Duration::from_secs(5),
        ..Timing::default()
    }
}

/// A running channel and its event stream.
pub struct Client {
    /// The channel.
    pub irc: Arc<Irc>,
    events: BoxStream<'static, giles_plugin_sdk::giles_types::Result<ChannelEvent>>,
}

impl Client {
    /// Starts a plain-TCP channel with `settings` and `timing`.
    pub async fn start(settings: Settings, timing: Timing) -> Self {
        let tls = settings.tls_config(ca_roots()).unwrap();
        let irc = Arc::new(Irc::start(settings, tls, timing));
        let events = irc.events().await.unwrap();
        Self { irc, events }
    }

    /// The next event.
    pub async fn event(&mut self) -> ChannelEvent {
        tokio::time::timeout(WAIT, self.events.next())
            .await
            .expect("no event")
            .expect("the event stream ended")
            .unwrap()
    }

    /// The next status, skipping messages.
    pub async fn status(&mut self) -> ChannelStatus {
        loop {
            if let ChannelEvent::Status { status, .. } = self.event().await {
                return status;
            }
        }
    }

    /// The next message, skipping statuses.
    pub async fn message(&mut self) -> (EventId, InboundMessage) {
        loop {
            if let ChannelEvent::Message { id, message } = self.event().await {
                return (id, message);
            }
        }
    }

    /// Waits for `Connected`.
    pub async fn connected(&mut self) {
        assert_eq!(self.status().await, ChannelStatus::Connected);
    }

    /// The next error status's message.
    pub async fn error(&mut self) -> String {
        match self.status().await {
            ChannelStatus::Error { message } => message,
            other => panic!("expected an error, got {other:?}"),
        }
    }
}
