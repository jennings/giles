//! Router-level tests of the web channel's pages, its event queue, and live output, against a
//! temporary store. Authentication is injected: a request carries a [`WebUser`] extension or
//! none.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::extract::Extension;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION,
    SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use doozers_store::{
    BlobMeta, BlobStore, InboxState, InviteKind, NewInboxEvent, NewUser, SessionKey, SessionKind,
    SignInMethod, Store,
};
use doozers_types::{
    AgentId, Block, Channel, ChannelEvent, ChannelStatus, ConversationId, Error, MediaRef, Message,
    MessageMeta, NativeId, OutboundMessage, Role, SendTarget, Timestamp, UserId,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::pages::MAX_MESSAGE_BYTES;
use super::*;
use crate::commands::NoPendingApprovals;
use crate::http::RouterBuilder;
use crate::router::Router as MessageRouter;
use crate::routes::{Route, RouteList};
use crate::sessions::{
    AgentPolicies, AgentPolicy, ChannelLookup, Sessions, SessionsOptions, SystemClock, Turn,
    TurnOutcome, TurnRunner,
};
use crate::users::{INVITE_TTL, User, UserList, Users};

mod admin;
mod agent_edit;
mod agent_environments;
mod agent_remove;
mod agents;
mod chat_actions;
mod jobs;
mod memory;
mod routes;
mod ssh_hosts;
mod ssh_keys;
mod toolsets;
mod users;
mod workspaces;

use admin::open_users;

const INSTANCE: &str = "web";
const CSRF: &str = "csrf-token-for-tests";
const WAIT: Duration = Duration::from_secs(10);

struct Env {
    _dir: TempDir,
    store: Store,
    /// The agents' memory databases, the ones the agent manager closes on removal.
    memory_dbs: crate::memory_dbs::MemoryDbs,
    blobs: BlobStore,
    channel: Arc<WebChannel>,
    /// The registry of the configuration [`load_config`] wrote; the app reads its running
    /// configuration. No component is registered.
    registry: Arc<crate::registry::Registry>,
    /// The routes the agent manager checks before it removes an agent, opened when first needed.
    routes: tokio::sync::OnceCell<Arc<crate::routes::Routes>>,
    /// What the agent manager is told about agents being busy.
    activity: Arc<crate::agents::remove::FakeActivity>,
    /// What the agent manager and the jobs page tell the scheduler about runtime jobs.
    jobs: crate::scheduler::JobChanges,
    app: WebApp,
    shutdown: CancellationToken,
}

async fn env() -> Env {
    env_with(WebConfig::default(), &["web:owner"]).await
}

/// The owner, with `identities`.
fn owner_config(identities: &[&str]) -> UserList {
    UserList::owner(identities, None)
}

/// The agents every test configuration has: `jeeves` (named "Jeeves", with a job whose prompt is
/// `prompts/morning.md`) and `bunter`.
const AGENTS: [(&str, &str); 2] = [
    (
        "jeeves",
        "name = \"Jeeves\"\nmodels = [\"fake/test-model\"]\ntools = [\"memory\"]\n\
         [[jobs]]\nid = \"morning\"\ncron = \"0 8 * * *\"\nprompt_file = \"prompts/morning.md\"\n",
    ),
    ("bunter", "models = [\"fake/test-model\"]\n"),
];

/// Writes a configuration under `dir` for an owner with `identities`: a provider `fake`, a web
/// channel, and [`AGENTS`], then `extra` (TOML appended to `doozers.toml`); then loads it.
fn load_config(dir: &std::path::Path, identities: &[&str], extra: &str) -> crate::config::Config {
    let root = crate::paths::DataRoot::new(dir, &dir.join("tmp")).unwrap();
    let default = format!("{:?}", identities[0]);
    let identities = identities
        .iter()
        .map(|id| format!("{id:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        root.config_file(),
        format!(
            "[owner]\nidentities = [{identities}]\ndefault = {default}\n\
             [provider.fake]\ncomponent = \"openai\"\n\
             config = {{ base_url = \"http://127.0.0.1:9/v1\" }}\n\
             [channel.web]\n\
             [toolset.files]\ncommand = \"/x\"\n{extra}"
        ),
    )
    .unwrap();
    for (agent, toml) in AGENTS {
        let agent_dir = root.agents_dir().join(agent);
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("agent.toml"), toml).unwrap();
    }
    let prompts = root.agents_dir().join("jeeves/prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("morning.md"), "Plan my day.").unwrap();
    crate::config::Config::load_with(
        &root,
        &crate::config::LoadOptions {
            env: &|_| None,
            doozers_exe: None,
            ..crate::config::LoadOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

/// A web channel with `config`, for an owner with `identities`.
async fn env_with(config: WebConfig, identities: &[&str]) -> Env {
    env_with_toml(config, identities, "").await
}

/// [`env_with`], with `extra` (TOML) appended to `doozers.toml`.
async fn env_with_toml(config: WebConfig, identities: &[&str], extra: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("doozers.db")).await.unwrap();
    let blobs = BlobStore::open(dir.path().join("blobs"), store.clone())
        .await
        .unwrap();
    let channel = Arc::new(WebChannel::new(
        InstanceName::new(INSTANCE).unwrap(),
        config,
        owner_config(identities),
        "0.0.0.0:8080".parse().unwrap(),
    ));
    let shutdown = CancellationToken::new();
    let root = crate::paths::DataRoot::new(dir.path(), &dir.path().join("tmp")).unwrap();
    let config = Arc::new(load_config(dir.path(), identities, extra));
    let registry = Arc::new(crate::registry::Registry::new(root, config));
    let jobs = crate::scheduler::JobChanges::default();
    let app = WebApp::new(
        channel.clone(),
        store.clone(),
        blobs.clone(),
        registry.live_config().clone(),
        jobs.clone(),
        shutdown.clone(),
    );
    Env {
        _dir: dir,
        store,
        memory_dbs: crate::memory_dbs::MemoryDbs::new(
            registry.root().clone(),
            doozers_store::JournalMode::Wal,
        ),
        blobs,
        channel,
        registry,
        routes: tokio::sync::OnceCell::new(),
        activity: Arc::default(),
        jobs,
        app,
        shutdown,
    }
}

fn owner() -> WebUser {
    WebUser {
        user: identity::owner(),
        role: doozers_store::Role::Admin,
        sender: NativeId::new("owner").unwrap(),
        csrf: CSRF.into(),
        login: auth::Login::Test,
    }
}

fn chat() -> ConversationId {
    ConversationId::new("0123456789abcdef0123456789abcdef").unwrap()
}

/// Sends a request to `app`; `form` is sent URL-encoded.
async fn call(
    app: axum::Router,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    form: Option<&str>,
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match form {
        Some(form) => {
            request = request.header(CONTENT_TYPE, "application/x-www-form-urlencoded");
            Body::from(form.to_owned())
        }
        None => Body::empty(),
    };
    app.oneshot(request.body(body).unwrap()).await.unwrap()
}

impl Env {
    /// The whole application as the main listener serves it.
    fn main_app(&self) -> axum::Router {
        self.app.install(RouterBuilder::new()).build()
    }

    /// The whole application as the trusted-proxy listener serves it.
    fn proxy_app(&self) -> axum::Router {
        self.app.proxy_app(self.main_app())
    }

    /// The whole application, as `user` (none: not logged in).
    fn router(&self, user: Option<WebUser>) -> axum::Router {
        let app = self.main_app();
        match user {
            Some(user) => app.layer(Extension(user)),
            None => app,
        }
    }

    async fn get(&self, path: &str) -> Response {
        self.request(Some(owner()), Method::GET, path, &[], None)
            .await
    }

    async fn request(
        &self,
        user: Option<WebUser>,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        form: Option<&str>,
    ) -> Response {
        call(self.router(user), method, path, headers, form).await
    }

    /// Posts `form` to `path` as the owner through htmx.
    async fn htmx_post(&self, path: &str, form: &str) -> Response {
        self.request(
            Some(owner()),
            Method::POST,
            path,
            &[("hx-request", "true"), ("x-csrf-token", CSRF)],
            Some(form),
        )
        .await
    }

    /// The pending events, as a fresh stream sees them (and the stream).
    async fn pending(&self) -> Vec<ChannelEvent> {
        let mut events = self.channel.events().await.unwrap();
        let mut out = Vec::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(50), events.next()).await
        {
            out.push(event.unwrap());
        }
        out
    }

    /// The pending message events' texts.
    async fn pending_texts(&self) -> Vec<String> {
        self.pending()
            .await
            .into_iter()
            .filter_map(|event| match event {
                ChannelEvent::Message { message, .. } => Some(message.text),
                _ => None,
            })
            .collect()
    }
}

async fn body_text(response: Response) -> String {
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}

/// An open event stream of a chat page.
struct Sse {
    body: BoxStream<'static, Result<bytes::Bytes, axum::Error>>,
    buffer: String,
}

/// One server-sent event: its id (if any) and data.
#[derive(Debug)]
struct SseMessage {
    id: Option<String>,
    data: String,
}

impl Sse {
    async fn open(env: &Env, path: &str, headers: &[(&str, &str)]) -> Self {
        let response = env
            .request(Some(owner()), Method::GET, path, headers, None)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
        Self {
            body: response.into_body().into_data_stream().boxed(),
            buffer: String::new(),
        }
    }

    /// The next event, skipping keep-alive comments.
    async fn next(&mut self) -> SseMessage {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let frame: String = self.buffer.drain(..end + 2).collect();
                let mut id = None;
                let mut data = Vec::new();
                let mut event = None;
                for line in frame.lines() {
                    if let Some(v) = line.strip_prefix("id: ") {
                        id = Some(v.to_owned());
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data.push(v);
                    } else if let Some(v) = line.strip_prefix("event: ") {
                        event = Some(v.to_owned());
                    }
                }
                if data.is_empty() {
                    continue;
                }
                assert_eq!(event.as_deref(), Some("message"), "{frame}");
                return SseMessage {
                    id,
                    data: data.join("\n"),
                };
            }
            let chunk = tokio::time::timeout(WAIT, self.body.next())
                .await
                .expect("no event")
                .expect("the stream ended")
                .unwrap();
            self.buffer.push_str(std::str::from_utf8(&chunk).unwrap());
        }
    }

    /// Whether the stream ends within the wait.
    async fn ends(&mut self) -> bool {
        loop {
            match tokio::time::timeout(WAIT, self.body.next()).await {
                Ok(None) => return true,
                Ok(Some(_)) => {}
                Err(_) => return false,
            }
        }
    }
}

fn text(t: &str) -> Block {
    Block::Text { text: t.into() }
}

fn stored(role: Role, content: Vec<Block>) -> Message {
    Message {
        role,
        content,
        meta: MessageMeta::new(Timestamp::now()),
    }
}

async fn store_chat(env: &Env, chat: &ConversationId, channel: &str, messages: Vec<Message>) {
    let key = SessionKey {
        agent: AgentId::new("jeeves").unwrap(),
        channel: channel.into(),
        conversation: chat.clone(),
    };
    let (session, _) = env
        .store
        .open_session(
            key,
            SessionKind::Direct,
            Some(identity::owner()),
            Timestamp::now(),
        )
        .await
        .unwrap();
    env.store
        .append_messages(session.id, messages)
        .await
        .unwrap();
}

#[tokio::test]
async fn every_page_needs_a_login() {
    let env = env().await;
    let chat = chat();
    let blob = "a".repeat(64);
    let cases = [
        (Method::GET, "/".to_owned()),
        (Method::GET, "/chats/new".to_owned()),
        (Method::GET, format!("/chats/{chat}")),
        (Method::GET, format!("/chats/{chat}/events")),
        (Method::POST, format!("/chats/{chat}/messages")),
        (Method::POST, format!("/chats/{chat}/approvals")),
        (Method::GET, format!("/blobs/{blob}")),
        (Method::POST, "/logout".to_owned()),
    ];
    for (method, path) in cases {
        let response = env
            .request(
                None,
                method.clone(),
                &path,
                &[("x-csrf-token", CSRF)],
                Some("text=hi&code=1&answer=yes"),
            )
            .await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
        let html = body_text(response).await;
        assert!(html.contains("doozers admin login-link"), "{html}");
    }
    assert!(env.pending_texts().await.is_empty());
}

#[tokio::test]
async fn the_chat_list_shows_this_instances_chats_newest_first() {
    let env = env().await;
    let response = env.get("/").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response).await.contains("No chats yet"));

    let older = ConversationId::new("a".repeat(32)).unwrap();
    store_chat(
        &env,
        &older,
        INSTANCE,
        vec![stored(Role::User, vec![text("first <chat>")])],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    store_chat(
        &env,
        &chat(),
        INSTANCE,
        vec![stored(Role::User, vec![text("second chat\nmore")])],
    )
    .await;
    // Another instance's conversation isn't a web chat.
    store_chat(
        &env,
        &ConversationId::new("b".repeat(32)).unwrap(),
        "irc",
        vec![stored(Role::User, vec![text("irc chat")])],
    )
    .await;

    let html = body_text(env.get("/").await).await;
    let second = html
        .find(&format!(
            r#"<a href="/chats/{}"><span class="chat-item-title">second chat</span>"#,
            chat()
        ))
        .expect(&html);
    let first = html
        .find(&format!(
            r#"<a href="/chats/{older}"><span class="chat-item-title">first &#60;chat&#62;</span>"#
        ))
        .expect(&html);
    assert!(second < first, "{html}");
    assert!(!html.contains("irc chat"), "{html}");
    assert!(html.contains(r#"href="/chats/new""#));
    assert!(html.contains("Pick a chat or start one."), "{html}");
    assert!(!html.contains("aria-current=\"page\"><span"), "{html}");

    // A chat's page lists the chats too, with the open one marked and its title above it.
    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert!(
        page.contains(&format!(
            r#"<a href="/chats/{}" aria-current="page"><span class="chat-item-title">second chat</span>"#,
            chat()
        )),
        "{page}"
    );
    assert!(page.contains("<h2>second chat</h2>"), "{page}");
    assert!(
        page.contains(&format!(
            r#"<a href="/chats/{older}"><span class="chat-item-title">first &#60;chat&#62;</span>"#
        )),
        "{page}"
    );
}

#[tokio::test]
async fn new_chat_redirects_to_a_fresh_chat_id() {
    let env = env().await;
    let response = env.get("/chats/new").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()[LOCATION].to_str().unwrap().to_owned();
    let id = location.strip_prefix("/chats/").unwrap();
    assert!(is_chat_id(id), "{location}");
    let again = env.get("/chats/new").await;
    assert_ne!(again.headers()[LOCATION], location.as_str());
}

#[tokio::test]
async fn chat_paths_need_a_chat_id() {
    let env = env().await;
    for path in [
        "/chats/abc",
        "/chats/0123456789ABCDEF0123456789ABCDEF",
        "/chats/0123456789abcdef0123456789abcdeg/events",
        "/blobs/xyz",
    ] {
        let response = env.get(path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn a_chat_page_shows_the_transcript_and_follows_its_events() {
    let env = env().await;
    store_chat(
        &env,
        &chat(),
        INSTANCE,
        vec![
            stored(Role::User, vec![text("hello **doozers**")]),
            stored(Role::Assistant, vec![text("<script>alert(1)</script> hi")]),
        ],
    )
    .await;
    let response = env.get(&format!("/chats/{}", chat())).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("hello <strong>doozers</strong>"), "{html}");
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt; hi"),
        "{html}"
    );
    assert!(
        html.contains(&format!(r#"hx-headers='{{"X-CSRF-Token": "{CSRF}"}}'"#)),
        "{html}"
    );
    let cursor = env.channel.hubs.cursor(0);
    assert!(
        html.contains(&format!(
            r#"sse-connect="/chats/{}/events?after={cursor}""#,
            chat()
        )),
        "{html}"
    );
    assert!(html.contains(r#"name="csrf" value="csrf-token-for-tests""#));
    // Nothing inline that the Content-Security-Policy would block.
    assert_eq!(
        html.matches("<script").count(),
        html.matches("<script src=").count()
    );
    assert!(!html.contains(" style="), "{html}");
}

#[tokio::test]
async fn posting_needs_the_csrf_token() {
    let env = env().await;
    let path = format!("/chats/{}/messages", chat());
    for (headers, form) in [
        (vec![], "text=hi"),
        (vec![], "text=hi&csrf=wrong"),
        (vec![("x-csrf-token", "wrong")], "text=hi"),
    ] {
        let response = env
            .request(Some(owner()), Method::POST, &path, &headers, Some(form))
            .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{form}");
    }
    assert!(env.pending_texts().await.is_empty());

    // The form field works without htmx.
    let form = format!("text=plain+post&csrf={CSRF}");
    let response = env
        .request(Some(owner()), Method::POST, &path, &[], Some(&form))
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[LOCATION],
        format!("/chats/{}", chat()).as_str()
    );
    assert_eq!(env.pending_texts().await, ["plain post"]);
}

#[tokio::test]
async fn a_posted_message_is_an_authenticated_channel_event() {
    let env = env().await;
    let mut events = env.channel.events().await.unwrap();
    let first = events.next().await.unwrap().unwrap();
    assert!(matches!(
        first,
        ChannelEvent::Status {
            status: ChannelStatus::Connected,
            ..
        }
    ));

    let response = env
        .htmx_post(&format!("/chats/{}/messages", chat()), "text=hi+there")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let form = body_text(response).await;
    assert!(form.starts_with(r#"<form id="compose""#), "{form}");
    assert!(
        form.contains("></textarea>"),
        "the form is empty again: {form}"
    );

    let ChannelEvent::Message { id, message } = events.next().await.unwrap().unwrap() else {
        panic!("not a message");
    };
    assert!(id.as_str().starts_with("web-"), "{id}");
    assert_eq!(message.text, "hi there");
    assert_eq!(message.conversation.id, chat());
    assert_eq!(message.conversation.kind, ConversationKind::Direct);
    assert_eq!(message.sender.as_str(), "owner");
    assert!(message.sender_authenticated);
    assert_eq!(
        message.message_id.as_ref().map(|m| m.as_str()),
        Some(id.as_str())
    );
}

#[tokio::test]
async fn bad_messages_are_refused_with_the_text_kept() {
    let env = env().await;
    let path = format!("/chats/{}/messages", chat());
    let response = env.htmx_post(&path, "text=+++").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body_text(response).await.contains("Write a message first."));

    let long = "x".repeat(MAX_MESSAGE_BYTES + 1);
    let response = env.htmx_post(&path, &format!("text={long}")).await;
    let form = body_text(response).await;
    assert!(form.contains("too long"), "{form}");
    assert!(form.contains(&long), "the text is kept");

    let response = env
        .request(
            Some(owner()),
            Method::POST,
            &path,
            &[],
            Some(&format!("text=&csrf={CSRF}")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(env.pending_texts().await.is_empty());
}

#[tokio::test]
async fn events_stay_pending_until_acked_and_come_again_on_a_new_stream() {
    let env = env().await;
    let user = owner();
    let a = env
        .channel
        .post(&user, &chat(), "a", Timestamp::now())
        .unwrap();
    env.channel
        .post(&user, &chat(), "b", Timestamp::now())
        .unwrap();

    let mut old = env.channel.events().await.unwrap();
    let status = old.next().await.unwrap().unwrap();
    env.channel.ack(status.id()).await.unwrap();
    assert_eq!(env.pending_texts().await, ["a", "b"]);
    // Opening a stream ended the earlier one.
    let rest: Vec<_> = old.collect().await;
    assert_eq!(
        rest.len(),
        2,
        "the old stream delivered what it had, then ended"
    );

    env.channel.ack(&a).await.unwrap();
    env.channel.ack(&a).await.unwrap();
    assert_eq!(env.pending_texts().await, ["b"]);

    // A stream that's open gets new events as they're posted.
    let mut live = env.channel.events().await.unwrap();
    assert!(live.next().await.is_some(), "b");
    env.channel
        .post(&user, &chat(), "c", Timestamp::now())
        .unwrap();
    let ChannelEvent::Message { message, .. } = live.next().await.unwrap().unwrap() else {
        panic!("not a message");
    };
    assert_eq!(message.text, "c");
}

#[tokio::test]
async fn posting_fails_while_too_many_events_wait() {
    let env = env().await;
    let user = owner();
    // The status event is pending too.
    for i in 1..MAX_PENDING {
        env.channel
            .post(&user, &chat(), &format!("m{i}"), Timestamp::now())
            .unwrap();
    }
    assert_eq!(
        env.channel
            .post(&user, &chat(), "one more", Timestamp::now()),
        Err(Busy)
    );
    let response = env
        .request(
            Some(owner()),
            Method::POST,
            &format!("/chats/{}/messages", chat()),
            &[],
            Some(&format!("text=x&csrf={CSRF}")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn sent_messages_reach_open_pages_and_only_conversations_are_targets() {
    let env = env().await;
    let mut sse = Sse::open(&env, &format!("/chats/{}/events", chat()), &[]).await;
    let png = MediaRef {
        id: doozers_types::MediaId::from_digest([7; 32]),
        media_type: "image/png".into(),
        size: 1,
        name: Some("pic.png".into()),
    };
    env.channel
        .send(OutboundMessage {
            target: SendTarget::Conversation(chat()),
            markdown: "**done** ![x](https://evil.example/?d=1)".into(),
            attachments: vec![png.clone()],
            reply_to: None,
        })
        .await
        .unwrap();
    let event = sse.next().await;
    assert!(event.data.contains("msg assistant"), "{}", event.data);
    assert!(
        event.data.contains("<strong>done</strong>"),
        "{}",
        event.data
    );
    assert!(!event.data.contains("evil.example"), "{}", event.data);
    assert!(
        event
            .data
            .contains(&format!(r#"<img src="/blobs/{}""#, png.id)),
        "{}",
        event.data
    );
    assert_eq!(event.id, Some(env.channel.hubs.cursor(1)));

    let err = env
        .channel
        .send(OutboundMessage {
            target: SendTarget::Recipient(NativeId::new("owner").unwrap()),
            markdown: "hi".into(),
            attachments: Vec::new(),
            reply_to: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
}

#[tokio::test]
async fn event_streams_resume_after_their_cursor_and_flag_stale_ones() {
    let env = env().await;
    let user = owner();
    env.channel
        .post(&user, &chat(), "one", Timestamp::now())
        .unwrap();
    let page_cursor = env.channel.hubs.cursor(1);
    // Posted between the page render and the stream opening.
    env.channel
        .post(&user, &chat(), "two", Timestamp::now())
        .unwrap();

    let path = format!("/chats/{}/events?after={page_cursor}", chat());
    let mut sse = Sse::open(&env, &path, &[]).await;
    let event = sse.next().await;
    assert!(event.data.contains("two"), "{}", event.data);
    assert_eq!(event.id, Some(env.channel.hubs.cursor(2)));

    // A reconnecting EventSource sends Last-Event-ID, which wins over the page's cursor.
    let last = env.channel.hubs.cursor(2);
    env.channel
        .post(&user, &chat(), "three", Timestamp::now())
        .unwrap();
    let mut sse = Sse::open(&env, &path, &[("last-event-id", &last)]).await;
    assert!(sse.next().await.data.contains("three"));

    // A cursor from before a restart: the page is told to reload.
    let path = format!("/chats/{}/events?after=0000000000000000-1", chat());
    let mut sse = Sse::open(&env, &path, &[]).await;
    let event = sse.next().await;
    assert!(event.data.contains(r#"id="chat-notice""#), "{}", event.data);
    assert!(event.data.contains("hx-swap-oob"), "{}", event.data);
}

#[tokio::test]
async fn event_streams_end_on_shutdown() {
    let env = env().await;
    let mut sse = Sse::open(&env, &format!("/chats/{}/events", chat()), &[]).await;
    env.channel
        .post(&owner(), &chat(), "before", Timestamp::now())
        .unwrap();
    assert!(sse.next().await.data.contains("before"));
    env.shutdown.cancel();
    assert!(sse.ends().await, "the stream outlived shutdown");
}

#[tokio::test(start_paused = true)]
async fn live_messages_stream_coalesce_and_finish() {
    let env = env().await;
    let mut sse = Sse::open(&env, &format!("/chats/{}/events", chat()), &[]).await;
    let sink = env.channel.deltas().unwrap();

    let mut writer = sink.start(&chat()).unwrap();
    let started = sse.next().await;
    assert!(
        started.data.contains("msg assistant live"),
        "{}",
        started.data
    );
    assert!(
        !started.data.contains("hx-swap-oob"),
        "appended, not swapped"
    );
    let element = started
        .data
        .split('"')
        .nth(1)
        .expect("an id attribute")
        .to_owned();

    // Within the update interval: coalesced, nothing sent.
    writer.push("Hel");
    tokio::time::advance(LIVE_UPDATE_INTERVAL).await;
    writer.push("lo **wor");
    let update = sse.next().await;
    assert!(
        update.data.starts_with(&format!(
            r#"<article hx-swap-oob="true" id="{element}" class="msg assistant live">"#
        )),
        "{}",
        update.data
    );
    assert!(update.data.contains("Hello"), "{}", update.data);

    // A page loaded now shows the live message as it is.
    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert!(page.contains(&format!(r#"id="{element}" class="msg assistant live""#)));

    writer.push("ld**");
    writer.finish().await.unwrap();
    let done = sse.next().await;
    assert!(
        done.data.starts_with(&format!(
            r#"<article hx-swap-oob="true" id="{element}" class="msg assistant">"#
        )),
        "{}",
        done.data
    );
    assert!(
        done.data.contains("Hello <strong>world</strong>"),
        "{}",
        done.data
    );
    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert!(
        !page.contains(&element),
        "a finished message comes from the transcript"
    );

    // Dropped unfinished: kept, marked cut off.
    let mut writer = sink.start(&chat()).unwrap();
    sse.next().await;
    writer.push("partial");
    drop(writer);
    let cut = sse.next().await;
    assert!(cut.data.contains("cut-off"), "{}", cut.data);
    assert!(cut.data.contains("partial"), "{}", cut.data);
}

#[tokio::test]
async fn approval_buttons_answer_with_the_code() {
    let env = env().await;
    let mut sse = Sse::open(&env, &format!("/chats/{}/events", chat()), &[]).await;
    assert!(matches!(
        env.channel.send_approval(&chat(), "bad code", "x"),
        Err(Error::InvalidRequest(_))
    ));
    env.channel
        .send_approval(&chat(), "4821", "Allow `shell__exec`?")
        .unwrap();
    let prompt = sse.next().await;
    assert!(
        prompt.data.starts_with(r#"<form id="a-4821""#),
        "{}",
        prompt.data
    );
    assert!(
        prompt.data.contains("<code>shell__exec</code>"),
        "{}",
        prompt.data
    );
    assert!(!prompt.data.contains("csrf"), "{}", prompt.data);

    // A page load shows the pending prompt, with the CSRF field for plain form posts.
    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert!(page.contains(r#"<form id="a-4821""#), "{page}");
    assert!(page.contains(r#"name="csrf" value="csrf-token-for-tests""#));

    let path = format!("/chats/{}/approvals", chat());
    for form in ["code=4821&answer=maybe", "code=48+21&answer=yes"] {
        let response = env.htmx_post(&path, form).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{form}");
    }
    let response = env
        .request(
            Some(owner()),
            Method::POST,
            &path,
            &[("hx-request", "true")],
            Some("code=4821&answer=yes!"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "no CSRF token");

    let response = env.htmx_post(&path, "code=4821&answer=yes%21").await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(env.pending_texts().await, ["yes! 4821"]);
    let posted = sse.next().await;
    assert!(posted.data.contains("yes! 4821"), "{}", posted.data);
    let answered = sse.next().await;
    assert!(
        answered
            .data
            .starts_with(r#"<div hx-swap-oob="true" id="a-4821" class="approval answered">"#),
        "{}",
        answered.data
    );
    assert!(answered.data.contains("Sent: yes!"), "{}", answered.data);
    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert!(!page.contains(r#"<form id="a-4821""#), "{page}");
    assert!(page.contains("Sent: yes!"), "{page}");

    // The approvals code reports the outcome; then the prompt is gone from page loads.
    assert!(
        env.channel
            .resolve_approval(&chat(), "4821", ApprovalOutcome::AllowedForSession)
    );
    let resolved = sse.next().await;
    assert!(
        resolved
            .data
            .starts_with(r#"<div hx-swap-oob="true" id="a-4821" class="approval answered">"#),
        "{}",
        resolved.data
    );
    assert!(
        resolved
            .data
            .contains("Allowed for the rest of this session."),
        "{}",
        resolved.data
    );
    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert!(!page.contains("a-4821"), "{page}");
    assert!(
        !env.channel
            .resolve_approval(&chat(), "4821", ApprovalOutcome::Denied)
    );
    let elsewhere = ConversationId::new("f".repeat(32)).unwrap();
    assert!(
        !env.channel
            .resolve_approval(&elsewhere, "4821", ApprovalOutcome::Denied)
    );

    // A prompt that ends without a button (a timeout, a typed reply) is retired the same way.
    env.channel.send_approval(&chat(), "99", "Again?").unwrap();
    sse.next().await;
    assert!(
        env.channel
            .resolve_approval(&chat(), "99", ApprovalOutcome::Expired)
    );
    assert!(sse.next().await.data.contains("Expired"));

    // Without htmx: a redirect back, `no` posted.
    let response = env
        .request(
            Some(owner()),
            Method::POST,
            &path,
            &[],
            Some(&format!("code=77&answer=no&csrf={CSRF}")),
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(env.pending_texts().await, ["yes! 4821", "no 77"]);
}

#[tokio::test]
async fn blobs_are_served_safely_to_the_user_whose_message_refers_to_them() {
    let env = env().await;
    let now = Timestamp::now();
    let png = env
        .blobs
        .put(
            &b"\x89PNG fake"[..],
            1024,
            BlobMeta {
                media_type: "image/png".into(),
                name: Some("pic.png".into()),
            },
            now,
        )
        .await
        .unwrap();
    let page = env
        .blobs
        .put(
            &b"<script>alert(1)</script>"[..],
            1024,
            BlobMeta {
                media_type: "text/html".into(),
                name: Some("evil page.html".into()),
            },
            now,
        )
        .await
        .unwrap();

    // The owner's chat refers to both; nothing refers to the third.
    store_chat(
        &env,
        &chat(),
        INSTANCE,
        vec![
            stored(Role::User, vec![Block::Image { media: png.clone() }]),
            stored(
                Role::Assistant,
                vec![Block::Document {
                    media: page.clone(),
                    name: None,
                }],
            ),
        ],
    )
    .await;
    let unreferenced = env
        .blobs
        .put(
            &b"nobody's"[..],
            1024,
            BlobMeta {
                media_type: "image/png".into(),
                name: None,
            },
            now,
        )
        .await
        .unwrap();

    let response = env.get(&format!("/blobs/{}", png.id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers[CONTENT_TYPE], "image/png");
    assert_eq!(headers[X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(headers[CONTENT_DISPOSITION], "inline");
    let policies: Vec<_> = headers.get_all(CONTENT_SECURITY_POLICY).iter().collect();
    assert_eq!(policies[0], "sandbox", "{policies:?}");
    assert_eq!(policies.len(), 2, "sandbox plus the base policy");
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"\x89PNG fake");

    let response = env.get(&format!("/blobs/{}", page.id)).await;
    assert_eq!(response.headers()[CONTENT_TYPE], "text/html");
    assert_eq!(
        response.headers()[CONTENT_DISPOSITION],
        "attachment; filename*=UTF-8''evil%20page.html"
    );
    assert_eq!(response.headers()[CONTENT_SECURITY_POLICY], "sandbox");

    let missing = "f".repeat(64);
    let response = env.get(&format!("/blobs/{missing}")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // A blob no message of the user's refers to is not found, however it is stored.
    let response = env.get(&format!("/blobs/{}", unreferenced.id)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Another user, an admin included, doesn't get the owner's blobs: they are no admin page's.
    for (user, role) in [
        ("someone", doozers_store::Role::Member),
        ("root", doozers_store::Role::Admin),
    ] {
        let stranger = WebUser {
            user: UserId::new(user).unwrap(),
            role,
            ..owner()
        };
        for id in [&png.id, &page.id] {
            let response = env
                .request(
                    Some(stranger.clone()),
                    Method::GET,
                    &format!("/blobs/{id}"),
                    &[],
                    None,
                )
                .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{user} {id}");
            assert!(body_text(response).await.contains("Not found"));
        }
    }
}

/// Finds the web channel for its instance.
struct Lookup(Arc<WebChannel>);

impl ChannelLookup for Lookup {
    fn channel(&self, instance: &str) -> Option<Arc<dyn Channel>> {
        (instance == INSTANCE).then(|| self.0.clone() as Arc<dyn Channel>)
    }
}

/// Stores the turn's messages and a reply, as the agent loop does, and sends the reply.
struct EchoRunner {
    store: Store,
}

#[async_trait]
impl TurnRunner for EchoRunner {
    async fn run_turn(&self, turn: Turn) -> TurnOutcome {
        let texts: Vec<String> = turn
            .messages
            .iter()
            .map(|m| m.message.text.clone())
            .collect();
        let reply = format!("You said: *{}*", texts.join(" "));
        let mut messages: Vec<Message> = texts
            .iter()
            .map(|t| stored(Role::User, vec![text(t)]))
            .collect();
        messages.push(stored(Role::Assistant, vec![text(&reply)]));
        self.store
            .append_messages(turn.session.id, messages)
            .await
            .unwrap();
        turn.reply.unwrap().send(reply).await.unwrap();
        TurnOutcome::Completed
    }
}

#[tokio::test]
async fn a_posted_message_reaches_the_agent_and_the_reply_reaches_the_page() {
    let env = env().await;
    let agents: Arc<dyn AgentPolicies> = Arc::new(HashMap::from([(
        AgentId::new("jeeves").unwrap(),
        AgentPolicy::default(),
    )]));
    let router = MessageRouter::new(
        owner_config(&["web:owner"]),
        RouteList::new(vec![Route {
            channel: None,
            kind: None,
            conversation: None,
            agent: AgentId::new("jeeves").unwrap(),
        }]),
        agents.clone(),
    );
    let sessions = Sessions::start(
        SessionsOptions {
            store: env.store.clone(),
            router,
            agents,
            channels: Arc::new(Lookup(env.channel.clone())),
            runner: Arc::new(EchoRunner {
                store: env.store.clone(),
            }),
            approvals: Arc::new(NoPendingApprovals),
            env_resets: Arc::new(crate::sessions::NoEnvResets),
            clock: Arc::new(SystemClock),
        },
        env.shutdown.clone(),
    )
    .await
    .unwrap();
    sessions.attach_channel(
        InstanceName::new(INSTANCE).unwrap(),
        COMPONENT.to_owned(),
        env.channel.clone(),
    );

    let mut sse = Sse::open(&env, &format!("/chats/{}/events", chat()), &[]).await;
    let response = env
        .htmx_post(&format!("/chats/{}/messages", chat()), "text=ping")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(sse.next().await.data.contains("msg user"));
    let reply = sse.next().await;
    assert!(
        reply.data.contains("You said: <em>ping</em>"),
        "{}",
        reply.data
    );

    // Acked once stored: nothing is pending any more.
    tokio::time::timeout(WAIT, async {
        while !lock(&env.channel.inbound).pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the events were never acked");

    // A reload shows both from the transcript, and the chat is listed.
    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert!(page.contains("<p>ping</p>"), "{page}");
    assert!(page.contains("You said: <em>ping</em>"), "{page}");
    let list = body_text(env.get("/").await).await;
    assert!(
        list.contains(r#"<span class="chat-item-title">ping</span>"#),
        "{list}"
    );

    env.shutdown.cancel();
    sessions.closed().await;
}

#[test]
fn approval_answers_and_codes() {
    for (value, answer) in [
        ("yes", ApprovalAnswer::Yes),
        ("yes!", ApprovalAnswer::YesForSession),
        ("no", ApprovalAnswer::No),
    ] {
        assert_eq!(ApprovalAnswer::parse(value), Some(answer));
        assert_eq!(answer.word(), value);
    }
    assert_eq!(ApprovalAnswer::parse("Yes"), None);
    assert!(is_approval_code("4821") && is_approval_code("AbC9"));
    assert!(!is_approval_code("") && !is_approval_code("48 21"));
    assert!(!is_approval_code(&"1".repeat(33)));
    assert!(is_chat_id(new_chat_id().as_str()));
    assert!(!is_chat_id("owner"));
}

// The web login: requests go through the real authentication middleware.

/// The session cookie as a request header.
fn session(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}")
}

/// A response's `Set-Cookie`.
fn set_cookie(response: &Response) -> &str {
    response.headers()[SET_COOKIE].to_str().unwrap()
}

/// The token a `Set-Cookie` for the session cookie sets.
fn cookie_token(set_cookie: &str) -> String {
    let value = set_cookie.strip_prefix("doozers_session=").unwrap();
    value.split(';').next().unwrap().to_owned()
}

/// A nonce for the sign-in POSTs of tests, sent as the nonce cookie and as the form's field.
const NONCE: &str = "0123456789abcdef0123456789abcdef";

/// A one-time `kind` link for `user`, made now: its token.
async fn invite_token(env: &Env, user: &UserId, kind: InviteKind) -> String {
    open_users(env)
        .await
        .create_invite(user, kind, None, Timestamp::now())
        .await
        .unwrap()
        .token
}

/// Spends the link `token` on `app` with `headers`, as a browser does: with the nonce of the form
/// it opened in its cookie and in the form.
async fn redeem(app: axum::Router, token: &str, headers: &[(&str, &str)]) -> Response {
    let cookie = format!("{NONCE_COOKIE}={NONCE}");
    let form = format!("token={token}&nonce={NONCE}");
    let mut sent = vec![("cookie", cookie.as_str())];
    sent.extend_from_slice(headers);
    call(app, Method::POST, "/invite", &sent, Some(&form)).await
}

/// Logs the owner in with a recovery link on the main listener; the session token.
async fn log_in(env: &Env) -> String {
    let token = invite_token(env, &identity::owner(), InviteKind::Recovery).await;
    let response = redeem(env.main_app(), &token, &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    cookie_token(set_cookie(&response))
}

fn proxy_config(proxy: serde_json::Value) -> WebConfig {
    WebConfig::from_json(&serde_json::json!({ "proxy": proxy })).unwrap()
}

/// Creates the active user `id` with no identity and the role `role`; their id.
async fn add_user(users: &Users, id: &str, role: doozers_store::Role) -> UserId {
    let new = NewUser {
        id: UserId::new(id).unwrap(),
        username: id.to_owned(),
        display_name: id.to_uppercase(),
        role,
        timezone: None,
        default_identity: None,
    };
    users
        .create(new, vec![], Timestamp::now())
        .await
        .unwrap()
        .id
}

/// The nonce a `Set-Cookie` for the nonce cookie sets.
fn nonce_of(set_cookie: &str) -> String {
    let value = set_cookie
        .strip_prefix(&format!("{NONCE_COOKIE}="))
        .unwrap_or_else(|| panic!("{set_cookie}"));
    value.split(';').next().unwrap().to_owned()
}

/// Every `Set-Cookie` of a response.
fn set_cookies(response: &Response) -> Vec<&str> {
    response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect()
}

#[tokio::test]
async fn invite_links_use_the_bound_address_unless_a_public_url_is_set() {
    let env = env().await;
    // `http_listen = "...:0"`: `serve` passes the address the listener actually bound.
    env.channel
        .set_listen_addr("0.0.0.0:41234".parse().unwrap());
    assert_eq!(
        env.channel.invite_url("abc"),
        "http://127.0.0.1:41234/invite/abc"
    );

    let config =
        WebConfig::from_json(&serde_json::json!({"public_url": "https://doozers.example.com"}))
            .unwrap();
    let env = env_with(config, &["web:owner"]).await;
    env.channel
        .set_listen_addr("127.0.0.1:41234".parse().unwrap());
    assert_eq!(
        env.channel.invite_url("abc"),
        "https://doozers.example.com/invite/abc"
    );
}

#[tokio::test]
async fn an_invite_sets_up_a_new_user_once() {
    let env = env().await;
    let users = open_users(&env).await;
    let newbie = add_user(&users, "newbie", doozers_store::Role::Member).await;
    let invite = users
        .create_invite(
            &newbie,
            InviteKind::Invite,
            Some(&identity::owner()),
            Timestamp::now(),
        )
        .await
        .unwrap();
    let url = env.channel.invite_url(&invite.token);
    // `0.0.0.0:8080` becomes loopback.
    let path = url.strip_prefix("http://127.0.0.1:8080").unwrap();
    assert_eq!(path, format!("/invite/{}", invite.token));

    // Opening it (twice: a link preview, then the person) only shows a button, and hands the
    // browser the nonce its form carries.
    let mut nonce = String::new();
    for _ in 0..2 {
        let response = call(env.main_app(), Method::GET, path, &[], None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
        let cookie = set_cookie(&response).to_owned();
        assert!(
            cookie.ends_with("; Path=/; Max-Age=900; HttpOnly; SameSite=Strict"),
            "{cookie}"
        );
        nonce = nonce_of(&cookie);
        let html = body_text(response).await;
        assert!(
            html.contains(r#"<form method="post" action="/invite">"#),
            "{html}"
        );
        assert!(html.contains(&invite.token), "{html}");
        assert!(
            html.contains(&format!(r#"name="nonce" value="{nonce}""#)),
            "{html}"
        );
        assert!(html.contains("NEWBIE, you're invited"), "{html}");
    }

    let cookie = format!("{NONCE_COOKIE}={nonce}");
    let form = format!("token={}&nonce={nonce}", invite.token);
    let response = call(
        env.main_app(),
        Method::POST,
        "/invite",
        &[("cookie", &cookie)],
        Some(&form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/setup");
    assert_eq!(response.headers()[CACHE_CONTROL], "no-store");
    let cookies = set_cookies(&response);
    assert_eq!(cookies.len(), 2, "{cookies:?}");
    assert!(
        cookies[0].ends_with("; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax"),
        "{cookies:?}"
    );
    assert_eq!(
        cookies[1],
        format!("{NONCE_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict")
    );
    let token = cookie_token(cookies[0]);
    assert_eq!(token.len(), 64);
    let stored = env
        .store
        .web_session(&token, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.user, newbie);
    assert_eq!(stored.method, SignInMethod::Invite);
    assert_eq!(
        stored.expires_at.as_millisecond() - stored.created_at.as_millisecond(),
        30 * 24 * 60 * 60 * 1000
    );
    // The user is signed in as `web:newbie`, the identity the invite gave them.
    let directory = env.channel.users().get();
    let found = directory.resolve("web", &NativeId::new("newbie").unwrap());
    assert_eq!(found.map(|user| &user.id), Some(&newbie));

    // The cookie logs in, and the setup page is where they land. It says that the link is the
    // only way to sign in so far.
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup",
        &[("cookie", &session(&token))],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("Set up how you sign in"), "{html}");
    assert!(html.contains("You're signed in as NEWBIE"), "{html}");
    assert!(html.contains("is the only way to sign in so far"), "{html}");

    // Single use, however it is asked.
    let response = call(
        env.main_app(),
        Method::POST,
        "/invite",
        &[("cookie", &cookie)],
        Some(&form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::GONE);
    assert!(!response.headers().contains_key(SET_COOKIE));
    let response = call(env.main_app(), Method::GET, path, &[], None).await;
    assert_eq!(response.status(), StatusCode::GONE);
    let html = body_text(response).await;
    assert!(html.contains("expired or was already used"), "{html}");
    assert!(html.contains("doozers admin login-link"), "{html}");
}

#[tokio::test]
async fn the_setup_page_needs_a_login() {
    let env = env().await;
    let response = call(env.main_app(), Method::GET, "/setup", &[], None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_recovery_link_signs_a_user_in_and_keeps_every_way_they_had() {
    let env = env().await;
    let users = open_users(&env).await;
    let owner = identity::owner();
    // The owner has a session from some other way of signing in, and identities.
    let older = "d".repeat(64);
    let now = Timestamp::now();
    env.store
        .web_session_create(
            &older,
            owner.clone(),
            SignInMethod::Password,
            now,
            now.checked_add(WEB_SESSION_TTL).unwrap(),
        )
        .await
        .unwrap();
    let before = users.list().get().user(&owner).unwrap().identities.clone();

    let token = invite_token(&env, &owner, InviteKind::Recovery).await;
    let path = format!("/invite/{token}");
    let response = call(env.main_app(), Method::GET, &path, &[], None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("Recover your account"), "{html}");
    assert!(
        html.contains("keeps every way you can already sign in"),
        "{html}"
    );

    let response = redeem(env.main_app(), &token, &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[LOCATION], "/setup");
    let new = cookie_token(set_cookie(&response));
    let stored = env
        .store
        .web_session(&new, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (stored.user, stored.method),
        (owner.clone(), SignInMethod::Recovery)
    );

    // The recovered session reaches the setup page, to add a method. Nothing the user had is gone:
    // the identities, and the session of another method.
    let response = call(
        env.main_app(),
        Method::GET,
        "/setup",
        &[("cookie", &session(&new))],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(users.list().get().user(&owner).unwrap().identities, before);
    let still = env
        .store
        .web_session(&older, Timestamp::now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.method, SignInMethod::Password);
}

#[tokio::test]
async fn a_new_session_replaces_the_browsers_old_cookie_session() {
    let env = env().await;
    let old = log_in(&env).await;
    let token = invite_token(&env, &identity::owner(), InviteKind::Recovery).await;
    let response = redeem(env.main_app(), &token, &[("cookie", &session(&old))]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        env.store.web_session(&old, Timestamp::now()).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn expired_and_unknown_links_log_nobody_in() {
    let env = env().await;
    let users = open_users(&env).await;
    let created = Timestamp::now()
        .checked_sub(INVITE_TTL + Duration::from_secs(1))
        .unwrap();
    let expired = users
        .create_invite(&identity::owner(), InviteKind::Invite, None, created)
        .await
        .unwrap();
    assert!(expired.expires_at < Timestamp::now());
    let response = redeem(env.main_app(), &expired.token, &[]).await;
    assert_eq!(response.status(), StatusCode::GONE);
    assert!(!response.headers().contains_key(SET_COOKIE));
    let path = format!("/invite/{}", expired.token);
    let response = call(env.main_app(), Method::GET, &path, &[], None).await;
    assert_eq!(response.status(), StatusCode::GONE);
    assert!(!response.headers().contains_key(SET_COOKIE));
    let html = body_text(response).await;
    assert!(html.contains("doozers admin login-link"), "{html}");

    for token in ["nope", &"0".repeat(64)] {
        let path = format!("/invite/{token}");
        let response = call(env.main_app(), Method::GET, &path, &[], None).await;
        assert_eq!(response.status(), StatusCode::GONE, "{token}");
        let response = redeem(env.main_app(), token, &[]).await;
        assert_eq!(response.status(), StatusCode::GONE, "{token}");
    }
}

#[tokio::test]
async fn a_sign_in_post_needs_the_nonce_of_its_browsers_form() {
    let env = env().await;
    let token = invite_token(&env, &identity::owner(), InviteKind::Recovery).await;
    let nonce = NONCE;
    let other = nonce.replace('0', "f");
    let cookie = format!("{NONCE_COOKIE}={nonce}");
    let wrong_cookie = format!("{NONCE_COOKIE}={other}");
    let with_field = format!("token={token}&nonce={nonce}");
    let without_field = format!("token={token}");
    let other_field = format!("token={token}&nonce={other}");
    let empty_field = format!("token={token}&nonce=");
    let cases = [
        ("no cookie", vec![], &with_field),
        (
            "no field",
            vec![("cookie", cookie.as_str())],
            &without_field,
        ),
        (
            "another field",
            vec![("cookie", cookie.as_str())],
            &other_field,
        ),
        (
            "another cookie",
            vec![("cookie", wrong_cookie.as_str())],
            &with_field,
        ),
        // Other cookies are no nonce.
        (
            "no nonce cookie",
            vec![("cookie", "theme=dark")],
            &with_field,
        ),
        (
            "an empty nonce",
            vec![("cookie", cookie.as_str())],
            &empty_field,
        ),
    ];
    for (what, headers, form) in cases {
        let response = call(
            env.main_app(),
            Method::POST,
            "/invite",
            &headers,
            Some(form.as_str()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{what}");
        assert!(!response.headers().contains_key(SET_COOKIE), "{what}");
    }
    // Nobody was signed in, and the link wasn't spent by the refusals.
    let now = Timestamp::now();
    assert!(env.store.user_invite(&token, now).await.unwrap().is_some());
    let response = redeem(env.main_app(), &token, &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn a_page_reuses_the_nonce_a_browser_already_has() {
    let env = env().await;
    let token = invite_token(&env, &identity::owner(), InviteKind::Recovery).await;
    let path = format!("/invite/{token}");
    let cookie = format!("{NONCE_COOKIE}={NONCE}");
    // A second tab opened before the first is submitted keeps the first's form working.
    let response = call(
        env.main_app(),
        Method::GET,
        &path,
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(nonce_of(set_cookie(&response)), NONCE);
    // A cookie that isn't a nonce is replaced.
    let response = call(
        env.main_app(),
        Method::GET,
        &path,
        &[("cookie", "doozers_login_nonce=short")],
        None,
    )
    .await;
    let made = nonce_of(set_cookie(&response));
    assert_eq!(made.len(), 32);
    assert_ne!(made, "short");
}

#[tokio::test]
async fn a_link_for_a_user_who_cant_sign_in_is_refused_and_not_spent() {
    let env = env().await;
    let users = open_users(&env).await;
    let ann = add_user(&users, "ann", doozers_store::Role::Member).await;
    let token = invite_token(&env, &ann, InviteKind::Invite).await;
    users
        .set_state(&ann, doozers_store::UserState::Disabled, Timestamp::now())
        .await
        .unwrap();
    let response = redeem(env.main_app(), &token, &[]).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response.headers().contains_key(SET_COOKIE));
    let found = env
        .store
        .user_invite(&token, Timestamp::now())
        .await
        .unwrap();
    assert!(found.is_some(), "not spent");
    // Enabled again, the same link works.
    users
        .set_state(&ann, doozers_store::UserState::Active, Timestamp::now())
        .await
        .unwrap();
    let response = redeem(env.main_app(), &token, &[]).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn expired_unknown_and_orphaned_sessions_are_logged_out() {
    let env = env().await;
    let long_ago = Timestamp::now()
        .checked_sub(WEB_SESSION_TTL + Duration::from_secs(60))
        .unwrap();
    let expired = "e".repeat(64);
    env.store
        .web_session_create(
            &expired,
            identity::owner(),
            SignInMethod::Recovery,
            long_ago,
            long_ago.checked_add(WEB_SESSION_TTL).unwrap(),
        )
        .await
        .unwrap();
    for token in [expired.as_str(), &"f".repeat(64)] {
        let response = call(
            env.main_app(),
            Method::GET,
            "/",
            &[("cookie", &session(token))],
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{token}");
        assert_eq!(
            set_cookie(&response),
            "doozers_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"
        );
    }

    // A live session whose `web:<user>` identity is gone logs nobody in.
    let token = log_in(&env).await;
    let moved = env_with(WebConfig::default(), &["irc:libera.chat:owner"]).await;
    let now = Timestamp::now();
    moved
        .store
        .web_session_create(
            &token,
            identity::owner(),
            SignInMethod::Recovery,
            now,
            now.checked_add(WEB_SESSION_TTL).unwrap(),
        )
        .await
        .unwrap();
    let response = call(
        moved.main_app(),
        Method::GET,
        "/",
        &[("cookie", &session(&token))],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_sessions_are_deleted_until_shutdown() {
    let env = env().await;
    let long_ago = Timestamp::now()
        .checked_sub(WEB_SESSION_TTL + Duration::from_secs(60))
        .unwrap();
    let expired = "e".repeat(64);
    env.store
        .web_session_create(
            &expired,
            identity::owner(),
            SignInMethod::Recovery,
            long_ago,
            long_ago.checked_add(WEB_SESSION_TTL).unwrap(),
        )
        .await
        .unwrap();
    let live = log_in(&env).await;
    let task = tokio::spawn(env.app.clone().expire_sessions());
    tokio::time::timeout(WAIT, async {
        // Looked up as of before it expired, so it's found until the row is gone.
        while env
            .store
            .web_session(&expired, long_ago)
            .await
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the expired session was never deleted");
    assert!(
        env.store
            .web_session(&live, Timestamp::now())
            .await
            .unwrap()
            .is_some()
    );
    env.shutdown.cancel();
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
}

#[tokio::test]
async fn logging_out_needs_the_csrf_token_and_ends_the_session() {
    let env = env().await;
    let token = log_in(&env).await;
    let cookie = session(&token);

    let response = call(
        env.main_app(),
        Method::POST,
        "/logout",
        &[("cookie", &cookie)],
        Some("csrf=wrong"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let now = Timestamp::now();
    assert!(env.store.web_session(&token, now).await.unwrap().is_some());

    let form = format!("csrf={}", auth::cookie_csrf(&token));
    let response = call(
        env.main_app(),
        Method::POST,
        "/logout",
        &[("cookie", &cookie)],
        Some(&form),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        set_cookie(&response),
        "doozers_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"
    );
    assert!(env.store.web_session(&token, now).await.unwrap().is_none());
    let response = call(
        env.main_app(),
        Method::GET,
        "/",
        &[("cookie", &cookie)],
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_cookie_login_posts_with_its_csrf_token() {
    let env = env().await;
    let token = log_in(&env).await;
    let cookie = session(&token);
    let csrf = auth::cookie_csrf(&token);
    let path = format!("/chats/{}", chat());
    let page = body_text(
        call(
            env.main_app(),
            Method::GET,
            &path,
            &[("cookie", &cookie)],
            None,
        )
        .await,
    )
    .await;
    assert!(page.contains(&csrf), "{page}");

    let messages = format!("{path}/messages");
    let response = call(
        env.main_app(),
        Method::POST,
        &messages,
        &[("cookie", &cookie)],
        Some("text=hi"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = call(
        env.main_app(),
        Method::POST,
        &messages,
        &[
            ("cookie", &cookie),
            ("hx-request", "true"),
            ("x-csrf-token", &csrf),
        ],
        Some("text=hi"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let senders: Vec<String> = env
        .pending()
        .await
        .into_iter()
        .filter_map(|event| match event {
            ChannelEvent::Message { message, .. } => Some(message.sender.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(senders, ["owner"]);
}

#[tokio::test]
async fn an_https_public_url_makes_the_cookies_secure() {
    let config =
        WebConfig::from_json(&serde_json::json!({"public_url": "https://doozers.example.com"}))
            .unwrap();
    let env = env_with(config, &["web:owner"]).await;
    let token = invite_token(&env, &identity::owner(), InviteKind::Recovery).await;
    let url = env.channel.invite_url(&token);
    assert!(
        url.starts_with("https://doozers.example.com/invite/"),
        "{url}"
    );
    let response = call(
        env.main_app(),
        Method::GET,
        &format!("/invite/{token}"),
        &[],
        None,
    )
    .await;
    assert!(set_cookie(&response).ends_with("; SameSite=Strict; Secure"));
    let response = redeem(env.main_app(), &token, &[]).await;
    let cookies = set_cookies(&response);
    assert_eq!(cookies.len(), 2, "{cookies:?}");
    assert!(
        cookies[0].ends_with("; SameSite=Lax; Secure"),
        "{cookies:?}"
    );
    assert!(
        cookies[1].ends_with("; SameSite=Strict; Secure"),
        "{cookies:?}"
    );
}

#[tokio::test]
async fn the_identity_header_counts_only_on_the_proxy_listener() {
    let env = env_with(
        proxy_config(serde_json::json!({"identity_header": "x-user"})),
        &["web:owner", "web:alice@example.com"],
    )
    .await;
    let alice = [("x-user", "alice@example.com")];

    let response = call(env.main_app(), Method::GET, "/", &alice, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = call(env.proxy_app(), Method::GET, "/", &[], None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = call(env.proxy_app(), Method::GET, "/", &alice, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(!html.contains("/logout"), "nothing to log out of: {html}");

    // Someone no user has the identity of is refused.
    let mallory = [("x-user", "mallory@example.com")];
    let response = call(env.proxy_app(), Method::GET, "/", &mallory, None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Posts carry the header's identity and need the login's CSRF token.
    let sender = NativeId::new("alice@example.com").unwrap();
    let csrf = auth::proxy_csrf(&env.channel.proxy_key, &sender);
    let path = format!("/chats/{}", chat());
    let page = body_text(call(env.proxy_app(), Method::GET, &path, &alice, None).await).await;
    assert!(page.contains(&csrf), "{page}");
    let messages = format!("{path}/messages");
    let response = call(
        env.proxy_app(),
        Method::POST,
        &messages,
        &alice,
        Some("text=hi"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = call(
        env.proxy_app(),
        Method::POST,
        &messages,
        &[alice[0], ("x-csrf-token", &csrf)],
        Some("text=hi"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let pending = env.pending().await;
    let Some(ChannelEvent::Message { message, .. }) = pending.last() else {
        panic!("no message: {pending:?}");
    };
    assert_eq!(message.sender, sender);
    assert!(message.sender_authenticated);

    // `X-Forwarded-Proto` too is believed only from the proxy.
    let https = [("x-forwarded-proto", "https")];
    let token = invite_token(&env, &identity::owner(), InviteKind::Recovery).await;
    let response = redeem(env.main_app(), &token, &https).await;
    assert!(set_cookies(&response).iter().all(|c| !c.contains("Secure")));
    let token = invite_token(&env, &identity::owner(), InviteKind::Recovery).await;
    let response = redeem(env.proxy_app(), &token, &https).await;
    assert!(
        set_cookies(&response)
            .iter()
            .all(|c| c.ends_with("; Secure"))
    );
}

/// The owner, `ann` (a member with the `web` identity `ann@example.com`, and `web:ann`), and
/// `away`, who is disabled (`away@example.com`, and `web:away`).
fn several_users() -> UserList {
    let mut users = vec![User::member("owner", &["web:owner"])];
    users.push(User::member("ann", &["web:ann@example.com", "web:ann"]));
    let mut away = User::member("away", &["web:away@example.com", "web:away"]);
    away.state = doozers_store::UserState::Disabled;
    users.push(away);
    UserList::new(users)
}

#[tokio::test]
async fn the_identity_header_logs_in_the_user_who_has_that_identity() {
    let env = env_with(
        proxy_config(serde_json::json!({"identity_header": "x-user"})),
        &["web:owner"],
    )
    .await;
    env.channel
        .users()
        .set(several_users().get().users().to_vec());

    let ann = [("x-user", "ann@example.com")];
    let response = call(env.proxy_app(), Method::GET, "/", &ann, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    // The login is ann's, and what she posts is from `web:ann@example.com`, which the router
    // resolves to her.
    let sender = NativeId::new("ann@example.com").unwrap();
    let csrf = auth::proxy_csrf(&env.channel.proxy_key, &sender);
    let path = format!("/chats/{}", chat());
    let response = call(
        env.proxy_app(),
        Method::POST,
        &format!("{path}/messages"),
        &[ann[0], ("x-csrf-token", &csrf)],
        Some("text=hi"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let directory = env.channel.users().get();
    assert_eq!(
        directory.resolve("web", &sender).unwrap().id,
        UserId::new("ann").unwrap()
    );
    // A disabled user is refused, and so is an identity nobody has.
    for header in ["away@example.com", "mallory@example.com"] {
        let response = call(
            env.proxy_app(),
            Method::GET,
            "/",
            &[("x-user", header)],
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{header}");
    }
}

#[tokio::test]
async fn a_cookie_session_ends_when_its_user_is_disabled_or_gone() {
    let env = env().await;
    env.channel
        .users()
        .set(several_users().get().users().to_vec());
    let now = Timestamp::now();
    let expires = now.checked_add(WEB_SESSION_TTL).unwrap();
    let cookie = |user: &str| {
        let token = user.repeat(64)[..64].to_owned();
        (token, UserId::new(user).unwrap())
    };
    for (user, status) in [
        ("ann", StatusCode::OK),
        ("away", StatusCode::UNAUTHORIZED),
        ("gone", StatusCode::UNAUTHORIZED),
    ] {
        let (token, id) = cookie(user);
        env.store
            .web_session_create(&token, id, SignInMethod::Recovery, now, expires)
            .await
            .unwrap();
        let response = call(
            env.main_app(),
            Method::GET,
            "/",
            &[("cookie", &session(&token))],
            None,
        )
        .await;
        assert_eq!(response.status(), status, "{user}");
    }
}

#[tokio::test]
async fn a_link_is_for_the_user_it_was_made_for() {
    let env = env().await;
    let users = open_users(&env).await;
    let ann = add_user(&users, "ann", doozers_store::Role::Member).await;
    let bob = add_user(&users, "bob", doozers_store::Role::Member).await;
    let for_ann = invite_token(&env, &ann, InviteKind::Invite).await;
    let for_bob = invite_token(&env, &bob, InviteKind::Invite).await;
    let now = Timestamp::now();
    for (token, user) in [(for_bob, bob), (for_ann, ann)] {
        let response = redeem(env.main_app(), &token, &[]).await;
        let session = cookie_token(set_cookie(&response));
        let stored = env.store.web_session(&session, now).await.unwrap().unwrap();
        assert_eq!(stored.user, user);
    }
}

#[tokio::test]
async fn a_tcp_proxy_listener_needs_the_shared_secret() {
    let secret = "0123456789abcdef";
    let env = env_with(
        proxy_config(serde_json::json!({
            "identity_header": "x-user",
            "listen": "127.0.0.1:0",
            "secret": secret,
        })),
        &["web:owner"],
    )
    .await;
    let owner = ("x-user", "owner");
    for headers in [
        vec![owner],
        vec![owner, ("x-doozers-proxy-secret", "0123456789abcdeF")],
    ] {
        let response = call(env.proxy_app(), Method::GET, "/", &headers, None).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{headers:?}");
    }
    let with_secret = [owner, ("x-doozers-proxy-secret", secret)];
    let response = call(env.proxy_app(), Method::GET, "/", &with_secret, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    // The main listener believes neither.
    let response = call(env.main_app(), Method::GET, "/", &with_secret, None).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_proxy_listener_binds_where_configured() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = crate::paths::DataRoot::new(root_dir.path(), &root_dir.path().join("tmp")).unwrap();
    root.create_layout().unwrap();

    let none = env().await;
    assert!(none.app.bind_proxy(&root).await.unwrap().is_none());

    let unix = env_with(
        proxy_config(serde_json::json!({"identity_header": "x-user"})),
        &["web:owner"],
    )
    .await;
    let listener = unix.app.bind_proxy(&root).await.unwrap().unwrap();
    assert_eq!(
        listener.describe(),
        root.proxy_socket().display().to_string()
    );
    assert!(matches!(
        unix.app.bind_proxy(&root).await,
        Err(ProxyError::InUse(_))
    ));

    let tcp = env_with(
        proxy_config(serde_json::json!({
            "identity_header": "x-user",
            "listen": "127.0.0.1:0",
            "secret": "0123456789abcdef",
        })),
        &["web:owner"],
    )
    .await;
    let listener = tcp.app.bind_proxy(&root).await.unwrap().unwrap();
    assert!(matches!(listener, ProxyListener::Tcp(_)));
    assert!(listener.describe().starts_with("127.0.0.1:"));
}

#[tokio::test]
async fn pages_show_what_the_user_wrote_without_the_time_prefix() {
    let env = env().await;
    store_chat(
        &env,
        &chat(),
        INSTANCE,
        vec![
            stored(
                Role::User,
                vec![text("[Thu 2026-10-22 14:03 PDT] what is on today?")],
            ),
            stored(Role::Assistant, vec![text("Lunch.")]),
        ],
    )
    .await;
    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert!(page.contains("<p>what is on today?</p>"), "{page}");
    assert!(!page.contains("PDT"), "{page}");
    let list = body_text(env.get("/").await).await;
    assert!(
        list.contains(r#"<span class="chat-item-title">what is on today?</span>"#),
        "{list}"
    );
}

#[tokio::test]
async fn a_reload_shows_each_post_once_until_it_is_stored() {
    let env = env().await;
    let path = format!("/chats/{}", chat());
    let response = env
        .htmx_post(&format!("{path}/messages"), "text=first")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let shown = |page: &str| page.matches("<p>first</p>").count();

    // Not taken by the core yet.
    let page = body_text(env.get(&path).await).await;
    assert_eq!(shown(&page), 1, "{page}");

    // Taken: an inbox row waiting for a turn, then the ack.
    let (id, event) = env
        .pending()
        .await
        .into_iter()
        .find_map(|event| match &event {
            ChannelEvent::Message { id, .. } => Some((id.clone(), event)),
            _ => None,
        })
        .unwrap();
    let (session, _) = env
        .store
        .open_session(
            SessionKey {
                agent: AgentId::new("jeeves").unwrap(),
                channel: INSTANCE.into(),
                conversation: chat(),
            },
            SessionKind::Direct,
            Some(identity::owner()),
            Timestamp::now(),
        )
        .await
        .unwrap();
    // Whole seconds: what the store keeps reads back the same.
    let received = Timestamp::from_second(Timestamp::now().as_second()).unwrap();
    let row = env
        .store
        .inbox_insert(NewInboxEvent {
            instance: INSTANCE.into(),
            event_id: id.clone(),
            session: Some(session.id),
            user: Some(identity::owner()),
            payload: serde_json::to_value(&event).unwrap(),
            state: InboxState::Queued,
            received_at: received,
        })
        .await
        .unwrap()
        .unwrap();
    env.channel.ack(&id).await.unwrap();
    let page = body_text(env.get(&path).await).await;
    assert_eq!(shown(&page), 1, "{page}");

    // A turn takes it, then stores it (with the time prefix).
    env.store.inbox_start(vec![row]).await.unwrap();
    let page = body_text(env.get(&path).await).await;
    assert_eq!(shown(&page), 1, "{page}");
    let mut message = stored(Role::User, vec![text("[Thu 2026-10-22 14:03 PDT] first")]);
    message.meta.at = received;
    env.store
        .append_messages(session.id, vec![message])
        .await
        .unwrap();
    let page = body_text(env.get(&path).await).await;
    assert_eq!(shown(&page), 1, "{page}");
    assert!(!page.contains(&format!(r#"id="m-{id}""#)), "{page}");
    env.store.inbox_finish(vec![row]).await.unwrap();
    let page = body_text(env.get(&path).await).await;
    assert_eq!(shown(&page), 1, "{page}");
}

#[tokio::test]
async fn a_live_message_already_stored_is_shown_once() {
    let env = env().await;
    let path = format!("/chats/{}", chat());
    let sink = env.channel.deltas().unwrap();
    let mut writer = sink.start(&chat()).unwrap();
    tokio::time::sleep(LIVE_UPDATE_INTERVAL).await;
    writer.push("Hello");
    let page = body_text(env.get(&path).await).await;
    assert!(page.contains("msg assistant live"), "{page}");

    // The turn stored the message, and the page loads before the writer finishes.
    store_chat(
        &env,
        &chat(),
        INSTANCE,
        vec![stored(Role::Assistant, vec![text("Hello there")])],
    )
    .await;
    let page = body_text(env.get(&path).await).await;
    assert!(!page.contains("msg assistant live"), "{page}");
    assert_eq!(page.matches("Hello there").count(), 1, "{page}");
    writer.finish().await.unwrap();
}

/// A provider that streams one text reply slowly enough for live updates.
struct SlowProvider;

#[async_trait]
impl doozers_types::Provider for SlowProvider {
    async fn list_models(&self) -> doozers_types::Result<Vec<doozers_types::ModelInfo>> {
        Ok(Vec::new())
    }

    async fn complete(
        &self,
        _req: doozers_types::CompletionRequest,
    ) -> doozers_types::Result<
        BoxStream<'static, doozers_types::Result<doozers_types::CompletionEvent>>,
    > {
        use doozers_types::{BlockKind, CompletionEvent, StopReason, Usage};
        let pause = || tokio::time::sleep(LIVE_UPDATE_INTERVAL + Duration::from_millis(20));
        let events = futures::stream::iter([
            CompletionEvent::BlockStart {
                index: 0,
                kind: BlockKind::Text,
                tool: None,
            },
            CompletionEvent::TextDelta {
                index: 0,
                text: "Hel".into(),
            },
        ])
        .chain(futures::stream::once(async move {
            pause().await;
            CompletionEvent::TextDelta {
                index: 0,
                text: "lo **world**".into(),
            }
        }))
        .chain(futures::stream::once(async move {
            pause().await;
            CompletionEvent::TextDelta {
                index: 0,
                text: ", done.".into(),
            }
        }))
        .chain(futures::stream::iter([
            CompletionEvent::BlockStop {
                index: 0,
                block: Block::text("Hello **world**, done."),
            },
            CompletionEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
            CompletionEvent::Stop(StopReason::EndTurn),
        ]))
        .map(Ok);
        Ok(events.boxed())
    }
}

/// The daemon's wiring (runtime, agent loop, sessions, web app) with `provider` as the model of
/// the only agent, which every message is routed to. Cancel `env.shutdown` and wait for the
/// sessions to close at the end.
async fn daemon(provider: Arc<dyn doozers_types::Provider>) -> (Env, crate::sessions::Sessions) {
    let dir = tempfile::tempdir().unwrap();
    let root = crate::paths::DataRoot::new(dir.path(), &dir.path().join("tmp")).unwrap();
    root.create_layout().unwrap();
    let write = |relative: &str, contents: &str| {
        let path = dir.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    };
    write(
        "doozers.toml",
        "[core]\nhttp_listen = \"127.0.0.1:0\"\n\
         [owner]\nidentities = [\"web:owner\"]\n\
         [provider.fake]\ncomponent = \"openai\"\nconfig = { base_url = \"http://127.0.0.1:9/v1\" }\n\
         [channel.web]\n\
         [[routes]]\nagent = \"jeeves\"\n",
    );
    write(
        "agents/jeeves/agent.toml",
        "models = [\"fake/test-model\"]\n",
    );
    let config = crate::config::Config::load_with(
        &root,
        &crate::config::LoadOptions {
            env: &|_| None,
            doozers_exe: None,
            ..crate::config::LoadOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let runtime = crate::runtime::Runtime::start(root, Arc::new(config))
        .await
        .unwrap();
    runtime
        .registry
        .register_provider(&InstanceName::new("fake").unwrap(), provider)
        .unwrap();
    let shutdown = CancellationToken::new();
    let approvals = Arc::new(crate::approvals::ApprovalBoard::default());
    let sessions = runtime
        .start_sessions(
            Arc::new(crate::agent_loop::AgentLoop::from_runtime(
                &runtime,
                approvals.clone(),
            )),
            approvals,
            shutdown.clone(),
        )
        .await
        .unwrap();
    let env = Env {
        store: runtime.store.clone(),
        memory_dbs: runtime.memory_dbs.clone(),
        blobs: runtime.blobs.clone(),
        channel: runtime.registry.web_channel().unwrap(),
        registry: runtime.registry.clone(),
        routes: tokio::sync::OnceCell::from(runtime.routes.clone()),
        activity: Arc::default(),
        jobs: runtime.jobs.clone(),
        app: WebApp::from_runtime(&runtime, shutdown.clone()).unwrap(),
        shutdown,
        _dir: dir,
    };
    (env, sessions)
}

/// A post becomes a turn whose streamed text reaches the open page live, then the transcript.
#[tokio::test]
async fn the_agent_loop_streams_a_reply_live_to_the_page() {
    let (env, sessions) = daemon(Arc::new(SlowProvider)).await;
    let mut sse = Sse::open(&env, &format!("/chats/{}/events", chat()), &[]).await;
    let response = env
        .htmx_post(&format!("/chats/{}/messages", chat()), "text=ping")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(sse.next().await.data.contains("<p>ping</p>"));

    // A live message: started, updated as the deltas arrive, then finished.
    let started = tokio::time::timeout(WAIT, sse.next()).await.unwrap();
    assert!(
        started.data.contains("msg assistant live"),
        "{}",
        started.data
    );
    let mut updates = Vec::new();
    loop {
        let event = tokio::time::timeout(WAIT, sse.next()).await.unwrap();
        let done = !event.data.contains("msg assistant live");
        updates.push(event.data);
        if done {
            break;
        }
    }
    let (last, live) = updates.split_last().unwrap();
    assert!(
        !live.is_empty(),
        "no live update before the end: {updates:#?}"
    );
    assert!(
        live.iter()
            .all(|u| u.starts_with(r#"<article hx-swap-oob="true""#)),
        "{live:#?}"
    );
    assert!(
        live[0].contains("Hello <strong>world</strong>"),
        "{}",
        live[0]
    );
    assert!(
        last.contains("Hello <strong>world</strong>, done.") && !last.contains("Writing"),
        "{last}"
    );

    // The agent loop stores the reply before it ends the live message, so a reload now shows it
    // once, from the transcript, and nothing live.
    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert!(page.contains(r#"<article id="m-h"#), "{page}");
    assert_eq!(
        page.matches("Hello <strong>world</strong>, done.").count(),
        1,
        "{page}"
    );
    assert!(!page.contains("msg assistant live"), "{page}");

    env.shutdown.cancel();
    sessions.closed().await;
}

/// A provider that asks for one call to a tool the agent doesn't have, then answers.
struct ToolCallingProvider;

#[async_trait]
impl doozers_types::Provider for ToolCallingProvider {
    async fn list_models(&self) -> doozers_types::Result<Vec<doozers_types::ModelInfo>> {
        Ok(Vec::new())
    }

    async fn complete(
        &self,
        req: doozers_types::CompletionRequest,
    ) -> doozers_types::Result<
        BoxStream<'static, doozers_types::Result<doozers_types::CompletionEvent>>,
    > {
        use doozers_types::{CompletionEvent, StopReason, Usage};
        let answered = req
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, Block::ToolResult { .. }));
        let (block, stop) = if answered {
            (Block::text("Done."), StopReason::EndTurn)
        } else {
            (
                Block::ToolUse {
                    id: "call-1".into(),
                    name: "shell__exec".into(),
                    input: serde_json::json!({"command": "ls\n-la"}),
                },
                StopReason::ToolUse,
            )
        };
        let events = [
            CompletionEvent::BlockStart {
                index: 0,
                kind: block.kind(),
                tool: None,
            },
            CompletionEvent::BlockStop { index: 0, block },
            CompletionEvent::Usage(Usage::default()),
            CompletionEvent::Stop(stop),
        ];
        Ok(futures::stream::iter(events).map(Ok).boxed())
    }
}

/// A turn's tool call reaches the open page as it starts, then its result; a reload shows it
/// once, from the transcript.
#[tokio::test]
async fn the_agent_loop_shows_tool_calls_on_the_page() {
    let (env, sessions) = daemon(Arc::new(ToolCallingProvider)).await;
    let mut sse = Sse::open(&env, &format!("/chats/{}/events", chat()), &[]).await;
    let response = env
        .htmx_post(&format!("/chats/{}/messages", chat()), "text=ping")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(sse.next().await.data.contains("<p>ping</p>"));

    let started = tokio::time::timeout(WAIT, sse.next()).await.unwrap().data;
    assert!(started.starts_with(r#"<article id="t-"#), "{started}");
    assert!(started.contains(r#"class="tool live""#), "{started}");
    assert!(started.contains("shell__exec"), "{started}");
    assert!(
        started.contains(r#"<code class="tool-first-line">command: ls …</code>"#),
        "{started}"
    );
    assert!(
        started.contains("<pre class=\"tool-input\">command: ls\n-la</pre>"),
        "{started}"
    );
    let element = started.split('"').nth(1).unwrap().to_owned();

    let ended = tokio::time::timeout(WAIT, sse.next()).await.unwrap().data;
    assert!(
        ended.starts_with(&format!(
            r#"<article hx-swap-oob="true" id="{element}" class="tool error">"#
        )),
        "{ended}"
    );
    assert!(ended.contains("Failed"), "{ended}");

    let reply = tokio::time::timeout(WAIT, sse.next()).await.unwrap().data;
    assert!(reply.contains("msg assistant"), "{reply}");
    loop {
        let event = tokio::time::timeout(WAIT, sse.next()).await.unwrap().data;
        if event.contains("<p>Done.</p>") && !event.contains("Writing") {
            break;
        }
    }

    let page = body_text(env.get(&format!("/chats/{}", chat())).await).await;
    assert_eq!(
        page.matches(&format!(r#"id="{element}""#)).count(),
        1,
        "{page}"
    );
    let shown = page.split(&format!(r#"id="{element}""#)).nth(1).unwrap();
    assert!(shown.starts_with(r#" class="tool error">"#), "{page}");
    assert!(page.find(&element) < page.find("<p>Done.</p>"), "{page}");

    env.shutdown.cancel();
    sessions.closed().await;
}

#[tokio::test]
async fn a_tool_call_dropped_without_its_result_shows_as_stopped() {
    let env = env().await;
    let mut sse = Sse::open(&env, &format!("/chats/{}/events", chat()), &[]).await;
    let sink = env.channel.deltas().unwrap();
    let input = serde_json::json!({});
    let call = doozers_types::LiveToolCall {
        id: "7",
        name: "slow",
        input: &input,
    };
    let writer = sink.tool_call(&chat(), call).unwrap();
    assert!(sse.next().await.data.contains("Running"));
    drop(writer);
    let stopped = sse.next().await.data;
    assert!(
        stopped.starts_with(r#"<article hx-swap-oob="true" id="t-7" class="tool">"#),
        "{stopped}"
    );
    assert!(stopped.contains("Stopped"), "{stopped}");
}

/// The owner may start a chat with any configured agent; the chat keeps it.
#[tokio::test]
async fn a_chat_can_be_started_with_any_agent_and_keeps_it() {
    let env = env().await;
    let list = body_text(env.get("/").await).await;
    for option in [
        r#"<option value="bunter">bunter</option>"#,
        r#"<option value="jeeves">Jeeves (jeeves)</option>"#,
    ] {
        assert!(list.contains(option), "{option}\n{list}");
    }

    let response = env.get("/chats/new?agent=jeeves").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response.headers()[LOCATION].to_str().unwrap().to_owned();
    let id = location.strip_prefix("/chats/").unwrap();
    assert_eq!(chat_agent(id), Some(AgentId::new("jeeves").unwrap()));
    let page = body_text(env.get(&location).await).await;
    assert!(
        page.contains(r#"<span class="muted">with Jeeves (jeeves)</span>"#),
        "{page}"
    );
    let response = env
        .htmx_post(&format!("{location}/messages"), "text=hello")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let pending = env.pending().await;
    let Some(ChannelEvent::Message { message, .. }) = pending.last() else {
        panic!("{pending:?}");
    };
    assert_eq!(message.conversation.id.as_str(), id);

    // Without an agent (or with an empty choice) the chat is routed as before.
    for path in ["/chats/new", "/chats/new?agent="] {
        let response = env.get(path).await;
        let location = response.headers()[LOCATION].to_str().unwrap();
        let plain = location.strip_prefix("/chats/").unwrap();
        assert!(
            is_chat_id(plain) && chat_agent(plain).is_none(),
            "{location}"
        );
    }

    // An agent that isn't configured: nothing to start, open, or post to.
    let gone = format!("/chats/gone.{}", chat());
    for path in [
        "/chats/new?agent=gone",
        "/chats/new?agent=Jeeves",
        gone.as_str(),
    ] {
        let response = env.get(path).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let response = env
        .htmx_post(&format!("{gone}/messages"), "text=hello")
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // Only the message to Jeeves is waiting.
    assert_eq!(env.pending_texts().await, ["hello"]);

    // A member is offered every agent and can pick one, like an admin.
    let guest = WebUser {
        user: UserId::new("guest").unwrap(),
        role: doozers_store::Role::Member,
        sender: NativeId::new("guest").unwrap(),
        csrf: CSRF.into(),
        login: auth::Login::Test,
    };
    let list = body_text(
        env.request(Some(guest.clone()), Method::GET, "/", &[], None)
            .await,
    )
    .await;
    assert!(list.contains("agent-picker"), "{list}");
    for agent in ["jeeves", "bunter"] {
        assert!(
            list.contains(&format!(r#"<option value="{agent}">"#)),
            "{list}"
        );
    }
    let response = env
        .request(
            Some(guest.clone()),
            Method::GET,
            "/chats/new?agent=jeeves",
            &[],
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let chat = response.headers()[LOCATION].to_str().unwrap().to_owned();
    assert!(chat.starts_with("/chats/jeeves."), "{chat}");
    let response = env
        .request(Some(guest), Method::GET, &chat, &[], None)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
}
