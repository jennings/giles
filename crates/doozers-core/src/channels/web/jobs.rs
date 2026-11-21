//! The scheduled jobs page: every job that delivers to the signed-in user, across all agents, in
//! one table with the agent as a column, and the forms that schedule, edit, disable, re-enable,
//! and delete the user's runtime jobs. Config jobs (`[[jobs]]` in `agent.toml`) are shown but not
//! changed here.
//!
//! # Whose jobs
//!
//! A runtime job delivers to its creator, so the page lists the runtime jobs the user created,
//! and only those can be changed: every handler that takes a job id answers 404 for anyone else's
//! ([`own_job`]). Config jobs run as the user `owner` ([`identity::OWNER`]) and deliver to them, so
//! that user, whatever their role, sees them: whose data they are decides it, not the admin role,
//! until runtime jobs replace them (#115). Like the memory page, it is open to every signed-in
//! user, not only admins; a new job may be for any agent the user may start a chat with
//! ([`agent_choices`]).
//!
//! # Changes
//!
//! A job is a row in `jobs` with a [`RuntimeSpec`], the same as the `schedule` tool makes, with
//! the same cap per user and agent ([`MAX_RUNTIME_JOBS_PER_USER`]), but without a conversation:
//! it delivers to the user's default identity. Every change notifies the scheduler
//! ([`JobChanges`](crate::scheduler::JobChanges)), which restarts the job's task. An edit or a
//! switch back on sets `changed_at`, so the scheduler doesn't make up the times the job missed
//! before it. An edit keeps the job's agent, whether it is disabled, and the conversation the
//! `schedule` tool recorded.
//!
//! Every post is CSRF-checked; one that succeeds answers 303 to the list. A form the page can't
//! accept is shown again with why (400; 409 at the cap). Every answer is `no-store`.
//!
//! # Times
//!
//! Times are shown, and read from the form, in the zone the job runs in ([`message_timezone`]).
//! The next run is the next time of the job's schedule inside its active hours ([`next_run`]), as
//! the scheduler would fire it; a one-shot job still in the table hasn't finished, so its time is
//! shown even when it has passed. The last run is the job's `job_runs` row.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::map_response;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use doozers_store::{JobKey, RuntimeJob, RuntimeJobId};
use doozers_types::{AgentId, Timestamp, UserId};
use jiff::tz::TimeZone;
use serde::Deserialize;

use super::auth::{WebUser, check_csrf};
use super::pages::{
    AgentChoice, WebApp, agent_choices, agent_label, error_page, internal_error, no_store,
};
use super::shell::{Nav, Shell};
use crate::agent_loop::history::message_timezone;
use crate::config::{ActiveHours, Config, JobPrompt, JobSession, parse_cron};
use crate::core_tools::MAX_RUNTIME_JOBS_PER_USER;
use crate::core_tools::schedule::{parse_at, short, show_time};
use crate::http;
use crate::identity;
use crate::scheduler::{JobDef, RuntimeSpec, When, in_active_hours};

/// How many times of a job's schedule [`next_run`] tries against its active hours before it
/// decides the job never runs inside them: a week of every-minute times, so any pattern with a
/// time inside a daily window is found.
const ACTIVE_HOURS_SEARCH: usize = 7 * 24 * 60;

/// How the form's time field writes a time: what `<input type="datetime-local">` sends.
const FORM_TIME_FORMAT: &str = "%Y-%m-%dT%H:%M";

/// The jobs page's routes, all `no-store`.
pub(super) fn routes() -> Router<WebApp> {
    Router::new()
        .route("/jobs", get(jobs_page).post(create))
        .route("/jobs/new", get(new_page))
        .route("/jobs/{id}/edit", get(edit_page).post(edit))
        .route("/jobs/{id}/disable", post(disable))
        .route("/jobs/{id}/enable", post(enable))
        .route("/jobs/{id}/delete", post(delete))
        .route_layer(map_response(async |response: Response| no_store(response)))
}

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
    /// A runtime job's number: the user's own job, which the row's buttons change.
    id: Option<i64>,
    /// Set in `agent.toml` (as opposed to a runtime job).
    from_config: bool,
    /// Switched off: it doesn't run until switched back on.
    disabled: bool,
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
    shell: Shell,
    jobs: Vec<JobRow>,
    /// Whether there's an agent the user may schedule a job with.
    can_create: bool,
    csrf: String,
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

