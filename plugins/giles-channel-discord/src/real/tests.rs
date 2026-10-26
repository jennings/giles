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

/// A message edit, which the plugin never asks for.
const MESSAGE_UPDATE: &str = r#"{"op": 0, "s": 3, "t": "MESSAGE_UPDATE", "d": {"id": "1"}}"#;

/// A DM as Discord documents `MESSAGE_CREATE`: no `guild_id`, a user author, a reply to another
/// message, a file, and a sticker.
const DM: &str = r#"{
    "op": 0, "s": 3, "t": "MESSAGE_CREATE",
    "d": {
        "id": "1234567890123456789", "channel_id": "319674150115610528", "type": 19,
        "content": "hello there", "timestamp": "2026-10-24T12:00:00.123000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false, "mentions": [],
        "mention_roles": [], "pinned": false, "flags": 0, "embeds": [],
        "author": {
            "id": "11111111111111111", "username": "alice", "discriminator": "0",
            "global_name": "Alice", "avatar": null
        },
        "attachments": [{
            "id": "2222", "filename": "photo.png", "size": 12345, "content_type": "image/png",
            "url": "https://cdn.discordapp.com/attachments/1/2/photo.png?ex=a&is=b&hm=c",
            "proxy_url": "https://media.discordapp.net/attachments/1/2/photo.png"
        }],
        "sticker_items": [{"id": "3333", "name": "wave", "format_type": 1}],
        "message_reference": {"type": 0, "message_id": "1234567890123456000", "channel_id": "319674150115610528"}
    }
}"#;

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
    assert!(event(MESSAGE_UPDATE).is_none());
    // The shard acts on a reconnect request by closing; the close is what the plugin sees.
    assert_eq!(convert(&Event::GatewayReconnect), None);
}

#[test]
fn a_dm_becomes_the_plugins_message() {
    let event = event(DM).expect("a wanted event");
    let Some(GatewayEvent::Message(message)) = convert(&event) else {
        panic!("not a message: {event:?}");
    };
    assert_eq!(message.id, 1_234_567_890_123_456_789);
    assert_eq!(message.channel_id, 319_674_150_115_610_528);
    assert_eq!(message.author.id, 11_111_111_111_111_111);
    assert!(
        !message.author.bot,
        "an author with no `bot` field is a person"
    );
    assert_eq!(message.author.username, "alice");
    assert_eq!(message.author.global_name.as_deref(), Some("Alice"));
    assert!(!message.guild && !message.webhook);
    assert_eq!(message.kind, 19);
    assert_eq!(message.content, "hello there");
    assert_eq!(message.attachments.len(), 1);
    assert_eq!(message.attachments[0].filename, "photo.png");
    assert_eq!(message.attachments[0].size, 12345);
    assert_eq!(
        message.attachments[0].content_type.as_deref(),
        Some("image/png")
    );
    assert!(message.attachments[0].url.contains("ex=a"));
    assert_eq!(message.stickers, ["wave"]);
    // 2026-10-24T12:00:00.123Z
    assert_eq!(message.timestamp_ms, 1_792_843_200_123);
    assert_eq!(message.reply_to, Some(1_234_567_890_123_456_000));
}

#[test]
fn guild_webhook_and_bot_messages_say_so() {
    let guild = DM.replace(
        r#""type": 19,"#,
        r#""type": 0, "guild_id": "5555", "webhook_id": "6666","#,
    );
    let Some(GatewayEvent::Message(message)) = convert(&event(&guild).unwrap()) else {
        panic!("not a message");
    };
    assert!(message.guild);
    assert!(message.webhook);
    let bot = DM.replace(r#""avatar": null"#, r#""avatar": null, "bot": true"#);
    let Some(GatewayEvent::Message(message)) = convert(&event(&bot).unwrap()) else {
        panic!("not a message");
    };
    assert!(message.author.bot);
}
