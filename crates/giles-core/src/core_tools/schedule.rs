//! The `schedule` core tool (PLAN section 6.11): the agent creates, lists, and cancels runtime
//! jobs for the requesting user ("remind me at 5"). A job is a row in `jobs` whose spec is a
//! [`RuntimeSpec`], created by the requester, who it runs as and delivers to; the scheduler
//! picks changes up through [`JobChanges`]. `list` also shows the agent's config jobs, which the
//! tool can't change.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use giles_store::{RuntimeJobId, Store};
use giles_types::{AgentId, Block, Timestamp, ToolSpec, UserId};
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use super::{CoreCall, CoreTool};
use crate::agent_loop::ToolOutput;
use crate::agent_loop::history::message_timezone;
use crate::config::{Config, JobPrompt, JobSession, parse_cron, parse_duration};
use crate::scheduler::{JobChanges, RuntimeSpec, When};
use crate::sessions::Clock;

/// The tool's name.
pub const SCHEDULE: &str = "schedule";

/// The most characters of a prompt `list` shows.
pub const MAX_LISTED_PROMPT_CHARS: usize = 100;

/// The most runtime jobs one user may have with one agent at a time, so a runaway agent can't
/// pile up jobs (each one costs model calls and messages).
pub const MAX_RUNTIME_JOBS_PER_USER: usize = 20;

/// How times are shown: `Thu 2026-10-22 17:00 PDT`.
const TIME_FORMAT: &str = "%a %Y-%m-%d %H:%M %Z";

/// The `schedule` tool.
pub struct ScheduleTool {
    store: Store,
    config: Arc<Config>,
    changes: JobChanges,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for ScheduleTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleTool")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl ScheduleTool {
    /// Keeps runtime jobs in `store`, reading times in the zones `config` gives each agent, and
    /// tells the scheduler through `changes`.
    pub fn new(
        store: Store,
        config: Arc<Config>,
        changes: JobChanges,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            config,
            changes,
            clock,
        }
    }
}

/// The tool's input.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    action: String,
    prompt: Option<String>,
    cron: Option<String>,
    at: Option<String>,
    #[serde(rename = "in")]
    after: Option<String>,
    session: Option<JobSession>,
    id: Option<i64>,
}

/// What the model asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// A new job.
    Create(RuntimeSpec),
    /// The requester's jobs and the agent's config jobs.
    List,
    /// Delete the requester's job with this id.
    Cancel(RuntimeJobId),
}

/// Parses the tool's input at `now`, reading `at` without an offset in `tz`: `create` turns
/// `in` into `at` and checks that the job will run; `list` and `cancel` take nothing else.
///
/// # Errors
///
/// The message for the model.
pub fn parse_input(input: Value, now: Timestamp, tz: &TimeZone) -> Result<Request, String> {
    let input: Input =
        serde_json::from_value(input).map_err(|error| format!("invalid input: {error}"))?;
    let create_only = input.prompt.is_some()
        || input.cron.is_some()
        || input.at.is_some()
        || input.after.is_some()
        || input.session.is_some();
    match input.action.as_str() {
        "create" => {
            if input.id.is_some() {
                return Err("`id` is only for `cancel`".to_owned());
            }
            let prompt = input.prompt.unwrap_or_default();
            if prompt.trim().is_empty() {
                return Err("`prompt` is required: what to do when the job runs".to_owned());
            }
            let (cron, at) = match (input.cron, input.at, input.after) {
                (Some(cron), None, None) => {
                    let pattern = parse_cron(&cron)?;
                    if When::Cron(Box::new(pattern)).next_after(tz, now).is_none() {
                        return Err(format!("the cron pattern {cron:?} never matches again"));
                    }
                    (Some(cron), None)
                }
                (None, Some(at), None) => (None, Some(parse_at(&at, tz)?)),
                (None, None, Some(after)) => {
                    let delay = parse_duration(&after)?;
                    let at = now
                        .checked_add(delay)
                        .map_err(|_| format!("`in` {after:?} is too far away"))?;
                    (None, Some(at))
                }
                _ => return Err("give exactly one of `cron`, `at`, and `in`".to_owned()),
            };
            if at.is_some_and(|at| at <= now) {
                return Err("that time has passed; give a time in the future".to_owned());
            }
            Ok(Request::Create(RuntimeSpec {
                cron,
                at,
                prompt,
                session: input.session.unwrap_or_default(),
            }))
        }
        "list" | "cancel" if create_only => Err(format!(
            "`{}` takes no `prompt`, `cron`, `at`, `in`, or `session`",
            input.action
        )),
        "list" if input.id.is_some() => Err("`id` is only for `cancel`".to_owned()),
        "list" => Ok(Request::List),
        "cancel" => match input.id {
            Some(id) => Ok(Request::Cancel(RuntimeJobId::new(id))),
            None => Err("`id` is required: the number `list` shows".to_owned()),
        },
        other => Err(format!(
            "unknown action {other:?}: use \"create\", \"list\", or \"cancel\""
        )),
    }
}

