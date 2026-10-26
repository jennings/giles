//! The session against the fake client, on paused time: statuses, the grace period, reconnect
//! delays, final closes, and shutdown.

use std::time::Duration;

use futures::stream::{BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    AuthChallenge, Channel as _, ChannelEvent, ChannelStatus, Result,
};
use tokio::time::Instant;

use crate::channel::{DiscordChannel, shutdown_wait};
use crate::client::GatewayEvent;
use crate::fake::FakeDiscord;
use crate::session::Timing;

use std::sync::Arc;

const HOUR: Duration = Duration::from_secs(3600);

struct Harness {
    client: Arc<FakeDiscord>,
    channel: DiscordChannel,
    events: BoxStream<'static, Result<ChannelEvent>>,
}

impl Harness {
    async fn start() -> Self {
        let client = Arc::new(FakeDiscord::default());
        let channel = DiscordChannel::start(client.clone(), Timing::default());
        let events = channel.events().await.unwrap();
        Self {
            client,
            channel,
            events,
        }
    }

    /// The next status the channel reports. Waits up to an hour of (virtual) time.
    async fn status(&mut self) -> ChannelStatus {
        let event = tokio::time::timeout(HOUR, self.events.next())
            .await
            .expect("a status in time")
            .expect("the stream is open")
            .unwrap();
        match event {
            ChannelEvent::Status { status, .. } => status,
            other => panic!("not a status: {other:?}"),
        }
    }

    /// Asserts that nothing is reported for `wait`.
    async fn silent_for(&mut self, wait: Duration) {
        if let Ok(event) = tokio::time::timeout(wait, self.events.next()).await {
            panic!("reported {event:?}");
        }
    }

    /// Reports `Ready`, stays up for `up_for`, then drops the connection, and returns how long
    /// the client went unpolled afterwards (the reconnect delay). The session must already be
    /// waiting for an event.
    async fn flap(&self, up_for: Duration) -> Duration {
        let polled = self.client.polls().len();
        self.client.push(GatewayEvent::Ready { user_id: 7 });
        self.client.polled(polled + 1).await;
        tokio::time::sleep(up_for).await;
        let closed_at = Instant::now();
        self.client.push(GatewayEvent::Closed { code: Some(1006) });
        self.client.polled(polled + 2).await;
        self.client.polls()[polled + 1] - closed_at
    }
}

fn ready() -> GatewayEvent {
    GatewayEvent::Ready { user_id: 7 }
}

fn closed(code: u16) -> GatewayEvent {
    GatewayEvent::Closed { code: Some(code) }
}

fn is_error(status: &ChannelStatus, containing: &str) -> bool {
    matches!(status, ChannelStatus::Error { message } if message.contains(containing))
}

#[tokio::test(start_paused = true)]
async fn a_session_is_connected_and_a_repeat_isnt_reported() {
    let mut h = Harness::start().await;
    h.client.push(ready());
    assert_eq!(h.status().await, ChannelStatus::Connected);
    // A drop that is over before anyone needs to know, resumed or on a fresh session, changes
    // nothing the operator sees.
    h.client.push(closed(1006));
    h.client.push(GatewayEvent::Resumed);
    h.client.push(closed(4009));
    h.client.push(ready());
    h.silent_for(HOUR).await;
}

#[tokio::test(start_paused = true)]
async fn a_rejected_token_needs_auth_and_is_never_retried() {
    let mut h = Harness::start().await;
    h.client.push(closed(4004));
    assert_eq!(
        h.status().await,
        ChannelStatus::NeedsAuth {
            challenge: AuthChallenge::Url {
                url: "https://discord.com/developers/applications".into()
            },
            expires_at: None,
        }
    );
    // Final: no grace-period error on top of it, and the client is not polled again, which is
    // what keeps Discord from seeing a stream of bad Identifies.
    h.silent_for(HOUR).await;
    assert_eq!(h.client.polls().len(), 1);
    let handle = h.channel.take_session();
    handle.shutdown(shutdown_wait()).await;
    assert_eq!(h.client.shutdowns(), 1);

    // Restarted with a corrected token (a new plugin process, a new client): connected.
    let mut restarted = Harness::start().await;
    restarted.client.push(ready());
    assert_eq!(restarted.status().await, ChannelStatus::Connected);
}

