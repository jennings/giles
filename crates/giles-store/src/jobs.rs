//! `jobs` and `job_runs`: runtime scheduled jobs and the last run of every job (PLAN 6.11).
//!
//! Config jobs live in `agent.toml`; only their last run time is stored here. Runtime jobs, made
//! by the `schedule` tool, are rows in `jobs` whose `spec` is the scheduler's JSON definition
//! (cron or time, prompt, options); the store keeps it opaque.

use giles_types::{AgentId, Timestamp, UserId};
use rusqlite::{OptionalExtension, Row, params};
use serde_json::Value;

use crate::time::{from_millis, to_millis};
use crate::{Result, Store, get_time, parse_id, query_all, query_opt, row_id};

row_id!(
    /// A row in `jobs`. Ids are never reused, even after a job is deleted.
    RuntimeJobId
);

/// A runtime job.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeJob {
    /// The row.
    pub id: RuntimeJobId,
    /// The agent it runs on.
    pub agent: AgentId,
    /// The user who created it. It runs with their role and delivers to them.
    pub creator: UserId,
    /// The scheduler's definition of the job.
    pub spec: Value,
    /// When it was created.
    pub created_at: Timestamp,
}

/// Which job a `job_runs` row is for. Config and runtime ids live in separate namespaces, so a
/// config job named like a runtime id can't collide with it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JobKey {
    /// A config job, by its `id` in `agent.toml`.
    Config(String),
    /// A runtime job.
    Runtime(RuntimeJobId),
}

impl JobKey {
    /// The stored form: `config:<id>` or `runtime:<row id>`.
    fn stored(&self) -> String {
        match self {
            Self::Config(id) => format!("config:{id}"),
            Self::Runtime(id) => format!("runtime:{id}"),
        }
    }
}

const SELECT: &str = "SELECT id, agent, creator, spec, created_at FROM jobs";

fn from_row(row: &Row<'_>) -> Result<RuntimeJob> {
    Ok(RuntimeJob {
        id: row.get(0)?,
        agent: parse_id(row.get(1)?, AgentId::new)?,
        creator: parse_id(row.get(2)?, UserId::new)?,
        spec: serde_json::from_str(&row.get::<_, String>(3)?)?,
        created_at: get_time(row, 4)?,
    })
}

