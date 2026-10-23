//! Conversations across sessions: what a channel shows as its chat list and chat history (the
//! web channel, PLAN 10). A conversation is every keyed session with one channel instance and
//! conversation id, whatever the agent, ended sessions included (`/new` keeps them, PLAN 6.2).

use giles_types::{ConversationId, Timestamp, UserId};
use rusqlite::params;

use crate::inbox::{self, InboxEntry};
use crate::messages::from_row;
use crate::{Result, SessionId, Store, StoredMessage, get_time, parse_id, query_all};

/// A conversation on one channel instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationActivity {
    /// The conversation id.
    pub conversation: ConversationId,
    /// When its first session started.
    pub started_at: Timestamp,
    /// When its last message was stored, or its last session started if that has none.
    pub last_active_at: Timestamp,
    /// The text of its first user message, if that starts with a text block.
    pub first_text: Option<String>,
}

/// A transcript row with the session it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationMessage {
    /// The session.
    pub session: SessionId,
    /// The row.
    pub message: StoredMessage,
}

/// A conversation as a chat page shows it ([`Store::conversation_transcript`]), read in one
/// transaction, so an inbound message is in exactly one of the two lists, or in both while its
/// turn runs.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationTranscript {
    /// The last transcript rows, oldest first.
    pub messages: Vec<ConversationMessage>,
    /// The inbox rows routed to the conversation's sessions that are `queued` (waiting for a
    /// turn) or `started` (taken by a running turn, which stores them in the transcript), oldest
    /// first.
    pub waiting: Vec<InboxEntry>,
}

