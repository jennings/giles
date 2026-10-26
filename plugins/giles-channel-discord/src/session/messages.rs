//! Messages through the whole channel against the fake client, on paused time: what reaches the
//! SDK, in what order, when the cursor moves, and what catch-up asks for and when.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Channel as _, ChannelEvent, ChannelStatus, ConversationKind, InboundMessage, Result,
};

use crate::channel::{DiscordChannel, Inbound, shutdown_wait};
use crate::client::{DiscordMessage, GatewayEvent, RestError};
use crate::fake::{FakeDiscord, FakeMedia, direct_message};
use crate::ingest::Clock;
use crate::session::Timing;
use crate::snowflake::at_unix_ms;
use crate::state::StateFile;

/// 2026-10-24T12:00:00Z: the clock of every test (the paused Tokio clock is separate).
const NOW: u64 = 1_792_843_200_000;
const HOUR_MS: u64 = 3_600_000;
const DAY: Duration = Duration::from_secs(24 * 3600);
const HOUR: Duration = Duration::from_secs(3600);
const BOT: u64 = 900;
const USER: u64 = 11;
const CHANNEL: u64 = 5000;
const OTHER_USER: u64 = 12;
const OTHER_CHANNEL: u64 = 6000;
/// `max_blob` of every test channel.
const MAX_BLOB: u64 = 1024 * 1024;

mod attachments;

/// A message id created `ms_ago` before [`NOW`]; `n` orders ids of one millisecond.
fn id(ms_ago: u64, n: u64) -> u64 {
    at_unix_ms(NOW - ms_ago) + n
}

fn dm(ms_ago: u64, text: &str) -> DiscordMessage {
    direct_message(id(ms_ago, 1), CHANNEL, USER, text)
}

fn dm_in(channel: u64, user: u64, ms_ago: u64, text: &str) -> DiscordMessage {
    direct_message(id(ms_ago, 1), channel, user, text)
}

struct Rig {
    client: Arc<FakeDiscord>,
    channel: DiscordChannel,
    events: BoxStream<'static, Result<ChannelEvent>>,
    state: StateFile,
    media: Arc<FakeMedia>,
    /// Events pushed into the fake Gateway so far.
    pushed: usize,
}

impl Rig {
    /// A channel over a fresh data directory.
    async fn start(dir: &Path) -> Self {
        Self::start_with(dir, Arc::new(FakeDiscord::default()), DAY).await
    }

    async fn start_with(dir: &Path, client: Arc<FakeDiscord>, max_age: Duration) -> Self {
        let state = StateFile::open(dir);
        let media = Arc::new(FakeMedia::default());
        let channel = DiscordChannel::start(
            client.clone(),
            Timing::default(),
            Inbound {
                state: state.clone(),
                catch_up_max_age: max_age,
                clock: Clock::new(|| NOW),
                media: media.clone(),
                max_blob: MAX_BLOB,
            },
        );
        let events = channel.events().await.unwrap();
        Self {
            client,
            channel,
            events,
            state,
            media,
            pushed: 0,
        }
    }

    /// The next thing the channel hands the SDK. Waits up to an hour of (virtual) time.
    async fn next(&mut self) -> ChannelEvent {
        tokio::time::timeout(HOUR, self.events.next())
            .await
            .expect("an event in time")
            .expect("the stream is open")
            .unwrap()
    }

    async fn message(&mut self) -> InboundMessage {
        match self.next().await {
            ChannelEvent::Message { message, .. } => message,
            other => panic!("not a message: {other:?}"),
        }
    }

    /// The texts of the next `n` messages.
    async fn texts(&mut self, n: usize) -> Vec<String> {
        let mut texts = Vec::new();
        for _ in 0..n {
            texts.push(self.message().await.text);
        }
        texts
    }

    async fn status(&mut self) -> ChannelStatus {
        match self.next().await {
            ChannelEvent::Status { status, .. } => status,
            other => panic!("not a status: {other:?}"),
        }
    }

    /// Has the fake Gateway report `event`.
    fn push(&mut self, event: GatewayEvent) {
        self.pushed += 1;
        self.client.push(event);
    }