/// A time: RFC 3339 with an offset (`2026-10-22T17:00:00Z`), or a date and time without one
/// (`2026-10-22T17:00`), read in `tz`.
fn parse_at(text: &str, tz: &TimeZone) -> Result<Timestamp, String> {
    if let Ok(at) = text.parse::<Timestamp>() {
        return Ok(at);
    }
    let civil: DateTime = text.parse().map_err(|_| {
        format!(
            "invalid time {text:?}: write \"YYYY-MM-DDTHH:MM\" in the user's time zone, or with \
             an offset"
        )
    })?;
    tz.to_ambiguous_zoned(civil)
        .compatible()
        .map(|zoned| zoned.timestamp())
        .map_err(|error| format!("invalid time {text:?}: {error}"))
}

fn show_time(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone()).strftime(TIME_FORMAT).to_string()
}

/// `text` on one line, cut to [`MAX_LISTED_PROMPT_CHARS`].
fn short(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(MAX_LISTED_PROMPT_CHARS) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line,
    }
}

/// How a job's schedule reads: `every \`0 9 * * *\`, next Fri ...` or `once, Thu ...`.
fn describe(when: &When, cron: Option<&str>, tz: &TimeZone, now: Timestamp) -> String {
    let next = when.next_after(tz, now);
    match (when, cron, next) {
        (When::Cron(_), Some(pattern), Some(next)) => {
            format!("every `{pattern}`, next {}", show_time(next, tz))
        }
        (When::Cron(_), Some(pattern), None) => format!("every `{pattern}`, no future run"),
        (When::At(at), ..) => format!("once, {}", show_time(*at, tz)),
        (When::Cron(_), None, _) => "on a schedule".to_owned(),
    }
}

impl ScheduleTool {
    async fn create(
        &self,
        agent: &AgentId,
        requester: UserId,
        spec: RuntimeSpec,
        tz: &TimeZone,
    ) -> ToolOutput {
        let now = self.clock.now();
        match self
            .store
            .jobs(Some(agent.clone()), Some(requester.clone()))
            .await
        {
            Ok(jobs) if jobs.len() >= MAX_RUNTIME_JOBS_PER_USER => {
                return ToolOutput::error(format!(
                    "too many scheduled jobs (at most {MAX_RUNTIME_JOBS_PER_USER}); cancel one \
                     first"
                ));
            }
            Ok(_) => {}
            Err(error) => {
                warn!(%agent, %error, "cannot count runtime jobs");
                return ToolOutput::error("the jobs couldn't be read");
            }
        }
        let when = match spec.when() {
            Ok(when) => when,
            Err(error) => return ToolOutput::error(error),
        };
        let value = match serde_json::to_value(&spec) {
            Ok(value) => value,
            Err(error) => return ToolOutput::error(format!("cannot store the job: {error}")),
        };
        match self
            .store
            .job_create(agent.clone(), requester, value, now)
            .await
        {
            Ok(job) => {
                self.changes.notify();
                info!(%agent, job = job.id.get(), "runtime job created");
                ok(format!(
                    "Scheduled job #{}: {}.",
                    job.id,
                    describe(&when, spec.cron.as_deref(), tz, now)
                ))
            }
            Err(error) => {
                warn!(%agent, %error, "cannot create a runtime job");
                ToolOutput::error("the job couldn't be saved")
            }
        }
    }

