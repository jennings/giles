//! Sending against the fake client, on paused time: rendering and splitting, the order of the
//! parts, recipients (opened once, remembered in the state file, refused), what each Discord error
//! means, a failure after some parts went out, and a rejected token.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    AuthChallenge, Channel as _, ChannelEvent, ChannelStatus, ConversationId, Error, MessageId,
    NativeId, OutboundMessage, Result, SendTarget,
};

use crate::channel::{DiscordChannel, Inbound};
use crate::client::{GatewayEvent, RestError};
use crate::fake::{FakeDiscord, FakeMedia, dm_channel_of};
use crate::ingest::Clock;
use crate::session::Timing;
use crate::state::StateFile;

const HOUR: Duration = Duration::from_secs(3600);
const USER: u64 = 82_198_898_841_029_460;
const DM: u64 = dm_channel_of(USER);
/// A channel id for replies into a conversation.
const CHANNEL: u64 = 319_674_150_115_610_528;

struct Harness {
    client: Arc<FakeDiscord>,
    channel: DiscordChannel,
    events: BoxStream<'static, Result<ChannelEvent>>,
    state: StateFile,
    _dir: Option<tempfile::TempDir>,
}

impl Harness {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = Self::start_in(dir.path()).await;
        harness._dir = Some(dir);
        harness
    }

    /// A channel over the data directory `dir`, which may hold the state of an earlier one.
    async fn start_in(dir: &Path) -> Self {
        let client = Arc::new(FakeDiscord::default());
        let state = StateFile::open(dir);
        let channel = DiscordChannel::start(
            client.clone(),
            Timing::default(),
            Inbound {
                state: state.clone(),
                catch_up_max_age: HOUR * 24,
                clock: Clock::system(),
                media: Arc::new(FakeMedia::default()),
                max_blob: 50 * 1024 * 1024,
            },
        );
        let events = channel.events().await.unwrap();
        Self {
            client,
            channel,
            events,
            state,
            _dir: None,
        }
    }

    async fn send(&self, target: SendTarget, markdown: &str) -> Result<MessageId> {
        self.channel
            .send(OutboundMessage {
                target,
                markdown: markdown.to_owned(),
                attachments: Vec::new(),
                reply_to: None,
            })
            .await
    }

    async fn reply(&self, markdown: &str) -> Result<MessageId> {
        self.send(conversation(CHANNEL), markdown).await
    }

    async fn to_user(&self, recipient: &str, markdown: &str) -> Result<MessageId> {
        self.send(recipient_of(recipient), markdown).await
    }

    /// A send to the user `USER`, which the outbox retries (unlike a reply).
    async fn to_dm(&self, markdown: &str) -> Result<MessageId> {
        self.to_user(&USER.to_string(), markdown).await
    }

    /// The next status the channel reports.
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
}

fn conversation(id: u64) -> SendTarget {
    SendTarget::Conversation(ConversationId::new(id.to_string()).unwrap())
}

fn recipient_of(recipient: &str) -> SendTarget {
    SendTarget::Recipient(NativeId::new(recipient).unwrap())
}

fn api(status: u16, code: u32) -> RestError {
    RestError::Api {
        status,
        code: Some(code),
    }
}

fn unavailable(error: &Error) -> &str {
    match error {
        Error::Unavailable { message, .. } => message,
        other => panic!("not unavailable: {other:?}"),
    }
}

