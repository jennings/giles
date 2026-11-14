//! The scheduled jobs page: every job that delivers to the signed-in user, across all agents, in
//! one table with the agent as a column. Read-only: runtime jobs are changed through the agent's
//! `schedule` tool and config jobs in `agent.toml`.
//!
//! # Whose jobs
//!
//! A runtime job delivers to its creator, so the page lists the runtime jobs the user created.
//! Config jobs deliver to the owner, so only the owner sees them. Like the memory page, it is
//! open to every signed-in user, not only the owner.
//!
//! # Times
//!
//! Times are shown in the zone the job runs in ([`message_timezone`]). The next run is the next
//! time of the job's schedule inside its active hours ([`next_run`]), as the scheduler would
//! fire it; a one-shot job still in the table hasn't finished, so its time is shown even when it
//! has passed. The last run is the job's `job_runs` row.

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use giles_store::{JobKey, RuntimeJob};
use giles_types::{AgentId, Timestamp};
use jiff::tz::TimeZone;

use super::admin::may_administer;
use super::auth::WebUser;
use super::pages::{WebApp, agent_label, internal_error, no_store};
use crate::agent_loop::history::message_timezone;
use crate::config::{ActiveHours, Config, JobPrompt, JobSession};
use crate::core_tools::schedule::{short, show_time};
use crate::http;
use crate::scheduler::{JobDef, When, in_active_hours};

/// How many times of a job's schedule [`next_run`] tries against its active hours before it
/// decides the job never runs inside them: a week of every-minute times, so any pattern with a
/// time inside a daily window is found.
const ACTIVE_HOURS_SEARCH: usize = 7 * 24 * 60;

/// A time on the page: in the job's zone, with the RFC 3339 form for `<time datetime>`.
struct TimeView {
    text: String,
    iso: String,
}

impl TimeView {
    fn new(at: Timestamp, tz: &TimeZone) -> Self {
        Self {
            text: show_time(at, tz),
            iso: at.to_string(),
        }
    }
}

/// One job in the table.
struct JobRow {
    /// How the page names the agent.
    agent: String,
    /// The config job's id, or `#<n>` for a runtime job.
    job: String,
    /// Set in `agent.toml` (as opposed to made by the agent with the `schedule` tool).
    from_config: bool,
    /// The cron pattern; `None` for a one-shot job.
    cron: Option<String>,
    /// `HH:MM-HH:MM`, for a job that runs only inside those hours.
    active_hours: Option<String>,
    next: Option<TimeView>,
    last: Option<TimeView>,
    /// Where it runs: its own session or the user's DM.
    session: &'static str,
    /// The prompt, cut short; empty for a prompt file.
    prompt: String,
    /// The prompt file's name, for a config job with `prompt_file`.
    prompt_file: Option<String>,
    /// Why the job doesn't run, if it doesn't.
    problem: Option<String>,
    /// The table's order: next run first, then the agent.
    next_at: Option<Timestamp>,
    agent_id: AgentId,
}

#[derive(Template)]
#[template(path = "web/jobs.html")]
struct JobsPage {
    jobs: Vec<JobRow>,
    /// Whether the user is the owner: the page shows the admin menu.
    admin: bool,
}

/// The job's next run after `now`: the next time of its schedule inside its active hours, or a
/// one-shot job's time. `None` if it has no such time.
fn next_run(job: &JobDef, tz: &TimeZone, now: Timestamp) -> Option<Timestamp> {
    if let When::At(at) = job.when {
        return Some(at);
    }
    let mut after = now;
    for _ in 0..ACTIVE_HOURS_SEARCH {
        let next = job.when.next_after(tz, after)?;
        if in_active_hours(job.active_hours.as_ref(), tz, next) {
            return Some(next);
        }
        after = next;
    }
    None
}

fn hours(hours: &ActiveHours) -> String {
    format!(
        "{}-{}",
        hours.start.strftime("%H:%M"),
        hours.end.strftime("%H:%M")
    )
}

fn session_name(session: JobSession) -> &'static str {
    match session {
        JobSession::Dm => "Your DM",
        _ => "Its own session",
    }
}

/// The zone `agent`'s jobs run in; the owner's or the system's for an agent that isn't
/// configured.
fn zone(config: &Config, agent: &AgentId) -> TimeZone {
    match config.agents.get(agent) {
        Some(agent) => message_timezone(config, agent),
        None => config
            .owner
            .timezone
            .clone()
            .unwrap_or_else(TimeZone::system),
    }
}

