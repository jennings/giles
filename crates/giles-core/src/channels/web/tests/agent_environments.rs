//! Router-level tests of the Environments field of the agent forms, over a configuration with two
//! environments, `box` (with a description) and `far`.

use super::agent_edit::{JEEVES, jeeves_fields, version_of};
use super::agents::guest;
use super::*;

/// Two environment instances, and nothing that has to run: configuration loading never starts a
/// plugin.
const ENVIRONMENTS: &str = "[environment.box]\ncomponent = \"docker\"\n\
     description = \"A <b>scratch</b> box\"\n[environment.far]\ncomponent = \"docker\"\n";

async fn env_with_environments() -> Env {
    env_with_toml(WebConfig::default(), &["web:owner"], ENVIRONMENTS).await
}

/// `jeeves`' tools, and an optional `environments` line, on top of [`JEEVES`].
fn write_jeeves(env: &Env, tools: &str, environments: Option<&str>) {
    env.hand_write_jeeves();
    let mut toml = JEEVES.replace(
        r#"tools = ["memory_add"]"#,
        &format!(
            "tools = {tools}{}",
            environments.map_or_else(String::new, |e| format!("\nenvironments = {e}"))
        ),
    );
    if environments.is_none() {
        toml = toml.replace("\nenvironments = ", "");
    }
    std::fs::write(env.jeeves_dir().join("agent.toml"), toml).unwrap();
}