    /// Has the fake Gateway report a new session of the bot.
    fn ready(&mut self) {
        self.push(GatewayEvent::Ready { user_id: BOT });
    }

    /// Asserts that the channel hands over nothing for `wait`. This polls the stream, which, as for
    /// the SDK, confirms the event handed out last.
    async fn silent_for(&mut self, wait: Duration) {
        if let Ok(event) = tokio::time::timeout(wait, self.events.next()).await {
            panic!("handed over {event:?}");
        }
    }

    fn cursor(&self, channel: u64) -> Option<u64> {
        self.state.get(channel).and_then(|dm| dm.cursor)
    }

    /// Waits until the session has handled every event pushed so far and asks for the next one.
    async fn settled(&self) {
        self.client.polled(self.pushed + 1).await;
    }
}

fn is_connected(status: &ChannelStatus) -> bool {
    *status == ChannelStatus::Connected
}

#[tokio::test(start_paused = true)]
async fn a_dm_reaches_the_sdk_as_an_inbound_event() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::start(dir.path()).await;
    rig.ready();
    assert!(is_connected(&rig.status().await));

    let mut sent = dm(1000, "hello there");
    sent.author.global_name = Some("Alice".into());
    rig.push(GatewayEvent::Message(sent.clone()));
    let event = rig.next().await;
    let ChannelEvent::Message {
        id: event_id,
        message,
    } = event
    else {
        panic!("not a message: {event:?}");
    };

    assert_eq!(
        event_id.as_str(),
        sent.id.to_string(),
        "event id is the message id"
    );
    assert_eq!(message.text, "hello there");
    assert_eq!(message.sender.as_str(), USER.to_string());
    assert!(message.sender_authenticated);
    assert_eq!(message.sender_display_name.as_deref(), Some("Alice"));
    assert_eq!(message.conversation.id.as_str(), CHANNEL.to_string());
    assert_eq!(message.conversation.kind, ConversationKind::Direct);
    assert_eq!(message.message_id.unwrap().as_str(), sent.id.to_string());
    // The channel was new, so its history was asked for before this was delivered: the age
    // limit's first id, the page size.
    let calls = rig.client.history_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].channel, calls[0].after, calls[0].limit),
        (CHANNEL, at_unix_ms(NOW - 24 * HOUR_MS), 100)
    );
}

#[tokio::test(start_paused = true)]
async fn the_cursor_moves_when_the_stream_is_polled_again_and_not_before() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::start(dir.path()).await;
    rig.ready();
    rig.status().await;
    let first = dm(2000, "one");
    let second = dm(1000, "two");
    rig.push(GatewayEvent::Message(first.clone()));
    rig.push(GatewayEvent::Message(second.clone()));

    assert_eq!(rig.message().await.text, "one");
    // The SDK has the event but hasn't said it is on disk: not even a cursor to a channel that
    // was learned from it can be ahead of the SDK.
    rig.settled().await;
    assert_eq!(rig.cursor(CHANNEL), None);
    assert_eq!(rig.message().await.text, "two");
    // Asking for the event after "two" is the SDK saying "one" is persisted, and not before.
    assert_eq!(rig.cursor(CHANNEL), Some(first.id));
    rig.silent_for(Duration::from_secs(1)).await;
    assert_eq!(rig.cursor(CHANNEL), Some(second.id));
    // And it is on disk for the next process.
    assert_eq!(
        StateFile::open(dir.path()).get(CHANNEL).unwrap().cursor,
        Some(second.id)
    );
}

