//! Router-level tests of archiving and deleting chats, over the test configuration with the
//! in-memory fake environment connected as `box`.

use std::sync::atomic::{AtomicBool, Ordering};

use doozers_store::{DeleteReason, WorkspaceOwner};
use doozers_types::{EnvironmentInfo, WorkspaceId};
use serde_json::json;

use crate::workspaces::fake::{Fake, TestClock};
use crate::workspaces::{WorkspaceState, Workspaces};

use super::*;

const ENVIRONMENT: &str =
    "[environment.box]\ncomponent = \"docker\"\naddress = \"http://127.0.0.1:9000\"\n";

/// What the delete route is told about the chat being worked on.
#[derive(Debug, Default)]
struct FakeActivity {
    busy: AtomicBool,
}

#[async_trait]
impl ChatActivity for FakeActivity {
    async fn is_busy(&self, _channel: &str, _conversation: &ConversationId) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

pub(super) struct Chats {
    pub(super) env: Env,
    activity: Arc<FakeActivity>,
    workspaces: Workspaces,
    fake: Arc<Fake>,
}

impl Chats {
    pub(super) async fn new() -> Self {
        let env = env_with_toml(WebConfig::default(), &["web:owner"], ENVIRONMENT).await;
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
        let ssh_keys =
            crate::ssh::SshKeys::for_tests(env.store.clone(), env._dir.path(), clock.clone()).await;
        let workspaces = Workspaces::new(env.store.clone(), env.registry.clone(), clock, ssh_keys);
        Self {
            env,
            activity: Arc::default(),
            workspaces,
            fake,
        }
    }

