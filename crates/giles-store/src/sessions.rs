//! `sessions`: one conversation's history with one agent (PLAN 6.2).

use giles_types::{AgentId, ConversationId, Timestamp, UserId};
use rusqlite::{Connection, Row, params};

use crate::time::to_millis;
use crate::{Error, Result, Store, get_opt_time, get_time, parse_id, query_all, query_opt, row_id};

row_id!(
    /// A row in `sessions`.
    SessionId
);

/// What a session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SessionKind {
    /// A direct conversation (DM) on a channel.
    Direct,
    /// A group conversation on a channel. Not used in v1 (PLAN 1.1).
    Group,
    /// An isolated job run (`session = "isolated"`, PLAN 6.11): no channel conversation.
    Job,
}

impl SessionKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Group => "group",
            Self::Job => "job",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "direct" => Ok(Self::Direct),
            "group" => Ok(Self::Group),
            "job" => Ok(Self::Job),
            other => Err(Error::corrupt(format!("session kind {other:?}"))),
        }
    }
}

/// The key of a channel session (PLAN 6.2): agent, channel instance, conversation id. At most one
/// open session exists per key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    /// The agent.
    pub agent: AgentId,
    /// The channel instance name, e.g. `whatsapp`.
    pub channel: String,
    /// The conversation on that channel.
    pub conversation: ConversationId,
}

/// A row in `sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The row id.
    pub id: SessionId,
    /// The agent.
    pub agent: AgentId,
    /// Direct, group, or job.
    pub kind: SessionKind,
    /// The channel instance; `None` for job sessions.
    pub channel: Option<String>,
    /// The conversation on `channel`; `None` for job sessions.
    pub conversation: Option<ConversationId>,
    /// The user a direct or job session belongs to.
    pub user: Option<UserId>,
    /// When it started.
    pub started_at: Timestamp,
    /// When it ended (`/new`, idle reset, job run finished); `None` while open.
    pub ended_at: Option<Timestamp>,
    /// `created_at` of its last message (highest sequence number), if it has any. Drives
    /// `idle_reset`.
    pub last_message_at: Option<Timestamp>,
}

impl Session {
    /// The session's key, unless it's a job session.
    pub fn key(&self) -> Option<SessionKey> {
        Some(SessionKey {
            agent: self.agent.clone(),
            channel: self.channel.clone()?,
            conversation: self.conversation.clone()?,
        })
    }
}

/// A session and the size of its transcript, from [`Store::list_sessions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// The session.
    pub session: Session,
    /// Its stored messages, empty result slots included.
    pub messages: u64,
}

const SELECT: &str = "SELECT s.id, s.agent, s.kind, s.channel, s.conversation, s.user_id,
        s.started_at, s.ended_at,
        (SELECT m.created_at FROM messages m WHERE m.session_id = s.id ORDER BY m.seq DESC LIMIT 1)
            AS last_message_at
    FROM sessions s";

fn from_row(row: &Row<'_>) -> Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        agent: parse_id(row.get(1)?, AgentId::new)?,
        kind: SessionKind::parse(&row.get::<_, String>(2)?)?,
        channel: row.get(3)?,
        conversation: row
            .get::<_, Option<String>>(4)?
            .map(|c| parse_id(c, ConversationId::new))
            .transpose()?,
        user: row
            .get::<_, Option<String>>(5)?
            .map(|u| parse_id(u, UserId::new))
            .transpose()?,
        started_at: get_time(row, 6)?,
        ended_at: get_opt_time(row, 7)?,
        last_message_at: get_opt_time(row, 8)?,
    })
}

pub(crate) fn get(conn: &Connection, id: SessionId) -> Result<Option<Session>> {
    let mut stmt = conn.prepare_cached(&format!("{SELECT} WHERE s.id = ?1"))?;
    query_opt(&mut stmt, [id], from_row)
}

fn find_open(conn: &Connection, key: &SessionKey) -> Result<Option<Session>> {
    let mut stmt = conn.prepare_cached(&format!(
        "{SELECT} WHERE s.agent = ?1 AND s.channel = ?2 AND s.conversation = ?3
            AND s.ended_at IS NULL"
    ))?;
    query_opt(
        &mut stmt,
        params![key.agent.as_str(), key.channel, key.conversation.as_str()],
        from_row,
    )
}

fn insert(
    conn: &Connection,
    agent: &AgentId,
    kind: SessionKind,
    place: Option<(&str, &str)>,
    user: Option<&UserId>,
    now: Timestamp,
) -> Result<Session> {
    conn.prepare_cached(
        "INSERT INTO sessions (agent, kind, channel, conversation, user_id, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        agent.as_str(),
        kind.as_str(),
        place.map(|p| p.0),
        place.map(|p| p.1),
        user.map(UserId::as_str),
        to_millis(now),
    ])?;
    get(conn, SessionId(conn.last_insert_rowid()))?
        .ok_or_else(|| Error::not_found("the session just inserted"))
}

