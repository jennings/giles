# Web Channel

The built-in web channel (PLAN sections 9 and 10) is `giles_core::channels::web`, behind the Cargo
feature `channel-web` (default on, forwarded by the `giles` crate). It has two halves:

- `WebChannel` implements `giles_types::Channel` with `ChannelInfo { identity_namespace: "web",
  output: Deltas, can_initiate: false }`. It's built by the registry's built-in channel builder
  (`WebChannel::from_instance`, see [Registry and Startup](registry.md)), registered like any
  channel, and attached to the session manager by `Runtime::start_sessions` with the other
  registered channels. At most one instance may use the component.
- `WebApp` is its HTTP side on the core's server ([HTTP Server](http.md)): `giles serve` builds
  it with `WebApp::from_runtime(&runtime, shutdown)` (from `Registry::web_channel()`, the typed
  handle) and `web.install(RouterBuilder::new())`, which replaces the placeholder `/` with the chat
  list and merges the other routes.

Neither half touches the agent loop: posted messages go through the normal inbound pipeline
(router, inbox, turns; [Sessions and Turns](sessions.md)), and replies come back through
`Channel::send` or the live-output hook.

## Routes

| Route                          | Does                                                             |
| ------------------------------ | ---------------------------------------------------------------- |
| `GET /`                        | The user's chats on this instance, most recently active first.  |
| `GET /chats/new`               | 303 to `/chats/<new id>`; nothing is stored until a message.    |
| `GET /chats/<id>`              | The transcript, live messages, pending prompts, and the form.   |
| `POST /chats/<id>/messages`    | Posts `text` as the user (CSRF-checked).                         |
| `POST /chats/<id>/approvals`   | An approval button: posts `<answer> <code>` (CSRF-checked).      |
| `GET /chats/<id>/events`       | Server-sent events for the page.                                 |
| `GET /blobs/<sha256>`          | An attachment.                                                   |

Every route needs a `WebUser` (below); without one it's a 401 page saying how to log in. A chat id
is 128 random bits as 32 lowercase hex digits (`new_chat_id`, `is_chat_id`), and is the
conversation id (PLAN section 10): web chats are created by the user. Any other id in a path is a
404. A chat is `ConversationKind::Direct`; v1 has one user, so every chat is the owner's.

## Who Is Asking

`channels::web::WebUser { user: UserId, sender: NativeId, csrf: String }` is an axum extractor
(`FromRequestParts` for any state) that reads a `WebUser` from the request's extensions, or
rejects with the login page. Authentication is a middleware that inserts it (the web login, TASKS
2.23); the extractor is the seam, so the admin pages (3.8) use the same one. `sender` is the native
id in the `web` namespace that posted messages carry (`InboundMessage::sender`, with
`sender_authenticated: true`); the router matches it against `[owner] identities` as
`web:<sender>`, like any channel's sender.

CSRF: `check_csrf(&user, &headers, form_field) -> Result<(), CsrfRejected>` accepts the token from
the `X-CSRF-Token` header (htmx: the chat page sets `hx-headers` with it, inherited by every form
on the page, including those that arrive over the event stream) or the `csrf` form field (plain
form posts without JavaScript), compared as SHA-256 digests. A mismatch is a 403 page. Every POST
checks it before acting.

## Inbound: Events and Acks

`WebChannel::post(user, chat, text, now)` builds a `ChannelEvent::Message` (event id `web-` plus
128 random bits, also used as the `message_id`), queues it, and shows the message on the chat's
open pages. The queue is in memory:

- An event stays pending until `ack`ed. `events()` returns a new stream that yields every pending
  event first, then new ones; opening a stream ends the previous one (its sender is dropped). So
  when the session manager reopens a stream after a database error, the unacked events come again,
  which is the at-least-once contract of PLAN section 8.5. Event ids are unique, so the inbox's
  dedupe makes redelivery harmless.
- The first pending event is a `Status { Connected }`, so the channel's status is recorded
  (`Sessions::channel_status`) like a plugin's.
