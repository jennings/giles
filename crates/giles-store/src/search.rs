//! `search`: FTS5 over transcripts for `memory_search` (PLAN 6.7).
//!
//! The index holds the top-level `Text` blocks of every message, joined by newlines. Triggers on
//! `messages` keep it current on insert, update of `content`, and delete (including deletes that
//! cascade from a session), so nothing here writes it. Tool inputs and results, reasoning, and
//! media aren't indexed.

use giles_types::{AgentId, ConversationId, Role, Timestamp, UserId};
use rusqlite::params;

use crate::messages::{StoredMessageId, parse_role};
use crate::sessions::{SessionId, SessionKind};
use crate::{Result, Store, get_time, parse_id, query_all};

/// Marks the start of a matched term in [`SearchHit::snippet`].
pub const SNIPPET_START: &str = "**";
/// Marks the end of a matched term in [`SearchHit::snippet`].
pub const SNIPPET_END: &str = "**";
/// Marks text cut from either end of [`SearchHit::snippet`].
pub const SNIPPET_ELLIPSIS: &str = "…";
/// Tokens (roughly words) per snippet.
const SNIPPET_TOKENS: i64 = 16;

/// Which sessions a search covers. `agent` is required; each other field narrows the scope when
/// set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchScope {
    /// Only this agent's sessions.
    pub agent: AgentId,
    /// Only sessions of this kind.
    pub kind: Option<SessionKind>,
    /// Only sessions belonging to this user.
    pub user: Option<UserId>,
    /// Only sessions on this channel instance.
    pub channel: Option<String>,
    /// Only sessions in this conversation.
    pub conversation: Option<ConversationId>,
}

impl SearchScope {
    /// A DM's scope (PLAN 6.7): every direct session of `user` with `agent`, on every channel.
    pub fn direct(agent: AgentId, user: UserId) -> Self {
        Self {
            agent,
            kind: Some(SessionKind::Direct),
            user: Some(user),
            channel: None,
            conversation: None,
        }
    }

    /// A group's scope: every session of `agent` in one conversation.
    pub fn conversation(agent: AgentId, channel: String, conversation: ConversationId) -> Self {
        Self {
            agent,
            kind: None,
            user: None,
            channel: Some(channel),
            conversation: Some(conversation),
        }
    }
}

/// One matching message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// The message.
    pub message: StoredMessageId,
    /// Its session.
    pub session: SessionId,
    /// Its sequence number.
    pub seq: i64,
    /// Who wrote it.
    pub role: Role,
    /// Its `created_at`.
    pub at: Timestamp,
    /// The session's channel instance (`None` for job sessions).
    pub channel: Option<String>,
    /// The session's conversation (`None` for job sessions).
    pub conversation: Option<ConversationId>,
    /// A short excerpt around the matches, which are wrapped in [`SNIPPET_START`] and
    /// [`SNIPPET_END`].
    pub snippet: String,
}

/// Turns free text into an FTS5 query: every whitespace-separated word with a letter or digit
/// becomes a quoted string (so FTS5 operators and punctuation in the input are plain text), and
/// the words are OR-ed. Ranking (BM25) puts messages matching more and rarer words first.
/// `None` if no word is left.
pub(crate) fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

