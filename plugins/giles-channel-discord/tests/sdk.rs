//! The channel over the wire, with a fake Discord client behind the real plugin SDK.

mod support;

use giles_channel_discord::client::GatewayEvent;
use giles_plugin_sdk::giles_types::{AuthChallenge, ChannelStatus};
use giles_proto::channel::v1::OutputMode;
use giles_proto::plugin::v1::{DescribeRequest, PluginKind, describe_response};
use support::{Served, TOKEN, ack, next_status, wait_until};
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