- At most `MAX_PENDING` (256) events wait; `post` then returns `Busy`, and the form shows "too many
  messages are waiting" (503 for a plain post). A restart loses pending events, but only ones the
  core never stored, i.e. posts the user saw accepted while Giles wasn't taking events.

Validation: the text must not be blank and is at most `MAX_MESSAGE_BYTES` (32 KiB). htmx posts get
the form back (empty on success, with the text and an error otherwise, as 200 because htmx swaps
only successful responses); plain posts get a 303 back to the chat, or an error page.

## Outbound and Live Output

Each chat has a hub (`hub.rs`): an event log every open page of the chat follows. A hub event is an
HTML fragment for the page's message list. Fragments that change an element already on the page
carry `hx-swap-oob="true"` (`render::out_of_band`), so htmx replaces the element with the same id
instead of appending.

Hubs are made on first use and dropped when idle: making a new hub first drops every hub that has
no open event stream, no live message, no unanswered prompt, no `Arc` held elsewhere, and no use
(publish, snapshot, subscription, lookup) for `HUB_IDLE` (10 minutes). So memory follows the chats
in use, not every chat since the start.

- `Channel::send(OutboundMessage)` appends a whole message: the Markdown through
  `giles_plugin_sdk::render::html` with `is_local_image` (raw HTML escaped, remote images dropped),
  then the attachments (raster images inline from `/blobs/<id>`, anything else as a link).
  `SendTarget::Recipient` is `InvalidRequest`: the channel can't start conversations.