#[tokio::test(start_paused = true)]
async fn closes_the_client_cannot_recover_from_are_final_errors() {
    for (code, needle) in [
        (4010, "close 4010"),
        (4011, "too many servers"),
        (4012, "close 4012"),
        (4013, "intents"),
        (4014, "intents"),
    ] {
        let mut h = Harness::start().await;
        h.client.push(closed(code));
        let status = h.status().await;
        assert!(is_error(&status, needle), "{code}: {status:?}");
        h.silent_for(HOUR).await;
        assert_eq!(h.client.polls().len(), 1, "{code}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_drop_is_reported_after_the_grace_period_and_cleared_when_it_ends() {
    let mut h = Harness::start().await;
    h.client.push(ready());
    assert_eq!(h.status().await, ChannelStatus::Connected);

    let dropped = Instant::now();
    h.client.push(closed(1006));
    let status = h.status().await;
    assert_eq!(dropped.elapsed(), Duration::from_secs(10));
    assert!(is_error(&status, "reconnecting"), "{status:?}");

    // Still down, and more drops: not reported again.
    h.client.push(closed(1006));
    h.silent_for(HOUR).await;

    h.client.push(GatewayEvent::Resumed);
    assert_eq!(h.status().await, ChannelStatus::Connected);
}

#[tokio::test(start_paused = true)]
async fn a_reconnect_inside_the_grace_period_is_not_reported() {
    let mut h = Harness::start().await;
    h.client.push(ready());
    assert_eq!(h.status().await, ChannelStatus::Connected);
    h.client.push(GatewayEvent::Closed { code: None });
    // The session waits 1 second, polls, and finds the session resumed.
    h.client.push(GatewayEvent::Resumed);
    h.silent_for(HOUR).await;
}

#[tokio::test(start_paused = true)]
async fn the_client_is_polled_after_a_delay_that_doubles_to_five_minutes() {
    let h = Harness::start().await;
    for _ in 0..12 {
        h.client.push(closed(1006));
    }
    h.client.polled(12).await;
    let polls = h.client.polls();
    let gaps: Vec<u64> = polls.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
    assert_eq!(gaps, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
}

#[tokio::test(start_paused = true)]
async fn the_delay_starts_over_only_after_a_session_stayed_up_for_a_minute() {
    let h = Harness::start().await;
    // The session is waiting for the first event.
    h.client.polled(1).await;
    let secs = |d: Duration| d.as_secs();
    // Up for less than a minute each time: the delay keeps doubling.
    assert_eq!(secs(h.flap(Duration::from_secs(30)).await), 1);
    assert_eq!(secs(h.flap(Duration::from_secs(59)).await), 2);
    assert_eq!(secs(h.flap(Duration::from_secs(10)).await), 4);
    // A full minute is healthy: back to the minimum, and doubling from there.
    assert_eq!(secs(h.flap(Duration::from_secs(60)).await), 1);
    assert_eq!(secs(h.flap(Duration::ZERO).await), 2);
}

#[tokio::test(start_paused = true)]
async fn an_invalid_session_loop_does_not_identify_in_a_tight_loop() {
    let h = Harness::start().await;
    // Discord answers every Identify with an invalid session, which the client turns into a
    // close with 1000 and a new Identify.
    for _ in 0..6 {
        h.client.push(ready());
        h.client.push(closed(1000));
    }
    h.client.polled(13).await;
    let polls = h.client.polls();
    let waits: Vec<u64> = polls
        .windows(2)
        .map(|w| (w[1] - w[0]).as_secs())
        .filter(|secs| *secs > 0)
        .collect();
    assert_eq!(waits, [1, 2, 4, 8, 16, 32]);
}

#[tokio::test(start_paused = true)]
async fn an_event_stream_that_ends_without_a_final_close_is_an_error() {
    let mut h = Harness::start().await;
    h.client.push(ready());
    assert_eq!(h.status().await, ChannelStatus::Connected);
    h.client.end();
    let status = h.status().await;
    assert!(is_error(&status, "won't be retried"), "{status:?}");
    h.silent_for(HOUR).await;
    // Polled for Ready, and once more to learn the stream ended; not again.
    assert_eq!(h.client.polls().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn stopping_closes_the_client_once_even_while_waiting_to_reconnect() {
    let mut h = Harness::start().await;
    h.client.push(ready());
    assert_eq!(h.status().await, ChannelStatus::Connected);
    h.client.push(closed(1006));
    h.client.polled(2).await;
    let stopped_at = Instant::now();
    h.channel.take_session().shutdown(shutdown_wait()).await;
    assert_eq!(h.client.shutdowns(), 1);
    // The reconnect delay doesn't hold the plugin up.
    assert_eq!(stopped_at.elapsed(), Duration::ZERO);
    assert_eq!(h.client.polls().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_client_that_hangs_while_closing_does_not_hang_the_plugin() {
    let h = Harness::start().await;
    h.client.hang_on_shutdown();
    h.client.polled(1).await;
    let started = Instant::now();
    h.channel.take_session().shutdown(shutdown_wait()).await;
    assert_eq!(h.client.shutdowns(), 1);
    assert_eq!(started.elapsed(), Timing::default().shutdown_wait);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_channel_closes_the_client() {
    let h = Harness::start().await;
    h.client.polled(1).await;
    let client = Arc::clone(&h.client);
    drop(h);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(client.shutdowns(), 1);
}