    async fn list(&self, agent: &AgentId, requester: Option<UserId>, tz: &TimeZone) -> ToolOutput {
        let now = self.clock.now();
        let mut out = String::new();
        if let Some(requester) = requester {
            let jobs = match self.store.jobs(Some(agent.clone()), Some(requester)).await {
                Ok(jobs) => jobs,
                Err(error) => {
                    warn!(%agent, %error, "cannot list runtime jobs");
                    return ToolOutput::error("the jobs couldn't be read");
                }
            };
            if jobs.is_empty() {
                out.push_str("No jobs scheduled for this user.");
            } else {
                out.push_str("Jobs scheduled for this user (cancel by number):");
            }
            for job in jobs {
                let line = match serde_json::from_value::<RuntimeSpec>(job.spec.clone()) {
                    Ok(spec) => match spec.when() {
                        Ok(when) => format!(
                            "{} ({}): {}",
                            describe(&when, spec.cron.as_deref(), tz, now),
                            session_name(spec.session),
                            short(&spec.prompt)
                        ),
                        Err(error) => format!("unusable: {error}"),
                    },
                    Err(error) => format!("unusable: {error}"),
                };
                let _ = write!(out, "\n- #{}: {line}", job.id);
            }
        } else {
            out.push_str("Runtime jobs belong to users; this turn has none.");
        }
        if let Some(config) = self.config.agents.get(agent)
            && !config.jobs.is_empty()
        {
            out.push_str("\n\nConfigured jobs (read-only):");
            for job in &config.jobs {
                let when = When::Cron(Box::new(job.cron.clone()));
                let prompt = match &job.prompt {
                    JobPrompt::Inline(text) => short(text),
                    JobPrompt::File(path) => format!(
                        "prompt in {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                };
                let _ = write!(
                    out,
                    "\n- `{}`: {} ({}): {prompt}",
                    job.id,
                    describe(&when, Some(job.cron.pattern.as_str()), tz, now),
                    session_name(job.session)
                );
            }
        }
        ok(out)
    }

    async fn cancel(&self, agent: &AgentId, requester: &UserId, id: RuntimeJobId) -> ToolOutput {
        let not_found = || ToolOutput::error(format!("no job #{id} of this user's to cancel"));
        match self.store.job(id).await {
            Ok(Some(job)) if job.agent == *agent && job.creator == *requester => {}
            Ok(_) => return not_found(),
            Err(error) => {
                warn!(%agent, %error, "cannot read a runtime job");
                return ToolOutput::error("the job couldn't be read");
            }
        }
        match self.store.job_delete(id).await {
            Ok(true) => {
                self.changes.notify();
                info!(%agent, job = id.get(), "runtime job cancelled");
                ok(format!("Cancelled job #{id}."))
            }
            Ok(false) => not_found(),
            Err(error) => {
                warn!(%agent, %error, "cannot delete a runtime job");
                ToolOutput::error("the job couldn't be cancelled")
            }
        }
    }
}

fn session_name(session: JobSession) -> &'static str {
    match session {
        JobSession::Dm => "in the DM",
        _ => "on its own",
    }
}

fn ok(text: String) -> ToolOutput {
    ToolOutput {
        content: vec![Block::text(text)],
        is_error: false,
    }
}

#[async_trait]
impl CoreTool for ScheduleTool {
    fn spec(&self) -> ToolSpec {
        spec()
    }

    async fn call(&self, call: CoreCall) -> ToolOutput {
        let agent = &call.context.agent;
        let Some(agent_config) = self.config.agents.get(agent) else {
            return ToolOutput::error(format!("agent `{agent}` is not configured"));
        };
        // Runtime jobs run as the requester, in their time zone (v1: the owner's).
        let tz = message_timezone(&self.config, agent_config);
        let request = match parse_input(call.input, self.clock.now(), &tz) {
            Ok(request) => request,
            Err(error) => return ToolOutput::error(error),
        };
        let requester = call.context.requester.clone();
        match (request, requester) {
            (Request::List, requester) => self.list(agent, requester, &tz).await,
            // A job run can't schedule jobs, so a job (or an instruction injected into one)
            // can't start a chain of jobs that create more jobs.
            (Request::Create(_), Some(_)) if call.turn.job => ToolOutput::error(
                "jobs can't be scheduled from a scheduled job's run; ask the user to schedule it",
            ),
            (Request::Create(spec), Some(requester)) => {
                self.create(agent, requester, spec, &tz).await
            }
            (Request::Cancel(id), Some(requester)) => self.cancel(agent, &requester, id).await,
            (_, None) => ToolOutput::error(
                "jobs are scheduled for the user who asks; this turn has no requesting user",
            ),
        }
    }
}

/// The tool's spec.
pub(super) fn spec() -> ToolSpec {
    ToolSpec {
        name: SCHEDULE.to_owned(),
        description: "Schedule a prompt for yourself to run later for this user, once or on a \
                      cron schedule (\"remind me at 5\", \"every weekday at 9, summarize my \
                      inbox\"); list this user's jobs and the configured ones; or cancel one of \
                      this user's jobs. When a job runs you get its prompt, and your reply is \
                      sent to the user; reply exactly NO_REPLY to send nothing. Times are in the \
                      user's time zone."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["create", "list", "cancel"],
                },
                "prompt": {
                    "type": "string",
                    "minLength": 1,
                    "description": "create: what you'll be asked to do when the job runs, \
                                    written to your future self, e.g. \"Remind the user to \
                                    call the dentist.\"",
                },
                "cron": {
                    "type": "string",
                    "description": "create, repeating: a five-field cron pattern (minute hour \
                                    day-of-month month day-of-week), e.g. \"0 9 * * 1-5\".",
                },
                "at": {
                    "type": "string",
                    "description": "create, once: a date and time, \"YYYY-MM-DDTHH:MM\" in the \
                                    user's time zone, or RFC 3339 with an offset.",
                },
                "in": {
                    "type": "string",
                    "description": "create, once: a delay from now, e.g. \"20m\", \"2h\", \
                                    \"1d\", \"1h30m\".",
                },
                "session": {
                    "type": "string",
                    "enum": ["isolated", "dm"],
                    "default": "isolated",
                    "description": "create: \"isolated\" runs with a fresh context; \"dm\" runs \
                                    in this user's DM with you, with its history.",
                },
                "id": {
                    "type": "integer",
                    "description": "cancel: the job's number from `list`.",
                },
            },
            "required": ["action"],
            "additionalProperties": false,
        }),
    }
}
