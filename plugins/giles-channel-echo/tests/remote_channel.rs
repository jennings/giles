//! The echo plugin as a registered channel of a running core (plugin host, `RemoteChannel`,
//! registry, sessions), unsandboxed: events into turns and acks back, replies, status, and
//! attachments both ways. Protocol violations the SDK can't produce (a lying blob header, an
//! unknown event variant) are tested against a fake plugin in
//! `crates/giles-core/src/plugin_host/remote_channel/tests.rs`.

mod support;

use std::fs;

use giles_core::config::Kind;
use giles_core::plugin_host::PluginState;
use giles_plugin_sdk::giles_types::{
    ChannelStatus, ConversationId, MediaId, OutboundMessage, SendTarget,
};
use sha2::{Digest as _, Sha256};

use self::support::{Home, WAIT, owner_message, wait_until};

fn sha256(bytes: &[u8]) -> MediaId {
    MediaId::from_digest(Sha256::digest(bytes).into())
}

#[tokio::test]
async fn messages_reach_turns_get_acked_and_replies_come_back() {
    let home = Home::serving("[channel.echo]\nunsandboxed = true\n");
    home.install_echo_as("echo");
    let core = home.start_core().await;
    assert!(core.runtime.registry.channel("echo").is_some());
    assert!(core.runtime.registry.pending().is_empty());

    home.inject("echo", "1", &owner_message("e1", "hello", &[]));
    let messages = core.turns.wait_for(1).await;
    assert_eq!(messages[0].text, "hello");
    assert_eq!(messages[0].sender.as_str(), "owner");

    // Acked once stored: the SDK forgot it (its spool is empty) and told the channel.
    wait_until(|| (!home.lines("echo", "acks.jsonl").is_empty()).then_some(())).await;
    assert_eq!(home.lines("echo", "acks.jsonl")[0]["event_id"], "e1");
    assert!(home.sdk_files("echo", "events").is_empty());

    // The turn's reply went out through `Send`.
    let sent = wait_until(|| {
        let sent = home.lines("echo", "sent.jsonl");
        (!sent.is_empty()).then_some(sent)
    })
    .await;
    assert_eq!(sent[0]["message"]["markdown"], "echo: hello");
    assert_eq!(sent[0]["message"]["target"]["conversation"], "c1");

    // Status events are recorded for `giles status`.
    home.inject(
        "echo",
        "2",
        &serde_json::json!({
            "event": { "type": "status", "id": "s1", "status": { "type": "connected" } },
        }),
    );
    let status = wait_until(|| core.sessions.channel_status("echo")).await;
    assert_eq!(status.status, ChannelStatus::Connected);
    core.stop().await;
}

#[tokio::test]
async fn attachments_travel_both_ways() {
    let home = Home::serving("[channel.echo]\nunsandboxed = true\n");
    home.install_echo_as("echo");
    let core = home.start_core().await;

    // Two events with the same bytes, which the SDK stores once.
    home.inject("echo", "1", &owner_message("e1", "one", &["inbound bytes"]));
    home.inject("echo", "2", &owner_message("e2", "two", &["inbound bytes"]));
    let messages = core.turns.wait_for(2).await;
    let id = sha256(b"inbound bytes");
    for message in &messages {
        assert_eq!(message.attachments.len(), 1, "{}", message.text);
        let media = &message.attachments[0];
        assert_eq!(media.id, id);
        assert_eq!(media.media_type, "text/plain");
        assert_eq!(media.name.as_deref(), Some("note.txt"));
        assert_eq!(media.size, 13);
    }
    let stored = core.runtime.blobs.stat(&id).await.unwrap().unwrap();
    assert_eq!(stored.size, 13);
    // Both acked: the plugin's copy is gone.
    wait_until(|| (home.lines("echo", "acks.jsonl").len() == 2).then_some(())).await;
    wait_until(|| home.sdk_files("echo", "blobs").is_empty().then_some(())).await;

    // Outbound: uploaded with `PutBlob`, read back by the plugin in `Send`.
    let channel = core.runtime.registry.channel("echo").unwrap();
    let message_id = tokio::time::timeout(
        WAIT,
        channel.send(OutboundMessage {
            target: SendTarget::Conversation(ConversationId::new("c1").unwrap()),
            markdown: "here it is".into(),
            attachments: vec![stored],
            reply_to: None,
        }),
    )
    .await
    .unwrap()
    .unwrap();
    let sent = home.lines("echo", "sent.jsonl");
    let record = sent
        .iter()
        .find(|line| line["message_id"] == message_id.as_str())
        .unwrap();
    assert_eq!(record["attachment_texts"][0], "inbound bytes");
    // The plugin's handle lived only as long as the `Send`.
    assert!(home.sdk_files("echo", "outbound").is_empty());
    core.stop().await;
}

#[tokio::test]
async fn an_attachment_over_max_blob_is_left_out_with_a_note() {
    let home = Home::serving("[channel.echo]\nunsandboxed = true\nmax_blob = 4\n");
    home.install_echo_as("echo");
    let core = home.start_core().await;

    home.inject("echo", "1", &owner_message("e1", "look", &["too long"]));
    let messages = core.turns.wait_for(1).await;
    assert!(messages[0].attachments.is_empty());
    assert_eq!(
        messages[0].text,
        "look\n\n[attachment \"note.txt\" (text/plain, 8 bytes) not received: it is larger than \
         this channel's limit of 4 bytes]"
    );
    assert!(
        core.runtime
            .blobs
            .stat(&sha256(b"too long"))
            .await
            .unwrap()
            .is_none()
    );
    // Refused, but the ack still frees the plugin's copy.
    wait_until(|| home.sdk_files("echo", "blobs").is_empty().then_some(())).await;
    core.stop().await;
}

#[tokio::test]
async fn the_host_registers_what_it_launches_and_stops_everything() {
    let home = Home::serving(
        "[channel.echo]\nunsandboxed = true\n\n[channel.missing]\nunsandboxed = true\n",
    );
    home.install_echo_as("echo");
    let core = home.start_core().await;
    let states: Vec<(String, PluginState)> = core
        .host
        .statuses()
        .into_iter()
        .map(|status| (status.instance.to_string(), status.state))
        .collect();
    assert_eq!(states.len(), 2, "{states:?}");
    assert!(
        matches!(states[0].1, PluginState::Running { .. }),
        "{states:?}"
    );
    assert_eq!(states[1], ("missing".to_owned(), PluginState::Failed));
    let pending: Vec<String> = core
        .runtime
        .registry
        .pending()
        .into_iter()
        .map(|p| p.name.to_string())
        .collect();
    assert_eq!(pending, ["missing"]);
    let pid_file = home.root.pid_file(Kind::Channel, "echo").unwrap();
    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let host = &core.host;
    tokio::time::timeout(WAIT, host.stop_all()).await.unwrap();
    assert_eq!(host.status("echo").unwrap().state, PluginState::Stopped);
    assert!(!pid_file.exists());
    // SAFETY: signal 0 only checks whether the process exists.
    #[allow(unsafe_code)]
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "the plugin still runs");
    core.stop().await;
}