/// The zone `agent`'s jobs of `user` (`None`: a config job) run in; the user's or the system's for
/// an agent that isn't configured.
fn zone(app: &WebApp, user: Option<&UserId>, config: &Config, agent: &AgentId) -> TimeZone {
    let users = app.inner.channel.users();
    match config.agents.get(agent) {
        Some(agent) => message_timezone(users, user, agent),
        None => users
            .timezone(user.unwrap_or(&identity::owner()))
            .unwrap_or_else(TimeZone::system),
    }
}

/// How the form names a time zone.
fn zone_name(tz: &TimeZone) -> String {
    tz.iana_name()
        .map_or_else(|| "the system's time zone".to_owned(), str::to_owned)
}

/// The row of a job the scheduler can run, with its last run.
fn row(
    app: &WebApp,
    config: &Config,
    job: &JobDef,
    last: Option<Timestamp>,
    now: Timestamp,
) -> JobRow {
    let tz = zone(app, job.creator.as_ref(), config, &job.agent);
    let configured = config.agents.contains_key(&job.agent);
    let next_at = (configured && !job.disabled)
        .then(|| next_run(job, &tz, now))
        .flatten();
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
    let (job_name, id) = match &job.key {
        JobKey::Config(id) => (id.clone(), None),
        JobKey::Runtime(id) => (format!("#{id}"), Some(id.get())),
    };
    JobRow {
        agent: agent_label(config, &job.agent),
        job: job_name,
        id,
        from_config: matches!(job.key, JobKey::Config(_)),
        disabled: job.disabled,
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
        id: Some(job.id.get()),
        from_config: false,
        disabled: false,
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
async fn jobs_page(State(app): State<WebApp>, user: WebUser) -> Response {
    let store = &app.inner.store;
    let runtime = match store.jobs(None, Some(user.user.clone())).await {
        Ok(jobs) => jobs,
        Err(err) => return internal_error("cannot list the scheduled jobs", &err),
    };
    let config = app.inner.agents.get();
    let now = app.inner.clock.now();
    let mut defs = Vec::new();
    let mut rows = Vec::new();
    // Config jobs are the user `owner`'s: they run as them and deliver to them.
    if user.user == identity::owner() {
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
            Err(err) => return internal_error("cannot read a job's last run", &err),
        };
        rows.push(row(&app, &config, def, last, now));
    }
    // Stable: jobs with the same next run (or none) keep the order above.
    rows.sort_by(|a, b| match (a.next_at, b.next_at) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.agent_id.cmp(&b.agent_id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.agent_id.cmp(&b.agent_id),
    });
    http::render(&JobsPage {
        shell: Shell::new(&user, Nav::Jobs),
        jobs: rows,
        can_create: !agent_choices(&config).is_empty(),
        csrf: user.csrf.clone(),
    })
}

/// The job form's fields, as posted and as shown (again after an error).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct JobForm {
    csrf: Option<String>,
    /// The agent of a new job; an edit keeps the job's.
    agent: String,
    /// `cron` (repeats) or `once`.
    repeat: String,
    cron: String,
    /// `YYYY-MM-DDTHH:MM` in the job's zone.
    at: String,
    prompt: String,
    /// `isolated` or `dm`.
    session: String,
}

impl JobForm {
    /// The empty form for a new job.
    fn blank() -> Self {
        Self {
            repeat: "cron".to_owned(),
            session: "isolated".to_owned(),
            ..Self::default()
        }
    }

    /// The form for editing a job with `spec`, its time in `tz`.
    fn of(spec: &RuntimeSpec, tz: &TimeZone) -> Self {
        Self {
            csrf: None,
            agent: String::new(),
            repeat: if spec.at.is_some() { "once" } else { "cron" }.to_owned(),
            cron: spec.cron.clone().unwrap_or_default(),
            at: spec
                .at
                .map(|at| {
                    at.to_zoned(tz.clone())
                        .strftime(FORM_TIME_FORMAT)
                        .to_string()
                })
                .unwrap_or_default(),
            prompt: spec.prompt.clone(),
            session: match spec.session {
                JobSession::Dm => "dm",
                _ => "isolated",
            }
            .to_owned(),
        }
    }