    pub(super) fn router(&self, user: Option<WebUser>) -> axum::Router {
        let app = self
            .env
            .app
            .install_chat_deletion(
                self.env.app.install(RouterBuilder::new()),
                self.activity.clone(),
                self.workspaces.clone(),
            )
            .build();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    /// `POST /chats/<chat>/<action>` as the owner with the CSRF token in the form: the status and
    /// `Location`.
    async fn post(&self, chat: &ConversationId, action: &str) -> (StatusCode, Option<String>) {
        let response = call(
            self.router(Some(owner())),
            Method::POST,
            &format!("/chats/{chat}/{action}"),
            &[],
            Some(&format!("csrf={CSRF}")),
        )
        .await;
        let location = response
            .headers()
            .get(LOCATION)
            .map(|l| l.to_str().unwrap().to_owned());
        (response.status(), location)
    }

    async fn page(&self, path: &str) -> String {
        let response = call(self.router(Some(owner())), Method::GET, path, &[], None).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        body_text(response).await
    }

    /// A workspace of `jeeves` in `chat`.
    async fn create_workspace(&self, chat: &ConversationId) -> WorkspaceId {
        self.workspaces
            .create(&workspace_owner(chat), "box", json!({}), None)
            .await
            .unwrap()
            .id
    }
}

fn workspace_owner(chat: &ConversationId) -> WorkspaceOwner {
    WorkspaceOwner::Conversation {
        agent: AgentId::new("jeeves").unwrap(),
        channel: INSTANCE.into(),
        conversation: chat.clone(),
        user: Some(identity::owner()),
    }
}

fn other_chat() -> ConversationId {
    ConversationId::new("a".repeat(32)).unwrap()
}

/// `chat()` ("kept for later") and [`other_chat`] ("still going"), stored.
async fn two_chats(env: &Env) {
    store_chat(
        env,
        &chat(),
        INSTANCE,
        vec![stored(Role::User, vec![text("kept for later")])],
    )
    .await;
    store_chat(
        env,
        &other_chat(),
        INSTANCE,
        vec![stored(Role::User, vec![text("still going")])],
    )
    .await;
}

#[tokio::test]
async fn archiving_moves_a_chat_to_the_archived_list_until_unarchived() {
    let chats = Chats::new().await;
    two_chats(&chats.env).await;
    let home = chats.page("/").await;
    assert!(home.contains(r#"href="/chats/archived""#), "{home}");
    let page = chats.page(&format!("/chats/{}", chat())).await;
    assert!(
        page.contains(&format!(r#"action="/chats/{}/archive""#, chat())),
        "{page}"
    );
    assert!(!page.contains("This chat is archived"), "{page}");

    assert_eq!(
        chats.post(&chat(), "archive").await,
        (StatusCode::SEE_OTHER, Some("/".to_owned()))
    );
    let home = chats.page("/").await;
    assert!(!home.contains("kept for later"), "{home}");
    assert!(home.contains("still going"), "{home}");
    let archived = chats.page("/chats/archived").await;
    assert!(archived.contains("<h1>Archived chats</h1>"), "{archived}");
    assert!(archived.contains("kept for later"), "{archived}");
    assert!(!archived.contains("still going"), "{archived}");
    assert!(!archived.contains(r#"href="/chats/new""#), "{archived}");

    // Its page is beside the archived list, says so, and offers Unarchive instead.
    let page = chats.page(&format!("/chats/{}", chat())).await;
    assert!(page.contains("This chat is archived"), "{page}");
    assert!(
        page.contains(&format!(
            r#"<a href="/chats/{}" aria-current="page"><span class="chat-item-title">kept for later</span>"#,
            chat()
        )),
        "{page}"
    );
    assert!(!page.contains("still going"), "{page}");
    assert!(
        page.contains(&format!(r#"action="/chats/{}/unarchive""#, chat())),
        "{page}"
    );
    assert!(
        !page.contains(&format!(r#"action="/chats/{}/archive""#, chat())),
        "{page}"
    );

    assert_eq!(
        chats.post(&chat(), "unarchive").await,
        (StatusCode::SEE_OTHER, Some(format!("/chats/{}", chat())))
    );
    assert!(chats.page("/").await.contains("kept for later"));
    assert!(
        chats
            .page("/chats/archived")
            .await
            .contains("No archived chats.")
    );
}

#[tokio::test]
async fn archiving_keeps_the_chats_workspace() {
    let chats = Chats::new().await;
    two_chats(&chats.env).await;
    let id = chats.create_workspace(&chat()).await;
    chats.post(&chat(), "archive").await;
    let record = chats.env.store.workspace(&id).await.unwrap().unwrap();
    assert_eq!(record.state, WorkspaceState::Active);
    assert!(chats.fake.has(&id));
}

#[tokio::test]
async fn posting_in_an_archived_chat_brings_it_back() {
    let chats = Chats::new().await;
    two_chats(&chats.env).await;
    chats.post(&chat(), "archive").await;
    let response = call(
        chats.router(Some(owner())),
        Method::POST,
        &format!("/chats/{}/messages", chat()),
        &[("hx-request", "true"), ("x-csrf-token", CSRF)],
        Some("text=back+again"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(chats.env.pending_texts().await, ["back again"]);
    assert!(chats.page("/").await.contains("kept for later"));
}

#[tokio::test]
async fn deleting_removes_the_chat_and_its_workspace() {
    let chats = Chats::new().await;
    two_chats(&chats.env).await;
    let id = chats.create_workspace(&chat()).await;
    let page = chats.page(&format!("/chats/{}", chat())).await;
    assert!(page.contains(r#"commandfor="delete-chat""#), "{page}");
    assert!(
        page.contains(&format!(r#"action="/chats/{}/delete""#, chat())),
        "{page}"
    );

    assert_eq!(
        chats.post(&chat(), "delete").await,
        (StatusCode::SEE_OTHER, Some("/".to_owned()))
    );
    let home = chats.page("/").await;
    assert!(!home.contains("kept for later"), "{home}");
    assert!(home.contains("still going"), "{home}");
    let page = chats.page(&format!("/chats/{}", chat())).await;
    assert!(!page.contains("kept for later"), "{page}");
    assert!(page.contains("<h2>New chat</h2>"), "{page}");
    let deleted = chats
        .workspaces
        .last_deleted(&workspace_owner(&chat()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deleted.id, id);
    assert_eq!(deleted.state, WorkspaceState::Deleted);
    assert_eq!(deleted.delete_reason, Some(DeleteReason::ChatDeleted));
    assert!(!chats.fake.has(&id));
    // Gone: nothing to delete or archive any more.
    assert_eq!(chats.post(&chat(), "delete").await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        chats.post(&chat(), "archive").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_an_archived_chat_returns_to_the_archived_list() {
    let chats = Chats::new().await;
    two_chats(&chats.env).await;
    chats.post(&chat(), "archive").await;
    assert_eq!(
        chats.post(&chat(), "delete").await,
        (StatusCode::SEE_OTHER, Some("/chats/archived".to_owned()))
    );
    assert!(
        chats
            .page("/chats/archived")
            .await
            .contains("No archived chats.")
    );
}

#[tokio::test]
async fn a_chat_being_worked_on_is_not_deleted() {
    let chats = Chats::new().await;
    two_chats(&chats.env).await;
    let id = chats.create_workspace(&chat()).await;

    chats.activity.busy.store(true, Ordering::SeqCst);
    assert_eq!(chats.post(&chat(), "delete").await.0, StatusCode::CONFLICT);
    chats.activity.busy.store(false, Ordering::SeqCst);

    // A post the core hasn't taken yet.
    chats
        .env
        .channel
        .post(&owner(), &chat(), "one more thing", Timestamp::now())
        .unwrap();
    assert_eq!(chats.post(&chat(), "delete").await.0, StatusCode::CONFLICT);

    assert!(chats.page("/").await.contains("kept for later"));
    let record = chats.env.store.workspace(&id).await.unwrap().unwrap();
    assert_eq!(record.state, WorkspaceState::Active);
}

#[tokio::test]
async fn chat_actions_need_a_login_and_the_csrf_token() {
    let chats = Chats::new().await;
    two_chats(&chats.env).await;
    for action in ["archive", "unarchive", "delete"] {
        let path = format!("/chats/{}/{action}", chat());
        let form = format!("csrf={CSRF}");
        let response = call(chats.router(None), Method::POST, &path, &[], Some(&form)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        for (headers, form) in [
            (vec![], String::new()),
            (vec![("x-csrf-token", "wrong")], "csrf=wrong".to_owned()),
        ] {
            let response = call(
                chats.router(Some(owner())),
                Method::POST,
                &path,
                &headers,
                Some(&form),
            )
            .await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path} {form}");
        }
    }
    let home = chats.page("/").await;
    assert!(home.contains("kept for later"), "{home}");
}

#[tokio::test]
async fn a_chat_with_nothing_stored_has_no_actions() {
    let chats = Chats::new().await;
    let page = chats.page(&format!("/chats/{}", chat())).await;
    assert!(!page.contains("/archive\""), "{page}");
    assert!(!page.contains("delete-chat"), "{page}");
    assert_eq!(
        chats.post(&chat(), "archive").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(chats.post(&chat(), "delete").await.0, StatusCode::NOT_FOUND);
    let bad = ConversationId::new("not-a-chat").unwrap();
    assert_eq!(chats.post(&bad, "delete").await.0, StatusCode::NOT_FOUND);
}
