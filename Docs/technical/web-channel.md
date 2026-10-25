# Web Channel

The built-in web channel (PLAN sections 9 and 10) is `giles_core::channels::web`, behind the Cargo
feature `channel-web` (default on, forwarded by the `giles` crate). It has two halves:

- `WebChannel` implements `giles_types::Channel` with `ChannelInfo { identity_namespace: "web",
  output: Deltas, can_initiate: false }`. It's built by the registry's built-in channel builder
  (`WebChannel::from_instance`, see [Registry and Startup](registry.md)), registered like any
  channel, and attached to the session manager by `Runtime::start_sessions` with the other
  registered channels. At most one instance may use the component.
- `WebApp` is its HTTP side on the core's server ([HTTP Server](http.md)): `giles serve` builds it
  with `WebApp::from_runtime(&runtime, shutdown)` (from `Registry::web_channel()`, the typed handle;
  it also spawns the hourly expired-session cleanup) and `web.install(RouterBuilder::new())`, which
  replaces the placeholder `/` with the chat list and merges the other routes, all behind the
  authentication middleware. `web.install_admin(builder, admin)` adds the admin pages
  ([Admin Pages](#admin-pages)). If a trusted proxy is configured, `serve` also opens
  `web.bind_proxy(&root)` before startup completes and serves `web.proxy_app(app)` on it
  ([Login](#login)).

Neither half touches the agent loop: posted messages go through the normal inbound pipeline
(router, inbox, turns; [Sessions and Turns](sessions.md)), and replies come back through
`Channel::send` or the live-output hook.

## Routes

| Route                          | Does                                                             |
| ------------------------------ | ---------------------------------------------------------------- |
| `GET /`                        | The user's chats on this instance, most recently active first, and the agent picker. |
| `GET /chats/new`               | 303 to `/chats/<new id>`; nothing is stored until a message. `?agent=<id>`: a chat with that agent. |
| `GET /chats/<id>`              | The transcript, live messages, pending prompts, and the form.   |
| `POST /chats/<id>/messages`    | Posts `text` as the user (CSRF-checked).                         |
| `POST /chats/<id>/approvals`   | An approval button: posts `<answer> <code>` (CSRF-checked).      |
| `GET /chats/<id>/events`       | Server-sent events for the page.                                 |
| `GET /blobs/<sha256>`          | An attachment.                                                   |
| `GET /login/<token>`           | A login link: a page with a button that posts the token.         |
| `POST /login`                  | Uses up the link (`token`), sets the session cookie, 303 to `/`. |
| `POST /logout`                 | Ends a cookie login (CSRF-checked), 303 to `/`.                  |
| `GET /admin`                   | The status page ([Admin Pages](#admin-pages)).                   |
| `GET /admin/plugins`           | The plugins page.                                                |
| `POST /admin/plugins/<i>/restart` | Restarts plugin instance `<i>` (CSRF-checked), 303 back.      |
| `GET /admin/agents`            | Every agent ([Agent Pages](#agent-pages)).                       |
| `GET /admin/agents/new`        | The form that creates an agent.                                  |
| `POST /admin/agents`           | Creates the agent (CSRF-checked), 303 to its page.               |
| `GET /admin/agents/<agent>`    | The agent's settings and files.                                  |
| `GET /admin/agents/<agent>/edit` | The form, filled in from the agent's files.                    |
| `POST /admin/agents/<agent>/edit` | Saves the edit (CSRF-checked), 303 to the agent's page.       |
| `GET /admin/routes`            | Every route in order ([Route Pages](#route-pages)).              |
| `GET /admin/routes/new`        | The form that adds a route.                                      |
| `POST /admin/routes`           | Adds the route (CSRF-checked), 303 to the list.                  |
| `GET /admin/routes/<id>/edit`  | The form, filled in with the route.                              |
| `POST /admin/routes/<id>`      | Replaces the route's rule (CSRF-checked), 303 to the list.       |
| `POST /admin/routes/<id>/remove` | Removes the route (CSRF-checked), 303 to the list.             |
| `POST /admin/routes/<id>/move` | Moves it one place, `direction=up` or `down` (CSRF-checked), 303. |

Every route except the two login routes needs a `WebUser` (below); without one it's a 401 page
saying how to log in. A chat id is 128 random bits as 32 lowercase hex digits (`new_chat_id`), or
`<agent>.<32 hex digits>` for a chat started with a chosen agent (`new_agent_chat_id`); either is
the conversation id (PLAN section 10): web chats are created by the user. `is_chat_id` accepts
both, and any other id in a path is a 404. A chat is `ConversationKind::Direct`; v1 has one user,
so every chat is the owner's. The chat id helpers are in `channels::chat_ids`, outside the
feature, because the router reads them.

### Choosing an Agent

A plain chat goes to whichever agent the stored routes pick. The chat list also has a picker, a
`GET` form to `/chats/new?agent=<id>`, listing the agents the user may start a chat with
(`agent_choices`, read from the running configuration, `WebApp`'s `LiveConfig`, so an agent added
while Giles runs is offered at once). v1 has one user, the owner, who may use every agent; with
roles (PLAN section 3.1), that function is where a member's `agents` globs apply. The chat it opens
has the id `<agent>.<hex>`, and the router sends its messages to that agent ahead of every route
([Identity and Routing](identity-and-routing.md#routing)), so a new agent needs no route. A chat's
page shows its agent ("with Jeeves (jeeves)"), and so does the chat list. If the agent isn't
configured (any more), or the user may not use it, `/chats/new?agent=`, the chat page, and both
POSTs answer 404 "No such agent" (and the router would drop the message anyway).

## Who Is Asking

`channels::web::WebUser { user: UserId, sender: NativeId, csrf: String, .. }` is an axum
extractor (`FromRequestParts` for any state) that reads a `WebUser` from the request's extensions,
or rejects with the login page. The authentication middleware (`login::authenticate`, applied by
`install`) inserts it; the extractor is the seam, so the admin pages (3.8) use the same one (their
router must be wrapped in the same middleware: `from_fn_with_state(web_app, authenticate)`, or
merged through `WebApp`). `sender` is the native id in the `web` namespace that posted messages
carry (`InboundMessage::sender`, with `sender_authenticated: true`); the router matches it against
`[owner] identities` as `web:<sender>`, like any channel's sender. `can_log_out()` is true for a
cookie login.

CSRF: `check_csrf(&user, &headers, form_field) -> Result<(), CsrfRejected>` accepts the token from
the `X-CSRF-Token` header (htmx: the chat page sets `hx-headers` with it, inherited by every form
on the page, including those that arrive over the event stream) or the `csrf` form field (plain
form posts without JavaScript), compared as SHA-256 digests. A mismatch is a 403 page. Every POST
that needs a login checks it before acting. The token is per login and needs no storage:

- Cookie login: SHA-256 of a fixed label and the session token. Only the browser has the token
  (the cookie is `HttpOnly`, and the store keeps only its hash), so another site can't compute it,
  and it survives restarts.
- Proxy login: SHA-256 of a random per-run key (`WebChannel::proxy_key`) and the identity, since
  the identity itself is no secret. A restart changes it; open pages then get a 403 on their next
  post and a reload fixes them.

On top of the token, the cookie is `SameSite=Lax`, so cross-site POSTs don't carry it at all.

## Login

PLAN section 3.4; v1 scope (section 1.1) is the login link and the trusted-proxy header. The
login *code* flow (a code sent to the user's default channel) needs the outbox (3.1) and isn't
built. Code: `login.rs` (middleware and routes), `links.rs`, `proxy.rs`, the cookie helpers in
`auth.rs`, and the keys in `config.rs`.

### Configuration

`WebConfig::from_json` parses the instance's `config` table (unknown keys are collected into
`WebConfig::unknown_keys` and logged by the registry, except in `proxy`, whose `RawProxy` keeps
`deny_unknown_fields` because it configures a listener and a shared secret; errors name the key and
are scrubbed of secrets):

- `public_url`: an `http`/`https` origin, normalized (lowercase host, no trailing slash); a path,
  query, fragment, or user info is an error. Login links start with it; `https` makes the cookie
  `Secure` on every listener. Unset: `http://<listener>`, the address `serve` bound for
  `core.http_listen` (`WebChannel::set_listen_addr` after the bind, so port 0 shows the real port),
  with an unspecified address shown as loopback.
- `proxy.identity_header` (required in `proxy`): the header naming the user. `proxy.listen`
  (optional TCP address) requires `proxy.secret` (at least 16 bytes; held as a `config::Secret`, so
  `Debug` output of `WebConfig` redacts it) and takes `proxy.secret_header` (default
  `X-Giles-Proxy-Secret`, must differ from the identity header); without `listen`, `secret` and
  `secret_header` are errors and the listener is the Unix socket. Neither header may be
  `Authorization`, `Cookie`, `Host`, `X-CSRF-Token`, `X-Forwarded-Proto`, or `Last-Event-ID`.

### Login Links

`WebChannel::create_login_link(&UserId, now) -> Result<LoginLink { url, expires_at },
LoginLinkError>` is what `giles admin login-link --user` calls over the admin service (2.24; the
registry's `web_channel()` gives the typed channel). It refuses anyone but the owner
(`UnknownUser`) and an owner without a `web:<user>` identity (`NoWebIdentity`: the router would drop
their messages). The link is `<link base>/login/<token>`, 256 random bits as 64 hex digits, valid
for `LOGIN_LINK_TTL` (10 minutes). Pending links live in memory only, by the SHA-256 of the token
(`links::LoginLinks`): nothing on disk can be replayed, and a restart invalidates them. Expired
links are dropped whenever one is created or redeemed.

`GET /login/<token>` doesn't use the link: it shows a page with a form that posts the token to
`POST /login`, so link previews (chat apps and mail clients fetch URLs) can't spend it. `POST
/login` redeems it (removed under the lock: single use, even with concurrent posts), creates a
session token (256 random bits) with `Store::web_session_create(token, user, now, now + 30 days)`,
deletes the browser's previous cookie session if it had one, and answers 303 to `/` with
`Set-Cookie: giles_session=<token>; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax` (`; Secure`
over HTTPS). A link that doesn't exist, was used, or expired gets the same 410 page either way.
Both answers are `Cache-Control: no-store`. The `Referrer-Policy: no-referrer` security header
keeps the token out of `Referer`.

No login CSRF check on `POST /login`: v1 has one user, so a forged login would log the victim in
as the owner, which needs a valid link anyway.

### The Session Cookie

The middleware reads `giles_session` (`WEB_SESSION_TTL`, 30 days), looks the token up with
`Store::web_session(token, now)` (expired rows don't count), and builds the `WebUser`: `user` and
`sender` are the session's user id, so a link login posts as `web:owner`. If the owner's identities
no longer include `web:<user>`, the session logs nobody in. A cookie that names no live session
gets a deleting `Set-Cookie` on the response. A store error is a 500. `WebApp::expire_sessions`
deletes expired rows now and every hour (`Store::web_sessions_expire`) until shutdown.

`POST /logout` (CSRF-checked) deletes the session and the cookie. A proxy login has nothing to end,
so the chat list shows no logout button for it.

### The Trusted-Proxy Listener

The identity header is honored only on a request marked `TrustedProxy`, a request extension only
the proxy listener's layer adds, and a client can't forge an extension. The main listener never
reads the header, so any local process (network-enabled plugins included) gets nothing by sending
it there.

- `WebApp::bind_proxy(&DataRoot) -> Result<Option<ProxyListener>, ProxyError>`: for the Unix socket,
  `check_socket_dir`, then `DataRoot::proxy_socket()` (`run/proxy.sock` or the `$TMPDIR` fallback).
  An existing socket that accepts a connection is `InUse` (another Giles); a stale one is removed;
  anything that isn't a socket is `NotASocket` and left alone. The new socket is chmod 0600 (the
  directory is 0700 already). For TCP, a plain bind. `giles serve` binds it before
  `start_sessions`, so a failure stops startup, and logs `trusted-proxy listener open at=<where>`.
- `WebApp::proxy_app(app)` wraps the main listener's whole application: the Unix socket marks every
  request; TCP first compares the SHA-256 of the secret header with the configured secret's and
  answers a bare 403 (logged) on a mismatch. `ProxyListener::serve(app, shutdown)` serves it like
  `http::serve` and removes the socket file afterwards. `serve` runs both servers in one task that
  ends at the first failure.
- With the mark, a request carrying the identity header is that identity: if
  `OwnerIdentities::contains("web", value)`, a `WebUser` for the owner with that `sender`; otherwise
  a 403 page (the value is logged so the operator can add it). Without the header, the cookie
  applies as usual.
- `X-Forwarded-Proto: https` marks the cookie `Secure` only with the mark.

### Rate Limits

PLAN section 3.4 rate-limits the login *code* flow per user name and per client address, because
an 8-character code can be guessed. Link and session tokens carry 256 random bits, so guessing is
hopeless at any rate, and a limit on redemption would only let a local process lock the owner out.
So there's no limiter yet; it comes with the code flow.

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

The agent loop's output delivery (`agent_loop::output`, see [Agent Loop](agent-loop.md)) streams
every assistant text block of a turn in a web chat through this hook: `start` at the block's first
non-whitespace delta, `push` for each delta, and `finish` once the response is stored. Whole-message
`send` keeps working on this channel either way (notices, restart notices).

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
for the model) is cut with `agent_loop::history::strip_time_prefix`, here and in chat titles, and
a media reference stored right before its media (`[media:<id> <name> <type>]`) is left out, since
the media itself is shown. A `send_attachment` call whose result isn't an error shows as an
assistant message with its caption and the media it sent (looked up by id among the media of the
loaded rows). A "New session" marker separates sessions (`/new`, idle reset). Messages sent with
`send` that the agent loop doesn't store (notices) are only on pages that were open at the time.
The chat list is
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

How the approvals code uses it: `approvals::ChannelPresenter` sends prompts for a conversation on
the web instance (`registry.web_channel()` whose `instance()` is `reply.instance()`) with
`send_approval` instead of `Reply::send`, which carries the typed-reply version of the prompt on
other channels, and calls `resolve_approval` however the approval ends (answer, typed reply,
timeout, `/stop`, or denial by a new message, PLAN section 6.4). `ApprovalOutcome` is
`approvals::ApprovalOutcome`, re-exported here ([Approvals](approvals.md)).

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

## Admin Pages

PLAN sections 8.7 and 10; code in `admin.rs` (pages) and `qr.rs` (QR codes). The admin pages are a
client of the admin service ([Admin Service](admin.md)), like `giles status`, but in-process:
`WebApp::install_admin(builder, admin: Arc<dyn AdminService>)` takes the generated
`giles.admin.v1` server trait, and `giles serve` passes the same `Arc<AdminService>` it serves on
the admin socket (`AdminListener::serve` takes the `Arc`). So the pages show exactly what the CLI
shows, and there's no token to handle: the socket's token keeps local processes (plugins included)
out of the service, and these routes are guarded by the web login instead. Tests pass a fake
implementation of the trait.

- Who: the routes sit behind the same authentication middleware as the chat pages, so no login is
  the 401 login page; then the `WebUser` must be the owner (`identity::owner()`), else a 403 page.
  v1 has only the owner; the `admin` role (PLAN section 3.1) comes with multi-user support.
- `GET /admin` calls `Status`: version, start time, the host's sandbox, the configuration error
  (always unset in v1), every configured channel with its runtime and last status (and when it
  arrived), and a table of the plugins (instance, kind, one-word state) linking to the plugins page.
- `GET /admin/plugins` calls `ListPlugins`: per plugin its state with details (pid, start time,
  retry time, restarts), last error, channel status, location, version, sandbox, what the sandbox
  lacks on this host, and the grants it requested. A plugin with a known state (supervision reports
  it) has a **Restart** form; toolsets, which don't report to the admin service yet, have none.
- `POST /admin/plugins/<instance>/restart` checks the owner, then CSRF (`check_csrf`: the `csrf`
  field or the `X-CSRF-Token` header), then calls `RestartPlugin` and answers 303 to
  `/admin/plugins`. Errors show the service's message: `NOT_FOUND` as 404,
  `FAILED_PRECONDITION` as 409, `UNAVAILABLE` as 503, `INVALID_ARGUMENT` as 400; anything else is
  logged and a generic 500.
- Every answer on the admin routes (pages, error pages, the 401 login page, redirects) is
  `Cache-Control: no-store`: a `map_response` route layer outside the authentication middleware
  applies `pages::no_store`, the helper the login routes use too. Times are UTC (`<time datetime>`
  carries RFC 3339).

A channel whose status is `NeedsAuth` gets a linking box under it: a QR code, a pair code, or a URL
(an `<a>` only for `http`/`https`, with `rel="noreferrer noopener"`; anything else, such as
`javascript:`, is shown as code), and the challenge's expiry. While any channel needs auth the page
carries `<meta http-equiv="refresh" content="5">` (`ADMIN_REFRESH_SECS`; `layout.html` has a
`head` block for it), so it follows the plugin's newest code without JavaScript; once every channel
is past `NeedsAuth` the page stops reloading.

`qr::svg(payload)` encodes the payload with the `qrcode` crate (error correction level M, no
default features: no image crate) and draws the modules itself as one `<path>` of rectangles, one
per horizontal run of dark modules, on a white square with the four-module quiet zone, so it scans
on a dark page too. The `<svg>` is inlined in the page (not an `<img>`: it's the code the page was
rendered with), and uses presentation attributes only, which the CSP allows. The payload never
appears in the markup; a payload too long for a QR code shows a note pointing at `giles status`,
which prints it.

Escaping: every string from the admin service (plugin errors, versions, channel errors, pair codes,
URLs, locations) goes through askama's HTML escaping; the QR `<svg>` is the only `|safe` value. The
service never returns secrets: plugin errors and stderr are scrubbed by the plugin host, configs
aren't part of `Status`, and remote URLs can't carry credentials (config validation).

## Agent Pages

TASKS.md 5.1 and 5.4, in `agents.rs`: `web.install_agents(builder, Arc<agents::AgentManager>)`,
which `serve` calls after `install_admin` with a manager over the runtime's registry (so the chat
list, reading the same `LiveConfig`, offers a new agent at once). Like the admin pages: the same
authentication middleware, `may_administer` (the owner only; a guest gets 403, no login 401) on
every route including the POST, `no-store` on every answer, and `check_csrf` on the POST. Unlike
them, they call the core directly (`giles_core::agents`) rather than the admin service: there are no
agent RPCs yet (PLAN section 8.7).

- The list reads the running configuration: id, label (`Name (id)`), models, and links to the
  page and to `/chats/new?agent=<id>` ([Choosing an Agent](#choosing-an-agent)).
- An agent's page looks the id up in the running configuration (anything else, `..` included, is
  a 404) and lists its directory with `agents::list_files(&agent, &viewer)` on a blocking thread.
  The walk never follows a link: entries come from `read_dir` with `file_type()` (a link is listed
  as one), files are opened with `O_NOFOLLOW | O_NONBLOCK`, and the descriptor's path
  (`F_GETPATH` on macOS, `/proc/self/fd` on Linux, the helper `load_skill` uses) must be inside the
  canonical agent directory. A directory swapped for a link mid-walk can still be listed (the
  names and sizes below it, since `read_dir` follows it), but no contents from elsewhere are shown.
  The walk stops once it has `MAX_FILES` entries. Dot names are skipped. Caps: `MAX_FILES` (200)
  entries, `MAX_DEPTH` (8), the first
  `MAX_FILE_BYTES` (64 KiB) of each file, `MAX_TOTAL_BYTES` (1 MiB) of contents per page. Text is
  UTF-8 without NUL (a character split by the cut is dropped); anything else is listed with its
  size. Contents are shown escaped in `<pre>`. Each entry has a `FileKind` (`Config`, `Soul`,
  `AgentMemory`, `UserMemory`, `GroupMemory` with the conversation percent-decoded, `JobPrompt`
  from the agent's `prompt_file`s, `Skill`, `Other`) that orders and labels it.
- Who sees which file is one hook, `FileKind::visible_to(viewer)`, applied before a file is read
  or counted. It allows everything now: v1's only user is the owner, the only one who can open the
  pages. With several users it is where a user is limited to their own `UserMemory`.
- The form offers `AgentManager::known_models()` as a `<datalist>`: every registered provider's
  `list_models()`, 5 s each, failures logged at `debug` with secrets scrubbed; any
  `<provider>/<model>` can still be typed. Lists are split at commas and whitespace
  (`agents::split_list`).
- The form asks for the name before the id. The id is derived from the name by
  `agents::agent_id_from_name`, a pure function: lowercase (`str::to_lowercase`), each space to `-`,
  every other character outside `a-z`, `0-9`, `-` removed. Nothing is trimmed or collapsed, ids
  are ASCII so `Café` gives `caf`, and whitespace other than a plain space is removed like a
  symbol. The result can be empty or `new`; creation refuses both as before. The Id input stays a
  visible, editable field and isn't `required`: `POST /admin/agents` with an empty (or blank) id
  derives it from the name, so a form posted without scripts, or by `curl`, works, and an invalid
  form comes back showing the id that was used.
- Live in the browser, `assets/agent-form.js` (embedded like the stylesheet, at
  `/static/agent-form-<hash>.js`, loaded `defer` through the layout's `head` block because the
  CSP is `script-src 'self'`) writes the same derivation into the Id as the owner types in the
  Name: `text.toLowerCase().replace(/ /g, '-').replace(/[^a-z0-9-]/g, '')`. It follows while the Id
  is empty or equals what the name derives (also on a form shown again after an error); typing in
  the Id stops it, and emptying the Id resumes it. The expression is the Rust function written
  in JavaScript (`to_lowercase` and `toLowerCase` agree on every character the filter keeps,
  U+212A KELVIN SIGN and U+0130 included); a unit test pins the expression in the file. After
  changing either, run the table in `agents/create.rs`'s tests through the script (for example
  with `bun` or `node`) by hand: this isn't automated.
- `CreateForm` (`agents.rs`) starts `soul` as `agents::DEFAULT_SOUL`, so the first `GET` shows it
  and a post with no `soul` field gets it; a `soul` that was posted empty stays empty, also when
  the form is shown again after an error.
- `AgentManager::create` (one at a time, under a lock): builds `agent.toml` with the TOML
  serializer (`agents::agent_toml`, unset values left out), validates it with
  `Config::validate_agent` (the startup checks), refuses `RESERVED_AGENT_IDS` (`new`, the form's
  address) and an id taken ignoring ASCII case, then writes the files into a staging directory,
  `agents/.<id>.creating/` (created exclusively, mode 0700; config loading skips dot names, and
  it's a sibling, so no agent's page lists it): `SOUL.md` then `agent.toml`, each as a 0600
  temporary file that is fsynced and renamed. It fsyncs the staging directory, renames it to
  `agents/<id>/` (refused if that exists and isn't empty), and fsyncs `agents/`, so the agent's
  directory appears whole or not at all; on any failure the staging directory is removed. Then
  `LiveConfig::add_agent`; if that refuses (another creator took the id, ignoring case, meanwhile)
  the new directory is removed. A staging directory left by a crash breaks nothing (startup skips
  it) and is removed when `serve` builds its `AgentManager` at the next start. Of two creations
  with the same id one wins; the other is told the id is taken, as is one whose directory exists
  on disk. An invalid form comes back with `CreateError::form_message` (key and reason, never a
  file path): 409 for a taken id, else 400. Write failures are logged and answered 500.
  The handler stores `SOUL.md` with `\n` line breaks: browsers submit a textarea's as `\r\n`.

### Editing

`AgentManager::edit_form(id)` and `AgentManager::edit(&AgentEdit)` (`agents/edit.rs`), behind
`GET` and `POST /admin/agents/<agent>/edit` (`EditForm`, `edit_page`, `edit` in `agents.rs`). The
id in the path must be a configured agent, exactly (anything else, `..` included, is a 404); the
form has no id field.

- **What the form is rendered from.** The files on disk, not the running configuration: `agent.toml`
  is read with `toml_edit` for `name`, `models`, `tools`, `ask`, `timezone` (a key that isn't a
  string or list of strings shows empty), and `SOUL.md` (CRLF shown as LF). `AgentForm::version` is
  the SHA-256 of both files (name, whether it exists, length, and bytes of each), carried in a
  hidden field. `read_text` refuses what the form must not write through: a symbolic link (the
  rename would replace the link), anything but a regular file, more than `MAX_EDIT_FILE_BYTES`
  (1 MiB), text that isn't UTF-8, and TOML that doesn't parse (`EditError::Unsupported`, a 409
  page on `GET`).
- **`edited_toml`.** A pure function from the text and the edit to the new text, with `toml_edit`.
  A managed key whose value already equals the field is not touched (so an unchanged form gives
  the file back byte for byte, and the text is returned as is rather than re-serialized when
  nothing changed); a changed one gets a new value that copies the decor of the old one (the space
  and the comment after it), keeping its place; a missing one is appended to the top-level
  table (`toml_edit` prints key-values before sub-tables, so never inside `[[jobs]]`); an empty
  one (`name`, `tools`, `ask`, `timezone`) is removed. `models` is always written, even empty, so
  validation says "at least one model". Removing a key takes its leading comment with it, except
  at the first key of the file (a header: kept) and up to the last blank line above the key
  (standalone notes: kept); what is kept moves to the next key's decor.
- **`edit`** (under the manager's one lock, shared with creation and removal): reads the files and
  returns `EditError::Stale` if `version` differs; checks `SOUL.md`'s length only if it changed
  (`SoulTooLong`; a hand-written longer file can still have its settings edited); builds the text
  and validates it with `Config::validate_agent` (`Invalid`); writes only the files that changed,
  through `commit`: each to `.<name>.edit.tmp` in the agent's directory (fsynced, the mode of the
  file it replaces), then reads both files again and compares with what it started from (`Stale`
  again, narrowing the window for a hand edit to the renames), then renames `SOUL.md` and
  `agent.toml` into place and fsyncs the directory. A failed second rename puts the first file
  back from the contents read earlier (`restore`); temporary files are removed on every path.
  Then `LiveConfig::replace_agent`. The form's `\r\n` become `\n` before the comparison, so a
  `SOUL.md` the owner didn't touch isn't rewritten.
- **Failures** come back as the form with the input and the same `version` (400 for invalid
  values, 409 for `Stale` and `Unsupported`; `Stale` adds a reload link), or as error pages
  (404, 500 for `Io` and `Task`, which are logged).
- **Taking effect.** `replace_agent` swaps in the validated `AgentConfig` and bumps
  `LiveConfig::subscribe`'s counter but keeps the agent's incarnation. New sessions read the new
  agent (the session prompt is assembled at a session's first turn); the agent loop's cached prompt
  of a running session is untouched, so it keeps its tools and `system` until compaction rebuilds
  `system` (PLAN section 6.3). The scheduler reads the agent again at every job run and restarts the
  agent's config job tasks only if its `[[jobs]]` changed ([Scheduler](scheduler.md#timing)), which
  also covers `[[jobs]]` the owner changed by hand before saving the form (the whole file is
  loaded). Core tools and the session manager's `AgentPolicies` read the live configuration at each
  use.

The new agent is usable at once because everything that looks agents up reads the running
configuration: the agent loop and core tools through `Registry::config()`, the session manager's
`AgentPolicies` and the router through the shared `LiveConfig`. Config jobs of agents created this
way (added to `agent.toml` by hand) start at the next restart, with the rest of a configuration
change; general reload stays deferred (D28).

## Route Pages

TASKS.md 5.2, in `routes.rs`: `web.install_routes(builder, Arc<routes::Routes>)`, which `serve`
calls last, with the runtime's `Routes` (the object whose `RouteList` the router reads). Like the
agent pages: the same authentication middleware, `may_administer` on every route including the
POSTs, `no-store` on every answer, and `check_csrf` (header or field) on every POST. Every change
is one `Routes` call, which validates, writes, and gives the router the new list before it
returns ([Identity and Routing](identity-and-routing.md#stored-routes)), so the next inbound
message uses it.

- The list shows each route's position, channel, kind, conversation, and agent (a link to the
  agent's page, or plain text if it's gone), with Edit, Up, Down, and Remove. Up is left out for
  the first route and Down for the last. A route with a problem (`RouteView::problems`: never
  used, channel instance or agent not configured) has a warning row under it. A notice says how
  many `[[routes]]` `giles.toml` still has (`Config::legacy_routes`).
- Buttons are plain `POST` forms with the CSRF token in a hidden field, so the pages work without
  scripts and removing is never a link. Routes in paths and forms are the store's stable ids
  (`RouteId`), never positions: a page opened before another change can't act on a different
  route; one for a route that's gone answers 404 "No such route". An id that isn't a number is the
  same 404.
- The add and edit forms are one template. Channel, kind, and agent are `<select>`s of what exists
  now (the running configuration), plus the route's current value when it's no longer there, so
  editing a route to a vanished agent shows it. The add form also has Position: "At the end" or
  "Before route N (what it matches)". An invalid form comes back with the same values and
  `RouteError`'s message: 400 for an invalid value, 409 for a route that could never be used or
  would hide another; stale ids are 404; database failures are logged and answered 500. Messages
  hold only what the owner typed or ids of agents and instances, escaped by the template.
- A move that would hide a route is refused with the reason, as is an edit; reordering therefore
  passes only through valid lists.

## Templates and Styles

Templates are in `crates/giles-core/templates/web/`: pages `chats.html`, `chat.html`, `error.html`,
`login_required.html`, `login.html`, `admin_status.html`, `admin_plugins.html`, `admin_agents.html`,
`admin_agent.html`, `admin_agent_new.html`, `admin_routes.html`, `admin_route_form.html` (extending
`layout.html`; `admin_nav.html` is included by the admin pages), and fragments `message.html`,
`approval.html`, `compose.html`, `attachments.html` (rendered by `render.rs`). Nothing inline: no
scripts, styles, or `hx-on` handlers (the CSP and `allowEval: false` forbid them). The chat page is
`hx-ext="sse"` with `sse-connect`; `#messages` has `sse-swap="message"` and `hx-swap="beforeend"`.
The compose form replaces itself (`hx-target="this"`, `hx-swap="outerHTML"`); approval forms use
`hx-swap="none"`. Every form also works as a plain form post. Styles are classes in
`assets/giles.css`.

## Tests

`crates/giles-core/src/channels/web/tests.rs` (router-level with `tower::ServiceExt::oneshot`, a
temporary store; the chat tests inject a `WebUser`, the login tests go through the real
middleware), plus unit tests in `hub.rs`, `render.rs`, `pages.rs`, `auth.rs`, `config.rs`,
`links.rs`, `proxy.rs`, `admin.rs`, `qr.rs`:

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
- Live output end to end: the daemon's wiring (`Runtime`, `AgentLoop`, sessions, `WebApp`) with a
  fake provider that streams slowly: a post becomes a turn whose text reaches the open page as a
  live message with out-of-band updates, then its final rendering, and a reload shows the stored
  reply once.
- Login: a link shows its button without being used, logs in once with the right cookie flags and
  a 30-day `web_sessions` row, then is 410; expired, unknown, and malformed links; link creation
  refusals; expired, unknown, and orphaned sessions logged out with the cookie deleted; the hourly
  cleanup; logout with CSRF; a cookie login's CSRF token; `public_url` and `X-Forwarded-Proto`
  making cookies `Secure` (the latter only through the proxy).
- Proxy: the identity header ignored on the main listener and honored on the proxy app, unknown
  identities refused, posts carry the header's identity and need its CSRF token; a TCP listener
  needs the exact secret; binding (socket mode 0600, `InUse`, stale socket replaced, non-socket
  left alone). `crates/giles/tests/serve.rs` runs the daemon with a proxy socket and checks it
  over real connections, and that the socket is gone after exit.
- Admin pages (`tests/admin.rs`, over a fake admin service): anonymous requests get 401 and a
  non-owner 403 on every admin route, with nothing restarted; the status page shows the fake state
  with plugin strings escaped and no reload; a `NeedsAuth` QR channel gets an `<svg>` (a new
  payload, a new code), the refresh, and never the payload text; pair codes and URLs are escaped,
  and a `javascript:` URL isn't a link; the plugins page shows details and a restart form only for
  supervised plugins; restarting needs the CSRF token (field or header) and shows the service's
  `NOT_FOUND` escaped; every admin answer, refusals and redirects included, is `no-store`. `qr.rs`
  checks that the SVG draws exactly the code's dark modules inside the quiet zone, and that an
  oversized payload is an error. `crates/giles/tests/serve.rs` checks that the daemon serves
  `/admin` from its own admin service, behind the login.
- Route pages (`tests/routes.rs`, over the test configuration): anonymous requests get 401 and a
  non-owner 403 on every route including the POSTs, with nothing changed; every POST needs the
  CSRF token (a wrong or missing one is 403, the header works as well as the field); the list is
  in order with escaped values, the agent links, no Up on the first route and no Down on the last,
  warnings for a route that never matches or points at a gone instance or agent, and the notice
  for `[[routes]]` still in `giles.toml`; adding stores the route and the router's `RouteList`
  picks it for the next message, at the end or before a chosen route; an invalid form (unknown
  instance or agent, bad kind, control characters) is shown again with the problem, its values
  and nothing stored; a duplicate, a shadowed route, and a route that would hide another are 409;
  edit, remove (not by `GET`), and move change which route wins in the router's list; a move that
  would hide a route is refused; ids that aren't numbers or routes are 404.