#[tokio::test(start_paused = true)]
async fn a_message_the_sdk_never_persisted_does_not_move_the_cursor() {
    // The SDK takes an event and then fails to write it: it never asks for the next one, and its
    // pump is dropped when the plugin goes down. Nothing may be recorded as received.
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::start(dir.path()).await;
    rig.ready();
    rig.status().await;
    let message = dm(1000, "never persisted");
    rig.push(GatewayEvent::Message(message.clone()));
    assert_eq!(rig.message().await.text, "never persisted");
    rig.settled().await;
    drop(std::mem::replace(
        &mut rig.events,
        futures::stream::empty().boxed(),
    ));
    // The channel lives on and learned the user, but the cursor is where it was: after a restart
    // the message is fetched again.
    rig.settled().await;
    assert_eq!(rig.cursor(CHANNEL), None);
    assert_eq!(
        StateFile::open(dir.path()).get(CHANNEL).unwrap().user,
        Some(USER)
    );
    drop(rig);

    let client = Arc::new(FakeDiscord::default());
    client.add_history(message.clone());
    let mut rig = Rig::start_with(dir.path(), client, DAY).await;
    rig.ready();
    rig.status().await;
    let again = rig.message().await;
    assert_eq!(again.message_id.unwrap().as_str(), message.id.to_string());
}

#[tokio::test(start_paused = true)]
async fn nothing_is_delivered_twice_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let history = [dm(3000, "one"), dm(2000, "two")];
    {
        let client = Arc::new(FakeDiscord::default());
        for message in &history {
            client.add_history(message.clone());
        }
        let mut rig = Rig::start_with(dir.path(), client, DAY).await;
        // The channel is known from an earlier life, so Ready alone catches it up.
        rig.state.record_dm(CHANNEL, USER);
        rig.ready();
        rig.status().await;
        assert_eq!(rig.texts(2).await, ["one", "two"]);
        // The SDK persists both, and asks for more.
        rig.silent_for(Duration::from_secs(1)).await;
        assert_eq!(rig.cursor(CHANNEL), Some(history[1].id));
    }

    // A new process, Discord's history unchanged, and one more message in it.
    let client = Arc::new(FakeDiscord::default());
    for message in &history {
        client.add_history(message.clone());
    }
    client.add_history(dm(1000, "three"));
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.ready();
    rig.status().await;
    assert_eq!(rig.texts(1).await, ["three"]);
    rig.silent_for(HOUR).await;
    // It asked for the page after the cursor, not after the age limit.
    assert_eq!(client.history_calls()[0].after, history[1].id);
}

#[tokio::test(start_paused = true)]
async fn a_bot_message_is_dropped_and_the_cursor_passes_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::start(dir.path()).await;
    rig.state.record_dm(CHANNEL, USER);
    rig.ready();
    rig.status().await;
    rig.settled().await;
    let mut own = dm(2000, "my reply");
    own.author.id = BOT;
    own.author.bot = true;
    let mut other_bot = dm(1500, "beep");
    other_bot.author.id = 777;
    other_bot.author.bot = true;
    let person = dm(1000, "question");
    for message in [&own, &other_bot, &person] {
        rig.push(GatewayEvent::Message((*message).clone()));
    }
    assert_eq!(rig.texts(1).await, ["question"]);
    rig.silent_for(Duration::from_secs(1)).await;
    assert_eq!(rig.cursor(CHANNEL), Some(person.id));
}

#[tokio::test(start_paused = true)]
async fn a_bot_message_alone_moves_the_cursor_once_the_stream_is_polled() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::start(dir.path()).await;
    rig.state.record_dm(CHANNEL, USER);
    rig.ready();
    rig.status().await;
    rig.settled().await;
    let mut own = dm(2000, "my reply");
    own.author.id = BOT;
    own.author.bot = true;
    rig.push(GatewayEvent::Message(own.clone()));
    rig.silent_for(Duration::from_secs(1)).await;
    assert_eq!(rig.cursor(CHANNEL), Some(own.id));
}

