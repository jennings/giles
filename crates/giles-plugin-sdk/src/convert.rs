//! Conversions between `giles-types` and the `giles-proto` wire types used by channels, by moving
//! fields, following the mapping tables in `Docs/technical/protocols.md`.
//!
//! Wire to domain validates: ids are checked, "Always set" fields must be present, and unspecified
//! enum values and unknown `oneof` variants are refused, all as [`Error::InvalidRequest`] or
//! [`Error::InvalidId`]. Domain to wire fails only for a variant of a `#[non_exhaustive]` domain
//! enum this version doesn't know.
//!
//! The SDK uses one direction of each; the other is public so tests, and the core's
//! `RemoteChannel` adapter if it chooses to, can use the same rules.
//!
//! Blob references are converted as this SDK's handles: a wire `BlobRef` id must be a SHA-256
//! `MediaId`. The protocol allows any plugin-local handle, so these helpers refuse events and send
//! requests from plugins with other handles, and an id that parses as a `MediaId` is only the
//! plugin's claim about the content: whether to trust it (rather than hashing what `GetBlob`
//! returns) is the core's decision, not something these helpers settle.

use giles_proto::channel::v1 as pb;
use giles_proto::types::v1::{
    BlobRef, ConversationKind as PbConversationKind, ConversationRef as PbConversationRef,
};
use giles_types::{
    AuthChallenge, ChannelEvent, ChannelStatus, ConversationId, ConversationKind, ConversationRef,
    Error, EventId, InboundMessage, MediaId, MediaRef, MessageId, NativeId, OutboundMessage,
    OutputMode, Result, SendTarget, Timestamp,
};
use tonic::{Code, Status};

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// `MediaRef` to `BlobRef`.
pub fn media_to_proto(media: MediaRef) -> BlobRef {
    BlobRef {
        id: media.id.to_string(),
        media_type: media.media_type,
        size: media.size,
        name: media.name,
    }
}

/// `BlobRef` to `MediaRef`. The id must be a SHA-256 id, as this SDK's handles are (see the
/// module docs).
pub fn media_from_proto(blob: BlobRef) -> Result<MediaRef> {
    Ok(MediaRef {
        id: MediaId::new(blob.id)?,
        media_type: blob.media_type,
        size: blob.size,
        name: blob.name,
    })
}

/// `Timestamp` to `google.protobuf.Timestamp` (nanoseconds always in `0..1e9`).
pub fn timestamp_to_proto(at: Timestamp) -> prost_types::Timestamp {
    let nanos = at.as_nanosecond();
    prost_types::Timestamp {
        seconds: i64::try_from(nanos.div_euclid(NANOS_PER_SECOND))
            .expect("jiff timestamps fit in i64 seconds"),
        nanos: i32::try_from(nanos.rem_euclid(NANOS_PER_SECOND)).expect("less than 1e9"),
    }
}

/// `google.protobuf.Timestamp` to `Timestamp`. Nanoseconds outside `0..1e9` or a time outside
/// jiff's range are invalid.
pub fn timestamp_from_proto(at: prost_types::Timestamp) -> Result<Timestamp> {
    if !(0..1_000_000_000).contains(&at.nanos) {
        return Err(invalid(format!(
            "timestamp nanos {} out of range",
            at.nanos
        )));
    }
    // Checked first: jiff asserts on some out-of-range inputs instead of returning an error.
    if !(Timestamp::MIN.as_second()..=Timestamp::MAX.as_second()).contains(&at.seconds) {
        return Err(invalid(format!("timestamp {}s out of range", at.seconds)));
    }
    let nanos = i128::from(at.seconds) * NANOS_PER_SECOND + i128::from(at.nanos);
    Timestamp::from_nanosecond(nanos).map_err(|e| invalid(format!("timestamp: {e}")))
}

