//! Pure conversions from the library's event and message types to the plugin's own
//! ([`crate::backend`]). No I/O, so they're tested directly with hand-built library values.

use giles_plugin_sdk::giles_types::Timestamp;
use whatsapp_rust::wacore::proto_helpers::MessageExt as _;
use whatsapp_rust::wacore::types::events::{Event, EventKind, InboundMessage as LibMessage};
use whatsapp_rust::wacore::types::message::MessageInfo;
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt as _, Server};
use whatsapp_rust::waproto::whatsapp as wa;

use crate::backend::{
    Address, ChatKind, Incoming, IncomingMedia, MediaKind, MediaPayload, WaEvent,
};

/// The kinds of library events [`event`] translates; the handler subscribes to just these.
pub(super) const EVENT_KINDS: &[EventKind] = &[
    EventKind::Connected,
    EventKind::Disconnected,
    EventKind::PairSuccess,
    EventKind::PairError,
    EventKind::LoggedOut,
    EventKind::PairingQrCode,
    EventKind::PairingCode,
    EventKind::PairingCodeRefresh,
    EventKind::PairingCodeError,
    EventKind::PairingQrCodesExhausted,
    EventKind::QrScannedWithoutMultidevice,
    EventKind::ClientOutdated,
    EventKind::StreamReplaced,
    EventKind::TemporaryBan,
    EventKind::ConnectFailure,
];

/// Translates a library event into the plugin's, or `None` for one the plugin has no use for.
///
/// Messages aren't handled here: they arrive through the durability hook, which makes them
/// durable before the client acknowledges them.
pub(super) fn event(event: &Event) -> Option<WaEvent> {
    Some(match event {
        Event::Connected(_) => WaEvent::Connected,
        Event::Disconnected(d) => WaEvent::Disconnected(d.reason.to_string()),
        Event::PairSuccess(_) => WaEvent::Paired,
        Event::PairError(e) => {
            WaEvent::LinkFailed(format!("WhatsApp refused the link: {}", e.error))
        }
        Event::LoggedOut(l) => WaEvent::LoggedOut(format!("{:?}", l.reason)),
        Event::PairingQrCode(qr) => WaEvent::PairingQr {
            code: qr.code.clone(),
            timeout: qr.timeout,
        },
        Event::PairingCode(code) => WaEvent::PairingCode {
            code: code.code.clone(),
            timeout: code.timeout,
        },
        Event::PairingCodeRefresh(_) => {
            WaEvent::LinkFailed("the pair code expired or was refused by the phone".to_owned())
        }
        Event::PairingCodeError(e) => WaEvent::LinkFailed(format!(
            "WhatsApp refused the pair code request: {}",
            e.error
        )),
        Event::PairingQrCodesExhausted(e) => WaEvent::QrExhausted {
            closed: e.disconnected,
        },
        Event::QrScannedWithoutMultidevice(_) => WaEvent::LinkFailed(
            "the phone scanned the code but has multi-device disabled".to_owned(),
        ),
        Event::ClientOutdated(_) => WaEvent::Fatal(
            "WhatsApp rejected this client version as outdated; update Giles".to_owned(),
        ),
        Event::StreamReplaced(_) => WaEvent::Fatal(
            "another client connected as this device and replaced this session".to_owned(),
        ),
        Event::TemporaryBan(ban) => WaEvent::Fatal(match &ban.message {
            Some(message) => format!(
                "WhatsApp temporarily banned this account ({}, for {} s): {message}",
                ban.code,
                ban.expire.num_seconds()
            ),
            None => format!(
                "WhatsApp temporarily banned this account ({}, for {} s)",
                ban.code,
                ban.expire.num_seconds()
            ),
        }),
        // The library keeps reconnecting after these (the service is down or failing): a drop.
        Event::ConnectFailure(f) if f.reason.should_reconnect() => {
            WaEvent::Disconnected(format!("{:?}", f.reason))
        }
        Event::ConnectFailure(f) => WaEvent::Fatal(match &f.message {
            Some(message) => format!(
                "WhatsApp refused the connection ({:?}): {message}",
                f.reason
            ),
            None => format!("WhatsApp refused the connection ({:?})", f.reason),
        }),
        _ => return None,
    })
}

/// The plugin's view of a decrypted message, or `None` if it carries nothing to deliver
/// (a reaction, an edit or revoke, a call log, a poll vote, ...): no text and no supported media.
pub(super) fn incoming(message: &LibMessage) -> Option<Incoming> {
    incoming_from(&message.message, &message.info)
}

