//! Router-level tests of the workspaces page over the test configuration (the channel `web`, the
//! agents `jeeves` and `bunter`, and the environments `box` and `far`) and an in-memory fake
//! environment connected as `box`.

use giles_store::WorkspaceOwner;
use giles_types::{EnvironmentInfo, WorkspaceDetail, WorkspaceId};
use serde_json::json;

use crate::workspaces::fake::{Fake, TestClock};
use crate::workspaces::{LeaseSlot, WorkspaceState, Workspaces};

use super::agents::guest;
use super::*;

const ENVIRONMENTS: &str = "[environment.box]\ncomponent = \"docker\"\n\
    address = \"http://127.0.0.1:9000\"\n\
    [environment.far]\ncomponent = \"docker\"\naddress = \"http://127.0.0.1:9001\"\n";

struct Wk {
    env: Env,
    workspaces: Workspaces,
    fake: Arc<Fake>,
}

impl Wk {
    /// `box` connected to a fake environment with no workspaces; `far` never connects.
    async fn new() -> Self {
        let env = env_with_toml(WebConfig::default(), &["web:owner"], ENVIRONMENTS).await;
        let clock = TestClock::new();
        let fake = Fake::new(clock.clone());
        let workspaces = Workspaces::new(env.store.clone(), env.registry.clone(), clock);
        let wk = Self {
            env,
            workspaces,
            fake,
        };
        wk.connect();
        wk
    }

    fn connect(&self) {
        self.env
            .registry
            .register_environment(
                &InstanceName::new("box").unwrap(),
                self.fake.clone(),
                EnvironmentInfo {
                    options_schema: json!({"type": "object"}),
                },
            )
            .unwrap();
    }

    fn disconnect(&self) {
        self.env
            .registry
            .unregister_environment(&InstanceName::new("box").unwrap())
            .unwrap();
    }

