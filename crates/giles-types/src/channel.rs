//! The [`Channel`] trait: a messaging service (PLAN sections 4.4, 5, 8.5).
//!
//! Channels never see model messages. They exchange [`InboundMessage`]s and [`OutboundMessage`]s,
//! which the core converts. Channel events are stored as JSON in the `inbox` table and outbound
//! messages in the `outbox` table, so these types serialize the same way as the message model
//! (`"type"` tags in `snake_case`, `None` fields omitted).

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::id::{ConversationId, EventId, MessageId, NativeId};
use crate::message::MediaRef;
use jiff::Timestamp;

/// A messaging service: web chat, IRC, email, WhatsApp.
///
/// Built-in channels are constructed from config; plugin channels are reached through the
/// `RemoteChannel` adapter.
#[async_trait]
pub trait Channel: Send + Sync {
    /// What the channel can do.
    fn info(&self) -> &ChannelInfo;

    /// Inbound events: messages and status changes. Delivery is at least once: every event must be
    /// acked with [`Channel::ack`] once the core has stored it, and unacked events come again when
    /// the stream is reopened.
    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>>;

    /// Acknowledges an event from [`Channel::events`]; the channel may forget it.
    async fn ack(&self, event: &EventId) -> Result<()>;

    /// Sends a message and returns its id on the channel. A recipient who can't be reached right
    /// now (e.g. an IRC account that isn't online) is [`Error::Unavailable`](crate::Error).
    async fn send(&self, msg: OutboundMessage) -> Result<MessageId>;
}

/// What a channel is and can do, declared once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// The identity namespace of the channel's native ids, e.g. `whatsapp` or `irc:libera.chat`
    /// (PLAN section 3.2). Instances in one namespace share identities. The core accepts it only
    /// if it starts with the channel component's own name.
    pub identity_namespace: String,
    /// How the channel wants assistant output delivered (PLAN section 6.9).
    pub output: OutputMode,
    /// Whether the channel can start a conversation with an identity, i.e. accepts
    /// [`SendTarget::Recipient`]. Proactive sends (jobs, login codes) need it.
    pub can_initiate: bool,
}

/// How a channel receives assistant output (PLAN section 6.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputMode {
    /// The channel streams live output.
    Deltas,
    /// The core sends whole chunks, flushed at paragraph boundaries outside code fences and at the
    /// end of the message.
    Blocks,
}

/// An inbound event from a channel. Every event has an id, unique within the channel instance,
/// that the core acks once it has stored the event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChannelEvent {
    /// A message arrived.
    Message {
        /// The event id.
        id: EventId,
        /// The message.
        message: InboundMessage,
    },
    /// The channel's connection state changed. Shown in `giles status` and the admin pages.
    Status {
        /// The event id.
        id: EventId,
        /// The new state.
        status: ChannelStatus,
    },
}

impl ChannelEvent {
    /// The event id, to ack with.
    pub fn id(&self) -> &EventId {
        match self {
            Self::Message { id, .. } | Self::Status { id, .. } => id,
        }
    }
}

/// A channel's connection state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChannelStatus {
    /// Connected and working.
    Connected,
    /// The operator must link or log in the channel, e.g. by scanning a QR code.
    NeedsAuth {
        /// What the operator must do.
        challenge: AuthChallenge,
        /// When the challenge stops working, if it expires.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at: Option<Timestamp>,
    },
    /// Not working.
    Error {
        /// What's wrong, for the operator.
        message: String,
    },
}

/// How the operator links a channel that reports [`ChannelStatus::NeedsAuth`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthChallenge {
    /// Scan a QR code with this payload.
    QrCode {
        /// The data to encode in the QR code.
        payload: String,
    },
    /// Enter this code on the device.
    PairCode {
        /// The code.
        code: String,
    },
    /// Open this URL.
    Url {
        /// The URL.
        url: String,
    },
}

/// A conversation on a channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationRef {
    /// The channel's id for the conversation.
    pub id: ConversationId,
    /// Direct or group.
    pub kind: ConversationKind,
    /// A display title, e.g. a group name or an email subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Whether a conversation is one-to-one. Routing rules can match on it (PLAN section 6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConversationKind {
    /// Between the agent and one person.
    Direct,
    /// Several people.
    Group,
}

/// A message a channel received.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Where it was sent.
    pub conversation: ConversationRef,
    /// Who sent it: the canonical native identifier (PLAN section 3.3).
    pub sender: NativeId,
    /// Other ids of the same sender, e.g. a WhatsApp LID next to the phone number. The sender is
    /// known if `sender` or any alias is linked to a user (PLAN section 3.2).
    pub sender_aliases: Vec<NativeId>,
    /// Whether the channel verified that `sender` sent it. The core drops unauthenticated
    /// messages.
    pub sender_authenticated: bool,
    /// The sender's display name, if the channel has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_display_name: Option<String>,
    /// The text, as the sender wrote it.
    pub text: String,
    /// Attachments, already in the blob store.
    pub attachments: Vec<MediaRef>,
    /// The message's own id on the channel, usable as an [`OutboundMessage::reply_to`] target.
    /// Not necessarily the event id: an email event id may be `UIDVALIDITY:UID`, while this is
    /// the `Message-ID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// The message this one replies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
    /// When the sender sent it, if the channel knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<Timestamp>,
}

