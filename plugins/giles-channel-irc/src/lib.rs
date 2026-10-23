//! The Giles IRC channel (`giles-channel-irc`): one IRC network per instance.
//!
//! The binary serves [`Irc`] through the plugin SDK; tests drive it directly against a local fake
//! server. Design notes are in `Docs/technical/irc.md`, configuration in `Docs/user/irc.md`.

mod casemap;
pub mod config;
mod conn;
mod inbound;
mod outbound;

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Channel, ChannelEvent, ChannelInfo, Error, EventId, MessageId, OutboundMessage, OutputMode,
    Result, SendTarget,
};
use giles_plugin_sdk::tls::rustls::ClientConfig;
use giles_plugin_sdk::{Configure, async_trait};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::{CancellationToken, DropGuard};

pub use self::config::{Config, Settings};
use self::conn::{Handles, Request, To};
pub use self::outbound::HOST_LEN;
use self::outbound::Recipient;

/// Inbound events buffered for the SDK, which persists each before taking the next. When it's
/// full, reading from the server waits.
const EVENT_BUFFER: usize = 256;

/// The connection's timeouts and delays. [`Default`] is what the plugin uses; tests shorten them.
#[derive(Debug, Clone)]
pub struct Timing {
    /// How long connecting (TCP and TLS) may take.
    pub connect_timeout: Duration,
    /// How long registration (CAP, SASL, up to the welcome) may take.
    pub register_timeout: Duration,
    /// How long the server may be quiet before the plugin sends a `PING`.
    pub ping_after: Duration,
    /// How long after that `PING` the connection is given up.
    pub ping_timeout: Duration,
    /// How long a `WHOIS` for a recipient may take.
    pub lookup_timeout: Duration,
    /// How long a reply into a conversation waits for a lost connection to come back.
    pub send_wait: Duration,
    /// How long one write may block.
    pub write_timeout: Duration,
    /// The first delay before reconnecting; it doubles after each failure.
    pub reconnect_min: Duration,
    /// The longest delay before reconnecting.
    pub reconnect_max: Duration,
    /// How long a connection must stay registered for the delay to start over at
    /// `reconnect_min`.
    pub stable_after: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            register_timeout: Duration::from_secs(60),
            ping_after: Duration::from_secs(120),
            ping_timeout: Duration::from_secs(60),
            lookup_timeout: Duration::from_secs(10),
            send_wait: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(300),
            stable_after: Duration::from_secs(60),
        }
    }
}

/// The plugin's `configure`: validates the config, then starts connecting in the background (a
/// server that's down doesn't fail `Configure`; it shows as an error status until it's back).
pub async fn configure(ctx: Configure) -> Result<Irc> {
    let settings = Settings::new(ctx.parse_config()?)?;
    let tls = settings.tls_config(giles_plugin_sdk::tls::root_store())?;
    tracing::info!(
        instance = ctx.instance(),
        host = %settings.host,
        port = settings.port,
        tls = settings.tls,
        namespace = %settings.namespace(),
        "IRC channel configured"
    );
    Ok(Irc::start(settings, tls, Timing::default()))
}

/// An IRC connection as a [`Channel`]. Dropping it closes the connection (`QUIT`).
pub struct Irc {
    info: ChannelInfo,
    mask_identities: bool,
    send_wait: Duration,
    requests: mpsc::Sender<Request>,
    registered: watch::Receiver<bool>,
    events: Arc<Mutex<mpsc::Receiver<ChannelEvent>>>,
    _stop: DropGuard,
}

impl Irc {
    /// Starts connecting to the server in `settings`, with TLS config `tls` (required when
    /// `settings.tls`; see [`Settings::tls_config`]). Must be called inside a Tokio runtime.
    pub fn start(settings: Settings, tls: Option<Arc<ClientConfig>>, timing: Timing) -> Self {
        let (events_tx, events) = mpsc::channel(EVENT_BUFFER);
        let (requests, requests_rx) = mpsc::channel(16);
        let (registered_tx, registered) = watch::channel(false);
        let stop = CancellationToken::new();
        let info = ChannelInfo {
            identity_namespace: settings.namespace(),
            output: OutputMode::Blocks,
            can_initiate: true,
        };
        let mask_identities = settings.mask_identities;
        let send_wait = timing.send_wait;
        let handles = Handles::new(events_tx, requests_rx, registered_tx, stop.clone());
        tokio::spawn(conn::run(Arc::new(settings), tls, timing, handles));
        Self {
            info,
            mask_identities,
            send_wait,
            requests,
            registered,
            events: Arc::new(Mutex::new(events)),
            _stop: stop.drop_guard(),
        }
    }

    async fn submit(&self, to: To, markdown: String) -> Result<MessageId> {
        let (done, result) = oneshot::channel();
        self.requests
            .send(Request { to, markdown, done })
            .await
            .map_err(|_| Error::unavailable("the IRC connection task has stopped"))?;
        result.await.unwrap_or_else(|_| {
            Err(Error::unavailable(
                "the connection to the IRC server was lost while sending",
            ))
        })
    }
}

#[async_trait]
impl Channel for Irc {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        let events = Arc::clone(&self.events);
        Ok(stream::unfold(events, |events| async move {
            let event = events.lock().await.recv().await?;
            Some((Ok(event), events))
        })
        .boxed())
    }

    async fn ack(&self, _event: &EventId) -> Result<()> {
        // The SDK keeps unacked events; IRC has nothing to acknowledge.
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        let mut markdown = msg.markdown;
        for media in &msg.attachments {
            let name = media.name.as_deref().unwrap_or(&media.media_type);
            markdown.push_str(&format!("\n\n[attachment not sent: {name}]"));
        }
        let to = match msg.target {
            SendTarget::Conversation(id) => {
                let mut registered = self.registered.clone();
                match tokio::time::timeout(self.send_wait, registered.wait_for(|r| *r)).await {
                    Ok(Ok(_)) => {}
                    _ => return Err(Error::unavailable("not connected to the IRC server")),
                }
                To::Conversation(id.as_str().to_owned())
            }
            SendTarget::Recipient(id) => {
                let recipient = outbound::recipient(id.as_str()).ok_or_else(|| {
                    Error::InvalidRequest(format!(
                        "{id} is not an IRC identity (account:<name> or mask:<nick>!<user>@<host>)"
                    ))
                })?;
                if matches!(recipient, Recipient::Mask { .. }) && !self.mask_identities {
                    return Err(Error::InvalidRequest(format!(
                        "{id}: mask identities are off on this instance"
                    )));
                }
                // The outbox retries; no point holding the send here.
                if !*self.registered.borrow() {
                    return Err(Error::unavailable("not connected to the IRC server"));
                }
                To::Recipient(recipient)
            }
            other => {
                return Err(Error::InvalidRequest(format!(
                    "unsupported send target {other:?}"
                )));
            }
        };
        self.submit(to, markdown).await
    }
}
