//! Router-level tests of the Users pages (`/admin/users`) over the test configuration, the stored
//! users (the imported owner and a second admin `root`, and a member `mia`), and the fake
//! environment connected as `box`. A request carries an injected [`WebUser`], as in the other admin
//! page tests.

use std::sync::atomic::{AtomicBool, Ordering};

use doozers_store::{
    MemoryOwner, MemoryViewer, NewMemory, Role as UserRole, SessionKind, UserState, WorkspaceOwner,
};
use doozers_types::{EnvironmentInfo, WorkspaceId};
use serde_json::json;

use super::admin::stored_users;
use crate::approvals::ApprovalBoard;
use crate::forget::{ForgetOptions, Forgetter, GroupSummaries, UserActivity};
use crate::secrets::{KeyFile, LocalSecrets};
use crate::workspaces::Workspaces;
use crate::workspaces::fake::{Fake, TestClock};

use super::agents::guest;
use super::*;

const ENVIRONMENT: &str =
    "[environment.box]\ncomponent = \"docker\"\naddress = \"http://127.0.0.1:9000\"\n";

/// Whether the user is being worked for.
#[derive(Debug, Default)]
struct Busy(AtomicBool);

#[async_trait]
impl UserActivity for Busy {
    async fn is_busy(&self, _: &UserId) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct NoSummaries;

#[async_trait]
impl GroupSummaries for NoSummaries {
    async fn regenerate(&self, _: doozers_store::SessionId) {}
}

pub(super) struct Pages {
    pub(super) env: Env,
    pub(super) users: Arc<Users>,
    forgetter: Forgetter,
    busy: Arc<Busy>,
    fake: Arc<Fake>,
    workspaces: Workspaces,
}

impl Pages {
    pub(super) async fn new() -> Self {
        let env = env_with_toml(WebConfig::default(), &["web:owner"], ENVIRONMENT).await;
        let users = stored_users(&env).await;
        let clock = TestClock::new();
        let fake = Fake::new(clock.clone());
        env.registry
            .register_environment(
                &InstanceName::new("box").unwrap(),
                fake.clone(),
                EnvironmentInfo {
                    options_schema: json!({"type": "object"}),
                },
                crate::registry::EnvironmentCapabilities::default(),
            )
            .unwrap();
        let root =
            crate::paths::DataRoot::new(env._dir.path(), &env._dir.path().join("tmp")).unwrap();
        let secrets = Arc::new(
            LocalSecrets::open(
                env.store.clone(),
                &KeyFile::new(&root, &root.secret_key_file()),
                clock.clone(),
            )
            .await
            .unwrap(),
        );
        let ssh_keys = crate::ssh::SshKeys::new(
            env.store.clone(),
            secrets.clone(),
            clock.clone(),
            env.channel.users().clone(),
        );
        let workspaces = Workspaces::new(
            env.store.clone(),
            env.registry.clone(),
            clock.clone(),
            ssh_keys,
        );
        let busy = Arc::new(Busy::default());
        let forgetter = Forgetter::new(ForgetOptions {
            store: env.store.clone(),
            users: users.clone(),
            root,
            memory_dbs: env.memory_dbs.clone(),
            secrets,
            workspaces: workspaces.clone(),
            approvals: Arc::new(ApprovalBoard::default()),
            jobs: env.jobs.clone(),
            activity: busy.clone(),
            summaries: Arc::new(NoSummaries),
            clock,
        });
        Self {
            env,
            users,
            forgetter,
            busy,
            fake,
            workspaces,
        }
    }

