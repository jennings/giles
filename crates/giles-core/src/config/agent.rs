//! `agents/<agent>/agent.toml` (PLAN sections 6.3, 6.4, 6.7, 6.10, 6.11, 11.2).

use std::collections::BTreeSet;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::time::Duration;

use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use giles_plugin_sdk::config::HumanDuration;
use giles_types::AgentId;
use jiff::civil::Time;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};

use super::error::{ConfigError, KeyPath};
use super::giles::parse_timezone;
use super::model::ModelRef;
use super::names::{invalid_name_reason, is_valid_name};

/// The iteration cap of job turns when the agent sets no `max_iterations_jobs` (PLAN section 6.3).
pub const DEFAULT_MAX_ITERATIONS_JOBS: u32 = 25;

/// The memory file character cap when the agent sets no `memory_cap` (PLAN section 6.7).
pub const DEFAULT_MEMORY_CAP: u32 = 2200;

/// How long an approval waits when the agent sets no `approval_timeout` (PLAN section 6.4).
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// An agent: `agents/<agent>/agent.toml`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AgentConfig {
    /// The agent's id: its directory name.
    pub id: AgentId,
    /// The agent's directory, `agents/<agent>/`.
    pub dir: PathBuf,
    /// The display name, if set.
    pub name: Option<String>,
    /// The models, in order (PLAN section 6.10). Never empty; v1 uses the first.
    pub models: Vec<ModelRef>,
    /// The model that writes compaction summaries. `None`: use `models`.
    pub compaction_model: Option<ModelRef>,
    /// The tool allowlist: globs over prefixed tool names (PLAN section 6.4).
    pub tools: Vec<String>,
    /// Globs of tools that need approval before each call.
    pub ask: Vec<String>,
    /// The iteration cap of interactive turns; `None` means no cap.
    pub max_iterations: Option<u32>,
    /// The iteration cap of job turns. [`DEFAULT_MAX_ITERATIONS_JOBS`] unless set.
    pub max_iterations_jobs: u32,
    /// What happens to the requester's new messages while a turn runs.
    pub busy_mode: BusyMode,
    /// The character cap of each memory file the agent writes. [`DEFAULT_MEMORY_CAP`] unless set.
    pub memory_cap: u32,
    /// How long a pending approval waits before it is denied. [`DEFAULT_APPROVAL_TIMEOUT`] unless
    /// set.
    pub approval_timeout: Duration,
    /// The agent's time zone, used when the owner sets none.
    pub timezone: Option<TimeZone>,
    /// Start a new session after this long without messages; off unless set.
    pub idle_reset: Option<Duration>,
    /// Config jobs (PLAN section 6.11), in file order.
    pub jobs: Vec<Job>,
}

/// What happens to the requester's new messages while a turn runs (PLAN section 6.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BusyMode {
    /// Append them to the running turn at the next tool-batch boundary.
    #[default]
    Steer,
    /// Keep them for the next turn.
    Queue,
}

/// A config job: a `[[jobs]]` entry in `agent.toml` (PLAN section 6.11).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Job {
    /// The stable id, unique within the agent; `job_runs` is keyed by it.
    pub id: String,
    /// The schedule: a five-field cron pattern (minute, hour, day of month, month, day of week).
    pub cron: Cron,
    /// Which session the job runs in.
    pub session: JobSession,
    /// Wait while a turn runs in the owner's DM.
    pub idle_only: bool,
    /// Run only inside these hours, in the job's time zone.
    pub active_hours: Option<ActiveHours>,
    /// The model for this job. `None`: the agent's `models`.
    pub model: Option<ModelRef>,
    /// The prompt.
    pub prompt: JobPrompt,
    /// Who answers approvals for `ask` tools. `None`: they're denied ("denied (unattended)").
    pub ask_to: Option<AskTo>,
    /// For `isolated` jobs, how many of the latest messages of the owner's DM to include.
    pub dm_excerpt: Option<u32>,
}

/// Which session a job runs in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobSession {
    /// A fresh context for every run.
    #[default]
    Isolated,
    /// The owner's DM session, as its own queued turn.
    Dm,
}

