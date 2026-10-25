//! Router-level tests of editing an agent (TASKS.md 5.4) over the test configuration's registry:
//! the agents `jeeves` (a name, a tool, and a job with a prompt file) and `bunter`.

use crate::agents::{AgentEdit, AgentManager, EditError};
use crate::config::{Config, LoadOptions};

use super::agents::guest;
use super::*;

/// What `jeeves`' `agent.toml` is made to say first: comments, a multi-line list, keys the form
/// doesn't manage, and the job whose prompt file the test configuration has.
const JEEVES: &str = r#"# Jeeves, the household agent.
name = "Jeeves"   # shown in chats
models = [
    "fake/test-model",   # first choice
]
tools = ["memory"]
max_iterations = 50
busy_mode = "queue"
memory_cap = 4000
compaction_model = "fake/summarizer"

[[jobs]]
id = "morning"   # runs daily
cron = "0 8 * * *"
prompt_file = "prompts/morning.md"
"#;

impl Env {
    fn jeeves_dir(&self) -> std::path::PathBuf {
        self.root().agents_dir().join("jeeves")
    }

    fn read_jeeves(&self, file: &str) -> String {
        std::fs::read_to_string(self.jeeves_dir().join(file)).unwrap()
    }

    /// Makes `jeeves`' files the commented ones above, and a `SOUL.md`.
    fn hand_write_jeeves(&self) {
        std::fs::write(self.jeeves_dir().join("agent.toml"), JEEVES).unwrap();
        std::fs::write(
            self.jeeves_dir().join("SOUL.md"),
            "You are Jeeves.\nBe brief.\n",
        )
        .unwrap();
    }

    /// The edit form of `agent`, as the owner gets it: the status, the page.
    async fn edit_page(&self, manager: &Arc<AgentManager>, agent: &str) -> (StatusCode, String) {
        let (status, html, _) = self
            .agents_call(
                manager,
                Method::GET,
                &format!("/admin/agents/{agent}/edit"),
                None,
            )
            .await;
        (status, html)
    }

    /// Posts `fields` to the edit form of `agent` as the owner.
    async fn edit_post(
        &self,
        manager: &Arc<AgentManager>,
        agent: &str,
        fields: &str,
    ) -> (StatusCode, String, Option<String>) {
        self.agents_call(
            manager,
            Method::POST,
            &format!("/admin/agents/{agent}/edit"),
            Some(&format!("csrf={CSRF}&{fields}")),
        )
        .await
    }
}

/// The `version` a rendered edit form carries.
fn version_of(html: &str) -> String {
    let marker = r#"name="version" value=""#;
    let start = html
        .find(marker)
        .unwrap_or_else(|| panic!("no version in {html}"))
        + marker.len();
    html[start..html[start..].find('"').unwrap() + start].to_owned()
}

/// A form body for `jeeves` as the form posts it (everything but the `csrf`).
fn jeeves_fields(version: &str, name: &str, models: &str, rest: &str) -> String {
    format!("version={version}&name={name}&models={models}{rest}")
}

#[tokio::test]
async fn only_the_owner_gets_the_edit_page_and_saves() {
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
                "/admin/agents/jeeves/edit",
                &[("x-csrf-token", CSRF)],
                Some("version=x&name=Hacked&models=fake%2Fm"),
            )
            .await;
            assert_eq!(response.status(), expected, "{method}");
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
            assert!(!body_text(response).await.contains("Jeeves"));
        }
    }
    assert!(env.read_jeeves("agent.toml").contains("name = \"Jeeves\""));
}