fn incoming_from(message: &wa::Message, info: &MessageInfo) -> Option<Incoming> {
    let body = body(message)?;
    let source = &info.source;
    let kind = if source.chat.is_status_broadcast()
        || source.chat.is_newsletter()
        || source.chat.is_broadcast_list()
    {
        ChatKind::Other
    } else if source.is_group || source.chat.is_group() {
        ChatKind::Group
    } else {
        ChatKind::Direct
    };
    Some(Incoming {
        id: info.id.to_string(),
        chat: source.chat.to_non_ad_string(),
        kind,
        sender_jid: source.sender.to_non_ad_string(),
        sender: address(&source.sender),
        sender_alt: source.sender_alt.as_ref().and_then(address),
        from_me: source.is_from_me,
        push_name: Some(info.push_name.trim())
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
        sent_at: Timestamp::from_second(info.timestamp.timestamp()).ok(),
        text: body.text,
        reply_to: body.reply_to,
        media: body.media,
    })
}

/// A JID as a phone or LID address, without its device.
fn address(jid: &Jid) -> Option<Address> {
    match jid.server {
        Server::Pn | Server::Hosted => Some(Address::phone(jid.user_base())),
        Server::Lid | Server::HostedLid => Some(Address::lid(jid.user_base())),
        _ => None,
    }
}

struct Body {
    text: String,
    reply_to: Option<String>,
    media: Option<IncomingMedia>,
}

/// The text, reply target, and attachment of a message with something to deliver.
fn body(message: &wa::Message) -> Option<Body> {
    let base = message.get_base_message();
    let media = media(base);
    let text = message
        .text_content()
        .or_else(|| message.get_caption())
        .unwrap_or_default()
        .to_owned();
    if text.is_empty() && media.is_none() {
        return None;
    }
    let reply_to = message
        .context_info()
        .and_then(|context| context.stanza_id.clone())
        .filter(|id| !id.is_empty());
    Some(Body {
        text,
        reply_to,
        media,
    })
}

/// The attachment of a base message.
fn media(base: &wa::Message) -> Option<IncomingMedia> {
    if let Some(image) = base.image_message.as_option() {
        return Some(IncomingMedia {
            kind: MediaKind::Image,
            media_type: image
                .mimetype
                .clone()
                .unwrap_or_else(|| "image/jpeg".to_owned()),
            name: None,
            size: image.file_length,
            payload: MediaPayload::Image(Box::new(image.clone())),
        });
    }
    if let Some(video) = base.video_message.as_option() {
        return Some(IncomingMedia {
            kind: MediaKind::Video,
            media_type: video
                .mimetype
                .clone()
                .unwrap_or_else(|| "video/mp4".to_owned()),
            name: None,
            size: video.file_length,
            payload: MediaPayload::Video(Box::new(video.clone())),
        });
    }
    if let Some(audio) = base.audio_message.as_option() {
        return Some(IncomingMedia {
            kind: MediaKind::Audio,
            media_type: audio
                .mimetype
                .clone()
                .unwrap_or_else(|| "audio/ogg; codecs=opus".to_owned()),
            name: None,
            size: audio.file_length,
            payload: MediaPayload::Audio(Box::new(audio.clone())),
        });
    }
    if let Some(document) = base.document_message.as_option() {
        return Some(IncomingMedia {
            kind: MediaKind::Document,
            media_type: document
                .mimetype
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_owned()),
            name: document
                .file_name
                .clone()
                .or_else(|| document.title.clone())
                .filter(|name| !name.is_empty()),
            size: document.file_length,
            payload: MediaPayload::Document(Box::new(document.clone())),
        });
    }
    if let Some(sticker) = base.sticker_message.as_option() {
        return Some(IncomingMedia {
            kind: MediaKind::Sticker,
            media_type: sticker
                .mimetype
                .clone()
                .unwrap_or_else(|| "image/webp".to_owned()),
            name: None,
            size: sticker.file_length,
            payload: MediaPayload::Sticker(Box::new(sticker.clone())),
        });
    }
    None
}