fn needs_auth() -> ChannelStatus {
    ChannelStatus::NeedsAuth {
        challenge: AuthChallenge::Url {
            url: "https://discord.com/developers/applications".into(),
        },
        expires_at: None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_reply_is_rendered_and_posted_to_its_channel() {
    let h = Harness::start().await;
    let id = h
        .reply("Hello *there*, see [the docs](https://example.com)")
        .await
        .unwrap();
    assert_eq!(
        h.client.messages(),
        [(
            CHANNEL,
            "Hello *there*, see [the docs](<https://example.com>)".to_owned()
        )]
    );
    // The id Discord gave the message.
    assert_eq!(id.as_str(), "5001");
    assert!(h.client.dms().is_empty(), "no DM channel to open");
}

#[tokio::test(start_paused = true)]
async fn long_text_is_split_at_discords_limit_and_posted_in_order() {
    let h = Harness::start().await;
    let text = "lorem ipsum ".repeat(500);
    let id = h.reply(&text).await.unwrap();

    let sent = h.client.messages();
    assert!(sent.len() >= 3, "{} messages", sent.len());
    assert!(sent.iter().all(|(channel, _)| *channel == CHANNEL));
    assert!(
        sent.iter().all(|(_, text)| text.chars().count() <= 2000),
        "a part is over the limit"
    );
    let words: Vec<_> = sent
        .iter()
        .flat_map(|(_, text)| text.split_whitespace())
        .collect();
    assert_eq!(words.len(), 1000, "no text lost or repeated");
    assert!(words.chunks(2).all(|pair| pair == ["lorem", "ipsum"]));
    // The id of the last message, which is the last one Discord was asked to create.
    assert_eq!(id.as_str(), (5_000 + sent.len()).to_string());
}

#[tokio::test(start_paused = true)]
async fn a_mention_in_the_text_is_defused() {
    let h = Harness::start().await;
    h.reply("@everyone @here <@82198898841029460> <@&5> <#6> `@everyone`")
        .await
        .unwrap();
    let (_, text) = &h.client.messages()[0];
    for live in [
        "@everyone",
        "@here",
        "<@82198898841029460>",
        "<@&5>",
        "<#6>",
    ] {
        assert!(!text.contains(live), "{live} is live in {text:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn nothing_to_send_and_attachments_are_refused_before_anything_is_sent() {
    let h = Harness::start().await;
    for empty in ["", "  \n\t\n "] {
        let error = h.reply(empty).await.unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }
    let media = giles_plugin_sdk::giles_types::MediaRef {
        id: giles_plugin_sdk::giles_types::MediaId::from_digest([7; 32]),
        media_type: "image/png".into(),
        name: None,
        size: 1,
    };
    let error = h
        .channel
        .send(OutboundMessage {
            target: conversation(CHANNEL),
            markdown: "see".into(),
            attachments: vec![media],
            reply_to: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    assert!(h.client.messages().is_empty() && h.client.dms().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_recipient_gets_a_dm_channel_that_is_opened_once() {
    let h = Harness::start().await;
    // The core strips the namespace; the prefix is accepted all the same.
    h.to_user(&USER.to_string(), "one").await.unwrap();
    h.to_user(&format!("discord:{USER}"), "two").await.unwrap();
    h.reply("elsewhere").await.unwrap();
    assert_eq!(h.client.dms(), [USER], "one POST /users/@me/channels");
    assert_eq!(
        h.client.messages(),
        [
            (DM, "one".to_owned()),
            (DM, "two".to_owned()),
            (CHANNEL, "elsewhere".to_owned())
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_user_who_cant_be_messaged_is_unavailable_and_asked_again_next_time() {
    for (code, status, needle) in [
        (50_007, 403, "50007"),
        (50_278, 403, "50278"),
        (10_013, 404, "10013"),
        (40_003, 400, "40003"),
    ] {
        let h = Harness::start().await;
        h.client.script_dms([Err(api(status, code))]);
        let error = h.to_user(&USER.to_string(), "hi").await.unwrap_err();
        assert!(error.is_retryable(), "{error:?}");
        assert!(unavailable(&error).contains(needle), "{error:?}");
        assert!(h.client.messages().is_empty(), "nothing was posted");

        // Not remembered: the next send asks again, and a user who can be reached now is reached.
        h.to_user(&USER.to_string(), "hi").await.unwrap();
        assert_eq!(h.client.dms(), [USER, USER]);
        assert_eq!(h.client.messages(), [(DM, "hi".to_owned())]);
    }
}

#[tokio::test(start_paused = true)]
async fn a_closed_dm_found_when_posting_is_unavailable_too() {
    let h = Harness::start().await;
    h.client.script_messages([Err(api(403, 50_007))]);
    let error = h.to_user(&USER.to_string(), "hi").await.unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
    assert!(unavailable(&error).contains("50007"), "{error:?}");
}

#[tokio::test(start_paused = true)]
async fn a_target_that_is_not_a_discord_id_is_invalid_and_costs_no_request() {
    let h = Harness::start().await;
    for recipient in [
        "abc",
        "12 34",
        "-5",
        "0",
        "+123",
        "1.5",
        "99999999999999999999",
        "whatsapp:+15551234567",
        "email:me@example.com",
        "discord:",
        "discord:abc",
        "discord:discord:5",
    ] {
        let error = h.to_user(recipient, "hi").await.unwrap_err();
        assert!(
            matches!(error, Error::InvalidRequest(_)),
            "{recipient}: {error:?}"
        );
    }
    for channel in ["abc", "0", "-1", "#general", "12x"] {
        let target = SendTarget::Conversation(ConversationId::new(channel).unwrap());
        let error = h.send(target, "hi").await.unwrap_err();
        assert!(
            matches!(error, Error::InvalidRequest(_)),
            "{channel}: {error:?}"
        );
    }
    assert!(h.client.messages().is_empty() && h.client.dms().is_empty());
}

#[tokio::test(start_paused = true)]
async fn each_discord_error_has_its_meaning() {
    enum Want {
        Unavailable,
        Invalid,
        NotFound,
    }
    use Want::{Invalid, NotFound, Unavailable};
    let cases = [
        (api(403, 50_007), Unavailable),
        (api(404, 10_013), Unavailable),
        (api(400, 40_003), Unavailable),
        (api(403, 50_278), Unavailable),
        (api(404, 10_003), NotFound),
        (api(413, 40_005), Invalid),
        (api(400, 50_045), Invalid),
        (api(400, 50_006), Invalid),
        (api(400, 50_035), Invalid),
        (RestError::TooLarge, Invalid),
        (RestError::RateLimited, Unavailable),
        (RestError::Transport, Unavailable),
        (
            RestError::Api {
                status: 500,
                code: Some(0),
            },
            Unavailable,
        ),
        (
            RestError::Api {
                status: 502,
                code: None,
            },
            Unavailable,
        ),
        // A code the plugin doesn't know is retried: the outbox expires the message.
        (api(403, 50_001), Unavailable),
    ];
    for (error, want) in cases {
        let h = Harness::start().await;
        h.client.script_messages([Err(error.clone())]);
        let got = h.reply("hi").await.unwrap_err();
        let ok = match want {
            Unavailable => matches!(got, Error::Unavailable { .. }),
            Invalid => matches!(got, Error::InvalidRequest(_)),
            NotFound => matches!(got, Error::NotFound(_)),
        };
        assert!(ok, "{error:?} became {got:?}");
        assert_eq!(h.client.messages().len(), 1, "{error:?}: no second try");
    }
}

/// Text that renders to several parts, each different from the others, and the parts.
fn long_text() -> (String, Vec<String>) {
    let text: String = (0..1_500).map(|i| format!("w{i:04} ")).collect();
    let parts = giles_plugin_sdk::render::discord(&text, 2000);
    assert!(parts.len() >= 4, "{} parts", parts.len());
    (text, parts)
}

/// The text of every message the fake was asked to post, in order.
fn posted(h: &Harness) -> Vec<String> {
    h.client
        .messages()
        .into_iter()
        .map(|(_, text)| text)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_retry_carries_on_after_the_parts_already_sent() {
    let h = Harness::start().await;
    let (text, parts) = long_text();
    let total = parts.len();
    // Two parts go out, the third fails.
    h.client
        .script_messages([Ok(1), Ok(2), Err(RestError::Transport)]);
    let error = h.to_dm(&text).await.unwrap_err();
    let message = unavailable(&error);
    assert!(error.is_retryable());
    assert!(
        message.contains(&format!("2 of {total} messages had already been sent")),
        "{message}"
    );
    assert!(
        message.contains("a retry carries on after them"),
        "{message}"
    );
    // The later parts were never attempted: nothing is posted out of order.
    assert_eq!(posted(&h), parts[..3]);

    // The outbox retries the whole message: the part that failed and the ones after it go, and
    // nothing that is already in the DM.
    let id = h.to_dm(&text).await.unwrap();
    let all = posted(&h);
    assert_eq!(all[..3], parts[..3]);
    assert_eq!(all[3..], parts[2..], "carried on at the part that failed");
    assert_eq!(id.as_str(), (5_000 + total - 2).to_string());

    // The same text sent as a new reply goes out in full: a finished send is not remembered.
    h.to_dm(&text).await.unwrap();
    let all = posted(&h);
    assert_eq!(all[3 + (total - 2)..], parts[..]);
}

#[tokio::test(start_paused = true)]
async fn a_part_that_fails_every_time_is_retried_alone_and_the_rest_is_never_repeated() {
    let h = Harness::start().await;
    let (text, parts) = long_text();
    let total = parts.len();
    // Discord's harmful-links filter (240000) refuses the second part, every time.
    let refused = || Err(api(400, 240_000));
    h.client
        .script_messages([Ok(1), refused(), refused(), refused()]);
    for _ in 0..3 {
        let error = h.to_dm(&text).await.unwrap_err();
        // Not a code the plugin knows: the outbox keeps retrying until the row expires.
        assert!(error.is_retryable(), "{error:?}");
        assert!(
            unavailable(&error).contains(&format!("1 of {total} messages")),
            "{error:?}"
        );
    }
    assert_eq!(
        posted(&h),
        [&parts[0][..], &parts[1], &parts[1], &parts[1]],
        "the first part once, the second every time"
    );

    // Once Discord takes it, the rest follows, and nothing before it is posted again.
    h.to_dm(&text).await.unwrap();
    assert_eq!(posted(&h)[4..], parts[1..]);
}

#[tokio::test(start_paused = true)]
async fn a_send_the_outbox_gave_up_on_leaves_its_progress() {
    let h = Harness::start().await;
    let (text, parts) = long_text();
    // The outbox drops a send at 60 s; each part takes 20 s, so two are accepted, and the third
    // is on its way when the send is dropped.
    h.client.delay_rest(Duration::from_secs(20));
    let cut = tokio::time::timeout(Duration::from_secs(50), h.to_dm(&text)).await;
    assert!(cut.is_err(), "the send was cut off");
    assert_eq!(posted(&h), parts[..3]);

    // The retry carries on at the third part, which Discord may or may not have taken.
    h.client.delay_rest(Duration::ZERO);
    h.to_dm(&text).await.unwrap();
    assert_eq!(posted(&h)[3..], parts[2..]);
}

#[tokio::test(start_paused = true)]
async fn an_invalid_part_after_some_went_out_says_so_without_promising_a_retry() {
    let h = Harness::start().await;
    h.client.script_messages([Ok(1), Err(api(400, 50_035))]);
    let text = "word ".repeat(1_000);
    let error = h.to_dm(&text).await.unwrap_err();
    let Error::InvalidRequest(message) = error else {
        panic!("{error:?}");
    };
    assert!(
        message.contains("1 of 3 messages had already been sent"),
        "{message}"
    );
    assert!(!message.contains("retry"), "{message}");

    // The outbox drops it, so the same text sent later is a new message, sent in full.
    h.to_dm(&text).await.unwrap();
    assert_eq!(h.client.messages().len(), 2 + 3);
}

#[tokio::test(start_paused = true)]
async fn a_reply_into_a_conversation_is_not_remembered_because_nothing_retries_it() {
    let h = Harness::start().await;
    let (text, parts) = long_text();
    let total = parts.len();
    // Two parts go out, the third fails.
    h.client
        .script_messages([Ok(1), Ok(2), Err(RestError::Transport)]);
    let error = h.reply(&text).await.unwrap_err();
    let message = unavailable(&error);
    // The error says what went out, and doesn't promise a retry that no one makes.
    assert!(
        message.contains(&format!("2 of {total} messages had already been sent")),
        "{message}"
    );
    assert!(!message.contains("retry"), "{message}");

    // The same text sent into the same conversation later is a new reply, sent in full: the
    // cut-off one holds no slot and makes no part skip.
    h.reply(&text).await.unwrap();
    let all = posted(&h);
    assert_eq!(all[3..], parts[..]);
}

#[tokio::test(start_paused = true)]
async fn a_dm_channel_discord_forgot_is_opened_again_once() {
    let h = Harness::start().await;
    h.to_user(&USER.to_string(), "one").await.unwrap();

    // The remembered channel is gone: Discord is asked again and the message goes to the new one.
    h.client.script_messages([Err(api(404, 10_003))]);
    h.client.script_dms([Ok(9_999)]);
    h.to_user(&USER.to_string(), "two").await.unwrap();
    assert_eq!(h.client.dms(), [USER, USER]);
    assert_eq!(
        h.client.messages(),
        [
            (DM, "one".to_owned()),
            (DM, "two".to_owned()),
            (9_999, "two".to_owned())
        ]
    );
    // The old channel is gone from the state file and the new one is recorded for the user.
    assert_eq!(h.state.get(DM), None);
    assert_eq!(h.state.channel_for_user(USER), Some(9_999));

    // The new channel is the remembered one now.
    h.to_user(&USER.to_string(), "three").await.unwrap();
    assert_eq!(h.client.dms(), [USER, USER]);
    assert_eq!(
        h.client.messages().last(),
        Some(&(9_999, "three".to_owned()))
    );
}

#[tokio::test(start_paused = true)]
async fn an_unknown_channel_twice_is_not_found_and_not_a_loop() {
    let h = Harness::start().await;
    h.to_user(&USER.to_string(), "one").await.unwrap();
    h.client
        .script_messages([Err(api(404, 10_003)), Err(api(404, 10_003))]);
    let error = h.to_user(&USER.to_string(), "two").await.unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    // Opened again once; the channel Discord just returned is not retried a second time.
    assert_eq!(h.client.dms(), [USER, USER]);
    assert_eq!(h.client.messages().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn an_unknown_channel_is_not_found_for_a_reply_and_for_a_new_dm() {
    let h = Harness::start().await;
    h.client.script_messages([Err(api(404, 10_003))]);
    let error = h.reply("hi").await.unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    assert!(h.client.dms().is_empty(), "a reply has no user to look up");

    // A channel Discord has just opened for us can't be unknown: no second DM request.
    h.client.script_messages([Err(api(404, 10_003))]);
    let error = h.to_user(&USER.to_string(), "hi").await.unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    assert_eq!(h.client.dms(), [USER]);
}

#[tokio::test(start_paused = true)]
async fn a_rejected_token_is_needs_auth_and_nothing_more_is_sent() {
    let mut h = Harness::start().await;
    h.client.push(GatewayEvent::Ready { user_id: 7 });
    assert_eq!(h.status().await, ChannelStatus::Connected);

    h.client.script_messages([Err(RestError::Unauthorized)]);
    let error = h.reply("hi").await.unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
    assert!(unavailable(&error).contains("token"), "{error:?}");
    assert_eq!(h.status().await, needs_auth());

    // REST is off for good: a reply, a recipient, a second try. Nothing reaches the client.
    let calls = h.client.messages().len();
    assert_eq!(calls, 1);
    for _ in 0..2 {
        assert!(h.reply("again").await.unwrap_err().is_retryable());
        assert!(h.to_user(&USER.to_string(), "again").await.is_err());
    }
    assert_eq!(h.client.messages().len(), calls);
    assert!(h.client.dms().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_rejected_token_on_a_dm_request_is_the_same() {
    let mut h = Harness::start().await;
    h.client.script_dms([Err(RestError::Unauthorized)]);
    let error = h.to_user(&USER.to_string(), "hi").await.unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
    assert_eq!(h.status().await, needs_auth());
    assert!(h.reply("hi").await.is_err());
    assert!(h.client.messages().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_401_after_the_gateway_gave_up_still_reports_needs_auth() {
    let mut h = Harness::start().await;
    // A final error status first: the session is over and polls nothing.
    h.client.push(GatewayEvent::Closed { code: Some(4014) });
    assert!(matches!(h.status().await, ChannelStatus::Error { .. }));
    h.client.script_messages([Err(RestError::Unauthorized)]);
    h.reply("hi").await.unwrap_err();
    assert_eq!(h.status().await, needs_auth());
}

#[tokio::test(start_paused = true)]
async fn a_token_the_gateway_rejected_stops_rest_too() {
    let mut h = Harness::start().await;
    h.client.push(GatewayEvent::Closed { code: Some(4004) });
    assert_eq!(h.status().await, needs_auth());
    let error = h.reply("hi").await.unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
    assert!(
        h.client.messages().is_empty(),
        "no request with a rejected token"
    );
}

#[tokio::test(start_paused = true)]
async fn a_token_a_history_request_found_rejected_stops_sends_too() {
    let mut h = Harness::start().await;
    h.state.record_dm(CHANNEL, USER);
    h.client.fail_history(RestError::Unauthorized);
    h.client.push(GatewayEvent::Ready { user_id: 7 });
    assert_eq!(h.status().await, ChannelStatus::Connected);
    assert_eq!(h.status().await, needs_auth());

    // The catch-up's 401 is the token's: a send finds it rejected and asks Discord nothing.
    let error = h.to_user(&USER.to_string(), "hi").await.unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
    assert!(unavailable(&error).contains("token"), "{error:?}");
    assert!(h.client.messages().is_empty() && h.client.dms().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_call_held_back_by_rate_limits_is_unavailable_and_says_why() {
    let h = Harness::start().await;
    // What `RealDiscord` answers when its 60 seconds run out.
    h.client.script_messages([Err(RestError::RateLimited)]);
    let error = h.reply("hi").await.unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
    assert!(unavailable(&error).contains("60 seconds"), "{error:?}");
}

#[tokio::test(start_paused = true)]
async fn a_dm_channel_is_remembered_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    {
        let first = Harness::start_in(dir.path()).await;
        first.to_user(&USER.to_string(), "one").await.unwrap();
        assert_eq!(first.client.dms(), [USER]);
    }
    // A new process over the same data directory finds the channel in the state file.
    let second = Harness::start_in(dir.path()).await;
    second.to_user(&USER.to_string(), "two").await.unwrap();
    assert!(
        second.client.dms().is_empty(),
        "no POST /users/@me/channels"
    );
    assert_eq!(second.client.messages(), [(DM, "two".to_owned())]);
}

#[tokio::test(start_paused = true)]
async fn a_channel_opened_for_a_send_is_recorded_and_caught_up_like_any_other() {
    let mut h = Harness::start().await;
    h.to_user(&USER.to_string(), "hello").await.unwrap();
    // Recorded as the DM channel with the user, with no cursor yet.
    assert_eq!(h.state.channel_for_user(USER), Some(DM));
    let known = h.state.get(DM).unwrap();
    assert_eq!((known.user, known.cursor), (Some(USER), None));

    // The next session catches the channel up: whatever the user wrote before the first live
    // message is fetched, as for any channel the plugin knows.
    h.client.push(GatewayEvent::Ready { user_id: 7 });
    assert_eq!(h.status().await, ChannelStatus::Connected);
    h.client.history_called(1).await;
    assert_eq!(h.client.history_calls()[0].channel, DM);
}

#[tokio::test(start_paused = true)]
async fn a_channel_inbound_messages_recorded_is_used_for_sends_without_asking_discord() {
    let h = Harness::start().await;
    // What the inbound side does when a user writes first.
    h.state.record_dm(CHANNEL, USER);
    h.to_user(&USER.to_string(), "reply").await.unwrap();
    assert!(h.client.dms().is_empty());
    assert_eq!(h.client.messages(), [(CHANNEL, "reply".to_owned())]);
}

#[tokio::test(start_paused = true)]
async fn the_parts_of_two_sends_never_interleave() {
    let h = Harness::start().await;
    h.client.delay_rest(Duration::from_secs(1));
    let first = "alpha ".repeat(900);
    let second = "bravo ".repeat(900);
    let (a, b) = tokio::join!(h.reply(&first), h.reply(&second));
    a.unwrap();
    b.unwrap();
    let texts: Vec<String> = h.client.messages().into_iter().map(|(_, t)| t).collect();
    assert_eq!(texts.len(), 6);
    // Three parts of one, then three of the other, whichever came first.
    let kinds: Vec<bool> = texts.iter().map(|t| t.starts_with("alpha")).collect();
    assert!(
        kinds == [true, true, true, false, false, false]
            || kinds == [false, false, false, true, true, true],
        "{kinds:?}"
    );
}