#[tokio::test]
async fn saving_needs_the_csrf_token() {
    let env = env().await;
    let manager = env.manager().await;
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let version = version_of(&html);
    let before = env.read_jeeves("agent.toml");
    let body = jeeves_fields(&version, "Hacked", "fake%2Fm", "");
    for (headers, form) in [
        (&[][..], body.clone()),
        (&[][..], format!("csrf=wrong&{body}")),
        (&[("x-csrf-token", "wrong")][..], body.clone()),
    ] {
        let response = call(
            env.agents_router(&manager, Some(owner())),
            Method::POST,
            "/admin/agents/jeeves/edit",
            headers,
            Some(&form),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{form}");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    }
    assert_eq!(env.read_jeeves("agent.toml"), before);
    let response = call(
        env.agents_router(&manager, Some(owner())),
        Method::POST,
        "/admin/agents/jeeves/edit",
        &[("x-csrf-token", CSRF)],
        Some(&body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn the_form_shows_the_agent_as_its_files_have_it_and_says_the_id_is_fixed() {
    let env = env().await;
    env.hand_write_jeeves();
    let manager = env.manager().await;
    let (status, html) = env.edit_page(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<h1>Edit Jeeves (jeeves)</h1>",
        r#"name="name" value="Jeeves""#,
        r#"name="models" value="fake/test-model""#,
        r#"name="tools" value="memory""#,
        r#"name="ask" value="""#,
        "You are Jeeves.\nBe brief.\n</textarea>",
        r#"action="/admin/agents/jeeves/edit""#,
        &format!(r#"name="csrf" value="{CSRF}""#),
        "so it can't be changed",
        "Everything else in <code>agent.toml</code> stays as written",
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(
        !html.contains(r#"name="id""#),
        "the id isn't a field: {html}"
    );

    // Unknown agents, and ids that aren't plain names, have no form.
    for path in ["nope", "..", "%2E%2E", "..%2Fjeeves"] {
        let (status, html) = env.edit_page(&manager, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(!html.contains("agent.toml"), "{path}");
    }
    // The list and the agent's page lead to it.
    let (_, list, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents", None)
        .await;
    assert!(
        list.contains(r#"<a href="/admin/agents/jeeves/edit">Edit</a>"#),
        "{list}"
    );
    let (_, page, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents/jeeves", None)
        .await;
    assert!(
        page.contains(r#"<a class="button" href="/admin/agents/jeeves/edit">Edit</a>"#),
        "{page}"
    );
}

#[tokio::test]
async fn an_edit_changes_only_what_the_form_manages_and_applies_to_new_chats_at_once() {
    let env = env().await;
    env.hand_write_jeeves();
    let manager = env.manager().await;
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let version = version_of(&html);

    let soul = "You are Jeeves II.\r\nBe very brief.";
    let fields = jeeves_fields(
        &version,
        "Jeeves%20II",
        "fake%2Ftest-model%2C+fake%2Fother",
        &format!(
            "&tools=memory%2C+memory_search&ask=memory_search&timezone=Europe%2FLondon&soul={}",
            soul.replace(' ', "+").replace("\r\n", "%0D%0A")
        ),
    );
    let (status, _, location) = env.edit_post(&manager, "jeeves", &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/admin/agents/jeeves"));

    // On disk: the managed keys are new, and every other line is as the owner wrote it.
    let toml = env.read_jeeves("agent.toml");
    let kept = [
        "# Jeeves, the household agent.",
        "max_iterations = 50",
        "busy_mode = \"queue\"",
        "memory_cap = 4000",
        "compaction_model = \"fake/summarizer\"",
        "[[jobs]]",
        "id = \"morning\"   # runs daily",
        "cron = \"0 8 * * *\"",
        "prompt_file = \"prompts/morning.md\"",
    ];
    for line in kept {
        assert!(toml.lines().any(|l| l == line), "{line} in\n{toml}");
    }
    assert!(
        toml.contains("name = \"Jeeves II\"   # shown in chats"),
        "{toml}"
    );
    assert!(
        toml.contains("tools = [\"memory\", \"memory_search\"]"),
        "{toml}"
    );
    assert!(toml.contains("ask = [\"memory_search\"]"), "{toml}");
    assert!(toml.contains("timezone = \"Europe/London\""), "{toml}");
    assert_eq!(
        env.read_jeeves("SOUL.md"),
        "You are Jeeves II.\nBe very brief."
    );
    let names: Vec<String> = std::fs::read_dir(env.jeeves_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        !names.iter().any(|n| n.contains("tmp")),
        "no temporary files are left: {names:?}"
    );

    // As startup would load it, with the unmanaged settings intact.
    let reloaded = Config::load_with(
        env.root(),
        &LoadOptions {
            env: &|_| None,
            giles_exe: None,
            ..LoadOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let jeeves = &reloaded.agents[&AgentId::new("jeeves").unwrap()];
    assert_eq!(jeeves.jobs.len(), 1);
    assert_eq!(jeeves.max_iterations, Some(50));
    assert_eq!(jeeves.memory_cap, 4000);
    assert_eq!(
        jeeves.compaction_model.as_ref().unwrap().to_string(),
        "fake/summarizer"
    );

    // Running, without a restart: the configuration, the lists, the chat chooser, a new chat.
    let live = env.registry.config();
    let running = &live.agents[&AgentId::new("jeeves").unwrap()];
    assert_eq!(running.name.as_deref(), Some("Jeeves II"));
    assert_eq!(running.models.len(), 2);
    assert_eq!(running.tools, ["memory", "memory_search"]);
    assert_eq!(running.jobs.len(), 1, "the job is still there");
    let (_, list, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents", None)
        .await;
    assert!(list.contains("Jeeves II (jeeves)"), "{list}");
    let chats = body_text(env.get("/").await).await;
    assert!(
        chats.contains(r#"<option value="jeeves">Jeeves II (jeeves)</option>"#),
        "{chats}"
    );
    let response = env.get("/chats/new?agent=jeeves").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn emptied_optional_fields_remove_their_keys() {
    let env = env().await;
    env.hand_write_jeeves();
    let manager = env.manager().await;
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let version = version_of(&html);
    let (status, _, _) = env
        .edit_post(
            &manager,
            "jeeves",
            &jeeves_fields(
                &version,
                "",
                "fake%2Ftest-model",
                "&tools=&ask=&timezone=&soul=",
            ),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let toml = env.read_jeeves("agent.toml");
    let parsed: toml::Table = toml::from_str(&toml).unwrap();
    for key in ["name", "tools", "ask", "timezone"] {
        assert!(!parsed.contains_key(key), "{key} in\n{toml}");
    }
    assert_eq!(parsed["models"][0].as_str(), Some("fake/test-model"));
    assert_eq!(parsed["busy_mode"].as_str(), Some("queue"));
    assert_eq!(parsed["jobs"].as_array().unwrap().len(), 1);
    assert!(
        toml.contains("# Jeeves, the household agent."),
        "the file's header stays\n{toml}"
    );
    assert_eq!(env.read_jeeves("SOUL.md"), "");
    let running = &env.registry.config().agents[&AgentId::new("jeeves").unwrap()];
    assert_eq!((running.name.as_deref(), running.tools.len()), (None, 0));
}

#[tokio::test]
async fn an_invalid_edit_is_shown_again_with_the_input_and_leaves_the_files_alone() {
    let env = env().await;
    env.hand_write_jeeves();
    let manager = env.manager().await;
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let version = version_of(&html);
    let toml = env.read_jeeves("agent.toml");
    let soul = env.read_jeeves("SOUL.md");
    let too_long = "x".repeat(crate::agents::MAX_SOUL_BYTES + 1);
    let cases = [
        ("nope%2Fm", "&soul=hello", "models[0]: no provider instance"),
        ("", "&soul=hello", "at least one model"),
        (
            "fake%2Fm",
            "&tools=bad%07glob&soul=hello",
            "tools[0]: invalid tool glob",
        ),
        (
            "fake%2Fm",
            "&timezone=Mars%2FOlympus&soul=hello",
            "timezone",
        ),
        (
            "fake%2Fm",
            &format!("&soul={too_long}"),
            "SOUL.md is too long",
        ),
    ];
    for (models, rest, message) in cases {
        let (status, html, _) = env
            .edit_post(
                &manager,
                "jeeves",
                &jeeves_fields(&version, "%22%3E%3Cscript%3E", models, rest),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{message}\n{html}");
        assert!(html.contains(message), "{message}\n{html}");
        // What was typed comes back, escaped, with the same version, and no paths.
        assert!(html.contains("&#34;&#62;&#60;script&#62;"), "{html}");
        assert!(!html.contains("\"><script>"), "{html}");
        assert_eq!(version_of(&html), version);
        assert!(!html.contains("/var/") && !html.contains("/tmp/"), "{html}");
        assert_eq!(env.read_jeeves("agent.toml"), toml, "{message}");
        assert_eq!(env.read_jeeves("SOUL.md"), soul, "{message}");
    }
    let running = &env.registry.config().agents[&AgentId::new("jeeves").unwrap()];
    assert_eq!(running.name.as_deref(), Some("Jeeves"));

    // An agent.toml that isn't TOML can't be edited with the form, and isn't touched.
    std::fs::write(env.jeeves_dir().join("agent.toml"), "models = [").unwrap();
    let (status, html) = env.edit_page(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(html.contains("valid TOML"), "{html}");
    assert_eq!(env.read_jeeves("agent.toml"), "models = [");
}

#[tokio::test]
async fn a_change_made_by_hand_after_the_form_was_opened_is_never_overwritten() {
    let env = env().await;
    env.hand_write_jeeves();
    let manager = env.manager().await;
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let version = version_of(&html);

    for (file, hand_edit) in [
        ("agent.toml", format!("{JEEVES}# added by hand\n")),
        ("SOUL.md", "Rewritten by hand.\n".to_owned()),
    ] {
        let original = env.read_jeeves(file);
        std::fs::write(env.jeeves_dir().join(file), &hand_edit).unwrap();
        let (status, html, _) = env
            .edit_post(
                &manager,
                "jeeves",
                &jeeves_fields(&version, "Late", "fake%2Ftest-model", "&soul=late"),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{file}\n{html}");
        assert!(html.contains("changed on disk"), "{html}");
        assert!(
            html.contains(r#"<a href="/admin/agents/jeeves/edit">Reload"#),
            "{html}"
        );
        // What the owner typed is still in the form.
        assert!(html.contains(r#"name="name" value="Late""#), "{html}");
        assert_eq!(env.read_jeeves(file), hand_edit, "the hand edit survives");
        assert!(!env.read_jeeves("SOUL.md").contains("late"));
        std::fs::write(env.jeeves_dir().join(file), original).unwrap();
    }

    // Put back as it was, the same form works again.
    let (status, _, _) = env
        .edit_post(
            &manager,
            "jeeves",
            &jeeves_fields(&version, "On%20time", "fake%2Ftest-model", "&soul=ok"),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    // After a reload the form carries the files' new version and saves.
    std::fs::write(env.jeeves_dir().join("SOUL.md"), "By hand again.\n").unwrap();
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    assert!(html.contains("By hand again."), "{html}");
    assert_ne!(version_of(&html), version);
    let (status, _, _) = env
        .edit_post(
            &manager,
            "jeeves",
            &jeeves_fields(
                &version_of(&html),
                "Reloaded",
                "fake%2Ftest-model",
                "&soul=fine",
            ),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(env.read_jeeves("SOUL.md"), "fine");
}

#[tokio::test]
async fn an_edit_that_changes_nothing_writes_nothing_and_a_hand_edited_file_is_adopted() {
    let env = env().await;
    env.hand_write_jeeves();
    let manager = env.manager().await;
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let version = version_of(&html);
    let modified = |file: &str| {
        std::fs::metadata(env.jeeves_dir().join(file))
            .unwrap()
            .modified()
            .unwrap()
    };
    let (toml_time, soul_time) = (modified("agent.toml"), modified("SOUL.md"));
    // The form as rendered, posted back unchanged (a browser's CRLF line breaks included).
    let (status, _, _) = env
        .edit_post(
            &manager,
            "jeeves",
            &jeeves_fields(
                &version,
                "Jeeves",
                "fake%2Ftest-model",
                "&tools=memory&soul=You+are+Jeeves.%0D%0ABe+brief.%0D%0A",
            ),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        (modified("agent.toml"), modified("SOUL.md")),
        (toml_time, soul_time)
    );
    assert_eq!(env.read_jeeves("agent.toml"), JEEVES);
}

#[tokio::test]
async fn edits_creations_and_edits_run_one_at_a_time() {
    let env = env().await;
    let manager = env.manager().await;
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let version = version_of(&html);
    let edit = |name: &str| AgentEdit {
        id: "jeeves".into(),
        name: name.into(),
        models: vec!["fake/test-model".into()],
        soul: name.into(),
        version: version.clone(),
        ..AgentEdit::default()
    };
    let (first, second) = (edit("First"), edit("Second"));
    // Both saw the same files: one saves, the other finds them changed.
    let (a, b) = tokio::join!(manager.edit(&first), manager.edit(&second));
    let (won, lost) = match (a, b) {
        (Ok(_), Err(lost)) => ("First", lost),
        (Err(lost), Ok(_)) => ("Second", lost),
        other => panic!("{other:?}"),
    };
    assert!(matches!(lost, EditError::Stale), "{lost}");
    assert_eq!(env.read_jeeves("SOUL.md"), won);
    assert!(
        env.read_jeeves("agent.toml")
            .contains(&format!("name = \"{won}\""))
    );
    assert_eq!(
        env.registry.config().agents[&AgentId::new("jeeves").unwrap()]
            .name
            .as_deref(),
        Some(won)
    );

    // An edit and a creation at once: both happen, neither disturbs the other.
    let fresh = manager.edit_form("jeeves").await.unwrap();
    let again = AgentEdit {
        id: "jeeves".into(),
        name: "Third".into(),
        models: fresh.models.clone(),
        soul: fresh.soul.clone(),
        version: fresh.version.clone(),
        ..AgentEdit::default()
    };
    let new = crate::agents::NewAgent {
        id: "butler".into(),
        models: vec!["fake/m".into()],
        ..crate::agents::NewAgent::default()
    };
    let (edited, created) = tokio::join!(manager.edit(&again), manager.create(&new));
    edited.unwrap();
    created.unwrap();
    assert_eq!(env.agent_dirs(), ["bunter", "butler", "jeeves"]);
    assert_eq!(env.registry.config().agents.len(), 3);
}

#[tokio::test]
async fn an_edit_replaces_the_configuration_for_new_work_and_keeps_the_agents_incarnation() {
    // A turn or a page holds a snapshot of the configuration, which an edit doesn't change; the
    // agent loop's cached session prompts follow the incarnation, which an edit keeps (see the
    // agent loop's tests), so running sessions go on with the prompt they started with.
    let env = env().await;
    let manager = env.manager().await;
    let jeeves = AgentId::new("jeeves").unwrap();
    let before = env.registry.config();
    let incarnation = env.registry.live_config().incarnation(&jeeves);
    assert!(incarnation.is_some());
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let fields = jeeves_fields(
        &version_of(&html),
        "Jeeves",
        "fake%2Ftest-model",
        "&tools=memory_search&soul=s",
    );
    let (status, _, _) = env.edit_post(&manager, "jeeves", &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let after = env.registry.config();
    assert_eq!(before.agents[&jeeves].tools, ["memory"]);
    assert_eq!(after.agents[&jeeves].tools, ["memory_search"]);
    assert_eq!(env.registry.live_config().incarnation(&jeeves), incarnation);
}