/// The `environments` of an `agent.toml`.
fn environments_of(toml: &str) -> Vec<String> {
    parsed(toml)["environments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect()
}

fn parsed(toml: &str) -> toml::Table {
    toml::from_str(toml).unwrap_or_else(|e| panic!("{e}\n{toml}"))
}

const BOX_FIELD: &str = r#"name="environment:box""#;
const NO_TOOLS_NOTE: &str = r#"id="environment-note-no-tools" role="status">"#;
const NO_ENVIRONMENT_NOTE: &str = r#"id="environment-note-no-environment" role="status">"#;

#[tokio::test]
async fn the_forms_have_a_box_for_each_configured_environment_and_none_without_any() {
    let env = env_with_environments().await;
    write_jeeves(&env, r#"["memory_add"]"#, Some(r#"["far"]"#));
    let manager = env.manager().await;

    let (_, edit) = env.edit_page(&manager, "jeeves").await;
    let (_, new, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents/new", None)
        .await;
    for (form, far) in [(&edit, " checked"), (&new, "")] {
        for expected in [
            r#"<span class="label">Environments</span>"#.to_owned(),
            // The description is the owner's text: escaped.
            format!(
                r#"<label class="tool"><input type="checkbox" {BOX_FIELD}> <code>box</code>: A &#60;b&#62;scratch&#60;/b&#62; box</label>"#
            ),
            format!(
                r#"<label class="tool"><input type="checkbox" name="environment:far"{far}> <code>far</code></label>"#
            ),
        ] {
            assert!(form.contains(&expected), "{expected}\n{form}");
        }
        assert!(!form.contains("<b>scratch"), "{form}");
        // Before the Tools field, which says the environment tools need one.
        assert!(
            form.find("Environments</span>").unwrap() < form.find("Tools</span>").unwrap(),
            "{form}"
        );
        assert!(form.contains("Needs an environment."), "{form}");
    }

    // With none configured the field is gone (and so are the notes).
    let bare = super::env().await;
    let manager = bare.manager().await;
    let (_, edit) = bare.edit_page(&manager, "jeeves").await;
    let (_, new, _) = bare
        .agents_call(&manager, Method::GET, "/admin/agents/new", None)
        .await;
    for form in [&edit, &new] {
        assert!(!form.contains("Environments"), "{form}");
        assert!(!form.contains("environment:"), "{form}");
        assert!(!form.contains("environment-note"), "{form}");
        assert!(form.contains("Needs an environment."), "{form}");
    }
}

#[tokio::test]
async fn ticked_environments_are_saved_in_place_and_unticking_them_all_removes_the_key() {
    let env = env_with_environments().await;
    env.hand_write_jeeves();
    let manager = env.manager().await;
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    assert!(!html.contains("checked> <code>box"), "{html}");

    // Ticking adds the key; the rest of the file stays as written.
    let tick = |boxes: &str| {
        jeeves_fields(
            &version_of(&html),
            "Jeeves",
            "fake%2Ftest-model",
            &format!("&tool%3Amemory_add=on&tool%3Abash=on&tool%3Aread=on{boxes}&soul="),
        )
    };
    let (status, _, _) = env
        .edit_post(
            &manager,
            "jeeves",
            &tick("&environment%3Abox=on&environment%3Afar=on"),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let toml = env.read_jeeves("agent.toml");
    assert_eq!(environments_of(&toml), &["box", "far"]);
    assert!(toml.contains("# Jeeves, the household agent."), "{toml}");
    assert!(toml.contains("id = \"morning\"   # runs daily"), "{toml}");
    let running = &env.registry.config().agents[&AgentId::new("jeeves").unwrap()];
    let names: Vec<&str> = running.environments.iter().map(|e| e.as_str()).collect();
    assert_eq!(names, ["box", "far"]);

    // The form shows them ticked, and saving it as it is writes nothing.
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    assert!(
        html.contains(&format!(r#"<input type="checkbox" {BOX_FIELD} checked>"#)),
        "{html}"
    );
    assert!(
        html.contains(r#"name="environment:far" checked>"#),
        "{html}"
    );
    let (status, _, _) = env
        .edit_post(
            &manager,
            "jeeves",
            &jeeves_fields(
                &version_of(&html),
                "Jeeves",
                "fake%2Ftest-model",
                "&tool%3Amemory_add=on&tool%3Abash=on&tool%3Aread=on&environment%3Abox=on&environment%3Afar=on&soul=You+are+Jeeves.%0ABe+brief.%0A",
            ),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(env.read_jeeves("agent.toml"), toml);

    // Unticking one keeps the other; unticking both removes the key.
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let fields = |boxes: &str| {
        jeeves_fields(
            &version_of(&html),
            "Jeeves",
            "fake%2Ftest-model",
            &format!("&tool%3Amemory_add=on{boxes}&soul=You+are+Jeeves.%0ABe+brief.%0A"),
        )
    };
    let (status, _, _) = env
        .edit_post(&manager, "jeeves", &fields("&environment%3Afar=on"))
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(environments_of(&env.read_jeeves("agent.toml")), &["far"]);

    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let (status, _, _) = env
        .edit_post(
            &manager,
            "jeeves",
            &jeeves_fields(
                &version_of(&html),
                "Jeeves",
                "fake%2Ftest-model",
                "&tool%3Amemory_add=on&soul=You+are+Jeeves.%0ABe+brief.%0A",
            ),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let toml = env.read_jeeves("agent.toml");
    assert!(!parsed(&toml).contains_key("environments"), "{toml}");
    assert!(toml.contains("# Jeeves, the household agent."), "{toml}");
    let running = &env.registry.config().agents[&AgentId::new("jeeves").unwrap()];
    assert!(running.environments.is_empty());
}

#[tokio::test]
async fn a_hand_written_order_survives_a_save_and_new_names_follow_it() {
    let env = env_with_environments().await;
    write_jeeves(&env, r#"["bash"]"#, Some(r#"["far", "box"]"#));
    let manager = env.manager().await;
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    let before = env.read_jeeves("agent.toml");
    let post = |boxes: &str| {
        jeeves_fields(
            &version_of(&html),
            "Jeeves",
            "fake%2Ftest-model",
            &format!("&tool%3Abash=on{boxes}&soul=You+are+Jeeves.%0ABe+brief.%0A"),
        )
    };
    let (status, _, _) = env
        .edit_post(
            &manager,
            "jeeves",
            &post("&environment%3Abox=on&environment%3Afar=on"),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        env.read_jeeves("agent.toml"),
        before,
        "the order isn't rewritten"
    );
}

#[tokio::test]
async fn an_environment_that_is_not_configured_still_shows_and_saving_it_is_refused() {
    let env = env_with_environments().await;
    write_jeeves(&env, r#"["bash"]"#, Some(r#"["gone", "box"]"#));
    let manager = env.manager().await;
    let toml = env.read_jeeves("agent.toml");
    let (status, html) = env.edit_page(&manager, "jeeves").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(r#"name="environment:gone" checked> <code>gone</code> Not configured in giles.toml, so saving fails until you untick it."#),
        "{html}"
    );
    assert!(html.contains(&format!(r#"{BOX_FIELD} checked>"#)), "{html}");

    // Saved as it is, it fails like a hand-written agent.toml does, with what was typed shown.
    let fields = jeeves_fields(
        &version_of(&html),
        "Jeeves",
        "fake%2Ftest-model",
        "&tool%3Abash=on&environment%3Agone=on&environment%3Abox=on&soul=hello",
    );
    let (status, html, _) = env.edit_post(&manager, "jeeves", &fields).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        html.contains("environments[0]: no environment instance"),
        "{html}"
    );
    assert!(
        html.contains(r#"name="environment:gone" checked>"#),
        "{html}"
    );
    assert_eq!(env.read_jeeves("agent.toml"), toml);

    // Unticked, it goes and the configured one stays.
    let fields = jeeves_fields(
        &version_of(&html),
        "Jeeves",
        "fake%2Ftest-model",
        "&tool%3Abash=on&environment%3Abox=on&soul=hello",
    );
    let (status, _, _) = env.edit_post(&manager, "jeeves", &fields).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(environments_of(&env.read_jeeves("agent.toml")), &["box"]);

    // With no environment configured at all it shows the same way, so it isn't dropped unseen.
    let bare = super::env().await;
    write_jeeves(&bare, r#"["bash"]"#, Some(r#"["gone"]"#));
    let manager = bare.manager().await;
    let (_, html) = bare.edit_page(&manager, "jeeves").await;
    assert!(
        html.contains(r#"name="environment:gone" checked>"#),
        "{html}"
    );
}

#[tokio::test]
async fn creating_an_agent_with_environments_writes_them_and_none_writes_no_key() {
    let env = env_with_environments().await;
    let manager = env.manager().await;
    let (status, _, _) = env
        .agents_call(
            &manager,
            Method::POST,
            "/admin/agents",
            Some(&format!(
                "csrf={CSRF}&id=butler&models=fake%2Fm&tool%3Abash=on&environment%3Afar=on&environment%3Abox=on"
            )),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let toml = std::fs::read_to_string(env.root().agents_dir().join("butler/agent.toml")).unwrap();
    assert_eq!(environments_of(&toml), &["box", "far"]);
    let running = &env.registry.config().agents[&AgentId::new("butler").unwrap()];
    assert_eq!(running.environments.len(), 2);

    let (status, _, _) = env
        .agents_call(
            &manager,
            Method::POST,
            "/admin/agents",
            Some(&format!("csrf={CSRF}&id=plain&models=fake%2Fm")),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let toml = std::fs::read_to_string(env.root().agents_dir().join("plain/agent.toml")).unwrap();
    assert!(!toml.contains("environments"), "{toml}");

    // A refused form is shown again with its boxes as they were ticked.
    let (status, html, _) = env
        .agents_call(
            &manager,
            Method::POST,
            "/admin/agents",
            Some(&format!(
                "csrf={CSRF}&id=bad&models=nope%2Fm&environment%3Afar=on"
            )),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        html.contains(r#"name="environment:far" checked>"#),
        "{html}"
    );
    assert!(!html.contains(&format!("{BOX_FIELD} checked")), "{html}");
}

#[tokio::test]
async fn the_forms_note_an_environment_without_its_tools_and_tools_without_an_environment() {
    let env = env_with_environments().await;
    let manager = env.manager().await;
    // (tools, environments, no tools note, no environment note)
    let cases = [
        (r#"["memory_add"]"#, Some(r#"["box"]"#), true, false),
        (r#"["bash"]"#, None, false, true),
        (r#"["read"]"#, Some(r#"["box"]"#), false, false),
        (r#"["memory_add"]"#, None, false, false),
        // A glob in Other tools allows the environment tools.
        (r#"["*"]"#, Some(r#"["box"]"#), false, false),
        (r#"["memory*"]"#, Some(r#"["box"]"#), true, false),
    ];
    for (tools, environments, no_tools, no_environment) in cases {
        write_jeeves(&env, tools, environments);
        let (_, html) = env.edit_page(&manager, "jeeves").await;
        for (id, shown) in [
            (NO_TOOLS_NOTE, no_tools),
            (NO_ENVIRONMENT_NOTE, no_environment),
        ] {
            // Both notes are always in the page, for the script to show; the server hides the
            // ones that don't apply.
            let note = html
                .find(id.trim_end_matches('>'))
                .unwrap_or_else(|| panic!("{id}\n{html}"));
            let tag_end = html[note..].find('>').unwrap();
            let hidden = html[note..note + tag_end].ends_with(" hidden");
            assert_eq!(!hidden, shown, "{tools} {environments:?} {id}\n{html}");
        }
    }

    // The notes say what to do.
    let (_, html) = env.edit_page(&manager, "jeeves").await;
    assert!(
        html.contains("so the agent can't use it. Tick <code>bash</code>"),
        "{html}"
    );
    assert!(html.contains("so the agent isn't offered them."), "{html}");

    // Both forms load the script that keeps the notes current as boxes are ticked, and the tools
    // the notes are about are marked for it. It is a file: no inline script.
    let script = crate::http::assets::agent_form_script();
    let (_, new, _) = env
        .agents_call(&manager, Method::GET, "/admin/agents/new", None)
        .await;
    for form in [&html, &new] {
        assert!(
            form.contains(&format!(r#"<script src="{}" defer></script>"#, script.path)),
            "{form}"
        );
        assert_eq!(
            form.matches("<script").count(),
            form.matches("<script src=").count(),
            "{form}"
        );
        assert!(
            form.contains(r#"name="tool:bash" data-environment-tool"#),
            "{form}"
        );
        assert!(
            !form.contains(r#"name="tool:memory_add" data-environment-tool"#),
            "{form}"
        );
    }
    let body = String::from_utf8(script.body.to_vec()).unwrap();
    for id in [
        "environment-note-no-tools",
        "environment-note-no-environment",
    ] {
        assert!(body.contains(id), "{id}");
    }
}

#[tokio::test]
async fn only_the_owner_gets_environment_boxes_saved() {
    let env = env_with_environments().await;
    write_jeeves(&env, r#"["bash"]"#, None);
    let manager = env.manager().await;
    let toml = env.read_jeeves("agent.toml");
    let response = call(
        env.agents_router(&manager, Some(guest())),
        Method::POST,
        "/admin/agents/jeeves/edit",
        &[("x-csrf-token", CSRF)],
        Some(&format!("csrf={CSRF}&models=fake%2Fm&environment%3Abox=on")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(env.read_jeeves("agent.toml"), toml);
}