    pub(super) fn router(&self, user: Option<WebUser>) -> axum::Router {
        let app = self
            .env
            .app
            .install_users(
                self.env.app.install(RouterBuilder::new()),
                self.users.clone(),
                self.forgetter.clone(),
            )
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    /// `method path` as `user`, with the CSRF token in the form of a POST unless `csrf` is false.
    /// The status, body, and `Location`. Every answer is `no-store`.
    pub(super) async fn send(
        &self,
        user: Option<WebUser>,
        method: Method,
        path: &str,
        csrf: bool,
    ) -> (StatusCode, String, Option<String>) {
        let form = (method == Method::POST).then(|| {
            if csrf {
                format!("csrf={CSRF}")
            } else {
                String::new()
            }
        });
        let response = call(self.router(user), method, path, &[], form.as_deref()).await;
        if response.status() != StatusCode::UNAUTHORIZED {
            assert_eq!(response.headers()[CACHE_CONTROL], "no-store", "{path}");
        }
        let status = response.status();
        let location = response
            .headers()
            .get(LOCATION)
            .map(|l| l.to_str().unwrap().to_owned());
        (status, body_text(response).await, location)
    }

    async fn get(&self, path: &str) -> (StatusCode, String, Option<String>) {
        self.send(Some(owner()), Method::GET, path, false).await
    }

    async fn post(&self, path: &str) -> (StatusCode, String, Option<String>) {
        self.send(Some(owner()), Method::POST, path, true).await
    }

    pub(super) fn user(&self, name: &str) -> Option<crate::users::User> {
        self.users
            .list()
            .get()
            .user(&UserId::new(name).unwrap())
            .cloned()
    }

    /// mia's chat with `jeeves` of two messages, a memory with `jeeves`, her profile, a runtime
    /// job, and a workspace for the chat.
    async fn give_mia_data(&self) -> WorkspaceId {
        let mia = UserId::new("mia").unwrap();
        let chat = ConversationId::new("a".repeat(32)).unwrap();
        let (session, _) = self
            .env
            .store
            .open_session(
                SessionKey {
                    agent: AgentId::new("jeeves").unwrap(),
                    channel: INSTANCE.into(),
                    conversation: chat.clone(),
                },
                SessionKind::Direct,
                Some(mia.clone()),
                Timestamp::now(),
            )
            .await
            .unwrap();
        self.env
            .store
            .append_messages(
                session.id,
                vec![
                    stored(Role::User, vec![text("my cat is Tom")]),
                    stored(Role::Assistant, vec![text("hello Mia")]),
                ],
            )
            .await
            .unwrap();
        self.env
            .memory_dbs
            .get(&AgentId::new("jeeves").unwrap())
            .await
            .unwrap()
            .add(
                &MemoryViewer::new(MemoryOwner::User(mia.clone()), false),
                NewMemory {
                    content: "mia likes tea".to_owned(),
                    core: false,
                    shared: false,
                    source_session: None,
                },
                2200,
                Timestamp::now(),
            )
            .await
            .unwrap();
        let dir = self.env._dir.path().join("users/mia");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("USER.md"), "likes cats").unwrap();
        self.env
            .store
            .job_create(
                AgentId::new("jeeves").unwrap(),
                mia.clone(),
                json!({"prompt": "x"}),
                Timestamp::now(),
            )
            .await
            .unwrap();
        self.workspaces
            .create(
                &WorkspaceOwner::Conversation {
                    agent: AgentId::new("jeeves").unwrap(),
                    channel: INSTANCE.into(),
                    conversation: chat,
                    user: Some(mia),
                },
                "box",
                json!({}),
                None,
            )
            .await
            .unwrap()
            .id
    }
}

/// An admin who the stored users say isn't one any more: the role a request carried.
fn stale_admin(name: &str) -> WebUser {
    WebUser {
        user: UserId::new(name).unwrap(),
        role: UserRole::Admin,
        sender: NativeId::new(name).unwrap(),
        csrf: CSRF.into(),
        display_name: "Test user".into(),
        login: auth::Login::Test,
    }
}

#[tokio::test]
async fn only_an_admin_opens_the_user_pages() {
    let p = Pages::new().await;
    let cases = [
        (Method::GET, "/admin/users"),
        (Method::GET, "/admin/users/mia/remove"),
        (Method::POST, "/admin/users/mia/remove"),
    ];
    for (method, path) in cases {
        let (status, ..) = p.send(None, method.clone(), path, true).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        let (status, body, _) = p.send(Some(guest()), method.clone(), path, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(body.contains("Only an admin"), "{body}");
    }
    assert!(p.user("mia").is_some());
}

#[tokio::test]
async fn the_list_shows_each_user_with_role_and_state_and_a_remove_link() {
    let p = Pages::new().await;
    p.users
        .set_state(
            &UserId::new("mia").unwrap(),
            UserState::Disabled,
            Timestamp::now(),
        )
        .await
        .unwrap();
    let (status, html, _) = p.get("/admin/users").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("<h1>Users</h1>"), "{html}");
    assert!(
        html.contains("<strong>owner</strong> <span class=\"meta\">(you)</span>"),
        "{html}"
    );
    assert!(html.contains("<strong>root</strong></td>"), "{html}");
    assert!(html.contains(">ROOT</td>"), "{html}");
    assert!(
        html.contains(r#"<span class="pill pill-ok">Admin</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="pill pill-neutral">Member</span>"#),
        "{html}"
    );
    assert!(
        html.contains(r#"<span class="pill pill-warn">Disabled</span>"#),
        "{html}"
    );
    for id in ["owner", "root", "mia"] {
        assert!(
            html.contains(&format!(r#"href="/admin/users/{id}/remove""#)),
            "{id}: {html}"
        );
    }
    // The sidebar's Admin group has the link, marked current.
    assert!(
        html.contains(r#"<a href="/admin/users" aria-current="page">Users</a>"#),
        "{html}"
    );
}

#[tokio::test]
async fn the_confirmation_counts_what_the_removal_deletes() {
    let p = Pages::new().await;
    let workspace = p.give_mia_data().await;
    let (status, html, _) = p.get("/admin/users/mia/remove").await;
    assert_eq!(status, StatusCode::OK, "{html}");
    assert!(html.contains("Remove MIA?"), "{html}");
    assert!(
        html.contains("1 chat in 1 session, with 2 messages and 0 summaries in them."),
        "{html}"
    );
    assert!(html.contains("1 private memory, in jeeves (1)"), "{html}");
    assert!(
        html.contains("everything else in the folder users/mia/"),
        "{html}"
    );
    assert!(html.contains("1 channel identity"), "{html}");
    assert!(html.contains("1 job they scheduled, which stop"), "{html}");
    assert!(
        html.contains("1 workspace, with everything in it"),
        "{html}"
    );
    assert!(
        html.contains(&format!(
            "Workspace {workspace} in the environment box, for jeeves in web"
        )),
        "{html}"
    );
    assert!(
        html.contains("A shared memory they wrote stays for everyone"),
        "{html}"
    );
    assert!(
        html.contains(r#"action="/admin/users/mia/remove""#),
        "{html}"
    );
    assert!(html.contains("Remove user</button>"), "{html}");
    assert!(
        html.contains(&format!(r#"name="csrf" value="{CSRF}""#)),
        "{html}"
    );

    // An unknown user is a 404.
    let (status, ..) = p.get("/admin/users/nobody/remove").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_user_with_work_running_can_be_looked_at_but_not_removed() {
    let p = Pages::new().await;
    p.busy.0.store(true, Ordering::SeqCst);
    let (status, html, _) = p.get("/admin/users/mia/remove").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("a chat or a scheduled job running"), "{html}");
    assert!(!html.contains("Remove user</button>"), "{html}");

    let (status, html, _) = p.post("/admin/users/mia/remove").await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    // Said once, in the page's words.
    assert_eq!(
        html.matches("a chat or a scheduled job running").count(),
        1,
        "{html}"
    );
    assert!(!html.contains("`/stop`"), "{html}");
    assert_eq!(p.user("mia").unwrap().state, UserState::Active);
}

#[tokio::test]
async fn removing_a_user_needs_the_csrf_token_then_forgets_them_and_says_so() {
    let p = Pages::new().await;
    let workspace = p.give_mia_data().await;
    let (status, ..) = p
        .send(
            Some(owner()),
            Method::POST,
            "/admin/users/mia/remove",
            false,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(p.user("mia").is_some());

    // A page of mia's chat is open: its live state is dropped with the chat.
    let chat = ConversationId::new("a".repeat(32)).unwrap();
    p.env.channel.hubs.get(&chat);
    assert!(p.env.channel.hubs.find(&chat).is_some());

    let (status, _, location) = p.post("/admin/users/mia/remove").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(p.env.channel.hubs.find(&chat).is_none());
    assert_eq!(location.as_deref(), Some("/admin/users?removed=mia"));
    assert!(p.user("mia").is_none());
    assert!(
        p.env
            .store
            .user(&UserId::new("mia").unwrap())
            .await
            .unwrap()
            .is_none()
    );
    assert!(!p.fake.has(&workspace));
    assert!(!p.env._dir.path().join("users/mia").exists());
    let (_, html, _) = p.get("/admin/users?removed=mia").await;
    assert!(html.contains("Removed mia."), "{html}");
    assert!(
        !html.contains(r#"href="/admin/users/mia/remove""#),
        "{html}"
    );
    // The other users are as they were, and mia is a 404 now.
    assert!(p.user("root").is_some() && p.user("owner").is_some());
    let (status, ..) = p.get("/admin/users/mia/remove").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, ..) = p.post("/admin/users/mia/remove").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_admin_cant_remove_themselves() {
    let p = Pages::new().await;
    let (status, html, _) = p.get("/admin/users/owner/remove").await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("You can&#39;t remove yourself"), "{html}");
    assert!(!html.contains("Remove user</button>"), "{html}");

    let (status, html, _) = p.post("/admin/users/owner/remove").await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    assert_eq!(
        html.matches("You can&#39;t remove yourself").count(),
        1,
        "{html}"
    );
    assert_eq!(p.user("owner").unwrap().state, UserState::Active);
}

#[tokio::test]
async fn the_last_active_admin_cant_be_removed() {
    let p = Pages::new().await;
    let root = UserId::new("root").unwrap();
    p.users
        .set_state(&root, UserState::Disabled, Timestamp::now())
        .await
        .unwrap();
    // A request whose role is stale: an admin who was demoted a moment ago.
    let acting = stale_admin("mia");
    let (status, html, _) = p
        .send(
            Some(acting.clone()),
            Method::GET,
            "/admin/users/owner/remove",
            false,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(html.contains("This is the last active admin"), "{html}");
    assert!(!html.contains("Remove user</button>"), "{html}");

    let (status, html, _) = p
        .send(
            Some(acting),
            Method::POST,
            "/admin/users/owner/remove",
            true,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{html}");
    assert_eq!(
        html.matches("This is the last active admin").count(),
        1,
        "{html}"
    );
    assert_eq!(p.user("owner").unwrap().state, UserState::Active);
}

#[tokio::test]
async fn a_page_that_cant_read_the_users_data_says_so_and_nothing_changed() {
    let p = Pages::new().await;
    // An agent's memory database that can't be opened.
    std::fs::create_dir_all(p.env._dir.path().join("agents/bunter/memory.db")).unwrap();
    let (status, html, _) = p.get("/admin/users/mia/remove").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{html}");
    assert!(
        html.contains("Couldn&#39;t read the user&#39;s data"),
        "{html}"
    );
    assert!(html.contains("Nothing was changed"), "{html}");
    assert!(!html.contains("disabled"), "{html}");
    assert_eq!(p.user("mia").unwrap().state, UserState::Active);
}
