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
    CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, LOCATION, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{Method, Request, StatusCode};
use axum::response::Response;
use futures::StreamExt;
use futures::stream::BoxStream;
use giles_store::{BlobMeta, BlobStore, InboxState, NewInboxEvent, SessionKey, SessionKind, Store};
use giles_types::{
    AgentId, Block, Channel, ChannelEvent, ChannelStatus, ConversationId, Error, MediaRef, Message,
    MessageMeta, NativeId, OutboundMessage, Role, SendTarget, Timestamp, UserId,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use super::pages::MAX_MESSAGE_BYTES;
use super::*;
use crate::commands::NoPendingApprovals;
use crate::config::{Identity, OwnerConfig, Route};
use crate::http::RouterBuilder;
use crate::router::Router as MessageRouter;
use crate::sessions::{
    AgentPolicy, ChannelLookup, Sessions, SessionsOptions, SystemClock, Turn, TurnOutcome,
    TurnRunner,
};

const INSTANCE: &str = "web";
const CSRF: &str = "csrf-token-for-tests";
const WAIT: Duration = Duration::from_secs(10);

struct Env {
    _dir: TempDir,
    store: Store,
    blobs: BlobStore,
    channel: Arc<WebChannel>,
    app: WebApp,
    shutdown: CancellationToken,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("giles.db")).await.unwrap();
    let blobs = BlobStore::open(dir.path().join("blobs"), store.clone())
        .await
        .unwrap();
    let channel = Arc::new(WebChannel::new(InstanceName::new(INSTANCE).unwrap()));
    let shutdown = CancellationToken::new();
    let app = WebApp::new(
        channel.clone(),
        store.clone(),
        blobs.clone(),
        shutdown.clone(),
    );
    Env {
        _dir: dir,
        store,
        blobs,
        channel,
        app,
        shutdown,
    }
}

fn owner() -> WebUser {
    WebUser {
        user: identity::owner(),
        sender: NativeId::new("owner").unwrap(),
        csrf: CSRF.into(),
    }
}

fn chat() -> ConversationId {
    ConversationId::new("0123456789abcdef0123456789abcdef").unwrap()
}

impl Env {
    /// The whole application, as `user` (none: not logged in).
    fn router(&self, user: Option<WebUser>) -> axum::Router {
        let app = self.app.install(RouterBuilder::new()).build();
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
        self.router(user)
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap()
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
        assert!(html.contains("giles admin login-link"), "{html}");
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
        .find(&format!(r#"<a href="/chats/{}">second chat</a>"#, chat()))
        .expect(&html);
    let first = html
        .find(&format!(
            r#"<a href="/chats/{older}">first &#60;chat&#62;</a>"#
        ))
        .expect(&html);
    assert!(second < first, "{html}");
    assert!(!html.contains("irc chat"), "{html}");
    assert!(html.contains(r#"href="/chats/new""#));
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
            stored(Role::User, vec![text("hello **giles**")]),
            stored(Role::Assistant, vec![text("<script>alert(1)</script> hi")]),
        ],
    )
    .await;
    let response = env.get(&format!("/chats/{}", chat())).await;
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;
    assert!(html.contains("hello <strong>giles</strong>"), "{html}");
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
        id: giles_types::MediaId::from_digest([7; 32]),
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
async fn blobs_are_served_safely_to_the_owner() {
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

    // Only the owner may see blobs (v1 has no other user).
    let stranger = WebUser {
        user: UserId::new("someone").unwrap(),
        ..owner()
    };
    let response = env
        .request(
            Some(stranger),
            Method::GET,
            &format!("/blobs/{}", png.id),
            &[],
            None,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
    let owner_identity = Identity::new("web:owner").unwrap();
    let router = MessageRouter::new(
        &OwnerConfig {
            identities: vec![owner_identity.clone()],
            default: owner_identity,
            timezone: None,
        },
        &[Route {
            channel: None,
            kind: None,
            conversation: None,
            agent: AgentId::new("jeeves").unwrap(),
        }],
    );
    let sessions = Sessions::start(
        SessionsOptions {
            store: env.store.clone(),
            router,
            agents: HashMap::from([(AgentId::new("jeeves").unwrap(), AgentPolicy::default())]),
            channels: Arc::new(Lookup(env.channel.clone())),
            runner: Arc::new(EchoRunner {
                store: env.store.clone(),
            }),
            approvals: Arc::new(NoPendingApprovals),
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
    assert!(list.contains(">ping</a>"), "{list}");

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
    assert!(list.contains(">what is on today?</a>"), "{list}");
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
