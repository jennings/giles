//! The web channel's HTTP side: the chat list, chat pages, posting, approval buttons, the event
//! streams, and `/blobs/<sha256>`. The login pages and the authentication middleware are in
//! [`login`](super::login).

use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;

use askama::Template;
use axum::Router;
use axum::body::Body;
use axum::extract::{Form, Path, Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::from_fn_with_state;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use futures::{Stream, StreamExt, stream};
use giles_store::{BlobStore, Store};
use giles_types::{ConversationId, MediaId};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tracing::error;

use super::auth::{WebUser, check_csrf};
use super::hub::{Event, Subscription};
use super::{ApprovalAnswer, Busy, WebChannel, is_approval_code, is_chat_id, new_chat_id, render};
use crate::agent_loop::history;
use crate::http::{self, RouterBuilder};
use crate::identity;
use crate::runtime::Runtime;
use crate::sessions::{Clock, SystemClock};

/// How many transcript rows a chat page shows, newest last.
pub const HISTORY_LIMIT: u32 = 200;

/// How many chats the chat list shows, most recently active first.
pub const CHAT_LIST_LIMIT: u32 = 200;

/// The longest message the form accepts, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024;

/// How long a blob may be cached: its URL is its content hash, so it never changes.
const BLOB_CACHE_CONTROL: &str = "private, max-age=31536000, immutable";

/// The web channel's HTTP application. Cheap to clone.
#[derive(Clone)]
pub struct WebApp {
    pub(super) inner: Arc<Inner>,
}

pub(super) struct Inner {
    pub(super) channel: Arc<WebChannel>,
    pub(super) store: Store,
    pub(super) blobs: BlobStore,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) shutdown: CancellationToken,
}

impl fmt::Debug for WebApp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebApp")
            .field("channel", &self.inner.channel)
            .finish_non_exhaustive()
    }
}

impl WebApp {
    /// The pages of `channel`, reading history from `store` and attachments from `blobs`. Event
    /// streams end when `shutdown` is cancelled.
    pub fn new(
        channel: Arc<WebChannel>,
        store: Store,
        blobs: BlobStore,
        shutdown: CancellationToken,
    ) -> Self {
        Self::with_clock(channel, store, blobs, Arc::new(SystemClock), shutdown)
    }

    /// [`new`](Self::new) with another time source.
    pub fn with_clock(
        channel: Arc<WebChannel>,
        store: Store,
        blobs: BlobStore,
        clock: Arc<dyn Clock>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                channel,
                store,
                blobs,
                clock,
                shutdown,
            }),
        }
    }

    /// The web application of the runtime's web channel, if `giles.toml` configures one. Also
    /// starts deleting expired web sessions every hour, until `shutdown`.
    pub fn from_runtime(runtime: &Runtime, shutdown: CancellationToken) -> Option<Self> {
        let channel = runtime.registry.web_channel()?;
        let app = Self::new(
            channel,
            runtime.store.clone(),
            runtime.blobs.clone(),
            shutdown,
        );
        tokio::spawn(app.clone().expire_sessions());
        Some(app)
    }

    /// The channel.
    pub fn channel(&self) -> &Arc<WebChannel> {
        &self.inner.channel
    }

    /// Adds the web channel's pages to `builder`, the chat list replacing the placeholder home
    /// page. Every page goes through the authentication middleware (`login::authenticate`).
    pub fn install(&self, builder: RouterBuilder) -> RouterBuilder {
        let auth = from_fn_with_state(self.clone(), super::login::authenticate);
        builder
            .home(get(chats).with_state(self.clone()).layer(auth.clone()))
            .merge(self.routes().route_layer(auth))
    }

    fn routes(&self) -> Router {
        Router::new()
            .route("/chats/new", get(new_chat))
            .route("/chats/{chat}", get(chat_page))
            .route("/chats/{chat}/messages", post(post_message))
            .route("/chats/{chat}/approvals", post(post_approval))
            .route("/chats/{chat}/events", get(events))
            .route("/blobs/{id}", get(blob))
            .merge(super::login::routes())
            .with_state(self.clone())
    }
}

#[derive(Template)]
#[template(path = "web/error.html")]
struct ErrorPage<'a> {
    title: &'a str,
    message: &'a str,
}

/// A page saying what went wrong, with `status`.
pub(super) fn error_page(status: StatusCode, title: &str, message: &str) -> Response {
    let mut response = http::render(&ErrorPage { title, message });
    if response.status().is_success() {
        *response.status_mut() = status;
    }
    response
}

#[derive(Template)]
#[template(path = "web/login_required.html")]
struct LoginRequiredPage;