impl Store {
    /// Returns the open session for `key`, creating it (with `kind`, `user`, and `started_at =
    /// now`) if there is none. The flag is `true` when it was created. `kind` must be
    /// [`SessionKind::Direct`] or [`SessionKind::Group`]; an existing session keeps its own kind
    /// and user.
    pub async fn open_session(
        &self,
        key: SessionKey,
        kind: SessionKind,
        user: Option<UserId>,
        now: Timestamp,
    ) -> Result<(Session, bool)> {
        if kind == SessionKind::Job {
            return Err(Error::invalid("job sessions have no key"));
        }
        self.write(move |conn| {
            if let Some(session) = find_open(conn, &key)? {
                return Ok((session, false));
            }
            let place = (key.channel.as_str(), key.conversation.as_str());
            let session = insert(conn, &key.agent, kind, Some(place), user.as_ref(), now)?;
            Ok((session, true))
        })
        .await
    }

    /// Creates a session for one isolated job run (PLAN 6.11), owned by `user` if the job has
    /// one. End it with [`end_session`](Self::end_session) when the run finishes.
    pub async fn create_job_session(
        &self,
        agent: AgentId,
        user: Option<UserId>,
        now: Timestamp,
    ) -> Result<Session> {
        self.write(move |conn| insert(conn, &agent, SessionKind::Job, None, user.as_ref(), now))
            .await
    }

    /// The open session for `key`, if any.
    pub async fn find_open_session(&self, key: SessionKey) -> Result<Option<Session>> {
        self.read(move |conn| find_open(conn, &key)).await
    }

    /// The session with this id, open or ended.
    pub async fn session(&self, id: SessionId) -> Result<Option<Session>> {
        self.read(move |conn| get(conn, id)).await
    }

    /// Ends the session at `now` (`/new`, idle reset, end of a job run). Returns `false` if it
    /// doesn't exist or had already ended. The session's messages stay; the next
    /// [`open_session`](Self::open_session) for its key creates a new one.
    pub async fn end_session(&self, id: SessionId, now: Timestamp) -> Result<bool> {
        self.write(move |conn| {
            let changed = conn
                .prepare_cached(
                    "UPDATE sessions SET ended_at = ?2 WHERE id = ?1 AND ended_at IS NULL",
                )?
                .execute(params![id, to_millis(now)])?;
            Ok(changed == 1)
        })
        .await
    }

    /// Every open session, oldest first.
    pub async fn open_sessions(&self) -> Result<Vec<Session>> {
        self.read(|conn| {
            let mut stmt =
                conn.prepare_cached(&format!("{SELECT} WHERE s.ended_at IS NULL ORDER BY s.id"))?;
            query_all(&mut stmt, [], from_row)
        })
        .await
    }

