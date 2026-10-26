//! The whole inbound path with the real client: a Gateway and a REST server of the test's own on
//! loopback, `RealDiscord` between them and the channel. Live messages come over the Gateway,
//! history over REST, paged by `after` as the requests say. Nothing here reaches Discord, and the
//! token is not one.
//!
//! The servers play Discord from its documentation, so these tests show that the pieces fit, not
//! that Discord behaves so: the `after` semantics in particular are for issue #20.

mod loopback;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{BoxStream, StreamExt as _};
use giles_channel_discord::channel::{DiscordChannel, Inbound, shutdown_wait};
use giles_channel_discord::fake::FakeMedia;
use giles_channel_discord::ingest::Clock;
use giles_channel_discord::real::RealDiscord;
use giles_channel_discord::session::Timing;
use giles_channel_discord::snowflake::at_unix_ms;
use giles_channel_discord::state::StateFile;
use giles_plugin_sdk::giles_types::{Channel as _, ChannelEvent, ChannelStatus, Result};
use loopback::rest::{Response, RestServer};
use loopback::{Frame, Gateway, WAIT, message_create, message_json, settings};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

const CHANNEL: u64 = 319_674_150_115_610_528;
const USER: u64 = 11_111_111_111_111_111;

/// The id of the nth message sent `minutes_ago` minutes before now.
fn id(minutes_ago: u64, n: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    at_unix_ms(u64::try_from(now).unwrap() - minutes_ago * 60_000) + n
}

/// Discord's answer to `GET /channels/{id}/messages?after=&limit=` from `history` (ascending by
/// id): the messages after `after`, up to `limit`, newest first.
fn history_server(
    history: Vec<(u64, Value)>,
) -> impl Fn(&loopback::rest::Request, usize) -> Response {
    move |request, _| {
        let after: u64 = request.query("after").unwrap().parse().unwrap();
        let limit: usize = request.query("limit").unwrap().parse().unwrap();
        let mut page: Vec<&Value> = history
            .iter()
            .filter(|(id, _)| *id > after)
            .take(limit)
            .map(|(_, message)| message)
            .collect();
        page.reverse();
        Response::json(200, &json!(page))
    }
}

/// A Gateway that identifies, says Ready, sends `live` as dispatches, and then answers
/// heartbeats until the client closes.
fn gateway_task(gateway: Gateway, live: Vec<Value>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut connection = gateway.accept(41_250).await;
        connection.expect_identify().await;
        connection.send(gateway.ready()).await;
        for (seq, message) in live.into_iter().enumerate() {
            connection
                .send(message_create(2 + u64::try_from(seq).unwrap(), message))
                .await;
        }
        loop {
            match connection.recv().await {
                Frame::Close(_) => break,
                Frame::Json(payload) if payload["op"] == 1 => {
                    connection.send(json!({"op": 11})).await;
                }
                Frame::Json(_) => {}
            }
        }
    })
}

struct Running {
    channel: DiscordChannel,
    events: BoxStream<'static, Result<ChannelEvent>>,
}

async fn start(
    gateway_url: &str,
    rest: &RestServer,
    dir: &Path,
    prepare: impl FnOnce(&StateFile),
) -> Running {
    let state = StateFile::open(dir);
    prepare(&state);
    let client = Arc::new(RealDiscord::with_urls(&settings(), gateway_url, &rest.host));
    let channel = DiscordChannel::start(
        client,
        Timing::default(),
        Inbound {
            state: state.clone(),
            catch_up_max_age: Duration::from_secs(24 * 3600),
            clock: Clock::system(),
            media: Arc::new(FakeMedia::default()),
            max_blob: 50 * 1024 * 1024,
        },
    );
    let events = channel.events().await.unwrap();
    Running { channel, events }
}

impl Running {
    async fn next(&mut self) -> ChannelEvent {
        tokio::time::timeout(WAIT, self.events.next())
            .await
            .expect("an event in time")
            .expect("the stream is open")
            .unwrap()
    }

    async fn status(&mut self) -> ChannelStatus {
        match self.next().await {
            ChannelEvent::Status { status, .. } => status,
            other => panic!("not a status: {other:?}"),
        }
    }

