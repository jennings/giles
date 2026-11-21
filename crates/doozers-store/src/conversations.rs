//! Conversations across sessions: what a channel shows as its chat list and chat history (the
//! web channel, PLAN 10), and archiving and deleting them. A conversation is every keyed session
//! with one channel instance and conversation id, whatever the agent, ended sessions included
//! (`/new` keeps them, PLAN 6.2).

use doozers_types::{AgentId, ConversationId, Timestamp, UserId};
use rusqlite::{Connection, OptionalExtension, params};

use crate::inbox::{self, InboxEntry};
use crate::messages::from_row;
use crate::time::to_millis;
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

/// What [`Store::delete_conversation`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationDeletion {
    /// The conversation is gone.
    Deleted {
        /// The agents its sessions were with, by id: with the channel instance and the
        /// conversation id, the keys of the workspaces it may have had.
        agents: Vec<AgentId>,
        /// Whether it was archived.
        archived: bool,
    },
    /// Nothing was deleted: inbound messages of the conversation are waiting for a turn or in
    /// one.
    Busy,
    /// The user has no such conversation on the channel instance.
    NotFound,
}

/// How a user stands to a conversation ([`Store::conversation_access`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationAccess {
    /// No session counted exists in it.
    Unstarted,
    /// Every session counted belongs to the user.
    Own,
    /// A session counted belongs to someone else, or to nobody.
    Foreign,
}

/// Whether `user` has a session in the conversation.
fn has_conversation(
    conn: &Connection,
    channel: &str,
    conversation: &str,
    user: &str,
) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM sessions
             WHERE channel = ?1 AND conversation = ?2 AND user_id = ?3)",
        )?
        .query_row(params![channel, conversation, user], |row| row.get(0))?)
}

