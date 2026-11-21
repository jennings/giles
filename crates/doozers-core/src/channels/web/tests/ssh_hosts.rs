//! Router-level tests of the Known SSH hosts page (`/admin/ssh-hosts`) over the test
//! configuration (the environments `box` and `far`) and a real store.

use doozers_store::{KnownHostId, NewKnownHost};
use doozers_types::Timestamp;
use ssh_key::{Algorithm, HashAlg, PrivateKey};

use super::agents::guest;
use super::ssh_keys::url_encode;
use super::*;
use crate::ssh::KnownHosts;

const ENVIRONMENTS: &str = "[environment.box]\ncomponent = \"docker\"\n\
    address = \"http://127.0.0.1:9000\"\n\
    [environment.far]\ncomponent = \"docker\"\naddress = \"http://127.0.0.1:9001\"\n";

/// What a request got back.
struct Answer {
    status: StatusCode,
    body: String,
    location: Option<String>,
}

struct Hosts {
    env: Env,
    hosts: KnownHosts,
}

/// A fresh Ed25519 key: its `known_hosts` fields (`ssh-ed25519 AAAA...`) and fingerprint.
struct Key {
    public: String,
    fingerprint: String,
}

impl Key {
    fn new() -> Self {
        let private =
            PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519).unwrap();
        let public = private.public_key();
        Self {
            public: public.to_openssh().unwrap(),
            fingerprint: public.fingerprint(HashAlg::Sha256).to_string(),
        }
    }

    /// `<hosts> ssh-ed25519 AAAA...`.
    fn line(&self, hosts: &str) -> String {
        format!("{hosts} {}", self.public)
    }

    /// The base64 field.
    fn blob(&self) -> &str {
        self.public.split(' ').nth(1).unwrap()
    }
}

impl Hosts {
    async fn new() -> Self {
        Self::with(ENVIRONMENTS).await
    }

    async fn with(environments: &str) -> Self {
        let env = env_with_toml(WebConfig::default(), &["web:owner"], environments).await;
        let clock: Arc<dyn crate::sessions::Clock> = Arc::new(SystemClock);
        let hosts = KnownHosts::new(env.store.clone(), clock);
        Self { env, hosts }
    }