impl Store {
    /// The conversations on channel instance `channel` whose sessions belong to `user`, most
    /// recently active first (ties by conversation id), at most `limit` of them.
    pub async fn channel_conversations(
        &self,
        channel: &str,
        user: &UserId,
        limit: u32,
    ) -> Result<Vec<ConversationActivity>> {
        let channel = channel.to_owned();
        let user = user.clone();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT s.conversation, min(s.started_at),
                    max(coalesce(
                        (SELECT m.created_at FROM messages m WHERE m.session_id = s.id
                         ORDER BY m.seq DESC LIMIT 1),
                        s.started_at)) AS active,
                    (SELECT json_extract(m.content, '$[0].text')
                     FROM messages m JOIN sessions f ON f.id = m.session_id
                     WHERE f.channel = ?1 AND f.conversation = s.conversation
                       AND m.role = 'user' AND m.tool_use_id IS NULL
                       AND json_extract(m.content, '$[0].type') = 'text'
                     ORDER BY f.id, m.seq LIMIT 1)
                 FROM sessions s
                 WHERE s.channel = ?1 AND s.user_id = ?2
                 GROUP BY s.conversation
                 ORDER BY active DESC, s.conversation
                 LIMIT ?3",
            )?;
            query_all(&mut stmt, params![channel, user.as_str(), limit], |row| {
                Ok(ConversationActivity {
                    conversation: parse_id(row.get(0)?, ConversationId::new)?,
                    started_at: get_time(row, 1)?,
                    last_active_at: get_time(row, 2)?,
                    first_text: row.get(3)?,
                })
            })
        })
        .await
    }

    /// The conversation `conversation` on channel instance `channel`: its last `limit`
    /// transcript rows, oldest first (every session's messages in session order, then by
    /// sequence number), and its inbound messages still waiting for a turn or in one. Summarized
    /// messages are included (this is a transcript to show, not a prompt) and empty slots come
    /// back as [`Entry::EmptySlot`](crate::Entry::EmptySlot).
    pub async fn conversation_transcript(
        &self,
        channel: &str,
        conversation: &ConversationId,
        limit: u32,
    ) -> Result<ConversationTranscript> {
        let channel = channel.to_owned();
        let conversation = conversation.clone();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT * FROM (
                    SELECT m.id, m.seq, m.role, m.content, m.meta, m.tool_use_id, m.session_id
                    FROM messages m JOIN sessions s ON s.id = m.session_id
                    WHERE s.channel = ?1 AND s.conversation = ?2
                    ORDER BY m.session_id DESC, m.seq DESC LIMIT ?3)
                 ORDER BY session_id, seq",
            )?;
            let messages = query_all(
                &mut stmt,
                params![channel, conversation.as_str(), limit],
                |row| {
                    Ok(ConversationMessage {
                        session: row.get(6)?,
                        message: from_row(row)?,
                    })
                },
            )?;
            let mut stmt = conn.prepare_cached(&format!(
                "{} WHERE state IN ('queued', 'started') AND session_id IN
                    (SELECT id FROM sessions WHERE channel = ?1 AND conversation = ?2)
                 ORDER BY id",
                inbox::SELECT
            ))?;
            let waiting = query_all(
                &mut stmt,
                params![channel, conversation.as_str()],
                inbox::from_row,
            )?;
            Ok(ConversationTranscript { messages, waiting })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use giles_types::{AgentId, Block, EventId};

    use super::*;
    use crate::messages::tests::{assistant, text, tool_use, user};
    use crate::sessions::tests::{at, store};
    use crate::{Entry, InboxState, NewInboxEvent, SessionKey, SessionKind};

    fn key(agent: &str, channel: &str, conversation: &str) -> SessionKey {
        SessionKey {
            agent: AgentId::new(agent).unwrap(),
            channel: channel.into(),
            conversation: ConversationId::new(conversation).unwrap(),
        }
    }

    fn owner() -> UserId {
        UserId::new("owner").unwrap()
    }

    fn texts(rows: &[ConversationMessage]) -> Vec<String> {
        rows.iter()
            .map(|row| match &row.message.entry {
                Entry::Message(m) => match &m.content[0] {
                    Block::Text { text } => text.clone(),
                    Block::ToolUse { id, .. } => format!("use:{id}"),
                    Block::ToolResult { tool_use_id, .. } => format!("result:{tool_use_id}"),
                    other => format!("{:?}", other.kind()),
                },
                Entry::EmptySlot { tool_use_id } => format!("slot:{tool_use_id}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn conversations_are_listed_by_last_activity_with_their_first_user_text() {
        let (_dir, store) = store().await;
        let (a, _) = store
            .open_session(
                key("jeeves", "web", "a"),
                SessionKind::Direct,
                Some(owner()),
                at(0),
            )
            .await
            .unwrap();
        store
            .append_messages(
                a.id,
                vec![user("first in a", 1), assistant(vec![text("hi")], 2)],
            )
            .await
            .unwrap();
        // `/new`: a second session of the same conversation, active later.
        store.end_session(a.id, at(3)).await.unwrap();
        let (a2, _) = store
            .open_session(
                key("jeeves", "web", "a"),
                SessionKind::Direct,
                Some(owner()),
                at(4),
            )
            .await
            .unwrap();
        store
            .append_messages(a2.id, vec![user("again", 50)])
            .await
            .unwrap();
        // Conversation b: no messages yet, started after a's last message of its first session.
        store
            .open_session(
                key("bunter", "web", "b"),
                SessionKind::Direct,
                Some(owner()),
                at(10),
            )
            .await
            .unwrap();
        // Not listed: another channel, another user.
        store
            .open_session(
                key("jeeves", "irc", "c"),
                SessionKind::Direct,
                Some(owner()),
                at(60),
            )
            .await
            .unwrap();
        let other = UserId::new("other").unwrap();
        store
            .open_session(
                key("jeeves", "web", "d"),
                SessionKind::Direct,
                Some(other),
                at(60),
            )
            .await
            .unwrap();

        let listed = store
            .channel_conversations("web", &owner(), 10)
            .await
            .unwrap();
        assert_eq!(
            listed,
            [
                ConversationActivity {
                    conversation: ConversationId::new("a").unwrap(),
                    started_at: at(0),
                    last_active_at: at(50),
                    first_text: Some("first in a".into()),
                },
                ConversationActivity {
                    conversation: ConversationId::new("b").unwrap(),
                    started_at: at(10),
                    last_active_at: at(10),
                    first_text: None,
                },
            ]
        );
        let first = store
            .channel_conversations("web", &owner(), 1)
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].conversation.as_str(), "a");
    }

    #[tokio::test]
    async fn first_text_skips_tool_results_and_non_text_starts() {
        let (_dir, store) = store().await;
        let (s, _) = store
            .open_session(
                key("jeeves", "web", "a"),
                SessionKind::Direct,
                Some(owner()),
                at(0),
            )
            .await
            .unwrap();
        let mut image_first = user("caption", 1);
        image_first.content.insert(
            0,
            Block::Image {
                media: giles_types::MediaRef {
                    id: giles_types::MediaId::from_digest([1; 32]),
                    media_type: "image/png".into(),
                    size: 1,
                    name: None,
                },
            },
        );
        let appended = store
            .append_messages(s.id, vec![assistant(vec![tool_use("t1")], 1)])
            .await
            .unwrap();
        store
            .fill_slot(
                appended[0].slots[0].id,
                vec![text("tool says")],
                false,
                at(2),
            )
            .await
            .unwrap();
        store
            .append_messages(s.id, vec![image_first, user("real first", 3)])
            .await
            .unwrap();
        let listed = store
            .channel_conversations("web", &owner(), 10)
            .await
            .unwrap();
        assert_eq!(listed[0].first_text.as_deref(), Some("real first"));
    }

    #[tokio::test]
    async fn transcripts_span_sessions_and_keep_the_newest() {
        let (_dir, store) = store().await;
        let (one, _) = store
            .open_session(
                key("jeeves", "web", "a"),
                SessionKind::Direct,
                Some(owner()),
                at(0),
            )
            .await
            .unwrap();
        let appended = store
            .append_messages(
                one.id,
                vec![user("u1", 1), assistant(vec![tool_use("t1")], 2)],
            )
            .await
            .unwrap();
        store.end_session(one.id, at(3)).await.unwrap();
        let (two, _) = store
            .open_session(
                key("bunter", "web", "a"),
                SessionKind::Direct,
                Some(owner()),
                at(4),
            )
            .await
            .unwrap();
        store
            .append_messages(two.id, vec![user("u2", 5), assistant(vec![text("a2")], 6)])
            .await
            .unwrap();
        // Another conversation's rows never appear.
        let (other, _) = store
            .open_session(
                key("jeeves", "web", "b"),
                SessionKind::Direct,
                Some(owner()),
                at(7),
            )
            .await
            .unwrap();
        store
            .append_messages(other.id, vec![user("elsewhere", 8)])
            .await
            .unwrap();
        // A summary doesn't hide the messages it covers.
        store
            .insert_summary(two.id, 1, 2, "summary", at(9))
            .await
            .unwrap();

        let a = ConversationId::new("a").unwrap();
        let all = store
            .conversation_transcript("web", &a, 100)
            .await
            .unwrap()
            .messages;
        assert_eq!(texts(&all), ["u1", "use:t1", "slot:t1", "u2", "a2"]);
        assert_eq!(
            all.iter().map(|r| r.session).collect::<Vec<_>>(),
            [one.id, one.id, one.id, two.id, two.id]
        );
        assert_eq!(all[2].message.id, appended[1].slots[0].id);

        let last = store
            .conversation_transcript("web", &a, 3)
            .await
            .unwrap()
            .messages;
        assert_eq!(texts(&last), ["slot:t1", "u2", "a2"]);
        assert_eq!(
            store.conversation_transcript("irc", &a, 10).await.unwrap(),
            ConversationTranscript {
                messages: Vec::new(),
                waiting: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn transcripts_list_inbound_messages_waiting_for_or_in_a_turn() {
        let (_dir, store) = store().await;
        let open = |agent: &str, conversation: &str| {
            store.open_session(
                key(agent, "web", conversation),
                SessionKind::Direct,
                Some(owner()),
                at(0),
            )
        };
        let (one, _) = open("jeeves", "a").await.unwrap();
        let (two, _) = open("bunter", "a").await.unwrap();
        let (other, _) = open("jeeves", "b").await.unwrap();
        let insert = |id: &str, session: SessionId, secs: i64| {
            store.inbox_insert(NewInboxEvent {
                instance: "web".into(),
                event_id: EventId::new(id).unwrap(),
                session: Some(session),
                user: Some(owner()),
                payload: serde_json::json!({ "text": id }),
                state: InboxState::Queued,
                received_at: at(secs),
            })
        };
        let done = insert("done", one.id, 1).await.unwrap().unwrap();
        let started = insert("started", one.id, 2).await.unwrap().unwrap();
        insert("queued", two.id, 3).await.unwrap();
        insert("elsewhere", other.id, 4).await.unwrap();
        store.inbox_start(vec![done, started]).await.unwrap();
        store.inbox_finish(vec![done]).await.unwrap();

        let a = ConversationId::new("a").unwrap();
        let waiting = store
            .conversation_transcript("web", &a, 10)
            .await
            .unwrap()
            .waiting;
        let ids: Vec<_> = waiting.iter().map(|w| w.event_id.as_str()).collect();
        assert_eq!(ids, ["started", "queued"]);
        assert_eq!(waiting[0].state, InboxState::Started);
        assert_eq!(waiting[1].received_at, at(3));
    }
}