    /// Deletes the session with its messages and summaries (an isolated job run that replied
    /// `NO_REPLY`, PLAN 6.11). Inbox and usage rows keep their data with the session cleared.
    /// Returns `false` if it didn't exist.
    pub async fn delete_session(&self, id: SessionId) -> Result<bool> {
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("DELETE FROM sessions WHERE id = ?1")?
                .execute([id])?
                == 1)
        })
        .await
    }

    /// The direct session of `user` with `agent` on one of the channel `instances` with the
    /// newest activity (last message, else start), open or ended: where a job finds the user's
    /// DM (PLAN 6.11).
    pub async fn latest_direct_session(
        &self,
        agent: AgentId,
        user: UserId,
        instances: Vec<String>,
    ) -> Result<Option<Session>> {
        let instances = serde_json::to_string(&instances)?;
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT * FROM ({SELECT} WHERE s.agent = ?1 AND s.kind = 'direct'
                    AND s.user_id = ?2 AND s.channel IN (SELECT value FROM json_each(?3))) q
                 ORDER BY coalesce(q.last_message_at, q.started_at) DESC, q.id DESC LIMIT 1"
            ))?;
            query_opt(
                &mut stmt,
                params![agent.as_str(), user.as_str(), instances],
                from_row,
            )
        })
        .await
    }

    /// Sessions with their message counts, newest activity (last message, else start) first:
    /// only `agent`'s if set, and only open ones unless `include_ended`.
    pub async fn list_sessions(
        &self,
        agent: Option<AgentId>,
        include_ended: bool,
    ) -> Result<Vec<SessionSummary>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT q.*, (SELECT count(*) FROM messages m WHERE m.session_id = q.id)
                 FROM ({SELECT} WHERE (?1 IS NULL OR s.agent = ?1)
                    AND (?2 OR s.ended_at IS NULL)) q
                 ORDER BY coalesce(q.last_message_at, q.started_at) DESC, q.id DESC"
            ))?;
            query_all(
                &mut stmt,
                params![agent.as_ref().map(AgentId::as_str), include_ended],
                |row| {
                    let messages: i64 = row.get(9)?;
                    Ok(SessionSummary {
                        session: from_row(row)?,
                        messages: u64::try_from(messages)
                            .map_err(|_| Error::corrupt("a negative message count"))?,
                    })
                },
            )
        })
        .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn at(secs: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + secs).unwrap()
    }

    pub(crate) fn key(conversation: &str) -> SessionKey {
        SessionKey {
            agent: AgentId::new("jeeves").unwrap(),
            channel: "whatsapp".into(),
            conversation: ConversationId::new(conversation).unwrap(),
        }
    }

    pub(crate) async fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        (dir, store)
    }

    fn owner() -> Option<UserId> {
        Some(UserId::new("owner").unwrap())
    }

    #[tokio::test]
    async fn open_session_reuses_the_open_session_for_a_key() {
        let (_dir, store) = store().await;
        let (first, created) = store
            .open_session(key("c1"), SessionKind::Direct, owner(), at(0))
            .await
            .unwrap();
        assert!(created);
        assert_eq!(first.started_at, at(0));
        assert_eq!(first.key(), Some(key("c1")));
        assert_eq!(first.user, owner());
        assert_eq!(first.last_message_at, None);

        let (again, created) = store
            .open_session(key("c1"), SessionKind::Group, None, at(5))
            .await
            .unwrap();
        assert!(!created);
        assert_eq!(again, first, "an existing session keeps its kind and user");

        let (other, created) = store
            .open_session(key("c2"), SessionKind::Direct, owner(), at(6))
            .await
            .unwrap();
        assert!(created);
        assert_ne!(other.id, first.id);
    }

    #[tokio::test]
    async fn ending_a_session_makes_the_key_start_a_new_one() {
        let (_dir, store) = store().await;
        let (first, _) = store
            .open_session(key("c1"), SessionKind::Direct, owner(), at(0))
            .await
            .unwrap();
        assert!(store.end_session(first.id, at(10)).await.unwrap());
        assert!(
            !store.end_session(first.id, at(11)).await.unwrap(),
            "already ended"
        );
        assert!(
            !store
                .end_session(SessionId::new(999), at(11))
                .await
                .unwrap()
        );
        assert_eq!(store.find_open_session(key("c1")).await.unwrap(), None);

        let ended = store.session(first.id).await.unwrap().unwrap();
        assert_eq!(ended.ended_at, Some(at(10)));

        let (second, created) = store
            .open_session(key("c1"), SessionKind::Direct, owner(), at(20))
            .await
            .unwrap();
        assert!(created);
        assert_ne!(second.id, first.id);
        assert_eq!(
            store.find_open_session(key("c1")).await.unwrap(),
            Some(second.clone())
        );
        assert_eq!(store.open_sessions().await.unwrap(), [second]);
    }

    #[tokio::test]
    async fn job_sessions_have_no_key() {
        let (_dir, store) = store().await;
        let agent = AgentId::new("jeeves").unwrap();
        let a = store
            .create_job_session(agent.clone(), owner(), at(0))
            .await
            .unwrap();
        let b = store.create_job_session(agent, None, at(1)).await.unwrap();
        assert_eq!(a.kind, SessionKind::Job);
        assert_eq!((a.key(), a.channel.as_deref()), (None, None));
        assert_ne!(a.id, b.id);
        assert_eq!(store.open_sessions().await.unwrap(), [a, b]);

        let err = store
            .open_session(key("c1"), SessionKind::Job, None, at(0))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    #[tokio::test]
    async fn list_sessions_orders_by_activity_and_filters() {
        use crate::messages::tests::user;

        let (_dir, store) = store().await;
        let jeeves = AgentId::new("jeeves").unwrap();
        // c1 starts first but has the newest message; c2 has none; the job is another agent's.
        let (c1, _) = store
            .open_session(key("c1"), SessionKind::Direct, owner(), at(0))
            .await
            .unwrap();
        let (c2, _) = store
            .open_session(key("c2"), SessionKind::Direct, owner(), at(10))
            .await
            .unwrap();
        let job = store
            .create_job_session(AgentId::new("bunter").unwrap(), None, at(5))
            .await
            .unwrap();
        store
            .append_messages(c1.id, vec![user("hi", 20), user("again", 30)])
            .await
            .unwrap();
        let (c3, _) = store
            .open_session(key("c3"), SessionKind::Direct, owner(), at(40))
            .await
            .unwrap();
        assert!(store.end_session(c3.id, at(41)).await.unwrap());

        let ids = |list: Vec<SessionSummary>| list.iter().map(|s| s.session.id).collect::<Vec<_>>();
        let open = store.list_sessions(None, false).await.unwrap();
        assert_eq!(open[0].session.last_message_at, Some(at(30)));
        assert_eq!(open[0].messages, 2);
        assert_eq!(open[1].messages, 0);
        assert_eq!(ids(open), [c1.id, c2.id, job.id]);
        assert_eq!(
            ids(store.list_sessions(None, true).await.unwrap()),
            [c3.id, c1.id, c2.id, job.id]
        );
        assert_eq!(
            ids(store.list_sessions(Some(jeeves), true).await.unwrap()),
            [c3.id, c1.id, c2.id]
        );
    }

    #[tokio::test]
    async fn latest_direct_session_picks_the_newest_activity_on_the_instances() {
        use crate::messages::tests::user;

        let (_dir, store) = store().await;
        let jeeves = AgentId::new("jeeves").unwrap();
        let latest = |instances: &[&str]| {
            let store = store.clone();
            let instances = instances.iter().map(|i| (*i).to_owned()).collect();
            let jeeves = jeeves.clone();
            async move {
                store
                    .latest_direct_session(jeeves, owner().unwrap(), instances)
                    .await
                    .unwrap()
                    .map(|s| s.id)
            }
        };
        assert_eq!(latest(&["whatsapp"]).await, None);
        // An ended session with the newest message beats an open one that is quieter.
        let (old, _) = store
            .open_session(key("c1"), SessionKind::Direct, owner(), at(0))
            .await
            .unwrap();
        store
            .append_messages(old.id, vec![user("late", 50)])
            .await
            .unwrap();
        store.end_session(old.id, at(51)).await.unwrap();
        let (quiet, _) = store
            .open_session(key("c2"), SessionKind::Direct, owner(), at(10))
            .await
            .unwrap();
        // Not candidates: another user's DM, a job session, another instance, another agent.
        store
            .open_session(
                key("c3"),
                SessionKind::Direct,
                UserId::new("ann").ok(),
                at(60),
            )
            .await
            .unwrap();
        store
            .create_job_session(jeeves.clone(), owner(), at(70))
            .await
            .unwrap();
        let mut irc = key("c4");
        irc.channel = "irc".into();
        let (irc, _) = store
            .open_session(irc, SessionKind::Direct, owner(), at(80))
            .await
            .unwrap();
        let mut other_agent = key("c5");
        other_agent.agent = AgentId::new("bunter").unwrap();
        store
            .open_session(other_agent, SessionKind::Direct, owner(), at(90))
            .await
            .unwrap();

        assert_eq!(latest(&["whatsapp"]).await, Some(old.id));
        assert_eq!(latest(&["whatsapp", "irc"]).await, Some(irc.id));
        assert_eq!(latest(&[]).await, None);
        store
            .append_messages(quiet.id, vec![user("now", 100)])
            .await
            .unwrap();
        assert_eq!(latest(&["whatsapp", "irc"]).await, Some(quiet.id));
    }

    #[tokio::test]
    async fn deleting_a_session_takes_its_messages() {
        use crate::messages::tests::user;

        let (_dir, store) = store().await;
        let job = store
            .create_job_session(AgentId::new("jeeves").unwrap(), owner(), at(0))
            .await
            .unwrap();
        store
            .append_messages(job.id, vec![user("prompt", 1)])
            .await
            .unwrap();
        assert!(store.delete_session(job.id).await.unwrap());
        assert!(!store.delete_session(job.id).await.unwrap());
        assert_eq!(store.session(job.id).await.unwrap(), None);
        assert_eq!(store.last_seq(job.id).await.unwrap(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_opens_of_one_key_create_one_session() {
        let (_dir, store) = store().await;
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..20 {
            let store = store.clone();
            tasks.spawn(async move {
                store
                    .open_session(key("c1"), SessionKind::Direct, owner(), at(i))
                    .await
            });
        }
        let mut created = 0;
        let mut ids = std::collections::HashSet::new();
        while let Some(result) = tasks.join_next().await {
            let (session, was_created) = result.unwrap().unwrap();
            created += usize::from(was_created);
            ids.insert(session.id);
        }
        assert_eq!((created, ids.len()), (1, 1));
    }
}