    fn router(&self, user: Option<WebUser>) -> axum::Router {
        let app = self
            .env
            .app
            .install_ssh_hosts(
                self.env.app.install(RouterBuilder::new()),
                self.hosts.clone(),
            )
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    /// `method path` as `user`, with `form` (URL-encoded) as the body if given. Every answer must
    /// be `no-store`.
    async fn send(&self, user: &WebUser, method: Method, path: &str, form: Option<&str>) -> Answer {
        let response = call(self.router(Some(user.clone())), method, path, &[], form).await;
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{path}");
        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .map(|l| l.to_str().unwrap().to_owned());
        Answer {
            status,
            body: body_text(response).await,
            location,
        }
    }

    async fn get(&self, user: &WebUser, path: &str) -> Answer {
        self.send(user, Method::GET, path, None).await
    }

    /// A POST of `pairs` and the user's CSRF token, URL-encoded.
    async fn post(&self, user: &WebUser, path: &str, pairs: &[(&str, &str)]) -> Answer {
        let mut form = format!("csrf={}", user.csrf);
        for (name, value) in pairs {
            form.push_str(&format!("&{name}={}", url_encode(value)));
        }
        self.send(user, Method::POST, path, Some(&form)).await
    }

    /// The owner pins `line` for `environment` through the form.
    async fn add(&self, environment: &str, line: &str) -> Answer {
        self.post(
            &owner(),
            "/admin/ssh-hosts/add",
            &[("environment", environment), ("line", line)],
        )
        .await
    }

    /// Records a key as a workspace's first connection does.
    async fn record(&self, environment: &str, host: &str, port: u16, key: &Key) -> KnownHostId {
        let new = NewKnownHost {
            environment: environment.to_owned(),
            host: host.to_owned(),
            port,
            key_type: "ssh-ed25519".to_owned(),
            public_key: key.blob().to_owned(),
            fingerprint: key.fingerprint.clone(),
            first_workspace: Some("wk-1".to_owned()),
            first_agent: Some("jeeves".to_owned()),
            first_user: Some("owner".to_owned()),
            added_by: None,
        };
        self.env
            .store
            .known_host_record_first_use(new, Timestamp::now())
            .await
            .unwrap()
            .unwrap()
            .id
    }

    async fn rows(&self) -> Vec<doozers_store::KnownHostRecord> {
        self.env.store.known_hosts_all().await.unwrap()
    }
}

#[tokio::test]
async fn every_route_needs_a_login_and_an_admin() {
    let hosts = Hosts::new().await;
    let id = hosts.record("box", "github.com", 22, &Key::new()).await;
    let routes = [
        (Method::GET, "/admin/ssh-hosts".to_owned()),
        (Method::POST, "/admin/ssh-hosts/add".to_owned()),
        (Method::GET, format!("/admin/ssh-hosts/{id}/delete")),
        (Method::POST, format!("/admin/ssh-hosts/{id}/delete")),
    ];
    for (method, path) in &routes {
        let form = (*method == Method::POST).then(|| format!("csrf={CSRF}"));
        let response = call(
            hosts.router(None),
            method.clone(),
            path,
            &[],
            form.as_deref(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
    }
    let key = Key::new();
    let line = key.line("example.org");
    for (method, path) in &routes {
        let form = (*method == Method::POST)
            .then(|| format!("csrf={CSRF}&environment=box&line={}", url_encode(&line)));
        let answer = hosts
            .send(&guest(), method.clone(), path, form.as_deref())
            .await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(answer.body.contains("Only an admin"), "{path}");
    }
    let rows = hosts.rows().await;
    assert_eq!(rows.len(), 1, "nothing was added or deleted");
}

#[tokio::test]
async fn every_change_needs_the_csrf_token() {
    let hosts = Hosts::new().await;
    let id = hosts.record("box", "github.com", 22, &Key::new()).await;
    let line = Key::new().line("example.org");
    let form = format!("environment=box&line={}", url_encode(&line));
    for path in [
        "/admin/ssh-hosts/add".to_owned(),
        format!("/admin/ssh-hosts/{id}/delete"),
    ] {
        for (headers, form) in [
            (vec![], form.clone()),
            (vec![], format!("csrf=wrong&{form}")),
            (
                vec![("x-csrf-token", "wrong")],
                format!("csrf={CSRF}&{form}"),
            ),
        ] {
            let response = call(
                hosts.router(Some(owner())),
                Method::POST,
                &path,
                &headers,
                Some(&form),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path} {form}");
        }
    }
    assert_eq!(hosts.rows().await.len(), 1, "nothing was added or deleted");
}

#[tokio::test]
async fn the_page_lists_recorded_and_pinned_keys_by_environment_with_every_column() {
    let hosts = Hosts::new().await;
    let recorded = Key::new();
    let id = hosts.record("box", "github.com", 22, &recorded).await;
    let alt = Key::new();
    hosts.record("far", "git.example", 2222, &alt).await;
    let pinned = Key::new();
    hosts
        .hosts
        .pin(&owner().user, "box", &pinned.line("gitlab.com"))
        .await
        .unwrap();
    hosts
        .env
        .store
        .known_host_touch_used(id, Timestamp::now())
        .await
        .unwrap();

    let answer = hosts.get(&owner(), "/admin/ssh-hosts").await;
    assert_eq!(answer.status, StatusCode::OK);
    let page = answer.body;
    assert!(page.contains("<h1>Known SSH hosts</h1>"), "{page}");

    // One section per environment, in order.
    let box_at = page.find("Environment <code>box</code>").unwrap();
    let far_at = page.find("Environment <code>far</code>").unwrap();
    assert!(box_at < far_at);

    // Port 22 shows the host alone; another port as [host]:port.
    assert!(page.contains("<code>github.com</code>"));
    assert!(page.contains("<code>gitlab.com</code>"));
    assert!(page.contains("<code>[git.example]:2222</code>"));
    assert!(!page.contains("github.com:22"));
    assert!(!page.contains("[github.com]"));

    for heading in ["Host", "Key type", "Fingerprint", "First seen", "Last used"] {
        assert!(page.contains(&format!("<th>{heading}</th>")), "{heading}");
    }
    assert!(page.contains("<code>ssh-ed25519</code>"));
    for key in [&recorded, &alt, &pinned] {
        assert!(page.contains(&format!("<code>{}</code>", key.fingerprint)));
    }
    // A recorded key says which workspace, agent, and user; a pinned one who pinned it.
    assert!(
        page.contains("by workspace wk-1, agent jeeves, user owner"),
        "{page}"
    );
    assert!(page.contains("pinned by owner"), "{page}");
    assert!(page.contains("UTC"), "the first-seen time: {page}");
    // Used once, never used twice.
    assert_eq!(page.matches(">never<").count(), 2, "{page}");
    assert!(page.contains(&format!("href=\"/admin/ssh-hosts/{id}/delete\"")));
    for row in hosts.rows().await {
        assert!(page.contains(&format!("href=\"/admin/ssh-hosts/{}/delete\"", row.id)));
    }
    // The sidebar marks the page current.
    assert!(
        page.contains("href=\"/admin/ssh-hosts\" aria-current=\"page\">Known SSH hosts</a>"),
        "{page}"
    );
}

#[tokio::test]
async fn an_empty_page_says_so_and_offers_the_environments() {
    let hosts = Hosts::new().await;
    let page = hosts.get(&owner(), "/admin/ssh-hosts").await.body;
    assert!(
        page.contains("No workspace has connected to an SSH host yet"),
        "{page}"
    );
    assert!(!page.contains("<table"));
    assert!(page.contains("action=\"/admin/ssh-hosts/add\""));
    assert!(
        page.contains("<option value=\"box\" selected>box</option>"),
        "{page}"
    );
    assert!(
        page.contains("<option value=\"far\">far</option>"),
        "{page}"
    );
    assert!(page.contains("<textarea"));
    assert!(page.contains(&format!("name=\"csrf\" value=\"{CSRF}\"")));
    assert!(!page.contains("<script>"), "no inline scripts");
}

#[tokio::test]
async fn without_environments_the_page_says_so_and_has_no_form() {
    let hosts = Hosts::with("").await;
    let page = hosts.get(&owner(), "/admin/ssh-hosts").await.body;
    assert!(page.contains("No environments are configured"), "{page}");
    assert!(!page.contains("action=\"/admin/ssh-hosts/add\""));
    // Pinning anyway is refused.
    let answer = hosts.add("box", &Key::new().line("github.com")).await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(hosts.rows().await.is_empty());
}

#[tokio::test]
async fn the_delete_page_names_the_host_and_what_deleting_does() {
    let hosts = Hosts::new().await;
    let key = Key::new();
    let id = hosts.record("far", "git.example", 2222, &key).await;
    let answer = hosts
        .get(&owner(), &format!("/admin/ssh-hosts/{id}/delete"))
        .await;
    assert_eq!(answer.status, StatusCode::OK);
    let page = answer.body;
    assert!(
        page.contains("Delete the key of <code>[git.example]:2222</code>?"),
        "{page}"
    );
    assert!(page.contains("<code>far</code>"));
    assert!(page.contains(&key.fingerprint));
    assert!(
        page.contains("trusts whatever key the host presents then"),
        "{page}"
    );
    assert!(page.contains("Other keys of the same host stay"), "{page}");
    assert!(page.contains(&format!("action=\"/admin/ssh-hosts/{id}/delete\"")));
    assert!(page.contains(&format!("name=\"csrf\" value=\"{CSRF}\"")));
    assert_eq!(hosts.rows().await.len(), 1, "a GET deletes nothing");
}

#[tokio::test]
async fn posting_the_delete_removes_the_key_and_redirects_with_a_notice() {
    let hosts = Hosts::new().await;
    let gone = hosts.record("box", "github.com", 22, &Key::new()).await;
    let kept = hosts.record("box", "gitlab.com", 22, &Key::new()).await;
    let answer = hosts
        .post(&owner(), &format!("/admin/ssh-hosts/{gone}/delete"), &[])
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(
        answer.location.as_deref(),
        Some("/admin/ssh-hosts?notice=deleted")
    );
    let rows = hosts.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, kept);
    let page = hosts
        .get(&owner(), "/admin/ssh-hosts?notice=deleted")
        .await
        .body;
    assert!(page.contains("Deleted the host key."), "{page}");
    assert!(!page.contains("<code>github.com</code>"));
    // Other notice values say nothing.
    let page = hosts
        .get(&owner(), "/admin/ssh-hosts?notice=%3Cb%3Ex%3C/b%3E")
        .await
        .body;
    assert!(!page.contains("banner-info"), "{page}");
}

#[tokio::test]
async fn an_unknown_id_is_a_404_for_both_the_page_and_the_post() {
    let hosts = Hosts::new().await;
    hosts.record("box", "github.com", 22, &Key::new()).await;
    for path in [
        "/admin/ssh-hosts/9999/delete",
        "/admin/ssh-hosts/abc/delete",
        "/admin/ssh-hosts/-1/delete",
    ] {
        let page = hosts.get(&owner(), path).await;
        assert_eq!(page.status, StatusCode::NOT_FOUND, "GET {path}");
        assert!(page.body.contains("No such known SSH host"));
        let post = hosts.post(&owner(), path, &[]).await;
        assert_eq!(post.status, StatusCode::NOT_FOUND, "POST {path}");
    }
    assert_eq!(hosts.rows().await.len(), 1);
}

#[tokio::test]
async fn adding_a_line_pins_the_key_and_the_page_says_pinned_by_the_owner() {
    let hosts = Hosts::new().await;
    let key = Key::new();
    let answer = hosts.add("far", &key.line("git.example")).await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    let rows = hosts.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].environment, "far");
    assert_eq!(rows[0].host, "git.example");
    assert_eq!(rows[0].port, 22);
    assert_eq!(rows[0].fingerprint, key.fingerprint);
    assert_eq!(rows[0].added_by.as_ref().map(|u| u.as_str()), Some("owner"));
    assert_eq!(
        answer.location,
        Some(format!("/admin/ssh-hosts?pinned={}", rows[0].id))
    );