/// `ConversationRef` to its wire form.
pub fn conversation_to_proto(conversation: ConversationRef) -> Result<PbConversationRef> {
    let kind = match conversation.kind {
        ConversationKind::Direct => PbConversationKind::Direct,
        ConversationKind::Group => PbConversationKind::Group,
        other => return Err(invalid(format!("unsupported conversation kind {other:?}"))),
    };
    Ok(PbConversationRef {
        id: conversation.id.to_string(),
        kind: kind.into(),
        title: conversation.title,
    })
}

/// A wire `ConversationRef` to the domain type.
pub fn conversation_from_proto(conversation: PbConversationRef) -> Result<ConversationRef> {
    let kind = match PbConversationKind::try_from(conversation.kind) {
        Ok(PbConversationKind::Direct) => ConversationKind::Direct,
        Ok(PbConversationKind::Group) => ConversationKind::Group,
        _ => {
            return Err(invalid(format!(
                "conversation.kind: unspecified or unknown value {}",
                conversation.kind
            )));
        }
    };
    Ok(ConversationRef {
        id: ConversationId::new(conversation.id)?,
        kind,
        title: conversation.title,
    })
}

/// `OutputMode` to its wire enum.
pub fn output_mode_to_proto(output: OutputMode) -> Result<pb::OutputMode> {
    match output {
        OutputMode::Deltas => Ok(pb::OutputMode::Deltas),
        OutputMode::Blocks => Ok(pb::OutputMode::Blocks),
        other => Err(invalid(format!("unsupported output mode {other:?}"))),
    }
}

/// A wire `OutputMode` value to the domain enum.
pub fn output_mode_from_proto(output: i32) -> Result<OutputMode> {
    match pb::OutputMode::try_from(output) {
        Ok(pb::OutputMode::Deltas) => Ok(OutputMode::Deltas),
        Ok(pb::OutputMode::Blocks) => Ok(OutputMode::Blocks),
        _ => Err(invalid(format!(
            "output: unspecified or unknown value {output}"
        ))),
    }
}

/// `ChannelEvent` to its wire form.
pub fn event_to_proto(event: ChannelEvent) -> Result<pb::ChannelEvent> {
    let (id, event) = match event {
        ChannelEvent::Message { id, message } => (
            id,
            pb::channel_event::Event::Message(inbound_to_proto(message)?),
        ),
        ChannelEvent::Status { id, status } => (
            id,
            pb::channel_event::Event::Status(status_to_proto(status)?),
        ),
        other => return Err(invalid(format!("unsupported channel event {other:?}"))),
    };
    Ok(pb::ChannelEvent {
        event_id: id.to_string(),
        event: Some(event),
    })
}

/// A wire `ChannelEvent` to the domain type. An event with no variant (one this version doesn't
/// know) is an error; the core acks such an event by id and drops it (PLAN section 7.8).
pub fn event_from_proto(event: pb::ChannelEvent) -> Result<ChannelEvent> {
    let id = EventId::new(event.event_id)?;
    match event.event {
        Some(pb::channel_event::Event::Message(message)) => Ok(ChannelEvent::Message {
            id,
            message: inbound_from_proto(message)?,
        }),
        Some(pb::channel_event::Event::Status(status)) => Ok(ChannelEvent::Status {
            id,
            status: status_from_proto(status)?,
        }),
        None => Err(invalid(format!("event {id}: unknown variant"))),
    }
}

fn inbound_to_proto(message: InboundMessage) -> Result<pb::InboundMessage> {
    Ok(pb::InboundMessage {
        conversation: Some(conversation_to_proto(message.conversation)?),
        sender: message.sender.to_string(),
        sender_display_name: message.sender_display_name,
        text: message.text,
        attachments: message
            .attachments
            .into_iter()
            .map(media_to_proto)
            .collect(),
        reply_to: message.reply_to.map(|id| id.to_string()),
        sent_at: message.sent_at.map(timestamp_to_proto),
        sender_authenticated: message.sender_authenticated,
        sender_aliases: message
            .sender_aliases
            .into_iter()
            .map(|alias| alias.to_string())
            .collect(),
        message_id: message.message_id.map(|id| id.to_string()),
    })
}