/// The plugin's view of each message of a batch that has something to deliver.
pub(super) fn batch(messages: &[LibMessage]) -> Vec<Incoming> {
    messages.iter().filter_map(incoming).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use whatsapp_rust::buffa::MessageField;
    use whatsapp_rust::wacore::types::events::{
        Connected, Disconnected, LoggedOut, PairSuccess, PairingCode, PairingQrCode,
        PairingQrCodesExhausted, TemporaryBan,
    };

    use super::*;

    fn jid(text: &str) -> Jid {
        text.parse().unwrap()
    }

    // ---- events

    #[test]
    fn qr_codes_and_pair_codes_keep_their_payload_and_lifetime() {
        let qr = Event::PairingQrCode(
            PairingQrCode::builder()
                .code("2@abc,def".to_owned())
                .timeout(Duration::from_secs(60))
                .build(),
        );
        match event(&qr) {
            Some(WaEvent::PairingQr { code, timeout }) => {
                assert_eq!(code, "2@abc,def");
                assert_eq!(timeout, Duration::from_secs(60));
            }
            _ => panic!("not a QR code"),
        }
        let pair = Event::PairingCode(
            PairingCode::builder()
                .code("ABCD1234".to_owned())
                .timeout(Duration::from_secs(180))
                .build(),
        );
        match event(&pair) {
            Some(WaEvent::PairingCode { code, timeout }) => {
                assert_eq!(code, "ABCD1234");
                assert_eq!(timeout, Duration::from_secs(180));
            }
            _ => panic!("not a pair code"),
        }
    }

    #[test]
    fn qr_exhaustion_says_whether_the_client_closed() {
        for closed in [true, false] {
            let exhausted = Event::PairingQrCodesExhausted(
                PairingQrCodesExhausted::builder()
                    .disconnected(closed)
                    .build(),
            );
            assert!(matches!(
                event(&exhausted),
                Some(WaEvent::QrExhausted { closed: c }) if c == closed
            ));
        }
    }

    #[test]
    fn connection_events() {
        assert!(matches!(
            event(&Event::Connected(Connected::builder().build())),
            Some(WaEvent::Connected)
        ));
        assert!(matches!(
            event(&Event::PairSuccess(
                PairSuccess::builder()
                    .id(jid("15551234567@s.whatsapp.net"))
                    .lid(jid("100000012345678@lid"))
                    .business_name(String::new())
                    .platform("android".to_owned())
                    .build()
            )),
            Some(WaEvent::Paired)
        ));
        let logged_out = Event::LoggedOut(Box::new(
            LoggedOut::builder()
                .on_connect(false)
                .reason(whatsapp_rust::wacore::types::events::ConnectFailureReason::LoggedOut)
                .build(),
        ));
        assert!(matches!(event(&logged_out), Some(WaEvent::LoggedOut(_))));
    }

    #[test]
    fn a_temporary_ban_is_fatal_and_says_for_how_long() {
        let ban = Event::TemporaryBan(Box::new(
            TemporaryBan::builder()
                .code(whatsapp_rust::wacore::types::events::TempBanReason::SentTooManySameMessage)
                .expire(whatsapp_rust::chrono::TimeDelta::seconds(3600))
                .message("slow down".to_owned())
                .build(),
        ));
        let Some(WaEvent::Fatal(why)) = event(&ban) else {
            panic!("not fatal");
        };
        assert!(why.contains("temporarily banned"), "{why}");
        assert!(why.contains("3600 s"), "{why}");
        assert!(why.contains("slow down"), "{why}");
    }

    #[test]
    fn a_connect_failure_the_library_retries_is_a_disconnect() {
        use whatsapp_rust::wacore::types::events::{ConnectFailure, ConnectFailureReason};
        for reason in [
            ConnectFailureReason::ServiceUnavailable,
            ConnectFailureReason::InternalServerError,
        ] {
            let failure = Event::ConnectFailure(
                ConnectFailure::builder()
                    .reason(reason)
                    .message("try later".to_owned())
                    .build(),
            );
            assert!(
                matches!(event(&failure), Some(WaEvent::Disconnected(_))),
                "{reason:?}"
            );
        }
    }

    #[test]
    fn a_connect_failure_the_library_gives_up_on_is_fatal() {
        use whatsapp_rust::wacore::types::events::{ConnectFailure, ConnectFailureReason};
        let failure = Event::ConnectFailure(
            ConnectFailure::builder()
                .reason(ConnectFailureReason::AccountLocked)
                .message("locked".to_owned())
                .build(),
        );
        let Some(WaEvent::Fatal(why)) = event(&failure) else {
            panic!("not fatal");
        };
        assert!(why.contains("refused the connection"), "{why}");
        assert!(why.contains("locked"), "{why}");
    }

    #[test]
    fn events_the_plugin_has_no_use_for_are_ignored() {
        use whatsapp_rust::wacore::types::events::OfflineSyncCompleted;
        assert!(
            event(&Event::OfflineSyncCompleted(
                OfflineSyncCompleted::builder().count(3).build()
            ))
            .is_none()
        );
        // Disconnects are forwarded: the session decides whether they matter.
        let disconnected = Event::Disconnected(
            Disconnected::builder()
                .reason(whatsapp_rust::wacore::net::DisconnectReason::ReadError(
                    "connection reset".to_owned(),
                ))
                .build(),
        );
        assert!(matches!(
            event(&disconnected),
            Some(WaEvent::Disconnected(reason)) if reason.contains("connection reset")
        ));
    }

    #[test]
    fn the_handler_subscribes_to_what_it_translates() {
        // Messages arrive through the durability hook, not the event bus.
        assert!(!EVENT_KINDS.contains(&EventKind::Messages));
        assert!(EVENT_KINDS.contains(&EventKind::PairingQrCode));
        assert!(EVENT_KINDS.contains(&EventKind::LoggedOut));
    }

    // ---- messages

    fn info(chat: &str, sender: &str) -> MessageInfo {
        let mut info = MessageInfo {
            id: "3EB0C431A1B2".into(),
            push_name: "Stephen".into(),
            timestamp: whatsapp_rust::chrono::DateTime::from_timestamp(1_792_929_600, 0).unwrap(),
            ..Default::default()
        };
        info.source.chat = jid(chat);
        info.source.sender = jid(sender);
        info
    }

    fn text(text: &str) -> wa::Message {
        let mut message = wa::Message::default();
        message.conversation = Some(text.to_owned());
        message
    }

    fn convert(message: wa::Message, info: MessageInfo) -> Option<Incoming> {
        incoming(
            &LibMessage::builder()
                .message(Arc::new(message))
                .info(Arc::new(info))
                .build(),
        )
    }

    use std::sync::Arc;

    #[test]
    fn a_direct_text_message() {
        let mut info = info("15557654321@s.whatsapp.net", "15557654321@s.whatsapp.net");
        info.source.sender_alt = Some(jid("100000012345678@lid"));
        let incoming = convert(text("hello"), info).unwrap();
        assert_eq!(incoming.id, "3EB0C431A1B2");
        assert_eq!(incoming.chat, "15557654321@s.whatsapp.net");
        assert_eq!(incoming.kind, ChatKind::Direct);
        assert_eq!(incoming.sender_jid, "15557654321@s.whatsapp.net");
        assert_eq!(incoming.sender, Some(Address::phone("15557654321")));
        assert_eq!(incoming.sender_alt, Some(Address::lid("100000012345678")));
        assert!(!incoming.from_me);
        assert_eq!(incoming.push_name.as_deref(), Some("Stephen"));
        assert_eq!(incoming.text, "hello");
        assert_eq!(incoming.reply_to, None);
        assert_eq!(
            incoming.sent_at,
            Some(Timestamp::from_second(1_792_929_600).unwrap())
        );
        assert!(incoming.media.is_none());
    }

    #[test]
    fn devices_are_stripped_from_addresses() {
        let info = info("15557654321@s.whatsapp.net", "15557654321:7@s.whatsapp.net");
        let incoming = convert(text("hi"), info).unwrap();
        assert_eq!(incoming.sender_jid, "15557654321@s.whatsapp.net");
        assert_eq!(incoming.sender, Some(Address::phone("15557654321")));
    }

    #[test]
    fn a_lid_sender_is_a_lid_address() {
        let info = info("100000012345678@lid", "100000012345678@lid");
        let incoming = convert(text("hi"), info).unwrap();
        assert_eq!(incoming.sender, Some(Address::lid("100000012345678")));
        assert_eq!(incoming.kind, ChatKind::Direct);
    }

    #[test]
    fn chats_are_classified() {
        let kind = |chat: &str, is_group: bool| {
            let mut info = info(chat, "15557654321@s.whatsapp.net");
            info.source.is_group = is_group;
            convert(text("hi"), info).unwrap().kind
        };
        assert_eq!(kind("120363012345678901@g.us", true), ChatKind::Group);
        assert_eq!(kind("120363012345678901@g.us", false), ChatKind::Group);
        assert_eq!(kind("status@broadcast", false), ChatKind::Other);
        assert_eq!(kind("1203630@newsletter", false), ChatKind::Other);
        assert_eq!(kind("123456789@broadcast", false), ChatKind::Other);
        assert_eq!(kind("15557654321@s.whatsapp.net", false), ChatKind::Direct);
    }

    #[test]
    fn echoes_are_flagged() {
        let mut info = info("15557654321@s.whatsapp.net", "15551112222@s.whatsapp.net");
        info.source.is_from_me = true;
        assert!(convert(text("hi"), info).unwrap().from_me);
    }

    #[test]
    fn a_blank_push_name_is_no_name() {
        let mut info = info("15557654321@s.whatsapp.net", "15557654321@s.whatsapp.net");
        info.push_name = "  ".into();
        assert_eq!(convert(text("hi"), info).unwrap().push_name, None);
    }

    #[test]
    fn a_reply_names_the_message_it_answers() {
        let mut quoted = wa::message::ExtendedTextMessage::default();
        quoted.text = Some("yes".to_owned());
        let mut context = wa::ContextInfo::default();
        context.stanza_id = Some("3EB0PREV".to_owned());
        quoted.context_info = MessageField::some(context);
        let mut message = wa::Message::default();
        message.extended_text_message = MessageField::some(quoted);
        let incoming = convert(
            message,
            info("15557654321@s.whatsapp.net", "15557654321@s.whatsapp.net"),
        )
        .unwrap();
        assert_eq!(incoming.text, "yes");
        assert_eq!(incoming.reply_to.as_deref(), Some("3EB0PREV"));
    }

    #[test]
    fn media_carries_its_type_name_size_and_caption() {
        let source = || info("15557654321@s.whatsapp.net", "15557654321@s.whatsapp.net");

        let mut image = wa::message::ImageMessage::default();
        image.caption = Some("look".to_owned());
        image.mimetype = Some("image/png".to_owned());
        image.file_length = Some(2048);
        let mut message = wa::Message::default();
        message.image_message = MessageField::some(image);
        let incoming = convert(message, source()).unwrap();
        assert_eq!(incoming.text, "look");
        let media = incoming.media.unwrap();
        assert_eq!(media.kind, MediaKind::Image);
        assert_eq!(media.media_type, "image/png");
        assert_eq!(media.size, Some(2048));
        assert!(matches!(media.payload, MediaPayload::Image(_)));

        let mut document = wa::message::DocumentMessage::default();
        document.file_name = Some("report.pdf".to_owned());
        document.mimetype = Some("application/pdf".to_owned());
        let mut message = wa::Message::default();
        message.document_message = MessageField::some(document);
        let media = convert(message, source()).unwrap().media.unwrap();
        assert_eq!(media.kind, MediaKind::Document);
        assert_eq!(media.name.as_deref(), Some("report.pdf"));

        let mut audio = wa::message::AudioMessage::default();
        audio.file_length = Some(10);
        let mut message = wa::Message::default();
        message.audio_message = MessageField::some(audio);
        let incoming = convert(message, source()).unwrap();
        assert_eq!(incoming.text, "");
        let media = incoming.media.unwrap();
        assert_eq!(media.kind, MediaKind::Audio);
        assert_eq!(media.media_type, "audio/ogg; codecs=opus");

        let mut video = wa::message::VideoMessage::default();
        video.caption = Some("clip".to_owned());
        let mut message = wa::Message::default();
        message.video_message = MessageField::some(video);
        let incoming = convert(message, source()).unwrap();
        assert_eq!(incoming.text, "clip");
        assert_eq!(incoming.media.unwrap().media_type, "video/mp4");

        let mut message = wa::Message::default();
        message.sticker_message = MessageField::some(wa::message::StickerMessage::default());
        let media = convert(message, source()).unwrap().media.unwrap();
        assert_eq!(media.kind, MediaKind::Sticker);
        assert_eq!(media.media_type, "image/webp");
    }

    #[test]
    fn messages_with_nothing_to_deliver_are_skipped() {
        let source = || info("15557654321@s.whatsapp.net", "15557654321@s.whatsapp.net");
        // A reaction, which has no text and no media.
        let mut reaction = wa::Message::default();
        reaction.reaction_message = MessageField::some(wa::message::ReactionMessage::default());
        assert!(convert(reaction, source()).is_none());
        // An empty message.
        assert!(convert(wa::Message::default(), source()).is_none());
        // An empty text.
        assert!(convert(text(""), source()).is_none());
    }

    #[test]
    fn a_batch_keeps_only_what_has_content() {
        let source = info("15557654321@s.whatsapp.net", "15557654321@s.whatsapp.net");
        let messages: Vec<LibMessage> = [text("one"), wa::Message::default(), text("three")]
            .into_iter()
            .map(|message| {
                LibMessage::builder()
                    .message(Arc::new(message))
                    .info(Arc::new(source.clone()))
                    .build()
            })
            .collect();
        let texts: Vec<String> = batch(&messages).into_iter().map(|m| m.text).collect();
        assert_eq!(texts, ["one", "three"]);
    }
}