/// The row of a job the scheduler can run, with its last run.
fn row(config: &Config, job: &JobDef, last: Option<Timestamp>, now: Timestamp) -> JobRow {
    let tz = zone(config, &job.agent);
    let configured = config.agents.contains_key(&job.agent);
    let next_at = configured.then(|| next_run(job, &tz, now)).flatten();
    let (prompt, prompt_file) = match &job.prompt {
        JobPrompt::Inline(text) => (short(text), None),
        JobPrompt::File(path) => (
            String::new(),
            Some(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
        ),
    };
    JobRow {
        agent: agent_label(config, &job.agent),
        job: match &job.key {
            JobKey::Config(id) => id.clone(),
            JobKey::Runtime(id) => format!("#{id}"),
        },
        from_config: matches!(job.key, JobKey::Config(_)),
        cron: match &job.when {
            When::Cron(cron) => Some(cron.pattern.as_str().to_owned()),
            When::At(_) => None,
        },
        active_hours: job.active_hours.as_ref().map(hours),
        next: next_at.map(|at| TimeView::new(at, &tz)),
        last: last.map(|at| TimeView::new(at, &tz)),
        session: session_name(job.session),
        prompt,
        prompt_file,
        problem: (!configured).then(|| "Doesn't run: the agent isn't configured".to_owned()),
        next_at,
        agent_id: job.agent.clone(),
    }
}

/// The row of a runtime job whose stored spec the scheduler can't use.
fn unusable_row(config: &Config, job: &RuntimeJob, error: String) -> JobRow {
    JobRow {
        agent: agent_label(config, &job.agent),
        job: format!("#{}", job.id),
        from_config: false,
        cron: None,
        active_hours: None,
        next: None,
        last: None,
        session: "",
        prompt: String::new(),
        prompt_file: None,
        problem: Some(format!("Doesn't run: {error}")),
        next_at: None,
        agent_id: job.agent.clone(),
    }
}

/// `GET /jobs`: the user's scheduled jobs with every agent, soonest next run first.
pub(super) async fn jobs_page(State(app): State<WebApp>, user: WebUser) -> Response {
    let store = &app.inner.store;
    let runtime = match store.jobs(None, Some(user.user.clone())).await {
        Ok(jobs) => jobs,
        Err(err) => return no_store(internal_error("cannot list the scheduled jobs", &err)),
    };
    let config = app.inner.agents.get();
    let now = app.inner.clock.now();
    let admin = may_administer(&user);
    let mut defs = Vec::new();
    let mut rows = Vec::new();
    if admin {
        for (agent, agent_config) in &config.agents {
            defs.extend(
                agent_config
                    .jobs
                    .iter()
                    .map(|job| JobDef::from_config(agent, job)),
            );
        }
    }
    for job in &runtime {
        match JobDef::from_runtime(job) {
            Ok(def) => defs.push(def),
            Err(error) => rows.push(unusable_row(&config, job, error)),
        }
    }
    for def in &defs {
        let last = match store.job_last_run(def.agent.clone(), def.key.clone()).await {
            Ok(last) => last,
            Err(err) => return no_store(internal_error("cannot read a job's last run", &err)),
        };
        rows.push(row(&config, def, last, now));
    }
    // Stable: jobs with the same next run (or none) keep the order above.
    rows.sort_by(|a, b| match (a.next_at, b.next_at) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.agent_id.cmp(&b.agent_id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.agent_id.cmp(&b.agent_id),
    });
    no_store(http::render(&JobsPage { jobs: rows, admin }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_cron;

    fn ts(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn job(when: When, active_hours: Option<&str>) -> JobDef {
        JobDef {
            key: JobKey::Config("check-in".into()),
            agent: AgentId::new("jeeves").unwrap(),
            creator: None,
            created_at: None,
            when,
            session: JobSession::Isolated,
            idle_only: false,
            active_hours: active_hours.map(|h| ActiveHours::parse(h).unwrap()),
            model: None,
            prompt: JobPrompt::Inline("Check in.".into()),
            ask_to: None,
            dm_excerpt: None,
        }
    }

    fn cron(pattern: &str) -> When {
        When::Cron(Box::new(parse_cron(pattern).unwrap()))
    }

    #[test]
    fn the_next_run_skips_times_outside_the_active_hours() {
        let job = job(cron("*/30 * * * *"), Some("08:00-22:00"));
        assert_eq!(
            next_run(&job, &TimeZone::UTC, ts("2026-10-22T22:10:00Z")),
            Some(ts("2026-10-23T08:00:00Z"))
        );
    }

    #[test]
    fn a_job_whose_times_are_all_outside_its_active_hours_has_no_next_run() {
        let job = job(cron("* 3 * * *"), Some("08:00-22:00"));
        assert_eq!(
            next_run(&job, &TimeZone::UTC, ts("2026-10-22T12:00:00Z")),
            None
        );
    }
}