fn inbound_from_proto(message: pb::InboundMessage) -> Result<InboundMessage> {
    let conversation = message
        .conversation
        .ok_or_else(|| invalid("message.conversation: missing"))?;
    Ok(InboundMessage {
        conversation: conversation_from_proto(conversation)?,
        sender: NativeId::new(message.sender)?,
        sender_aliases: message
            .sender_aliases
            .into_iter()
            .map(NativeId::new)
            .collect::<Result<_, _>>()?,
        sender_authenticated: message.sender_authenticated,
        sender_display_name: message.sender_display_name,
        text: message.text,
        attachments: message
            .attachments
            .into_iter()
            .map(media_from_proto)
            .collect::<Result<_>>()?,
        message_id: message.message_id.map(MessageId::new).transpose()?,
        reply_to: message.reply_to.map(MessageId::new).transpose()?,
        sent_at: message.sent_at.map(timestamp_from_proto).transpose()?,
    })
}

fn status_to_proto(status: ChannelStatus) -> Result<pb::ChannelStatus> {
    use pb::channel_status::State;
    let state = match status {
        ChannelStatus::Connected => State::Connected(pb::Connected {}),
        ChannelStatus::NeedsAuth {
            challenge,
            expires_at,
        } => {
            use pb::needs_auth::Challenge;
            let challenge = match challenge {
                AuthChallenge::QrCode { payload } => Challenge::QrCode(payload),
                AuthChallenge::PairCode { code } => Challenge::PairCode(code),
                AuthChallenge::Url { url } => Challenge::Url(url),
                other => return Err(invalid(format!("unsupported auth challenge {other:?}"))),
            };
            State::NeedsAuth(pb::NeedsAuth {
                challenge: Some(challenge),
                expires_at: expires_at.map(timestamp_to_proto),
            })
        }
        ChannelStatus::Error { message } => State::Error(message),
        other => return Err(invalid(format!("unsupported channel status {other:?}"))),
    };
    Ok(pb::ChannelStatus { state: Some(state) })
}

fn status_from_proto(status: pb::ChannelStatus) -> Result<ChannelStatus> {
    use pb::channel_status::State;
    match status.state {
        Some(State::Connected(pb::Connected {})) => Ok(ChannelStatus::Connected),
        Some(State::NeedsAuth(needs_auth)) => {
            use pb::needs_auth::Challenge;
            let challenge = match needs_auth.challenge {
                Some(Challenge::QrCode(payload)) => AuthChallenge::QrCode { payload },
                Some(Challenge::PairCode(code)) => AuthChallenge::PairCode { code },
                Some(Challenge::Url(url)) => AuthChallenge::Url { url },
                None => return Err(invalid("status.needs_auth.challenge: missing or unknown")),
            };
            Ok(ChannelStatus::NeedsAuth {
                challenge,
                expires_at: needs_auth
                    .expires_at
                    .map(timestamp_from_proto)
                    .transpose()?,
            })
        }
        Some(State::Error(message)) => Ok(ChannelStatus::Error { message }),
        None => Err(invalid("status.state: missing or unknown")),
    }
}

/// `OutboundMessage` to a `SendRequest`.
pub fn outbound_to_proto(message: OutboundMessage) -> Result<pb::SendRequest> {
    use pb::send_request::Target;
    let target = match message.target {
        SendTarget::Conversation(id) => Target::ConversationId(id.to_string()),
        SendTarget::Recipient(id) => Target::Recipient(id.to_string()),
        other => return Err(invalid(format!("unsupported send target {other:?}"))),
    };
    Ok(pb::SendRequest {
        target: Some(target),
        markdown: message.markdown,
        attachments: message
            .attachments
            .into_iter()
            .map(media_to_proto)
            .collect(),
        reply_to: message.reply_to.map(|id| id.to_string()),
    })
}