    let page = hosts.get(&owner(), &answer.location.unwrap()).await.body;
    assert!(
        page.contains("Pinned the ssh-ed25519 key for git.example in far."),
        "{page}"
    );
    assert!(page.contains("pinned by owner"), "{page}");
    assert!(page.contains(&key.fingerprint));
}

#[tokio::test]
async fn a_line_with_several_hosts_and_a_port_pins_each_and_the_notice_names_them() {
    let hosts = Hosts::new().await;
    let key = Key::new();
    let answer = hosts
        .add(
            "box",
            &key.line("git.example,[git.example]:2222,203.0.113.7"),
        )
        .await;
    assert_eq!(answer.status, StatusCode::SEE_OTHER);
    assert_eq!(hosts.rows().await.len(), 3);
    let page = hosts.get(&owner(), &answer.location.unwrap()).await.body;
    assert!(
        page.contains(
            "Pinned the ssh-ed25519 key for git.example, [git.example]:2222, 203.0.113.7 in box."
        ),
        "{page}"
    );
    // An id in ?pinned= that isn't a key says nothing.
    let page = hosts
        .get(&owner(), "/admin/ssh-hosts?pinned=9999,x")
        .await
        .body;
    assert!(!page.contains("banner-info"), "{page}");
}

#[tokio::test]
async fn a_line_that_cant_be_pinned_shows_the_reason_and_the_pasted_text_and_pins_nothing() {
    let hosts = Hosts::new().await;
    let key = Key::new();
    let (kind, blob) = ("ssh-ed25519", key.blob());
    let cases = [
        (
            "hashed name",
            format!("|1|c2FsdA==|aGFzaA== {kind} {blob}"),
            "hashed host names",
        ),
        (
            "marker",
            format!("@cert-authority *.example {kind} {blob}"),
            "markers",
        ),
        (
            "wildcard",
            format!("*.example.com {kind} {blob}"),
            "wildcard",
        ),
        (
            "certificate key type",
            format!("github.com ssh-ed25519-cert-v01@openssh.com {blob}"),
            "be pinned",
        ),
        (
            "garbage",
            "this is not a known_hosts line at all".to_owned(),
            "pin that line",
        ),
        ("empty", "   ".to_owned(), "paste a known_hosts line"),
    ];
    for (name, line, expected) in cases {
        let answer = hosts.add("box", &line).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{name}");
        assert!(answer.body.contains("banner-error"), "{name}");
        assert!(
            answer.body.contains("pin that line"),
            "{name}: {}",
            answer.body
        );
        assert!(answer.body.contains(expected), "{name}: {}", answer.body);
        // The pasted text stays in the form, escaped, with the environment still chosen.
        let kept = line.trim();
        if !kept.is_empty() && !kept.contains(['@', '<', '&', '"', '\'']) {
            assert!(answer.body.contains(kept), "{name}: {}", answer.body);
        }
        assert!(
            answer.body.contains("<option value=\"box\" selected>"),
            "{name}"
        );
    }
    assert!(hosts.rows().await.is_empty());
}