/// The answer to a request that needs a login and has none: 401 and how to log in.
pub(super) fn login_required() -> Response {
    let mut response = http::render(&LoginRequiredPage);
    if response.status().is_success() {
        *response.status_mut() = StatusCode::UNAUTHORIZED;
    }
    response
}

fn not_found() -> Response {
    error_page(
        StatusCode::NOT_FOUND,
        "Not found",
        "There is nothing at this address.",
    )
}

/// Logs a database or blob store error and answers 500.
pub(super) fn internal_error(what: &str, err: &dyn std::error::Error) -> Response {
    error!(error = %err, "{what}");
    error_page(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong",
        "Giles couldn't load this page. The error is in its log.",
    )
}

/// The chat id in a path, if it is one.
fn chat_id(raw: &str) -> Option<ConversationId> {
    is_chat_id(raw)
        .then(|| ConversationId::new(raw).ok())
        .flatten()
}

/// Whether htmx sent the request (as opposed to a plain form post).
fn is_htmx(headers: &HeaderMap) -> bool {
    headers.get("hx-request").is_some_and(|v| v == "true")
}

/// The first line of a chat's first message (without the agent loop's time prefix), cut to 80
/// characters.
fn chat_title(first_text: Option<&str>) -> String {
    const MAX: usize = 80;
    let line = first_text
        .map(history::strip_time_prefix)
        .and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or_default();
    if line.is_empty() {
        return "Untitled chat".to_owned();
    }
    match line.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

struct ChatItem {
    id: String,
    title: String,
    active: String,
    active_at: String,
}

#[derive(Template)]
#[template(path = "web/chats.html")]
struct ChatsPage {
    chats: Vec<ChatItem>,
    /// Set for a cookie login: the page shows a logout button.
    logout_csrf: Option<String>,
}

/// `GET /`: the user's chats, most recently active first.
async fn chats(State(app): State<WebApp>, user: WebUser) -> Response {
    let channel = app.inner.channel.instance().as_str();
    let listed = match app
        .inner
        .store
        .channel_conversations(channel, &user.user, CHAT_LIST_LIMIT)
        .await
    {
        Ok(listed) => listed,
        Err(err) => return internal_error("cannot list the web chats", &err),
    };
    let chats = listed
        .into_iter()
        .filter(|c| is_chat_id(c.conversation.as_str()))
        .map(|c| ChatItem {
            id: c.conversation.to_string(),
            title: chat_title(c.first_text.as_deref()),
            active: c.last_active_at.strftime("%Y-%m-%d %H:%M UTC").to_string(),
            active_at: c.last_active_at.to_string(),
        })
        .collect();
    http::render(&ChatsPage {
        chats,
        logout_csrf: user.can_log_out().then(|| user.csrf.clone()),
    })
}

/// `GET /chats/new`: a fresh chat id. Nothing is stored until the first message.
async fn new_chat(_: WebUser) -> Redirect {
    Redirect::to(&format!("/chats/{}", new_chat_id()))
}

#[derive(Template)]
#[template(path = "web/chat.html")]
struct ChatPage<'a> {
    title: &'a str,
    chat: &'a str,
    csrf: &'a str,
    cursor: String,
    truncated: bool,
    items: Vec<String>,
    compose: String,
}

/// `GET /chats/<id>`: the stored transcript, the user's messages not stored yet, live messages
/// and approval prompts, and the form. The page follows `/chats/<id>/events` from the event it
/// reflects.
///
/// Order matters. The hub snapshot comes first: anything published after it is replayed by the
/// event stream, so the page leaves out posts it will replay. The posts the core hasn't taken
/// come next, then the transcript and the waiting inbox rows (one read): a post acked in between
/// is in the inbox by then. A live message the transcript already has (it ended and was stored
/// after the snapshot) is shown from the transcript only.
async fn chat_page(State(app): State<WebApp>, user: WebUser, Path(chat): Path<String>) -> Response {
    let Some(chat) = chat_id(&chat) else {
        return not_found();
    };
    let channel = &app.inner.channel;
    let hub = channel.hub(&chat);
    let snapshot = hub.snapshot();
    let unacked = channel.unacked(&chat);
    let transcript = match app
        .inner
        .store
        .conversation_transcript(channel.instance().as_str(), &chat, HISTORY_LIMIT)
        .await
    {
        Ok(transcript) => transcript,
        Err(err) => return internal_error("cannot load a web chat", &err),
    };
    let rows = &transcript.messages;
    let mut items = render::history(rows);
    items.extend(render::waiting(
        rows,
        &transcript.waiting,
        unacked,
        &hub.posted_after(snapshot.seq),
    ));
    items.extend(
        snapshot
            .live
            .into_iter()
            .filter(|live| !render::is_stored(live, rows))
            .map(|live| live.html),
    );
    items.extend(
        snapshot
            .approvals
            .iter()
            .map(|approval| match &approval.sent {
                Some(label) => render::approval_answered(&approval.code, &approval.prompt, label),
                None => render::approval(
                    chat.as_str(),
                    &approval.code,
                    &approval.prompt,
                    Some(&user.csrf),
                ),
            }),
    );
    http::render(&ChatPage {
        title: "Chat",
        chat: chat.as_str(),
        csrf: &user.csrf,
        cursor: channel.hubs.cursor(snapshot.seq),
        truncated: rows.len() >= HISTORY_LIMIT as usize,
        items,
        compose: render::compose(chat.as_str(), &user.csrf, "", None),
    })
}

