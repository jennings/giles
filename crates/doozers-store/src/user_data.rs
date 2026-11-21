//! What a user owns in `doozers.db`, counted and deleted: the database half of forgetting a user
//! (PLAN 11.4). The core does the rest (workspaces, secrets, memories, files, the user row) and
//! calls these in an order that a retry can finish.
//!
//! [`Store::user_data`] counts what [`Store::user_data_forget`] would change, for the confirmation
//! page. [`Store::user_data_forget`] makes every change in one transaction, so it either did all
//! of it or none:
//!
//! - **Deleted:** the user's direct and job sessions with their messages and summaries; their
//!   `inbox` rows and the sessions' (queued text and dedupe keys); their `outbox` rows and those
//!   addressed to one of their identities; their runtime jobs (with the last-run times); their web
//!   sessions, link codes, invites, password, passkeys, and OpenID Connect connections; their SSH
//!   key rows; the archive rows of conversations nothing else is in.
//! - **Group sessions** keep their rows. A user message the user sent there becomes a tombstone
//!   ([`TOMBSTONE`], no sender), and every summary whose range covers one is deleted, so the
//!   compaction that runs when the session next needs one writes it again from the tombstoned
//!   messages. The `search` index follows the messages by its triggers.
//! - **Cleared:** the token usage that assistant messages in those group sessions recorded, since
//!   it was measured against the summaries just deleted; `usage.user_id` (the token counts stay), the creator of the invites the user
//!   made, `first_user` and `added_by` of known SSH hosts, and `created_by` and `updated_by` of
//!   local secrets.
//!
//! FTS5 keeps a deleted row's words in the index as delete markers until its segments are merged.
//! Before it deletes, a forget turns on FTS5's `secure-delete` option for `search`, which removes a
//! deleted row's words from the index when the row goes, instead of merging the whole index inside
//! the write transaction. The option is stored in the index, so it applies to every later delete
//! too, and issuing it again is harmless. Words of messages deleted before the first forget may
//! stay in older segments until SQLite merges them. (SQLite also keeps what a delete frees in the
//! file until the pages are reused or the database is copied with `VACUUM`, which `doozers backup`
//! does.)
//!
//! The user's row, which cascades to their identities, and everything outside this database are
//! left to the caller. The user's workspaces are [`Store::user_workspaces`] and
//! [`Store::workspaces_forget_user`].

use doozers_types::{ConversationId, UserId};
use rusqlite::{Connection, params};
use serde_json::json;

use crate::{Error, Result, Store, parse_id, query_all};

/// The text a group message of a forgotten user is replaced with.
pub const TOMBSTONE: &str = "[This message was removed.]";

/// How many rows of each kind hold a user's data in `doozers.db`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserData {
    /// Direct sessions, deleted.
    pub direct_sessions: u64,
    /// The channel conversations they were in: a chat, whatever the number of agents and resets.
    pub conversations: u64,
    /// Isolated job-run sessions of the user's jobs, deleted.
    pub job_sessions: u64,
    /// Messages in the deleted sessions, tool-result slots included.
    pub messages: u64,
    /// Summaries of the deleted sessions.
    pub summaries: u64,
    /// The user's own messages in group sessions, which become tombstones.
    pub group_messages: u64,
    /// Summaries in group sessions that cover one of them, deleted to be written again.
    pub group_summaries: u64,
    /// `inbox` rows deleted.
    pub inbox: u64,
    /// `outbox` rows deleted.
    pub outbox: u64,
    /// `usage` rows, which keep their token counts and lose the user.
    pub usage: u64,
    /// Runtime jobs deleted.
    pub runtime_jobs: u64,
    /// Web login sessions deleted.
    pub web_sessions: u64,
    /// Channel identities. Counted for the confirmation page: they are deleted with the user's
    /// row, not by [`Store::user_data_forget`].
    pub identities: u64,
    /// Pending link codes deleted.
    pub link_codes: u64,
    /// Passwords (0 or 1) deleted.
    pub passwords: u64,
    /// Passkeys deleted.
    pub passkeys: u64,
    /// OpenID Connect connections deleted.
    pub oidc_connections: u64,
    /// Invite and recovery links for the user, deleted.
    pub invites: u64,
    /// Invite and recovery links the user made for others, which stay without a creator.
    pub invites_made: u64,
    /// SSH keys deleted (their secrets are the core's).
    pub ssh_keys: u64,
}

