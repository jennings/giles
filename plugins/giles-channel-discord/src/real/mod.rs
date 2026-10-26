//! The Discord client on `twilight-gateway` and `twilight-http`: the only code that touches the
//! library's types.
//!
//! It is thin on purpose. The decisions (what to report, how long to wait before reconnecting,
//! what is final, what to deliver) are in [`status`](crate::status), [`session`](crate::session),
//! and [`inbound`](crate::inbound) and are tested against a fake client; this file maps the
//! library's events to the plugin's, closes the shard, and makes the REST calls. [`convert`] is
//! pure and tested from JSON fixtures of the documented payloads, and `tests/gateway.rs` and
//! `tests/rest.rs` drive the whole client against a Gateway and a REST server of their own on
//! loopback. Nothing here has talked to Discord itself (issue #20).

use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use futures::{Stream as _, StreamExt as _};
use giles_plugin_sdk::async_trait;
use giles_plugin_sdk::tls::rustls;
use tokio::sync::Mutex;
use twilight_gateway::error::{ReceiveMessageError, ReceiveMessageErrorType};
use twilight_gateway::{
    CloseFrame, ConfigBuilder, Event, EventTypeFlags, Intents, Message, Shard, ShardId, ShardState,
};
use twilight_http::Client as Http;

use crate::client::{Body, Discord, GatewayEvent, NewMessage, Page, RestError};
use crate::config::Settings;

mod cdn;
mod message;
mod multipart;
mod rest;

pub use cdn::{CdnPolicy, MAX_REDIRECTS};

/// What the plugin asks Discord for: direct messages. Not `MESSAGE_CONTENT`: Discord delivers the
/// content of a bot's DMs without it, so no privileged intent has to be enabled in the Developer
/// Portal (`Docs/technical/discord.md`, "Gateway Intents").
const INTENTS: Intents = Intents::DIRECT_MESSAGES;

/// The events the client deserializes; `twilight-gateway` skips the rest without parsing them.
/// Closes always come through. Reconnect requests and invalid sessions are handled inside the
/// shard, which closes the connection, and the close is what the plugin sees.
const EVENTS: EventTypeFlags = EventTypeFlags::READY
    .union(EventTypeFlags::RESUMED)
    .union(EventTypeFlags::MESSAGE_CREATE);

/// How long [`shutdown`](Discord::shutdown) waits for Discord's answer to the close frame. It must
/// stay under the second `twilight-gateway` sleeps before every connect: if the peer ends the TCP
/// connection without answering, the shard starts reconnecting (and would Identify, putting the bot
/// back online after it was shut down), and the wait must be over before that can happen.
const CLOSE_WAIT: Duration = Duration::from_millis(900);

/// A Gateway shard, polled by the session, a REST client, and a client for the CDN.
pub struct RealDiscord {
    state: Mutex<State>,
    http: Http,
    /// The client of calls with files: a longer timeout, the same rate limiter.
    uploads: Http,
    cdn: cdn::Cdn,
}

/// The shard, and what the client has noticed about it.
struct State {
    shard: Shard,
    /// Whether the shard was connected the last time it was polled.
    up: bool,
    /// Whether its stream has ended (it must not be polled after that).
    ended: bool,
}

/// Makes `aws-lc-rs`, the provider the SDK's TLS config uses, the process default. With default
/// features off `rustls` doesn't choose a provider, and `twilight-gateway` builds its TLS
/// connector from the default one (it has no hook for a `ClientConfig`), panicking without one. An
/// error means a provider is already installed, which is fine.
fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

impl RealDiscord {
    /// A shard for the bot `settings.token` belongs to, and a REST client for it. The shard
    /// connects when first polled.
    ///
    /// Installs the process's default crypto provider first, which is why this is the one place a
    /// connection is set up.
    pub fn new(settings: &Settings) -> Self {
        Self::build(settings, None, None)
    }

