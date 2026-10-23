//! `messages` and `summaries`: session transcripts, tool-result slots, and compaction summaries
//! (PLAN 6.3, 6.8).

use giles_types::{Block, Message, MessageMeta, Role, Timestamp};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::sessions::SessionId;
use crate::time::to_millis;
use crate::{Error, Result, Store, get_time, query_all, query_opt, row_id};

row_id!(
    /// A row in `messages`: a stored message or a tool-result slot.
    StoredMessageId
);

row_id!(
    /// A row in `summaries`.
    SummaryId
);

/// What [`Store::append_messages`] stored for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appended {
    /// The message's row.
    pub id: StoredMessageId,
    /// Its sequence number in the session.
    pub seq: i64,
    /// One empty result slot per `ToolUse` block of an assistant message, in block order; empty
    /// for other messages.
    pub slots: Vec<Slot>,
}

/// An empty tool-result slot: a `User` message reserved for the result of one `ToolUse`
/// (PLAN 6.3). Fill it with [`Store::fill_slot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// The slot's row; pass it to [`Store::fill_slot`].
    pub id: StoredMessageId,
    /// Its sequence number, right after the assistant message and earlier slots.
    pub seq: i64,
    /// The `ToolUse` block's id.
    pub tool_use_id: String,
}

/// A message as loaded from the transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    /// The row.
    pub id: StoredMessageId,
    /// The sequence number in its session.
    pub seq: i64,
    /// The message, or an empty slot.
    pub entry: Entry,
}

/// The content of a transcript row.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// A message. A filled slot is a `User` message with one `ToolResult` block.
    Message(Message),
    /// A tool-result slot not filled yet. A turn fills every slot before its next provider
    /// request, so these appear only while a tool runs, or after a crash until startup recovery
    /// fills them.
    EmptySlot {
        /// The `ToolUse` block's id.
        tool_use_id: String,
    },
}

/// A compaction summary (PLAN 6.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    /// The row.
    pub id: SummaryId,
    /// The session it summarizes.
    pub session: SessionId,
    /// The first sequence number it covers.
    pub from_seq: i64,
    /// The last sequence number it covers.
    pub through_seq: i64,
    /// The summary text.
    pub content: String,
    /// When it was written.
    pub created_at: Timestamp,
}

/// What a prompt is built from: the newest summary, if any, and every message after it.
#[derive(Debug, Clone, PartialEq)]
pub struct History {
    /// The summary with the highest `through_seq` (the newest among equals).
    pub summary: Option<Summary>,
    /// Messages with `seq > summary.through_seq` (all of them without a summary), in order.
    pub messages: Vec<StoredMessage>,
}

fn role_str(role: Role) -> Result<&'static str> {
    match role {
        Role::User => Ok("user"),
        Role::Assistant => Ok("assistant"),
        other => Err(Error::invalid(format!("unsupported role {other:?}"))),
    }
}

pub(crate) fn parse_role(value: &str) -> Result<Role> {
    match value {
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        other => Err(Error::corrupt(format!("role {other:?}"))),
    }
}

/// A message serialized for insertion.
struct Prepared {
    role: &'static str,
    content: String,
    meta: String,
    at: i64,
    /// `ToolUse` ids of an assistant message: one slot each.
    tool_uses: Vec<String>,
    /// The `meta` of its slots until they're filled: only the time.
    slot_meta: String,
}

impl Prepared {
    fn new(message: &Message) -> Result<Self> {
        let tool_uses = if message.role == Role::Assistant {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    Block::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            role: role_str(message.role)?,
            content: serde_json::to_string(&message.content)?,
            meta: serde_json::to_string(&message.meta)?,
            at: to_millis(message.meta.at),
            tool_uses,
            slot_meta: serde_json::to_string(&MessageMeta::new(message.meta.at))?,
        })
    }
}

const SELECT: &str = "SELECT id, seq, role, content, meta, tool_use_id FROM messages";