#[derive(Debug, Deserialize)]
struct ComposeForm {
    #[serde(default)]
    csrf: Option<String>,
    text: String,
}

/// `POST /chats/<id>/messages`: posts the text as the user. htmx gets a fresh form back (the
/// message itself arrives over the event stream); a plain form post is redirected to the chat.
async fn post_message(
    State(app): State<WebApp>,
    user: WebUser,
    Path(chat): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ComposeForm>,
) -> Response {
    let Some(chat) = chat_id(&chat) else {
        return not_found();
    };
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let htmx = is_htmx(&headers);
    let failed = |status: StatusCode, message: &str| {
        if htmx {
            // htmx only swaps successful responses: show the error in the form.
            Html(render::compose(
                chat.as_str(),
                &user.csrf,
                &form.text,
                Some(message),
            ))
            .into_response()
        } else {
            error_page(status, "Message not sent", message)
        }
    };
    if form.text.trim().is_empty() {
        return failed(StatusCode::BAD_REQUEST, "Write a message first.");
    }
    if form.text.len() > MAX_MESSAGE_BYTES {
        return failed(
            StatusCode::PAYLOAD_TOO_LARGE,
            "That message is too long: at most 32 KiB.",
        );
    }
    let now = app.inner.clock.now();
    if let Err(Busy) = app.inner.channel.post(&user, &chat, &form.text, now) {
        return failed(StatusCode::SERVICE_UNAVAILABLE, &Busy.to_string());
    }
    if htmx {
        Html(render::compose(chat.as_str(), &user.csrf, "", None)).into_response()
    } else {
        Redirect::to(&format!("/chats/{chat}")).into_response()
    }
}

#[derive(Debug, Deserialize)]
struct ApprovalForm {
    #[serde(default)]
    csrf: Option<String>,
    code: String,
    answer: String,
}

/// `POST /chats/<id>/approvals`: an approval button. Posts `yes <code>`, `yes! <code>`, or
/// `no <code>` as the user; htmx gets 204 (the answered prompt arrives over the event stream).
async fn post_approval(
    State(app): State<WebApp>,
    user: WebUser,
    Path(chat): Path<String>,
    headers: HeaderMap,
    Form(form): Form<ApprovalForm>,
) -> Response {
    let Some(chat) = chat_id(&chat) else {
        return not_found();
    };
    if let Err(rejected) = check_csrf(&user, &headers, form.csrf.as_deref()) {
        return rejected.into_response();
    }
    let (Some(answer), true) = (
        ApprovalAnswer::parse(&form.answer),
        is_approval_code(&form.code),
    ) else {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Not an answer",
            "That approval answer isn't valid.",
        );
    };
    let now = app.inner.clock.now();
    if let Err(Busy) = app
        .inner
        .channel
        .answer_approval(&user, &chat, &form.code, answer, now)
    {
        return error_page(
            StatusCode::SERVICE_UNAVAILABLE,
            "Answer not sent",
            &Busy.to_string(),
        );
    }
    if is_htmx(&headers) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        Redirect::to(&format!("/chats/{chat}")).into_response()
    }
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    after: Option<String>,
}

/// What the event stream sends.
enum Item {
    Event(Arc<Event>),
    Reload,
}