impl InboundMessage {
    /// The sender and then its aliases: every id to look up when resolving the sender.
    pub fn sender_ids(&self) -> impl Iterator<Item = &NativeId> {
        std::iter::once(&self.sender).chain(&self.sender_aliases)
    }
}

/// A message for a channel to send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    /// Where to send it.
    pub target: SendTarget,
    /// The text, as CommonMark; the channel renders it (PLAN section 10).
    pub markdown: String,
    /// Attachments in the blob store.
    pub attachments: Vec<MediaRef>,
    /// The message this one replies to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<MessageId>,
}

/// Where an [`OutboundMessage`] goes. Serialized externally tagged:
/// `{"conversation": "#giles"}` or `{"recipient": "account:stephen"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SendTarget {
    /// Reply into an existing conversation.
    Conversation(ConversationId),
    /// Start or continue a direct conversation with an identity. Needs
    /// [`ChannelInfo::can_initiate`]. Proactive sends (jobs, approval prompts for job turns, login
    /// codes) use this.
    Recipient(NativeId),
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::id::MediaId;

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
            sender_display_name: None,
            text: "hi".into(),
            attachments: vec![MediaRef {
                id: MediaId::from_digest([0; 32]),
                media_type: "image/jpeg".into(),
                size: 5,
                name: None,
            }],
            message_id: Some(MessageId::new("3EB1").unwrap()),
            reply_to: Some(MessageId::new("3EB0").unwrap()),
            sent_at: Some("2026-10-22T12:00:00Z".parse().unwrap()),
        }
    }

    #[test]
    fn sender_ids_lists_the_sender_first() {
        let msg = inbound();
        let ids: Vec<&str> = msg.sender_ids().map(NativeId::as_str).collect();
        assert_eq!(ids, ["+15551234567", "123456789@lid"]);
    }

    #[test]
    fn message_event_json() {
        let event = ChannelEvent::Message {
            id: EventId::new("wamid.1").unwrap(),
            message: inbound(),
        };
        assert_eq!(event.id().as_str(), "wamid.1");
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            json!({
                "type": "message",
                "id": "wamid.1",
                "message": {
                    "conversation": {"id": "120363@g.us", "kind": "group", "title": "Family"},
                    "sender": "+15551234567",
                    "sender_aliases": ["123456789@lid"],
                    "sender_authenticated": true,
                    "text": "hi",
                    "attachments": [{"id": "0".repeat(64), "media_type": "image/jpeg", "size": 5}],
                    "message_id": "3EB1",
                    "reply_to": "3EB0",
                    "sent_at": "2026-10-22T12:00:00Z",
                },
            })
        );
        assert_eq!(serde_json::from_value::<ChannelEvent>(json).unwrap(), event);
    }

    #[test]
    fn status_event_json() {
        let event = ChannelEvent::Status {
            id: EventId::new("s1").unwrap(),
            status: ChannelStatus::NeedsAuth {
                challenge: AuthChallenge::QrCode {
                    payload: "2@abc".into(),
                },
                expires_at: None,
            },
        };
        assert_eq!(event.id().as_str(), "s1");
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json,
            json!({
                "type": "status",
                "id": "s1",
                "status": {
                    "type": "needs_auth",
                    "challenge": {"type": "qr_code", "payload": "2@abc"},
                },
            })
        );
        assert_eq!(serde_json::from_value::<ChannelEvent>(json).unwrap(), event);
        assert_eq!(
            serde_json::to_value(ChannelStatus::Connected).unwrap(),
            json!({"type": "connected"})
        );
    }

    #[test]
    fn outbound_message_json() {
        let msg = OutboundMessage {
            target: SendTarget::Recipient(NativeId::new("account:stephen").unwrap()),
            markdown: "**hi**".into(),
            attachments: vec![],
            reply_to: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(
            json,
            json!({
                "target": {"recipient": "account:stephen"},
                "markdown": "**hi**",
                "attachments": [],
            })
        );
        assert_eq!(
            serde_json::from_value::<OutboundMessage>(json).unwrap(),
            msg
        );
        assert_eq!(
            serde_json::to_value(SendTarget::Conversation(
                ConversationId::new("#giles").unwrap()
            ))
            .unwrap(),
            json!({"conversation": "#giles"})
        );
    }

    #[test]
    fn channel_info_json() {
        let info = ChannelInfo {
            identity_namespace: "irc:libera.chat".into(),
            output: OutputMode::Blocks,
            can_initiate: true,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            json!({
                "identity_namespace": "irc:libera.chat",
                "output": "blocks",
                "can_initiate": true,
            })
        );
        assert_eq!(serde_json::from_value::<ChannelInfo>(json).unwrap(), info);
    }
}