#[tokio::test(start_paused = true)]
async fn server_messages_are_not_reported_and_teach_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::start(dir.path()).await;
    rig.ready();
    rig.status().await;
    let mut guild = dm(1000, "in a server");
    guild.guild = true;
    rig.push(GatewayEvent::Message(guild));
    rig.silent_for(HOUR).await;
    assert_eq!(rig.state.channels(), Vec::<u64>::new());
    assert!(rig.client.history_calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_empty_dm_is_not_delivered_and_three_in_a_row_are_an_error_until_text_comes() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::start(dir.path()).await;
    rig.state.record_dm(CHANNEL, USER);
    rig.ready();
    assert!(is_connected(&rig.status().await));
    rig.settled().await;
    rig.push(GatewayEvent::Message(dm(5000, "")));
    rig.push(GatewayEvent::Message(dm(4000, "")));
    rig.silent_for(HOUR).await;
    rig.push(GatewayEvent::Message(dm(3000, "")));
    match rig.status().await {
        ChannelStatus::Error { message } => assert!(message.contains("no text"), "{message}"),
        other => panic!("{other:?}"),
    }
    // A fourth empty message doesn't say it again: the next thing the SDK hears is the recovery.
    rig.push(GatewayEvent::Message(dm(2500, "")));
    rig.push(GatewayEvent::Message(dm(2000, "hi")));
    assert_eq!(rig.texts(1).await, ["hi"]);
    assert!(is_connected(&rig.status().await));
}

// --- catch-up ---

#[tokio::test(start_paused = true)]
async fn a_new_session_catches_up_every_known_channel_in_order_from_its_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    for (ago, text) in [(5000, "seen"), (4000, "missed one"), (3000, "missed two")] {
        client.add_history(dm(ago, text));
    }
    client.add_history(dm_in(OTHER_CHANNEL, OTHER_USER, 3500, "other missed"));
    // The state of an earlier life: "seen" was the last message handled in CHANNEL.
    {
        let state = StateFile::open(dir.path());
        state.record_dm(CHANNEL, USER);
        state.advance(CHANNEL, id(5000, 1));
        state.record_dm(OTHER_CHANNEL, OTHER_USER);
    }
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.ready();
    assert!(is_connected(&rig.status().await));
    assert_eq!(
        rig.texts(3).await,
        ["missed one", "missed two", "other missed"]
    );
    let calls = client.history_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!((calls[0].channel, calls[0].after), (CHANNEL, id(5000, 1)));
    // No cursor for the other channel: the age limit's first id.
    assert_eq!(
        (calls[1].channel, calls[1].after),
        (OTHER_CHANNEL, at_unix_ms(NOW - 24 * HOUR_MS))
    );
}

#[tokio::test(start_paused = true)]
async fn a_resumed_session_does_not_catch_up_and_a_new_one_after_it_does() {
    let dir = tempfile::tempdir().unwrap();
    let mut rig = Rig::start(dir.path()).await;
    rig.state.record_dm(CHANNEL, USER);
    rig.ready();
    rig.status().await;
    rig.settled().await;
    assert_eq!(rig.client.history_calls().len(), 1);

    // A drop, and a resume: Discord replays the gap itself.
    rig.push(GatewayEvent::Closed { code: Some(1006) });
    rig.push(GatewayEvent::Resumed);
    rig.silent_for(HOUR).await;
    assert_eq!(rig.client.history_calls().len(), 1, "no pass after Resumed");
    // A replayed message is delivered once.
    let message = dm(1000, "replayed");
    rig.push(GatewayEvent::Message(message.clone()));
    rig.push(GatewayEvent::Message(message));
    assert_eq!(rig.texts(1).await, ["replayed"]);
    rig.silent_for(HOUR).await;

    // The session was lost (4009: timed out), so the next is a new one.
    rig.push(GatewayEvent::Closed { code: Some(4009) });
    rig.ready();
    rig.silent_for(HOUR).await;
    let calls = rig.client.history_calls();
    assert_eq!(calls.len(), 2, "a pass after Ready");
    // From after what was handled, which the cursor knows.
    assert_eq!(calls[1].after, id(1000, 1));
}

#[tokio::test(start_paused = true)]
async fn a_live_message_during_a_pass_is_delivered_once_and_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    client.add_history(dm(4000, "a"));
    client.add_history(dm(3000, "b"));
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.state.record_dm(CHANNEL, USER);
    client.pause_history();
    rig.ready();
    assert!(is_connected(&rig.status().await));
    client.history_called(1).await;

    // While the page is held: "b" arrives live too (it is in the history), and so does "c".
    rig.push(GatewayEvent::Message(dm(3000, "b")));
    rig.push(GatewayEvent::Message(dm(2000, "c")));
    rig.settled().await;
    rig.silent_for(Duration::from_secs(1)).await;
    client.resume_history();
    assert_eq!(rig.texts(3).await, ["a", "b", "c"]);
    rig.silent_for(HOUR).await;
}