/// A `SendRequest` to an `OutboundMessage`.
pub fn outbound_from_proto(request: pb::SendRequest) -> Result<OutboundMessage> {
    use pb::send_request::Target;
    let target = match request.target {
        Some(Target::ConversationId(id)) => SendTarget::Conversation(ConversationId::new(id)?),
        Some(Target::Recipient(id)) => SendTarget::Recipient(NativeId::new(id)?),
        None => return Err(invalid("target: missing or unknown")),
    };
    Ok(OutboundMessage {
        target,
        markdown: request.markdown,
        attachments: request
            .attachments
            .into_iter()
            .map(media_from_proto)
            .collect::<Result<_>>()?,
        reply_to: request.reply_to.map(MessageId::new).transpose()?,
    })
}

/// A component error as a gRPC status: `InvalidId` and `InvalidRequest` are `INVALID_ARGUMENT`,
/// `NotFound` is `NOT_FOUND`, `Unavailable` is `UNAVAILABLE` (its `retry_after` doesn't cross the
/// wire), anything else is `UNKNOWN`.
pub fn status_from_error(error: Error) -> Status {
    match error {
        Error::InvalidId(e) => Status::invalid_argument(e.to_string()),
        Error::InvalidRequest(message) => Status::invalid_argument(message),
        Error::NotFound(message) => Status::not_found(message),
        Error::Unavailable { message, .. } => Status::unavailable(message),
        other => Status::unknown(other.to_string()),
    }
}