/// `GET /chats/<id>/events`: server-sent events, each an HTML fragment for the message list
/// (event name `message`), starting after `Last-Event-ID` or `?after=`. Ends when the daemon
/// shuts down.
async fn events(
    State(app): State<WebApp>,
    _: WebUser,
    Path(chat): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(chat) = chat_id(&chat) else {
        return not_found();
    };
    let channel = app.inner.channel.clone();
    let cursor = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .or(query.after.as_deref());
    let subscription = channel.hub(&chat).subscribe(channel.hubs.after(cursor));
    let stream = event_stream(subscription)
        .map(move |item| Ok::<_, Infallible>(sse_event(&channel, item)))
        .take_until(app.inner.shutdown.clone().cancelled_owned());
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// A subscription's events: a reload notice if events were lost, the replay, then the feed
/// (with a reload notice whenever the stream falls behind it).
fn event_stream(subscription: Subscription) -> impl Stream<Item = Item> + Send + 'static {
    let Subscription {
        replay,
        gap,
        seq,
        feed,
    } = subscription;
    let last = replay.last().map_or(seq, |event| event.seq);
    let first = gap
        .then_some(Item::Reload)
        .into_iter()
        .chain(replay.into_iter().map(Item::Event));
    let live = stream::unfold((feed, last), |(mut feed, last)| async move {
        loop {
            match feed.recv().await {
                Ok(event) if event.seq > last => {
                    let seq = event.seq;
                    return Some((Item::Event(event), (feed, seq)));
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => return Some((Item::Reload, (feed, last))),
                Err(RecvError::Closed) => return None,
            }
        }
    });
    stream::iter(first).chain(live)
}

fn sse_event(channel: &WebChannel, item: Item) -> SseEvent {
    match item {
        Item::Event(event) => SseEvent::default()
            .event("message")
            .id(channel.hubs.cursor(event.seq))
            .data(&event.html),
        Item::Reload => SseEvent::default()
            .event("message")
            .data(render::reload_notice()),
    }
}

/// Whether `user` may see blobs. v1 has one user, the owner, and every blob belongs to the
/// owner's conversations and jobs, so only the owner may; per-blob checks come with multi-user
/// support.
fn may_see_blobs(user: &WebUser) -> bool {
    user.user == identity::owner()
}

/// `filename*=` of `Content-Disposition` (RFC 6266, RFC 8187): UTF-8, percent-encoded.
fn encoded_filename(name: &str) -> String {
    let mut out = String::from("UTF-8''");
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `GET /blobs/<sha256>`: an attachment, for the logged-in user. Served with its stored media
/// type, `nosniff` (from the security headers), and `Content-Security-Policy: sandbox`, and as a
/// download unless it's a raster image, so a stored HTML or SVG blob can never run in Giles's
/// origin.
async fn blob(State(app): State<WebApp>, user: WebUser, Path(id): Path<String>) -> Response {
    let Ok(id) = id.parse::<MediaId>() else {
        return not_found();
    };
    if !may_see_blobs(&user) {
        return not_found();
    }
    let blob = match app.inner.blobs.get(&id).await {
        Ok(Some(blob)) => blob,
        Ok(None) => return not_found(),
        Err(err) => return internal_error("cannot read a blob", &err),
    };
    let media = blob.media;
    let content_type = HeaderValue::from_str(&media.media_type)
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    let disposition = if render::is_inline_image(&media.media_type) {
        HeaderValue::from_static("inline")
    } else {
        let name = media.name.as_deref().unwrap_or(media.id.as_str());
        HeaderValue::from_str(&format!("attachment; filename*={}", encoded_filename(name)))
            .unwrap_or(HeaderValue::from_static("attachment"))
    };
    let mut response = Response::new(Body::from_stream(ReaderStream::new(blob.reader)));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, content_type);
    headers.insert(CONTENT_LENGTH, HeaderValue::from(media.size));
    headers.insert(CONTENT_DISPOSITION, disposition);
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(BLOB_CACHE_CONTROL));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_titles_use_the_first_line_cut_to_80_characters() {
        assert_eq!(chat_title(None), "Untitled chat");
        assert_eq!(chat_title(Some("  \n\n ")), "Untitled chat");
        assert_eq!(chat_title(Some("\n  hello there \nmore")), "hello there");
        let long = "é".repeat(100);
        let title = chat_title(Some(&long));
        assert_eq!(title.chars().count(), 81);
        assert!(title.ends_with('…'));
        assert_eq!(chat_title(Some(&"x".repeat(80))), "x".repeat(80));
        // The agent loop's time prefix isn't part of the title.
        assert_eq!(
            chat_title(Some("[Thu 2026-10-22 14:03 PDT] what's on today?\nmore")),
            "what's on today?"
        );
        assert_eq!(
            chat_title(Some("[Thu 2026-10-22 14:03 PDT]")),
            "Untitled chat"
        );
    }

    #[test]
    fn download_names_are_percent_encoded() {
        assert_eq!(encoded_filename("report.pdf"), "UTF-8''report.pdf");
        assert_eq!(
            encoded_filename("a b\"c;é.txt"),
            "UTF-8''a%20b%22c%3B%C3%A9.txt"
        );
    }
}