#[tokio::test(start_paused = true)]
async fn a_full_page_is_followed_by_the_next_after_its_last_message() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    for n in 1..=130 {
        client.add_history(direct_message(
            id(HOUR_MS, n),
            CHANNEL,
            USER,
            &format!("m{n}"),
        ));
    }
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.state.record_dm(CHANNEL, USER);
    rig.ready();
    rig.status().await;
    let texts = rig.texts(130).await;
    assert_eq!(texts.first().map(String::as_str), Some("m1"));
    assert_eq!(texts.last().map(String::as_str), Some("m130"));
    let calls = client.history_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].after, id(HOUR_MS, 100));
    assert_eq!((calls[0].limit, calls[1].limit), (100, 100));
}

#[tokio::test(start_paused = true)]
async fn catch_up_is_off_with_a_zero_age_limit() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    client.add_history(dm(5000, "missed"));
    let mut rig = Rig::start_with(dir.path(), client.clone(), Duration::ZERO).await;
    rig.state.record_dm(CHANNEL, USER);
    rig.ready();
    rig.status().await;
    rig.push(GatewayEvent::Message(dm(1000, "live")));
    assert_eq!(rig.texts(1).await, ["live"]);
    rig.silent_for(HOUR).await;
    assert!(client.history_calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failed_page_is_tried_again_with_growing_delays_and_live_channels_flow_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    client.add_history(dm(4000, "missed"));
    client.fail_history(RestError::TimedOut);
    client.fail_history(RestError::Transport);
    client.fail_history(RestError::Api {
        status: 502,
        code: None,
    });
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.state.record_dm(CHANNEL, USER);
    rig.state.record_dm(OTHER_CHANNEL, OTHER_USER);
    rig.ready();
    assert!(is_connected(&rig.status().await));
    // CHANNEL's first request failed; OTHER_CHANNEL's went next, and it is live. (The failures
    // are used in order, so the first request of each pass fails until they run out.)
    client.history_called(1).await;
    rig.silent_for(Duration::from_millis(100)).await;
    rig.push(GatewayEvent::Message(dm_in(
        OTHER_CHANNEL,
        OTHER_USER,
        500,
        "live elsewhere",
    )));
    // OTHER_CHANNEL's pass hasn't succeeded yet either (two failures queued behind): the live
    // message arrives when it does, and CHANNEL's "missed" after its own retries succeed.
    let mut texts = rig.texts(2).await;
    texts.sort();
    assert_eq!(texts, ["live elsewhere", "missed"]);

    let calls = client.history_calls();
    // Every failure of a channel was followed by its retry after a longer delay than the last.
    let mut by_channel = std::collections::HashMap::<u64, Vec<tokio::time::Instant>>::new();
    for call in &calls {
        by_channel.entry(call.channel).or_default().push(call.at);
    }
    for (channel, times) in &by_channel {
        let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps.windows(2).all(|w| w[1] >= w[0]),
            "channel {channel}: retry gaps {gaps:?} don't grow"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn retries_wait_one_two_four_seconds_up_to_a_minute() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    for _ in 0..9 {
        client.fail_history(RestError::Transport);
    }
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.state.record_dm(CHANNEL, USER);
    rig.ready();
    rig.status().await;
    client.history_called(10).await;
    let times: Vec<_> = client.history_calls().iter().map(|call| call.at).collect();
    let gaps: Vec<u64> = times.windows(2).map(|w| (w[1] - w[0]).as_secs()).collect();
    assert_eq!(gaps, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
}

#[tokio::test(start_paused = true)]
async fn a_rejected_token_during_catch_up_needs_auth_and_live_messages_still_flow() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    client.fail_history(RestError::Unauthorized);
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.state.record_dm(CHANNEL, USER);
    client.pause_history();
    rig.ready();
    assert!(is_connected(&rig.status().await));
    client.history_called(1).await;
    rig.push(GatewayEvent::Message(dm(1000, "live while waiting")));
    rig.settled().await;
    client.resume_history();
    assert!(matches!(
        rig.status().await,
        ChannelStatus::NeedsAuth { .. }
    ));
    // The sends stop too: a 401 on any REST call means the token is rejected.
    assert!(rig.channel.token().is_rejected());
    assert_eq!(rig.texts(1).await, ["live while waiting"]);
    // No more requests for anything, including a channel seen for the first time.
    rig.push(GatewayEvent::Message(dm_in(
        OTHER_CHANNEL,
        OTHER_USER,
        500,
        "new",
    )));
    assert_eq!(rig.texts(1).await, ["new"]);
    assert_eq!(client.history_calls().len(), 1);

    // The Gateway drops and a new session starts: the status stays NeedsAuth (nothing is
    // announced), no history is asked for, and messages still come live.
    rig.push(GatewayEvent::Closed { code: Some(1006) });
    rig.ready();
    rig.silent_for(Duration::from_secs(120)).await;
    rig.push(GatewayEvent::Message(dm(900, "after the reconnect")));
    assert_eq!(rig.texts(1).await, ["after the reconnect"]);
    assert_eq!(client.history_calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_rejected_token_found_by_a_send_stops_catch_up_but_live_messages_still_flow() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    client.add_history(dm(4000, "never delivered"));
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.state.record_dm(CHANNEL, USER);
    client.pause_history();
    rig.ready();
    assert!(is_connected(&rig.status().await));
    client.history_called(1).await;
    rig.push(GatewayEvent::Message(dm(5000, "live while waiting")));
    rig.settled().await;

    // A send gets a 401 while the history request is in flight, as a 401 on the page would: the
    // request is dropped, nothing more is asked of Discord, and what was held back is delivered.
    rig.channel.token().reject();
    assert!(matches!(
        rig.status().await,
        ChannelStatus::NeedsAuth { .. }
    ));
    assert_eq!(rig.texts(1).await, ["live while waiting"]);
    client.resume_history();
    rig.settled().await;
    assert_eq!(client.history_calls().len(), 1);
    assert!(rig.channel.token().is_rejected());

    // The Gateway session goes on: a reconnect doesn't announce `Connected` over the NeedsAuth,
    // and messages still arrive.
    rig.push(GatewayEvent::Closed { code: Some(1006) });
    rig.ready();
    rig.silent_for(Duration::from_secs(120)).await;
    rig.push(GatewayEvent::Message(dm(4900, "after the reconnect")));
    assert_eq!(rig.texts(1).await, ["after the reconnect"]);
    assert_eq!(client.history_calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn stopping_during_catch_up_stops_and_closes_the_client() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    client.add_history(dm(4000, "never delivered"));
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.state.record_dm(CHANNEL, USER);
    client.pause_history();
    rig.ready();
    rig.status().await;
    client.history_called(1).await;

    let handle = rig.channel.take_session();
    handle.shutdown(shutdown_wait()).await;
    assert_eq!(client.shutdowns(), 1);
    // The stream ends, with nothing delivered and nothing recorded.
    assert!(rig.events.next().await.is_none());
    assert_eq!(rig.cursor(CHANNEL), None);
}

#[tokio::test(start_paused = true)]
async fn a_pass_in_progress_when_the_session_drops_does_not_hold_up_live_messages() {
    let dir = tempfile::tempdir().unwrap();
    let client = Arc::new(FakeDiscord::default());
    let mut rig = Rig::start_with(dir.path(), client.clone(), DAY).await;
    rig.state.record_dm(CHANNEL, USER);
    client.pause_history();
    rig.ready();
    rig.status().await;
    client.history_called(1).await;
    // The connection drops and comes back with a new session while the request hangs: the
    // request of the old session is abandoned and the pass asked again.
    rig.push(GatewayEvent::Closed { code: Some(1006) });
    rig.ready();
    client.history_called(2).await;
    client.resume_history();
    rig.push(GatewayEvent::Message(dm(1000, "after")));
    assert_eq!(rig.texts(1).await, ["after"]);
}
