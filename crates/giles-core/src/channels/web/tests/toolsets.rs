//! Router-level tests of the toolset pages over the test configuration (the channel `web`, the
//! agents `jeeves` and `bunter`, and the stdio toolset `files` of `giles.toml`), a real
//! [`ToolsetManager`] whose remote servers never answer, and a fake admin service.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use giles_proto::admin::v1 as pb;
use giles_types::ToolSpec;

use super::admin::FakeAdmin;
use super::agents::guest;
use super::*;
use crate::agents::{AgentManager, AgentManagerOptions};
use crate::config::{Config, LoadOptions, ToolsetSpec};
use crate::registry::Registry;
use crate::routes::Routes;
use crate::toolsets::{
    ToolsetHost, ToolsetLauncher, ToolsetManager, ToolsetManagerOptions, ToolsetSource, ToolsetView,
};

const XSS: &str = "<script>alert(1)</script>";

/// A remote toolset that never answers: nothing listens on the port.
const BOX: &str = r#"{ "url": "http://127.0.0.1:9/mcp" }"#;

fn spec(json: &str) -> ToolsetSpec {
    ToolsetSpec::from_json(json).unwrap_or_else(|e| panic!("{e}"))
}

/// `pairs` as a form body.
fn encode(pairs: &[(&str, &str)]) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

fn write_secrets(env: &Env, text: &str) {
    let file = env.root().secrets_file();
    std::fs::write(&file, text).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn updated_at(view: &ToolsetView) -> Timestamp {
    match view.source {
        ToolsetSource::Stored { updated_at, .. } => updated_at,
        ToolsetSource::File => panic!("{view:?} is not stored"),
    }
}

/// The stored toolsets' names.
async fn stored(env: &Env) -> Vec<String> {
    env.store
        .toolsets()
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.name)
        .collect()
}

/// The stored spec of `name`.
async fn stored_spec(env: &Env, name: &str) -> ToolsetSpec {
    let row = env
        .store
        .toolsets()
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("no stored toolset {name}"));
    ToolsetSpec::from_json(&row.spec).unwrap()
}

/// What the fake admin service says about a toolset.
fn plugin(
    instance: &str,
    state: pb::PluginState,
    tools: u32,
    error: Option<&str>,
) -> pb::PluginInstance {
    pb::PluginInstance {
        instance: instance.into(),
        kind: pb::InstanceKind::Toolset.into(),
        state: state.into(),
        tool_count: Some(tools),
        last_error: error.map(str::to_owned),
        ..pb::PluginInstance::default()
    }
}

/// The toolset pages over a registry of their own, so a test can change `giles.toml` first.
struct Toolsets {
    app: WebApp,
    registry: Arc<Registry>,
    manager: Arc<ToolsetManager>,
    admin: Arc<FakeAdmin>,
}