- `Channel::deltas()` returns the channel itself as a `DeltaSink` (the contract is in
  [Message Model and Traits](message-model.md#channel)). `start(conversation)` appends an empty
  live message ("Writing…"); `push` re-renders the whole text so far and replaces the element, at
  most every `LIVE_UPDATE_INTERVAL` (50 ms; pushes in between are coalesced, and the final text is
  always sent); `finish` replaces it with the final rendering. A writer dropped unfinished leaves
  its text, marked "Stopped before the end."

The agent loop (2.8) streams like this, and sends blocks through `Reply::send` on any channel
whose `deltas()` is `None`:

```rust
let channel = reply.channel();
if let Some(sink) = channel.as_deref().and_then(|c| c.deltas()) {
    let mut writer = sink.start(reply.conversation())?; // at a text block's BlockStart
    writer.push(&delta);                                // each TextDelta
    writer.finish().await?;                             // at its BlockStop
}
```

Whole-message `send` keeps working on this channel either way (notices, restart notices, 2.4's
replies).

### Event Streams and Cursors

A page is rendered with a cursor naming the last hub event it reflects, `<epoch>-<seq>`, where the
epoch is random per run of the daemon. Event numbers come from one counter for every chat, so a
chat's numbers only grow, even across a dropped and remade hub. Its `sse-connect` URL is
`/chats/<id>/events?after=<cursor>`, and the stream starts after `Last-Event-ID` if the browser
sends one (an `EventSource` reconnecting by itself does), else after `after`. Every SSE event has
the event name `message`, the cursor as its `id`, and the fragment as its data.

- Publishing and subscribing happen under the hub's lock, so an event is either in the replay or in
  the live feed, never both or neither. The hub keeps its last `RING` (128) events for replay. Its
  floor is the newest event it no longer has (dropped from the ring, or from before the hub was
  made).
- A cursor below the floor (older than the ring, or from a dropped hub), from another epoch (a
  restart), or ahead of the hub, and a stream that lags the broadcast channel, get a notice swapped
  into `#chat-notice` ("Some messages may be missing. Reload the page"), and the stream continues
  with new events.
- Streams end when the daemon's shutdown token is cancelled (`take_until`), so they never hold up
  the HTTP server's drain. Keep-alive comments every 15 s keep proxies from closing idle streams.

Known edge: if the htmx SSE extension gives up on an `EventSource` (the browser closed it after an
HTTP error, e.g. a 401 once the login expired) and creates a new one, it reconnects with the page's
original `after` cursor and replays events the page already showed. A reload fixes the page.

### History

The chat page shows the stored transcript, not a log of what was displayed:
`Store::conversation_transcript(instance, chat, HISTORY_LIMIT)` (the last 200 rows across every
session of the conversation, any agent, summarized messages included, and the inbox rows still
waiting; see [Storage](storage.md#conversations)). `render::history` shows text blocks (joined by
blank lines) and media of user and assistant messages, and drops tool results, empty slots, tool
calls, reasoning, and provider data (PLAN section 6.9); a message left with nothing to show is
skipped. A user message's time prefix (`[Thu 2026-10-22 14:03 PDT] `, which the agent loop stores
for the model) is cut with `agent_loop::history::strip_time_prefix`, here and in chat titles. A
"New session" marker separates sessions (`/new`, idle reset). Messages sent with `send` that the
agent loop doesn't store (notices) are only on pages that were open at the time. The chat list is
`Store::channel_conversations(instance, user, 200)`, titled with the first line of the first user
message (80 characters at most).

The page is assembled so that a message is shown once, whatever happens while it loads:

1. The hub snapshot (cursor, live messages, prompts). Everything published later is replayed by
   the stream.
2. The posts the core hasn't acked (`WebChannel::unacked`).
3. The transcript and the waiting inbox rows, in one read transaction. A post acked after step 2
   was inserted into the inbox before its ack, so it's in this read.
4. The user's messages not in the transcript yet (`render::waiting`): waiting rows, then unacked
   posts, without the posts the stream will replay (`Hub::posted_after(cursor)`), and without any
   the transcript already has. A started row is in the transcript if a user row has its receive
   time and its text (without the prefix); an unacked post if one has its text and was stored at
   or after it was sent. They're rendered exactly as the post showed them live (`m-<event id>`).
5. Live messages, except one the transcript already has (`render::is_stored`: an assistant row
   stored after the live message started contains its text so far): it ended and was stored after
   the snapshot, and the stream's replay of its end finds no element and changes nothing.
6. Prompts: unanswered ones with their buttons (and a `csrf` field), answered ones with what was
   sent.

A post the router drops (no route) isn't in the inbox, so after a reload it's gone, like any
dropped message.

## Approval Prompts (Hook for 2.7)

`WebChannel::send_approval(chat, code, markdown) -> Result<MessageId>` shows a prompt with three
buttons in `/chats/<id>/approvals` forms: Allow (`yes`), Allow for this session (`yes!`), Deny
(`no`), each with the code in a hidden field. The prompt Markdown is rendered like any message. The
code must be 1 to 32 ASCII letters and digits (what `commands::parse` accepts), else
`InvalidRequest`. The prompt stays in the hub, so page loads show it, until it's resolved; a
restart forgets it, as it does the approval.

The POST checks CSRF, the answer (`yes`, `yes!`, `no`), and the code, then
`answer_approval(user, chat, code, answer, now)` posts `<answer> <code>` as an ordinary message from
the user (so the core's command parsing and pending-approval check decide what it means) and
replaces the prompt's buttons with what was sent ("Sent: yes!"), not an outcome: the prompt may
have expired. htmx gets 204; plain posts a 303.

`WebChannel::resolve_approval(chat, code, ApprovalOutcome) -> bool` retires a prompt however it
ended: every open page replaces it with the outcome (`Allowed`, `AllowedForSession`, `Denied`,
`Expired`), and page loads no longer show it. `false` if the chat doesn't show it (resolved
already, or forgotten by a restart or with an idle hub); it never makes a hub.

How 2.7 uses it: approval prompts for a conversation on the web instance go to
`registry.web_channel()` (when `reply.instance()` is its `instance()`) with `send_approval` instead
of `Reply::send`, which shows the typed-reply version of the prompt on other channels; when the
approval ends (answer, typed reply, timeout, or denial by a new message, PLAN section 6.4), 2.7
calls `resolve_approval`. 2.7 may instead mark prompts in `OutboundMessage` (or a `Channel` hook
like `deltas`) and have `send` call `send_approval`; either way the template, the endpoint, the
answer path, and the retirement are here.

## Blobs

`GET /blobs/<sha256>` (PLAN section 9, 1.7's contract): the id must be 64 lowercase hex digits;
the user must be allowed to see blobs (v1: the owner, who owns every blob; per-blob checks come
with multi-user support); then the blob is streamed from the blob store (hash verified at the end;
a mismatch aborts the response). Headers: the stored media type (or `application/octet-stream` if
it isn't a valid header value), `Content-Length`, `Content-Security-Policy: sandbox` (next to the
base policy), `nosniff` (from the security headers), `Content-Disposition: inline` for raster
images (PNG, JPEG, GIF, WebP, AVIF, the set the renderer keeps) and `attachment;
filename*=UTF-8''<name>` for anything else, and `Cache-Control: private, max-age=31536000,
immutable` (the URL is the content hash). An HTML or SVG blob therefore downloads, and even opened
directly it runs in a sandboxed opaque origin.

## Templates and Styles

Templates are in `crates/giles-core/templates/web/`: pages `chats.html`, `chat.html`,
`error.html`, `login_required.html` (extending `layout.html`), and fragments `message.html`,
`approval.html`, `compose.html`, `attachments.html` (rendered by `render.rs`). Nothing inline: no
scripts, styles, or `hx-on` handlers (the CSP and `allowEval: false` forbid them). The chat page is
`hx-ext="sse"` with `sse-connect`; `#messages` has `sse-swap="message"` and `hx-swap="beforeend"`.
The compose form replaces itself (`hx-target="this"`, `hx-swap="outerHTML"`); approval forms use
`hx-swap="none"`. Every form also works as a plain form post. Styles are classes in
`assets/giles.css`.

## Tests

`crates/giles-core/src/channels/web/tests.rs` (router-level with `tower::ServiceExt::oneshot`, an
injected `WebUser`, a temporary store), plus unit tests in `hub.rs`, `render.rs`, `pages.rs`,
`auth.rs`, `config.rs`:

- Every route without a user is a 401 login page; malformed chat and blob ids are 404s.
- The chat list: only this instance's chats, newest first, escaped titles. A chat page: transcript
  rendered and sanitized, cursor and CSRF token in place, nothing inline.
- Posting: CSRF required (header or field), the event carries the chat, the sender, and
  `sender_authenticated`; blank and oversized texts refused with the text kept; `Busy` at the limit.
- Acks: pending until acked, redelivered on a new stream, an open stream gets new posts, opening a
  stream ends the previous one.
- Streams: `send` output reaches open pages sanitized; resume after `after` and `Last-Event-ID`;
  stale cursors get the reload notice; streams end on shutdown. Hubs: idle ones are dropped (and
  only those), and a page from a dropped hub is told to reload (paused Tokio time).
- Live output: start, coalesced and out-of-band updates, the snapshot on page load, finish, and the
  cut-off marker on drop (paused Tokio time); a live message already stored is shown once.
- Approvals: prompt fragment and page form, CSRF and validation, `yes! <code>` posted, the "Sent"
  replacement, `resolve_approval` outcomes (and `false` for unknown prompts and chats),
  plain-post redirect.
- History: the time prefix is cut from user messages and chat titles; a post shows once on reload
  while unacked, queued, started, and stored (`render::waiting` unit tests cover replayed and
  already-stored posts).
- Blobs: media type, `nosniff`, `sandbox`, inline vs. attachment, 404s, non-owner refused.
- End to end: a real `Sessions` with the web channel attached and a fake turn runner that stores
  the messages and replies through `Reply::send`: a post becomes a turn, the reply reaches the open
  page, the events are acked, and a reload shows both from the transcript.
