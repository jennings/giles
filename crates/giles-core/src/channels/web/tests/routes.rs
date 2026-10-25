//! Router-level tests of the route pages over the test configuration: the channel
//! `web` and the agents `jeeves` and `bunter`.

use giles_store::NewRoute;
use giles_types::{ConversationKind, ConversationRef};

use crate::config::{Config, LiveConfig, LoadOptions};
use crate::routes::{RouteList, Routes};

use super::agents::guest;
use super::*;

const XSS: &str = "<script>alert(1)</script>";

impl Env {
    async fn routes(&self) -> Arc<Routes> {
        Arc::new(
            Routes::open(self.store.clone(), self.registry.live_config().clone())
                .await
                .unwrap(),
        )
    }

    fn routes_router(&self, routes: &Arc<Routes>, user: Option<WebUser>) -> axum::Router {
        let app = self
            .app
            .install_routes(self.app.install(RouterBuilder::new()), routes.clone())
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    /// `method path` as the owner, with the CSRF token in the form if `form` is given. Every
    /// answer is `no-store`. The status, body, and `Location`.
    async fn route_call(
        &self,
        routes: &Arc<Routes>,
        method: Method,
        path: &str,
        form: Option<&str>,
    ) -> (StatusCode, String, Option<String>) {
        let form = form.map(|form| format!("csrf={CSRF}&{form}"));
        let response = call(
            self.routes_router(routes, Some(owner())),
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
}

/// The stored routes as `channel/kind/conversation>agent`, `*` for any, in order.
async fn stored(env: &Env) -> Vec<String> {
    env.store
        .routes()
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            let r = r.route;
            format!(
                "{}/{}/{}>{}",
                r.channel.as_deref().unwrap_or("*"),
                match r.kind {
                    Some(ConversationKind::Direct) => "direct",
                    Some(_) => "group",
                    None => "*",
                },
                r.conversation.as_deref().unwrap_or("*"),
                r.agent
            )
        })
        .collect()
}

async fn id_of(env: &Env, agent: &str) -> i64 {
    env.store
        .routes()
        .await
        .unwrap()
        .iter()
        .find(|r| r.route.agent.as_str() == agent)
        .unwrap()
        .id
        .get()
}

/// The agent the router's list picks for a direct conversation.
fn picked(list: &RouteList, instance: &str, conversation: &str) -> Option<String> {
    let instance = InstanceName::new(instance).unwrap();
    let conversation = ConversationRef {
        id: ConversationId::new(conversation).unwrap(),
        kind: ConversationKind::Direct,
        title: None,
    };
    list.get()
        .iter()
        .find(|route| route.matches(&instance, &conversation))
        .map(|route| route.agent.to_string())
}

fn route_form(channel: &str, conversation: &str, agent: &str) -> String {
    format!("channel={channel}&kind=&conversation={conversation}&agent={agent}&before=")
}

#[tokio::test]
async fn only_the_owner_gets_the_route_pages() {
    let env = env().await;
    let routes = env.routes().await;
    routes
        .add(
            &crate::routes::RouteSpec {
                agent: "jeeves".into(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let id = id_of(&env, "jeeves").await;
    let cases = [
        (Method::GET, "/admin/routes".to_owned()),
        (Method::GET, "/admin/routes/new".to_owned()),
        (Method::GET, format!("/admin/routes/{id}/edit")),
        (Method::POST, "/admin/routes".to_owned()),
        (Method::POST, format!("/admin/routes/{id}")),
        (Method::POST, format!("/admin/routes/{id}/remove")),
        (Method::POST, format!("/admin/routes/{id}/move")),
    ];
    for (method, path) in cases {
        for (user, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(guest()), StatusCode::FORBIDDEN),
        ] {
            let response = call(
                env.routes_router(&routes, user),
                method.clone(),
                &path,
                &[("x-csrf-token", CSRF)],
                Some(&format!("csrf={CSRF}&agent=bunter&direction=down")),
            )
            .await;
            assert_eq!(response.status(), expected, "{method} {path}");
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        }
    }
    assert_eq!(stored(&env).await, ["*/*/*>jeeves"]);
}

#[tokio::test]
async fn every_change_needs_the_csrf_token() {
    let env = env().await;
    let routes = env.routes().await;
    routes
        .add(
            &crate::routes::RouteSpec {
                agent: "jeeves".into(),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let id = id_of(&env, "jeeves").await;
    let posts = [
        "/admin/routes".to_owned(),
        format!("/admin/routes/{id}"),
        format!("/admin/routes/{id}/remove"),
        format!("/admin/routes/{id}/move"),
    ];
    for path in posts {
        let form = "channel=&kind=&conversation=%23x&agent=bunter&before=&direction=down";
        for (headers, body) in [
            // No token, a wrong token in the form, and a wrong one in the header.
            (vec![], form.to_owned()),
            (vec![], format!("csrf=wrong&{form}")),
            (
                vec![("x-csrf-token", "wrong")],
                format!("csrf={CSRF}&{form}"),
            ),
        ] {
            let response = call(
                env.routes_router(&routes, Some(owner())),
                Method::POST,
                &path,
                &headers,
                Some(&body),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        }
    }
    assert_eq!(stored(&env).await, ["*/*/*>jeeves"]);
    // The token may come in the header instead of the form, like the other pages.
    let response = call(
        env.routes_router(&routes, Some(owner())),
        Method::POST,
        &format!("/admin/routes/{id}/remove"),
        &[("x-csrf-token", CSRF)],
        Some(""),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert!(stored(&env).await.is_empty());
}

#[tokio::test]
async fn the_list_shows_routes_in_order_escaped_with_links_and_buttons() {
    let env = env().await;
    let routes = env.routes().await;
    let (status, html, _) = env
        .route_call(&routes, Method::GET, "/admin/routes", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        html.contains("No routes, so every message is dropped"),
        "{html}"
    );
    assert!(
        html.contains(r#"<a href="/admin/routes">Routes</a>"#),
        "{html}"
    );

    for (channel, conversation, agent) in [("web", XSS, "jeeves"), ("", "", "bunter")] {
        let spec = crate::routes::RouteSpec {
            channel: channel.into(),
            conversation: conversation.into(),
            agent: agent.into(),
            ..Default::default()
        };
        routes.add(&spec, None).await.unwrap();
    }
    let (_, html, _) = env
        .route_call(&routes, Method::GET, "/admin/routes", None)
        .await;
    let first = html
        .find("route-1")
        .and_then(|_| html.find("Jeeves (jeeves)"))
        .unwrap();
    let second = html
        .find(r#"<a href="/admin/agents/bunter">bunter</a>"#)
        .unwrap();
    assert!(first < second, "{html}");
    for expected in [
        "<code>web</code>",
        "<code>&#60;script&#62;alert(1)&#60;/script&#62;</code>",
        r#"<a href="/admin/agents/jeeves">Jeeves (jeeves)</a>"#,
        r#"<span class="detail">any</span>"#,
        r#"action="/admin/routes/1/move""#,
        r#"action="/admin/routes/2/remove""#,
        &format!(r#"name="csrf" value="{CSRF}""#),
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert!(!html.contains(XSS), "{html}");
    // The first route can't move up and the last can't move down.
    assert!(!html.contains(r#"aria-label="Move route 1 up""#), "{html}");
    assert!(html.contains(r#"aria-label="Move route 1 down""#), "{html}");
    assert!(html.contains(r#"aria-label="Move route 2 up""#), "{html}");
    assert!(
        !html.contains(r#"aria-label="Move route 2 down""#),
        "{html}"
    );
}

#[tokio::test]
async fn a_route_that_never_matches_or_points_at_something_gone_is_flagged() {
    let env = env().await;
    let routes = env.routes().await;
    for (channel, agent) in [(None, "jeeves"), (Some("oftc"), "wooster")] {
        env.store
            .route_insert(
                usize::MAX,
                NewRoute {
                    channel: channel.map(str::to_owned),
                    kind: None,
                    conversation: None,
                    agent: giles_types::AgentId::new(agent).unwrap(),
                },
            )
            .await
            .unwrap();
    }
    let (_, html, _) = env
        .route_call(&routes, Method::GET, "/admin/routes", None)
        .await;
    for expected in [
        "Never used: route 1 (every conversation) would always be tried first",
        "There is no channel instance oftc.",
        "There is no agent wooster.",
        "<code>wooster</code>",
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
}

#[tokio::test]
async fn giles_toml_routes_that_are_still_there_are_reported() {
    let env = env().await;
    let path = env.registry.root().config_file();
    let toml = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{toml}[[routes]]\nagent = \"jeeves\"\n")).unwrap();
    let config = Config::load_with(
        env.registry.root(),
        &LoadOptions {
            env: &|_| None,
            giles_exe: None,
            ..LoadOptions::default()
        },
    )
    .unwrap();
    let routes = Arc::new(
        Routes::open(env.store.clone(), LiveConfig::new(Arc::new(config)))
            .await
            .unwrap(),
    );
    let (_, html, _) = env
        .route_call(&routes, Method::GET, "/admin/routes", None)
        .await;
    assert!(
        html.contains("giles.toml still has 1 <code>[[routes]]</code> rule"),
        "{html}"
    );
    // It was imported once, so the list shows it.
    assert_eq!(stored(&env).await, ["*/*/*>jeeves"]);
}

#[tokio::test]
async fn adding_a_route_stores_it_and_the_router_uses_it_at_once() {
    let env = env().await;
    let routes = env.routes().await;
    let list = routes.list();
    let (status, html, _) = env
        .route_call(&routes, Method::GET, "/admin/routes/new", None)
        .await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        r#"<option value="web">web</option>"#,
        r#"<option value="bunter">bunter</option>"#,
        r#"<option value="" selected>At the end, after every route</option>"#,
        r#"action="/admin/routes""#,
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    assert_eq!(picked(&list, "web", "chat-1"), None);

    let (status, _, location) = env
        .route_call(
            &routes,
            Method::POST,
            "/admin/routes",
            Some(&route_form("", "", "jeeves")),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(location.unwrap().starts_with("/admin/routes#route-"));
    assert_eq!(stored(&env).await, ["*/*/*>jeeves"]);
    assert_eq!(picked(&list, "web", "chat-1").as_deref(), Some("jeeves"));

    // A specific route before the first one.
    let before = id_of(&env, "jeeves").await;
    let (status, _, _) = env
        .route_call(
            &routes,
            Method::POST,
            "/admin/routes",
            Some(&format!(
                "channel=web&kind=direct&conversation=chat-1&agent=bunter&before={before}"
            )),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(
        stored(&env).await,
        ["web/direct/chat-1>bunter", "*/*/*>jeeves"]
    );
    assert_eq!(picked(&list, "web", "chat-1").as_deref(), Some("bunter"));
    assert_eq!(picked(&list, "web", "chat-2").as_deref(), Some("jeeves"));
}

#[tokio::test]
async fn an_invalid_route_is_shown_again_with_the_problem_and_nothing_is_stored() {
    let env = env().await;
    let routes = env.routes().await;
    let cases = [
        (
            route_form("irc", "", "jeeves"),
            StatusCode::BAD_REQUEST,
            "no channel instance `irc`",
        ),
        (
            route_form("", "", "wooster"),
            StatusCode::BAD_REQUEST,
            "no agent `wooster`",
        ),
        (
            route_form("", "", ""),
            StatusCode::BAD_REQUEST,
            "choose an agent",
        ),
        (
            "channel=&kind=dm&conversation=&agent=jeeves&before=".to_owned(),
            StatusCode::BAD_REQUEST,
            "unknown kind",
        ),
        (
            route_form("", "x%00y", "jeeves"),
            StatusCode::BAD_REQUEST,
            "control characters",
        ),
        (
            format!("channel=&kind=&conversation={XSS}&agent=wooster&before="),
            StatusCode::BAD_REQUEST,
            "no agent `wooster`",
        ),
    ];
    for (form, want_status, want) in cases {
        let (status, html, _) = env
            .route_call(&routes, Method::POST, "/admin/routes", Some(&form))
            .await;
        assert_eq!(status, want_status, "{form}");
        assert!(html.contains(want), "{want}\n{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
        // Nothing the owner typed comes back unescaped.
        assert!(!html.contains(XSS), "{html}");
    }
    // The form comes back with what was typed.
    let (_, html, _) = env
        .route_call(
            &routes,
            Method::POST,
            "/admin/routes",
            Some(&route_form("web", "%23giles", "wooster")),
        )
        .await;
    assert!(html.contains(r##"value="#giles""##), "{html}");
    assert!(
        html.contains(r#"<option value="web" selected>web</option>"#),
        "{html}"
    );
    assert!(stored(&env).await.is_empty());
}

#[tokio::test]
async fn a_duplicate_or_hidden_route_is_refused_with_a_conflict() {
    let env = env().await;
    let routes = env.routes().await;
    let post = |form: String| {
        let (env, routes) = (&env, routes.clone());
        async move {
            env.route_call(&routes, Method::POST, "/admin/routes", Some(&form))
                .await
        }
    };
    assert_eq!(
        post(route_form("web", "%23a", "jeeves")).await.0,
        StatusCode::SEE_OTHER
    );

    let (status, html, _) = post(route_form("web", "%23a", "bunter")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        html.contains("route 1 already matches channel web, conversation #a"),
        "{html}"
    );

    assert_eq!(
        post(route_form("web", "", "bunter")).await.0,
        StatusCode::SEE_OTHER
    );
    let (status, html, _) = post(route_form("web", "%23b", "jeeves")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        html.contains("could never be used: route 2 (channel web) would always be tried first"),
        "{html}"
    );

    // A route that would hide route 1.
    let first = id_of(&env, "jeeves").await;
    let (status, html, _) = post(format!(
        "channel=&kind=&conversation=&agent=bunter&before={first}"
    ))
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        html.contains("would leave route 2 (channel web, conversation #a) unusable"),
        "{html}"
    );
    assert_eq!(stored(&env).await, ["web/*/#a>jeeves", "web/*/*>bunter"]);
}

#[tokio::test]
async fn editing_a_route_shows_its_values_and_saves_them_in_place() {
    let env = env().await;
    let routes = env.routes().await;
    let list = routes.list();
    for (conversation, agent) in [("%23a", "jeeves"), ("", "bunter")] {
        env.route_call(
            &routes,
            Method::POST,
            "/admin/routes",
            Some(&route_form("web", conversation, agent)),
        )
        .await;
    }
    let first = id_of(&env, "jeeves").await;
    let (status, html, _) = env
        .route_call(
            &routes,
            Method::GET,
            &format!("/admin/routes/{first}/edit"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "<h1>Edit route</h1>",
        r##"value="#a""##,
        r#"<option value="web" selected>web</option>"#,
        r#"<option value="jeeves" selected>Jeeves (jeeves)</option>"#,
        &format!(r#"action="/admin/routes/{first}""#),
    ] {
        assert!(html.contains(expected), "{expected}\n{html}");
    }
    // Editing doesn't offer a position.
    assert!(!html.contains("After every route"), "{html}");

    assert_eq!(picked(&list, "web", "#a").as_deref(), Some("jeeves"));
    let (status, _, location) = env
        .route_call(
            &routes,
            Method::POST,
            &format!("/admin/routes/{first}"),
            Some(&route_form("web", "%23a", "bunter")),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.unwrap(), format!("/admin/routes#route-{first}"));
    assert_eq!(stored(&env).await, ["web/*/#a>bunter", "web/*/*>bunter"]);
    assert_eq!(picked(&list, "web", "#a").as_deref(), Some("bunter"));

    // A bad edit is shown again on the edit form, and changes nothing.
    let (status, html, _) = env
        .route_call(
            &routes,
            Method::POST,
            &format!("/admin/routes/{first}"),
            Some(&route_form("web", "%23a", "wooster")),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        html.contains("<h1>Edit route</h1>") && html.contains("no agent `wooster`"),
        "{html}"
    );
    assert!(
        html.contains(r#"<option value="wooster" selected>wooster (no such agent)</option>"#),
        "{html}"
    );
    assert_eq!(stored(&env).await, ["web/*/#a>bunter", "web/*/*>bunter"]);
}

#[tokio::test]
async fn removing_a_route_stops_the_router_using_it() {
    let env = env().await;
    let routes = env.routes().await;
    let list = routes.list();
    for (conversation, agent) in [("%23a", "jeeves"), ("", "bunter")] {
        env.route_call(
            &routes,
            Method::POST,
            "/admin/routes",
            Some(&route_form("", conversation, agent)),
        )
        .await;
    }
    assert_eq!(picked(&list, "web", "#a").as_deref(), Some("jeeves"));
    let first = id_of(&env, "jeeves").await;

    // Removing is a post: a link can't do it.
    let (status, _, _) = env
        .route_call(
            &routes,
            Method::GET,
            &format!("/admin/routes/{first}/remove"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    let (status, _, location) = env
        .route_call(
            &routes,
            Method::POST,
            &format!("/admin/routes/{first}/remove"),
            Some(""),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.unwrap(), "/admin/routes");
    assert_eq!(stored(&env).await, ["*/*/*>bunter"]);
    assert_eq!(picked(&list, "web", "#a").as_deref(), Some("bunter"));

    // Again: it's gone.
    let (status, html, _) = env
        .route_call(
            &routes,
            Method::POST,
            &format!("/admin/routes/{first}/remove"),
            Some(""),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(html.contains("no longer exists"), "{html}");
}

#[tokio::test]
async fn moving_a_route_changes_which_one_wins() {
    let env = env().await;
    let routes = env.routes().await;
    let list = routes.list();
    // Both match #a on web; neither hides the other.
    for (channel, conversation, agent) in [("web", "", "jeeves"), ("", "%23a", "bunter")] {
        env.route_call(
            &routes,
            Method::POST,
            "/admin/routes",
            Some(&route_form(channel, conversation, agent)),
        )
        .await;
    }
    let (jeeves, bunter) = (id_of(&env, "jeeves").await, id_of(&env, "bunter").await);
    assert_eq!(picked(&list, "web", "#a").as_deref(), Some("jeeves"));

    let (status, _, location) = env
        .route_call(
            &routes,
            Method::POST,
            &format!("/admin/routes/{bunter}/move"),
            Some("direction=up"),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(location.unwrap(), format!("/admin/routes#route-{bunter}"));
    assert_eq!(stored(&env).await, ["*/*/#a>bunter", "web/*/*>jeeves"]);
    assert_eq!(picked(&list, "web", "#a").as_deref(), Some("bunter"));
    assert_eq!(picked(&list, "web", "#b").as_deref(), Some("jeeves"));

    let (status, _, _) = env
        .route_call(
            &routes,
            Method::POST,
            &format!("/admin/routes/{bunter}/move"),
            Some("direction=down"),
        )
        .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(stored(&env).await, ["web/*/*>jeeves", "*/*/#a>bunter"]);

    let (status, _, _) = env
        .route_call(
            &routes,
            Method::POST,
            &format!("/admin/routes/{jeeves}/move"),
            Some("direction=sideways"),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_move_that_would_hide_a_route_is_refused() {
    let env = env().await;
    let routes = env.routes().await;
    for (conversation, agent) in [("%23a", "jeeves"), ("", "bunter")] {
        env.route_call(
            &routes,
            Method::POST,
            "/admin/routes",
            Some(&route_form("", conversation, agent)),
        )
        .await;
    }
    let catch_all = id_of(&env, "bunter").await;
    let (status, html, _) = env
        .route_call(
            &routes,
            Method::POST,
            &format!("/admin/routes/{catch_all}/move"),
            Some("direction=up"),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        html.contains("would leave route 2 (conversation #a) unusable"),
        "{html}"
    );
    assert_eq!(stored(&env).await, ["*/*/#a>jeeves", "*/*/*>bunter"]);
}

#[tokio::test]
async fn ids_that_are_not_routes_are_a_404() {
    let env = env().await;
    let routes = env.routes().await;
    for (method, path) in [
        (Method::GET, "/admin/routes/abc/edit"),
        (Method::GET, "/admin/routes/99/edit"),
        (Method::POST, "/admin/routes/abc"),
        (Method::POST, "/admin/routes/99"),
        (Method::POST, "/admin/routes/abc/remove"),
        (Method::POST, "/admin/routes/99/move"),
    ] {
        let (status, _, _) = env
            .route_call(
                &routes,
                method.clone(),
                path,
                Some(&format!("{}&direction=up", route_form("", "", "jeeves"))),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
    }
    let (status, _, _) = env
        .route_call(
            &routes,
            Method::POST,
            "/admin/routes",
            Some("channel=&kind=&conversation=&agent=jeeves&before=77"),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(stored(&env).await.is_empty());
}
