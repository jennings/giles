//! Messages in and out of the transcript: the stored form of inbound messages (with their time
//! prefix, PLAN section 6.7) and the request history built from the store.

use giles_store::{Entry, History};
use giles_types::{Block, Message, MessageMeta, Role, Timestamp, UserId};
use jiff::tz::TimeZone;

use crate::config::{AgentConfig, Config};
use crate::sessions::{INTERRUPTED_RESULT, TurnMessage};

/// How the time prefix is formatted (`jiff` strftime): `[Thu 2026-10-22 14:03 PDT]`.
const PREFIX_FORMAT: &str = "%a %Y-%m-%d %H:%M %Z";

/// What introduces a compaction summary at the start of the history (TASKS.md 2.10).
pub const SUMMARY_INTRO: &str = "Summary of the earlier conversation:";

/// The time zone message times are shown in: the owner's (`[owner] timezone`, standing in for
/// the user's), else the agent's, else the system's (PLAN section 6.7; v1's only user is the
/// owner).
pub fn message_timezone(config: &Config, agent: &AgentConfig) -> TimeZone {
    config
        .owner
        .timezone
        .clone()
        .or_else(|| agent.timezone.clone())
        .unwrap_or_else(TimeZone::system)
}

/// The prefix that tells the model when a message was sent, e.g. `[Thu 2026-10-22 14:03 PDT]`.
/// Minutes are enough for a conversation, and leaving out seconds keeps the text short.
pub fn time_prefix(at: Timestamp, tz: &TimeZone) -> String {
    let zoned = at.to_zoned(tz.clone());
    format!("[{}]", zoned.strftime(PREFIX_FORMAT))
}

/// The stored form of an inbound message: a `User` message whose text starts with the time it
/// was sent (`sent_at`, else when the core received it). The prefix is part of the stored text,
/// so the transcript renders the same on every later request and the cache prefix holds.
/// `meta.at` is when the core received it (it drives `idle_reset`).
pub fn user_message(message: &TurnMessage, sender: Option<&UserId>, tz: &TimeZone) -> Message {
    let sent = message.message.sent_at.unwrap_or(message.received_at);
    let prefix = time_prefix(sent, tz);
    let text = &message.message.text;
    let text = if text.is_empty() {
        prefix
    } else {
        format!("{prefix} {text}")
    };
    let mut meta = MessageMeta::new(message.received_at);
    meta.sender = sender.cloned();
    Message {
        role: Role::User,
        content: vec![Block::text(text)],
        meta,
    }
}

