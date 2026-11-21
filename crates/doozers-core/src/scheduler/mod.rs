//! The scheduler (PLAN section 6.11): runs config jobs (`[[jobs]]` in `agent.toml`) and runtime
//! jobs (the `jobs` table, made by the `schedule` tool) on their cron patterns or one-shot times.
//! Design: `Docs/technical/scheduler.md`.
//!
//! Each job has a task that sleeps until its next time, then runs it ([`run`]): an `isolated`
//! job in a fresh job session, a `dm` job as its own turn in the user's DM session, queued
//! behind the turn running there ([`Sessions::hold`]). A runtime job scheduled in a conversation
//! uses that conversation as its DM. A reply of exactly `NO_REPLY` sends and stores nothing; any
//! other reply goes through [`JobDelivery`] (the outbox) into that conversation, or else to the
//! user's default identity, and is appended to the DM session. Each run's start is recorded in
//! `job_runs`; times missed while the core was down are coalesced into one run at startup.

mod job;
mod run;
mod schedule;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use doozers_store::{JobKey, OutboxId, RuntimeJobId, Store};
use doozers_types::{AgentId, Timestamp, UserId};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, error, info, warn};

pub use job::{JobConversation, JobDef, RuntimeSpec};
pub use run::{JobReply, NO_REPLY, job_reply};
pub use schedule::{When, in_active_hours};

use crate::agent_loop::history::message_timezone;
use crate::config::Job;
use crate::outbox::{Destination, Outbox, OutboxItem, OutboxKind};
use crate::registry::Registry;
use crate::sessions::{Clock, Sessions, TurnRunner};
use crate::users::UserList;
use crate::workspaces::Workspaces;

/// The longest the scheduler sleeps before looking at the clock again, so a wall clock that
/// jumps (a laptop waking up) delays a job by at most this much.
pub const MAX_SLEEP: Duration = Duration::from_secs(60);

/// How long the scheduler waits before reading the runtime jobs again after a database error.
pub const RELOAD_RETRY: Duration = Duration::from_secs(30);

/// What a job sends to its user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeliveryKind {
    /// A job's reply.
    JobReply,
    /// An approval prompt of a job turn with `ask_to = "deliver_to"`.
    ApprovalPrompt,
}

/// One proactive send of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// Who it goes to.
    pub user: UserId,
    /// The conversation it goes into; `None`: the user's default identity.
    pub conversation: Option<JobConversation>,
    /// What it is.
    pub kind: DeliveryKind,
    /// The message.
    pub markdown: String,
    /// When it's dropped if still undelivered; `None`: the outbox's default.
    pub expires_at: Option<Timestamp>,
}

/// Where jobs send their output: the outbox, which delivers into the job's conversation or to the
/// user's default identity once the channel is up.
#[async_trait]
pub trait JobDelivery: Send + Sync {
    /// Queues `delivery`.
    ///
    /// # Errors
    ///
    /// Why it can't be queued (no channel can reach the identity or the conversation, a database
    /// error).
    async fn enqueue(&self, delivery: Delivery) -> Result<OutboxId, String>;

    /// Withdraws a queued send that hasn't gone out yet; nothing if it has.
    async fn remove(&self, id: OutboxId);
}

/// The daemon's [`JobDelivery`]: the outbox, into the job's conversation, or else to the user's
/// default identity.
#[derive(Debug, Clone)]
pub struct OutboxDelivery {
    outbox: Outbox,
    users: UserList,
}

impl OutboxDelivery {
    /// Sends through `outbox`, to the default identity of the delivery's user in `users` when
    /// there's no conversation.
    pub fn new(outbox: Outbox, users: UserList) -> Self {
        Self { outbox, users }
    }
}

#[async_trait]
impl JobDelivery for OutboxDelivery {
    async fn enqueue(&self, delivery: Delivery) -> Result<OutboxId, String> {
        let kind = match delivery.kind {
            DeliveryKind::JobReply => OutboxKind::JobReply,
            DeliveryKind::ApprovalPrompt => OutboxKind::ApprovalPrompt,
        };
        let mut item = match delivery.conversation {
            Some(conversation) => OutboxItem {
                user: Some(delivery.user),
                ..OutboxItem::new(
                    Destination::Conversation {
                        instance: conversation.channel,
                        conversation: conversation.id,
                    },
                    kind,
                    delivery.markdown,
                )
            },
            None => {
                let directory = self.users.get();
                directory
                    .user(&delivery.user)
                    .and_then(|user| OutboxItem::to_user(user, kind, delivery.markdown))
                    .ok_or_else(|| format!("user {} has no identity", delivery.user))?
            }
        };
        item.expires_at = delivery.expires_at;
        self.outbox
            .enqueue(item)
            .await
            .map_err(|error| error.to_string())
    }

