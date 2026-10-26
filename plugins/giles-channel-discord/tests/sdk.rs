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

#[tokio::test]
async fn a_send_over_the_wire_posts_a_message_and_failures_have_their_codes() {
    use giles_channel_discord::client::RestError;
    use giles_channel_discord::fake::dm_channel_of;
    use giles_proto::channel::v1::{SendRequest, send_request::Target};

    let served = configured().await;
    let send = |target: Target| SendRequest {
        target: Some(target),
        markdown: "Hello *there*".into(),
        attachments: Vec::new(),
        reply_to: None,
    };
    let mut rpc = served.rpc();

    // A proactive send to an identity opens the DM channel and posts to it.
    let sent = rpc
        .send(send(Target::Recipient(USER.to_string())))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(sent.message_id, "5001");
    assert_eq!(served.client.dms(), [USER]);
    assert_eq!(
        served.client.messages(),
        [(dm_channel_of(USER), "Hello *there*".to_owned())]
    );

    // A reply into a conversation posts to that channel and opens nothing.
    rpc.send(send(Target::ConversationId(CHANNEL.to_string())))
        .await
        .unwrap();
    assert_eq!(served.client.dms(), [USER]);
    assert_eq!(served.client.messages()[1].0, CHANNEL);

    // A user who can't be messaged is UNAVAILABLE (the outbox retries), a target that isn't a
    // Discord id INVALID_ARGUMENT (it doesn't).
    served.client.script_dms([Err(RestError::Api {
        status: 403,
        code: Some(50_007),
    })]);
    let status = rpc
        .send(send(Target::Recipient("5551234".into())))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable, "{status:?}");
    let status = rpc
        .send(send(Target::Recipient("whatsapp:+15551234567".into())))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
}

// Attachments through the SDK's real blob store.

/// The files in the SDK's blob directory (inbound blobs and their sidecars, and the core's
/// uploads under `outbound/`): zero when nothing is kept.
fn blob_files(served: &Served) -> usize {
    std::fs::read_dir(served.data_dir().join("giles-sdk/blobs")).map_or(0, Iterator::count)
}

/// A blob the core puts for a send, as the `BlobRef` the send names.
async fn put_blob(
    served: &Served,
    media_type: &str,
    name: Option<&str>,
    chunks: Vec<Vec<u8>>,
) -> giles_proto::types::v1::BlobRef {
    use giles_proto::plugin::v1::{BlobChunk, PutBlobRequest, blob_chunk};

    let chunk = |part| PutBlobRequest {
        chunk: Some(BlobChunk { part: Some(part) }),
    };
    let header = chunk(blob_chunk::Part::Header(giles_proto::types::v1::BlobRef {
        id: String::new(),
        media_type: media_type.into(),
        size: chunks.iter().map(|c| c.len() as u64).sum(),
        name: name.map(str::to_owned),
    }));
    let requests = std::iter::once(header)
        .chain(chunks.into_iter().map(|c| chunk(blob_chunk::Part::Data(c))))
        .collect::<Vec<_>>();
    served
        .plugin()
        .put_blob(futures::stream::iter(requests))
        .await
        .unwrap()
        .into_inner()
        .blob
        .unwrap()
}