    /// The spec the form asks for, its time read in `tz`; it must run after `now`.
    fn spec(&self, tz: &TimeZone, now: Timestamp) -> Result<RuntimeSpec, String> {
        let prompt = self.prompt.trim().replace("\r\n", "\n");
        if prompt.is_empty() {
            return Err("Write a prompt: what the agent does when the job runs.".to_owned());
        }
        let session = match self.session.as_str() {
            "isolated" => JobSession::Isolated,
            "dm" => JobSession::Dm,
            _ => return Err("Choose where the job runs.".to_owned()),
        };
        let (cron, at) = match self.repeat.as_str() {
            "cron" => {
                let pattern = self.cron.trim();
                if pattern.is_empty() {
                    return Err("Write a cron schedule, such as 0 9 * * 1-5.".to_owned());
                }
                let parsed = parse_cron(pattern)?;
                if When::Cron(Box::new(parsed)).next_after(tz, now).is_none() {
                    return Err(format!("The cron schedule {pattern} never matches again."));
                }
                (Some(pattern.to_owned()), None)
            }
            "once" => {
                let text = self.at.trim();
                if text.is_empty() {
                    return Err("Pick the time the job runs.".to_owned());
                }
                let at = parse_at(text, tz)?;
                if at <= now {
                    return Err("That time has passed. Pick a time in the future.".to_owned());
                }
                (None, Some(at))
            }
            _ => return Err("Choose whether the job repeats or runs once.".to_owned()),
        };
        Ok(RuntimeSpec {
            cron,
            at,
            prompt,
            session,
            conversation: None,
            disabled: false,
            changed_at: None,
        })
    }
}

#[derive(Template)]
#[template(path = "web/job_form.html")]
struct JobFormPage<'a> {
    shell: Shell,
    title: String,
    action: String,
    submit: &'static str,
    csrf: &'a str,
    /// The agents a new job may be for; empty when editing.
    agents: Vec<AgentChoice>,
    /// The agent of the job being edited.
    agent: Option<String>,
    form: &'a JobForm,
    /// The zone the form's times are read in.
    zone: String,
    error: Option<String>,
}

/// Which form: a new job, or an edit of the user's job.
enum Target<'a> {
    New,
    Edit(&'a RuntimeJob),
}

