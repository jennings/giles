//! `usage`: one row per provider request (PLAN 11.7). Recorded only; v1 has no budgets.

use giles_types::{AgentId, Timestamp, Usage, UserId};
use rusqlite::params;

use crate::sessions::SessionId;
use crate::time::to_millis;
use crate::{Error, Result, Store, parse_id, query_all};

/// One provider request, for [`Store::record_usage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    /// When the request finished.
    pub at: Timestamp,
    /// The provider instance.
    pub provider: String,
    /// The model id.
    pub model: String,
    /// The agent that made the request.
    pub agent: AgentId,
    /// The turn's requester, if any.
    pub user: Option<UserId>,
    /// The session, if any.
    pub session: Option<SessionId>,
    /// Token counts (`input_tokens` excludes cached tokens).
    pub usage: Usage,
    /// Whether the request went to a fallback model (PLAN 6.10).
    pub fallback: bool,
}

/// Totals for one agent, from [`Store::usage_by_agent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentUsage {
    /// The agent.
    pub agent: AgentId,
    /// Provider requests.
    pub requests: u64,
    /// Requests that went to a fallback model.
    pub fallbacks: u64,
    /// Summed token counts. `Usage::cache_hit_rate` gives the prompt cache hit rate.
    pub tokens: Usage,
}

fn to_i64(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::invalid(format!("{what} {value} exceeds i64::MAX")))
}

fn to_u64(value: i64, what: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::corrupt(format!("{what} {value} is negative")))
}

impl Store {
    /// Records one provider request.
    pub async fn record_usage(&self, record: UsageRecord) -> Result<()> {
        let u = record.usage;
        let tokens = [
            to_i64(u.input_tokens, "input_tokens")?,
            to_i64(u.output_tokens, "output_tokens")?,
            to_i64(u.cache_read_tokens, "cache_read_tokens")?,
            to_i64(u.cache_write_tokens, "cache_write_tokens")?,
        ];
        self.write(move |conn| {
            conn.prepare_cached(
                "INSERT INTO usage (at, provider, model, agent, user_id, session_id, input_tokens,
                    output_tokens, cache_read_tokens, cache_write_tokens, fallback)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?
            .execute(params![
                to_millis(record.at),
                record.provider,
                record.model,
                record.agent.as_str(),
                record.user.as_ref().map(UserId::as_str),
                record.session,
                tokens[0],
                tokens[1],
                tokens[2],
                tokens[3],
                record.fallback,
            ])?;
            Ok(())
        })
        .await
    }

    /// Totals per agent for requests at or after `since` (all time if `None`), sorted by agent.
    pub async fn usage_by_agent(&self, since: Option<Timestamp>) -> Result<Vec<AgentUsage>> {
        let since = since.map(to_millis);
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT agent, count(*), sum(fallback), sum(input_tokens), sum(output_tokens),
                        sum(cache_read_tokens), sum(cache_write_tokens)
                 FROM usage WHERE ?1 IS NULL OR at >= ?1
                 GROUP BY agent ORDER BY agent",
            )?;
            query_all(&mut stmt, [since], |row| {
                Ok(AgentUsage {
                    agent: parse_id(row.get(0)?, AgentId::new)?,
                    requests: to_u64(row.get(1)?, "count")?,
                    fallbacks: to_u64(row.get(2)?, "fallbacks")?,
                    tokens: Usage {
                        input_tokens: to_u64(row.get(3)?, "input_tokens")?,
                        output_tokens: to_u64(row.get(4)?, "output_tokens")?,
                        cache_read_tokens: to_u64(row.get(5)?, "cache_read_tokens")?,
                        cache_write_tokens: to_u64(row.get(6)?, "cache_write_tokens")?,
                    },
                })
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::tests::session;
    use crate::sessions::tests::{at, store};

    fn record(agent: &str, secs: i64, usage: Usage, fallback: bool) -> UsageRecord {
        UsageRecord {
            at: at(secs),
            provider: "anthropic".into(),
            model: "claude".into(),
            agent: AgentId::new(agent).unwrap(),
            user: Some(UserId::new("owner").unwrap()),
            session: None,
            usage,
            fallback,
        }
    }

    fn tokens(input: u64, output: u64, read: u64, write: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_write_tokens: write,
        }
    }

    #[tokio::test]
    async fn aggregates_per_agent_since_a_time() {
        let (_dir, store) = store().await;
        let s = session(&store, "c1").await;
        let mut first = record("jeeves", 10, tokens(100, 50, 0, 900), false);
        first.session = Some(s);
        store.record_usage(first).await.unwrap();
        store
            .record_usage(record("jeeves", 20, tokens(10, 40, 900, 0), true))
            .await
            .unwrap();
        store
            .record_usage(record("bunter", 5, tokens(1, 2, 3, 4), false))
            .await
            .unwrap();

        let all = store.usage_by_agent(None).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].agent.as_str(), "bunter");
        let jeeves = &all[1];
        assert_eq!((jeeves.requests, jeeves.fallbacks), (2, 1));
        assert_eq!(jeeves.tokens, tokens(110, 90, 900, 900));

        // `since` is inclusive.
        let recent = store.usage_by_agent(Some(at(10))).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].requests, 2);
        assert!(store.usage_by_agent(Some(at(21))).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn counts_beyond_sqlite_integers_are_rejected() {
        let (_dir, store) = store().await;
        let err = store
            .record_usage(record("jeeves", 1, tokens(u64::MAX, 0, 0, 0), false))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }
}