/// What [`Store::user_data_forget`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserDataDeletion {
    /// The rows are changed.
    Done {
        /// What was changed, as [`Store::user_data`] counted it just before (the `identities` it
        /// counts are the user's row's to delete).
        data: Box<UserData>,
        /// The group sessions whose summaries were deleted, because they covered a message that is
        /// now a tombstone. Their history is the raw messages until compaction writes the
        /// summaries again, which the core does.
        group_sessions: Vec<crate::SessionId>,
        /// The channel conversations of the deleted direct sessions, as (channel instance,
        /// conversation id): the live state a channel keeps for one is the channel's to drop.
        conversations: Vec<(String, ConversationId)>,
    },
    /// Nothing changed: an inbound message of the user's is waiting for a turn or in one.
    Busy,
}

/// The sessions a forget deletes (`?1` is the user): everything but a group's.
const OWN_SESSIONS: &str = "SELECT id FROM sessions WHERE user_id = ?1 AND kind <> 'group'";

/// The user's own messages in group sessions (`?1`), from `messages m` and `sessions s`.
const GROUP_MESSAGES: &str = "FROM messages m JOIN sessions s ON s.id = m.session_id
     WHERE s.kind = 'group' AND m.role = 'user' AND m.tool_use_id IS NULL
       AND m.meta ->> '$.sender' = ?1";

/// Summaries (`u`) with a range that covers one of those messages.
const GROUP_SUMMARIES: &str = "FROM summaries u WHERE EXISTS (
        SELECT 1 FROM messages m JOIN sessions s ON s.id = m.session_id
        WHERE s.kind = 'group' AND m.session_id = u.session_id AND m.role = 'user'
          AND m.tool_use_id IS NULL AND m.meta ->> '$.sender' = ?1
          AND m.seq BETWEEN u.from_seq AND u.through_seq)";

/// Whose `inbox` rows are the user's: sent by them, or routed to a session that is deleted.
fn inbox_rows() -> String {
    format!("user_id = ?1 OR session_id IN ({OWN_SESSIONS})")
}

/// Whose `outbox` rows are the user's: made for them, or addressed to one of their identities.
const OUTBOX_ROWS: &str = "user_id = ?1 OR (target = 'recipient' AND recipient IN (
        SELECT identity FROM user_identities WHERE user_id = ?1))";

fn number(conn: &Connection, sql: &str, user: &str) -> Result<u64> {
    let count: i64 = conn
        .prepare_cached(sql)?
        .query_row([user], |row| row.get(0))?;
    u64::try_from(count).map_err(|_| Error::corrupt("a negative count"))
}

fn count(conn: &Connection, user: &str) -> Result<UserData> {
    let n = |sql: &str| number(conn, sql, user);
    Ok(UserData {
        direct_sessions: n("SELECT count(*) FROM sessions WHERE user_id = ?1 AND kind = 'direct'")?,
        conversations: n("SELECT count(*) FROM (SELECT DISTINCT channel, conversation
                 FROM sessions WHERE user_id = ?1 AND kind = 'direct')")?,
        job_sessions: n("SELECT count(*) FROM sessions WHERE user_id = ?1 AND kind = 'job'")?,
        messages: n(&format!(
            "SELECT count(*) FROM messages WHERE session_id IN ({OWN_SESSIONS})"
        ))?,
        summaries: n(&format!(
            "SELECT count(*) FROM summaries WHERE session_id IN ({OWN_SESSIONS})"
        ))?,
        group_messages: n(&format!("SELECT count(*) {GROUP_MESSAGES}"))?,
        group_summaries: n(&format!("SELECT count(*) {GROUP_SUMMARIES}"))?,
        inbox: n(&format!(
            "SELECT count(*) FROM inbox WHERE {}",
            inbox_rows()
        ))?,
        outbox: n(&format!("SELECT count(*) FROM outbox WHERE {OUTBOX_ROWS}"))?,
        usage: n("SELECT count(*) FROM usage WHERE user_id = ?1")?,
        runtime_jobs: n("SELECT count(*) FROM jobs WHERE creator = ?1")?,
        web_sessions: n("SELECT count(*) FROM web_sessions WHERE user_id = ?1")?,
        identities: n("SELECT count(*) FROM user_identities WHERE user_id = ?1")?,
        link_codes: n("SELECT count(*) FROM user_link_codes WHERE user_id = ?1")?,
        passwords: n("SELECT count(*) FROM user_passwords WHERE user_id = ?1")?,
        passkeys: n("SELECT count(*) FROM user_passkeys WHERE user_id = ?1")?,
        oidc_connections: n("SELECT count(*) FROM user_oidc_connections WHERE user_id = ?1")?,
        invites: n("SELECT count(*) FROM user_invites WHERE user_id = ?1")?,
        invites_made: n("SELECT count(*) FROM user_invites WHERE created_by = ?1")?,
        ssh_keys: n("SELECT count(*) FROM ssh_keys WHERE user_id = ?1")?,
    })
}