/// Who answers approvals in a job turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AskTo {
    /// The user the job delivers to; in v1, always the owner.
    DeliverTo,
}

/// A job's prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobPrompt {
    /// `prompt = "..."`.
    Inline(String),
    /// `prompt_file`: a file in the agent's directory (absolute path), read at each run.
    File(PathBuf),
}

/// `active_hours = "HH:MM-HH:MM"`: from the start time up to (not including) the end time. An end
/// before the start wraps past midnight (`"22:00-06:00"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveHours {
    /// The first minute inside the window.
    pub start: Time,
    /// The first minute after the window.
    pub end: Time,
}

impl ActiveHours {
    /// Parses `"HH:MM-HH:MM"` (24-hour, two digits each). The start and end must differ.
    ///
    /// # Errors
    ///
    /// The reason, if `text` is not valid.
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = || {
            format!("invalid active hours {text:?}: write \"HH:MM-HH:MM\", e.g. \"08:00-22:00\"")
        };
        let (start, end) = text.split_once('-').ok_or_else(invalid)?;
        let start = parse_hhmm(start).ok_or_else(invalid)?;
        let end = parse_hhmm(end).ok_or_else(invalid)?;
        if start == end {
            return Err(format!(
                "invalid active hours {text:?}: the start and end are the same"
            ));
        }
        Ok(Self { start, end })
    }

    /// Whether `time` is inside the window.
    pub fn contains(&self, time: Time) -> bool {
        if self.start < self.end {
            self.start <= time && time < self.end
        } else {
            self.start <= time || time < self.end
        }
    }
}

/// `HH:MM`, exactly two digits each.
fn parse_hhmm(text: &str) -> Option<Time> {
    let (h, m) = text.split_once(':')?;
    if h.len() != 2 || m.len() != 2 || !(h.bytes().chain(m.bytes())).all(|b| b.is_ascii_digit()) {
        return None;
    }
    Time::new(h.parse().ok()?, m.parse().ok()?, 0, 0).ok()
}