/// The messages a request carries: the summary, if any, as a leading `User` message, then every
/// later message. An empty slot (only possible if filling it failed) is sent as an
/// `interrupted` error result, so the request still pairs every `ToolUse` with a result.
pub fn request_messages(history: History) -> Vec<Message> {
    let mut messages = Vec::with_capacity(history.messages.len() + 1);
    if let Some(summary) = history.summary {
        let meta = MessageMeta::new(summary.created_at);
        messages.push(Message {
            role: Role::User,
            content: vec![Block::text(format!(
                "{SUMMARY_INTRO}\n\n{}",
                summary.content
            ))],
            meta,
        });
    }
    for stored in history.messages {
        match stored.entry {
            Entry::Message(message) => messages.push(message),
            Entry::EmptySlot { tool_use_id } => {
                tracing::warn!(%tool_use_id, "an empty tool-result slot reached a request");
                messages.push(Message {
                    role: Role::User,
                    content: vec![Block::ToolResult {
                        tool_use_id,
                        content: vec![Block::text(INTERRUPTED_RESULT)],
                        is_error: true,
                    }],
                    meta: MessageMeta::new(Timestamp::UNIX_EPOCH),
                });
            }
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use giles_store::InboxId;
    use giles_store::{SessionId, StoredMessage, StoredMessageId, Summary, SummaryId};
    use giles_types::{
        ConversationId, ConversationKind, ConversationRef, EventId, InboundMessage, NativeId,
    };

    use super::*;

    fn turn_message(text: &str, sent_at: Option<Timestamp>) -> TurnMessage {
        TurnMessage {
            inbox: InboxId::new(1),
            event: EventId::new("e1").unwrap(),
            received_at: "2026-10-22T21:03:30Z".parse().unwrap(),
            message: InboundMessage {
                conversation: ConversationRef {
                    id: ConversationId::new("owner").unwrap(),
                    kind: ConversationKind::Direct,
                    title: None,
                },
                sender: NativeId::new("owner").unwrap(),
                sender_aliases: Vec::new(),
                sender_authenticated: true,
                sender_display_name: None,
                text: text.to_owned(),
                attachments: Vec::new(),
                message_id: None,
                reply_to: None,
                sent_at,
            },
        }
    }

    fn la() -> TimeZone {
        TimeZone::get("America/Los_Angeles").unwrap()
    }

    #[test]
    fn the_prefix_is_local_time_to_the_minute_with_the_zone() {
        let at: Timestamp = "2026-10-22T21:03:59Z".parse().unwrap();
        assert_eq!(time_prefix(at, &la()), "[Thu 2026-10-22 14:03 PDT]");
        assert_eq!(
            time_prefix(at, &TimeZone::UTC),
            "[Thu 2026-10-22 21:03 UTC]"
        );
        // Standard time after the switch.
        let winter: Timestamp = "2026-12-01T08:00:00Z".parse().unwrap();
        assert_eq!(time_prefix(winter, &la()), "[Tue 2026-12-01 00:00 PST]");
    }

    #[test]
    fn user_messages_carry_the_sent_time_and_the_sender() {
        let owner = UserId::new("owner").unwrap();
        let sent: Timestamp = "2026-10-22T20:59:00Z".parse().unwrap();
        let message = user_message(&turn_message("hello", Some(sent)), Some(&owner), &la());
        assert_eq!(message.role, Role::User);
        assert_eq!(
            message.content,
            [Block::text("[Thu 2026-10-22 13:59 PDT] hello")]
        );
        assert_eq!(message.meta.sender, Some(owner));
        assert_eq!(message.meta.at, "2026-10-22T21:03:30Z".parse().unwrap());

        // Without `sent_at`, the time it was received; without text, the prefix alone.
        let message = user_message(&turn_message("", None), None, &TimeZone::UTC);
        assert_eq!(message.content, [Block::text("[Thu 2026-10-22 21:03 UTC]")]);
        assert_eq!(message.meta.sender, None);
    }

    #[test]
    fn the_summary_leads_and_empty_slots_become_interrupted_results() {
        let at = Timestamp::UNIX_EPOCH;
        let history = History {
            summary: Some(Summary {
                id: SummaryId::new(1),
                session: SessionId::new(1),
                from_seq: 1,
                through_seq: 4,
                content: "They talked about tea.".to_owned(),
                created_at: at,
            }),
            messages: vec![
                StoredMessage {
                    id: StoredMessageId::new(5),
                    seq: 5,
                    entry: Entry::Message(Message {
                        role: Role::Assistant,
                        content: vec![Block::ToolUse {
                            id: "t1".to_owned(),
                            name: "x".to_owned(),
                            input: serde_json::json!({}),
                        }],
                        meta: MessageMeta::new(at),
                    }),
                },
                StoredMessage {
                    id: StoredMessageId::new(6),
                    seq: 6,
                    entry: Entry::EmptySlot {
                        tool_use_id: "t1".to_owned(),
                    },
                },
            ],
        };
        let messages = request_messages(history);
        assert_eq!(messages.len(), 3);
        assert_eq!(
            messages[0].content,
            [Block::text(
                "Summary of the earlier conversation:\n\nThey talked about tea."
            )]
        );
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(
            messages[2].content,
            [Block::ToolResult {
                tool_use_id: "t1".to_owned(),
                content: vec![Block::text("interrupted")],
                is_error: true,
            }]
        );
    }
}
