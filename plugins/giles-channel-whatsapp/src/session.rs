//! The task that owns the WhatsApp client: it reports the connection's status, turns inbound
//! batches into events, and holds each batch's acknowledgement until the SDK has persisted it.
//!
//! ```text
//! client ──WaEvent──▶ session ──Outgoing──▶ Channel::events ──▶ SDK spool (persist, then poll)
//!    ▲                   │                        │
//!    └── Commit::done ◀──┴──── `persisted` ◀──────┘ fires when the SDK polls the stream again
//! ```
//!
//! The SDK persists an event before it polls the channel's stream for the next one, so a stream
//! poll after an event means that event is on disk. The session emits a batch's events and waits
//! for that poll after the last one; only then does it resolve the batch's [`Commit`], which
//! lets the client acknowledge the messages to WhatsApp. A failure, a shutdown, or a dropped
//! stream resolves it as not durable, and WhatsApp delivers the messages again.

use std::sync::Arc;
use std::time::Duration;

use giles_plugin_sdk::giles_types::{ChannelEvent, ChannelStatus, EventId, Timestamp};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::backend::{Backend, Batch, Connection, Connector, WaEvent};
use crate::config::Settings;
use crate::datadir::DataDir;
use crate::inbound;
use crate::media::MediaStore;
use crate::status;

/// The session's delays. [`Default`] is what the plugin uses; tests shorten them.
#[derive(Debug, Clone)]
pub(crate) struct Timing {
    /// How long a dropped connection may take to come back before it's reported. The client
    /// reconnects by itself, and WhatsApp recycles connections now and then, so most drops are
    /// over before anyone needs to know.
    pub disconnect_grace: Duration,
    /// The first delay before starting a stopped client again; it doubles after each failure.
    pub restart_min: Duration,
    /// The longest delay before starting the client again.
    pub restart_max: Duration,
    /// How long a client must run for the delay to start over at `restart_min`.
    pub stable_after: Duration,
    /// How long a reply waits for a dropped connection to come back.
    pub send_wait: Duration,
    /// How long shutting the client down may take.
    pub shutdown_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            disconnect_grace: Duration::from_secs(10),
            restart_min: Duration::from_secs(5),
            restart_max: Duration::from_secs(300),
            stable_after: Duration::from_secs(120),
            send_wait: Duration::from_secs(30),
            shutdown_wait: Duration::from_secs(10),
        }
    }
}

/// An event on its way to the SDK.
pub(crate) struct Outgoing {
    /// The event.
    pub event: ChannelEvent,
    /// Fired when the SDK has persisted `event` (when it asks for the next one).
    pub persisted: Option<oneshot::Sender<()>>,
}

/// How a connection ended.
enum End {
    /// The plugin is shutting down.
    Stop,
    /// The client can't go on and shouldn't be restarted without the operator (a logout, a ban,
    /// QR codes that ran out): the plugin idles with its error status.
    Terminal,
    /// The client died by itself: start another after a delay.
    Restart(String),
}

/// What [`run`] works with.
pub(crate) struct Session {
    pub connector: Arc<dyn Connector>,
    pub store: Arc<dyn MediaStore>,
    pub settings: Arc<Settings>,
    pub data: Arc<DataDir>,
    pub timing: Timing,
    /// Events for the SDK.
    pub out: mpsc::Sender<Outgoing>,
    /// The backend while it's connected, for sends.
    pub backend: watch::Sender<Option<Arc<dyn Backend>>>,
    /// Cancelled when the channel is dropped.
    pub stop: CancellationToken,
}