/// Maps a row whose first six columns are those of [`SELECT`].
pub(crate) fn from_row(row: &Row<'_>) -> Result<StoredMessage> {
    let content: Option<String> = row.get(3)?;
    let entry = match content {
        Some(content) => Entry::Message(Message {
            role: parse_role(&row.get::<_, String>(2)?)?,
            content: serde_json::from_str(&content)?,
            meta: serde_json::from_str(&row.get::<_, String>(4)?)?,
        }),
        None => Entry::EmptySlot {
            tool_use_id: row
                .get::<_, Option<String>>(5)?
                .ok_or_else(|| Error::corrupt("a message without content or tool_use_id"))?,
        },
    };
    Ok(StoredMessage {
        id: row.get(0)?,
        seq: row.get(1)?,
        entry,
    })
}

fn require_session(conn: &Connection, session: SessionId) -> Result<()> {
    let exists = conn
        .prepare_cached("SELECT 1 FROM sessions WHERE id = ?1")?
        .query_row([session], |_| Ok(()))
        .optional()?;
    exists.ok_or_else(|| Error::not_found(format!("session {session}")))
}

fn last_seq(conn: &Connection, session: SessionId) -> Result<i64> {
    Ok(conn
        .prepare_cached("SELECT coalesce(max(seq), 0) FROM messages WHERE session_id = ?1")?
        .query_row([session], |row| row.get(0))?)
}

/// The JSON `content` and `meta` of a filled slot.
fn slot_result(
    tool_use_id: String,
    content: Vec<Block>,
    is_error: bool,
    at: Timestamp,
) -> Result<(String, String)> {
    let blocks = [Block::ToolResult {
        tool_use_id,
        content,
        is_error,
    }];
    Ok((
        serde_json::to_string(&blocks)?,
        serde_json::to_string(&MessageMeta::new(at))?,
    ))
}

