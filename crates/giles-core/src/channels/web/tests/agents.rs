//! Router-level tests of the agent pages (TASKS.md 5.1) over the test configuration's registry.

use std::os::unix::fs::symlink;

use crate::agents::{AgentCreator, CreateError, MAX_FILE_BYTES, MAX_FILES, NewAgent};
use crate::config::{Config, LoadOptions};
use crate::paths::DataRoot;

use super::*;

const XSS: &str = "<script>alert(1)</script>";

pub(super) fn guest() -> WebUser {
    WebUser {
        user: UserId::new("guest").unwrap(),
        sender: NativeId::new("guest").unwrap(),
        csrf: CSRF.into(),
        login: auth::Login::Test,
    }
}

/// Lists two models, `fake/listed-a` and `fake/listed-b`.
struct ListingProvider;

#[async_trait]
impl giles_types::Provider for ListingProvider {
    async fn list_models(&self) -> giles_types::Result<Vec<giles_types::ModelInfo>> {
        Ok(["listed-b", "listed-a"]
            .into_iter()
            .map(|id| giles_types::ModelInfo {
                id: id.into(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
                media_types: Vec::new(),
                reasoning: false,
            })
            .collect())
    }

    async fn complete(
        &self,
        _: giles_types::CompletionRequest,
    ) -> giles_types::Result<BoxStream<'static, giles_types::Result<giles_types::CompletionEvent>>>
    {
        Err(Error::InvalidRequest("not in this fake".into()))
    }
}

impl Env {
    fn root(&self) -> &DataRoot {
        self.registry.root()
    }