    async fn remove(&self, id: OutboxId) {
        if let Err(error) = self.outbox.remove(id).await {
            warn!(%id, %error, "cannot withdraw a queued send");
        }
    }
}

/// Tells the scheduler that runtime jobs changed in the store (the `schedule` tool). Cheap to
/// clone; every clone signals the same scheduler.
#[derive(Debug, Clone)]
pub struct JobChanges(Arc<watch::Sender<u64>>);

impl Default for JobChanges {
    fn default() -> Self {
        Self(Arc::new(watch::Sender::new(0)))
    }
}

impl JobChanges {
    /// Runtime jobs were created, changed, or deleted: the scheduler reads them again.
    pub fn notify(&self) {
        self.0
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// A receiver that sees every later [`notify`](Self::notify).
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.0.subscribe()
    }
}

/// What [`Scheduler::start`] needs.
pub struct SchedulerOptions {
    /// The database: runtime jobs, `job_runs`, sessions.
    pub store: Store,
    /// The configuration (agents and their jobs, the owner) and the registered channels (which
    /// instance owns the owner's default identity).
    pub registry: Arc<Registry>,
    /// The session manager: DM sessions are held through it for `dm` jobs and replies.
    pub sessions: Sessions,
    /// Runs the job turns: the agent loop.
    pub runner: Arc<dyn TurnRunner>,
    /// Sends replies and approval prompts.
    pub delivery: Arc<dyn JobDelivery>,
    /// Signals changes to runtime jobs.
    pub changes: JobChanges,
    /// The environment workspaces: an isolated run's is deleted when the run ends.
    pub workspaces: Workspaces,
    /// The time source.
    pub clock: Arc<dyn Clock>,
}

impl fmt::Debug for SchedulerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerOptions")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// The running scheduler. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Scheduler {
    tasks: TaskTracker,
    running: JobsRunning,
}

/// The job runs in progress, per agent and per creator: from the moment a job's time comes until
/// its run is over (waiting for a busy DM and the turn included). The web UI won't remove an
/// agent that has one ([`Scheduler::agent_busy`]), or a user whose job has ([`Scheduler::user_busy`]).
#[derive(Debug, Clone, Default)]
pub(crate) struct JobsRunning(Arc<Mutex<Counts>>);

#[derive(Debug, Default)]
struct Counts {
    agents: HashMap<AgentId, usize>,
    creators: HashMap<UserId, usize>,
}

/// One job run in progress; dropping it ends the count.
pub(crate) struct RunningJob {
    running: JobsRunning,
    agent: AgentId,
    creator: Option<UserId>,
}

fn decrement<K: std::hash::Hash + Eq>(counts: &mut HashMap<K, usize>, key: &K) {
    if let Some(count) = counts.get_mut(key) {
        *count -= 1;
        if *count == 0 {
            counts.remove(key);
        }
    }
}

impl JobsRunning {
    /// Counts a run of a job of `agent`, made by `creator` if a runtime job, until the result is
    /// dropped.
    pub(crate) fn begin(&self, agent: &AgentId, creator: Option<&UserId>) -> RunningJob {
        {
            let mut counts = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            *counts.agents.entry(agent.clone()).or_default() += 1;
            if let Some(creator) = creator {
                *counts.creators.entry(creator.clone()).or_default() += 1;
            }
        }
        RunningJob {
            running: self.clone(),
            agent: agent.clone(),
            creator: creator.cloned(),
        }
    }

    fn is_running(&self, agent: &AgentId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .agents
            .contains_key(agent)
    }

    fn is_running_for(&self, user: &UserId) -> bool {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .creators
            .contains_key(user)
    }
}

impl Drop for RunningJob {
    fn drop(&mut self) {
        let mut counts = self
            .running
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        decrement(&mut counts.agents, &self.agent);
        if let Some(creator) = &self.creator {
            decrement(&mut counts.creators, creator);
        }
    }
}

/// Everything a job's task uses.
pub(crate) struct Context {
    pub(crate) running: JobsRunning,
    pub(crate) store: Store,
    pub(crate) registry: Arc<Registry>,
    pub(crate) sessions: Sessions,
    pub(crate) runner: Arc<dyn TurnRunner>,
    pub(crate) delivery: Arc<dyn JobDelivery>,
    pub(crate) workspaces: Workspaces,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) shutdown: CancellationToken,
}