/// Fills the empty slots in `session` (every session if `None`) with an error result reading
/// `text`. Returns the sessions that had any.
fn fill_empty(
    conn: &Connection,
    session: Option<SessionId>,
    text: &str,
    at: Timestamp,
) -> Result<Vec<SessionId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, session_id, tool_use_id FROM messages
         WHERE content IS NULL AND (?1 IS NULL OR session_id = ?1)
         ORDER BY session_id, seq",
    )?;
    let empty = query_all(&mut stmt, [session], |row| {
        Ok((
            row.get::<_, StoredMessageId>(0)?,
            row.get::<_, SessionId>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut update = conn.prepare_cached(
        "UPDATE messages SET content = ?2, meta = ?3, created_at = ?4 WHERE id = ?1",
    )?;
    let mut sessions = Vec::new();
    for (id, session, tool_use_id) in empty {
        let content = vec![Block::Text {
            text: text.to_owned(),
        }];
        let (content, meta) = slot_result(tool_use_id, content, true, at)?;
        update.execute(params![id, content, meta, to_millis(at)])?;
        if sessions.last() != Some(&session) {
            sessions.push(session);
        }
    }
    Ok(sessions)
}

const SELECT_SUMMARY: &str =
    "SELECT id, session_id, from_seq, through_seq, content, created_at FROM summaries";

fn summary_from_row(row: &Row<'_>) -> Result<Summary> {
    Ok(Summary {
        id: row.get(0)?,
        session: row.get(1)?,
        from_seq: row.get(2)?,
        through_seq: row.get(3)?,
        content: row.get(4)?,
        created_at: get_time(row, 5)?,
    })
}

impl Store {
    /// Appends `messages` to the session's transcript in one transaction, numbering them after
    /// its last message. Each assistant message gets one empty result slot per `ToolUse` block,
    /// numbered right after it and before the next message, so the transcript invariant (every
    /// `ToolUse` has a `ToolResult`, PLAN 6.3) holds once the slots are filled. `created_at` is
    /// `meta.at`. Fails with [`Error::NotFound`] if the session doesn't exist; ended sessions
    /// accept messages (a turn may finish after `/new`).
    pub async fn append_messages(
        &self,
        session: SessionId,
        messages: Vec<Message>,
    ) -> Result<Vec<Appended>> {
        let prepared = messages
            .iter()
            .map(Prepared::new)
            .collect::<Result<Vec<_>>>()?;
        self.write(move |conn| {
            require_session(conn, session)?;
            let mut seq = last_seq(conn, session)?;
            let mut insert = conn.prepare_cached(
                "INSERT INTO messages
                    (session_id, seq, role, content, meta, tool_use_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut out = Vec::with_capacity(prepared.len());
            for message in prepared {
                seq += 1;
                insert.execute(params![
                    session,
                    seq,
                    message.role,
                    message.content,
                    message.meta,
                    None::<String>,
                    message.at
                ])?;
                let id = StoredMessageId(conn.last_insert_rowid());
                let message_seq = seq;
                let mut slots = Vec::with_capacity(message.tool_uses.len());
                for tool_use_id in message.tool_uses {
                    seq += 1;
                    insert.execute(params![
                        session,
                        seq,
                        "user",
                        None::<String>,
                        message.slot_meta,
                        tool_use_id,
                        message.at
                    ])?;
                    slots.push(Slot {
                        id: StoredMessageId(conn.last_insert_rowid()),
                        seq,
                        tool_use_id,
                    });
                }
                out.push(Appended {
                    id,
                    seq: message_seq,
                    slots,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Fills an empty slot with `ToolResult { tool_use_id, content, is_error }` at `at`.
    /// [`Error::NotFound`] if `slot` isn't a slot; [`Error::Conflict`] if it's already filled.
    pub async fn fill_slot(
        &self,
        slot: StoredMessageId,
        content: Vec<Block>,
        is_error: bool,
        at: Timestamp,
    ) -> Result<()> {
        self.write(move |conn| {
            let found = conn
                .prepare_cached(
                    "SELECT tool_use_id, content IS NULL FROM messages
                     WHERE id = ?1 AND tool_use_id IS NOT NULL",
                )?
                .query_row([slot], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
                })
                .optional()?;
            let (tool_use_id, empty) =
                found.ok_or_else(|| Error::not_found(format!("tool-result slot {slot}")))?;
            if !empty {
                return Err(Error::conflict(format!("slot {slot} is already filled")));
            }
            let (content, meta) = slot_result(tool_use_id, content, is_error, at)?;
            conn.prepare_cached(
                "UPDATE messages SET content = ?2, meta = ?3, created_at = ?4 WHERE id = ?1",
            )?
            .execute(params![slot, content, meta, to_millis(at)])?;
            Ok(())
        })
        .await
    }

    /// Fills every empty slot of `session` with the error result `text` (`cancelled`, `denied`,
    /// `iteration limit`, ...: PLAN 6.3). Returns `true` if there were any.
    pub async fn fill_empty_slots(
        &self,
        session: SessionId,
        text: impl Into<String>,
        at: Timestamp,
    ) -> Result<bool> {
        let text = text.into();
        self.write(move |conn| Ok(!fill_empty(conn, Some(session), &text, at)?.is_empty()))
            .await
    }

    /// Startup recovery: fills every empty slot in every session with the error result `text`
    /// (`interrupted`) and returns the sessions that had any, oldest first.
    pub async fn fill_all_empty_slots(
        &self,
        text: impl Into<String>,
        at: Timestamp,
    ) -> Result<Vec<SessionId>> {
        let text = text.into();
        self.write(move |conn| fill_empty(conn, None, &text, at))
            .await
    }

    /// The session's prompt history: its newest summary and the messages after it.
    /// [`Error::NotFound`] if the session doesn't exist.
    pub async fn load_history(&self, session: SessionId) -> Result<History> {
        self.read(move |conn| {
            require_session(conn, session)?;
            let mut stmt = conn.prepare_cached(&format!(
                "{SELECT_SUMMARY} WHERE session_id = ?1 ORDER BY through_seq DESC, id DESC LIMIT 1"
            ))?;
            let summary = query_opt(&mut stmt, [session], summary_from_row)?;
            let after = summary.as_ref().map_or(0, |s| s.through_seq);
            let mut stmt = conn.prepare_cached(&format!(
                "{SELECT} WHERE session_id = ?1 AND seq > ?2 ORDER BY seq"
            ))?;
            let messages = query_all(&mut stmt, params![session, after], from_row)?;
            Ok(History { summary, messages })
        })
        .await
    }

    /// The last `limit` messages of the session, in order (a job's `dm_excerpt`, PLAN 6.11).
    /// The cut may fall between an assistant message and its slots; callers that send the
    /// excerpt to a model drop leading slots.
    pub async fn recent_messages(
        &self,
        session: SessionId,
        limit: u32,
    ) -> Result<Vec<StoredMessage>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT * FROM ({SELECT} WHERE session_id = ?1 ORDER BY seq DESC LIMIT ?2)
                 ORDER BY seq"
            ))?;
            query_all(&mut stmt, params![session, limit], from_row)
        })
        .await
    }

    /// Stores a summary of messages `from_seq..=through_seq`. The range must be non-empty,
    /// start at 1 or later, end at or before the last message, and not separate an assistant
    /// message from its slots (the message after `through_seq` can't be a slot); otherwise
    /// [`Error::InvalidRequest`].
    pub async fn insert_summary(
        &self,
        session: SessionId,
        from_seq: i64,
        through_seq: i64,
        content: impl Into<String>,
        at: Timestamp,
    ) -> Result<Summary> {
        let content = content.into();
        if from_seq < 1 || from_seq > through_seq {
            return Err(Error::invalid(format!(
                "summary range {from_seq}..={through_seq}"
            )));
        }
        self.write(move |conn| {
            require_session(conn, session)?;
            let last = last_seq(conn, session)?;
            if through_seq > last {
                return Err(Error::invalid(format!(
                    "summary ends at {through_seq}, after the last message ({last})"
                )));
            }
            let next_is_slot = conn
                .prepare_cached(
                    "SELECT tool_use_id IS NOT NULL FROM messages
                     WHERE session_id = ?1 AND seq = ?2",
                )?
                .query_row(params![session, through_seq + 1], |row| {
                    row.get::<_, bool>(0)
                })
                .optional()?
                .unwrap_or(false);
            if next_is_slot {
                return Err(Error::invalid(format!(
                    "summary ends at {through_seq}, between a tool use and its result"
                )));
            }
            conn.prepare_cached(
                "INSERT INTO summaries (session_id, from_seq, through_seq, content, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?
            .execute(params![
                session,
                from_seq,
                through_seq,
                content,
                to_millis(at)
            ])?;
            Ok(Summary {
                id: SummaryId(conn.last_insert_rowid()),
                session,
                from_seq,
                through_seq,
                content,
                created_at: crate::time::from_millis(to_millis(at))?,
            })
        })
        .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use giles_types::UserId;
    use serde_json::json;

    use super::*;
    use crate::sessions::SessionKind;
    use crate::sessions::tests::{at, key, store};

    pub(crate) fn text(text: &str) -> Block {
        Block::Text { text: text.into() }
    }

    pub(crate) fn user(content: &str, secs: i64) -> Message {
        Message {
            role: Role::User,
            content: vec![text(content)],
            meta: MessageMeta::new(at(secs)),
        }
    }

    pub(crate) fn assistant(content: Vec<Block>, secs: i64) -> Message {
        let mut meta = MessageMeta::new(at(secs));
        meta.provider = Some("anthropic".into());
        meta.model = Some("claude".into());
        Message {
            role: Role::Assistant,
            content,
            meta,
        }
    }

    pub(crate) fn tool_use(id: &str) -> Block {
        Block::ToolUse {
            id: id.into(),
            name: "memory_search".into(),
            input: json!({ "query": "secret input" }),
        }
    }

    pub(crate) async fn session(store: &Store, conversation: &str) -> SessionId {
        let owner = UserId::new("owner").unwrap();
        let (session, _) = store
            .open_session(key(conversation), SessionKind::Direct, Some(owner), at(0))
            .await
            .unwrap();
        session.id
    }

    fn message(entry: &StoredMessage) -> &Message {
        match &entry.entry {
            Entry::Message(message) => message,
            Entry::EmptySlot { tool_use_id } => panic!("empty slot {tool_use_id}"),
        }
    }

    #[tokio::test]
    async fn appending_numbers_messages_and_reserves_one_slot_per_tool_use() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let first = store.append_messages(s, vec![user("hi", 1)]).await.unwrap();
        assert_eq!((first[0].seq, first[0].slots.len()), (1, 0));

        let reply = assistant(vec![text("let me look"), tool_use("a"), tool_use("b")], 2);
        let appended = store
            .append_messages(s, vec![reply.clone(), user("also", 3)])
            .await
            .unwrap();
        assert_eq!(appended[0].seq, 2);
        let slots: Vec<_> = appended[0]
            .slots
            .iter()
            .map(|slot| (slot.seq, slot.tool_use_id.as_str()))
            .collect();
        assert_eq!(slots, [(3, "a"), (4, "b")]);
        assert_eq!((appended[1].seq, appended[1].slots.len()), (5, 0));

        let history = store.load_history(s).await.unwrap();
        assert_eq!(history.summary, None);
        let seqs: Vec<_> = history.messages.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, [1, 2, 3, 4, 5]);
        assert_eq!(message(&history.messages[1]), &reply);
        assert_eq!(
            history.messages[2].entry,
            Entry::EmptySlot {
                tool_use_id: "a".into()
            }
        );
        assert_eq!(history.messages[3].id, appended[0].slots[1].id);

        let session = store.session(s).await.unwrap().unwrap();
        assert_eq!(session.last_message_at, Some(at(3)));
    }

    #[tokio::test]
    async fn tool_use_in_a_user_message_gets_no_slot() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let mut odd = user("x", 1);
        odd.content.push(tool_use("a"));
        let appended = store.append_messages(s, vec![odd]).await.unwrap();
        assert!(appended[0].slots.is_empty());
    }

    #[tokio::test]
    async fn appending_to_a_missing_session_fails() {
        let (_dir, store) = store().await;
        let err = store
            .append_messages(SessionId::new(42), vec![user("hi", 1)])
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }

    #[tokio::test]
    async fn a_slot_is_filled_once_with_its_tool_result() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let appended = store
            .append_messages(s, vec![assistant(vec![tool_use("a")], 1)])
            .await
            .unwrap();
        let slot = appended[0].slots[0].id;

        store
            .fill_slot(slot, vec![text("found it")], false, at(5))
            .await
            .unwrap();
        let err = store
            .fill_slot(slot, vec![text("again")], false, at(6))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");

        // Not a slot: the assistant message itself, and a row that doesn't exist.
        for id in [appended[0].id, StoredMessageId::new(999)] {
            let err = store.fill_slot(id, vec![], true, at(6)).await.unwrap_err();
            assert!(matches!(err, Error::NotFound(_)), "{err}");
        }

        let history = store.load_history(s).await.unwrap();
        let filled = message(&history.messages[1]);
        assert_eq!(filled.role, Role::User);
        assert_eq!(filled.meta, MessageMeta::new(at(5)));
        assert_eq!(
            filled.content,
            [Block::ToolResult {
                tool_use_id: "a".into(),
                content: vec![text("found it")],
                is_error: false,
            }]
        );
    }

    #[tokio::test]
    async fn empty_slots_get_synthetic_error_results() {
        let (_dir, store) = store().await;
        let s1 = session(&store, "c1").await;
        let s2 = session(&store, "c2").await;
        let s3 = session(&store, "c3").await;
        let a = store
            .append_messages(s1, vec![assistant(vec![tool_use("a"), tool_use("b")], 1)])
            .await
            .unwrap();
        store
            .append_messages(s2, vec![assistant(vec![tool_use("c")], 1)])
            .await
            .unwrap();
        store
            .append_messages(s3, vec![assistant(vec![tool_use("d")], 1)])
            .await
            .unwrap();
        store
            .fill_slot(a[0].slots[0].id, vec![text("ok")], false, at(2))
            .await
            .unwrap();

        assert!(
            store
                .fill_empty_slots(s1, "cancelled", at(3))
                .await
                .unwrap()
        );
        assert!(
            !store
                .fill_empty_slots(s1, "cancelled", at(4))
                .await
                .unwrap()
        );
        let history = store.load_history(s1).await.unwrap();
        assert_eq!(
            message(&history.messages[2]).content,
            [Block::ToolResult {
                tool_use_id: "b".into(),
                content: vec![text("cancelled")],
                is_error: true,
            }]
        );
        // The filled slot kept its real result.
        assert!(matches!(
            &message(&history.messages[1]).content[0],
            Block::ToolResult {
                is_error: false,
                ..
            }
        ));

        let recovered = store
            .fill_all_empty_slots("interrupted", at(9))
            .await
            .unwrap();
        assert_eq!(recovered, [s2, s3]);
        assert!(
            store
                .fill_all_empty_slots("interrupted", at(9))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn history_starts_after_the_newest_summary() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let messages = (1..=6).map(|i| user(&format!("m{i}"), i)).collect();
        store.append_messages(s, messages).await.unwrap();

        store
            .insert_summary(s, 1, 2, "first two", at(10))
            .await
            .unwrap();
        let newest = store
            .insert_summary(s, 1, 4, "first four", at(11))
            .await
            .unwrap();
        // An older, shorter summary written later doesn't win.
        store
            .insert_summary(s, 1, 3, "first three", at(12))
            .await
            .unwrap();

        let history = store.load_history(s).await.unwrap();
        assert_eq!(history.summary, Some(newest.clone()));
        assert_eq!(newest.content, "first four");
        let seqs: Vec<_> = history.messages.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, [5, 6]);
    }

    #[tokio::test]
    async fn summaries_reject_bad_ranges() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        store
            .append_messages(
                s,
                vec![
                    user("q", 1),
                    assistant(vec![tool_use("a")], 2),
                    user("r", 3),
                ],
            )
            .await
            .unwrap();
        // seq 1 user, 2 assistant, 3 slot, 4 user.
        for (from, through) in [(0, 1), (3, 2), (1, 5), (1, 2)] {
            let err = store
                .insert_summary(s, from, through, "x", at(9))
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::InvalidRequest(_)),
                "{from}..={through}: {err}"
            );
        }
        store.insert_summary(s, 1, 3, "ok", at(9)).await.unwrap();
        store.insert_summary(s, 1, 4, "ok", at(9)).await.unwrap();
        let err = store
            .insert_summary(SessionId::new(99), 1, 1, "x", at(9))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }

    #[tokio::test]
    async fn recent_messages_are_the_tail_in_order() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let messages = (1..=5).map(|i| user(&format!("m{i}"), i)).collect();
        store.append_messages(s, messages).await.unwrap();
        let tail = store.recent_messages(s, 2).await.unwrap();
        let seqs: Vec<_> = tail.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, [4, 5]);
        assert_eq!(store.recent_messages(s, 10).await.unwrap().len(), 5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_appends_get_distinct_contiguous_seqs() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..20 {
            let store = store.clone();
            tasks.spawn(async move {
                let reply = assistant(vec![tool_use(&format!("t{i}"))], i);
                store.append_messages(s, vec![user("q", i), reply]).await
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.unwrap().unwrap();
        }
        let history = store.load_history(s).await.unwrap();
        let seqs: Vec<_> = history.messages.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, (1..=60).collect::<Vec<_>>());
        // Each assistant message is directly followed by its slot.
        for pair in history.messages.windows(2) {
            if let Entry::Message(Message {
                role: Role::Assistant,
                content,
                ..
            }) = &pair[0].entry
            {
                let Block::ToolUse { id, .. } = &content[0] else {
                    panic!("expected a tool use")
                };
                assert_eq!(
                    pair[1].entry,
                    Entry::EmptySlot {
                        tool_use_id: id.clone()
                    }
                );
            }
        }
    }
}
