//! The task that owns the Discord connection: it reports the connection's status and paces
//! reconnects.
//!
//! ```text
//! Discord::next_event ──GatewayEvent──▶ session ──Staged──▶ deliver ──Outgoing──▶ Channel::events
//!        ▲                               │  ▲  │
//!        └── polled again only after the ┘  │  └─ Ingest: marks, buffers, catch-up passes
//!            reconnect delay                └──── Discord::messages_after (one page at a time)
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
//! and then only waits to be stopped. A 401 on REST is not final: a send that gets one
//! ([`TokenState::reject`]) and a history request that gets one both stop all REST (the sends
//! refuse, nothing is fetched, every channel goes live) and report `NeedsAuth`, but the Gateway
//! session is still up, so live messages keep arriving, and the status stays `NeedsAuth` through
//! drops and reconnects until the Gateway itself says the token is bad (4004), or a restart.
//!
//! Messages go through [`Ingest`] ([`crate::ingest`]), which decides what is delivered and which
//! channel's history to fetch next. The session makes the one history request in flight, in the same
//! `select!` as the Gateway poll, so a slow or failing request never holds up a live event (the
//! ingest buffers the live events of the channel being caught up, and hands the rest over at once),
//! and a stop ends it like everything else.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use giles_plugin_sdk::giles_types::{ChannelEvent, ChannelStatus, EventId};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::client::{Discord, GatewayEvent, Page, RestError};
use crate::deliver::Staged;
use crate::ingest::{Cursor, Fetch, Ingest, Next, Out};
use crate::status::{self, Outcome};
use crate::token::TokenState;

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
    /// The delay before a failed history request is tried again, doubling on every failure.
    pub catch_up_retry_min: Duration,
    /// The longest delay between tries of a history request.
    pub catch_up_retry_max: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            disconnect_grace: Duration::from_secs(10),
            reconnect_min: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(300),
            stable_after: Duration::from_secs(60),
            shutdown_wait: Duration::from_secs(5),
            catch_up_retry_min: Duration::from_secs(1),
            catch_up_retry_max: Duration::from_secs(60),
        }
    }
}

/// What the session hands to the channel's event stream.
#[derive(Debug)]
pub struct Outgoing {
    /// The event for the SDK, if there is one: a message or a status.
    pub event: Option<ChannelEvent>,
    /// Where the channel's cursor goes once the SDK has taken the event after this item, which
    /// it does after persisting this one: the next poll of the stream confirms it.
    pub cursor: Option<Cursor>,
}

/// What [`run`] works with.
pub struct Session {
    /// The connection.
    pub client: Arc<dyn Discord>,
    /// The delays.
    pub timing: Timing,
    /// Events for the SDK, by way of [`deliver`](crate::deliver), which downloads the files of a
    /// message into it.
    pub out: mpsc::Sender<Staged>,
    /// Cancelled when the channel is dropped or the plugin is stopping.
    pub stop: CancellationToken,
    /// What is delivered, and what is fetched.
    pub ingest: Ingest,
    /// Whether Discord has rejected the token: the session ends when a send says so, and says so
    /// to the sends when the Gateway or a history request does.
    pub token: Arc<TokenState>,
}

/// The history request in flight, and what it asked.
type Fetching = Pin<Box<dyn Future<Output = (Fetch, Result<Page, RestError>)> + Send>>;

/// Asks `client` for the page `fetch` describes.
fn fetching(client: Arc<dyn Discord>, fetch: Fetch) -> Fetching {
    Box::pin(async move {
        let page = client
            .messages_after(fetch.channel, fetch.after, fetch.limit)
            .await;
        (fetch, page)
    })
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
    /// The history request in flight has an answer.
    Fetched(Fetch, Result<Page, RestError>),
    /// A history request has waited out its retry delay.
    RetryDue,
    /// A send found the token rejected.
    Rejected,
}

