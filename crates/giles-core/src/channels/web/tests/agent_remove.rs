//! Router-level tests of removing an agent over the test configuration's registry:
//! the agents `jeeves` (a name, a tool, and a job with a prompt file) and `bunter`.

use async_trait::async_trait;
use serde_json::json;

use crate::agents::{AgentEdit, AgentManager, CreateError, EditError, NewAgent, RemoveError};
use crate::channels::chat_ids::new_agent_chat_id;
use crate::config::{Config, LoadOptions};
use crate::routes::RouteSpec;
use crate::sessions::AgentPolicies;

use super::agents::guest;
use super::*;

impl Env {
    fn trash_agents(&self) -> std::path::PathBuf {
        self.root().trash_dir().join("agents")
    }

    /// The directories in `trash/agents/`, sorted; none if there is no trash.
    fn trashed(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.trash_agents())
            .map(|dir| {
                dir.map(|e| e.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// `POST /admin/agents/<agent>/remove` as the owner.
    async fn remove_post(&self, manager: &Arc<AgentManager>, agent: &str) -> (StatusCode, String) {
        let (status, html, _) = self
            .agents_call(
                manager,
                Method::POST,
                &format!("/admin/agents/{agent}/remove"),
                Some(&format!("csrf={CSRF}")),
            )
            .await;
        (status, html)
    }

    async fn route_to(&self, conversation: &str, agent: &str) -> giles_store::RouteId {
        self.shared_routes()
            .await
            .add(
                &RouteSpec {
                    channel: "web".into(),
                    kind: String::new(),
                    conversation: conversation.into(),
                    agent: agent.into(),
                },
                None,
            )
            .await
            .unwrap()
    }

    async fn runtime_job(&self, agent: &str) -> giles_store::RuntimeJob {
        self.store
            .job_create(
                AgentId::new(agent).unwrap(),
                identity::owner(),
                json!({"cron": "0 9 * * *", "prompt": "Tea."}),
                Timestamp::now(),
            )
            .await
            .unwrap()
    }
}

fn jeeves() -> AgentId {
    AgentId::new("jeeves").unwrap()
}

#[tokio::test]
async fn only_the_owner_gets_the_removal_pages_and_a_post_needs_the_csrf_token() {
    let env = env().await;
    let manager = env.manager().await;
    for method in [Method::GET, Method::POST] {
        for (user, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(guest()), StatusCode::FORBIDDEN),
        ] {
            let response = call(
                env.agents_router(&manager, user),
                method.clone(),
                "/admin/agents/jeeves/remove",
                &[("x-csrf-token", CSRF)],
                Some(""),
            )
            .await;
            assert_eq!(response.status(), expected, "{method}");
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
            assert!(!body_text(response).await.contains("Jeeves"));
        }
    }
    for (headers, form) in [
        (&[][..], ""),
        (&[][..], "csrf=wrong"),
        (&[("x-csrf-token", "wrong")][..], ""),
    ] {
        let response = call(
            env.agents_router(&manager, Some(owner())),
            Method::POST,
            "/admin/agents/jeeves/remove",
            headers,
            Some(form),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{form}");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    }
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
    assert!(env.registry.config().agents.contains_key(&jeeves()));
    assert!(env.trashed().is_empty());
    assert!(!env.root().trash_dir().exists());
}

#[tokio::test]
async fn the_confirmation_page_says_exactly_what_will_happen() {
    let env = env().await;
    let manager = env.manager().await;
    env.runtime_job("jeeves").await;
    env.runtime_job("jeeves").await;
    env.runtime_job("bunter").await;

    let (status, html, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents/jeeves/remove", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<h1>Remove Jeeves (jeeves)?</h1>",
        "<code>agents/jeeves/</code>",
        "<code>trash/agents/jeeves-&lt;date and time, UTC&gt;/</code>",
        "Nothing is deleted.",
        "move that directory back to <code>agents/jeeves/</code> and restart Giles",
        "Stops the 1 job in its <code>agent.toml</code>",
        "Deletes the 2 jobs the agent scheduled itself",
        "That can't be undone.",
        "If you later create an agent with the same id, those chats belong to it again.",
        "not part of <code>giles backup</code>",
        r#"action="/admin/agents/jeeves/remove""#,
        &format!(r#"name="csrf" value="{CSRF}""#),
        "<button type=\"submit\">Remove agent</button>",
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }

    // An agent with nothing scheduled says so, and the page changes nothing.
    let (_, html, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents/bunter/remove", None)
        .await;
    assert!(
        html.contains("It has no jobs in <code>agent.toml</code>."),
        "{html}"
    );
    assert!(
        html.contains("Deletes the 1 job the agent scheduled itself"),
        "{html}"
    );
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
    assert!(!env.root().trash_dir().exists());

    // Unknown agents, and ids that aren't plain names, have no page.
    for path in ["nope", "..", "%2E%2E", "..%2Fjeeves"] {
        let (status, _, _) = env
            .agents_call(
                &manager,
                Method::GET,
                &format!("/admin/agents/{path}/remove"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        let (status, _) = env.remove_post(&manager, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    // The list and the agent's page lead to it.
    let (_, list, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents", None)
        .await;
    assert!(
        list.contains(r#"<a href="/admin/agents/jeeves/remove">Remove</a>"#),
        "{list}"
    );
    let (_, page, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents/jeeves", None)
        .await;
    assert!(
        page.contains(r#"<a class="button" href="/admin/agents/jeeves/remove">Remove</a>"#),
        "{page}"
    );
}

#[tokio::test]
async fn removing_moves_the_files_aside_and_takes_the_agent_out_of_service() {
    let env = env().await;
    let manager = env.manager().await;
    let dir = env.root().agents_dir().join("jeeves");
    std::fs::write(dir.join("SOUL.md"), "You are Jeeves.").unwrap();
    std::fs::create_dir_all(dir.join("users/owner")).unwrap();
    std::fs::write(dir.join("users/owner/MEMORY.md"), "Likes tea.").unwrap();
    // A chat started with the agent before, stored in the database.
    let chat = new_agent_chat_id(&jeeves());
    let session = env
        .store
        .open_session(
            SessionKey {
                agent: jeeves(),
                channel: INSTANCE.into(),
                conversation: chat.clone(),
            },
            SessionKind::Direct,
            Some(identity::owner()),
            Timestamp::now(),
        )
        .await
        .unwrap()
        .0;
    let chats_before = body_text(env.get("/").await).await;
    assert!(chats_before.contains(r#"<option value="jeeves">Jeeves (jeeves)</option>"#));

    let (status, html) = env.remove_post(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::OK, "{html}");

    // The files are moved, not copied or deleted.
    assert_eq!(env.agent_dirs(), ["bunter"]);
    let trashed = env.trashed();
    assert_eq!(trashed.len(), 1, "{trashed:?}");
    let name = &trashed[0];
    let stamp = name.strip_prefix("jeeves-").expect(name);
    assert!(
        stamp.len() == 16 && stamp.ends_with('Z') && stamp.as_bytes()[8] == b'T',
        "a UTC time like 20261024T120000Z: {stamp}"
    );
    let moved = env.trash_agents().join(name);
    assert_eq!(
        std::fs::read_to_string(moved.join("SOUL.md")).unwrap(),
        "You are Jeeves."
    );
    assert_eq!(
        std::fs::read_to_string(moved.join("users/owner/MEMORY.md")).unwrap(),
        "Likes tea."
    );
    assert!(
        std::fs::read_to_string(moved.join("agent.toml"))
            .unwrap()
            .contains("[[jobs]]")
    );
    assert_eq!(
        std::fs::read_to_string(moved.join("prompts/morning.md")).unwrap(),
        "Plan my day."
    );
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(env.root().trash_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }
    // The page says where.
    assert!(html.contains(&moved.display().to_string()), "{html}");
    assert!(html.contains("Removed Jeeves (jeeves)"), "{html}");

    // Out of the running configuration, and so out of every list and chooser and the router.
    assert!(!env.registry.config().agents.contains_key(&jeeves()));
    assert!(env.registry.live_config().policy(&jeeves()).is_none());
    assert!(
        env.registry
            .live_config()
            .policy(&AgentId::new("bunter").unwrap())
            .is_some()
    );
    let (_, list, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents", None)
        .await;
    assert!(!list.contains("jeeves"), "{list}");
    assert!(list.contains("bunter"));
    let chats = body_text(env.get("/").await).await;
    assert!(!chats.contains(r#"<option value="jeeves">"#), "{chats}");
    assert!(chats.contains(r#"<option value="bunter">"#), "{chats}");
    assert_eq!(
        env.get("/chats/new?agent=jeeves").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        env.get(&format!("/chats/{chat}")).await.status(),
        StatusCode::NOT_FOUND
    );
    for path in ["", "/edit", "/remove"] {
        let (status, _, _) = env
            .agents_call(
                &manager,
                Method::GET,
                &format!("/admin/agents/jeeves{path}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    // Removing it again is a 404, not another move.
    let (status, _) = env.remove_post(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(env.trashed().len(), 1);

    // The history is kept, inert.
    assert!(env.store.session(session.id).await.unwrap().is_some());
    // A restart loads what's left: bunter alone.
    let reloaded = Config::load_with(
        env.root(),
        &LoadOptions {
            env: &|_| None,
            giles_exe: None,
            ..LoadOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        reloaded
            .agents
            .keys()
            .map(AgentId::as_str)
            .collect::<Vec<_>>(),
        ["bunter"]
    );
}

#[tokio::test]
async fn routes_that_name_the_agent_keep_it_and_say_which() {
    let env = env().await;
    let manager = env.manager().await;
    let first = env.route_to("#giles", "jeeves").await;
    let other = env.route_to("#other", "bunter").await;
    let second = env.route_to("#tea", "jeeves").await;

    let (status, html) = env.remove_post(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    assert!(
        html.contains("2 routes send conversations to this agent"),
        "{html}"
    );
    for (id, position, criteria) in [(first, 1, "#giles"), (second, 3, "#tea")] {
        assert!(
            html.contains(&format!(
                r#"<a href="/admin/routes#route-{id}">Route {position}</a>"#
            )),
            "{html}"
        );
        assert!(html.contains(criteria), "{criteria}\n{html}");
    }
    assert!(
        !html.contains(&format!("route-{other}")),
        "bunter's route isn't listed: {html}"
    );
    assert!(
        html.contains(r#"<a href="/admin/routes">routes page</a>"#),
        "{html}"
    );
    assert!(
        !html.contains("Remove agent</button>"),
        "no button while it can't work: {html}"
    );
    // Nothing changed.
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
    assert!(env.registry.config().agents.contains_key(&jeeves()));
    assert!(!env.root().trash_dir().exists());

    // The GET page says the same and offers no button.
    let (_, page, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents/jeeves/remove", None)
        .await;
    assert!(page.contains("Route 1"), "{page}");
    assert!(!page.contains("Remove agent</button>"), "{page}");

    // The error type carries them for other callers.
    let Err(RemoveError::Routed(routes)) = manager.remove("jeeves").await else {
        panic!("routed");
    };
    assert_eq!(
        routes.iter().map(|r| r.position).collect::<Vec<_>>(),
        [1, 3]
    );

    // Once the owner has dealt with them, it goes.
    let routes = env.shared_routes().await;
    routes.remove(first).await.unwrap();
    routes.remove(second).await.unwrap();
    let (status, _) = env.remove_post(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env.agent_dirs(), ["bunter"]);
    // And no route to a removed agent can be made.
    let err = routes
        .add(
            &RouteSpec {
                channel: "web".into(),
                kind: String::new(),
                conversation: "#new".into(),
                agent: "jeeves".into(),
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no agent `jeeves`"), "{err}");
}

#[tokio::test]
async fn an_agent_that_is_working_is_not_removed() {
    let env = env().await;
    let manager = env.manager().await;
    env.activity.busy.lock().unwrap().insert(jeeves());

    let (status, html) = env.remove_post(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    assert!(
        html.contains("working on a chat or a scheduled job right now"),
        "{html}"
    );
    assert!(!html.contains("Remove agent</button>"), "{html}");
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
    assert!(env.registry.config().agents.contains_key(&jeeves()));

    // Another agent's work doesn't matter, and this one goes once it's idle.
    let (status, _) = env.remove_post(&manager, "bunter").await;
    assert_eq!(status, StatusCode::OK);
    env.activity.busy.lock().unwrap().clear();
    let (status, _) = env.remove_post(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env.trashed().len(), 2);
}

#[tokio::test]
async fn runtime_jobs_of_the_agent_are_deleted_and_the_scheduler_is_told() {
    let env = env().await;
    let manager = env.manager().await;
    let gone = env.runtime_job("jeeves").await;
    let kept = env.runtime_job("bunter").await;
    for (agent, key) in [
        ("jeeves", giles_store::JobKey::Config("morning".into())),
        ("jeeves", giles_store::JobKey::Runtime(gone.id)),
        ("bunter", giles_store::JobKey::Runtime(kept.id)),
    ] {
        env.store
            .job_set_last_run(AgentId::new(agent).unwrap(), key, Timestamp::now())
            .await
            .unwrap();
    }
    let mut changes = env.jobs.subscribe();
    changes.borrow_and_update();

    let (status, html) = env.remove_post(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("The 1 job in its <code>agent.toml</code> stopped."),
        "{html}"
    );
    assert!(
        html.contains("The 1 job it scheduled itself was deleted."),
        "{html}"
    );
    assert_eq!(env.store.job(gone.id).await.unwrap(), None);
    assert_eq!(env.store.job(kept.id).await.unwrap(), Some(kept.clone()));
    assert!(
        env.store
            .job_last_run(jeeves(), giles_store::JobKey::Config("morning".into()))
            .await
            .unwrap()
            .is_none()
    );
    assert!(changes.has_changed().unwrap(), "the scheduler is told");
}

#[tokio::test]
async fn the_id_is_free_again_and_the_old_files_stay_in_the_trash() {
    let env = env().await;
    let manager = env.manager().await;
    let old_chat = env
        .store
        .open_session(
            SessionKey {
                agent: jeeves(),
                channel: INSTANCE.into(),
                conversation: new_agent_chat_id(&jeeves()),
            },
            SessionKind::Direct,
            Some(identity::owner()),
            Timestamp::now(),
        )
        .await
        .unwrap()
        .0;
    assert_eq!(env.remove_post(&manager, "jeeves").await.0, StatusCode::OK);

    let (status, html, _) = env
        .agents_call(
            &manager,
            Method::POST,
            "/admin/agents",
            Some(&format!("csrf={CSRF}&id=jeeves&models=fake%2Fm&soul=Again")),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{html}");
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
    assert!(env.registry.config().agents.contains_key(&jeeves()));
    assert_eq!(
        std::fs::read_to_string(env.root().agents_dir().join("jeeves/SOUL.md")).unwrap(),
        "Again"
    );
    // The old chat of that id belongs to the new agent, which has a new incarnation.
    assert!(env.store.session(old_chat.id).await.unwrap().is_some());
    let chats = body_text(env.get("/").await).await;
    assert!(
        chats.contains(r#"<option value="jeeves">jeeves</option>"#),
        "{chats}"
    );

    // Removing the new one in the same second doesn't collide with the first.
    assert_eq!(env.remove_post(&manager, "jeeves").await.0, StatusCode::OK);
    let trashed = env.trashed();
    assert_eq!(trashed.len(), 2, "{trashed:?}");
    let contents: Vec<String> = trashed
        .iter()
        .map(|n| {
            std::fs::read_to_string(env.trash_agents().join(n).join("SOUL.md")).unwrap_or_default()
        })
        .collect();
    assert!(contents.contains(&"Again".to_owned()), "{contents:?}");
    assert!(contents.contains(&String::new()), "{contents:?}");
}

#[tokio::test]
async fn removal_creation_and_edits_run_one_at_a_time() {
    let env = env().await;
    let manager = env.manager().await;
    let form = manager.edit_form("jeeves").await.unwrap();
    let edit = AgentEdit {
        id: "jeeves".into(),
        name: "Edited".into(),
        models: form.models.clone(),
        soul: "edited soul".into(),
        version: form.version.clone(),
        ..AgentEdit::default()
    };
    // An edit and a removal at once: the edit is saved and then moved with the agent, or finds
    // the agent gone. Never half of each.
    let (edited, removed) = tokio::join!(manager.edit(&edit), manager.remove("jeeves"));
    removed.unwrap();
    assert_eq!(env.agent_dirs(), ["bunter"]);
    assert!(!env.registry.config().agents.contains_key(&jeeves()));
    let moved = env.trash_agents().join(&env.trashed()[0]);
    match edited {
        Ok(_) => {
            assert!(
                std::fs::read_to_string(moved.join("agent.toml"))
                    .unwrap()
                    .contains("Edited")
            );
            assert_eq!(
                std::fs::read_to_string(moved.join("SOUL.md")).unwrap(),
                "edited soul"
            );
        }
        Err(EditError::NotFound(_)) => {
            assert!(
                !std::fs::read_to_string(moved.join("agent.toml"))
                    .unwrap()
                    .contains("Edited")
            );
        }
        Err(other) => panic!("{other}"),
    }

    // A creation of the same id and a removal at once: afterwards the configuration and the
    // directory agree.
    let again = NewAgent {
        id: "bunter".into(),
        models: vec!["fake/m".into()],
        soul: "new bunter".into(),
        ..NewAgent::default()
    };
    let (created, removed) = tokio::join!(manager.create(&again), manager.remove("bunter"));
    removed.unwrap();
    let exists = env
        .registry
        .config()
        .agents
        .contains_key(&AgentId::new("bunter").unwrap());
    match created {
        // Removal came first, then the creation made it again.
        Ok(_) => {
            assert!(exists);
            assert_eq!(
                std::fs::read_to_string(env.root().agents_dir().join("bunter/SOUL.md")).unwrap(),
                "new bunter"
            );
        }
        // The creation found the old one still there: it was refused, and removal then ran.
        Err(CreateError::Exists(_)) => {
            assert!(!exists);
            assert!(!env.root().agents_dir().join("bunter").exists());
        }
        Err(other) => panic!("{other}"),
    }
}

#[tokio::test]
async fn the_chat_list_still_renders_with_a_chat_of_a_removed_agent() {
    let env = env().await;
    let manager = env.manager().await;
    let chat = new_agent_chat_id(&jeeves());
    store_chat(
        &env,
        &chat,
        INSTANCE,
        vec![stored(Role::User, vec![text("tea please")])],
    )
    .await;
    let html = body_text(env.get("/").await).await;
    assert!(html.contains("tea please"), "{html}");
    assert!(html.contains("Jeeves (jeeves)"), "{html}");

    assert_eq!(env.remove_post(&manager, "jeeves").await.0, StatusCode::OK);
    let response = env.get("/").await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    // The chat is still listed, now by the id alone; the agent is gone from the chooser.
    assert!(html.contains("tea please"), "{html}");
    assert!(!html.contains("Jeeves"), "{html}");
    assert!(!html.contains(r#"<option value="jeeves">"#), "{html}");
}

/// Says "idle" the first time it is asked and "busy" after, and notes whether the agent was in
/// the running configuration each time: what a turn that begins after the first look does.
#[derive(Debug)]
struct WorkAppearsAfterTheFirstLook {
    registry: Arc<crate::registry::Registry>,
    configured_when_asked: std::sync::Mutex<Vec<bool>>,
}

#[async_trait]
impl crate::agents::AgentActivity for WorkAppearsAfterTheFirstLook {
    async fn is_busy(&self, agent: &AgentId) -> bool {
        let mut asked = self.configured_when_asked.lock().unwrap();
        asked.push(self.registry.config().agents.contains_key(agent));
        asked.len() > 1
    }
}

impl Env {
    async fn manager_with(&self, activity: Arc<dyn crate::agents::AgentActivity>) -> AgentManager {
        AgentManager::new(crate::agents::AgentManagerOptions {
            registry: self.registry.clone(),
            store: self.store.clone(),
            routes: self.shared_routes().await,
            jobs: self.jobs.clone(),
            activity,
        })
    }
}

#[tokio::test]
async fn work_that_starts_after_the_first_look_is_seen_and_the_agent_is_put_back() {
    let env = env().await;
    let probe = Arc::new(WorkAppearsAfterTheFirstLook {
        registry: env.registry.clone(),
        configured_when_asked: Default::default(),
    });
    let manager = env.manager_with(probe.clone()).await;
    let before = env.registry.live_config().incarnation(&jeeves());

    let error = manager.remove("jeeves").await.unwrap_err();
    assert!(matches!(error, RemoveError::Busy), "{error}");
    // The second look came after the agent was taken out of the configuration, so a turn that
    // begins from then on finds no agent: work seen then is all there is.
    assert_eq!(*probe.configured_when_asked.lock().unwrap(), [true, false]);
    // Put back, files untouched, nothing in the trash, the id still in use.
    assert!(env.registry.config().agents.contains_key(&jeeves()));
    assert!(env.registry.live_config().incarnation(&jeeves()) > before);
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
    assert!(!env.root().trash_dir().exists());
    let (status, _, _) = env
        .agents_call(
            &env.manager().await,
            Method::GET,
            "/admin/agents/jeeves",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // And the routes were released: a route to the agent can be made again.
    env.route_to("#again", "jeeves").await;
}

#[tokio::test]
async fn a_move_that_fails_puts_the_agent_back() {
    let env = env().await;
    let manager = env.manager().await;
    // A file where the trash directory should be: it can't be created.
    std::fs::write(env.root().trash_dir(), "in the way").unwrap();

    let error = manager.remove("jeeves").await.unwrap_err();
    assert!(matches!(error, RemoveError::Io { .. }), "{error}");
    assert!(env.registry.config().agents.contains_key(&jeeves()));
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
    assert!(env.root().agents_dir().join("jeeves/agent.toml").is_file());
    // The page says it failed and that nothing changed (a 500, logged, no paths).
    let (status, html) = env.remove_post(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(html.contains("Nothing was changed"), "{html}");
    assert!(
        !html.contains("in the way") && !html.contains("/trash"),
        "{html}"
    );
    // Out of the way, the same removal works.
    std::fs::remove_file(env.root().trash_dir()).unwrap();
    assert_eq!(env.remove_post(&manager, "jeeves").await.0, StatusCode::OK);
}