impl Store {
    /// Creates a runtime job.
    pub async fn job_create(
        &self,
        agent: AgentId,
        creator: UserId,
        spec: Value,
        now: Timestamp,
    ) -> Result<RuntimeJob> {
        let spec_json = serde_json::to_string(&spec)?;
        self.write(move |conn| {
            conn.prepare_cached(
                "INSERT INTO jobs (agent, creator, spec, created_at) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                agent.as_str(),
                creator.as_str(),
                spec_json,
                to_millis(now)
            ])?;
            Ok(RuntimeJob {
                id: RuntimeJobId(conn.last_insert_rowid()),
                agent,
                creator,
                spec,
                created_at: from_millis(to_millis(now))?,
            })
        })
        .await
    }

    /// A runtime job by id.
    pub async fn job(&self, id: RuntimeJobId) -> Result<Option<RuntimeJob>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!("{SELECT} WHERE id = ?1"))?;
            query_opt(&mut stmt, [id], from_row)
        })
        .await
    }

    /// Runtime jobs, oldest first, optionally only one agent's and/or one creator's.
    pub async fn jobs(
        &self,
        agent: Option<AgentId>,
        creator: Option<UserId>,
    ) -> Result<Vec<RuntimeJob>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare_cached(&format!(
                "{SELECT} WHERE (?1 IS NULL OR agent = ?1) AND (?2 IS NULL OR creator = ?2)
                 ORDER BY id"
            ))?;
            let params = params![
                agent.as_ref().map(AgentId::as_str),
                creator.as_ref().map(UserId::as_str)
            ];
            query_all(&mut stmt, params, from_row)
        })
        .await
    }

    /// Replaces a job's spec. Returns `false` if the job doesn't exist.
    pub async fn job_update_spec(&self, id: RuntimeJobId, spec: Value) -> Result<bool> {
        let spec = serde_json::to_string(&spec)?;
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("UPDATE jobs SET spec = ?2 WHERE id = ?1")?
                .execute(params![id, spec])?
                == 1)
        })
        .await
    }

    /// Deletes a runtime job and its `job_runs` row. Returns `false` if it didn't exist.
    pub async fn job_delete(&self, id: RuntimeJobId) -> Result<bool> {
        self.write(move |conn| {
            Ok(conn
                .prepare_cached("DELETE FROM jobs WHERE id = ?1")?
                .execute([id])?
                == 1)
        })
        .await
    }

    /// Deletes every runtime job of `agent`, and the last-run times of the agent's config jobs
    /// too (those of runtime jobs go with the jobs). Returns how many runtime jobs there were.
    /// For an agent that was removed: nothing of its schedule is left to run again if the id is
    /// used again.
    pub async fn agent_jobs_delete(&self, agent: AgentId) -> Result<usize> {
        self.write(move |conn| {
            let jobs = conn
                .prepare_cached("DELETE FROM jobs WHERE agent = ?1")?
                .execute([agent.as_str()])?;
            conn.prepare_cached("DELETE FROM job_runs WHERE agent = ?1")?
                .execute([agent.as_str()])?;
            Ok(jobs)
        })
        .await
    }

    /// When the job last ran, if ever.
    pub async fn job_last_run(&self, agent: AgentId, job: JobKey) -> Result<Option<Timestamp>> {
        self.read(move |conn| {
            conn.prepare_cached("SELECT last_run_at FROM job_runs WHERE agent = ?1 AND job = ?2")?
                .query_row(params![agent.as_str(), job.stored()], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?
                .map(from_millis)
                .transpose()
        })
        .await
    }

    /// Records that the job ran at `at`, replacing the previous time.
    pub async fn job_set_last_run(&self, agent: AgentId, job: JobKey, at: Timestamp) -> Result<()> {
        self.write(move |conn| {
            conn.prepare_cached(
                "INSERT INTO job_runs (agent, job, last_run_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (agent, job) DO UPDATE SET last_run_at = excluded.last_run_at",
            )?
            .execute(params![agent.as_str(), job.stored(), to_millis(at)])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::sessions::tests::{at, store};

    fn agent(name: &str) -> AgentId {
        AgentId::new(name).unwrap()
    }

    fn user(name: &str) -> UserId {
        UserId::new(name).unwrap()
    }

    #[tokio::test]
    async fn deleting_an_agents_jobs_leaves_every_other_agents_alone() {
        let (_dir, store) = store().await;
        let spec = json!({ "cron": "0 8 * * *", "prompt": "hi" });
        let gone = store
            .job_create(agent("jeeves"), user("owner"), spec.clone(), at(1))
            .await
            .unwrap();
        let other = store
            .job_create(agent("bunter"), user("owner"), spec.clone(), at(2))
            .await
            .unwrap();
        let mut runs = vec![(agent("jeeves"), JobKey::Config("morning".into()))];
        runs.push((agent("jeeves"), JobKey::Runtime(gone.id)));
        runs.push((agent("bunter"), JobKey::Config("morning".into())));
        runs.push((agent("bunter"), JobKey::Runtime(other.id)));
        for (agent, key) in &runs {
            store
                .job_set_last_run(agent.clone(), key.clone(), at(5))
                .await
                .unwrap();
        }

        assert_eq!(store.agent_jobs_delete(agent("jeeves")).await.unwrap(), 1);
        assert_eq!(store.job(gone.id).await.unwrap(), None);
        assert_eq!(store.job(other.id).await.unwrap(), Some(other.clone()));
        for (agent, key) in runs {
            let kept = agent.as_str() == "bunter";
            let last = store.job_last_run(agent.clone(), key).await.unwrap();
            assert_eq!(last.is_some(), kept, "{agent}");
        }
        // Nothing left to delete: zero, and no error.
        assert_eq!(store.agent_jobs_delete(agent("jeeves")).await.unwrap(), 0);
        assert_eq!(store.agent_jobs_delete(agent("nobody")).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn runtime_jobs_round_trip_and_filter() {
        let (_dir, store) = store().await;
        let spec = json!({ "at": "2026-10-22T17:00:00Z", "prompt": "remind me" });
        let a = store
            .job_create(agent("jeeves"), user("owner"), spec.clone(), at(1))
            .await
            .unwrap();
        let b = store
            .job_create(agent("jeeves"), user("guest"), json!({}), at(2))
            .await
            .unwrap();
        let c = store
            .job_create(agent("bunter"), user("owner"), json!({}), at(3))
            .await
            .unwrap();

        assert_eq!(store.job(a.id).await.unwrap(), Some(a.clone()));
        assert_eq!(a.spec, spec);
        let ids = |jobs: Vec<RuntimeJob>| jobs.into_iter().map(|j| j.id).collect::<Vec<_>>();
        assert_eq!(
            ids(store.jobs(None, None).await.unwrap()),
            [a.id, b.id, c.id]
        );
        assert_eq!(
            ids(store.jobs(Some(agent("jeeves")), None).await.unwrap()),
            [a.id, b.id]
        );
        assert_eq!(
            ids(store.jobs(None, Some(user("owner"))).await.unwrap()),
            [a.id, c.id]
        );
        assert_eq!(
            ids(store
                .jobs(Some(agent("bunter")), Some(user("guest")))
                .await
                .unwrap()),
            []
        );

        assert!(
            store
                .job_update_spec(b.id, json!({ "cron": "0 9 * * *" }))
                .await
                .unwrap()
        );
        assert_eq!(
            store.job(b.id).await.unwrap().unwrap().spec,
            json!({ "cron": "0 9 * * *" })
        );
        assert!(
            !store
                .job_update_spec(RuntimeJobId::new(99), json!({}))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn deleted_ids_are_not_reused_and_take_their_run_record_along() {
        let (_dir, store) = store().await;
        let a = store
            .job_create(agent("jeeves"), user("owner"), json!({}), at(1))
            .await
            .unwrap();
        let key = JobKey::Runtime(a.id);
        store
            .job_set_last_run(agent("jeeves"), key.clone(), at(5))
            .await
            .unwrap();
        assert!(store.job_delete(a.id).await.unwrap());
        assert!(!store.job_delete(a.id).await.unwrap());
        assert_eq!(store.job(a.id).await.unwrap(), None);
        assert_eq!(
            store.job_last_run(agent("jeeves"), key).await.unwrap(),
            None
        );

        let b = store
            .job_create(agent("jeeves"), user("owner"), json!({}), at(2))
            .await
            .unwrap();
        assert!(b.id > a.id);
    }

    #[tokio::test]
    async fn last_runs_are_per_agent_and_namespaced() {
        let (_dir, store) = store().await;
        let config = JobKey::Config("1".into());
        let runtime = JobKey::Runtime(RuntimeJobId::new(1));
        assert_eq!(
            store
                .job_last_run(agent("jeeves"), config.clone())
                .await
                .unwrap(),
            None
        );

        store
            .job_set_last_run(agent("jeeves"), config.clone(), at(10))
            .await
            .unwrap();
        store
            .job_set_last_run(agent("jeeves"), config.clone(), at(20))
            .await
            .unwrap();
        assert_eq!(
            store
                .job_last_run(agent("jeeves"), config.clone())
                .await
                .unwrap(),
            Some(at(20))
        );
        assert_eq!(
            store.job_last_run(agent("jeeves"), runtime).await.unwrap(),
            None
        );
        assert_eq!(
            store.job_last_run(agent("bunter"), config).await.unwrap(),
            None
        );
    }
}