impl Store {
    /// Full-text search over transcripts in `scope`, best matches first, at most `limit` hits.
    /// `query` is free text (see the module docs); a query with no words returns nothing.
    /// English stemming applies (`running` finds `run`), and diacritics are ignored.
    pub async fn search(
        &self,
        scope: SearchScope,
        query: &str,
        limit: u32,
    ) -> Result<Vec<SearchHit>> {
        let Some(query) = fts_query(query) else {
            return Ok(Vec::new());
        };
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT m.id, m.session_id, m.seq, m.role, m.created_at, s.channel,
                        s.conversation, snippet(search, 0, ?8, ?9, ?10, ?11)
                 FROM search
                 JOIN messages m ON m.id = search.rowid
                 JOIN sessions s ON s.id = m.session_id
                 WHERE search MATCH ?1
                   AND s.agent = ?2
                   AND (?3 IS NULL OR s.kind = ?3)
                   AND (?4 IS NULL OR s.user_id = ?4)
                   AND (?5 IS NULL OR s.channel = ?5)
                   AND (?6 IS NULL OR s.conversation = ?6)
                 ORDER BY rank, m.id DESC
                 LIMIT ?7",
            )?;
            let params = params![
                query,
                scope.agent.as_str(),
                scope.kind.map(SessionKind::as_str),
                scope.user.as_ref().map(UserId::as_str),
                scope.channel,
                scope.conversation.as_ref().map(ConversationId::as_str),
                limit,
                SNIPPET_START,
                SNIPPET_END,
                SNIPPET_ELLIPSIS,
                SNIPPET_TOKENS,
            ];
            query_all(&mut stmt, params, |row| {
                Ok(SearchHit {
                    message: row.get(0)?,
                    session: row.get(1)?,
                    seq: row.get(2)?,
                    role: parse_role(&row.get::<_, String>(3)?)?,
                    at: get_time(row, 4)?,
                    channel: row.get(5)?,
                    conversation: row
                        .get::<_, Option<String>>(6)?
                        .map(|c| parse_id(c, ConversationId::new))
                        .transpose()?,
                    snippet: row.get(7)?,
                })
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_quotes_words_and_drops_punctuation() {
        assert_eq!(fts_query("  "), None);
        assert_eq!(fts_query("-- !! ..."), None);
        assert_eq!(
            fts_query(r#"dentist NEAR(tuesday) "5pm""#).as_deref(),
            Some(r#""dentist" OR "NEAR(tuesday)" OR """5pm""""#)
        );
    }
}

#[cfg(test)]
mod store_tests {
    use giles_types::Block;

    use super::*;
    use crate::messages::tests::{assistant, session, text, tool_use, user};
    use crate::sessions::SessionKey;
    use crate::sessions::tests::{at, store};

    fn jeeves() -> AgentId {
        AgentId::new("jeeves").unwrap()
    }

    fn owner() -> UserId {
        UserId::new("owner").unwrap()
    }

    async fn hits(store: &Store, scope: SearchScope, query: &str) -> Vec<(SessionId, i64)> {
        store
            .search(scope, query, 50)
            .await
            .unwrap()
            .into_iter()
            .map(|hit| (hit.session, hit.seq))
            .collect()
    }

    #[tokio::test]
    async fn indexes_text_blocks_of_both_roles_only() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let reply = assistant(vec![text("the dentist is on Tuesday"), tool_use("a")], 2);
        let appended = store
            .append_messages(s, vec![user("when is my dentist visit?", 1), reply])
            .await
            .unwrap();
        store
            .fill_slot(
                appended[1].slots[0].id,
                vec![text("toolresult")],
                false,
                at(3),
            )
            .await
            .unwrap();
        let mut reasoning = assistant(
            vec![Block::Reasoning {
                text: Some("pondering".into()),
                opaque: None,
            }],
            4,
        );
        reasoning.content.push(text("visible"));
        store.append_messages(s, vec![reasoning]).await.unwrap();

        let scope = SearchScope::direct(jeeves(), owner());
        assert_eq!(hits(&store, scope.clone(), "dentist").await.len(), 2);
        assert_eq!(hits(&store, scope.clone(), "visible").await, [(s, 4)]);
        for hidden in ["secret", "toolresult", "pondering", "memory_search"] {
            assert!(
                hits(&store, scope.clone(), hidden).await.is_empty(),
                "{hidden}"
            );
        }
    }

    #[tokio::test]
    async fn hits_carry_snippets_role_and_place() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        store
            .append_messages(s, vec![user("I went running in the park with Ana", 7)])
            .await
            .unwrap();
        let found = store
            .search(SearchScope::direct(jeeves(), owner()), "run", 10)
            .await
            .unwrap();
        assert_eq!(found.len(), 1, "stemming: run finds running");
        let hit = &found[0];
        assert_eq!((hit.session, hit.seq, hit.role), (s, 1, Role::User));
        assert_eq!(hit.at, at(7));
        assert_eq!(hit.channel.as_deref(), Some("whatsapp"));
        assert_eq!(hit.conversation.as_ref().map(|c| c.as_str()), Some("c1"));
        assert!(hit.snippet.contains("**running**"), "{}", hit.snippet);
    }

    #[tokio::test]
    async fn triggers_follow_updates_and_deletes() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let appended = store
            .append_messages(s, vec![user("apples", 1), user("bananas", 2)])
            .await
            .unwrap();
        let scope = SearchScope::direct(jeeves(), owner());
        let (first, second) = (appended[0].id, appended[1].id);

        // Update: a tombstone replaces the content (PLAN 11.4, `user forget`).
        let tombstone = serde_json::to_string(&[text("cherries")]).unwrap();
        store
            .write(move |conn| {
                conn.execute(
                    "UPDATE messages SET content = ?2 WHERE id = ?1",
                    rusqlite::params![first, tombstone],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(hits(&store, scope.clone(), "apples").await.is_empty());
        assert_eq!(hits(&store, scope.clone(), "cherries").await, [(s, 1)]);

        // An update that leaves no text removes the row from the index.
        store
            .write(move |conn| {
                conn.execute("UPDATE messages SET content = '[]' WHERE id = ?1", [first])?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(hits(&store, scope.clone(), "cherries").await.is_empty());

        // Delete, directly and by cascade from the session.
        store
            .write(move |conn| {
                conn.execute("DELETE FROM messages WHERE id = ?1", [second])?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(hits(&store, scope.clone(), "bananas").await.is_empty());

        store
            .append_messages(s, vec![user("dates", 3)])
            .await
            .unwrap();
        assert_eq!(hits(&store, scope.clone(), "dates").await.len(), 1);
        store
            .write(move |conn| {
                conn.execute("DELETE FROM sessions WHERE id = ?1", [s])?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(hits(&store, scope, "dates").await.is_empty());
        let rows = store
            .read(|conn| {
                Ok(conn.query_row("SELECT count(*) FROM search", [], |r| r.get::<_, i64>(0))?)
            })
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn scopes_filter_by_agent_user_kind_and_conversation() {
        let (_dir, store) = store().await;
        let mine = session(&store, "c1").await;
        let other_chat = session(&store, "c2").await;
        let other_agent = SessionKey {
            agent: AgentId::new("bunter").unwrap(),
            channel: "whatsapp".into(),
            conversation: giles_types::ConversationId::new("c1").unwrap(),
        };
        let (other_agent, _) = store
            .open_session(other_agent, SessionKind::Direct, Some(owner()), at(0))
            .await
            .unwrap();
        let guest = SessionKey {
            agent: jeeves(),
            channel: "irc".into(),
            conversation: giles_types::ConversationId::new("guest").unwrap(),
        };
        let (guest, _) = store
            .open_session(
                guest,
                SessionKind::Direct,
                Some(UserId::new("guest").unwrap()),
                at(0),
            )
            .await
            .unwrap();
        let group = SessionKey {
            agent: jeeves(),
            channel: "irc".into(),
            conversation: giles_types::ConversationId::new("#giles").unwrap(),
        };
        let (group, _) = store
            .open_session(group, SessionKind::Group, None, at(0))
            .await
            .unwrap();
        let job = store
            .create_job_session(jeeves(), Some(owner()), at(0))
            .await
            .unwrap();
        for s in [mine, other_chat, other_agent.id, guest.id, group.id, job.id] {
            store
                .append_messages(s, vec![user("kiwi", 1)])
                .await
                .unwrap();
        }

        let mut dm = hits(&store, SearchScope::direct(jeeves(), owner()), "kiwi").await;
        dm.sort();
        assert_eq!(dm, [(mine, 1), (other_chat, 1)]);

        let room = SearchScope::conversation(
            jeeves(),
            "irc".into(),
            giles_types::ConversationId::new("#giles").unwrap(),
        );
        assert_eq!(hits(&store, room, "kiwi").await, [(group.id, 1)]);

        let all_jeeves = SearchScope {
            agent: jeeves(),
            kind: None,
            user: None,
            channel: None,
            conversation: None,
        };
        assert_eq!(hits(&store, all_jeeves.clone(), "kiwi").await.len(), 5);
        let jobs = SearchScope {
            kind: Some(SessionKind::Job),
            ..all_jeeves
        };
        assert_eq!(hits(&store, jobs, "kiwi").await, [(job.id, 1)]);
    }

    #[tokio::test]
    async fn better_matches_rank_first_and_limit_applies() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        store
            .append_messages(
                s,
                vec![
                    user("plum", 1),
                    user("plum and pear", 2),
                    user("pear", 3),
                    user("nothing relevant", 4),
                ],
            )
            .await
            .unwrap();
        let scope = SearchScope::direct(jeeves(), owner());
        let found = hits(&store, scope.clone(), "plum pear").await;
        assert_eq!(found.len(), 3);
        assert_eq!(found[0], (s, 2), "matching both words ranks first");
        let found = store.search(scope.clone(), "plum pear", 1).await.unwrap();
        assert_eq!(found.len(), 1);
        // Operator syntax and stray quotes are plain words, never a query error.
        for query in [r#"pear" OR"#, "NEAR(plum", "plum*", "-pear", "^"] {
            store.search(scope.clone(), query, 5).await.unwrap();
        }
    }
}