#[tokio::test]
async fn a_downloaded_attachment_is_a_blob_the_core_fetches_and_it_goes_with_the_ack() {
    use giles_channel_discord::client::Attachment;
    use giles_channel_discord::fake::Served as Cdn;
    use giles_proto::plugin::v1::{GetBlobRequest, blob_chunk};

    let served = configured().await;
    let (acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    let (status_id, _) = next_status(&mut events).await;
    acks.unbounded_send(ack(&status_id)).unwrap();

    let url = "https://cdn.discordapp.com/attachments/1/2/photo.png?ex=1&is=2&hm=3";
    served
        .client
        .serve(url, Cdn::file(b"\x89PNG not really a png"));
    let mut sent = direct_message(id_now(1), CHANNEL, USER, "a photo");
    sent.attachments.push(Attachment {
        url: url.to_owned(),
        filename: "photo.png".to_owned(),
        content_type: Some("image/png".to_owned()),
        size: 21,
    });
    served.client.push(GatewayEvent::Message(sent));
    let (id, message) = next_message(&mut events).await;
    assert_eq!(message.text, "a photo");
    let [media] = &message.attachments[..] else {
        panic!("one attachment: {message:?}");
    };
    assert_eq!(media.media_type, "image/png");
    assert_eq!(media.name.as_deref(), Some("photo.png"));
    assert_eq!(media.size, 21);

    // The core fetches the bytes with GetBlob.
    let mut stream = served
        .plugin()
        .get_blob(GetBlobRequest {
            id: media.id.to_string(),
        })
        .await
        .unwrap()
        .into_inner();
    let mut bytes = Vec::new();
    while let Some(response) = stream.message().await.unwrap() {
        if let blob_chunk::Part::Data(data) = response.chunk.unwrap().part.unwrap() {
            bytes.extend(data);
        }
    }
    assert_eq!(bytes, b"\x89PNG not really a png");
    assert!(blob_files(&served) > 0, "kept until the event is acked");

    // The ack releases it.
    acks.unbounded_send(ack(&id)).unwrap();
    wait_until(|| blob_files(&served) == 0 && served.spooled() == 0).await;
}

#[tokio::test]
async fn an_attachment_that_cannot_be_downloaded_still_delivers_the_message_with_a_note() {
    use giles_channel_discord::client::Attachment;

    let served = configured().await;
    let (_acks, mut events) = served.events().await;
    served.client.push(GatewayEvent::Ready { user_id: 7 });
    next_status(&mut events).await;
    let mut sent = direct_message(id_now(1), CHANNEL, USER, "");
    sent.attachments.push(Attachment {
        url: "https://cdn.discordapp.com/attachments/1/2/gone.pdf?ex=1".to_owned(),
        filename: "gone.pdf".to_owned(),
        content_type: None,
        size: 1,
    });
    served.client.push(GatewayEvent::Message(sent));
    let (_, message) = next_message(&mut events).await;
    assert!(message.attachments.is_empty());
    assert_eq!(
        message.text,
        "[attachment not received: gone.pdf is no longer available from Discord]"
    );
    assert_eq!(blob_files(&served), 0, "nothing half-written is left");
}

#[tokio::test]
async fn a_send_reads_the_blobs_the_core_put_and_uploads_them() {
    use giles_channel_discord::fake::dm_channel_of;
    use giles_proto::channel::v1::{SendRequest, send_request::Target};

    let served = configured().await;
    let first = put_blob(
        &served,
        "application/pdf",
        Some("notes.pdf"),
        vec![b"%PDF".to_vec(), b"-1.7".to_vec()],
    )
    .await;
    let second = put_blob(&served, "image/png", None, vec![b"png".to_vec()]).await;
    let sent = served
        .rpc()
        .send(SendRequest {
            target: Some(Target::Recipient(USER.to_string())),
            markdown: "Here are the **files**".into(),
            attachments: vec![first, second],
            reply_to: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(sent.message_id, "5001");

    let posted = served.client.posted();
    assert_eq!(posted.len(), 1, "one message carries both files");
    assert_eq!(posted[0].0, dm_channel_of(USER));
    assert_eq!(posted[0].1.content, "Here are the **files**");
    let files: Vec<_> = posted[0]
        .1
        .files
        .iter()
        .map(|f| (f.name.as_str(), f.media_type.as_str(), f.bytes.to_vec()))
        .collect();
    assert_eq!(
        files,
        [
            ("notes.pdf", "application/pdf", b"%PDF-1.7".to_vec()),
            ("attachment.png", "image/png", b"png".to_vec()),
        ]
    );
    // The core's uploads are the Send's: gone once it has ended.
    wait_until(|| {
        std::fs::read_dir(served.data_dir().join("giles-sdk/outbound")).map_or(0, Iterator::count)
            == 0
    })
    .await;
}

#[tokio::test]
async fn a_file_over_discords_limit_is_invalid_argument_and_posts_nothing() {
    use giles_proto::channel::v1::{SendRequest, send_request::Target};

    let served = configured().await;
    // 21 MiB in 1 MiB chunks: over Discord's 20 MiB per file, under the default `max_blob`.
    let big = put_blob(
        &served,
        "video/mp4",
        Some("clip.mp4"),
        vec![vec![0_u8; 1024 * 1024]; 21],
    )
    .await;
    let status = served
        .rpc()
        .send(SendRequest {
            target: Some(Target::ConversationId(CHANNEL.to_string())),
            markdown: "a clip".into(),
            attachments: vec![big],
            reply_to: None,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert!(status.message().contains("clip.mp4"), "{status:?}");
    assert!(served.client.posted().is_empty());
}
