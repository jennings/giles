//! The channel over the wire, with a fake Discord client behind the real plugin SDK.

mod support;

use std::time::Duration;

use giles_channel_discord::client::GatewayEvent;
use giles_channel_discord::fake::direct_message;
use giles_plugin_sdk::giles_types::ChannelEvent;
use giles_plugin_sdk::giles_types::{AuthChallenge, ChannelStatus};
use giles_proto::channel::v1::OutputMode;
use giles_proto::plugin::v1::{DescribeRequest, PluginKind, describe_response};
use support::{Served, TOKEN, ack, next_event, next_message, next_status, wait_until};
use tonic::Code;

#[tokio::test]
async fn describe_and_configure_over_the_wire() {
    let served = Served::start().await;
    let described = served
        .plugin()
        .describe(DescribeRequest {
            core_protocol_minor: 0,
            core_features: vec!["channel.status".into()],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(described.kind(), PluginKind::Channel);
    assert_eq!(described.name, "discord");
    let Some(describe_response::Capabilities::Channel(capabilities)) = described.capabilities
    else {
        panic!("no channel capabilities");
    };
    assert_eq!(capabilities.output(), OutputMode::Blocks);
    assert!(capabilities.can_initiate);

    // Bad config is refused with the key named, and never with the token in it.
    for (config, needle) in [
        (serde_json::json!({}), "invalid config: token: required"),
        (
            serde_json::json!({"token": TOKEN, "tokn": "x"}),
            "unknown field `tokn`",
        ),
        (
            serde_json::json!({"token": format!("Bot {TOKEN}")}),
            "invalid config: token: paste the token alone",
        ),
        (
            serde_json::json!({"token": TOKEN, "max_blob": 0}),
            "invalid config: max_blob: must be more than 0",
        ),
        (
            serde_json::json!({"token": TOKEN, "catch_up_max_age": "31d"}),
            "invalid config: catch_up_max_age: must be at most",
        ),
    ] {
        let status = served.configure(config).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert!(status.message().contains(needle), "{status:?}");
        assert!(!status.message().contains("not-a-real-token"), "{status:?}");
    }
    assert_eq!(
        served
            .configure(serde_json::json!({"token": TOKEN}))
            .await
            .unwrap(),
        "discord"
    );
}

#[tokio::test]
async fn a_status_survives_until_the_core_acks_it() {
    let served = Served::start().await;
    served
        .configure(serde_json::json!({"token": TOKEN}))
        .await
        .unwrap();
    let (acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    let (id, status) = next_status(&mut events).await;
    assert_eq!(status, ChannelStatus::Connected);

    // Not acked: a reconnecting core gets it again, from the spool.
    drop(acks);
    drop(events);
    let (acks, mut events) = served.events().await;
    let (again, status) = next_status(&mut events).await;
    assert_eq!(
        (again.as_str(), status),
        (id.as_str(), ChannelStatus::Connected)
    );
    acks.unbounded_send(ack(&id)).unwrap();
    wait_until(|| served.spooled() == 0).await;
}

#[tokio::test]
async fn a_rejected_token_reaches_the_core_as_needs_auth() {
    let served = Served::start().await;
    served
        .configure(serde_json::json!({"token": TOKEN}))
        .await
        .unwrap();
    let (_acks, mut events) = served.events().await;
    served
        .client
        .push(GatewayEvent::Closed { code: Some(4004) });
    let (_, status) = next_status(&mut events).await;
    assert_eq!(
        status,
        ChannelStatus::NeedsAuth {
            challenge: AuthChallenge::Url {
                url: "https://discord.com/developers/applications".into()
            },
            expires_at: None,
        }
    );
}

// --- direct messages, persistence, and the cursor ---

/// A message id created now, so the plugin's age limit has nothing to say about it.
fn id_now(n: u64) -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    giles_channel_discord::snowflake::at_unix_ms(u64::try_from(now).unwrap()) + n
}

const CHANNEL: u64 = 319_674_150_115_610_528;
const USER: u64 = 82_198_898_841_029_460;

async fn configured() -> Served {
    let served = Served::start().await;
    served
        .configure(serde_json::json!({"token": TOKEN}))
        .await
        .unwrap();
    served
}

#[tokio::test]
async fn a_dm_is_persisted_before_it_is_acknowledged_and_comes_again_until_it_is() {
    let served = configured().await;
    let (acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    next_status(&mut events).await;
    let sent = direct_message(id_now(1), CHANNEL, USER, "hello over the wire");
    served.client.push(GatewayEvent::Message(sent.clone()));

    let (event_id, message) = next_message(&mut events).await;
    assert_eq!(event_id, sent.id.to_string());
    assert_eq!(message.text, "hello over the wire");
    assert_eq!(message.sender.as_str(), format!("discord:{USER}"));
    assert!(message.sender_authenticated);
    assert_eq!(message.conversation.id.as_str(), CHANNEL.to_string());
    // On disk with the SDK before the core had it, and the plugin's cursor follows once the SDK
    // has moved on.
    // (The status of the session is in the spool too, unacked.)
    assert_eq!(served.spooled(), 2);
    wait_until(|| served.cursor(CHANNEL).as_deref() == Some(&sent.id.to_string())).await;

    // Never acked: a core that reconnects gets it again, with the same id.
    drop(acks);
    drop(events);
    let (acks, mut events) = served.events().await;
    // (The status of the new session is in the spool as well, and comes first.)
    let (again, message) = loop {
        match next_event(&mut events).await {
            ChannelEvent::Message { id, message } => break (id.to_string(), message),
            ChannelEvent::Status { .. } => {}
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        (again.as_str(), message.text.as_str()),
        (event_id.as_str(), "hello over the wire")
    );
    acks.unbounded_send(ack(&event_id)).unwrap();
    wait_until(|| served.spooled() == 1).await; // only the status is left
}

#[tokio::test]
async fn messages_come_in_the_order_they_were_sent() {
    let served = configured().await;
    let (_acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    for n in 1..=5 {
        served.client.push(GatewayEvent::Message(direct_message(
            id_now(n),
            CHANNEL,
            USER,
            &format!("message {n}"),
        )));
    }
    let mut texts = Vec::new();
    while texts.len() < 5 {
        if let ChannelEvent::Message { message, .. } = next_event(&mut events).await {
            texts.push(message.text);
        }
    }
    assert_eq!(
        texts,
        [
            "message 1",
            "message 2",
            "message 3",
            "message 4",
            "message 5"
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn nothing_is_acknowledged_while_the_sdk_cannot_persist() {
    use std::os::unix::fs::PermissionsExt as _;

    let served = configured().await;
    let (_acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    next_status(&mut events).await;

    // The SDK's spool can't take a file: renaming into `events/` fails, and the SDK retries
    // instead of dropping the event.
    let spool = served.data_dir().join("giles-sdk/events");
    let writable = std::fs::metadata(&spool).unwrap().permissions();
    std::fs::set_permissions(&spool, std::fs::Permissions::from_mode(0o555)).unwrap();
    let can_still_write = std::fs::File::create(spool.join("probe")).is_ok();
    if can_still_write {
        // Running as a user the permissions don't bind (root): the failure can't be made.
        std::fs::remove_file(spool.join("probe")).unwrap();
        std::fs::set_permissions(&spool, writable).unwrap();
        return;
    }

    let sent = direct_message(id_now(1), CHANNEL, USER, "can't be stored");
    served.client.push(GatewayEvent::Message(sent.clone()));
    // The plugin hands the event over and then waits for the SDK to ask for the next one, which
    // it doesn't while the write fails: no event on the wire, and the cursor stays.
    let nothing = tokio::time::timeout(Duration::from_millis(600), events.message()).await;
    assert!(
        nothing.is_err(),
        "an event that wasn't persisted was offered"
    );
    assert_eq!(served.cursor(CHANNEL), None);
    // Only the status of the session is in the spool, not the message.
    assert_eq!(served.spooled(), 1);

    // The disk recovers: the SDK's retry persists it, offers it, and only then does the cursor move.
    std::fs::set_permissions(&spool, writable).unwrap();
    let (event_id, message) = next_message(&mut events).await;
    assert_eq!(event_id, sent.id.to_string());
    assert_eq!(message.text, "can't be stored");
    wait_until(|| served.cursor(CHANNEL).as_deref() == Some(&sent.id.to_string())).await;
}

#[tokio::test]
async fn the_history_before_a_new_channels_first_message_reaches_the_core_first() {
    let served = configured().await;
    let (acks, mut events) = served.events().await;
    // Discord has three messages of a DM the plugin has never seen; the first is also what
    // arrives live, as the Gateway would send it after the plugin connected.
    let first = direct_message(id_now(1), CHANNEL, USER, "from before");
    served.client.add_history(first.clone());
    served
        .client
        .add_history(direct_message(id_now(2), CHANNEL, USER, "missed one"));
    served
        .client
        .add_history(direct_message(id_now(3), CHANNEL, USER, "missed two"));
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    next_status(&mut events).await;
    served.client.push(GatewayEvent::Message(first));
    let mut texts = Vec::new();
    while texts.len() < 3 {
        let (id, message) = next_message(&mut events).await;
        acks.unbounded_send(ack(&id)).unwrap();
        texts.push(message.text);
    }
    assert_eq!(texts, ["from before", "missed one", "missed two"]);
    wait_until(|| served.spooled() <= 1).await;
}