impl Scheduler {
    /// Starts a task per config job of every agent and per runtime job in the store, and keeps
    /// them in step with the running configuration (config jobs, as agents are edited and
    /// removed) and [`JobChanges`] (runtime jobs). Start it after the session manager.
    /// Everything stops when `shutdown` is cancelled; a running job turn is cancelled with it.
    pub fn start(options: SchedulerOptions, shutdown: CancellationToken) -> Self {
        let tasks = TaskTracker::new();
        let in_progress = JobsRunning::default();
        let context = Arc::new(Context {
            running: in_progress.clone(),
            store: options.store,
            registry: options.registry,
            sessions: options.sessions,
            runner: options.runner,
            delivery: options.delivery,
            workspaces: options.workspaces,
            clock: options.clock,
            shutdown,
        });
        let mut running = HashMap::new();
        let config_changes = context.registry.live_config().subscribe();
        reconcile_config_jobs(&context, &tasks, &mut running);
        tasks.spawn(config_jobs(
            context.clone(),
            config_changes,
            tasks.clone(),
            running,
        ));
        let changes = options.changes.subscribe();
        tasks.spawn(runtime_jobs(context, changes, tasks.clone()));
        Self {
            tasks,
            running: in_progress,
        }
    }

    /// Whether a job run of `agent` is in progress now: a job's time has come and its run isn't
    /// over, whether it waits for the DM or its turn is running (an isolated run's turn is not a
    /// session the session manager knows). The count starts before the run reads the agent's
    /// configuration, so a run that read it before the agent was taken out is counted.
    pub fn agent_busy(&self, agent: &AgentId) -> bool {
        self.running.is_running(agent)
    }

    /// Whether a run of a runtime job `user` made is in progress now, an isolated run's turn
    /// included, which the session manager doesn't know. The count starts before the run checks
    /// that its creator is still active, so a run that began before the user was disabled is
    /// counted.
    pub fn user_busy(&self, user: &UserId) -> bool {
        self.running.is_running_for(user)
    }

    /// Waits until every task has stopped. Call it after cancelling the shutdown token.
    pub async fn closed(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }
}

/// The tasks of one agent's config jobs: stopped together, when its jobs change or it is removed.
struct AgentJobs {
    /// The jobs the tasks were started for.
    jobs: Vec<Job>,
    stop: CancellationToken,
}

/// Makes the tasks of the config jobs match the running configuration: an agent that is gone, or
/// whose `[[jobs]]` changed since its tasks started, has them stopped; one with jobs and no tasks
/// gets them started. Edits to an agent that leave its jobs alone keep the tasks (and their
/// place in the schedule); each run reads the agent's other settings when it starts. Idempotent,
/// so it only needs to run after the configuration changed.
fn reconcile_config_jobs(
    context: &Arc<Context>,
    tasks: &TaskTracker,
    running: &mut HashMap<AgentId, AgentJobs>,
) {
    let config = context.registry.config();
    running.retain(|agent, running| {
        let keep = config
            .agents
            .get(agent)
            .is_some_and(|agent| agent.jobs == running.jobs);
        if !keep {
            info!(%agent, "the agent's jobs changed or the agent was removed; its config jobs stop");
            running.stop.cancel();
        }
        keep
    });
    for (agent, agent_config) in &config.agents {
        if agent_config.jobs.is_empty() || running.contains_key(agent) {
            continue;
        }
        let stop = context.shutdown.child_token();
        for job in &agent_config.jobs {
            tasks.spawn(job_task(
                context.clone(),
                JobDef::from_config(agent, job),
                stop.child_token(),
            ));
        }
        running.insert(
            agent.clone(),
            AgentJobs {
                jobs: agent_config.jobs.clone(),
                stop,
            },
        );
    }
}