#[tokio::test]
async fn an_unknown_environment_is_refused_and_pins_nothing() {
    let hosts = Hosts::new().await;
    let line = Key::new().line("github.com");
    for environment in ["nowhere", "", "BOX"] {
        let answer = hosts.add(environment, &line).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{environment:?}");
        assert!(answer.body.contains("banner-error"), "{environment:?}");
        let expected = if environment.is_empty() {
            "Choose an environment."
        } else {
            "There is no environment named"
        };
        assert!(
            answer.body.contains(expected),
            "{environment:?}: {}",
            answer.body
        );
        assert!(
            answer.body.contains(&line),
            "the pasted line stays: {environment:?}"
        );
    }
    assert!(hosts.rows().await.is_empty());
}

#[tokio::test]
async fn pinning_a_known_key_again_says_it_is_already_known() {
    let hosts = Hosts::new().await;
    let key = Key::new();
    let recorded = hosts.record("box", "github.com", 22, &key).await;
    let answer = hosts.add("box", &key.line("github.com")).await;
    assert_eq!(answer.status, StatusCode::CONFLICT);
    assert!(
        answer
            .body
            .contains("That key is already known for the host."),
        "{}",
        answer.body
    );
    assert!(answer.body.contains(&key.line("github.com")));
    let rows = hosts.rows().await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, recorded);

    // The same key for another environment, or another key for the same host, is new.
    assert_eq!(
        hosts.add("far", &key.line("github.com")).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        hosts
            .add("box", &Key::new().line("github.com"))
            .await
            .status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(hosts.rows().await.len(), 3);
}