/// Parses a five-field cron pattern (minute, hour, day of month, month, day of week; no seconds
/// or years), as config and runtime jobs take it.
///
/// # Errors
///
/// The reason, if `pattern` is not valid.
pub fn parse_cron(pattern: &str) -> Result<Cron, String> {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(pattern)
        .map_err(|e| format!("invalid cron pattern {pattern:?}: {e}"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    name: Option<String>,
    models: Vec<ModelRef>,
    compaction_model: Option<ModelRef>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    ask: Vec<String>,
    max_iterations: Option<u32>,
    max_iterations_jobs: Option<u32>,
    #[serde(default)]
    busy_mode: BusyMode,
    memory_cap: Option<u32>,
    approval_timeout: Option<HumanDuration>,
    timezone: Option<String>,
    idle_reset: Option<HumanDuration>,
    #[serde(default)]
    jobs: Vec<RawJob>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJob {
    id: String,
    cron: String,
    #[serde(default)]
    session: JobSession,
    #[serde(default)]
    idle_only: bool,
    active_hours: Option<String>,
    model: Option<ModelRef>,
    prompt: Option<String>,
    prompt_file: Option<String>,
    ask_to: Option<AskTo>,
    dm_excerpt: Option<u32>,
}

/// Loads `agent.toml` from `dir` for agent `id`. `providers` are the configured provider instances
/// every model must name.
pub(crate) fn load(
    id: &str,
    dir: &Path,
    providers: &BTreeSet<&str>,
) -> Result<AgentConfig, ConfigError> {
    let file = dir.join("agent.toml");
    let source = super::read_file(&file)?;
    let raw: RawAgent = super::parse_toml(&file, &source)?;
    let err = |path: KeyPath, reason: String| {
        ConfigError::at(&file, path, reason).located(&file, &source)
    };
    let root = KeyPath::root();

    let check_model = |path: KeyPath, model: &ModelRef| {
        if providers.contains(model.provider.as_str()) {
            Ok(())
        } else {
            Err(err(
                path,
                format!(
                    "no provider instance {:?} (`[provider.{0}]` in giles.toml) for model {model}",
                    model.provider.as_str()
                ),
            ))
        }
    };
    if raw.models.is_empty() {
        return Err(err(
            root.key("models"),
            "must list at least one model".into(),
        ));
    }
    for (i, model) in raw.models.iter().enumerate() {
        check_model(root.key("models").index(i), model)?;
    }
    if let Some(model) = &raw.compaction_model {
        check_model(root.key("compaction_model"), model)?;
    }
    for key in ["tools", "ask"] {
        let globs = if key == "tools" { &raw.tools } else { &raw.ask };
        for (i, glob) in globs.iter().enumerate() {
            if glob.is_empty() || glob.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(err(
                    root.key(key).index(i),
                    format!("invalid tool glob {glob:?}: it must be non-empty, with no spaces"),
                ));
            }
        }
    }
    let positive = |key: &str, value: Option<u32>| match value {
        Some(0) => Err(err(root.key(key), "must be at least 1".into())),
        _ => Ok(value),
    };
    let max_iterations = positive("max_iterations", raw.max_iterations)?;
    let max_iterations_jobs = positive("max_iterations_jobs", raw.max_iterations_jobs)?
        .unwrap_or(DEFAULT_MAX_ITERATIONS_JOBS);
    let memory_cap = positive("memory_cap", raw.memory_cap)?.unwrap_or(DEFAULT_MEMORY_CAP);
    let nonzero = |key: &str, value: Option<HumanDuration>| match value {
        Some(HumanDuration(d)) if d.is_zero() => {
            Err(err(root.key(key), "must be longer than 0s".into()))
        }
        _ => Ok(value.map(|d| d.0)),
    };
    let approval_timeout =
        nonzero("approval_timeout", raw.approval_timeout)?.unwrap_or(DEFAULT_APPROVAL_TIMEOUT);
    let idle_reset = nonzero("idle_reset", raw.idle_reset)?;
    let timezone = raw
        .timezone
        .as_deref()
        .map(|name| parse_timezone(name).map_err(|r| err(root.key("timezone"), r)))
        .transpose()?;

    let mut ids = BTreeSet::new();
    let mut jobs = Vec::with_capacity(raw.jobs.len());
    for (i, job) in raw.jobs.into_iter().enumerate() {
        let path = root.key("jobs").index(i);
        let err = |key: &str, reason: String| err(path.key(key), reason);
        if !is_valid_name(&job.id) {
            return Err(err("id", invalid_name_reason("job", &job.id)));
        }
        if !ids.insert(job.id.clone()) {
            return Err(err(
                "id",
                format!("another job already has id {:?}", job.id),
            ));
        }
        let cron = parse_cron(&job.cron).map_err(|r| err("cron", r))?;
        let active_hours = job
            .active_hours
            .as_deref()
            .map(|text| ActiveHours::parse(text).map_err(|r| err("active_hours", r)))
            .transpose()?;
        if let Some(model) = &job.model {
            check_model(path.key("model"), model)?;
        }
        let prompt = match (job.prompt, job.prompt_file) {
            (Some(_), Some(_)) => {
                return Err(err(
                    "prompt_file",
                    "set `prompt` or `prompt_file`, not both".into(),
                ));
            }
            (None, None) => {
                let reason = "missing `prompt` or `prompt_file`";
                return Err(ConfigError::at(&file, path, reason).located(&file, &source));
            }
            (Some(text), None) if text.trim().is_empty() => {
                return Err(err("prompt", "must not be empty".into()));
            }
            (Some(text), None) => JobPrompt::Inline(text),
            (None, Some(name)) => {
                JobPrompt::File(prompt_file(dir, &name).map_err(|r| err("prompt_file", r))?)
            }
        };
        if job.dm_excerpt.is_some() && job.session == JobSession::Dm {
            return Err(err(
                "dm_excerpt",
                "only for `session = \"isolated\"`; a `dm` job already runs in the DM".into(),
            ));
        }
        jobs.push(Job {
            id: job.id,
            cron,
            session: job.session,
            idle_only: job.idle_only,
            active_hours,
            model: job.model,
            prompt,
            ask_to: job.ask_to,
            dm_excerpt: job.dm_excerpt,
        });
    }

    Ok(AgentConfig {
        id: AgentId::new(id).map_err(|e| ConfigError::file(&file, e.to_string()))?,
        dir: dir.to_path_buf(),
        name: raw.name,
        models: raw.models,
        compaction_model: raw.compaction_model,
        tools: raw.tools,
        ask: raw.ask,
        max_iterations,
        max_iterations_jobs,
        busy_mode: raw.busy_mode,
        memory_cap,
        approval_timeout,
        timezone,
        idle_reset,
        jobs,
    })
}

/// Resolves a job's `prompt_file`: a relative path inside the agent's directory, with no `..`,
/// naming an existing file.
fn prompt_file(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let relative = Path::new(name);
    if name.is_empty()
        || !relative
            .components()
            .all(|c| matches!(c, PathComponent::Normal(_) | PathComponent::CurDir))
    {
        return Err(format!(
            "invalid prompt file {name:?}: it must be a relative path inside the agent's \
             directory, with no `..`"
        ));
    }
    let path = dir.join(relative);
    if !path.is_file() {
        return Err(format!("prompt file {} does not exist", path.display()));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(h: i8, m: i8) -> Time {
        Time::new(h, m, 0, 0).unwrap()
    }

    #[test]
    fn active_hours() {
        let day = ActiveHours::parse("08:00-22:00").unwrap();
        assert_eq!((day.start, day.end), (time(8, 0), time(22, 0)));
        assert!(day.contains(time(8, 0)));
        assert!(day.contains(time(21, 59)));
        assert!(!day.contains(time(22, 0)));
        assert!(!day.contains(time(7, 59)));

        let night = ActiveHours::parse("22:30-06:00").unwrap();
        assert!(night.contains(time(23, 0)));
        assert!(night.contains(time(0, 0)));
        assert!(night.contains(time(5, 59)));
        assert!(!night.contains(time(6, 0)));
        assert!(!night.contains(time(12, 0)));

        let to_midnight = ActiveHours::parse("18:00-00:00").unwrap();
        assert!(to_midnight.contains(time(23, 59)));
        assert!(!to_midnight.contains(time(0, 0)));

        for bad in [
            "",
            "8:00-22:00",
            "08:00",
            "08:00-24:00",
            "08:60-09:00",
            "08:00-22:00-23:00",
            "08.00-22.00",
            "+8:00-22:00",
            "08:00 - 22:00",
        ] {
            let err = ActiveHours::parse(bad).unwrap_err();
            assert!(err.contains("HH:MM-HH:MM"), "{bad:?}: {err}");
        }
        let err = ActiveHours::parse("08:00-08:00").unwrap_err();
        assert!(err.contains("the same"), "{err}");
    }

    #[test]
    fn cron_patterns_have_five_fields() {
        for ok in [
            "*/30 * * * *",
            "0 8 * * 1-5",
            "0 8 * * MON-FRI",
            "15 3 1 * *",
        ] {
            assert!(parse_cron(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "* * * *",
            "0 0 8 * * *",
            "0 0 8 * * * 2030",
            "61 * * * *",
            "nonsense",
        ] {
            let err = parse_cron(bad).unwrap_err();
            assert!(err.contains("invalid cron pattern"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn prompt_files_stay_inside_the_agent_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("morning.md"), "Good morning").unwrap();
        std::fs::create_dir(dir.path().join("jobs")).unwrap();
        std::fs::write(dir.path().join("jobs/weekly.md"), "Weekly").unwrap();
        assert_eq!(
            prompt_file(dir.path(), "morning.md").unwrap(),
            dir.path().join("morning.md")
        );
        assert_eq!(
            prompt_file(dir.path(), "./jobs/weekly.md").unwrap(),
            dir.path().join("./jobs/weekly.md")
        );
        for bad in ["", "/etc/passwd", "../secrets.toml", "jobs/../morning.md"] {
            let err = prompt_file(dir.path(), bad).unwrap_err();
            assert!(err.contains("relative path inside"), "{bad:?}: {err}");
        }
        let err = prompt_file(dir.path(), "missing.md").unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        let err = prompt_file(dir.path(), "jobs").unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
    }
}