impl Env {
    /// The toolset pages of this environment's configuration, plus an agent `wooster` that lists
    /// tools of a toolset `box` and `core` (TOML) appended to `giles.toml`.
    async fn toolsets(&self, core: &str) -> Toolsets {
        let root = self.root();
        let wooster = root.agents_dir().join("wooster");
        std::fs::create_dir_all(&wooster).unwrap();
        std::fs::write(
            wooster.join("agent.toml"),
            "models = [\"fake/test-model\"]\ntools = [\"memory_add\", \"box__*\"]\n\
             ask = [\"box__delete_*\"]\n",
        )
        .unwrap();
        let path = root.config_file();
        let toml = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{toml}{core}")).unwrap();
        let config = Config::load_with(
            root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let registry = Arc::new(Registry::new(root.clone(), Arc::new(config)));
        let app = WebApp::new(
            self.channel.clone(),
            self.store.clone(),
            self.blobs.clone(),
            registry.live_config().clone(),
            self.shutdown.clone(),
        );
        let host = Arc::new(
            ToolsetHost::start(
                ToolsetLauncher::new(
                    root.clone(),
                    PathBuf::from("/nonexistent/giles"),
                    Err("there is no sandbox in tests".to_owned()),
                    self.blobs.clone(),
                ),
                &registry,
            )
            .await,
        );
        let manager = ToolsetManager::start(ToolsetManagerOptions {
            store: self.store.clone(),
            root: root.clone(),
            config: registry.live_config().clone(),
            host,
            agent_lock: Arc::default(),
        })
        .await
        .unwrap();
        Toolsets {
            app,
            registry,
            manager,
            admin: Arc::new(FakeAdmin::default()),
        }
    }
}

impl Toolsets {
    fn router(&self, user: Option<WebUser>) -> axum::Router {
        let builder = self.app.install(RouterBuilder::new());
        let app = self
            .app
            .install_toolsets(
                builder,
                self.manager.clone(),
                self.admin.clone(),
                self.registry.clone(),
            )
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    /// `method path` as the owner, with the CSRF token and `pairs` as the form if `pairs` is
    /// given. Every answer is `no-store`. The status, body, and `Location`.
    async fn send(
        &self,
        method: Method,
        path: &str,
        pairs: Option<&[(&str, &str)]>,
    ) -> (StatusCode, String, Option<String>) {
        let form = pairs.map(|pairs| {
            let mut all = vec![("csrf", CSRF)];
            all.extend_from_slice(pairs);
            encode(&all)
        });
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

    async fn get(&self, path: &str) -> (StatusCode, String) {
        let (status, html, _) = self.send(Method::GET, path, None).await;
        (status, html)
    }

    async fn post(
        &self,
        path: &str,
        pairs: &[(&str, &str)],
    ) -> (StatusCode, String, Option<String>) {
        self.send(Method::POST, path, Some(pairs)).await
    }

    /// The agent form, which has a box for each toolset in the running configuration.
    async fn agent_form(&self, env: &Env) -> String {
        let manager = Arc::new(AgentManager::new(AgentManagerOptions {
            registry: self.registry.clone(),
            store: env.store.clone(),
            memory_dbs: env.memory_dbs.clone(),
            routes: Arc::new(
                Routes::open(env.store.clone(), self.registry.live_config().clone())
                    .await
                    .unwrap(),
            ),
            jobs: env.jobs.clone(),
            activity: env.activity.clone(),
            lock: Arc::default(),
        }));
        let app = self
            .app
            .install_agents(self.app.install(RouterBuilder::new()), manager)
            .build()
            .layer(Extension(owner()));
        let response = call(app, Method::GET, "/admin/agents/new", &[], None).await;
        assert_eq!(response.status(), StatusCode::OK);
        body_text(response).await
    }

    fn set_plugins(&self, plugins: Vec<pb::PluginInstance>) {
        self.admin.status.lock().unwrap().plugins = plugins;
    }
}

/// A form that adds the toolset `name`, a remote server that never answers.
fn remote_form(name: &str) -> Vec<(&str, &str)> {
    vec![("name", name), ("url", "http://127.0.0.1:9/mcp")]
}

#[tokio::test]
async fn only_the_owner_gets_the_toolset_pages() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager.add("box", &spec(BOX)).await.unwrap();
    let cases = [
        (Method::GET, "/admin/mcp-servers"),
        (Method::GET, "/admin/mcp-servers/new"),
        (Method::GET, "/admin/mcp-servers/box"),
        (Method::GET, "/admin/mcp-servers/box/edit"),
        (Method::GET, "/admin/mcp-servers/box/remove"),
        (Method::GET, "/admin/mcp-servers/box/rename?new=chest"),
        (Method::POST, "/admin/mcp-servers"),
        (Method::POST, "/admin/mcp-servers/box/edit"),
        (Method::POST, "/admin/mcp-servers/box/remove"),
        (Method::POST, "/admin/mcp-servers/box/rename"),
        (Method::POST, "/admin/mcp-servers/box/restart"),
    ];
    let body = encode(&[
        ("csrf", CSRF),
        ("name", "other"),
        ("url", "http://127.0.0.1:9/other"),
        ("deadline", "2m"),
    ]);
    for (method, path) in cases {
        for (user, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(guest()), StatusCode::FORBIDDEN),
        ] {
            let response = call(
                t.router(user),
                method.clone(),
                path,
                &[("x-csrf-token", CSRF)],
                Some(&body),
            )
            .await;
            assert_eq!(response.status(), expected, "{method} {path}");
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        }
    }
    assert_eq!(stored(&env).await, ["box"]);
    assert_eq!(stored_spec(&env, "box").await, spec(BOX));
    assert!(t.admin.restarted().is_empty());
}

#[tokio::test]
async fn every_change_needs_the_csrf_token() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager.add("box", &spec(BOX)).await.unwrap();
    t.set_plugins(vec![plugin("box", pb::PluginState::Running, 0, None)]);
    let updated = updated_at(&t.manager.get("box").await.unwrap()).to_string();
    let form = encode(&[
        ("name", "other"),
        ("updated_at", updated.as_str()),
        ("url", "http://127.0.0.1:9/other"),
    ]);
    for path in [
        "/admin/mcp-servers",
        "/admin/mcp-servers/box/edit",
        "/admin/mcp-servers/box/remove",
        "/admin/mcp-servers/box/rename",
        "/admin/mcp-servers/box/restart",
    ] {
        for (headers, body) in [
            // No token, a wrong token in the form, and a wrong one in the header.
            (vec![], form.clone()),
            (vec![], format!("csrf=wrong&{form}")),
            (
                vec![("x-csrf-token", "wrong")],
                format!("csrf={CSRF}&{form}"),
            ),
        ] {
            let response = call(
                t.router(Some(owner())),
                Method::POST,
                path,
                &headers,
                Some(&body),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }
    assert_eq!(stored(&env).await, ["box"]);
    assert_eq!(stored_spec(&env, "box").await, spec(BOX));
    assert!(t.admin.restarted().is_empty());
    // The token may come in the header instead of the form, like the other pages.
    let response = call(
        t.router(Some(owner())),
        Method::POST,
        "/admin/mcp-servers/box/restart",
        &[("x-csrf-token", CSRF)],
        Some(""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(t.admin.restarted(), ["toolset/box"]);
    let response = call(
        t.router(Some(owner())),
        Method::POST,
        "/admin/mcp-servers/box/remove",
        &[("x-csrf-token", CSRF)],
        Some(""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(stored(&env).await.is_empty());
}

#[tokio::test]
async fn the_list_shows_state_tools_and_errors_and_giles_toml_toolsets_read_only() {
    let env = env().await;
    let t = env.toolsets("").await;
    let (_, html) = t.get("/admin/mcp-servers").await;
    assert!(html.contains(r#"id="toolset-files""#), "{html}");
    t.manager.add("box", &spec(BOX)).await.unwrap();
    t.manager
        .add(
            "remote",
            &spec(r#"{ "url": "https://mcp.example.com/mcp" }"#),
        )
        .await
        .unwrap();
    t.set_plugins(vec![
        plugin("files", pb::PluginState::Running, 4, None),
        plugin(
            "box",
            pb::PluginState::Failed,
            0,
            Some(&format!("cannot connect {XSS}")),
        ),
        plugin("remote", pb::PluginState::Idle, 2, None),
    ]);
    let (status, html) = t.get("/admin/mcp-servers").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        r#"<a href="/admin/mcp-servers">MCP servers</a>"#,
        r#"<a class="button" href="/admin/mcp-servers/new">New MCP server</a>"#,
        r#"<a href="/admin/mcp-servers/box">box</a>"#,
        "<code>http://127.0.0.1:9/mcp</code>",
        // The stdio toolset of giles.toml, as it runs.
        "<td>command</td>",
        "<code>/x</code>",
        "<td>remote URL</td>",
        "<code>https://mcp.example.com/mcp</code>",
        r#"<td class="state-bad">failed</td>"#,
        r#"<td class="state-ok">running</td>"#,
        r#"<td class="state-ok">idle</td>"#,
        "<td>4</td>",
        "<td>2</td>",
        "cannot connect &#60;script&#62;alert(1)&#60;/script&#62;",
        r#"href="/admin/mcp-servers/box/edit""#,
        r#"href="/admin/mcp-servers/box/remove""#,
        r#"href="/admin/mcp-servers/remote/edit""#,
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(!html.contains(XSS), "{html}");
    // The names come in order, and the toolset of `giles.toml` is listed but can't be changed.
    let at = |name: &str| {
        html.find(&format!(r#"<a href="/admin/mcp-servers/{name}">"#))
            .unwrap()
    };
    assert!(
        at("box") < at("files") && at("files") < at("remote"),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="detail">giles.toml</span>"#),
        "{html}"
    );
    assert!(html.contains("Edit it in giles.toml"), "{html}");
    assert!(!html.contains("/admin/mcp-servers/files/edit"), "{html}");
    assert!(!html.contains("/admin/mcp-servers/files/remove"), "{html}");
}

#[tokio::test]
async fn creating_a_toolset_stores_it_starts_it_and_gives_agents_a_box_for_it() {
    let env = env().await;
    write_secrets(&env, "box-token = \"s3cr3t-value\"\n");
    let t = env.toolsets("").await;
    let (status, html) = t.get("/admin/mcp-servers/new").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        r#"<form class="agent-form" method="post" action="/admin/mcp-servers">"#,
        &format!(r#"name="csrf" value="{CSRF}""#),
        r#"id="url" name="url""#,
        r#"<option value="none" selected>No authentication</option>"#,
        r#"<option value="secret">A secret in secrets.toml</option>"#,
        r#"name="bearer_secret""#,
        // Hidden until the URL is plain http.
        r#"<label class="tool" id="insecure-field" hidden><input type="checkbox" name="insecure"> Allow insecure connections</label>"#,
        r#"name="deadline""#,
        r#"name="parallel""#,
        r#"name="skills""#,
        "/static/mcp-server-form-",
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    // The form has nothing for a server Giles would run.
    for absent in [
        r#"name="transport""#,
        r#"name="command""#,
        r#"name="args""#,
        "env_name_",
        r#"name="network""#,
        r#"name="unsandboxed""#,
        r#"value="env""#,
    ] {
        assert!(!html.contains(absent), "{absent}\n{html}");
    }
    assert!(!t.agent_form(&env).await.contains("box__*"));

    let (status, _, location) = t
        .post(
            "/admin/mcp-servers",
            &[
                ("name", "box"),
                ("url", "http://mcp.example.com/mcp"),
                ("insecure", "on"),
                ("bearer", "secret"),
                ("bearer_secret", "box-token"),
                ("deadline", "90s"),
                ("parallel", "on"),
                ("skills", "/srv/skills\r\n\r\n"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/admin/mcp-servers/box"));
    assert_eq!(
        stored_spec(&env, "box").await,
        spec(
            r#"{ "url": "http://mcp.example.com/mcp", "insecure": true,
                 "auth": { "bearer": { "secret": "box-token" } },
                 "deadline": "90s", "parallel": true, "skills": ["/srv/skills"] }"#
        )
    );
    // It is running: in the live configuration, so the agent form offers its box at once.
    assert!(
        t.registry
            .config()
            .toolsets
            .keys()
            .any(|n| n.as_str() == "box")
    );
    assert!(t.agent_form(&env).await.contains("box__*"));

    // Without a token, only the URL is stored: a secret name typed with no secret chosen is left
    // out, and so is "Allow insecure connections" for a URL that isn't plain http.
    let (status, _, _) = t
        .post(
            "/admin/mcp-servers",
            &[
                ("name", "remote"),
                ("url", "https://mcp.example.com/mcp"),
                ("insecure", "on"),
                ("bearer", "none"),
                ("bearer_secret", "ignored"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        stored_spec(&env, "remote").await,
        spec(r#"{ "url": "https://mcp.example.com/mcp" }"#)
    );
}

#[tokio::test]
async fn a_server_giles_would_run_is_refused_at_its_key() {
    let env = env().await;
    let t = env.toolsets("").await;
    // A post shaped like a stdio server: the form has no such settings, so it has no URL.
    let (status, html, _) = t
        .post(
            "/admin/mcp-servers",
            &[
                ("name", "shell"),
                ("transport", "command"),
                ("command", "/bin/sh"),
                ("args", "-c\nid"),
                ("unsandboxed", "on"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        html.contains(r#"role="alert">url: a stored toolset is a remote server"#),
        "{html}"
    );
    assert!(html.contains("[toolset.&#60;name&#62;]"), "{html}");
    assert!(stored(&env).await.is_empty());
}

#[tokio::test]
async fn an_invalid_form_keeps_its_values_and_names_the_key() {
    let env = env().await;
    let t = env.toolsets("").await;
    // A secret that isn't in secrets.toml.
    let (status, html, _) = t
        .post(
            "/admin/mcp-servers",
            &[
                ("name", "box"),
                ("url", "https://mcp.example.com/mcp"),
                ("bearer", "secret"),
                ("bearer_secret", "no-such-secret"),
                ("deadline", "90s"),
                ("parallel", "on"),
                ("skills", "/keep/me\n/and/me"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    for expected in [
        r#"role="alert">auth.bearer: no secret"#,
        r#"id="name" name="name" value="box""#,
        r#"id="url" name="url" value="https://mcp.example.com/mcp""#,
        r#"<option value="secret" selected>"#,
        r#"name="bearer_secret" value="no-such-secret""#,
        r#"id="deadline" name="deadline" value="90s""#,
        r#"name="parallel" checked"#,
        "/keep/me\n/and/me</textarea>",
        &format!(r#"name="csrf" value="{CSRF}""#),
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    // A value that isn't a duration, a URL that isn't one, and plain http to another machine.
    for (field, value, error) in [
        ("deadline", "soon", "deadline: invalid duration"),
        ("url", "not a url", "url: "),
        ("url", "http://mcp.example.com/mcp", "url: "),
    ] {
        let mut form = remote_form("box");
        form.retain(|(key, _)| *key != field);
        form.push((field, value));
        let (status, html, _) = t.post("/admin/mcp-servers", &form).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}");
        assert!(html.contains(error), "{error}\n{html}");
        assert!(
            html.contains(&format!(r#"name="{field}" value="{value}""#)),
            "{html}"
        );
    }
    // Nothing was stored or started.
    assert!(stored(&env).await.is_empty());
    assert!(
        !t.registry
            .config()
            .toolsets
            .keys()
            .any(|n| n.as_str() == "box")
    );
}

#[tokio::test]
async fn a_taken_name_is_a_conflict_and_a_bad_name_is_not() {
    let env = env().await;
    let t = env.toolsets("").await;
    let (status, _, _) = t.post("/admin/mcp-servers", &remote_form("box")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    // A stored toolset has the name (ignoring case), and so does a toolset of `giles.toml`.
    for (name, error) in [
        ("box", "there is already a toolset with that name"),
        ("BOX", "there is already a toolset with that name"),
        ("files", "the name is taken by"),
    ] {
        let (status, html, _) = t.post("/admin/mcp-servers", &remote_form(name)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{name}");
        assert!(html.contains(error), "{error}\n{html}");
        assert!(
            html.contains(&format!(r#"id="name" name="name" value="{name}""#)),
            "{html}"
        );
    }
    // A name that can't be one is an invalid value; so is the address of the form.
    for name in ["", "two words", "new", "NEW"] {
        let (status, _, _) = t.post("/admin/mcp-servers", &remote_form(name)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name:?}");
    }
    assert_eq!(stored(&env).await, ["box"]);

    // The channel `web` is another kind of instance, so its name is free for a toolset.
    let (status, _, _) = t.post("/admin/mcp-servers", &remote_form("web")).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(stored(&env).await, ["box", "web"]);
}

#[tokio::test]
async fn editing_replaces_the_settings_and_a_stale_edit_is_refused() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager
        .add(
            "box",
            &spec(r#"{ "url": "http://127.0.0.1:9/v1", "deadline": "30m" }"#),
        )
        .await
        .unwrap();
    let loaded = updated_at(&t.manager.get("box").await.unwrap()).to_string();

    let (status, html) = t.get("/admin/mcp-servers/box/edit").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<h1>Edit box</h1>",
        r#"action="/admin/mcp-servers/box/edit""#,
        &format!(r#"name="updated_at" value="{loaded}""#),
        r#"name="url" value="http://127.0.0.1:9/v1""#,
        r#"name="deadline" value="30m""#,
        "<code>box</code>",
        // The URL is plain http, so the box shows.
        r#"<label class="tool" id="insecure-field"><input type="checkbox" name="insecure">"#,
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    // The name can't be changed here.
    assert!(!html.contains(r#"name="name""#), "{html}");

    async fn edit(t: &Toolsets, url: &str, updated: &str) -> (StatusCode, String, Option<String>) {
        t.post(
            "/admin/mcp-servers/box/edit",
            &[("updated_at", updated), ("url", url), ("deadline", "10m")],
        )
        .await
    }
    let (status, _, location) = edit(&t, "http://127.0.0.1:9/v2", &loaded).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/admin/mcp-servers/box"));
    let v2 = spec(r#"{ "url": "http://127.0.0.1:9/v2", "deadline": "10m" }"#);
    assert_eq!(stored_spec(&env, "box").await, v2);
    assert_ne!(
        updated_at(&t.manager.get("box").await.unwrap()).to_string(),
        loaded
    );

    // The form was loaded before that change, so this one is refused, with the values typed and
    // a way to reload.
    let (status, html, _) = edit(&t, "http://127.0.0.1:9/v3", &loaded).await;
    assert_eq!(status, StatusCode::CONFLICT);
    for expected in [
        "the toolset was changed since it was loaded",
        r#"<a href="/admin/mcp-servers/box/edit">Reload the current values</a>"#,
        r#"name="url" value="http://127.0.0.1:9/v3""#,
        &format!(r#"name="updated_at" value="{loaded}""#),
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert_eq!(stored_spec(&env, "box").await, v2);

    // An invalid edit is shown again with its key (400), and changes nothing.
    let now = updated_at(&t.manager.get("box").await.unwrap()).to_string();
    let (status, html, _) = t
        .post(
            "/admin/mcp-servers/box/edit",
            &[
                ("updated_at", now.as_str()),
                ("url", "http://127.0.0.1:9/v2"),
                ("deadline", "never"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(html.contains("deadline: invalid duration"), "{html}");
    assert!(html.contains(r#"name="deadline" value="never""#), "{html}");
    assert_eq!(stored_spec(&env, "box").await, v2);
    // A toolset that isn't there is a 404.
    let (status, _) = t.get("/admin/mcp-servers/nothing/edit").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_toolset_of_giles_toml_can_only_be_looked_at() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.set_plugins(vec![plugin("files", pb::PluginState::Running, 2, None)]);
    let (status, html) = t.get("/admin/mcp-servers/files").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("configured in giles.toml"), "{html}");
    assert!(html.contains("<dd>command</dd>"), "{html}");
    assert!(
        html.contains(r#"action="/admin/mcp-servers/files/restart""#),
        "{html}"
    );
    assert!(!html.contains("/admin/mcp-servers/files/edit"), "{html}");
    assert!(!html.contains("/admin/mcp-servers/files/remove"), "{html}");
    for path in [
        "/admin/mcp-servers/files/edit",
        "/admin/mcp-servers/files/remove",
    ] {
        let (status, html) = t.get(path).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}");
        assert!(html.contains("only be changed there"), "{html}");
    }
    for path in [
        "/admin/mcp-servers/files/edit",
        "/admin/mcp-servers/files/remove",
    ] {
        let (status, html, _) = t
            .post(path, &[("url", "https://mcp.example.com/mcp")])
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}");
        assert!(html.contains("only be changed there"), "{html}");
    }
    assert!(
        t.registry
            .config()
            .toolsets
            .keys()
            .any(|n| n.as_str() == "files")
    );
}

#[tokio::test]
async fn removing_lists_the_agents_that_mention_the_toolset_then_removes_it() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager.add("box", &spec(BOX)).await.unwrap();
    t.manager.add("other", &spec(BOX)).await.unwrap();
    assert!(t.agent_form(&env).await.contains("box__*"));

    let (status, html) = t.get("/admin/mcp-servers/box/remove").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<h1>Remove box?</h1>",
        r#"<a href="/admin/agents/wooster/edit">wooster</a>"#,
        "<code>box__*</code>",
        "<code>box__delete_*</code>",
        r#"action="/admin/mcp-servers/box/remove""#,
        &format!(r#"name="csrf" value="{CSRF}""#),
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(
        !html.contains("jeeves") && !html.contains("bunter"),
        "{html}"
    );
    // A toolset no agent mentions says so. Looking at the page removes nothing.
    let (_, html) = t.get("/admin/mcp-servers/other/remove").await;
    assert!(
        html.contains("No agent's tools or ask-first list mentions"),
        "{html}"
    );
    assert_eq!(stored(&env).await, ["box", "other"]);

    let (status, _, location) = t.post("/admin/mcp-servers/box/remove", &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/admin/mcp-servers"));
    assert_eq!(stored(&env).await, ["other"]);
    assert!(
        !t.registry
            .config()
            .toolsets
            .keys()
            .any(|n| n.as_str() == "box")
    );
    assert!(!t.agent_form(&env).await.contains("box__*"));
    // The agent's settings are left alone, and the toolset is gone.
    assert!(
        t.registry
            .config()
            .agents
            .values()
            .any(|agent| agent.tools.iter().any(|tool| tool == "box__*"))
    );
    let (status, _) = t.get("/admin/mcp-servers/box").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = t.post("/admin/mcp-servers/box/remove", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn values_are_escaped() {
    let env = env().await;
    let t = env.toolsets("").await;
    let escaped = "&#60;script&#62;alert(1)&#60;/script&#62;";
    // On the form that shows an error.
    let (status, html, _) = t
        .post(
            "/admin/mcp-servers",
            &[
                ("name", XSS),
                ("url", XSS),
                ("bearer", "secret"),
                ("bearer_secret", XSS),
                ("deadline", XSS),
                ("skills", XSS),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(html.contains(escaped), "{html}");
    assert!(!html.contains(XSS), "{html}");

    // On every page of a stored toolset.
    let spec = spec(&format!(
        r#"{{ "url": "http://127.0.0.1:9/mcp", "skills": ["/srv/{XSS}"] }}"#
    ));
    t.manager.add("box", &spec).await.unwrap();
    t.set_plugins(vec![plugin(
        "box",
        pb::PluginState::Failed,
        0,
        Some(&format!("broken {XSS}")),
    )]);
    t.registry.tools().set_toolset_tools(
        &InstanceName::new("box").unwrap(),
        vec![ToolSpec {
            name: "read".into(),
            description: format!("Reads {XSS}"),
            input_schema: serde_json::json!({ "type": "object" }),
        }],
    );
    for path in [
        "/admin/mcp-servers",
        "/admin/mcp-servers/box",
        "/admin/mcp-servers/box/edit",
        "/admin/mcp-servers/box/remove",
    ] {
        let (status, html) = t.get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(!html.contains(XSS), "{path}\n{html}");
        if path != "/admin/mcp-servers/box/remove" {
            assert!(html.contains(escaped), "{path}\n{html}");
        }
    }
}

#[tokio::test]
async fn a_secret_named_by_reference_never_appears_in_the_html() {
    let env = env().await;
    write_secrets(&env, "api = \"tok-9f8e7d\"\n");
    // A token stored as text, which only an older database can hold: it fails at startup.
    env.store
        .toolset_insert(
            "plain",
            r#"{ "url": "https://mcp.example.com/plain", "auth": { "bearer": "plain-bearer-777" } }"#,
            Timestamp::now(),
        )
        .await
        .unwrap();
    let t = env.toolsets("").await;
    let secrets = ["tok-9f8e7d", "plain-bearer-777"];
    let (status, _, _) = t
        .post(
            "/admin/mcp-servers",
            &[
                ("name", "remote"),
                ("url", "https://mcp.example.com/mcp"),
                ("bearer", "secret"),
                ("bearer_secret", "api"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let mut pages = vec!["/admin/mcp-servers".to_owned()];
    for name in ["remote", "plain"] {
        pages.push(format!("/admin/mcp-servers/{name}"));
        pages.push(format!("/admin/mcp-servers/{name}/edit"));
        pages.push(format!("/admin/mcp-servers/{name}/remove"));
    }
    for path in &pages {
        let (status, html) = t.get(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        for secret in secrets {
            assert!(!html.contains(secret), "{path} shows {secret}\n{html}");
        }
    }
    // The secret is shown by name.
    let (_, html) = t.get("/admin/mcp-servers/remote").await;
    assert!(html.contains("secret api"), "{html}");
    let (_, html) = t.get("/admin/mcp-servers/remote/edit").await;
    assert!(
        html.contains(r#"<option value="secret" selected>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"name="bearer_secret" value="api""#),
        "{html}"
    );

    // A form shown again after an error holds names, not values.
    let (status, html, _) = t
        .post(
            "/admin/mcp-servers",
            &[
                ("name", "again"),
                ("url", "http://mcp.example.com/plain"),
                ("bearer", "secret"),
                ("bearer_secret", "api"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        html.contains(r#"name="bearer_secret" value="api""#),
        "{html}"
    );
    for secret in secrets {
        assert!(!html.contains(secret), "{secret}\n{html}");
    }

    // Saving the toolset with the token as text with a secret instead starts it.
    let updated = updated_at(&t.manager.get("plain").await.unwrap()).to_string();
    let (status, _, _) = t
        .post(
            "/admin/mcp-servers/plain/edit",
            &[
                ("updated_at", updated.as_str()),
                ("url", "https://mcp.example.com/plain"),
                ("bearer", "secret"),
                ("bearer_secret", "api"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        stored_spec(&env, "plain").await,
        spec(
            r#"{ "url": "https://mcp.example.com/plain",
                 "auth": { "bearer": { "secret": "api" } } }"#
        )
    );
    assert_eq!(t.manager.get("plain").await.unwrap().failure, None);
}

#[tokio::test]
async fn the_toolset_page_shows_settings_state_and_tools_and_restarts() {
    let env = env().await;
    write_secrets(&env, "box-token = \"s3cr3t-value\"\n");
    let t = env.toolsets("").await;
    t.manager
        .add(
            "box",
            &spec(
                r#"{ "url": "http://127.0.0.1:9/mcp", "auth": { "bearer": { "secret": "box-token" } },
                     "deadline": "90s", "parallel": true, "skills": ["/srv/skills"] }"#,
            ),
        )
        .await
        .unwrap();
    t.set_plugins(vec![plugin("box", pb::PluginState::Running, 2, None)]);
    t.registry.tools().set_toolset_tools(
        &InstanceName::new("box").unwrap(),
        vec![
            ToolSpec {
                name: "read".into(),
                description: "Read a <b>file</b>.".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            },
            ToolSpec {
                name: "write".into(),
                description: "Write a file.".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            },
        ],
    );
    let (status, html) = t.get("/admin/mcp-servers/box").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<h1>box</h1>",
        r#"<dd class="state-ok">running</dd>"#,
        "<dd>remote URL</dd>",
        "<code>http://127.0.0.1:9/mcp</code>",
        "<code>secret box-token</code>",
        "<code>90s</code>",
        "<code>allowed</code>",
        "<code>/srv/skills</code>",
        "<dt>Added</dt>",
        "<dt>Changed</dt>",
        "<code>box__read</code>",
        "Read a &#60;b&#62;file&#60;/b&#62;.",
        "<code>box__write</code>",
        r#"href="/admin/mcp-servers/box/edit""#,
        r#"href="/admin/mcp-servers/box/remove""#,
        r#"action="/admin/mcp-servers/box/restart""#,
        &format!(r#"name="csrf" value="{CSRF}""#),
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(!html.contains("<b>file</b>"), "{html}");
    assert!(!html.contains("s3cr3t-value"), "{html}");

    // Restart asks the admin service, then comes back here.
    let (status, _, location) = t.post("/admin/mcp-servers/box/restart", &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/admin/mcp-servers/box"));
    assert_eq!(t.admin.restarted(), ["toolset/box"]);
    // The name may be in any case, and the service gets the toolset's own.
    let (status, _, location) = t.post("/admin/mcp-servers/BOX/restart", &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/admin/mcp-servers/box"));
    assert_eq!(t.admin.restarted(), ["toolset/box", "toolset/box"]);
    // A toolset that is gone is a 404 that the service never hears of.
    let (status, html, _) = t.post("/admin/mcp-servers/nothing/restart", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("No such MCP server"), "{html}");
    let (status, _) = t.get("/admin/mcp-servers/nothing").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(t.admin.restarted(), ["toolset/box", "toolset/box"]);
    // The service's refusal is shown, and a toolset the service doesn't supervise has no restart
    // button.
    t.set_plugins(Vec::new());
    let (status, html, _) = t.post("/admin/mcp-servers/box/restart", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("there is no plugin instance"), "{html}");
    let (_, html) = t.get("/admin/mcp-servers/box").await;
    assert!(!html.contains("/admin/mcp-servers/box/restart"), "{html}");
    assert!(html.contains("<code>box__read</code>"), "{html}");
}

#[tokio::test]
async fn restart_refuses_a_name_that_is_not_a_toolset() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager.add("box", &spec(BOX)).await.unwrap();
    // `web` is a channel instance that the service supervises, but not a toolset.
    t.set_plugins(vec![
        plugin("box", pb::PluginState::Running, 0, None),
        pb::PluginInstance {
            kind: pb::InstanceKind::Channel.into(),
            ..plugin("web", pb::PluginState::Running, 0, None)
        },
    ]);
    let (status, html, location) = t.post("/admin/mcp-servers/web/restart", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("No such MCP server"), "{html}");
    assert_eq!(location, None);
    assert!(t.admin.restarted().is_empty());
}

#[tokio::test]
async fn a_stored_toolset_that_failed_shows_why_and_can_be_fixed() {
    let env = env().await;
    // A secret the stored toolset needs isn't there when it starts, so it is failed.
    env.store
        .toolset_insert(
            "broken",
            r#"{ "url": "https://mcp.example.com/mcp", "auth": { "bearer": { "secret": "late" } } }"#,
            Timestamp::now(),
        )
        .await
        .unwrap();
    let t = env.toolsets("").await;
    let (status, html) = t.get("/admin/mcp-servers").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains(r#"<a href="/admin/mcp-servers/broken">broken</a>"#),
        "{html}"
    );
    assert!(html.contains("no secret"), "{html}");
    let (_, html) = t.get("/admin/mcp-servers/broken").await;
    assert!(html.contains("Last error"), "{html}");
    assert!(html.contains("secret late"), "{html}");

    // Saving the form with a way that works starts it.
    write_secrets(&env, "fixed = \"x\"\n");
    let updated = updated_at(&t.manager.get("broken").await.unwrap()).to_string();
    let (status, _, _) = t
        .post(
            "/admin/mcp-servers/broken/edit",
            &[
                ("updated_at", updated.as_str()),
                ("url", "https://mcp.example.com/mcp"),
                ("bearer", "secret"),
                ("bearer_secret", "fixed"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(
        t.registry
            .config()
            .toolsets
            .keys()
            .any(|n| n.as_str() == "broken")
    );
    let view = t.manager.get("broken").await.unwrap();
    assert_eq!(view.failure, None);
}

#[tokio::test]
async fn a_stored_spec_that_cannot_be_read_gets_an_empty_form_and_can_be_removed() {
    let env = env().await;
    env.store
        .toolset_insert("odd", r#"{ "deadline": 5 }"#, Timestamp::now())
        .await
        .unwrap();
    let t = env.toolsets("").await;
    let (status, html) = t.get("/admin/mcp-servers/odd/edit").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("the stored spec can"), "{html}");
    assert!(html.contains("Fill in the form to replace it."), "{html}");
    let (status, html) = t.get("/admin/mcp-servers/odd").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("the stored spec can"), "{html}");
    let (status, _, _) = t.post("/admin/mcp-servers/odd/remove", &[]).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(stored(&env).await.is_empty());
}

/// The text of the test agent `wooster`'s `agent.toml`.
fn wooster(env: &Env) -> String {
    std::fs::read_to_string(env.root().agents_dir().join("wooster").join("agent.toml")).unwrap()
}

#[tokio::test]
async fn renaming_previews_the_agents_whose_globs_change_then_renames() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager.add("box", &spec(BOX)).await.unwrap();
    let before = wooster(&env);

    // The page links to the form, which asks for the name and says what happens to sessions.
    let (_, html) = t.get("/admin/mcp-servers/box").await;
    assert!(
        html.contains(r#"href="/admin/mcp-servers/box/rename""#),
        "{html}"
    );
    let (status, html) = t.get("/admin/mcp-servers/box/rename").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<h1>Rename box</h1>",
        r#"name="new""#,
        "Running conversations keep the old tool names",
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(!html.contains("What changes"), "{html}");

    // With a name, the agents whose globs change, and the button. Nothing changes yet.
    let (status, html) = t.get("/admin/mcp-servers/box/rename?new=chest").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "What changes",
        r#"<a href="/admin/agents/wooster/edit">wooster</a>"#,
        "<code>box__*</code>",
        "<code>chest__*</code>",
        "<code>box__delete_*</code>",
        "<code>chest__delete_*</code>",
        r#"action="/admin/mcp-servers/box/rename""#,
        r#"name="new" value="chest""#,
        &format!(r#"name="csrf" value="{CSRF}""#),
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(
        !html.contains("jeeves") && !html.contains("bunter"),
        "{html}"
    );
    assert_eq!(stored(&env).await, ["box"]);
    assert_eq!(wooster(&env), before);

    // A toolset no agent mentions says so.
    t.manager.add("other", &spec(BOX)).await.unwrap();
    let (status, html) = t.get("/admin/mcp-servers/other/rename?new=lone").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("so no agent changes"), "{html}");

    // Posting renames, and the page is now at the new name.
    let (status, _, location) = t
        .post("/admin/mcp-servers/box/rename", &[("new", "chest")])
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.as_deref(), Some("/admin/mcp-servers/chest"));
    assert_eq!(stored(&env).await, ["chest", "other"]);
    let after = wooster(&env);
    assert!(
        after.contains("chest__*") && after.contains("chest__delete_*"),
        "{after}"
    );
    assert!(!after.contains("box__"), "{after}");
    let (status, _) = t.get("/admin/mcp-servers/chest").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = t.get("/admin/mcp-servers/box").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = t.get("/admin/mcp-servers/box/rename").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = t
        .post("/admin/mcp-servers/box/rename", &[("new", "x")])
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_glob_that_would_stop_matching_refuses_the_rename_and_changes_nothing() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager.add("box", &spec(BOX)).await.unwrap();
    let file = env.root().agents_dir().join("wooster").join("agent.toml");
    std::fs::write(
        &file,
        "models = [\"fake/test-model\"]\ntools = [\"memory_add\", \"box*\"]\n",
    )
    .unwrap();
    let before = wooster(&env);
    let previewed = t.get("/admin/mcp-servers/box/rename?new=chest").await;
    let (status, html, _) = t
        .post("/admin/mcp-servers/box/rename", &[("new", "chest")])
        .await;
    for (status, html) in [previewed, (status, html)] {
        assert_eq!(status, StatusCode::BAD_REQUEST, "{html}");
        // The manager's reason names the agent and the glob, and the form keeps the name.
        for expected in ["box*", "wooster", r#"name="new" value="chest""#] {
            assert!(html.contains(expected), "{expected}\n{html}");
        }
        assert!(!html.contains("What changes"), "{html}");
    }
    assert_eq!(stored(&env).await, ["box"]);
    assert_eq!(wooster(&env), before);
}

/// A file the manager can't read isn't the owner's name or glob to fix: the check fails, the page
/// says which step and why with a 500, and nothing changed. Undoing a step that failed is shown by
/// a unit test of the page in `toolsets.rs`, because it can't be provoked from here.
#[tokio::test]
async fn an_agent_file_that_cannot_be_read_fails_the_rename_and_changes_nothing() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager.add("box", &spec(BOX)).await.unwrap();
    let file = env.root().agents_dir().join("wooster").join("agent.toml");
    let before = wooster(&env);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&file).is_ok() {
        // Running as root: permissions don't stop the read, so this failure can't be provoked.
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        return;
    }
    let previewed = t.get("/admin/mcp-servers/box/rename?new=chest").await;
    let (status, html, _) = t
        .post("/admin/mcp-servers/box/rename", &[("new", "chest")])
        .await;
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

    for (status, html) in [previewed, (status, html)] {
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{html}");
        for expected in [
            "Renaming can't go ahead",
            "reading the agent `wooster`",
            "Permission denied",
            "Nothing was changed.",
            r#"name="new" value="chest""#,
        ] {
            assert!(html.contains(expected), "{expected}\n{html}");
        }
        assert!(!html.contains("What changes"), "{html}");
        assert!(!html.contains("partly done"), "{html}");
    }
    assert_eq!(stored(&env).await, ["box"]);
    assert_eq!(wooster(&env), before);
}

#[tokio::test]
async fn a_name_that_cannot_be_used_comes_back_on_the_form() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager.add("box", &spec(BOX)).await.unwrap();
    t.manager.add("chest", &spec(BOX)).await.unwrap();
    let before = wooster(&env);

    // Taken by another stored toolset, or by one of giles.toml: 409.
    for taken in ["chest", "CHEST", "files"] {
        let (status, html, _) = t
            .post("/admin/mcp-servers/box/rename", &[("new", taken)])
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{taken}\n{html}");
        assert!(
            html.contains(&format!(r#"name="new" value="{taken}""#)),
            "{html}"
        );
        let (status, _) = t
            .get(&format!("/admin/mcp-servers/box/rename?new={taken}"))
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{taken}");
    }
    // Not a name: 400.
    let (status, _, _) = t
        .post("/admin/mcp-servers/box/rename", &[("new", "has space")])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // What was typed is escaped, in the form and in the reason.
    let (status, html, _) = t
        .post("/admin/mcp-servers/box/rename", &[("new", XSS)])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        html.contains("&#60;script&#62;alert(1)&#60;/script&#62;"),
        "{html}"
    );
    assert!(!html.contains(XSS), "{html}");
    let query: String = url::form_urlencoded::byte_serialize(XSS.as_bytes()).collect();
    let (status, html) = t
        .get(&format!("/admin/mcp-servers/box/rename?new={query}"))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!html.contains(XSS), "{html}");
    // No name at all is asked for again.
    let (status, _, _) = t
        .post("/admin/mcp-servers/box/rename", &[("new", " ")])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(stored(&env).await, ["box", "chest"]);
    assert_eq!(wooster(&env), before);
}

#[tokio::test]
async fn a_toolset_of_giles_toml_cannot_be_renamed() {
    let env = env().await;
    let t = env.toolsets("").await;
    t.manager.add("box", &spec(BOX)).await.unwrap();
    let (_, html) = t.get("/admin/mcp-servers/files").await;
    assert!(!html.contains("/admin/mcp-servers/files/rename"), "{html}");
    let (status, html) = t.get("/admin/mcp-servers/files/rename?new=chest").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(html.contains("only be changed there"), "{html}");
    let (status, html, _) = t
        .post("/admin/mcp-servers/files/rename", &[("new", "chest")])
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(html.contains("only be changed there"), "{html}");
    assert_eq!(stored(&env).await, ["box"]);
    assert!(
        t.registry
            .config()
            .toolsets
            .keys()
            .any(|n| n.as_str() == "files")
    );
}