    fn agents_router(&self, creator: &Arc<AgentCreator>, user: Option<WebUser>) -> axum::Router {
        let app = self
            .app
            .install_agents(self.app.install(RouterBuilder::new()), creator.clone())
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    fn creator(&self) -> Arc<AgentCreator> {
        Arc::new(AgentCreator::new(self.registry.clone()))
    }

    /// `method path` as the owner; the status and body. Every answer is `no-store`.
    async fn agents_call(
        &self,
        creator: &Arc<AgentCreator>,
        method: Method,
        path: &str,
        form: Option<&str>,
    ) -> (StatusCode, String, Option<String>) {
        let response = call(
            self.agents_router(creator, Some(owner())),
            method,
            path,
            &[],
            form,
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

    /// The names in `agents/`.
    fn agent_dirs(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.root().agents_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

/// The create form for agent `id` with model `model`, plus `extra` fields.
fn form(id: &str, model: &str, extra: &str) -> String {
    format!("csrf={CSRF}&id={id}&models={model}{extra}")
}

#[tokio::test]
async fn only_the_owner_gets_the_agent_pages() {
    let env = env().await;
    let creator = env.creator();
    let cases = [
        (Method::GET, "/admin/agents"),
        (Method::GET, "/admin/agents/new"),
        (Method::GET, "/admin/agents/jeeves"),
        (Method::POST, "/admin/agents"),
    ];
    for (method, path) in cases {
        for (user, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(guest()), StatusCode::FORBIDDEN),
        ] {
            let response = call(
                env.agents_router(&creator, user),
                method.clone(),
                path,
                &[("x-csrf-token", CSRF)],
                Some(&form("butler", "fake/m", "")),
            )
            .await;
            assert_eq!(response.status(), expected, "{method} {path}");
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
            let html = body_text(response).await;
            assert!(!html.contains("Jeeves"), "{html}");
        }
    }
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
}

#[tokio::test]
async fn the_list_shows_every_agent_with_its_models_and_links() {
    let env = env().await;
    let creator = env.creator();
    let (status, html, _) = env
        .agents_call(&creator, Method::GET, "/admin/agents", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        r#"<a href="/admin/agents/jeeves">Jeeves (jeeves)</a>"#,
        r#"<a href="/admin/agents/bunter">bunter</a>"#,
        "<code>fake/test-model</code>",
        r#"<a href="/chats/new?agent=jeeves">Chat</a>"#,
        r#"<a href="/admin/agents">Agents</a>"#,
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
}

#[tokio::test]
async fn an_agents_page_shows_its_files_escaped_and_refuses_links() {
    let env = env().await;
    let creator = env.creator();
    let dir = env.root().agents_dir().join("jeeves");
    let write = |relative: &str, contents: &[u8]| {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    };
    write("SOUL.md", format!("You are Jeeves. {XSS}").as_bytes());
    write("MEMORY.md", b"The owner likes tea.");
    write("users/owner/MEMORY.md", b"Prefers Earl Grey.");
    write("groups/libera/%23giles/MEMORY.md", b"The channel is quiet.");
    write("skills/greet/SKILL.md", b"---\nname: greet\n---\nSay hi.");
    write("photo.png", &[0x89, b'P', b'N', b'G', 0, 1, 2]);
    write("big.md", &vec![b'x'; MAX_FILE_BYTES as usize + 10]);
    write(".hidden", b"not listed");
    // Outside the agent's directory: a link to it is listed, never followed.
    let outside = env.root().root().join("outside-secret.txt");
    std::fs::write(&outside, "TOP SECRET").unwrap();
    symlink(&outside, dir.join("link.md")).unwrap();
    symlink(env.root().root(), dir.join("linked-dir")).unwrap();

    let (status, html, _) = env
        .agents_call(&creator, Method::GET, "/admin/agents/jeeves", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<h1>Jeeves (jeeves)</h1>",
        "<code>memory</code>",
        "<code>agent.toml</code> <span class=\"detail\">Configuration</span>",
        "name = &#34;Jeeves&#34;",
        "You are Jeeves. &#60;script&#62;alert(1)&#60;/script&#62;",
        "Agent-wide memory",
        "Memory about user owner",
        "Prefers Earl Grey.",
        "<code>groups/libera/%23giles/MEMORY.md</code> <span class=\"detail\">Memory from group #giles on libera</span>",
        "Prompt of job morning",
        "Plan my day.",
        "Skill greet",
        "7 bytes, not text: not shown",
        "only the first 64 KiB are shown",
        "<code>link.md</code>",
        "<code>linked-dir</code>",
        "a symbolic link: not followed",
        r#"<a class="button" href="/chats/new?agent=jeeves">Chat</a>"#,
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(!html.contains(XSS), "{html}");
    assert!(!html.contains("TOP SECRET"), "{html}");
    assert!(!html.contains(".hidden"), "{html}");
    assert!(!html.contains("giles.toml"), "{html}");
    // The cut-off file shows exactly the cap.
    assert!(html.contains(&"x".repeat(MAX_FILE_BYTES as usize)));
    assert!(!html.contains(&"x".repeat(MAX_FILE_BYTES as usize + 1)));
    // agent.toml comes first.
    assert!(html.find("agent.toml").unwrap() < html.find("SOUL.md").unwrap());

    // Only configured agents have pages: no paths, no `..`.
    for path in [
        "/admin/agents/nope",
        "/admin/agents/..",
        "/admin/agents/%2E%2E",
        "/admin/agents/..%2Fjeeves",
        "/admin/agents/jeeves%2F..%2F..",
    ] {
        let (status, html, _) = env.agents_call(&creator, Method::GET, path, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(!html.contains("agent.toml"), "{path}\n{html}");
    }
}

#[tokio::test]
async fn a_page_lists_at_most_max_files() {
    let env = env().await;
    let creator = env.creator();
    let notes = env.root().agents_dir().join("bunter/notes");
    std::fs::create_dir_all(&notes).unwrap();
    for i in 0..MAX_FILES + 5 {
        std::fs::write(notes.join(format!("{i:04}.md")), "note").unwrap();
    }
    let (status, html, _) = env
        .agents_call(&creator, Method::GET, "/admin/agents/bunter", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        html.matches("class=\"admin-item agent-file\"").count(),
        MAX_FILES
    );
    assert!(html.contains("Not every file is listed"), "{html}");
    // agent.toml is listed ahead of the notes.
    assert!(html.contains("<code>agent.toml</code>"), "{html}");
}

#[tokio::test]
async fn creating_an_agent_writes_files_that_load_and_makes_it_usable_at_once() {
    let env = env().await;
    env.registry
        .register_provider(
            &InstanceName::new("fake").unwrap(),
            Arc::new(ListingProvider),
        )
        .unwrap();
    let creator = env.creator();
    let (status, html, _) = env
        .agents_call(&creator, Method::GET, "/admin/agents/new", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(r#"<option value="fake/listed-a"></option>"#),
        "{html}"
    );
    assert!(html.contains(&format!(r#"name="csrf" value="{CSRF}""#)));

    let soul = "You are Butler.\nBe brief.";
    let fields = format!(
        "&name=The%20Butler&tools=memory%2C+load_skill&ask=memory&timezone=Europe%2FLondon&soul={}",
        soul.replace(' ', "+").replace('\n', "%0A")
    );
    let (status, _, location) = env
        .agents_call(
            &creator,
            Method::POST,
            "/admin/agents",
            Some(&form("butler", "fake/listed-a", &fields)),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/admin/agents/butler"));

    // The files, loaded as at startup.
    let dir = env.root().agents_dir().join("butler");
    assert_eq!(std::fs::read_to_string(dir.join("SOUL.md")).unwrap(), soul);
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "no temporary files are left: {names:?}");
    let reloaded = Config::load_with(
        env.root(),
        &LoadOptions {
            env: &|_| None,
            giles_exe: None,
            ..LoadOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let butler = &reloaded.agents[&AgentId::new("butler").unwrap()];
    assert_eq!(butler.name.as_deref(), Some("The Butler"));
    assert_eq!(butler.models[0].to_string(), "fake/listed-a");
    assert_eq!(butler.tools, ["memory", "load_skill"]);
    assert_eq!(butler.ask, ["memory"]);
    assert_eq!(
        butler.timezone.as_ref().unwrap().iana_name(),
        Some("Europe/London")
    );

    // Running: listed, with a page, and a chat can be started with it.
    assert!(env.registry.config().agents.contains_key(&butler.id));
    let (_, list, _) = env
        .agents_call(&creator, Method::GET, "/admin/agents", None)
        .await;
    assert!(list.contains("The Butler (butler)"), "{list}");
    let (status, page, _) = env
        .agents_call(&creator, Method::GET, "/admin/agents/butler", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Be brief."), "{page}");
    let chats = body_text(env.get("/").await).await;
    assert!(
        chats.contains(r#"<option value="butler">The Butler (butler)</option>"#),
        "{chats}"
    );
    let response = env.get("/chats/new?agent=butler").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_browsers_crlf_line_breaks_in_the_soul_are_stored_as_lf() {
    let env = env().await;
    let creator = env.creator();
    let (status, _, _) = env
        .agents_call(
            &creator,
            Method::POST,
            "/admin/agents",
            Some(&form(
                "butler",
                "fake/m",
                "&soul=One.%0D%0A%0D%0ATwo.%0D%0A",
            )),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let soul = std::fs::read_to_string(env.root().agents_dir().join("butler/SOUL.md")).unwrap();
    assert_eq!(soul, "One.\n\nTwo.\n");
}

#[tokio::test]
async fn an_invalid_form_is_shown_again_and_writes_nothing() {
    let env = env().await;
    let creator = env.creator();
    let cases = [
        ("Bad%20Id", "fake/m", "", StatusCode::BAD_REQUEST, "invalid"),
        ("..%2Fx", "fake/m", "", StatusCode::BAD_REQUEST, "invalid"),
        ("", "fake/m", "", StatusCode::BAD_REQUEST, "invalid"),
        ("new", "fake/m", "", StatusCode::BAD_REQUEST, "reserved"),
        (
            "jeeves",
            "fake/m",
            "",
            StatusCode::CONFLICT,
            "already an agent `jeeves`",
        ),
        (
            "JEEVES",
            "fake/m",
            "",
            StatusCode::CONFLICT,
            "already an agent `jeeves`",
        ),
        (
            "butler",
            "nope/m",
            "",
            StatusCode::BAD_REQUEST,
            "models[0]: no provider instance",
        ),
        (
            "butler",
            "",
            "",
            StatusCode::BAD_REQUEST,
            "at least one model",
        ),
        (
            "butler",
            "fake/m",
            "&tools=bad%07glob",
            StatusCode::BAD_REQUEST,
            "tools[0]: invalid tool glob",
        ),
        (
            "butler",
            "fake/m",
            "&timezone=Mars%2FOlympus",
            StatusCode::BAD_REQUEST,
            "timezone",
        ),
        (
            "butler",
            "fake/m",
            &format!("&name={}", "%3Cb%3E"),
            StatusCode::SEE_OTHER,
            "",
        ),
    ];
    for (id, model, extra, expected, message) in cases {
        let (status, html, _) = env
            .agents_call(
                &creator,
                Method::POST,
                "/admin/agents",
                Some(&form(id, model, extra)),
            )
            .await;
        assert_eq!(status, expected, "{id} {model} {extra}\n{html}");
        if expected == StatusCode::SEE_OTHER {
            continue;
        }
        assert!(html.contains(message), "{message}\n{html}");
        // The form keeps what was typed, escaped.
        assert!(html.contains(r#"<form class="agent-form""#), "{html}");
        assert!(
            !html.contains("/var/") && !html.contains("/tmp/"),
            "no paths: {html}"
        );
        assert_eq!(env.agent_dirs(), ["bunter", "jeeves"], "{id}");
    }
    // The last case was valid; the name stays text.
    let page = std::fs::read_to_string(env.root().agents_dir().join("butler/agent.toml")).unwrap();
    assert!(page.contains("name = \"<b>\""), "{page}");

    // A typed name is shown escaped when the form comes back.
    let (_, html, _) = env
        .agents_call(
            &creator,
            Method::POST,
            "/admin/agents",
            Some(&form("other", "nope/m", "&name=%22%3E%3Cscript%3E")),
        )
        .await;
    assert!(html.contains("&#34;&#62;&#60;script&#62;"), "{html}");
    assert!(!html.contains("\"><script>"), "{html}");
}

#[tokio::test]
async fn creating_an_agent_needs_the_csrf_token() {
    let env = env().await;
    let creator = env.creator();
    for (headers, form) in [
        (&[][..], "id=butler&models=fake/m".to_owned()),
        (&[][..], "csrf=wrong&id=butler&models=fake/m".to_owned()),
        (
            &[("x-csrf-token", "wrong")][..],
            "id=butler&models=fake/m".to_owned(),
        ),
    ] {
        let response = call(
            env.agents_router(&creator, Some(owner())),
            Method::POST,
            "/admin/agents",
            headers,
            Some(&form),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{form}");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    }
    assert_eq!(env.agent_dirs(), ["bunter", "jeeves"]);
    let response = call(
        env.agents_router(&creator, Some(owner())),
        Method::POST,
        "/admin/agents",
        &[("x-csrf-token", CSRF)],
        Some("id=butler&models=fake/m"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn of_two_creations_with_the_same_id_one_wins() {
    let env = env().await;
    let creator = env.creator();
    let agent = |soul: &str| NewAgent {
        id: "butler".into(),
        models: vec!["fake/m".into()],
        soul: soul.into(),
        ..NewAgent::default()
    };
    let (first, second) = (agent("first"), agent("second"));
    let (a, b) = tokio::join!(creator.create(&first), creator.create(&second));
    let (won, lost) = match (a, b) {
        (Ok(_), Err(lost)) => ("first", lost),
        (Err(lost), Ok(_)) => ("second", lost),
        other => panic!("{other:?}"),
    };
    assert!(matches!(lost, CreateError::Exists(_)), "{lost}");
    let soul = std::fs::read_to_string(env.root().agents_dir().join("butler/SOUL.md")).unwrap();
    assert_eq!(soul, won);
    assert_eq!(env.agent_dirs(), ["bunter", "butler", "jeeves"]);
    assert_eq!(env.registry.config().agents.len(), 3);

    // A second creator over the same registry (another page) can't take it either, and neither
    // can a directory someone made by hand.
    let other = env.creator();
    assert!(matches!(
        other.create(&agent("again")).await,
        Err(CreateError::Exists(_))
    ));
    std::fs::create_dir(env.root().agents_dir().join("manual")).unwrap();
    let manual = NewAgent {
        id: "manual".into(),
        ..agent("x")
    };
    assert!(matches!(
        creator.create(&manual).await,
        Err(CreateError::Exists(_))
    ));
    assert!(!env.root().agents_dir().join("manual/SOUL.md").exists());
}

/// A creation cut off by a crash leaves only its staging directory, which doesn't stop startup
/// and is removed when the next daemon's creator is built.
#[tokio::test]
async fn a_staging_directory_left_by_a_crash_breaks_nothing() {
    let env = env().await;
    let stale = env.root().agents_dir().join(".butler.creating");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("SOUL.md"), "half").unwrap();
    std::fs::write(stale.join(".agent.toml.tmp"), "models = [").unwrap();

    let loaded = Config::load_with(
        env.root(),
        &LoadOptions {
            env: &|_| None,
            giles_exe: None,
            ..LoadOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(loaded.agents.len(), 2);

    let creator = env.creator();
    assert!(!stale.exists());
    let (_, list, _) = env
        .agents_call(&creator, Method::GET, "/admin/agents", None)
        .await;
    assert!(!list.contains("creating"), "{list}");
    let (status, _, _) = env
        .agents_call(
            &creator,
            Method::POST,
            "/admin/agents",
            Some(&form("butler", "fake/m", "")),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(env.agent_dirs(), ["bunter", "butler", "jeeves"]);
}