/// The form for `target` with `form`'s values, and `error` (with `status`) if a post was refused.
fn render_form(
    app: &WebApp,
    user: &WebUser,
    target: &Target<'_>,
    form: &JobForm,
    error: Option<(StatusCode, String)>,
) -> Response {
    let config = app.inner.agents.get();
    let (title, action, submit, agents, agent, zone) = match target {
        Target::New => {
            let agents = agent_choices(&config);
            // The zone of the chosen agent: say which when they all agree.
            let mut zones = agents.iter().filter_map(|choice| {
                AgentId::new(&choice.id)
                    .ok()
                    .map(|id| zone_name(&zone(app, Some(&user.user), &config, &id)))
            });
            let zone = match zones.next() {
                Some(first) if zones.all(|other| other == first) => first,
                Some(_) => "the time zone of the job's agent".to_owned(),
                None => "the system's time zone".to_owned(),
            };
            (
                "New job".to_owned(),
                "/jobs".to_owned(),
                "Schedule",
                agents,
                None,
                zone,
            )
        }
        Target::Edit(job) => (
            format!("Edit job #{}", job.id),
            format!("/jobs/{}/edit", job.id),
            "Save",
            Vec::new(),
            Some(agent_label(&config, &job.agent)),
            zone_name(&zone(app, Some(&user.user), &config, &job.agent)),
        ),
    };
    let (status, error) = match error {
        Some((status, error)) => (status, Some(error)),
        None => (StatusCode::OK, None),
    };
    let mut response = http::render(&JobFormPage {
        shell: Shell::new(user, Nav::Jobs),
        title,
        action,
        submit,
        csrf: &user.csrf,
        agents,
        agent,
        form,
        zone,
        error,
    });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

/// 303 to the list.
fn to_list() -> Response {
    Redirect::to("/jobs").into_response()
}

/// The user's runtime job with the number in the path; `Err` the answer if there's no such job,
/// or it's someone else's (404 either way, so the page doesn't tell which).
async fn own_job(app: &WebApp, user: &WebUser, id: &str) -> Result<RuntimeJob, Box<Response>> {
    let not_found = || {
        error_page(
            StatusCode::NOT_FOUND,
            "No such job",
            "You have no scheduled job with this number. It may have run once already, or been \
             deleted.",
        )
    };
    let Ok(id) = id.parse::<i64>() else {
        return Err(Box::new(not_found()));
    };
    match app.inner.store.job(RuntimeJobId::new(id)).await {
        Ok(Some(job)) if job.creator == user.user => Ok(job),
        Ok(_) => Err(Box::new(not_found())),
        Err(err) => Err(Box::new(internal_error(
            "cannot read a scheduled job",
            &err,
        ))),
    }
}

/// The spec of the user's `job`; `Err` a 409 page if it can't be read (only deleting is left).
fn stored_spec(job: &RuntimeJob) -> Result<RuntimeSpec, Box<Response>> {
    serde_json::from_value(job.spec.clone()).map_err(|error| {
        Box::new(error_page(
            StatusCode::CONFLICT,
            "Job can't be read",
            &format!(
                "Job #{} can't be read ({error}). Delete it instead.",
                job.id
            ),
        ))
    })
}

/// Stores `spec` as `job`'s, tells the scheduler, and answers 303 to the list (404 if the job
/// went in the meantime, such as a one-shot job that ran).
async fn save(app: &WebApp, job: &RuntimeJob, spec: &RuntimeSpec) -> Response {
    let value = match serde_json::to_value(spec) {
        Ok(value) => value,
        Err(err) => return internal_error("cannot write a job's spec", &err),
    };
    match app.inner.store.job_update_spec(job.id, value).await {
        Ok(true) => {
            app.inner.jobs.notify();
            to_list()
        }
        Ok(false) => error_page(
            StatusCode::NOT_FOUND,
            "No such job",
            "The job has run once already, or been deleted.",
        ),
        Err(err) => internal_error("cannot change a scheduled job", &err),
    }
}

/// `GET /jobs/new`: the form for a new job.
async fn new_page(State(app): State<WebApp>, user: WebUser) -> Response {
    render_form(&app, &user, &Target::New, &JobForm::blank(), None)
}

/// `POST /jobs` (CSRF-checked): schedules the job, then 303 to the list.
async fn create(
    State(app): State<WebApp>,
    user: WebUser,
    headers: HeaderMap,
    Form(form): Form<JobForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let refuse = |status, error: String| {
        render_form(&app, &user, &Target::New, &form, Some((status, error)))
    };
    let config = app.inner.agents.get();
    let agent = match agent_choices(&config)
        .iter()
        .find(|choice| choice.id == form.agent)
        .and_then(|choice| AgentId::new(&choice.id).ok())
    {
        Some(agent) => agent,
        None => return refuse(StatusCode::BAD_REQUEST, "Pick an agent.".to_owned()),
    };
    let now = app.inner.clock.now();
    let spec = match form.spec(&zone(&app, Some(&user.user), &config, &agent), now) {
        Ok(spec) => spec,
        Err(error) => return refuse(StatusCode::BAD_REQUEST, error),
    };
    let store = &app.inner.store;
    match store
        .jobs(Some(agent.clone()), Some(user.user.clone()))
        .await
    {
        Ok(jobs) if jobs.len() >= MAX_RUNTIME_JOBS_PER_USER => {
            return refuse(
                StatusCode::CONFLICT,
                format!(
                    "You have {MAX_RUNTIME_JOBS_PER_USER} jobs with this agent, the most allowed. \
                     Delete one first."
                ),
            );
        }
        Ok(_) => {}
        Err(err) => return internal_error("cannot count the scheduled jobs", &err),
    }
    let value = match serde_json::to_value(&spec) {
        Ok(value) => value,
        Err(err) => return internal_error("cannot write a job's spec", &err),
    };
    match store.job_create(agent, user.user.clone(), value, now).await {
        Ok(_) => {
            app.inner.jobs.notify();
            to_list()
        }
        Err(err) => internal_error("cannot create a scheduled job", &err),
    }
}

/// `GET /jobs/<id>/edit`: the form, filled in from the user's job. A spec that can't be read
/// gives a blank form, which replaces it.
async fn edit_page(State(app): State<WebApp>, user: WebUser, Path(id): Path<String>) -> Response {
    let job = match own_job(&app, &user, &id).await {
        Ok(job) => job,
        Err(response) => return *response,
    };
    let config = app.inner.agents.get();
    let form = match serde_json::from_value::<RuntimeSpec>(job.spec.clone()) {
        Ok(spec) => JobForm::of(&spec, &zone(&app, Some(&user.user), &config, &job.agent)),
        Err(_) => JobForm::blank(),
    };
    render_form(&app, &user, &Target::Edit(&job), &form, None)
}

/// `POST /jobs/<id>/edit` (CSRF-checked): saves the edit, then 303 to the list. The job keeps its
/// agent, whether it is disabled, and the conversation it was scheduled in.
async fn edit(
    State(app): State<WebApp>,
    user: WebUser,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<JobForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let job = match own_job(&app, &user, &id).await {
        Ok(job) => job,
        Err(response) => return *response,
    };
    let config = app.inner.agents.get();
    let now = app.inner.clock.now();
    let mut spec = match form.spec(&zone(&app, Some(&user.user), &config, &job.agent), now) {
        Ok(spec) => spec,
        Err(error) => {
            return render_form(
                &app,
                &user,
                &Target::Edit(&job),
                &form,
                Some((StatusCode::BAD_REQUEST, error)),
            );
        }
    };
    if let Ok(old) = serde_json::from_value::<RuntimeSpec>(job.spec.clone()) {
        spec.disabled = old.disabled;
        spec.conversation = old.conversation;
    }
    spec.changed_at = Some(now);
    save(&app, &job, &spec).await
}

/// A form with nothing but the CSRF token: the row buttons.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CsrfForm {
    csrf: Option<String>,
}

