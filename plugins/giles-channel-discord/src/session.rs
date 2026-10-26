//! The task that owns the Discord connection: it reports the connection's status and paces
//! reconnects.
//!
//! ```text
//! Discord::next_event ──GatewayEvent──▶ session ──ChannelEvent::Status──▶ Channel::events ──▶ SDK
//!        ▲                                  │
//!        └── polled again only after the ───┘ reconnect delay
//! ```
//!
//! The client reconnects, resumes, and re-identifies by itself, but only while it is polled, so the
//! session controls the pace: after every close it waits (1 second, doubling to 5 minutes, back to
//! 1 second once a session has stayed up for a minute) before polling again. The library resets its
//! own backoff on every close, so without this a server that accepts the connection and closes it at
//! once would be retried, and Identified to, every second, and Discord resets the token of a bot
//! that Identifies too often (1000 times in 24 hours).
//!
//! The statuses are in [`crate::status`]. A drop is reported only if it outlasts the grace period; a
//! rejected token and the other closes the client never retries are final: the session reports them
//! and then only waits to be stopped.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use giles_plugin_sdk::giles_types::{ChannelEvent, ChannelStatus, EventId};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{Discord, GatewayEvent};
use crate::status::{self, Outcome};

/// The session's delays. [`Default`] is what the plugin uses; tests that need other values can
/// build their own.
#[derive(Debug, Clone)]
pub struct Timing {
    /// How long a dropped connection may take to come back before it's reported. The client
    /// resumes by itself, and Discord recycles connections now and then, so most drops are over
    /// before anyone needs to know.
    pub disconnect_grace: Duration,
    /// The delay before polling the client again after the first close.
    pub reconnect_min: Duration,
    /// The longest delay before polling the client again.
    pub reconnect_max: Duration,
    /// How long a session must stay up for the delay to start over at `reconnect_min`.
    pub stable_after: Duration,
    /// How long closing the connection may take.
    pub shutdown_wait: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            disconnect_grace: Duration::from_secs(10),
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(300),
            stable_after: Duration::from_secs(60),
            shutdown_wait: Duration::from_secs(5),
        }
    }
}

/// What [`run`] works with.
pub struct Session {
    /// The connection.
    pub client: Arc<dyn Discord>,
    /// The delays.
    pub timing: Timing,
    /// Events for the SDK.
    pub out: mpsc::Sender<ChannelEvent>,
    /// Cancelled when the channel is dropped or the plugin is stopping.
    pub stop: CancellationToken,
}

/// The future of one [`Discord::next_event`] call, started after the reconnect delay.
type Poll<'a> = Pin<Box<dyn Future<Output = Option<GatewayEvent>> + Send + 'a>>;

/// Waits until `at`, then asks for the next event.
async fn poll_at(client: &dyn Discord, at: Option<Instant>) -> Option<GatewayEvent> {
    if let Some(at) = at {
        tokio::time::sleep_until(at).await;
    }
    client.next_event().await
}

/// What woke the loop in [`run`].
enum Step {
    /// The channel is stopping.
    Stop,
    /// The connection has been down for the grace period.
    GraceOver,
    /// The client reported something, or ended.
    Event(Option<GatewayEvent>),
}

/// Runs the connection until the channel is stopped: reports statuses, delays polling after a
/// close, and closes the client at the end.
pub async fn run(session: Session) {
    let Session {
        client,
        timing,
        out,
        stop,
    } = session;
    let mut reporter = Reporter::new(out, stop.clone());
    // The delay the next close will impose, and when to poll after the last one.
    let mut delay = timing.reconnect_min;
    let mut poll_after: Option<Instant> = None;
    // When the current session came up, and when the connection dropped (until it comes back or is
    // reported).
    let mut up_since: Option<Instant> = None;
    let mut dropped_at: Option<Instant> = None;
    let mut pending: Option<Poll<'_>> = None;
    // A final close, or the client ending: nothing more is polled.
    let mut finished = false;
    loop {
        let step = if finished {
            stop.cancelled().await;
            Step::Stop
        } else {
            let poll =
                pending.get_or_insert_with(|| Box::pin(poll_at(&*client, poll_after.take())));
            let grace_over = async {
                match dropped_at {
                    Some(at) => tokio::time::sleep_until(at + timing.disconnect_grace).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = stop.cancelled() => Step::Stop,
                () = grace_over => Step::GraceOver,
                event = poll.as_mut() => Step::Event(event),
            }
        };
        match step {
            Step::Stop => break,
            Step::GraceOver => {
                dropped_at = None;
                reporter.report(status::disconnected()).await;
            }
            Step::Event(None) => {
                pending = None;
                finished = true;
                tracing::warn!("the Discord client ended its event stream");
                reporter.report(status::stopped()).await;
            }
            Step::Event(Some(event)) => {
                pending = None;
                match status::classify(&event) {
                    Outcome::Up => {
                        let now = Instant::now();
                        match event {
                            GatewayEvent::Resumed => tracing::info!("resumed the Discord session"),
                            _ => tracing::info!("connected to Discord with a new session"),
                        }
                        up_since = Some(now);
                        dropped_at = None;
                        reporter.report(ChannelStatus::Connected).await;
                    }
                    Outcome::Down => {
                        let now = Instant::now();
                        if up_since
                            .take()
                            .is_some_and(|since| now - since >= timing.stable_after)
                        {
                            delay = timing.reconnect_min;
                        }
                        tracing::warn!("disconnected from Discord; reconnecting in {delay:?}");
                        dropped_at.get_or_insert(now);
                        poll_after = Some(now + delay);
                        delay = (delay * 2).min(timing.reconnect_max);
                    }
                    Outcome::Final(status) => {
                        tracing::error!("the Discord connection is over: {event:?}");
                        finished = true;
                        dropped_at = None;
                        reporter.report(status).await;
                    }
                    Outcome::Unchanged => {}
                }
            }
        }
    }
    // The pending poll holds the client; end it before closing the connection.
    drop(pending);
    if tokio::time::timeout(timing.shutdown_wait, client.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("closing the Discord connection took too long");
    }
}

/// Sends status events, skipping a repeat of the last one.
struct Reporter {
    out: mpsc::Sender<ChannelEvent>,
    stop: CancellationToken,
    last: Option<ChannelStatus>,
    counter: u64,
}

impl Reporter {
    fn new(out: mpsc::Sender<ChannelEvent>, stop: CancellationToken) -> Self {
        Self {
            out,
            stop,
            last: None,
            counter: 0,
        }
    }

    async fn report(&mut self, status: ChannelStatus) {
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
            () = self.stop.cancelled() => {}
            // Fails only once the channel is gone, when nobody needs the status.
            _ = self.out.send(event) => {}
        }
    }
}

#[cfg(test)]
mod tests;