    /// Like [`new`](Self::new), connecting to the Gateway at `url` (`ws://...`, no TLS) instead of
    /// Discord's. For tests that run a Gateway of their own.
    pub fn with_gateway_url(settings: &Settings, url: &str) -> Self {
        Self::build(settings, Some(url), None)
    }

    /// Like [`with_gateway_url`](Self::with_gateway_url), with REST calls going to `rest_host`
    /// (`127.0.0.1:<port>`, plain HTTP) instead of `discord.com`. For tests that run a REST
    /// server of their own.
    pub fn with_urls(settings: &Settings, gateway_url: &str, rest_host: &str) -> Self {
        Self::build(settings, Some(gateway_url), Some(rest_host))
    }

    fn build(settings: &Settings, gateway_url: Option<&str>, rest_host: Option<&str>) -> Self {
        install_crypto_provider();
        let mut gateway = ConfigBuilder::new(settings.token.expose().to_owned(), INTENTS);
        if let Some(url) = gateway_url {
            gateway = gateway.proxy_url(url.to_owned());
        }
        let token = settings.token.expose().to_owned();
        let mut main = Http::builder().token(token.clone());
        if let Some(host) = rest_host {
            main = main.proxy(host.to_owned(), true);
        }
        let main = main.build();
        // A second client for file uploads only, so the 5 minutes an upload may take
        // (`rest::UPLOAD_TIMEOUT`) don't become the timeout of every call (the library's default,
        // 10 seconds, stays the main client's). It shares the main client's rate limiter, so
        // Discord's buckets and the global limit see one bot. It keeps its own memory of a 401
        // (the library's lock-out is per client); the plugin's `TokenState` is what stops sends.
        let mut uploads = Http::builder()
            .token(token)
            .timeout(rest::UPLOAD_TIMEOUT)
            .ratelimiter(main.ratelimiter().cloned());
        if let Some(host) = rest_host {
            uploads = uploads.proxy(host.to_owned(), true);
        }
        Self {
            state: Mutex::new(State {
                shard: Shard::with_config(ShardId::ONE, gateway.build()),
                up: false,
                ended: false,
            }),
            http: main,
            uploads: uploads.build(),
            cdn: cdn::Cdn::new(CdnPolicy::discord()),
        }
    }

    /// This client downloading under `policy` instead of Discord's. For tests that run a CDN of
    /// their own on loopback.
    #[must_use]
    pub fn with_cdn(mut self, policy: CdnPolicy) -> Self {
        self.cdn = cdn::Cdn::new(policy);
        self
    }
}

/// What polling the shard produced.
enum Polled {
    /// A message, an error, or the end of the stream.
    Item(Option<Result<Message, ReceiveMessageError>>),
    /// The connection went away without a close frame, and the shard has already started
    /// connecting again.
    Dropped,
}

impl State {
    /// Polls the shard once for something to report.
    ///
    /// `twilight-gateway` surfaces a close frame, a transport error, and a failed connect, but
    /// not a connection that Discord (or a proxy) ends with a plain TCP close: the shard notes the
    /// disconnect and reconnects inside the same poll, with nothing to see. The plugin must see
    /// it, or it could neither report it nor pace it, so the state is read after every poll: a
    /// shard that was connected and is now disconnected, with no message to show for it, is
    /// [`Polled::Dropped`].
    async fn poll(&mut self) -> Polled {
        std::future::poll_fn(|cx| {
            let polled = Pin::new(&mut self.shard).poll_next(cx);
            let up = matches!(
                self.shard.state(),
                ShardState::Active | ShardState::Resuming | ShardState::Identifying
            );
            match polled {
                Poll::Ready(item) => {
                    self.up = up;
                    Poll::Ready(Polled::Item(item))
                }
                Poll::Pending if self.up && !up => {
                    self.up = false;
                    Poll::Ready(Polled::Dropped)
                }
                Poll::Pending => {
                    self.up = up;
                    Poll::Pending
                }
            }
        })
        .await
    }
}

