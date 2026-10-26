//! The library-to-plugin conversion, from the payloads Discord documents.

use twilight_gateway::{CloseFrame, Event, parse};

use super::{EVENTS, convert};
use crate::client::GatewayEvent;

/// A READY dispatch: the shape in Discord's documentation, for a bot in no servers.
const READY: &str = r#"{
    "op": 0, "s": 1, "t": "READY",
    "d": {
        "v": 10,
        "user": {
            "id": "82198898841029460", "username": "Giles", "discriminator": "0",
            "global_name": null, "avatar": null, "bot": true, "mfa_enabled": true,
            "verified": true, "flags": 0
        },
        "guilds": [],
        "session_id": "a1b2c3d4",
        "resume_gateway_url": "wss://gateway-us-east1-b.discord.gg",
        "application": { "id": "82198898841029461", "flags": 8978432 },
        "shard": [0, 1]
    }
}"#;

const RESUMED: &str = r#"{"op": 0, "s": 42, "t": "RESUMED", "d": {}}"#;

/// A DM, which the skeleton neither asks for nor maps.
const MESSAGE_CREATE: &str = r#"{"op": 0, "s": 3, "t": "MESSAGE_CREATE", "d": {"id": "1"}}"#;

fn event(json: &str) -> Option<Event> {
    parse(json.to_owned(), EVENTS)
        .unwrap_or_else(|e| panic!("{json}: {e}"))
        .map(Into::into)
}

#[test]
fn ready_carries_the_bots_user_id() {
    let event = event(READY).expect("a wanted event");
    assert_eq!(
        convert(&event),
        Some(GatewayEvent::Ready {
            user_id: 82_198_898_841_029_460
        })
    );
}

#[test]
fn resumed_is_resumed() {
    let event = event(RESUMED).expect("a wanted event");
    assert_eq!(convert(&event), Some(GatewayEvent::Resumed));
}

#[test]
fn closes_carry_their_code_when_they_have_one() {
    assert_eq!(
        convert(&Event::GatewayClose(Some(CloseFrame::new(4004, "")))),
        Some(GatewayEvent::Closed { code: Some(4004) })
    );
    assert_eq!(
        convert(&Event::GatewayClose(None)),
        Some(GatewayEvent::Closed { code: None })
    );
}

#[test]
fn events_the_plugin_doesnt_use_are_skipped_unparsed() {
    // Not wanted, so the library doesn't even deserialize it.
    assert!(event(MESSAGE_CREATE).is_none());
    // The shard acts on a reconnect request by closing; the close is what the plugin sees.
    assert_eq!(convert(&Event::GatewayReconnect), None);
}