impl Store {
    /// The conversations on channel instance `channel` whose sessions belong to `user`, archived
    /// ones or the others as `archived` says, most recently active first (ties by conversation
    /// id), at most `limit` of them.
    pub async fn channel_conversations(
        &self,
        channel: &str,
        user: &UserId,
        archived: bool,
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
                   AND EXISTS (SELECT 1 FROM archived_conversations a
                               WHERE a.channel = s.channel AND a.conversation = s.conversation)
                       = ?3
                 GROUP BY s.conversation
                 ORDER BY active DESC, s.conversation
                 LIMIT ?4",
            )?;
            let params = params![channel, user.as_str(), archived, limit];
            query_all(&mut stmt, params, |row| {
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

    /// How `user` stands to the conversation `conversation` on channel instance `channel`:
    /// [`Unstarted`](ConversationAccess::Unstarted) if it has no session,
    /// [`Own`](ConversationAccess::Own) if all of its sessions are the user's, and
    /// [`Foreign`](ConversationAccess::Foreign) if any belongs to someone else. Every session
    /// counts, ended ones too: a conversation whose id belongs to one user for good, such as a
    /// web chat's. A session with no user belongs to nobody, so it is foreign to everyone.
    pub async fn conversation_access(
        &self,
        channel: &str,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<ConversationAccess> {
        self.access(channel, conversation, user, false).await
    }

    /// [`conversation_access`](Self::conversation_access) counting only open sessions, so a
    /// conversation is free once the sessions in it have ended: for a conversation id that follows
    /// whoever holds it now, such as an IRC nickname.
    pub async fn open_conversation_access(
        &self,
        channel: &str,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<ConversationAccess> {
        self.access(channel, conversation, user, true).await
    }

    async fn access(
        &self,
        channel: &str,
        conversation: &ConversationId,
        user: &UserId,
        open_only: bool,
    ) -> Result<ConversationAccess> {
        let (channel, conversation, user) = (
            channel.to_owned(),
            conversation.to_string(),
            user.to_string(),
        );
        self.read(move |conn| {
            let (sessions, foreign): (i64, i64) = conn
                .prepare_cached(
                    "SELECT count(*), coalesce(sum(user_id IS NOT ?3), 0) FROM sessions
                     WHERE channel = ?1 AND conversation = ?2
                       AND (?4 = 0 OR ended_at IS NULL)",
                )?
                .query_row(params![channel, conversation, user, open_only], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?;
            Ok(match (sessions, foreign) {
                (0, _) => ConversationAccess::Unstarted,
                (_, 0) => ConversationAccess::Own,
                _ => ConversationAccess::Foreign,
            })
        })
        .await
    }

    /// The conversation `conversation` on channel instance `channel` as `user`'s: its last
    /// `limit` transcript rows, oldest first (every session's messages in session order, then by
    /// sequence number), and its inbound messages still waiting for a turn or in one. Only
    /// `user`'s sessions count, so a conversation of someone else's reads as empty. Summarized
    /// messages are included (this is a transcript to show, not a prompt) and empty slots come
    /// back as [`Entry::EmptySlot`](crate::Entry::EmptySlot).
    pub async fn conversation_transcript(
        &self,
        channel: &str,
        conversation: &ConversationId,
        user: &UserId,
        limit: u32,
    ) -> Result<ConversationTranscript> {
        let channel = channel.to_owned();
        let conversation = conversation.clone();
        let user = user.clone();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT * FROM (
                    SELECT m.id, m.seq, m.role, m.content, m.meta, m.tool_use_id, m.session_id
                    FROM messages m JOIN sessions s ON s.id = m.session_id
                    WHERE s.channel = ?1 AND s.conversation = ?2 AND s.user_id = ?4
                    ORDER BY m.session_id DESC, m.seq DESC LIMIT ?3)
                 ORDER BY session_id, seq",
            )?;
            let messages = query_all(
                &mut stmt,
                params![channel, conversation.as_str(), limit, user.as_str()],
                |row| {
                    Ok(ConversationMessage {
                        session: row.get(6)?,
                        message: from_row(row)?,
                    })
                },
            )?;
            let mut stmt = conn.prepare_cached(&format!(
                "{} WHERE state IN ('queued', 'started') AND session_id IN
                    (SELECT id FROM sessions
                     WHERE channel = ?1 AND conversation = ?2 AND user_id = ?3)
                 ORDER BY id",
                inbox::SELECT
            ))?;
            let waiting = query_all(
                &mut stmt,
                params![channel, conversation.as_str(), user.as_str()],
                inbox::from_row,
            )?;
            Ok(ConversationTranscript { messages, waiting })
        })
        .await
    }

    /// When the conversation `conversation` on channel instance `channel` was archived, or
    /// `None` if it isn't archived.
    pub async fn conversation_archived_at(
        &self,
        channel: &str,
        conversation: &ConversationId,
    ) -> Result<Option<Timestamp>> {
        let (channel, conversation) = (channel.to_owned(), conversation.to_string());
        self.read(move |conn| {
            let millis: Option<i64> = conn
                .prepare_cached(
                    "SELECT archived_at FROM archived_conversations
                     WHERE channel = ?1 AND conversation = ?2",
                )?
                .query_row(params![channel, conversation], |row| row.get(0))
                .optional()?;
            millis.map(crate::time::from_millis).transpose()
        })
        .await
    }

    /// Archives `user`'s conversation `conversation` on channel instance `channel` at `now`;
    /// one already archived keeps its time. Returns `false`, archiving nothing, if the user has
    /// no session in it.
    pub async fn archive_conversation(
        &self,
        channel: &str,
        conversation: &ConversationId,
        user: &UserId,
        now: Timestamp,
    ) -> Result<bool> {
        let (channel, conversation, user) = (
            channel.to_owned(),
            conversation.to_string(),
            user.to_string(),
        );
        self.write(move |conn| {
            if !has_conversation(conn, &channel, &conversation, &user)? {
                return Ok(false);
            }
            conn.prepare_cached(
                "INSERT INTO archived_conversations (channel, conversation, archived_at)
                 VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            )?
            .execute(params![channel, conversation, to_millis(now)])?;
            Ok(true)
        })
        .await
    }

    /// Takes `user`'s conversation `conversation` on channel instance `channel` out of the
    /// archive. Returns whether it was archived; `false`, changing nothing, if the user has no
    /// session in it.
    pub async fn unarchive_conversation(
        &self,
        channel: &str,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<bool> {
        let (channel, conversation, user) = (
            channel.to_owned(),
            conversation.to_string(),
            user.to_string(),
        );
        self.write(move |conn| {
            if !has_conversation(conn, &channel, &conversation, &user)? {
                return Ok(false);
            }
            Ok(conn
                .prepare_cached(
                    "DELETE FROM archived_conversations WHERE channel = ?1 AND conversation = ?2",
                )?
                .execute(params![channel, conversation])?
                > 0)
        })
        .await
    }

    /// Deletes `user`'s conversation `conversation` on channel instance `channel`, in one
    /// transaction: the user's sessions in it with their messages and summaries, the inbox rows
    /// routed to them (their text and dedupe keys), the outbox rows waiting to go into it, and
    /// its archive row. Usage rows stay, without their session. Refused while an inbound message
    /// of those sessions is `queued` or `started`. Workspaces are the core's: it deletes them
    /// through their plugins, by the keys this returns.
    pub async fn delete_conversation(
        &self,
        channel: &str,
        conversation: &ConversationId,
        user: &UserId,
    ) -> Result<ConversationDeletion> {
        let (channel, conversation, user) = (
            channel.to_owned(),
            conversation.to_string(),
            user.to_string(),
        );
        self.write(move |conn| {
            const SESSIONS: &str =
                "SELECT id FROM sessions WHERE channel = ?1 AND conversation = ?2 AND user_id = ?3";
            let keys = params![channel, conversation, user];
            let mut stmt = conn.prepare_cached(
                "SELECT DISTINCT agent FROM sessions
                 WHERE channel = ?1 AND conversation = ?2 AND user_id = ?3
                 ORDER BY agent",
            )?;
            let agents = query_all(&mut stmt, keys, |row| parse_id(row.get(0)?, AgentId::new))?;
            if agents.is_empty() {
                return Ok(ConversationDeletion::NotFound);
            }
            let busy: bool = conn
                .prepare_cached(&format!(
                    "SELECT EXISTS (SELECT 1 FROM inbox
                     WHERE state IN ('queued', 'started') AND session_id IN ({SESSIONS}))"
                ))?
                .query_row(keys, |row| row.get(0))?;
            if busy {
                return Ok(ConversationDeletion::Busy);
            }
            conn.prepare_cached(&format!(
                "DELETE FROM inbox WHERE session_id IN ({SESSIONS})"
            ))?
            .execute(keys)?;
            conn.prepare_cached(
                "DELETE FROM outbox
                 WHERE target = 'conversation' AND instance = ?1 AND recipient = ?2",
            )?
            .execute(params![channel, conversation])?;
            conn.prepare_cached(
                "DELETE FROM sessions WHERE channel = ?1 AND conversation = ?2 AND user_id = ?3",
            )?
            .execute(keys)?;
            let archived = conn
                .prepare_cached(
                    "DELETE FROM archived_conversations WHERE channel = ?1 AND conversation = ?2",
                )?
                .execute(params![channel, conversation])?
                > 0;
            Ok(ConversationDeletion::Deleted { agents, archived })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use doozers_types::{AgentId, Block, EventId};

    use super::*;
    use crate::messages::tests::{assistant, text, tool_use, user};
    use crate::sessions::tests::{at, store};
    use crate::{
        Entry, InboxState, NewInboxEvent, NewOutbox, OutboxTarget, SessionKey, SessionKind,
    };

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
            .channel_conversations("web", &owner(), false, 10)
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
            .channel_conversations("web", &owner(), false, 1)
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
                media: doozers_types::MediaRef {
                    id: doozers_types::MediaId::from_digest([1; 32]),
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
            .channel_conversations("web", &owner(), false, 10)
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
            .conversation_transcript("web", &a, &owner(), 100)
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
            .conversation_transcript("web", &a, &owner(), 3)
            .await
            .unwrap()
            .messages;
        assert_eq!(texts(&last), ["slot:t1", "u2", "a2"]);
        // Another user reads nothing of it.
        let stranger = UserId::new("stranger").unwrap();
        assert_eq!(
            store
                .conversation_transcript("web", &a, &stranger, 100)
                .await
                .unwrap(),
            ConversationTranscript {
                messages: Vec::new(),
                waiting: Vec::new(),
            }
        );
        assert_eq!(
            store
                .conversation_transcript("irc", &a, &owner(), 10)
                .await
                .unwrap(),
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
            .conversation_transcript("web", &a, &owner(), 10)
            .await
            .unwrap()
            .waiting;
        let ids: Vec<_> = waiting.iter().map(|w| w.event_id.as_str()).collect();
        assert_eq!(ids, ["started", "queued"]);
        assert_eq!(waiting[0].state, InboxState::Started);
        assert_eq!(waiting[1].received_at, at(3));
    }

    fn listed(conversations: &[ConversationActivity]) -> Vec<&str> {
        conversations
            .iter()
            .map(|c| c.conversation.as_str())
            .collect()
    }

    #[tokio::test]
    async fn archived_conversations_are_listed_apart_until_unarchived() {
        let (_dir, store) = store().await;
        for (agent, conversation, secs) in [("jeeves", "a", 0), ("bunter", "b", 1)] {
            store
                .open_session(
                    key(agent, "web", conversation),
                    SessionKind::Direct,
                    Some(owner()),
                    at(secs),
                )
                .await
                .unwrap();
        }
        let a = ConversationId::new("a").unwrap();
        let lists = async || {
            let active = store
                .channel_conversations("web", &owner(), false, 10)
                .await
                .unwrap();
            let archived = store
                .channel_conversations("web", &owner(), true, 10)
                .await
                .unwrap();
            (
                listed(&active)
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>(),
                listed(&archived)
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            store.conversation_archived_at("web", &a).await.unwrap(),
            None
        );

        assert!(
            store
                .archive_conversation("web", &a, &owner(), at(5))
                .await
                .unwrap()
        );
        assert_eq!(lists().await, (vec!["b".to_owned()], vec!["a".to_owned()]));
        // Archiving again keeps the first time; a `/new` session doesn't bring it back.
        assert!(
            store
                .archive_conversation("web", &a, &owner(), at(9))
                .await
                .unwrap()
        );
        assert_eq!(
            store.conversation_archived_at("web", &a).await.unwrap(),
            Some(at(5))
        );
        let (first, _) = store
            .open_session(
                key("jeeves", "web", "a"),
                SessionKind::Direct,
                Some(owner()),
                at(10),
            )
            .await
            .unwrap();
        store.end_session(first.id, at(11)).await.unwrap();
        store
            .open_session(
                key("jeeves", "web", "a"),
                SessionKind::Direct,
                Some(owner()),
                at(12),
            )
            .await
            .unwrap();
        assert_eq!(lists().await, (vec!["b".to_owned()], vec!["a".to_owned()]));
        // The same id on another instance is another conversation.
        assert_eq!(
            store.conversation_archived_at("irc", &a).await.unwrap(),
            None
        );

        assert!(
            store
                .unarchive_conversation("web", &a, &owner())
                .await
                .unwrap()
        );
        assert!(
            !store
                .unarchive_conversation("web", &a, &owner())
                .await
                .unwrap()
        );
        assert_eq!(
            store.conversation_archived_at("web", &a).await.unwrap(),
            None
        );
        assert_eq!(
            lists().await,
            (vec!["a".to_owned(), "b".to_owned()], Vec::<String>::new())
        );
    }

    #[tokio::test]
    async fn only_the_users_own_conversations_can_be_archived() {
        let (_dir, store) = store().await;
        store
            .open_session(
                key("jeeves", "web", "a"),
                SessionKind::Direct,
                Some(owner()),
                at(0),
            )
            .await
            .unwrap();
        let a = ConversationId::new("a").unwrap();
        let other = UserId::new("other").unwrap();
        let none = ConversationId::new("none").unwrap();
        assert!(
            !store
                .archive_conversation("web", &a, &other, at(1))
                .await
                .unwrap()
        );
        assert!(
            !store
                .archive_conversation("irc", &a, &owner(), at(1))
                .await
                .unwrap()
        );
        assert!(
            !store
                .archive_conversation("web", &none, &owner(), at(1))
                .await
                .unwrap()
        );
        assert_eq!(
            store.conversation_archived_at("web", &a).await.unwrap(),
            None
        );
        assert_eq!(
            store.conversation_archived_at("web", &none).await.unwrap(),
            None
        );

        // Nobody else takes it out of the archive either.
        assert!(
            store
                .archive_conversation("web", &a, &owner(), at(2))
                .await
                .unwrap()
        );
        assert!(
            !store
                .unarchive_conversation("web", &a, &other)
                .await
                .unwrap()
        );
        assert_eq!(
            store.conversation_archived_at("web", &a).await.unwrap(),
            Some(at(2))
        );
    }

    #[tokio::test]
    async fn deleting_a_conversation_removes_its_sessions_and_what_they_carry() {
        let (_dir, store) = store().await;
        let open = |agent: &str, conversation: &str, secs: i64| {
            store.open_session(
                key(agent, "web", conversation),
                SessionKind::Direct,
                Some(owner()),
                at(secs),
            )
        };
        // Conversation a: two agents, a `/new`, a summary, usage, inbox rows, an outbox reply.
        let (one, _) = open("jeeves", "a", 0).await.unwrap();
        store
            .append_messages(
                one.id,
                vec![user("secret plans", 1), assistant(vec![text("ok")], 2)],
            )
            .await
            .unwrap();
        store
            .insert_summary(one.id, 1, 2, "summary", at(3))
            .await
            .unwrap();
        store.end_session(one.id, at(4)).await.unwrap();
        let (two, _) = open("bunter", "a", 5).await.unwrap();
        store
            .append_messages(two.id, vec![user("more", 6)])
            .await
            .unwrap();
        // Conversation b stays.
        let (kept, _) = open("jeeves", "b", 7).await.unwrap();
        store
            .append_messages(kept.id, vec![user("secret elsewhere", 8)])
            .await
            .unwrap();
        let insert = |id: &str, session: SessionId| {
            store.inbox_insert(NewInboxEvent {
                instance: "web".into(),
                event_id: EventId::new(id).unwrap(),
                session: Some(session),
                user: Some(owner()),
                payload: serde_json::json!({ "text": id }),
                state: InboxState::Queued,
                received_at: at(9),
            })
        };
        let done = insert("done-a", one.id).await.unwrap().unwrap();
        let kept_row = insert("done-b", kept.id).await.unwrap().unwrap();
        store.inbox_start(vec![done, kept_row]).await.unwrap();
        store.inbox_finish(vec![done, kept_row]).await.unwrap();
        store
            .record_usage(crate::UsageRecord {
                at: at(9),
                provider: "fake".into(),
                model: "m".into(),
                agent: AgentId::new("jeeves").unwrap(),
                user: Some(owner()),
                session: Some(one.id),
                usage: doozers_types::Usage::default(),
                fallback: false,
            })
            .await
            .unwrap();
        let reply = |conversation: &str| NewOutbox {
            instance: "web".into(),
            target: OutboxTarget::Conversation(ConversationId::new(conversation).unwrap()),
            kind: "job_reply".into(),
            user: Some(owner()),
            payload: serde_json::json!({ "text": "reply" }),
            created_at: at(9),
            expires_at: at(100),
        };
        store.outbox_enqueue(reply("a")).await.unwrap();
        let kept_reply = store.outbox_enqueue(reply("b")).await.unwrap();
        let a = ConversationId::new("a").unwrap();
        store
            .archive_conversation("web", &a, &owner(), at(10))
            .await
            .unwrap();

        // Someone else's, or none at all.
        let other = UserId::new("other").unwrap();
        assert_eq!(
            store.delete_conversation("web", &a, &other).await.unwrap(),
            ConversationDeletion::NotFound
        );
        assert_eq!(
            store
                .delete_conversation("irc", &a, &owner())
                .await
                .unwrap(),
            ConversationDeletion::NotFound
        );

        assert_eq!(
            store
                .delete_conversation("web", &a, &owner())
                .await
                .unwrap(),
            ConversationDeletion::Deleted {
                agents: vec![
                    AgentId::new("bunter").unwrap(),
                    AgentId::new("jeeves").unwrap()
                ],
                archived: true,
            }
        );
        assert_eq!(store.session(one.id).await.unwrap(), None);
        assert_eq!(store.session(two.id).await.unwrap(), None);
        assert!(
            store
                .conversation_transcript("web", &a, &owner(), 10)
                .await
                .unwrap()
                .messages
                .is_empty()
        );
        assert_eq!(
            store.conversation_archived_at("web", &a).await.unwrap(),
            None
        );
        assert_eq!(store.inbox_entry(done).await.unwrap(), None);
        assert!(store.inbox_entry(kept_row).await.unwrap().is_some());
        let due: Vec<_> = store
            .outbox_due(at(50), 10)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(due, [kept_reply]);
        // Usage stays for the totals, without the session.
        assert_eq!(store.usage_by_agent(None).await.unwrap().len(), 1);
        let left = store
            .channel_conversations("web", &owner(), false, 10)
            .await
            .unwrap();
        assert_eq!(listed(&left), ["b"]);
        assert_eq!(
            store
                .delete_conversation("web", &a, &owner())
                .await
                .unwrap(),
            ConversationDeletion::NotFound
        );
    }

    #[tokio::test]
    async fn a_conversation_with_messages_waiting_is_not_deleted() {
        let (_dir, store) = store().await;
        let (session, _) = store
            .open_session(
                key("jeeves", "web", "a"),
                SessionKind::Direct,
                Some(owner()),
                at(0),
            )
            .await
            .unwrap();
        let row = store
            .inbox_insert(NewInboxEvent {
                instance: "web".into(),
                event_id: EventId::new("e1").unwrap(),
                session: Some(session.id),
                user: Some(owner()),
                payload: serde_json::json!({ "text": "hi" }),
                state: InboxState::Queued,
                received_at: at(1),
            })
            .await
            .unwrap()
            .unwrap();
        let a = ConversationId::new("a").unwrap();
        assert_eq!(
            store
                .delete_conversation("web", &a, &owner())
                .await
                .unwrap(),
            ConversationDeletion::Busy
        );
        store.inbox_start(vec![row]).await.unwrap();
        assert_eq!(
            store
                .delete_conversation("web", &a, &owner())
                .await
                .unwrap(),
            ConversationDeletion::Busy
        );
        assert!(store.session(session.id).await.unwrap().is_some());
        store.inbox_finish(vec![row]).await.unwrap();
        assert!(matches!(
            store
                .delete_conversation("web", &a, &owner())
                .await
                .unwrap(),
            ConversationDeletion::Deleted { .. }
        ));
    }

    #[tokio::test]
    async fn a_conversation_is_foreign_to_everyone_but_the_user_of_its_sessions() {
        let (_dir, store) = store().await;
        let ann = UserId::new("ann").unwrap();
        let bob = UserId::new("bob").unwrap();
        let access = |conversation: &str, user: &UserId| {
            let conversation = ConversationId::new(conversation).unwrap();
            let user = user.clone();
            let store = store.clone();
            async move {
                store
                    .conversation_access("web", &conversation, &user)
                    .await
                    .unwrap()
            }
        };
        let open = |agent: &str, conversation: &str, user: Option<&UserId>, secs: i64| {
            store.open_session(
                key(agent, "web", conversation),
                SessionKind::Direct,
                user.cloned(),
                at(secs),
            )
        };

        assert_eq!(access("a", &ann).await, ConversationAccess::Unstarted);
        let (first, _) = open("jeeves", "a", Some(&ann), 0).await.unwrap();
        assert_eq!(access("a", &ann).await, ConversationAccess::Own);
        assert_eq!(access("a", &bob).await, ConversationAccess::Foreign);
        // An ended session and another agent's session still make it hers.
        store.end_session(first.id, at(1)).await.unwrap();
        open("bunter", "a", Some(&ann), 2).await.unwrap();
        assert_eq!(access("a", &ann).await, ConversationAccess::Own);
        assert_eq!(access("a", &bob).await, ConversationAccess::Foreign);
        // Another conversation, and the same id on another instance, are others.
        assert_eq!(access("b", &bob).await, ConversationAccess::Unstarted);
        assert_eq!(
            store
                .conversation_access("irc", &ConversationId::new("a").unwrap(), &bob)
                .await
                .unwrap(),
            ConversationAccess::Unstarted
        );
        // One foreign session is enough, and a session with no user is nobody's.
        open("jeeves", "c", Some(&ann), 3).await.unwrap();
        open("bunter", "c", Some(&bob), 4).await.unwrap();
        assert_eq!(access("c", &ann).await, ConversationAccess::Foreign);
        assert_eq!(access("c", &bob).await, ConversationAccess::Foreign);
        open("jeeves", "d", None, 5).await.unwrap();
        assert_eq!(access("d", &ann).await, ConversationAccess::Foreign);

        // Counting only open sessions, a conversation is free once its sessions have ended: the
        // nickname of a direct conversation can pass to another user.
        let open_access = |conversation: &str, user: &UserId| {
            let conversation = ConversationId::new(conversation).unwrap();
            let user = user.clone();
            let store = store.clone();
            async move {
                store
                    .open_conversation_access("web", &conversation, &user)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(open_access("c", &ann).await, ConversationAccess::Foreign);
        let (second, _) = open("jeeves", "e", Some(&ann), 6).await.unwrap();
        assert_eq!(open_access("e", &bob).await, ConversationAccess::Foreign);
        store.end_session(second.id, at(7)).await.unwrap();
        assert_eq!(open_access("e", &bob).await, ConversationAccess::Unstarted);
        assert_eq!(access("e", &bob).await, ConversationAccess::Foreign);
        assert_eq!(open_access("a", &bob).await, ConversationAccess::Foreign);
    }
}