impl Store {
    /// What [`user_data_forget`](Self::user_data_forget) would change for `user` now.
    pub async fn user_data(&self, user: &UserId) -> Result<UserData> {
        let user = user.to_string();
        self.read(move |conn| count(conn, &user)).await
    }

    /// The ids of the sessions [`user_data_forget`](Self::user_data_forget) deletes, for the
    /// in-memory state keyed by them.
    pub async fn user_session_ids(&self, user: &UserId) -> Result<Vec<crate::SessionId>> {
        let user = user.to_string();
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!("{OWN_SESSIONS} ORDER BY id"))?;
            query_all(&mut stmt, [user], |row| Ok(row.get(0)?))
        })
        .await
    }

    /// Deletes and clears what `user` owns in this database, in one transaction (see the
    /// [module docs](self)), and returns what it changed. Idempotent: a second call finds nothing.
    /// It doesn't touch the user's row, so a call after a crash finishes what was left.
    ///
    /// Refused ([`UserDataDeletion::Busy`], nothing changed) while an inbound message of the user's
    /// is `queued` or `started`.
    pub async fn user_data_forget(&self, user: &UserId) -> Result<UserDataDeletion> {
        let user = user.to_string();
        self.write(move |conn| {
            let user = user.as_str();
            let busy: bool = conn
                .prepare_cached(&format!(
                    "SELECT EXISTS (SELECT 1 FROM inbox
                     WHERE state IN ('queued', 'started') AND ({}))",
                    inbox_rows()
                ))?
                .query_row([user], |row| row.get(0))?;
            if busy {
                return Ok(UserDataDeletion::Busy);
            }
            let data = count(conn, user)?;
            let conversations = {
                let mut stmt = conn.prepare_cached(
                    "SELECT DISTINCT channel, conversation FROM sessions
                     WHERE user_id = ?1 AND kind = 'direct' ORDER BY channel, conversation",
                )?;
                query_all(&mut stmt, [user], |row| {
                    Ok((row.get::<_, String>(0)?, parse_id(row.get(1)?, ConversationId::new)?))
                })?
            };
            let group_sessions = {
                let mut stmt = conn.prepare_cached(&format!(
                    "SELECT DISTINCT u.session_id {GROUP_SUMMARIES} ORDER BY u.session_id"
                ))?;
                query_all(&mut stmt, [user], |row| Ok(row.get(0)?))?
            };
            // Words of what is deleted below leave the full-text index as the rows do.
            conn.execute(
                "INSERT INTO search (search, rank) VALUES ('secure-delete', 1)",
                [],
            )?;
            let run = |sql: &str| -> Result<()> {
                conn.prepare_cached(sql)?.execute([user])?;
                Ok(())
            };

            // Every way to sign in, and every link, so a half-forgotten user can't be used.
            run("DELETE FROM user_link_codes WHERE user_id = ?1")?;
            run("DELETE FROM user_invites WHERE user_id = ?1")?;
            run("UPDATE user_invites SET created_by = NULL WHERE created_by = ?1")?;
            run("DELETE FROM user_passkeys WHERE user_id = ?1")?;
            run("DELETE FROM user_passwords WHERE user_id = ?1")?;
            run("DELETE FROM user_oidc_connections WHERE user_id = ?1")?;
            run("DELETE FROM web_sessions WHERE user_id = ?1")?;

            // Work the user scheduled and messages for them. The job trigger deletes job_runs.
            run("DELETE FROM jobs WHERE creator = ?1")?;
            run(&format!("DELETE FROM inbox WHERE {}", inbox_rows()))?;
            run(&format!("DELETE FROM outbox WHERE {OUTBOX_ROWS}"))?;

            // Group sessions: summaries first, since they're found by the senders still there.
            run(&format!("DELETE FROM summaries WHERE id IN (SELECT u.id {GROUP_SUMMARIES})"))?;
            // The token counts the assistant's messages in those sessions recorded were measured
            // against the summaries just deleted, so they no longer say how big the history is.
            for session in &group_sessions {
                conn.prepare_cached(
                    "UPDATE messages SET meta = json_remove(meta, '$.usage')
                     WHERE session_id = ?1 AND role = 'assistant'",
                )?
                .execute([session])?;
            }
            conn.prepare_cached(&format!(
                "UPDATE messages SET content = ?2, meta = json_remove(meta, '$.sender')
                 WHERE id IN (SELECT m.id {GROUP_MESSAGES})"
            ))?
            .execute(params![
                user,
                json!([{ "type": "text", "text": TOMBSTONE }]).to_string()
            ])?;

            // The rest of the sessions: messages and summaries cascade, inbox and usage rows lose
            // the session. A group session the user is the recorded user of keeps its rows.
            run(&format!("DELETE FROM sessions WHERE id IN ({OWN_SESSIONS})"))?;
            run("UPDATE sessions SET user_id = NULL WHERE user_id = ?1")?;
            for (channel, conversation) in &conversations {
                conn.prepare_cached(
                    "DELETE FROM archived_conversations WHERE channel = ?1 AND conversation = ?2
                     AND NOT EXISTS (SELECT 1 FROM sessions WHERE channel = ?1 AND conversation = ?2)",
                )?
                .execute(params![channel, conversation.as_str()])?;
            }

            // Kept, with the user gone.
            run("UPDATE usage SET user_id = NULL WHERE user_id = ?1")?;
            run("DELETE FROM ssh_keys WHERE user_id = ?1")?;
            run("UPDATE ssh_known_hosts SET first_user = NULL WHERE first_user = ?1")?;
            run("UPDATE ssh_known_hosts SET added_by = NULL WHERE added_by = ?1")?;
            run("UPDATE secrets SET created_by = NULL WHERE created_by = ?1")?;
            run("UPDATE secrets SET updated_by = NULL WHERE updated_by = ?1")?;
            Ok(UserDataDeletion::Done {
                data: Box::new(data),
                group_sessions,
                conversations,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use doozers_types::{AgentId, Block, Role};
    use serde_json::json;

    use super::*;
    use crate::messages::tests::{assistant, text, user};
    use crate::sessions::tests::{at, key, store};
    use crate::{
        Entry, InboxState, NewIdentity, NewInboxEvent, NewOutbox, NewUser, OutboxTarget,
        SessionKind, UsageRecord, Verification,
    };

    fn id(name: &str) -> UserId {
        UserId::new(name).unwrap()
    }

    async fn add_user(store: &Store, name: &str, role: crate::Role) {
        let new = NewUser {
            id: id(name),
            username: name.to_owned(),
            display_name: name.to_owned(),
            role,
            timezone: None,
            default_identity: Some(format!("web:{name}")),
        };
        let web = NewIdentity {
            namespace: "web".to_owned(),
            native_id: name.to_owned(),
            verification: Verification::Admin,
        };
        store.user_create(new, vec![web], at(0)).await.unwrap();
    }

    /// A message the user `sender` sent into a group session.
    fn said(content: &str, secs: i64, sender: &str) -> doozers_types::Message {
        let mut message = user(content, secs);
        message.meta.sender = Some(id(sender));
        message
    }

    /// An assistant reply that recorded how big the prompt it answered was.
    fn replied_with_usage(content: &str, secs: i64) -> doozers_types::Message {
        let mut message = assistant(vec![text(content)], secs);
        message.meta.usage = Some(doozers_types::Usage {
            input_tokens: 90_000,
            output_tokens: 10,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        message
    }

    async fn transcript(store: &Store, session: crate::SessionId) -> Vec<String> {
        store
            .recent_messages(session, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|stored| match stored.entry {
                Entry::Message(message) => match &message.content[0] {
                    Block::Text { text } => text.clone(),
                    other => format!("{other:?}"),
                },
                Entry::EmptySlot { .. } => "(slot)".to_owned(),
            })
            .collect()
    }

    #[tokio::test]
    async fn forgetting_deletes_a_users_sessions_and_keeps_everyone_elses() {
        let (_dir, store) = store().await;
        add_user(&store, "ann", crate::Role::Member).await;
        add_user(&store, "bob", crate::Role::Member).await;
        let (ann, _) = store
            .open_session(key("a"), SessionKind::Direct, Some(id("ann")), at(1))
            .await
            .unwrap();
        let (bob, _) = store
            .open_session(key("b"), SessionKind::Direct, Some(id("bob")), at(1))
            .await
            .unwrap();
        for (session, who) in [(ann.id, "ann"), (bob.id, "bob")] {
            store
                .append_messages(
                    session,
                    vec![user("hello", 2), user(&format!("hello {who}marker"), 3)],
                )
                .await
                .unwrap();
        }
        store
            .insert_summary(ann.id, 1, 1, "ann's summary", at(3))
            .await
            .unwrap();
        store
            .archive_conversation(
                "whatsapp",
                &ConversationId::new("a").unwrap(),
                &id("ann"),
                at(4),
            )
            .await
            .unwrap();

        let before = store.user_data(&id("ann")).await.unwrap();
        assert_eq!(
            (
                before.direct_sessions,
                before.conversations,
                before.messages,
                before.summaries
            ),
            (1, 1, 2, 1)
        );
        let UserDataDeletion::Done {
            data,
            conversations,
            ..
        } = store.user_data_forget(&id("ann")).await.unwrap()
        else {
            panic!("busy");
        };
        assert_eq!(*data, before);
        assert_eq!(conversations.len(), 1);
        assert_eq!(conversations[0].0, "whatsapp");

        assert!(store.session(ann.id).await.unwrap().is_none());
        assert_eq!(
            transcript(&store, bob.id).await,
            ["hello", "hello bobmarker"]
        );
        // None of the user's words are left in the file, not even in the full-text index.
        let copy = _dir.path().join("copy.db");
        store.vacuum_into(copy.clone()).await.unwrap();
        let bytes = std::fs::read(copy).unwrap();
        let has = |needle: &str| bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
        // The tokenizer stems words, so the index holds "annmark".
        assert!(!has("annmark") && has("bobmark"));
        assert_eq!(
            store.user_data(&id("ann")).await.unwrap(),
            UserData {
                identities: 1,
                ..UserData::default()
            }
        );
        assert!(
            store
                .conversation_archived_at("whatsapp", &ConversationId::new("a").unwrap())
                .await
                .unwrap()
                .is_none()
        );

        // A second time finds nothing.
        let UserDataDeletion::Done { data, .. } = store.user_data_forget(&id("ann")).await.unwrap()
        else {
            panic!("busy");
        };
        assert_eq!(
            *data,
            UserData {
                identities: 1,
                ..UserData::default()
            }
        );
    }

    #[tokio::test]
    async fn a_queued_message_makes_it_busy_and_nothing_changes() {
        let (_dir, store) = store().await;
        add_user(&store, "ann", crate::Role::Member).await;
        let (ann, _) = store
            .open_session(key("a"), SessionKind::Direct, Some(id("ann")), at(1))
            .await
            .unwrap();
        store
            .inbox_insert(NewInboxEvent {
                instance: "whatsapp".into(),
                event_id: doozers_types::EventId::new("e1").unwrap(),
                session: Some(ann.id),
                user: Some(id("ann")),
                payload: json!({}),
                state: InboxState::Queued,
                received_at: at(2),
            })
            .await
            .unwrap();
        assert_eq!(
            store.user_data_forget(&id("ann")).await.unwrap(),
            UserDataDeletion::Busy
        );
        assert!(store.session(ann.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn inbox_outbox_usage_jobs_and_sign_in_rows_are_deleted_or_cleared() {
        let (_dir, store) = store().await;
        add_user(&store, "ann", crate::Role::Member).await;
        add_user(&store, "bob", crate::Role::Admin).await;
        let (ann, _) = store
            .open_session(key("a"), SessionKind::Direct, Some(id("ann")), at(1))
            .await
            .unwrap();
        for (event, user) in [("e1", "ann"), ("e2", "bob")] {
            store
                .inbox_insert(NewInboxEvent {
                    instance: "whatsapp".into(),
                    event_id: doozers_types::EventId::new(event).unwrap(),
                    session: (user == "ann").then_some(ann.id),
                    user: Some(id(user)),
                    payload: json!({}),
                    state: InboxState::Done,
                    received_at: at(2),
                })
                .await
                .unwrap();
        }
        let send = |user: Option<&str>, target: OutboxTarget| NewOutbox {
            instance: "whatsapp".into(),
            target,
            kind: "notice".into(),
            user: user.map(id),
            payload: json!({}),
            created_at: at(3),
            expires_at: at(4000),
        };
        store
            .outbox_enqueue(send(
                Some("ann"),
                OutboxTarget::Conversation(ConversationId::new("a").unwrap()),
            ))
            .await
            .unwrap();
        store
            .outbox_enqueue(send(
                None,
                OutboxTarget::Recipient(doozers_types::NativeId::new("web:ann").unwrap()),
            ))
            .await
            .unwrap();
        store
            .outbox_enqueue(send(
                Some("bob"),
                OutboxTarget::Conversation(ConversationId::new("b").unwrap()),
            ))
            .await
            .unwrap();
        for (user, session) in [("ann", Some(ann.id)), ("bob", None)] {
            store
                .record_usage(UsageRecord {
                    at: at(5),
                    provider: "p".into(),
                    model: "m".into(),
                    agent: AgentId::new("jeeves").unwrap(),
                    user: Some(id(user)),
                    session,
                    usage: doozers_types::Usage {
                        input_tokens: 10,
                        output_tokens: 20,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                    },
                    fallback: false,
                })
                .await
                .unwrap();
        }
        store
            .job_create(AgentId::new("jeeves").unwrap(), id("ann"), json!({}), at(6))
            .await
            .unwrap();
        store
            .job_create(AgentId::new("jeeves").unwrap(), id("bob"), json!({}), at(6))
            .await
            .unwrap();
        // An invite bob made for ann, and one ann made for bob.
        store
            .user_invite_create(
                "a".repeat(43).as_str(),
                &id("ann"),
                crate::InviteKind::Recovery,
                Some(&id("bob")),
                at(7),
                at(9000),
            )
            .await
            .unwrap();
        store
            .user_invite_create(
                "b".repeat(43).as_str(),
                &id("bob"),
                crate::InviteKind::Recovery,
                Some(&id("ann")),
                at(7),
                at(9000),
            )
            .await
            .unwrap();

        for name in ["ann", "bob"] {
            sign_in_rows(&store, name).await;
        }

        let before = store.user_data(&id("ann")).await.unwrap();
        assert_eq!(
            (
                before.inbox,
                before.outbox,
                before.usage,
                before.runtime_jobs,
                before.invites,
                before.invites_made
            ),
            (1, 2, 1, 1, 1, 1)
        );
        assert_eq!(
            (
                before.web_sessions,
                before.link_codes,
                before.passwords,
                before.passkeys,
                before.oidc_connections,
                before.ssh_keys
            ),
            (1, 1, 1, 1, 1, 1)
        );
        assert!(matches!(
            store.user_data_forget(&id("ann")).await.unwrap(),
            UserDataDeletion::Done { .. }
        ));

        let count = |sql: &'static str| {
            let store = store.clone();
            async move {
                store
                    .read(move |conn| Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))?))
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            count("SELECT count(*) FROM inbox WHERE user_id = 'ann'").await,
            0
        );
        assert_eq!(count("SELECT count(*) FROM inbox").await, 1);
        assert_eq!(count("SELECT count(*) FROM outbox").await, 1);
        assert_eq!(count("SELECT count(*) FROM jobs").await, 1);
        assert_eq!(count("SELECT count(*) FROM user_invites").await, 1);
        assert_eq!(
            count("SELECT count(*) FROM user_invites WHERE created_by IS NULL").await,
            1,
            "bob's invite for ann is gone, ann's for bob stays with no creator"
        );
        // Usage keeps both rows and token counts; ann's has no user and no session now.
        assert_eq!(count("SELECT count(*) FROM usage").await, 2);
        assert_eq!(
            count("SELECT count(*) FROM usage WHERE user_id = 'ann'").await,
            0
        );
        assert_eq!(count("SELECT sum(output_tokens) FROM usage").await, 40);

        // Every way of signing in and every link is ann's no more, and bob's are as they were.
        for table in [
            "web_sessions",
            "user_link_codes",
            "user_passwords",
            "user_passkeys",
            "user_oidc_connections",
            "ssh_keys",
        ] {
            let sql: &'static str = Box::leak(
                format!("SELECT count(*) FROM {table} WHERE user_id = 'bob'").into_boxed_str(),
            );
            assert_eq!(count(sql).await, 1, "{table}");
            let sql: &'static str = Box::leak(
                format!("SELECT count(*) FROM {table} WHERE user_id = 'ann'").into_boxed_str(),
            );
            assert_eq!(count(sql).await, 0, "{table}");
        }
        // Known hosts and secrets stay, with ann cleared from them and bob still named.
        assert_eq!(count("SELECT count(*) FROM ssh_known_hosts").await, 2);
        assert_eq!(
            count(
                "SELECT count(*) FROM ssh_known_hosts WHERE first_user = 'ann' OR added_by = 'ann'"
            )
            .await,
            0
        );
        assert_eq!(
            count(
                "SELECT count(*) FROM ssh_known_hosts WHERE first_user = 'bob' AND added_by = 'bob'"
            )
            .await,
            1
        );
        assert_eq!(count("SELECT count(*) FROM secrets").await, 2);
        assert_eq!(
            count(
                "SELECT count(*) FROM secrets
                 WHERE created_by = 'ann' OR updated_by = 'ann'"
            )
            .await,
            0
        );
        assert_eq!(
            count("SELECT count(*) FROM secrets WHERE created_by = 'bob' AND updated_by = 'bob'")
                .await,
            1
        );
    }

    /// One of each way to sign in, link, key, trusted host, and secret that `name` has.
    async fn sign_in_rows(store: &Store, name: &str) {
        let user = id(name);
        store
            .web_session_create(
                &format!("token-of-{name}"),
                user.clone(),
                crate::SignInMethod::Password,
                at(1),
                at(9000),
            )
            .await
            .unwrap();
        store
            .user_link_code_create(&user, "irc", "ABCD2345", [7; 16], at(1), at(9000))
            .await
            .unwrap();
        store
            .user_password_set(&user, "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA", at(1))
            .await
            .unwrap();
        store
            .user_passkey_add(
                crate::NewPasskey {
                    user: user.clone(),
                    credential_id: format!("credential-of-{name}").into_bytes(),
                    credential: "{}".into(),
                    kind: crate::PasskeyKind::Discoverable,
                    name: "laptop".into(),
                },
                at(1),
            )
            .await
            .unwrap();
        if store.oidc_provider("google").await.unwrap().is_none() {
            store
                .oidc_provider_add(
                    "google",
                    crate::OidcProviderSettings {
                        display_name: "Google".into(),
                        issuer: "https://accounts.example".into(),
                        client_id: "client".into(),
                        client_secret: None,
                        scopes: vec!["openid".into()],
                        enabled: true,
                    },
                    at(1),
                )
                .await
                .unwrap();
        }
        store
            .user_oidc_connect("google", "https://accounts.example", name, &user, at(1))
            .await
            .unwrap();
        store
            .ssh_key_insert(
                crate::NewSshKey {
                    user: user.clone(),
                    label: "laptop".into(),
                    algorithm: "ssh-ed25519".into(),
                    public_key: format!("ssh-ed25519 AAAA{name}"),
                    fingerprint: format!("SHA256:{name}"),
                },
                at(1),
            )
            .await
            .unwrap();
        store
            .known_host_pin(
                crate::NewKnownHost {
                    environment: "box".into(),
                    host: format!("{name}.example"),
                    port: 22,
                    key_type: "ssh-ed25519".into(),
                    public_key: format!("AAAA{name}"),
                    fingerprint: format!("SHA256:{name}"),
                    first_workspace: None,
                    first_agent: None,
                    first_user: Some(name.to_owned()),
                    added_by: Some(user.clone()),
                },
                at(1),
            )
            .await
            .unwrap();
        store
            .secret_put(
                &format!("users/{name}/github"),
                crate::SealedValue {
                    algorithm: "aes-256-gcm".into(),
                    nonce: vec![0; 12],
                    ciphertext: vec![1; 16],
                },
                Some(&user),
                at(1),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn group_messages_become_tombstones_and_the_summaries_covering_them_are_dropped() {
        let (_dir, store) = store().await;
        add_user(&store, "ann", crate::Role::Member).await;
        add_user(&store, "bob", crate::Role::Member).await;
        // Summaries are cumulative: each one covers everything from the first message.
        let (group, _) = store
            .open_session(key("room"), SessionKind::Group, None, at(1))
            .await
            .unwrap();
        store
            .append_messages(
                group.id,
                vec![
                    said("ann's secret annmarker", 2, "ann"),
                    replied_with_usage("noted", 3),
                    said("bob's remark", 4, "bob"),
                    assistant(vec![text("fine")], 5),
                    said("bob again", 6, "bob"),
                ],
            )
            .await
            .unwrap();
        store
            .insert_summary(group.id, 1, 2, "ann told a secret", at(7))
            .await
            .unwrap();
        store
            .insert_summary(group.id, 1, 4, "ann told a secret, bob remarked", at(8))
            .await
            .unwrap();
        // A group where ann spoke after the summary's range: it stays.
        let (other, _) = store
            .open_session(key("lounge"), SessionKind::Group, None, at(1))
            .await
            .unwrap();
        store
            .append_messages(
                other.id,
                vec![said("hi", 2, "bob"), said("ann's words", 3, "ann")],
            )
            .await
            .unwrap();
        store
            .insert_summary(other.id, 1, 1, "bob said hi", at(4))
            .await
            .unwrap();

        let before = store.user_data(&id("ann")).await.unwrap();
        assert_eq!((before.group_messages, before.group_summaries), (2, 2));
        let UserDataDeletion::Done { group_sessions, .. } =
            store.user_data_forget(&id("ann")).await.unwrap()
        else {
            panic!("busy");
        };
        assert_eq!(group_sessions, [group.id]);

        assert_eq!(
            transcript(&store, group.id).await,
            [TOMBSTONE, "noted", "bob's remark", "fine", "bob again"]
        );
        let first = store
            .recent_messages(group.id, 100)
            .await
            .unwrap()
            .remove(0);
        let Entry::Message(first) = &first.entry else {
            panic!("a message");
        };
        assert_eq!((first.role, first.meta.sender.clone()), (Role::User, None));
        // Both summaries covered ann's message, so the history is every message again.
        let history = store.load_history(group.id).await.unwrap();
        assert_eq!(history.summary, None);
        assert_eq!(history.messages.len(), 5);
        // The other group's summary stopped short of ann's message.
        let history = store.load_history(other.id).await.unwrap();
        assert_eq!(
            history.summary.map(|s| (s.from_seq, s.through_seq)),
            Some((1, 1))
        );
        assert_eq!(transcript(&store, other.id).await, ["hi", TOMBSTONE]);
        let found = store
            .search(
                crate::SearchScope::conversation(
                    AgentId::new("jeeves").unwrap(),
                    "whatsapp".to_owned(),
                    ConversationId::new("room").unwrap(),
                ),
                "secret",
                5,
            )
            .await
            .unwrap();
        assert!(found.is_empty(), "the search index follows the content");
        // The assistant's recorded usage, measured against the deleted summaries, is gone, and
        // none of the user's words are left in the file.
        let rows = store.recent_messages(group.id, 100).await.unwrap();
        for row in &rows {
            if let Entry::Message(message) = &row.entry {
                assert_eq!(message.meta.usage, None, "{message:?}");
            }
        }
        let copy = _dir.path().join("copy.db");
        store.vacuum_into(copy.clone()).await.unwrap();
        let bytes = std::fs::read(copy).unwrap();
        let has = |needle: &str| bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
        assert!(!has("annmark"));
    }
}