    async fn texts(&mut self, n: usize) -> Vec<String> {
        let mut texts = Vec::new();
        for _ in 0..n {
            match self.next().await {
                ChannelEvent::Message { message, .. } => texts.push(message.text),
                other => panic!("not a message: {other:?}"),
            }
        }
        texts
    }

    /// Polls the stream for `wait` and expects nothing: as for the SDK, this confirms the last
    /// event handed out.
    async fn quiet(&mut self, wait: Duration) {
        if let Ok(event) = tokio::time::timeout(wait, self.events.next()).await {
            panic!("handed over {event:?}");
        }
    }

    async fn stop(self) {
        self.channel.take_session().shutdown(shutdown_wait()).await;
    }
}

#[tokio::test]
async fn history_over_rest_and_live_messages_over_the_gateway_meet_without_a_gap_or_a_repeat() {
    let gateway = Gateway::bind().await;
    let history: Vec<(u64, Value)> = (1..=150)
        .map(|n| {
            let message_id = id(60, n);
            (
                message_id,
                message_json(message_id, CHANNEL, USER, &format!("m{n}")),
            )
        })
        .collect();
    let last = history[149].clone();
    let live_new = id(1, 1);
    let rest = RestServer::start(history_server(history.clone())).await;
    // After Ready the Gateway sends the last message of the history again (it was sent while the
    // session was being set up) and one that is newer than the history.
    let url = gateway.url.clone();
    let server = gateway_task(
        gateway,
        vec![
            last.1.clone(),
            message_json(live_new, CHANNEL, USER, "live"),
        ],
    );
    let dir = tempfile::tempdir().unwrap();
    let cursor = id(120, 1);
    let mut running = start(&url, &rest, dir.path(), |state| {
        state.record_dm(CHANNEL, USER);
        state.advance(CHANNEL, cursor);
    })
    .await;

    assert_eq!(running.status().await, ChannelStatus::Connected);
    let texts = running.texts(151).await;
    let mut expected: Vec<String> = (1..=150).map(|n| format!("m{n}")).collect();
    expected.push("live".into());
    assert_eq!(texts, expected, "each message once, in order");
    running.quiet(Duration::from_millis(500)).await;

    // The history was fetched in two pages, each after the last message of the one before, the
    // first after the cursor; and nothing else.
    let requests = rest.requests();
    let afters: Vec<Option<&str>> = requests.iter().map(|r| r.query("after")).collect();
    assert_eq!(
        afters,
        [
            Some(cursor.to_string().as_str()),
            Some(history[99].0.to_string().as_str())
        ]
    );
    assert!(requests.iter().all(|r| r.query("limit") == Some("100")));
    assert!(requests.iter().all(|r| {
        r.target
            .starts_with(&format!("/api/v10/channels/{CHANNEL}/messages?"))
    }));
    // The cursor is on disk once the SDK has taken everything.
    assert_eq!(
        StateFile::open(dir.path()).get(CHANNEL).unwrap().cursor,
        Some(live_new)
    );
    running.stop().await;
    server.await.unwrap();
}

#[tokio::test]
async fn a_rejected_token_in_catch_up_is_needs_auth_and_live_messages_still_arrive() {
    let gateway = Gateway::bind().await;
    let rest = RestServer::start(|_, _| {
        Response::json(401, &json!({"message": "401: Unauthorized", "code": 0}))
    })
    .await;
    let url = gateway.url.clone();
    let server = gateway_task(gateway, vec![message_json(id(1, 1), CHANNEL, USER, "live")]);
    let dir = tempfile::tempdir().unwrap();
    let mut running = start(&url, &rest, dir.path(), |state| {
        state.record_dm(CHANNEL, USER);
    })
    .await;

    assert_eq!(running.status().await, ChannelStatus::Connected);
    assert!(matches!(
        running.status().await,
        ChannelStatus::NeedsAuth { .. }
    ));
    assert_eq!(running.texts(1).await, ["live"]);
    // Discord was asked once, and not again.
    assert_eq!(rest.requests().len(), 1);
    running.stop().await;
    server.await.unwrap();
}