/// Runs clients until the channel is dropped: connect, serve events, and after a client dies by
/// itself, wait and connect again. A client that's no longer usable (logged out, banned, QR codes
/// used up) ends the loop: starting another would only repeat the failure, or (for QR codes) ask
/// WhatsApp for pairing over and over. The operator restarts the plugin for a new attempt.
pub(crate) async fn run(session: Session) {
    let mut reporter = Reporter::default();
    let mut delay = session.timing.restart_min;
    loop {
        let started = Instant::now();
        let connected = tokio::select! {
            () = session.stop.cancelled() => return,
            connected = session.connector.connect() => connected,
        };
        let end = match connected {
            Err(why) => {
                tracing::warn!("couldn't start the WhatsApp client: {why}");
                reporter
                    .report(&session, status::failed_to_start(&why))
                    .await;
                End::Restart(why)
            }
            Ok(connection) => {
                tracing::info!("the WhatsApp client started");
                let end = drive(&session, &mut reporter, connection).await;
                session.backend.send_replace(None);
                end
            }
        };
        match end {
            End::Stop => return,
            End::Terminal => {
                session.stop.cancelled().await;
                return;
            }
            End::Restart(why) => {
                if started.elapsed() >= session.timing.stable_after {
                    delay = session.timing.restart_min;
                }
                tracing::warn!("{why}; starting the client again in {delay:?}");
                tokio::select! {
                    () = session.stop.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(session.timing.restart_max);
            }
        }
    }
}

/// Serves one client's events until it ends.
async fn drive(session: &Session, reporter: &mut Reporter, connection: Connection) -> End {
    let Connection {
        backend,
        mut events,
    } = connection;
    // When the connection dropped, and why, until it comes back or is reported.
    let mut dropped: Option<(Instant, String)> = None;
    let end = loop {
        let grace_over = async {
            match &dropped {
                Some((at, _)) => {
                    tokio::time::sleep_until(*at + session.timing.disconnect_grace).await
                }
                None => std::future::pending().await,
            }
        };
        let event = tokio::select! {
            biased;
            () = session.stop.cancelled() => break End::Stop,
            () = grace_over => {
                if let Some((_, reason)) = dropped.take() {
                    reporter.report(session, status::disconnected(&reason)).await;
                }
                continue;
            }
            event = events.recv() => event,
        };
        let Some(event) = event else {
            let reason = "its event stream ended";
            reporter.report(session, status::restarting(reason)).await;
            break End::Restart(format!("the WhatsApp client stopped ({reason})"));
        };
        match &event {
            WaEvent::Messages(_) => {}
            WaEvent::Connected => {
                dropped = None;
                session.backend.send_replace(Some(Arc::clone(&backend)));
            }
            WaEvent::Disconnected(reason) => {
                tracing::warn!("disconnected from WhatsApp: {reason}");
                session.backend.send_replace(None);
                dropped.get_or_insert_with(|| (Instant::now(), reason.clone()));
            }
            other => tracing::debug!("{}", describe(other)),
        }
        let terminal = matches!(
            event,
            WaEvent::LoggedOut(_) | WaEvent::Fatal(_) | WaEvent::QrExhausted { closed: true }
        );
        if matches!(event, WaEvent::LoggedOut(_))
            && let Err(e) = session.data.mark_logged_out()
        {
            tracing::error!("recording the logout: {e}");
        }
        if let WaEvent::Messages(batch) = event {
            handle_batch(session, &*backend, batch).await;
            continue;
        }
        if let Some(status) = status::for_event(&event, &session.settings.link, Timestamp::now()) {
            reporter.report(session, status).await;
        }
        if terminal {
            break End::Terminal;
        }
        if let WaEvent::Stopped(reason) = &event {
            break End::Restart(format!("the WhatsApp client stopped ({reason})"));
        }
    };
    // A client that died is already down; this releases what it still holds.
    if tokio::time::timeout(session.timing.shutdown_wait, backend.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("shutting the WhatsApp client down took too long");
    }
    end
}

/// A short description of an event for the debug log. Never includes message content, a QR code,
/// or a pair code.
fn describe(event: &WaEvent) -> &'static str {
    match event {
        WaEvent::PairingQr { .. } => "WhatsApp offered a QR code",
        WaEvent::PairingCode { .. } => "WhatsApp offered a pair code",
        WaEvent::QrExhausted { .. } => "WhatsApp's QR codes ran out",
        WaEvent::LinkFailed(_) => "linking failed",
        WaEvent::Paired => "the phone accepted the link",
        WaEvent::Connected => "connected to WhatsApp",
        WaEvent::Disconnected(_) => "disconnected from WhatsApp",
        WaEvent::LoggedOut(_) => "logged out by WhatsApp",
        WaEvent::Fatal(_) => "the WhatsApp client can't continue",
        WaEvent::Stopped(_) => "the WhatsApp client stopped",
        WaEvent::Messages(_) => "messages",
    }
}

/// Turns a batch into events, hands them to the SDK, and resolves the batch's commit once the
/// SDK has persisted the last one.
async fn handle_batch(session: &Session, backend: &dyn Backend, batch: Batch) {
    let Batch { messages, commit } = batch;
    let mut events = Vec::with_capacity(messages.len());
    for message in messages {
        match inbound::event(message, backend, &*session.store, session.settings.max_blob).await {
            Ok(Some(event)) => events.push(event),
            Ok(None) => {}
            Err(e) => {
                // Not durable: WhatsApp delivers the batch again.
                tracing::error!("keeping an inbound message: {e}");
                commit.done(false);
                return;
            }
        }
    }
    let Some(last) = events.len().checked_sub(1) else {
        // Nothing to deliver (echoes, status updates): done with them.
        commit.done(true);
        return;
    };
    let mut persisted = None;
    for (index, event) in events.into_iter().enumerate() {
        let (tx, rx) = oneshot::channel();
        let wait = (index == last).then_some(tx);
        let sent = tokio::select! {
            () = session.stop.cancelled() => false,
            sent = session.out.send(Outgoing { event, persisted: wait }) => sent.is_ok(),
        };
        if !sent {
            commit.done(false);
            return;
        }
        persisted = Some(rx);
    }
    let Some(persisted) = persisted else {
        commit.done(false);
        return;
    };
    let durable = tokio::select! {
        () = session.stop.cancelled() => false,
        persisted = persisted => persisted.is_ok(),
    };
    commit.done(durable);
}

/// Sends status events, skipping a repeat of the last one.
#[derive(Default)]
struct Reporter {
    last: Option<ChannelStatus>,
    counter: u64,
}

impl Reporter {
    async fn report(&mut self, session: &Session, status: ChannelStatus) {
        if self.last.as_ref() == Some(&status) {
            return;
        }
        self.last = Some(status.clone());
        self.counter += 1;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let Ok(id) = EventId::new(format!("status:{nanos}:{}", self.counter)) else {
            return;
        };
        let event = ChannelEvent::Status { id, status };
        tokio::select! {
            () = session.stop.cancelled() => {}
            // Fails only once the channel is gone, when nobody needs the status.
            _ = session.out.send(Outgoing { event, persisted: None }) => {}
        }
    }
}