/// The plugin's view of a library event, or `None` for one it doesn't use.
fn convert(event: &Event) -> Option<GatewayEvent> {
    match event {
        Event::Ready(ready) => Some(GatewayEvent::Ready {
            user_id: ready.user.id.get(),
        }),
        Event::Resumed => Some(GatewayEvent::Resumed),
        Event::MessageCreate(create) => Some(GatewayEvent::Message(message::convert(&create.0))),
        Event::GatewayClose(frame) => Some(GatewayEvent::Closed {
            code: frame.as_ref().map(|frame| frame.code),
        }),
        _ => None,
    }
}

/// The plugin's view of a Gateway message, or `None` for one it doesn't use.
fn convert_message(message: Message) -> Option<GatewayEvent> {
    let event: Event = match message {
        Message::Close(frame) => Event::GatewayClose(frame),
        Message::Text(json) => match twilight_gateway::parse(json, EVENTS) {
            Ok(Some(event)) => event.into(),
            Ok(None) => return None,
            // The error's text is the event as received, which may be a message: say only that
            // it happened. The next poll carries on.
            Err(_) => {
                tracing::warn!("a Discord Gateway event couldn't be deserialized");
                return None;
            }
        },
    };
    convert(&event)
}

#[async_trait]
impl Discord for RealDiscord {
    async fn next_event(&self) -> Option<GatewayEvent> {
        let mut state = self.state.lock().await;
        if state.ended {
            return None;
        }
        loop {
            match state.poll().await {
                // A close the shard won't reconnect after: it has closed the connection.
                Polled::Item(None) => {
                    state.ended = true;
                    return None;
                }
                Polled::Item(Some(Ok(message))) => {
                    if let Some(event) = convert_message(message) {
                        return Some(event);
                    }
                }
                // The shard failed to connect (the only error that isn't an event that failed to
                // deserialize). It tries again when polled, after a delay.
                Polled::Item(Some(Err(error))) => match error.kind() {
                    ReceiveMessageErrorType::Deserializing { .. } => {
                        tracing::warn!("a Discord Gateway event couldn't be deserialized");
                    }
                    _ => {
                        tracing::debug!("connecting to the Discord Gateway failed: {error}");
                        return Some(GatewayEvent::Closed { code: None });
                    }
                },
                Polled::Dropped => {
                    tracing::debug!("the Discord Gateway connection ended without a close");
                    return Some(GatewayEvent::Closed { code: None });
                }
            }
        }
    }

    async fn messages_after(&self, channel: u64, after: u64, limit: u8) -> Result<Page, RestError> {
        rest::messages_after(&self.http, channel, after, limit).await
    }

    async fn create_message(&self, channel: u64, message: NewMessage) -> Result<u64, RestError> {
        rest::create_message(&self.http, &self.uploads, channel, &message).await
    }

    async fn create_dm(&self, user: u64) -> Result<u64, RestError> {
        rest::create_dm(&self.http, user).await
    }

    async fn download(&self, url: &str, max: u64) -> Result<Body, RestError> {
        self.cdn.download(url, max).await
    }

    async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        // Without a session there is nothing to end, and the bot isn't shown online: dropping the
        // shard drops the connection. (A fatally closed shard has already closed it.)
        if state.ended || !state.shard.state().is_identified() {
            return;
        }
        // Close code 1000 ends the session and shows the bot offline, instead of leaving it
        // "online" until Discord times the connection out. The shard sends the frame, and
        // Discord's close frame in answer completes the handshake. Only the wait is bounded by
        // `CLOSE_WAIT`; the frame goes out on the first poll.
        state.shard.close(CloseFrame::NORMAL);
        let shard = &mut state.shard;
        let answered = async {
            while let Some(message) = shard.next().await {
                if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                    break;
                }
            }
        };
        if tokio::time::timeout(CLOSE_WAIT, answered).await.is_err() {
            tracing::debug!("Discord didn't answer the close frame");
        }
        state.ended = true;
    }
}

#[cfg(test)]
mod tests;
