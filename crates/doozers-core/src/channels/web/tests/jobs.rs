//! Router-level tests of the scheduled jobs page.

use crate::scheduler::RuntimeSpec;
use crate::sessions::Clock;

use super::agents::guest;
use super::*;

const NOW: &str = "2026-10-22T12:00:00Z";

struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        NOW.parse().unwrap()
    }
}

/// Sends a request to `env`'s pages as `user`, with the clock at [`NOW`]; `form` is posted with
/// the CSRF token in front.
async fn send(
    env: &Env,
    user: WebUser,
    method: Method,
    path: &str,
    form: Option<&str>,
) -> Response {
    let app = WebApp::with_clock(
        env.channel.clone(),
        env.store.clone(),
        env.blobs.clone(),
        env.registry.live_config().clone(),
        env.jobs.clone(),
        Arc::new(FixedClock),
        env.shutdown.clone(),
    );
    let router = app
        .install(RouterBuilder::new())
        .build()
        .layer(Extension(user));
    let form = form.map(|form| format!("csrf={CSRF}&{form}"));
    call(router, method, path, &[], form.as_deref()).await
}

async fn jobs_page(env: &Env, user: WebUser) -> (StatusCode, String) {
    let response = send(env, user, Method::GET, "/jobs", None).await;
    (response.status(), body_text(response).await)
}

/// Posts `form` to `path` as `user`.
async fn post(env: &Env, user: WebUser, path: &str, form: &str) -> Response {
    send(env, user, Method::POST, path, Some(form)).await
}

/// The stored spec of job `id`; `None` once it's deleted.
async fn spec(env: &Env, id: doozers_store::RuntimeJobId) -> Option<serde_json::Value> {
    env.store.job(id).await.unwrap().map(|job| job.spec)
}

fn assert_to_list(response: &Response) {
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/jobs");
}

/// Stores a runtime job of `agent` created by `creator`: `cron` or a one-shot `at`.
pub(super) async fn create(
    env: &Env,
    agent: &str,
    creator: &str,
    schedule: &str,
    prompt: &str,
) -> doozers_store::RuntimeJobId {
    let (cron, at) = match schedule.parse::<Timestamp>() {
        Ok(at) => (None, Some(at)),
        Err(_) => (Some(schedule.to_owned()), None),
    };
    let spec = RuntimeSpec {
        cron,
        at,
        prompt: prompt.to_owned(),
        session: crate::config::JobSession::Isolated,
        conversation: None,
        disabled: false,
        changed_at: None,
    };
    env.store
        .job_create(
            AgentId::new(agent).unwrap(),
            UserId::new(creator).unwrap(),
            serde_json::to_value(spec).unwrap(),
            NOW.parse().unwrap(),
        )
        .await
        .unwrap()
        .id
}

/// Where `needle` is in `page`; panics if it isn't there.
fn position(page: &str, needle: &str) -> usize {
    page.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not in the page:\n{page}"))
}