    fn router(&self, user: Option<WebUser>) -> axum::Router {
        let app = self
            .env
            .app
            .install_workspaces(
                self.env.app.install(RouterBuilder::new()),
                self.workspaces.clone(),
            )
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    /// `method path` as the owner, with the CSRF token in the form for a POST. Every answer is
    /// `no-store`. The status, body, and `Location`.
    async fn call(&self, method: Method, path: &str) -> (StatusCode, String, Option<String>) {
        let form = (method == Method::POST).then(|| format!("csrf={CSRF}"));
        let response = call(
            self.router(Some(owner())),
            method,
            path,
            &[],
            form.as_deref(),
        )
        .await;
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{path}");
        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .map(|l| l.to_str().unwrap().to_owned());
        (status, body_text(response).await, location)
    }

    async fn page(&self) -> String {
        let (status, body, _) = self.call(Method::GET, "/admin/workspaces").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

    /// A workspace of `jeeves` in the web chat [`chat`].
    async fn create(&self) -> WorkspaceId {
        let owner = WorkspaceOwner::Conversation {
            agent: AgentId::new("jeeves").unwrap(),
            channel: INSTANCE.into(),
            conversation: chat(),
        };
        self.workspaces
            .create(&owner, "box", json!({"image": "alpine"}))
            .await
            .unwrap()
            .id
    }

    fn owner_of_chat() -> WorkspaceOwner {
        WorkspaceOwner::Conversation {
            agent: AgentId::new("jeeves").unwrap(),
            channel: INSTANCE.into(),
            conversation: chat(),
        }
    }

    async fn state(&self, id: &WorkspaceId) -> WorkspaceState {
        self.env.store.workspace(id).await.unwrap().unwrap().state
    }
}

#[tokio::test]
async fn only_the_owner_gets_the_workspace_pages() {
    let wk = Wk::new().await;
    let id = wk.create().await;
    let cases = [
        (Method::GET, "/admin/workspaces".to_owned()),
        (Method::GET, format!("/admin/workspaces/{id}/stop")),
        (Method::POST, format!("/admin/workspaces/{id}/stop")),
        (Method::GET, format!("/admin/workspaces/{id}/delete")),
        (Method::POST, format!("/admin/workspaces/{id}/delete")),
    ];
    for (method, path) in cases {
        let form = (method == Method::POST).then(|| format!("csrf={CSRF}"));
        let response = call(
            wk.router(Some(guest())),
            method.clone(),
            &path,
            &[],
            form.as_deref(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        let response = call(wk.router(None), method.clone(), &path, &[], form.as_deref()).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
    }
    assert_eq!(wk.state(&id).await, WorkspaceState::Active);
    assert!(wk.fake.has(&id));
    assert!(!wk.fake.log().iter().any(|l| l.starts_with("stop")));
}

#[tokio::test]
async fn every_change_needs_the_csrf_token() {
    let wk = Wk::new().await;
    let id = wk.create().await;
    for action in ["stop", "delete"] {
        let path = format!("/admin/workspaces/{id}/{action}");
        for (headers, form) in [
            (vec![], String::new()),
            (vec![], "csrf=wrong".to_owned()),
            (vec![("x-csrf-token", "wrong")], format!("csrf={CSRF}")),
        ] {
            let response = call(
                wk.router(Some(owner())),
                Method::POST,
                &path,
                &headers,
                Some(&form),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path} {form}");
        }
    }
    assert_eq!(wk.state(&id).await, WorkspaceState::Active);
    assert_eq!(wk.fake.running(&id), Some(true));
}

#[tokio::test]
async fn the_page_lists_a_workspace_with_what_its_environment_says_now() {
    let wk = Wk::new().await;
    let id = wk.create().await;
    wk.fake.report_space(Some(5 << 30), Some(1536));
    wk.fake.set_details(vec![
        WorkspaceDetail::new("Image", "alpine:3.20"),
        WorkspaceDetail::new("Volume", "giles-box.1"),
    ]);

    let page = wk.page().await;
    assert!(page.contains(&format!("<code>{id}</code>")), "{page}");
    assert!(page.contains("running"), "{page}");
    assert!(page.contains("<code>box</code>"), "{page}");
    // Owner: the agent and the web chat, linked.
    assert!(
        page.contains(r#"href="/admin/agents/jeeves">Jeeves (jeeves)</a>"#),
        "{page}"
    );
    assert!(
        page.contains(&format!(r#"href="/chats/{}""#, chat())),
        "{page}"
    );
    assert!(page.contains("2026-11-10 12:00:00 UTC"), "{page}");
    assert!(page.contains("1.5 KiB"), "{page}");
    assert!(page.contains("fake box"), "{page}");
    assert!(
        page.contains("<dt>Image</dt>\n<dd>alpine:3.20</dd>"),
        "{page}"
    );
    assert!(
        page.contains("<dt>Volume</dt>\n<dd>giles-box.1</dd>"),
        "{page}"
    );
    assert!(page.contains("5 GiB free"), "{page}");
    // `far` has no workspaces and never connected.
    assert!(page.contains("<code>far</code>"), "{page}");
    assert!(page.contains("is not connected right now"), "{page}");
    // The admin nav links to the page.
    assert!(
        page.contains(r#"<a href="/admin/workspaces">Workspaces</a>"#),
        "{page}"
    );
    assert!(!page.contains(r#"state-warn">In use"#), "{page}");

    // After a stop, the page says so.
    wk.fake.report_space(None, None);
    let (status, _, location) = wk
        .call(Method::POST, &format!("/admin/workspaces/{id}/stop"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        location.unwrap(),
        format!("/admin/workspaces#workspace-{id}")
    );
    let page = wk.page().await;
    assert!(page.contains("stopped"), "{page}");
    assert!(
        page.contains("<dt>Disk used</dt>\n<dd>unknown</dd>"),
        "{page}"
    );
}

#[tokio::test]
async fn a_job_runs_workspace_is_shown_as_one() {
    let wk = Wk::new().await;
    wk.workspaces
        .create(
            &WorkspaceOwner::Job {
                session: giles_store::SessionId::new(41),
            },
            "box",
            json!({}),
        )
        .await
        .unwrap();
    let page = wk.page().await;
    assert!(page.contains("job run (session 41)"), "{page}");
}

#[tokio::test]
async fn conversations_of_other_channels_are_not_linked_to_a_chat() {
    let wk = Wk::new().await;
    wk.workspaces
        .create(
            &WorkspaceOwner::Conversation {
                agent: AgentId::new("bunter").unwrap(),
                channel: "whatsapp".into(),
                conversation: chat(),
            },
            "box",
            json!({}),
        )
        .await
        .unwrap();
    let page = wk.page().await;
    assert!(page.contains("whatsapp"), "{page}");
    assert!(!page.contains("/chats/"), "{page}");
}

#[tokio::test]
async fn an_environment_that_is_down_or_failing_shows_unknown_and_is_named() {
    let wk = Wk::new().await;
    wk.create().await;

    wk.fake.fail_lists(true);
    let page = wk.page().await;
    assert!(page.contains("<code>box</code>"), "{page}");
    assert!(page.contains("failed to list its workspaces"), "{page}");
    assert!(
        page.contains("<dt>Disk used</dt>\n<dd>unknown</dd>"),
        "{page}"
    );
    assert!(
        page.contains("<dt>Details</dt>\n<dd>\nunknown\n</dd>"),
        "{page}"
    );
    // What Giles stored still shows.
    assert!(page.contains("fake box"), "{page}");
    assert!(page.contains(">unknown</span>"), "{page}");
    wk.fake.fail_lists(false);

    wk.disconnect();
    let page = wk.page().await;
    assert!(
        page.contains("<dt>Disk used</dt>\n<dd>unknown</dd>"),
        "{page}"
    );
    assert!(page.contains("is not connected right now"), "{page}");
}

#[tokio::test]
async fn text_from_a_plugin_is_escaped() {
    let wk = Wk::new().await;
    wk.create().await;
    wk.fake.set_details(vec![WorkspaceDetail::new(
        "<b>label</b>",
        "<script>alert(1)</script>",
    )]);
    let page = wk.page().await;
    assert!(!page.contains("<script>alert(1)"), "{page}");
    assert!(!page.contains("<b>label"), "{page}");
    assert!(page.contains("alert(1)"), "{page}");
    assert!(page.contains("label"), "{page}");
}

#[tokio::test]
async fn stopping_a_workspace_nothing_is_using_needs_no_confirmation() {
    let wk = Wk::new().await;
    let id = wk.create().await;
    let page = wk.page().await;
    // The button posts straight away.
    assert!(
        page.contains(&format!(
            r#"method="post" action="/admin/workspaces/{id}/stop""#
        )),
        "{page}"
    );
    // Asking for the confirmation page of a workspace nothing is using goes back to the list.
    let (status, _, location) = wk
        .call(Method::GET, &format!("/admin/workspaces/{id}/stop"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.unwrap().starts_with("/admin/workspaces"));

    let (status, _, _) = wk
        .call(Method::POST, &format!("/admin/workspaces/{id}/stop"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(wk.fake.running(&id), Some(false));
    assert_eq!(wk.state(&id).await, WorkspaceState::Active);
}

#[tokio::test]
async fn stopping_a_workspace_a_turn_is_using_asks_first() {
    let wk = Wk::new().await;
    let id = wk.create().await;
    let slot = LeaseSlot::default();
    wk.workspaces
        .open(&Wk::owner_of_chat(), &slot)
        .await
        .unwrap();

    let page = wk.page().await;
    assert!(page.contains(r#"state-warn">In use"#), "{page}");
    assert!(page.contains("yes, a running turn is using it"), "{page}");
    // The button leads to the confirmation instead of posting.
    assert!(
        page.contains(&format!(r#"href="/admin/workspaces/{id}/stop""#)),
        "{page}"
    );
    assert!(
        !page.contains(&format!(
            r#"method="post" action="/admin/workspaces/{id}/stop""#
        )),
        "{page}"
    );

    let (status, confirm, _) = wk
        .call(Method::GET, &format!("/admin/workspaces/{id}/stop"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        confirm.contains("A running turn is using this workspace"),
        "{confirm}"
    );
    assert!(
        confirm.contains("ends the command the turn is running"),
        "{confirm}"
    );
    assert!(confirm.contains("background processes"), "{confirm}");
    assert!(
        confirm.contains(&format!(r#"name="csrf" value="{CSRF}""#)),
        "{confirm}"
    );
    assert_eq!(wk.fake.running(&id), Some(true), "asking changed nothing");

    let (status, _, _) = wk
        .call(Method::POST, &format!("/admin/workspaces/{id}/stop"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(wk.fake.running(&id), Some(false));
    assert_eq!(wk.state(&id).await, WorkspaceState::Active);
    drop(slot);
}

#[tokio::test]
async fn deleting_a_workspace_nothing_is_using_has_a_plain_confirmation() {
    let wk = Wk::new().await;
    let id = wk.create().await;

    let (status, confirm, _) = wk
        .call(Method::GET, &format!("/admin/workspaces/{id}/delete"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        confirm.contains(&format!("Delete workspace <code>{id}</code>?")),
        "{confirm}"
    );
    assert!(confirm.contains("an operator deleted it"), "{confirm}");
    assert!(!confirm.contains("running turn"), "{confirm}");
    assert!(
        confirm.contains(&format!(r#"name="csrf" value="{CSRF}""#)),
        "{confirm}"
    );
    assert!(wk.fake.has(&id), "asking deleted nothing");

    let (status, _, location) = wk
        .call(Method::POST, &format!("/admin/workspaces/{id}/delete"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.unwrap(), "/admin/workspaces");
    assert!(!wk.fake.has(&id));
    assert_eq!(wk.state(&id).await, WorkspaceState::Deleted);
    // The model is told on its next call, and the page no longer lists it.
    let last = wk
        .workspaces
        .last_deleted(&Wk::owner_of_chat())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        last.delete_reason,
        Some(giles_store::DeleteReason::Operator)
    );
    let page = wk.page().await;
    assert!(!page.contains(id.as_str()), "{page}");
    assert!(page.contains("No workspaces."), "{page}");
}

#[tokio::test]
async fn deleting_a_workspace_a_turn_is_using_warns_then_stops_and_deletes_it() {
    let wk = Wk::new().await;
    let id = wk.create().await;
    let slot = LeaseSlot::default();
    wk.workspaces
        .open(&Wk::owner_of_chat(), &slot)
        .await
        .unwrap();

    let (status, confirm, _) = wk
        .call(Method::GET, &format!("/admin/workspaces/{id}/delete"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        confirm.contains("A running turn is using this workspace. Deleting it stops the command"),
        "{confirm}"
    );
    assert!(wk.fake.has(&id));

    let (status, _, _) = wk
        .call(Method::POST, &format!("/admin/workspaces/{id}/delete"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let log = wk.fake.log();
    assert_eq!(
        log[log.len() - 2..],
        [format!("stop {id}"), format!("delete {id}")]
    );
    assert_eq!(wk.state(&id).await, WorkspaceState::Deleted);
    // The turn's next call finds no workspace.
    assert!(
        wk.workspaces
            .open(&Wk::owner_of_chat(), &slot)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn deleting_while_the_environment_is_down_still_frees_the_owner() {
    let wk = Wk::new().await;
    let id = wk.create().await;
    wk.disconnect();
    let (status, _, _) = wk
        .call(Method::POST, &format!("/admin/workspaces/{id}/delete"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(wk.state(&id).await, WorkspaceState::Deleting);
    assert!(
        wk.workspaces
            .current(&Wk::owner_of_chat())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn acting_on_a_workspace_that_is_gone_or_not_ready_says_so() {
    let wk = Wk::new().await;
    let id = wk.create().await;
    wk.workspaces.delete_by_id(&id).await.unwrap();
    for path in [
        format!("/admin/workspaces/{id}/delete"),
        format!("/admin/workspaces/{id}/stop"),
        "/admin/workspaces/ws-nope/delete".to_owned(),
        "/admin/workspaces/Not%20An%20Id/stop".to_owned(),
    ] {
        for method in [Method::GET, Method::POST] {
            let (status, body, _) = wk.call(method.clone(), &path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
            assert!(body.contains("There is no such workspace"), "{body}");
        }
    }

    // One being created can't be acted on yet.
    wk.env
        .store
        .workspace_insert(giles_store::NewWorkspace {
            id: WorkspaceId::new("ws-making").unwrap(),
            environment: "box".into(),
            owner: Wk::owner_of_chat(),
            options: json!({}),
            now: "2026-11-10T12:00:00Z".parse().unwrap(),
        })
        .await
        .unwrap();
    let (status, body, _) = wk
        .call(Method::POST, "/admin/workspaces/ws-making/delete")
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("still being created"), "{body}");
    let page = wk.page().await;
    assert!(page.contains("being created"), "{page}");
}

#[tokio::test]
async fn an_environment_that_is_down_refuses_a_stop() {
    let wk = Wk::new().await;
    let id = wk.create().await;
    wk.disconnect();
    let (status, body, _) = wk
        .call(Method::POST, &format!("/admin/workspaces/{id}/stop"))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("is not connected right now"), "{body}");
    assert_eq!(wk.state(&id).await, WorkspaceState::Active);
}