/// Runs the connection until the channel is stopped: reports statuses, delivers messages and
/// fetches the history that catch-up needs, delays polling after a close, and closes the client
/// at the end.
pub async fn run(session: Session) {
    let Session {
        client,
        timing,
        out,
        stop,
        mut ingest,
        token,
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
    // The history request in flight (one at a time).
    let mut fetch: Option<Fetching> = None;
    // A final close, or the client ending: nothing more is polled.
    let mut finished = false;
    loop {
        let step = if finished {
            tokio::select! {
                biased;
                () = stop.cancelled() => Step::Stop,
                () = token.rejection() => Step::Rejected,
            }
        } else {
            // Start the next history request, or find out when one is due.
            let mut retry_at = None;
            if fetch.is_none() {
                match ingest.next(Instant::now()) {
                    Next::Now(request) => fetch = Some(fetching(Arc::clone(&client), request)),
                    Next::Wait(at) => retry_at = Some(at),
                    Next::Idle => {}
                }
            }
            let poll =
                pending.get_or_insert_with(|| Box::pin(poll_at(&*client, poll_after.take())));
            let grace_over = async {
                match dropped_at {
                    Some(at) => tokio::time::sleep_until(at + timing.disconnect_grace).await,
                    None => std::future::pending().await,
                }
            };
            let retry_due = async {
                match retry_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let fetched = async {
                match fetch.as_mut() {
                    Some(request) => request.await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = stop.cancelled() => Step::Stop,
                () = grace_over => Step::GraceOver,
                () = token.rejection() => Step::Rejected,
                (request, page) = fetched => Step::Fetched(request, page),
                () = retry_due => Step::RetryDue,
                event = poll.as_mut() => Step::Event(event),
            }
        };
        match step {
            Step::Stop => break,
            Step::GraceOver => {
                dropped_at = None;
                reporter
                    .report(if token.is_rejected() {
                        status::rejected_token()
                    } else {
                        status::disconnected()
                    })
                    .await;
            }
            Step::RetryDue => {}
            Step::Rejected => {
                // The same as a 401 on a history page: no more requests, every channel waiting
                // for one goes live, and the Gateway session goes on.
                fetch = None;
                let outs = ingest.on_token_rejected();
                tracing::error!("Discord rejected the bot token on a REST call");
                reporter.report(status::rejected_token()).await;
                reporter.emit(outs).await;
            }
            Step::Fetched(request, page) => {
                fetch = None;
                if matches!(page, Err(RestError::Unauthorized)) {
                    // The ingest reports the status and stops fetching; the sends stop too.
                    token.note_rejected();
                }
                let outs = ingest.on_page(request, page, Instant::now());
                reporter.emit(outs).await;
            }
            Step::Event(None) => {
                pending = None;
                finished = true;
                tracing::warn!("the Discord client ended its event stream");
                reporter.report(status::stopped()).await;
            }
            Step::Event(Some(GatewayEvent::Message(message))) => {
                pending = None;
                let outs = ingest.on_message(message);
                reporter.emit(outs).await;
            }
            Step::Event(Some(event)) => {
                pending = None;
                match status::classify(&event) {
                    Outcome::Up => {
                        let now = Instant::now();
                        up_since = Some(now);
                        dropped_at = None;
                        match event {
                            GatewayEvent::Ready { user_id } => {
                                tracing::info!("connected to Discord with a new session");
                                // A new session: what was sent while there was none is not
                                // replayed, so every channel is caught up over REST. A request
                                // of the session before is dropped.
                                fetch = None;
                                ingest.on_ready(user_id);
                            }
                            _ => tracing::info!("resumed the Discord session"),
                        }
                        // A rejected token (a 401 on REST) stays what the owner has to fix,
                        // however well the Gateway connection is doing.
                        reporter
                            .report(if token.is_rejected() {
                                status::rejected_token()
                            } else {
                                ChannelStatus::Connected
                            })
                            .await;
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
                        if status == status::rejected_token() {
                            // REST stops too: Discord asks for no more requests with a bad token.
                            token.note_rejected();
                        }
                        reporter.report(status).await;
                    }
                    Outcome::Unchanged => {}
                }
            }
        }
    }
    // The pending poll holds the client; end it before closing the connection. A history request
    // in flight is abandoned: nothing was handled of it, and the cursor didn't move.
    drop(pending);
    drop(fetch);
    if tokio::time::timeout(timing.shutdown_wait, client.shutdown())
        .await
        .is_err()
    {
        tracing::warn!("closing the Discord connection took too long");
    }
}

/// Sends what the session produces to the channel's event stream, and statuses among it,
/// skipping a repeat of the last status.
struct Reporter {
    out: mpsc::Sender<Staged>,
    stop: CancellationToken,
    last: Option<ChannelStatus>,
    counter: u64,
}

impl Reporter {
    fn new(out: mpsc::Sender<Staged>, stop: CancellationToken) -> Self {
        Self {
            out,
            stop,
            last: None,
            counter: 0,
        }
    }

    /// Hands over what [`Ingest`] says, in order. A full buffer (the SDK is persisting) holds the
    /// session until there is room, or until it is stopped.
    async fn emit(&mut self, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Deliver {
                    event,
                    files,
                    cursor,
                } => {
                    self.send(Staged {
                        outgoing: Outgoing {
                            event: Some(*event),
                            cursor: Some(cursor),
                        },
                        files,
                    })
                    .await;
                }
                Out::Advance(cursor) => {
                    self.send(
                        Outgoing {
                            event: None,
                            cursor: Some(cursor),
                        }
                        .into(),
                    )
                    .await;
                }
                Out::Status(status) => self.report(status).await,
            }
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
        self.send(
            Outgoing {
                event: Some(event),
                cursor: None,
            }
            .into(),
        )
        .await;
    }

    async fn send(&self, staged: Staged) {
        tokio::select! {
            () = self.stop.cancelled() => {}
            // Fails only once the channel is gone, when nobody needs it.
            _ = self.out.send(staged) => {}
        }
    }
}

#[cfg(test)]
mod messages;
#[cfg(test)]
mod tests;