#[tokio::test]
async fn the_owner_sees_every_agents_jobs_in_one_list_soonest_first() {
    let env = env().await;
    create(&env, "jeeves", "owner", "0 0 1 1 *", "Yearly review").await;
    create(&env, "bunter", "owner", "2026-10-22T12:00:01Z", "Stretch").await;
    create(&env, "bunter", "guest", "0 9 * * *", "Guest reminder").await;
    create(&env, "gone", "owner", "0 9 * * *", "Orphaned").await;
    env.store
        .job_set_last_run(
            AgentId::new("jeeves").unwrap(),
            doozers_store::JobKey::Config("morning".into()),
            "2026-10-22T08:00:00Z".parse().unwrap(),
        )
        .await
        .unwrap();

    let (status, page) = jobs_page(&env, owner()).await;

    assert_eq!(status, StatusCode::OK);
    // One table, with the agent of each job, soonest next run first: the one-shot in a second,
    // the config job within a day, the yearly job, then the job that doesn't run.
    let stretch = position(&page, "Stretch");
    let morning = position(&page, "<code>morning</code>");
    let yearly = position(&page, "Yearly review");
    let orphaned = position(&page, "Orphaned");
    assert!(stretch < morning && morning < yearly && yearly < orphaned);
    assert!(page[..stretch].contains(r#"<td data-label="Agent" class="m-meta">bunter</td>"#));
    assert!(
        page[stretch..morning]
            .contains(r#"<td data-label="Agent" class="m-meta">Jeeves (jeeves)</td>"#)
    );
    assert!(page.contains("From <code>morning.md</code>"));
    assert!(page.contains(r#"<time datetime="2026-10-22T08:00:00Z">"#));
    let orphaned_row = &page[page[..orphaned].rfind("<tr>").unwrap()..orphaned];
    assert!(orphaned_row.contains(r#"<span class="warn-text">"#));
    assert!(!page.contains("Guest reminder"));
}

#[tokio::test]
async fn other_users_see_only_the_jobs_they_scheduled() {
    let env = env().await;
    create(&env, "bunter", "guest", "0 9 * * *", "Guest reminder").await;
    create(&env, "jeeves", "owner", "0 9 * * *", "Owner reminder").await;

    let (status, page) = jobs_page(&env, guest()).await;

    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Guest reminder"));
    assert!(!page.contains("Owner reminder"));
    // Config jobs deliver to the owner.
    assert!(!page.contains("morning"));
}

#[tokio::test]
async fn the_form_schedules_a_job_for_the_user_and_tells_the_scheduler() {
    let env = env().await;
    let seen = env.jobs.subscribe();
    let (_, page) = jobs_page(&env, owner()).await;
    assert!(page.contains(r#"href="/jobs/new""#));
    let form = body_text(send(&env, owner(), Method::GET, "/jobs/new", None).await).await;
    assert!(form.contains(r#"<option value="bunter">bunter</option>"#));

    let response = post(
        &env,
        owner(),
        "/jobs",
        "agent=bunter&repeat=cron&cron=0+9+*+*+1-5&at=&prompt=Stand-up%3F%0D%0ANotes.&session=dm",
    )
    .await;
    assert_to_list(&response);
    let once = post(
        &env,
        owner(),
        "/jobs",
        "agent=jeeves&repeat=once&cron=ignored&at=2026-10-22T18%3A30%3A00Z&prompt=Tea.&session=isolated",
    )
    .await;
    assert_to_list(&once);

    assert!(seen.has_changed().unwrap(), "the scheduler wasn't told");
    let jobs = env.store.jobs(None, None).await.unwrap();
    assert_eq!(jobs.len(), 2);
    assert_eq!(jobs[0].agent, AgentId::new("bunter").unwrap());
    assert_eq!(jobs[0].creator, identity::owner());
    assert_eq!(
        jobs[0].spec,
        serde_json::json!({"cron": "0 9 * * 1-5", "prompt": "Stand-up?\nNotes.", "session": "dm"})
    );
    assert_eq!(
        jobs[1].spec,
        serde_json::json!({"at": "2026-10-22T18:30:00Z", "prompt": "Tea.", "session": "isolated"})
    );
}

#[tokio::test]
async fn the_form_refuses_a_job_that_would_not_run_and_keeps_what_was_typed() {
    let env = env().await;
    for (form, status, error) in [
        (
            "agent=bunter&repeat=cron&cron=0+9+*+*+*&prompt=+",
            StatusCode::BAD_REQUEST,
            "Write a prompt",
        ),
        (
            "agent=bunter&repeat=cron&cron=0+9+*&prompt=Kept",
            StatusCode::BAD_REQUEST,
            "invalid cron pattern",
        ),
        (
            "agent=bunter&repeat=once&at=2026-10-22T11%3A00%3A00Z&prompt=Kept",
            StatusCode::BAD_REQUEST,
            "That time has passed",
        ),
        (
            "agent=nobody&repeat=cron&cron=0+9+*+*+*&prompt=Kept",
            StatusCode::BAD_REQUEST,
            "Pick an agent",
        ),
    ] {
        let response = post(&env, owner(), "/jobs", &format!("{form}&session=isolated")).await;
        assert_eq!(response.status(), status, "{form}");
        let page = body_text(response).await;
        assert!(page.contains(error), "{form}: {page}");
        if form.ends_with("Kept") {
            assert!(page.contains(">Kept</textarea>"), "{form}");
        }
    }
    // Without the token nothing is posted.
    let app = env.router(Some(owner()));
    let response = call(
        app,
        Method::POST,
        "/jobs",
        &[],
        Some("agent=bunter&repeat=cron&cron=0+9+*+*+*&prompt=p&session=dm"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A member may schedule with any agent, like an admin: the job is theirs.
    let response = post(
        &env,
        guest(),
        "/jobs",
        "agent=bunter&repeat=cron&cron=0+9+*+*+*&prompt=p&session=dm",
    )
    .await;
    assert_to_list(&response);
    let stored = env.store.jobs(None, None).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].creator, guest().user);
    env.store.job_delete(stored[0].id).await.unwrap();
    assert!(env.store.jobs(None, None).await.unwrap().is_empty());

    // At most MAX_RUNTIME_JOBS_PER_USER with one agent.
    for _ in 0..crate::core_tools::MAX_RUNTIME_JOBS_PER_USER {
        create(&env, "bunter", "owner", "0 9 * * *", "p").await;
    }
    let full = post(
        &env,
        owner(),
        "/jobs",
        "agent=bunter&repeat=cron&cron=0+9+*+*+*&prompt=p&session=dm",
    )
    .await;
    assert_eq!(full.status(), StatusCode::CONFLICT);
    assert!(body_text(full).await.contains("Delete one first"));
}

#[tokio::test]
async fn edit_disable_enable_and_delete_change_only_the_users_own_jobs() {
    let env = env().await;
    let mine = create(
        &env,
        "bunter",
        "owner",
        "2026-10-23T09:00:00Z",
        "Call the dentist.",
    )
    .await;
    let theirs = create(&env, "bunter", "guest", "0 9 * * *", "Theirs.").await;
    let path = |id: doozers_store::RuntimeJobId, action: &str| format!("/jobs/{id}/{action}");

    // The edit form is filled in from the job, its time in the job's zone.
    let form = body_text(send(&env, owner(), Method::GET, &path(mine, "edit"), None).await).await;
    let local = "2026-10-23T09:00:00Z"
        .parse::<Timestamp>()
        .unwrap()
        .to_zoned(jiff::tz::TimeZone::system())
        .strftime("%Y-%m-%dT%H:%M")
        .to_string();
    assert!(form.contains(&format!(r#"value="{local}""#)), "{form}");
    assert!(form.contains(">Call the dentist.</textarea>"));

    let mut seen = env.jobs.subscribe();
    let edited = post(
        &env,
        owner(),
        &path(mine, "edit"),
        "repeat=cron&cron=0+8+*+*+*&prompt=Every+morning.&session=dm",
    )
    .await;
    assert_to_list(&edited);
    assert!(seen.has_changed().unwrap());
    seen.borrow_and_update();
    assert_eq!(
        spec(&env, mine).await.unwrap(),
        serde_json::json!({"cron": "0 8 * * *", "prompt": "Every morning.", "session": "dm",
                           "changed_at": NOW})
    );

    assert_to_list(&post(&env, owner(), &path(mine, "disable"), "").await);
    assert_eq!(spec(&env, mine).await.unwrap()["disabled"], true);
    assert!(seen.has_changed().unwrap());
    let (_, page) = jobs_page(&env, owner()).await;
    assert!(page.contains(r#"<span class="warn-text">Disabled</span>"#));
    assert!(page.contains(&format!(r#"action="/jobs/{mine}/enable""#)));
    // An edit keeps it disabled.
    let edited = post(
        &env,
        owner(),
        &path(mine, "edit"),
        "repeat=cron&cron=0+7+*+*+*&prompt=Earlier.&session=dm",
    )
    .await;
    assert_to_list(&edited);
    assert_eq!(spec(&env, mine).await.unwrap()["disabled"], true);

    assert_to_list(&post(&env, owner(), &path(mine, "enable"), "").await);
    let enabled = spec(&env, mine).await.unwrap();
    assert_eq!(enabled.get("disabled"), None);
    assert_eq!(enabled["changed_at"], NOW);

    // Someone else's job is not there to change.
    for action in ["edit", "disable", "enable", "delete"] {
        let response = post(
            &env,
            owner(),
            &path(theirs, action),
            "repeat=cron&cron=0+1+*+*+*&prompt=Mine+now.&session=dm",
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{action}");
    }
    let response = send(&env, owner(), Method::GET, &path(theirs, "edit"), None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(spec(&env, theirs).await.unwrap()["prompt"], "Theirs.");

    seen.borrow_and_update();
    assert_to_list(&post(&env, owner(), &path(mine, "delete"), "").await);
    assert_eq!(spec(&env, mine).await, None);
    assert!(seen.has_changed().unwrap());
}

#[tokio::test]
async fn an_edit_keeps_the_conversation_the_job_was_scheduled_in() {
    let env = env().await;
    let job = create(&env, "bunter", "owner", "0 9 * * *", "Stand-up?").await;
    let mut value = spec(&env, job).await.unwrap();
    value["conversation"] = serde_json::json!({"channel": "discord", "id": "1234"});
    env.store.job_update_spec(job, value).await.unwrap();

    let edited = post(
        &env,
        owner(),
        &format!("/jobs/{job}/edit"),
        "repeat=cron&cron=0+8+*+*+*&prompt=Earlier.&session=isolated",
    )
    .await;
    assert_to_list(&edited);
    let value = spec(&env, job).await.unwrap();
    assert_eq!(value["prompt"], "Earlier.");
    assert_eq!(
        value["conversation"],
        serde_json::json!({"channel": "discord", "id": "1234"})
    );
}

#[tokio::test]
async fn a_one_shot_job_whose_time_passed_while_disabled_must_be_edited_first() {
    let env = env().await;
    let job = create(&env, "bunter", "owner", "2026-10-22T11:00:00Z", "Too late.").await;
    let mut value = spec(&env, job).await.unwrap();
    value["disabled"] = true.into();
    env.store.job_update_spec(job, value).await.unwrap();

    let response = post(&env, owner(), &format!("/jobs/{job}/enable"), "").await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        body_text(response)
            .await
            .contains("Edit it to pick a new time")
    );
    assert_eq!(spec(&env, job).await.unwrap()["disabled"], true);
}
