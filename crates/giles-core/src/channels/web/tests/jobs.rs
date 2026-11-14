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

/// `env`'s pages, with the clock at [`NOW`], as `user`.
async fn jobs_page(env: &Env, user: WebUser) -> (StatusCode, String) {
    let app = WebApp::with_clock(
        env.channel.clone(),
        env.store.clone(),
        env.blobs.clone(),
        env.registry.live_config().clone(),
        Arc::new(FixedClock),
        env.shutdown.clone(),
    );
    let router = app
        .install(RouterBuilder::new())
        .build()
        .layer(Extension(user));
    let response = call(router, Method::GET, "/jobs", &[], None).await;
    (response.status(), body_text(response).await)
}

/// Stores a runtime job of `agent` created by `creator`: `cron` or a one-shot `at`.
async fn create(env: &Env, agent: &str, creator: &str, schedule: &str, prompt: &str) {
    let (cron, at) = match schedule.parse::<Timestamp>() {
        Ok(at) => (None, Some(at)),
        Err(_) => (Some(schedule.to_owned()), None),
    };
    let spec = RuntimeSpec {
        cron,
        at,
        prompt: prompt.to_owned(),
        session: crate::config::JobSession::Isolated,
    };
    env.store
        .job_create(
            AgentId::new(agent).unwrap(),
            UserId::new(creator).unwrap(),
            serde_json::to_value(spec).unwrap(),
            NOW.parse().unwrap(),
        )
        .await
        .unwrap();
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
            giles_store::JobKey::Config("morning".into()),
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
    assert!(page[..stretch].contains("<td>bunter</td>"));
    assert!(page[stretch..morning].contains("<td>Jeeves (jeeves)</td>"));
    assert!(page.contains("From <code>morning.md</code>"));
    assert!(page.contains(r#"<time datetime="2026-10-22T08:00:00Z">"#));
    let orphaned_row = &page[page[..orphaned].rfind("<tr>").unwrap()..orphaned];
    assert!(orphaned_row.contains(r#"<span class="state-warn">"#));
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