/// The reverse of [`status_from_error`]: `INVALID_ARGUMENT` is `InvalidRequest`, `NOT_FOUND` is
/// `NotFound`, `UNAVAILABLE` is `Unavailable`, anything else is `Other` carrying the status.
pub fn error_from_status(status: Status) -> Error {
    match status.code() {
        Code::InvalidArgument => Error::InvalidRequest(status.message().to_owned()),
        Code::NotFound => Error::NotFound(status.message().to_owned()),
        Code::Unavailable => Error::unavailable(status.message()),
        _ => Error::other(status),
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidRequest(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(n: u8) -> MediaRef {
        MediaRef {
            id: MediaId::from_digest([n; 32]),
            media_type: "image/png".into(),
            size: 5,
            name: n.is_multiple_of(2).then(|| "a.png".into()),
        }
    }

    fn inbound() -> InboundMessage {
        InboundMessage {
            conversation: ConversationRef {
                id: ConversationId::new("120363@g.us").unwrap(),
                kind: ConversationKind::Group,
                title: Some("Family".into()),
            },
            sender: NativeId::new("+15551234567").unwrap(),
            sender_aliases: vec![NativeId::new("123456789@lid").unwrap()],
            sender_authenticated: true,
            sender_display_name: Some(String::new()),
            text: "hi".into(),
            attachments: vec![media(1), media(2)],
            message_id: Some(MessageId::new("3EB1").unwrap()),
            reply_to: Some(MessageId::new("3EB0").unwrap()),
            sent_at: Some("2026-10-22T12:00:00.5Z".parse().unwrap()),
        }
    }

    fn round_trip(event: ChannelEvent) {
        let wire = event_to_proto(event.clone()).unwrap();
        assert_eq!(event_from_proto(wire).unwrap(), event);
    }

    #[test]
    fn events_round_trip() {
        round_trip(ChannelEvent::Message {
            id: EventId::new("wamid.1").unwrap(),
            message: inbound(),
        });
        let mut bare = inbound();
        bare.conversation.kind = ConversationKind::Direct;
        bare.conversation.title = None;
        bare.sender_aliases.clear();
        bare.sender_authenticated = false;
        bare.sender_display_name = None;
        bare.attachments.clear();
        bare.message_id = None;
        bare.reply_to = None;
        bare.sent_at = None;
        round_trip(ChannelEvent::Message {
            id: EventId::new("e2").unwrap(),
            message: bare,
        });

        let statuses = [
            ChannelStatus::Connected,
            ChannelStatus::Error {
                message: "logged out".into(),
            },
            ChannelStatus::NeedsAuth {
                challenge: AuthChallenge::QrCode {
                    payload: "2@abc".into(),
                },
                expires_at: Some("2026-10-22T12:01:00Z".parse().unwrap()),
            },
            ChannelStatus::NeedsAuth {
                challenge: AuthChallenge::PairCode {
                    code: "ABCD-1234".into(),
                },
                expires_at: None,
            },
            ChannelStatus::NeedsAuth {
                challenge: AuthChallenge::Url {
                    url: "https://example.com/link".into(),
                },
                expires_at: None,
            },
        ];
        for status in statuses {
            round_trip(ChannelEvent::Status {
                id: EventId::new("s1").unwrap(),
                status,
            });
        }
    }

    #[test]
    fn fields_move_to_their_wire_names() {
        let wire = event_to_proto(ChannelEvent::Message {
            id: EventId::new("e1").unwrap(),
            message: inbound(),
        })
        .unwrap();
        assert_eq!(wire.event_id, "e1");
        let Some(pb::channel_event::Event::Message(message)) = wire.event else {
            panic!("not a message");
        };
        let conversation = message.conversation.unwrap();
        assert_eq!(conversation.kind(), PbConversationKind::Group);
        assert_eq!(message.sender_display_name.as_deref(), Some(""));
        assert_eq!(message.attachments[0].id, media(1).id.as_str());
        assert_eq!(message.attachments[0].name, None);
        assert_eq!(
            message.sent_at,
            Some(prost_types::Timestamp {
                seconds: 1_792_670_400,
                nanos: 500_000_000
            })
        );
    }

    #[test]
    fn invalid_wire_events_are_refused() {
        let good = || {
            event_to_proto(ChannelEvent::Message {
                id: EventId::new("e1").unwrap(),
                message: inbound(),
            })
            .unwrap()
        };
        fn message(event: &mut pb::ChannelEvent) -> &mut pb::InboundMessage {
            match event.event.as_mut() {
                Some(pb::channel_event::Event::Message(message)) => message,
                _ => unreachable!(),
            }
        }
        type Corrupt = Box<dyn Fn(&mut pb::ChannelEvent)>;
        let cases: Vec<(&str, Corrupt)> = vec![
            ("empty event id", Box::new(|e| e.event_id.clear())),
            ("no variant", Box::new(|e| e.event = None)),
            (
                "no conversation",
                Box::new(move |e| message(e).conversation = None),
            ),
            (
                "unspecified kind",
                Box::new(move |e| message(e).conversation.as_mut().unwrap().kind = 0),
            ),
            (
                "unknown kind",
                Box::new(move |e| message(e).conversation.as_mut().unwrap().kind = 9),
            ),
            (
                "empty conversation id",
                Box::new(move |e| message(e).conversation.as_mut().unwrap().id.clear()),
            ),
            ("empty sender", Box::new(move |e| message(e).sender.clear())),
            (
                "empty alias",
                Box::new(move |e| message(e).sender_aliases.push(String::new())),
            ),
            (
                "bad blob id",
                Box::new(move |e| message(e).attachments[0].id = "h1".into()),
            ),
            (
                "empty reply_to",
                Box::new(move |e| message(e).reply_to = Some(String::new())),
            ),
            (
                "negative nanos",
                Box::new(move |e| message(e).sent_at.as_mut().unwrap().nanos = -1),
            ),
            (
                "out of range",
                Box::new(move |e| message(e).sent_at.as_mut().unwrap().seconds = i64::MAX),
            ),
        ];
        for (name, corrupt) in cases {
            let mut event = good();
            corrupt(&mut event);
            let err = event_from_proto(event).unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest(_) | Error::InvalidId(_)),
                "{name}: {err:?}"
            );
        }

        for status in [
            pb::ChannelStatus { state: None },
            pb::ChannelStatus {
                state: Some(pb::channel_status::State::NeedsAuth(pb::NeedsAuth {
                    challenge: None,
                    expires_at: None,
                })),
            },
        ] {
            let event = pb::ChannelEvent {
                event_id: "s1".into(),
                event: Some(pb::channel_event::Event::Status(status)),
            };
            assert!(matches!(
                event_from_proto(event),
                Err(Error::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn timestamps_before_the_epoch_keep_positive_nanos() {
        let at: Timestamp = "1969-12-31T23:59:59.25Z".parse().unwrap();
        let wire = timestamp_to_proto(at);
        assert_eq!(
            wire,
            prost_types::Timestamp {
                seconds: -1,
                nanos: 250_000_000
            }
        );
        assert_eq!(timestamp_from_proto(wire).unwrap(), at);
    }

    #[test]
    fn outbound_messages_round_trip_with_either_target() {
        for target in [
            SendTarget::Conversation(ConversationId::new("#giles").unwrap()),
            SendTarget::Recipient(NativeId::new("account:stephen").unwrap()),
        ] {
            let message = OutboundMessage {
                target,
                markdown: "**hi**".into(),
                attachments: vec![media(3)],
                reply_to: Some(MessageId::new("m1").unwrap()),
            };
            let wire = outbound_to_proto(message.clone()).unwrap();
            assert_eq!(outbound_from_proto(wire).unwrap(), message);
        }
        let wire = outbound_to_proto(OutboundMessage {
            target: SendTarget::Recipient(NativeId::new("bob").unwrap()),
            markdown: String::new(),
            attachments: vec![],
            reply_to: None,
        })
        .unwrap();
        assert_eq!(
            wire.target,
            Some(pb::send_request::Target::Recipient("bob".into()))
        );
    }

    #[test]
    fn invalid_send_requests_are_refused() {
        use pb::send_request::Target;
        let request = |target| pb::SendRequest {
            target,
            markdown: "hi".into(),
            attachments: vec![],
            reply_to: None,
        };
        for bad in [
            request(None),
            request(Some(Target::ConversationId(String::new()))),
            request(Some(Target::Recipient(String::new()))),
            pb::SendRequest {
                attachments: vec![BlobRef::default()],
                ..request(Some(Target::Recipient("bob".into())))
            },
        ] {
            let err = outbound_from_proto(bad.clone()).unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest(_) | Error::InvalidId(_)),
                "{bad:?}: {err:?}"
            );
        }
    }

    #[test]
    fn output_modes_map_and_unspecified_is_refused() {
        for mode in [OutputMode::Deltas, OutputMode::Blocks] {
            let wire = output_mode_to_proto(mode).unwrap();
            assert_eq!(output_mode_from_proto(wire.into()).unwrap(), mode);
        }
        assert!(output_mode_from_proto(0).is_err());
        assert!(output_mode_from_proto(7).is_err());
    }

    #[test]
    fn errors_and_statuses_map_both_ways() {
        let cases = [
            (Error::InvalidRequest("bad".into()), Code::InvalidArgument),
            (Error::NotFound("x".into()), Code::NotFound),
            (Error::unavailable("offline"), Code::Unavailable),
            (Error::other("boom"), Code::Unknown),
        ];
        for (error, code) in cases {
            let status = status_from_error(error);
            assert_eq!(status.code(), code);
            let back = error_from_status(status);
            match code {
                Code::InvalidArgument => {
                    assert!(matches!(back, Error::InvalidRequest(m) if m == "bad"))
                }
                Code::NotFound => assert!(matches!(back, Error::NotFound(m) if m == "x")),
                Code::Unavailable => {
                    assert!(back.is_retryable());
                    assert_eq!(back.to_string(), "unavailable: offline");
                }
                _ => assert!(matches!(back, Error::Other(_))),
            }
        }
        let invalid_id = Error::from(NativeId::new("").unwrap_err());
        assert_eq!(status_from_error(invalid_id).code(), Code::InvalidArgument);
    }
}