#[tokio::test]
async fn a_pasted_line_is_escaped() {
    let hosts = Hosts::new().await;
    let answer = hosts.add("box", "<script>alert(1)</script>").await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(!answer.body.contains("<script>alert(1)"), "{}", answer.body);
    assert!(
        answer.body.contains("&#60;script&#62;alert(1)"),
        "{}",
        answer.body
    );
}

#[tokio::test]
async fn the_sidebar_links_the_page_for_admins_only() {
    let hosts = Hosts::new().await;
    // A member is refused, so its sidebar comes from the shell: any page it can open shows it.
    let shell =
        crate::channels::web::shell::Shell::new(&guest(), crate::channels::web::shell::Nav::Chats);
    let links: Vec<&str> = shell
        .groups
        .iter()
        .flat_map(|g| &g.links)
        .map(|l| l.href)
        .collect();
    assert!(!links.contains(&"/admin/ssh-hosts"), "{links:?}");

    let page = hosts.get(&owner(), "/admin/ssh-hosts").await.body;
    assert!(page.contains("href=\"/admin/ssh-hosts\""));
    let keys = page.find("href=\"/admin/ssh-keys\"").unwrap();
    let known = page.find("href=\"/admin/ssh-hosts\"").unwrap();
    let admin = page.find("href=\"/admin\"").unwrap();
    assert!(
        admin < keys && keys < known,
        "under the Admin group, after SSH keys"
    );
}