/// Keeps the config jobs' tasks in step with the running configuration
/// ([`reconcile_config_jobs`]) until shutdown.
async fn config_jobs(
    context: Arc<Context>,
    mut changes: watch::Receiver<u64>,
    tasks: TaskTracker,
    mut running: HashMap<AgentId, AgentJobs>,
) {
    loop {
        tokio::select! {
            biased;
            () = context.shutdown.cancelled() => return,
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        reconcile_config_jobs(&context, &tasks, &mut running);
    }
}

/// Keeps a task running for each enabled runtime job in the store: reads them at startup and
/// again at each change, starting tasks for new jobs, stopping the tasks of deleted and disabled
/// ones, and restarting the task of a job whose spec changed (an edit), so it runs on its new
/// schedule.
async fn runtime_jobs(
    context: Arc<Context>,
    mut changes: watch::Receiver<u64>,
    tasks: TaskTracker,
) {
    // Each task with the spec it was started from.
    let mut running: HashMap<RuntimeJobId, (serde_json::Value, CancellationToken)> = HashMap::new();
    loop {
        changes.borrow_and_update();
        let retry = match context.store.jobs(None, None).await {
            Ok(jobs) => {
                running.retain(|id, (spec, stop)| {
                    let keep = jobs.iter().any(|job| job.id == *id && job.spec == *spec)
                        && !stop.is_cancelled();
                    if !keep {
                        stop.cancel();
                    }
                    keep
                });
                for job in jobs {
                    if running.contains_key(&job.id) {
                        continue;
                    }
                    match JobDef::from_runtime(&job) {
                        Ok(def) if def.disabled => {
                            debug!(job = %def.label(), agent = %def.agent, "runtime job disabled");
                        }
                        Ok(def) => {
                            debug!(job = %def.label(), agent = %def.agent, "runtime job scheduled");
                            let stop = context.shutdown.child_token();
                            running.insert(job.id, (job.spec.clone(), stop.clone()));
                            tasks.spawn(job_task(context.clone(), def, stop));
                        }
                        Err(reason) => {
                            warn!(job = job.id.get(), agent = %job.agent, %reason, "runtime job skipped");
                        }
                    }
                }
                None
            }
            Err(error) => {
                error!(%error, "cannot read the runtime jobs; retrying");
                Some(RELOAD_RETRY)
            }
        };
        tokio::select! {
            biased;
            () = context.shutdown.cancelled() => return,
            changed = changes.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = sleep_or_forever(retry) => {}
        }
    }
}

async fn sleep_or_forever(delay: Option<Duration>) {
    match delay {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending().await,
    }
}

/// One job: sleeps until each of its times and runs it, until `stop` (shutdown, or the job was
/// deleted) or a one-shot job has run, or its agent is no longer configured. The agent's
/// configuration is read again for every run, so an edit (a time zone, a model, the tools) applies
/// to the next one.
async fn job_task(context: Arc<Context>, job: JobDef, stop: CancellationToken) {
    let label = job.label();
    let config = context.registry.config();
    let Some(agent) = config.agents.get(&job.agent) else {
        warn!(job = %label, agent = %job.agent, "job of an agent that isn't configured; skipped");
        return;
    };
    let mut tz = message_timezone(&context.registry.users(), job.creator.as_ref(), agent);
    let last = match context
        .store
        .job_last_run(job.agent.clone(), job.key.clone())
        .await
    {
        Ok(last) => last,
        Err(error) => {
            error!(job = %label, %error, "cannot read the job's last run; assuming none");
            None
        }
    };
    let mut next = job
        .when
        .first_fire(&tz, last.max(job.since), context.clock.now());
    loop {
        let Some(at) = next else {
            debug!(job = %label, "the job has no future run");
            break;
        };
        if !sleep_until(&*context.clock, at, &stop).await {
            return;
        }
        // Before the agent is read, so that removing the agent (which takes it out of the
        // configuration and then looks here) sees every run that read it.
        let _running = context.running.begin(&job.agent, job.creator.as_ref());
        let config = context.registry.config();
        let Some(agent) = config.agents.get(&job.agent) else {
            info!(job = %label, agent = %job.agent, "the job's agent was removed; the job stops");
            return;
        };
        tz = message_timezone(&context.registry.users(), job.creator.as_ref(), agent);
        let now = context.clock.now();
        // A job of a user who was disabled (or removed) doesn't run: checked once the run is
        // counted, so removing the user, which disables them and then looks for runs, sees it.
        let creator_active = job.creator.as_ref().is_none_or(|creator| {
            context
                .registry
                .users()
                .get()
                .user(creator)
                .is_some_and(crate::users::User::is_active)
        });
        if !creator_active {
            debug!(job = %label, "the job's creator isn't active; skipped");
        } else if in_active_hours(job.active_hours.as_ref(), &tz, now) {
            if !run::run(&context, &job, agent, &stop).await {
                return;
            }
        } else {
            debug!(job = %label, "outside the job's active hours; skipped");
        }
        if job.is_one_shot() {
            break;
        }
        next = job.when.next_after(&tz, context.clock.now());
    }
    if let JobKey::Runtime(id) = job.key
        && job.is_one_shot()
    {
        match context.store.job_delete(id).await {
            Ok(_) => info!(job = %label, "one-shot job done and deleted"),
            Err(error) => error!(job = %label, %error, "cannot delete a finished one-shot job"),
        }
    }
}

/// Sleeps until the clock reads `at`, waking at least every [`MAX_SLEEP`] to read it again.
/// Returns `false` if `stop` was cancelled first.
async fn sleep_until(clock: &dyn Clock, at: Timestamp, stop: &CancellationToken) -> bool {
    loop {
        let now = clock.now();
        if now >= at {
            return true;
        }
        let left = Duration::try_from(at.duration_since(now)).unwrap_or(Duration::ZERO);
        tokio::select! {
            biased;
            () = stop.cancelled() => return false,
            () = tokio::time::sleep(left.min(MAX_SLEEP)) => {}
        }
    }
}