/// `POST /jobs/<id>/disable` (CSRF-checked): switches the user's job off, then 303 to the list.
async fn disable(
    State(app): State<WebApp>,
    user: WebUser,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let job = match own_job(&app, &user, &id).await {
        Ok(job) => job,
        Err(response) => return *response,
    };
    let mut spec = match stored_spec(&job) {
        Ok(spec) => spec,
        Err(response) => return *response,
    };
    if spec.disabled {
        return to_list();
    }
    spec.disabled = true;
    save(&app, &job, &spec).await
}

/// `POST /jobs/<id>/enable` (CSRF-checked): switches the user's job back on, then 303 to the
/// list. A one-shot job whose time passed while it was off is refused (409): edit it first.
async fn enable(
    State(app): State<WebApp>,
    user: WebUser,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let job = match own_job(&app, &user, &id).await {
        Ok(job) => job,
        Err(response) => return *response,
    };
    let mut spec = match stored_spec(&job) {
        Ok(spec) => spec,
        Err(response) => return *response,
    };
    if !spec.disabled {
        return to_list();
    }
    let now = app.inner.clock.now();
    if spec.at.is_some_and(|at| at <= now) {
        return error_page(
            StatusCode::CONFLICT,
            "Job's time has passed",
            &format!(
                "Job #{} was to run once, at a time that passed while it was disabled. Edit it to \
                 pick a new time, or delete it.",
                job.id
            ),
        );
    }
    spec.disabled = false;
    spec.changed_at = Some(now);
    save(&app, &job, &spec).await
}

/// `POST /jobs/<id>/delete` (CSRF-checked): deletes the user's job, then 303 to the list.
async fn delete(
    State(app): State<WebApp>,
    user: WebUser,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let job = match own_job(&app, &user, &id).await {
        Ok(job) => job,
        Err(response) => return *response,
    };
    match app.inner.store.job_delete(job.id).await {
        Ok(_) => {
            app.inner.jobs.notify();
            to_list()
        }
        Err(err) => internal_error("cannot delete a scheduled job", &err),
    }
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
            conversation: None,
            since: None,
            disabled: false,
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
