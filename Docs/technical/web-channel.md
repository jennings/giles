# Web channel

The built-in web channel (PLAN sections 9 and 10) is `doozers_core::channels::web`, behind the Cargo
feature `channel-web` (default on, forwarded by the `doozers` crate). It has two halves:

- `WebChannel` implements `doozers_types::Channel` with
  `ChannelInfo { identity_namespace: "web", output: Deltas, can_initiate: false }`. It's built by
  the registry's built-in channel builder (`WebChannel::from_instance`, see
  [Registry and startup](registry.md)), registered like any channel, and attached to the session
  manager by `Runtime::start_sessions` with the other registered channels. At most one instance may
  use the component.
- `WebApp` is its HTTP side on the core's server ([HTTP Server](http.md)): `doozers serve` builds it
  with `WebApp::from_runtime(&runtime, shutdown)` (from `Registry::web_channel()`, the typed handle;
  it also spawns the hourly expired-session cleanup) and `web.install(RouterBuilder::new())`, which
  replaces the placeholder `/` with the chat list and merges the other routes, all behind the
  authentication middleware. `web.install_admin(builder, admin)` adds the admin pages
  ([Admin pages](#admin-pages)). If a trusted proxy is configured, `serve` also opens
  `web.bind_proxy(&root)` before startup completes and serves `web.proxy_app(app)` on it
  ([Login](#login)).

Neither half touches the agent loop: posted messages go through the normal inbound pipeline (router,
inbox, turns; [Sessions and turns](sessions.md)), and replies come back through `Channel::send` or
the live-output hook.

## Routes

| Route                          | Does                                                             |
| ------------------------------ | ---------------------------------------------------------------- |
| `GET /`                        | The user's chats on this instance that aren't archived, most recently active first, and the agent picker. |
| `GET /chats/archived`          | The user's archived chats, most recently active first.           |
| `GET /chats/new`               | 303 to `/chats/<new id>`; nothing is stored until a message. `?agent=<id>`: a chat with that agent. |
| `GET /chats/<id>`              | The chat list it is in, then the chat: transcript, live messages, pending prompts, and the form. Its title is its first message's, from the list. Every `/chats/<id>` route is the chat's user's only: 404 for anyone else. |
| `POST /chats/<id>/messages`    | Unarchives the chat, then posts `text` as the user (CSRF-checked). |
| `POST /chats/<id>/archive`     | Archives the user's chat (CSRF-checked), 303 to `/` ([Archiving and deleting chats](#archiving-and-deleting-chats)). |
| `POST /chats/<id>/unarchive`   | Unarchives the chat (CSRF-checked), 303 to the chat.            |
| `POST /chats/<id>/delete`      | Deletes the user's chat and its workspaces (CSRF-checked), 303 to the list it was in; 409 while it's being worked on. |
| `POST /chats/<id>/approvals`   | An approval button: posts `<answer> <code>` (CSRF-checked).      |
| `GET /chats/<id>/events`       | Server-sent events for the page.                                 |
| `GET /blobs/<sha256>`          | An attachment, for a user who owns a message that refers to it ([Blobs](#blobs)). |
| `GET /jobs`                    | The user's scheduled jobs with every agent ([Jobs page](#jobs-page)). |
| `GET /jobs/new`                | The form that schedules a job.                                   |
| `POST /jobs`                   | Schedules the job (CSRF-checked), 303 to the list.               |
| `GET /jobs/<id>/edit`          | The form, filled in from the user's job.                         |
| `POST /jobs/<id>/edit`         | Saves the edit (CSRF-checked), 303 to the list.                  |
| `POST /jobs/<id>/disable`      | Switches the user's job off (CSRF-checked), 303 to the list.     |
| `POST /jobs/<id>/enable`       | Switches it back on (CSRF-checked), 303; 409 for a one-shot whose time passed. |
| `POST /jobs/<id>/delete`       | Deletes the user's job (CSRF-checked), 303 to the list.          |
| `GET /login`                   | The login page: the ways to sign in ([The login page](#the-login-page)); 303 to `/` when already logged in. |
| `GET /invite/<token>`          | An invite or recovery link: a page with a button that posts the token and the sign-in nonce ([Invite and recovery links](#invite-and-recovery-links)). |
| `POST /invite`                 | Checks the nonce, spends the link (`token`), sets the session cookie, 303 to `/setup`. |
| `GET /setup`                   | Where a user lands after a link: the sign-in methods they can add. |
| `POST /login/password`         | Checks the nonce, the limits, and the password (`username`, `password`); sets the session cookie, 303 to `/`. The login page again with the reason otherwise: 401, 429, or 503 ([Passwords](#passwords)). |
| `GET /setup/password`          | The form that sets or changes the user's password; 403 unless the login came from an invite or recovery link or the user confirmed it's them in the last 10 minutes ([Step-up re-verification](#step-up-re-verification)). |
| `POST /setup/password`         | Sets the password (CSRF-checked), 303 to the page, which says it is saved; the form again with why (400) if it breaks a rule. |
| `POST /login/passkey/discover` | JSON `{nonce}` (nonce-checked, rate-limited): the options of a sign-in that names no user, and a ceremony id ([Passkeys](#passkeys)). |
| `POST /login/passkey/username` | JSON `{nonce, username}`: the options of a sign-in by username; the same answer for a name that isn't a user. |
| `POST /login/passkey/finish`   | JSON `{nonce, ceremony, credential}`: checks the passkey and signs in, answering `{redirect}` and the session cookie; a 401 with one message otherwise. |
| `GET /setup/passkey`           | The user's passkeys and the two buttons that add one; 403 under the same rule as `/setup/password` ([Passkeys](#passkeys)). |
| `POST /setup/passkey/start`    | JSON `{kind}` (CSRF-checked): the options for `navigator.credentials.create` and a ceremony id; 403 for any other login, 409 when passkeys aren't available. |
| `POST /setup/passkey/finish`   | JSON `{ceremony, name, credential}` (CSRF-checked): verifies and stores the passkey, answers `{redirect}`. |
| `GET /account/reverify`        | `?next=`: the user's own ways to confirm it's them: a password form, a passkey button, and a link for each connected provider ([Step-up re-verification](#step-up-re-verification)). |
| `POST /account/reverify/password` | `password`, `next` (CSRF-checked, counted like a login attempt): records the confirmation and 303 to `next`; the page again with why (401, 429, or 503) otherwise. |
| `POST /account/reverify/passkey/start` | JSON (CSRF-checked): the options of a sign-in with one of the user's own passkeys, and a ceremony id bound to the login. |
| `POST /account/reverify/passkey/finish` | JSON `{ceremony, next, credential}` (CSRF-checked): checks the passkey, records the confirmation, answers `{redirect}`; a 401 with one message otherwise. |
| `GET /account/reverify/oidc/<id>` | `?csrf=&next=`: sends the browser to provider `id` to confirm it's the user ([OpenID Connect](#openid-connect)). |
| `POST /logout`                 | Ends a cookie login (CSRF-checked), 303 to `/`.                  |
| `GET /admin`                   | The status page ([Admin pages](#admin-pages)).                   |
| `GET /admin/plugins`           | The plugins page.                                                |
| `POST /admin/plugins/<k>/<i>/restart` | Restarts the `<k>` instance `<i>` (CSRF-checked), 303 back. |
| `GET /admin/agents`            | Every agent ([Agent pages](#agent-pages)).                       |
| `GET /admin/agents/new`        | The form that creates an agent.                                  |
| `POST /admin/agents`           | Creates the agent (CSRF-checked), 303 to its page.               |
| `GET /admin/agents/<agent>`    | The agent's settings and files.                                  |
| `GET /admin/agents/<agent>/edit` | The form, filled in from the agent's files.                    |
| `POST /admin/agents/<agent>/edit` | Saves the edit (CSRF-checked), 303 to the agent's page.       |
| `GET /admin/agents/<agent>/remove` | What removing the agent would do, and the button.            |
| `POST /admin/agents/<agent>/remove` | Removes the agent (CSRF-checked); 200 with what happened, or the page again with why not (409). |
| `GET /admin/agents/<agent>/memory` | The agent's memories the user can see, and a search (`q`) ([Memory page](#memory-page)). |
| `POST /admin/agents/<agent>/memory` | Adds a memory (CSRF-checked), 303 back. |
| `POST /admin/agents/<agent>/memory/edit` | Changes a memory's content and flags (CSRF-checked), 303 back; 409 if it changed since the form. |
| `POST /admin/agents/<agent>/memory/forget` | Forgets a memory (CSRF-checked), 303 back. |
| `GET /admin/workspaces`        | Every environment workspace with its live state ([Workspaces page](#workspaces-page)). |
| `GET /admin/workspaces/<id>/stop` | The confirmation for stopping a workspace a turn is using; a redirect to the list otherwise. |
| `POST /admin/workspaces/<id>/stop` | Stops the workspace (CSRF-checked), 303 to the list.         |
| `GET /admin/workspaces/<id>/delete` | What deleting does, with a warning if a turn is using it, and the button. |
| `POST /admin/workspaces/<id>/delete` | Deletes the workspace (CSRF-checked), 303 to the list.     |
| `GET /admin/ssh-hosts`         | The known SSH hosts by environment, and the form that adds one ([Known SSH hosts page](#known-ssh-hosts-page)). |
| `POST /admin/ssh-hosts/add`    | Pins the key of a pasted `known_hosts` line (CSRF-checked), 303 to the list; the page again with the reason (400, 409) if it can't. |
| `GET /admin/ssh-hosts/<id>/delete` | What deleting the key does, and the button.                  |
| `POST /admin/ssh-hosts/<id>/delete` | Deletes the key (CSRF-checked), 303 to the list.            |
| `GET /admin/identity-providers` | The OpenID Connect providers with their redirect URIs ([Identity provider pages](#identity-provider-pages)). |
| `GET /admin/identity-providers/new` | The form that adds a provider. |
| `POST /admin/identity-providers` | Adds the provider (CSRF-checked), 303 to the list; the form again with the reason if it can't be used. |
| `GET /admin/identity-providers/<id>/edit` | The form, filled in with the provider; it never has the client secret. |
| `POST /admin/identity-providers/<id>` | Replaces the provider's settings (CSRF-checked), 303 to the list. |
| `POST /admin/identity-providers/<id>/enabled` | Enables or disables the provider (CSRF-checked), 303 to the list. |
| `GET /admin/identity-providers/<id>/remove` | What removing the provider does, and the button. |
| `POST /admin/identity-providers/<id>/remove` | Removes the provider, its connections, and its secret (CSRF-checked), 303 to the list. |
| `GET /login/oidc/<id>` | Starts a sign-in: 303 to the provider ([OpenID Connect](#openid-connect)). |
| `GET /setup/oidc/<id>?csrf=` | Starts connecting an account to the logged-in user: 303 to the provider. |
| `GET /auth/oidc/<id>/callback` | Where the provider sends the browser back; finishes the sign-in or connection. |
| `GET /admin/routes`            | Every route in order ([Route pages](#route-pages)).              |
| `GET /admin/routes/new`        | The form that adds a route.                                      |
| `POST /admin/routes`           | Adds the route (CSRF-checked), 303 to the list.                  |
| `GET /admin/routes/<id>/edit`  | The form, filled in with the route.                              |
| `POST /admin/routes/<id>`      | Replaces the route's rule (CSRF-checked), 303 to the list.       |
| `POST /admin/routes/<id>/remove` | Removes the route (CSRF-checked), 303 to the list.             |
| `POST /admin/routes/<id>/move` | Moves it one place, `direction=up` or `down` (CSRF-checked), 303. |

Every route except `GET /invite/<token>`, `POST /invite`, `POST /login/password`, and the
`POST /login/passkey/...` routes needs a `WebUser` (below); without one it's a 401 page saying how
to log in. A chat id is 128 random bits as 32 lowercase hex digits (`new_chat_id`), or
`<agent>.<32 hex digits>` for a chat started with a chosen agent (`new_agent_chat_id`); either is
the conversation id (PLAN section 10): web chats are created by the user. `is_chat_id` accepts both,
and any other id in a path is a 404. A chat is `ConversationKind::Direct`, and belongs to the user
who started it ([Whose data is whose](#whose-data-is-whose)). The chat id helpers are in
`channels::chat_ids`, outside the feature, because the router reads them.

### Choosing an agent

A plain chat goes to whichever agent the stored routes pick. The chat list also has a picker, a
`GET` form to `/chats/new?agent=<id>`, listing the agents the user may start a chat with
(`agent_choices`, read from the running configuration, `WebApp`'s `LiveConfig`, so an agent added
while Doozers runs is offered at once). Every user, admin or member, may use every agent; with
per-user allowlists, that function is where they apply. The jobs page offers the same agents. The
chat it opens has the id `<agent>.<hex>`, and the router sends its messages to that agent ahead of
every route ([Identity and Routing](identity-and-routing.md#routing)), so a new agent needs no
route. A chat's page shows its agent ("with Jeeves (jeeves)"), and so does the chat list. If the
agent isn't configured (any more), `/chats/new?agent=`, the chat page, and both POSTs answer 404 "No
such agent" (and the router would drop the message anyway).

## Who is asking

`channels::web::WebUser { user: UserId, sender: NativeId, csrf: String, .. }` is an axum extractor
(`FromRequestParts` for any state) that reads a `WebUser` from the request's extensions, or rejects
with the 401 login-required page. The authentication middleware (`login::authenticate`, applied by
`install`) inserts it; the extractor is the seam, so the admin pages use the same one (their router
must be wrapped in the same middleware: `from_fn_with_state(web_app, authenticate)`, or merged
through `WebApp`). `sender` is the native id in the `web` namespace that posted messages carry
(`InboundMessage::sender`, with `sender_authenticated: true`); the router matches it against the
users' identities as `web:<sender>`, like any channel's sender. `can_log_out()` is true for a cookie
login.

CSRF: `check_csrf(&user, &headers, form_field) -> Result<(), CsrfRejected>` accepts the token from
the `X-CSRF-Token` header (htmx: the chat page sets `hx-headers` with it, inherited by every form on
the page, including those that arrive over the event stream) or the `csrf` form field (plain form
posts without JavaScript), compared as SHA-256 digests. A mismatch is a 403 page. Every POST that
needs a login checks it before acting. The token is per login and needs no storage:

- Cookie login: SHA-256 of a fixed label and the session token. Only the browser has the token (the
  cookie is `HttpOnly`, and the store keeps only its hash), so another site can't compute it, and it
  survives restarts.
- Proxy login: SHA-256 of a random per-run key (`WebChannel::proxy_key`) and the identity, since the
  identity itself is no secret. A restart changes it; open pages then get a 403 on their next post
  and a reload fixes them.

On top of the token, the cookie is `SameSite=Lax`, so cross-site POSTs don't carry it at all.

## Whose data is whose

Every page and post that takes the id of a user's object is that user's alone. Anyone else gets the
404 page of an object that doesn't exist, never a 403 that says it does. The admin pages are the
exception: they are about the instance, not about an object of someone's, and answer 403 to a member
([Admin pages](#admin-pages)). The rule, per object:

| Object | Belongs to | How a handler decides | Another user gets |
| ------ | ---------- | --------------------- | ----------------- |
| Chat: page, event stream, post, approval button, archive, unarchive, delete | The user of its sessions | `pages::own_chat` runs first in every `/chats/<id>` handler; below | 404 "No such chat" |
| Chat list, archived list | The signed-in user | `Store::channel_conversations(instance, user, ..)` lists only the user's sessions | Their own chats only |
| Session | Its user | The session pump drops messages and commands in a conversation another user's session holds | Dropped |
| Agent picker | Nobody's | Every user may use every agent | The same list |
| Transcript and waiting messages | The user of the sessions | `Store::conversation_transcript(instance, chat, user, ..)` reads only the user's sessions | Nothing, even past the check |
| Approval prompt | The turn's requester | The button is a chat route; the core matches the user and the session | 404 on the web; an ordinary message elsewhere ([Approvals](approvals.md#replies)) |
| Scheduled job | Its creator | `jobs::own_job` loads the job and compares `creator` | 404 |
| Config job | The user `owner` | The page lists config jobs only for that user, whatever their role | Not listed |
| Memory | `MemoryOwner::User`, or shared | The memory store, asked as the user's viewer; a group's section is admins' | 404 "no memory you can see"; shared ones are admins' to write |
| SSH key | Its user | `SshKeys` compares the key's user on every call | 404 |
| OAuth connection | The user | Tokens live under `users/<user>/..`; a pending sign-in is bound to the user who began it | "Expired" |
| Workspace | Its conversation or job run | Reached through its chat, and listed for admins on the [Workspaces page](#workspaces-page) | The chat's 404 |
| Blob | Not owned: stored once per content | `BlobStore::reference_of`: a message of the user's refers to it ([Blobs](#blobs)) | 404, an admin included |
| Usage | Recorded with the requester | No page shows it per user; the admin report sums it by agent and model | Admins' only |

A web chat is claimed, not assigned. `/chats/new` mints a random id and stores nothing until the
first message. A chat with a session is its user's: `Store::conversation_access` says `Own`,
`Unstarted`, or `Foreign` (any session, ended ones and other agents' included, with another user or
none), and `own_chat` answers 404 for `Foreign`. A chat with no session yet is the user's unless a
message from another user is waiting in it for the core (`WebChannel::posted_by_other`): the first
poster claims it. The id of a chat nobody has started is therefore a secret until its first message,
and a page for an unknown id shows an empty new chat where another user's chat is 404. That tells
only someone who already has the id that the chat exists.

The core enforces the same rule below the web pages. For a web chat the session pump drops a message
or command in a conversation with a session of another user, ended sessions included, so a post that
got past the pages can't run in someone else's session
([Sessions and turns](sessions.md#the-inbound-pipeline)). `own_chat` runs before the handler touches
the chat's hub, so a stranger can't subscribe to a chat's events, make its hub, or mark its prompt
answered.

## Login

PLAN section 3.4. A browser signs in with a one-time invite or recovery link, a username and
password, a passkey, or an OpenID Connect identity provider, or the trusted-proxy header marks the
user. The login *code* flow (a code sent to the user's default channel) is dropped. Code: `login.rs`
(middleware, the login page, `start_session`, and logout), `invite.rs` (the link pages and the setup
page), `signin.rs` (the sign-in methods the pages offer), `password.rs` (the rules and the hashing),
`password_pages.rs` (the password routes), `passkey.rs` (the relying party and the ceremonies),
`pending.rs` (the table the ceremonies are kept in), `passkey_pages.rs` (the passkey setup routes),
`passkey_signin.rs` (the passkey sign-in routes), `oidc.rs` (the OpenID Connect protocol),
`oidc_pages.rs` (its sign-in routes), `oidc_admin.rs` (the provider pages), `limits.rs` (the limits
on guessing), `proxy.rs`, the cookie and nonce helpers in `auth.rs`, and the keys in `config.rs`.

### Configuration

`WebConfig::from_json` parses the instance's `config` table (unknown keys, `proxy` included, are
collected into `WebConfig::unknown_keys` and logged by the registry; errors name the key and are
scrubbed of secrets):

- `public_url`: an `http`/`https` origin, normalized (lowercase host, no trailing slash); a path,
  query, fragment, or user info is an error. Invite and recovery links start with it; `https` makes
  the cookies `Secure` on every listener. Unset: `http://<listener>`, the address `serve` bound for
  `core.http_listen` (`WebChannel::set_listen_addr` after the bind, so port 0 shows the real port),
  with an unspecified address shown as loopback.
- `proxy.identity_header` (required in `proxy`): the header naming the user. `proxy.listen`
  (optional TCP address) requires `proxy.secret` (at least 16 bytes; held as a `config::Secret`, so
  `Debug` output of `WebConfig` redacts it) and takes `proxy.secret_header` (default
  `X-Doozers-Proxy-Secret`, must differ from the identity header); without `listen`, `secret` and
  `secret_header` are errors and the listener is the Unix socket. Neither header may be
  `Authorization`, `Cookie`, `Host`, `X-CSRF-Token`, `X-Forwarded-Proto`, or `Last-Event-ID`.

### Invite and recovery links

One mechanism makes every one-time link: `Users::create_invite(user, kind, created_by, now)` stores
a `user_invites` row ([User invites](storage.md#user-invites)) and returns the token and expiry
(`INVITE_TTL`, 72 hours). The token is 256 random bits as 64 lowercase hex digits
(`is_invite_token`); the table keeps its SHA-256, so links survive a restart and nothing stored can
be replayed. The user must exist and be active. Creating a link gives the user the identity
`web:<id>`, verified as `admin`, if they lack it (a web sign-in sends as `web:<id>`, so without it
the router would drop their messages), and refuses a user whose `web:<id>` belongs to another
(`WebIdentityTaken`). A second link for a user doesn't end the first.

- `kind = invite` sets up a new account: an admin creates the user, then a link for them. The admin
  pages that do it come later; the tests make one through this API.
- `kind = recovery` signs in a user who lost every sign-in method. It adds and removes nothing: the
  user's identities, sessions, and other methods stay. `doozers admin login-link` makes one over the
  admin service ([Admin service](admin.md#users-and-the-operator)), after `Users::login_user` has
  found the user (and created the first admin on a fresh install). The web channel only says where
  links point: `WebChannel::invite_url(token)` is `<link base>/invite/<token>`.

`GET /invite/<token>` doesn't spend the link: if `Store::user_invite(token, now)` finds it unused
and unexpired, it shows a page with a form that posts `token` and `nonce` to `POST /invite`, so link
previews (chat apps and mail clients fetch URLs) can't spend it. The page also sets the nonce cookie
([Sign-in CSRF](#sign-in-csrf)). A token that is malformed, unknown, used, or expired gets the same
410 page, which also tells the operator about `doozers admin login-link`.

`POST /invite` checks the nonce first, then reads the link without spending it. If its user can no
longer sign in (disabled, gone, or without `web:<id>`) it answers 403 and leaves the link for when
they can. Otherwise `Store::user_invite_redeem` spends it in one statement, so of two concurrent
posts one gets the link and the other the 410 page. The winner goes through `login::start_session`,
which every way of signing in that answers with a page ends in. It calls `login::begin_session`,
which creates a session token (256 random bits) with
`Store::web_session_create(token, user, method, now, now + 30 days)`, where `method` is `invite`,
`recovery`, or `password` and is stored with the session, and deletes the browser's previous cookie
session if it had one. `start_session` answers 303 to the given page with
`Set-Cookie: doozers_session=<token>; Path=/; Max-Age=2592000; HttpOnly; SameSite=Lax` (`; Secure`
over HTTPS). `POST /invite` sends the browser to `/setup` and deletes the nonce cookie. All these
answers are `Cache-Control: no-store`. The `Referrer-Policy: no-referrer` security header keeps the
token out of `Referer`.

`GET /setup` needs a login. It lists `WebApp::setup_options(user)`, the sign-in methods the user can
add (`signin.rs`), each with a label, a sentence, and the page that adds it. Each method contributes
its entry there when the user may add it. Today those are passkeys ([Passkeys](#passkeys)) and the
password ([Passwords](#passwords)), for a login that may change sign-in methods
([Step-up re-verification](#step-up-re-verification)). With no entries the page says that this login
can't add a way to sign in, and points to the account page, where the user can confirm it's them,
and to a new link.

### The login page

`GET /login` (`login.rs`, template `login.html`) lists the ways to sign in.
`WebApp::login_options(nonce)` (`signin.rs`) gives the methods that are a link to another page, each
a label and the page of its method (one for each enabled [identity provider](#openid-connect)); with
none, the page has no list. Below it is the password form, which is on the page itself
([Passwords](#passwords)), and always the paragraph that says to open an invite or recovery link and
how the operator makes one. A request that needs a login and has none gets the 401 login-required
page, which only says so and links to `/login`. The page answers `Cache-Control: no-store`, and 303
to `/` for a browser that is logged in.

The page issues the [sign-in nonce](#sign-in-csrf): `login::render_login` puts
`auth::nonce_for_form` in the password form and sets the cookie (`auth::nonce_cookie`), as
`invite_page` does, and `POST /login/password` calls `auth::check_nonce` first. Every response that
holds the form does the same, the failed sign-in's page included.

The password form is a partial, `password_form.html`, so that another method can put it behind "Use
a password instead". `WebApp::password_collapsed()` (`signin.rs`) says whether the page does that,
and is true where passkeys are available ([Passkeys](#passkeys)): the form is inside
`<details class="password-signin">` with that summary (open again after a failed sign-in), and needs
no script, and the passkey section is above it. Where they aren't, the form is shown under its own
heading.

### Sign-in CSRF

A sign-in has no session to hold a CSRF token yet, and a forged one is a login CSRF: another site
posts the attacker's own credentials or link from the victim's browser, and the victim ends up in
the attacker's account. So every POST that signs a browser in carries a nonce: the form has a hidden
`nonce` field, and the browser has the same value in the `doozers_login_nonce` cookie (`HttpOnly`,
`SameSite=Strict`, the whole site, 15 minutes, `Secure` over HTTPS).
`auth::check_nonce(headers, field)` runs before the handler reads anything else, compares SHA-256
digests, and answers 403 when the cookie is missing or isn't a nonce (32 lowercase hex digits), the
field is missing, or they differ. Another site's form post doesn't carry a `SameSite=Strict` cookie,
and can't read a value only this site's page holds.

A page with a sign-in form calls `auth::nonce_for_form(headers)`: the browser's own nonce if it has
a well-formed one (so a second tab's form keeps working), else a new one, and sets it as the cookie
in the same response (`auth::nonce_cookie`). A successful sign-in deletes the cookie
(`auth::clear_nonce_cookie`).

### The session cookie

The middleware reads `doozers_session` (`WEB_SESSION_TTL`, 30 days), looks the token up with
`Store::web_session(token, now)` (expired rows don't count; the row also records how the session
signed in, `method`), and builds the `WebUser`: `user` and `sender` are the session's user id, so a
cookie login posts as `web:<id>` (`web:owner` for the owner). If the user is gone or disabled, or no
longer has the identity `web:<user>`, the session logs nobody in. A cookie that names no live
session gets a deleting `Set-Cookie` on the response. A store error is a 500.
`WebApp::expire_sessions` deletes expired sessions and expired invite links now and every hour
(`Store::web_sessions_expire`, `Store::user_invites_expire`) until shutdown.

A request that finds a live session records its use as `last_seen_at` (`Store::web_session_seen`),
but only when none was recorded or the last one is 10 minutes old (`SEEN_EVERY`), so a page full of
requests writes once. The account page shows it.

`POST /logout` (CSRF-checked) deletes the session and the cookie. A proxy login has nothing to end,
so the sidebar shows no logout button for it.

### The trusted-proxy listener

The identity header is honored only on a request marked `TrustedProxy`, a request extension only the
proxy listener's layer adds, and a client can't forge an extension. The main listener never reads
the header, so any local process (network-enabled plugins included) gets nothing by sending it
there.

- `WebApp::bind_proxy(&DataRoot) -> Result<Option<ProxyListener>, ProxyError>`: for the Unix socket,
  `check_socket_dir`, then `DataRoot::proxy_socket()` (`run/proxy.sock` or the `$TMPDIR` fallback).
  An existing socket that accepts a connection is `InUse` (another Doozers); a stale one is removed;
  anything that isn't a socket is `NotASocket` and left alone. The new socket is chmod 0600 (the
  directory is 0700 already). For TCP, a plain bind. `doozers serve` binds it before
  `start_sessions`, so a failure stops startup, and logs `trusted-proxy listener open at=<where>`.
- `WebApp::proxy_app(app)` wraps the main listener's whole application: the Unix socket marks every
  request; TCP first compares the SHA-256 of the secret header with the configured secret's and
  answers a bare 403 (logged) on a mismatch. `ProxyListener::serve(app, shutdown)` serves it like
  `http::serve` and removes the socket file afterwards. `serve` runs both servers in one task that
  ends at the first failure.
- With the mark, a request carrying the identity header is that identity: if
  `UserDirectory::resolve("web", value)` finds an active user, a `WebUser` for that user with that
  `sender`; otherwise a 403 page, for a value no user has and for a disabled user (the value is
  logged so the operator can see it). Without the header, the cookie applies as usual.
- The identity is one an admin sets (`Users::set_proxy_identity`), verified as `admin`, and not one
  a user adds: the Identities page refuses the `web` namespace, so a user can't claim a header
  value.
- `X-Forwarded-Proto: https` marks the cookie `Secure` only with the mark.

### Passwords

PLAN section 3.4. A user with a password signs in at `/login` with their username and that password.
Hashing and the rules are in `password.rs`, the pages in `password_pages.rs`, and the hash is in
`user_passwords` ([User passwords](storage.md#user-passwords)), never in the secret store.

**Rules.** `password::check_new` follows NIST SP 800-63B-4 for a memorized secret used alone: at
least 15 and at most 128 characters, and no composition rules, hints, or expiry. A character is a
Unicode scalar value of the password after NFKC normalization, which also applies when a password is
checked, so one typed on another keyboard or operating system still matches; any character may be in
it, spaces included. It is also refused if it is one character repeated, or contains the name
Doozers or, from 4 characters on, the username: the guesses an attacker tries first. There is no
list of leaked passwords. A password under 15 characters can't be on such a list in the first place,
and the list would be a large file to ship.

**Hashing.** Argon2id (the `argon2` crate) with the OWASP Password Storage Cheat Sheet's first
choice, 19 MiB of memory, 2 iterations, and 1 lane, a random 16-byte salt, and a 32-byte output,
stored as a PHC string (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`). That costs about 20 ms and
19 MiB, so `Passwords` runs every hash and check on a blocking thread, and a semaphore lets 4 run at
once (76 MiB at most), because anyone can start a check by posting the login form. A request waits
up to 5 seconds for a place and then gets a 503 page and `Retry-After`, and its attempt isn't
counted against the limits. A check uses the parameters in the stored string, so changing them later
leaves every existing hash working.

**Signing in.** `POST /login/password` (`username`, `password`, `nonce`):

1. `check_nonce`, as for every sign-in POST. A mismatch is a 403 before anything else is read.
2. The limits ([Rate limits](#rate-limits)): over one, a 429 login page with the wait and
   `Retry-After`, with nothing hashed.
3. The user is found by username, compared without regard to ASCII case, and must be able to sign in
   (active, with the `web:<id>` identity): a disabled user is no one here. The hash is read from the
   store for every attempt, for a fixed id that no user has when nobody matched, and the result is
   dropped, so the read doesn't tell a user from no user either.
4. The password is checked against that user's hash, or, for no user, a user without a password, or
   a user who can't sign in, against `DUMMY_HASH`: a constant with the same parameters, so the work
   is the same, and the answer is `false` whatever the result. A password longer than 128 characters
   is checked as an empty one for the same reason. The tests assert that the dummy path runs
   (`Passwords` counts checks in tests) and that the dummy hash has the current parameters, not that
   two requests take the same wall-clock time.
5. Every failure is the same 401 login page with "Wrong username or password.", with the username
   put back in the form (escaped) and never the password. Success goes through
   `login::start_session` with the method `password`, which creates a new session token and deletes
   the browser's old cookie session, so a session that existed before the login isn't the one after
   it, and answers 303 to `/` and deletes the nonce cookie.

**Setting a password.** The setup page lists "Password" (or "Change your password" for a user who
has one) for a login that may change sign-in methods
([Step-up re-verification](#step-up-re-verification)). `GET` and `POST /setup/password` answer 403
with a page that links to the confirmation for any other login (a proxy login, which can't confirm,
gets the reason). Changing a password needs no separate field for the current one: the confirmation,
with the current password or another method, is what proves it's the user. The form shows the
username (read-only, so a password manager files the password under it), two new-password fields,
and the rules. `POST` checks CSRF, then the two fields match, then `check_new`, hashes, and replaces
the stored hash with `Store::user_password_set`; changing the password doesn't end other sessions.

**Plain HTTP.** Passwords sent over plain HTTP can be read by anyone on the network. The login form,
the failed sign-in's page, and the page that sets a password carry a warning when
`WebApp::plain_http` says the request is plain HTTP (`is_https` says no: the public URL isn't
`https`, and the trusted proxy didn't say `X-Forwarded-Proto: https`) and its host isn't this
machine: the `Host` header, or the host of the link base when there is none, isn't `localhost`, a
name under it, or a loopback IP address. Those are the pages a browser treats as secure anyway. The
warning doesn't stop anyone from signing in: on an insecure origin a password is the only choice.

### Passkeys

PLAN section 3.4. A passkey is a WebAuthn credential, made and checked with the `webauthn-rs` crate
(feature `conditional-ui`, which the discoverable sign-in needs, and no `danger-` feature).
`passkey.rs` has what needs no request, `passkey_pages.rs` the setup page and the registration, and
`passkey_signin.rs` the sign-in. The credentials are in `user_passkeys`
([User passkeys](storage.md#user-passkeys)).

**Availability.** A passkey belongs to a relying party id (RP ID) and to nothing else, so changing
the RP ID breaks every passkey. The RP ID is the host of the channel's `public_url`, and the origin
a ceremony is checked against is that URL, so a browser at another address fails the origin check.
`RelyingParty::new` therefore offers passkeys only when `public_url` is set, its host is a domain
name (WebAuthn has no RP ID for an IP address, `127.0.0.1` included), and the origin is secure:
`https`, or `http` at `localhost` or a name under it, which a browser treats as secure. Otherwise
the reason is an `Unavailable` value, and its text is what the setup page says (it names
`public_url` and what to set it to). The setup page still lists "Passkey", with "Not available here"
as its sentence, so a user who is told to add one can find out why they can't. There is no fallback:
a `public_url` that is unset means no passkeys, even where the request came over HTTPS, because
nothing else says what the RP ID should be.

**Registration.** Only a login that may change sign-in methods may add a passkey, as for a password
and for the same reason (`WebApp::may_change_sign_in_methods`,
[Step-up re-verification](#step-up-re-verification)). `passkey.js` makes two requests, both JSON,
both CSRF-checked with the `X-CSRF-Token` header:

1. `POST /setup/passkey/start` `{kind: "discoverable" | "non_resident"}` calls
   `start_passkey_registration` for the user: user verification required, no attestation, the user
   handle (see User handles below) as the user id, the username and display name, and every
   credential the user has already in `excludeCredentials`, so an authenticator that has one isn't
   asked for a second. It keeps the ceremony state and answers `{ceremony, options}`.
2. `POST /setup/passkey/finish` `{ceremony, name, credential}` takes the ceremony out, calls
   `finish_passkey_registration`, and stores the `Passkey` serialized as JSON with its credential
   id, `kind`, and a name (what the user typed, trimmed, without control characters, at most 64
   characters, or "Passkey 2", "Non-resident key 3" and so on). A credential id that exists already
   is a 409. The page then shows the saved passkey.

**The override.** The setup page has two buttons. "Add a passkey" registers a discoverable
credential, which the authenticator keeps, so the browser can offer it without a username. "Add a
non-resident key" registers one it doesn't keep, for a hardware key with few slots for passkeys: a
CTAP 2.0 key can't delete one passkey without a factory reset that wipes every credential on it.
`start_passkey_registration` always builds `residentKey: "discouraged"` and
`requireResidentKey: false`. For the first button `passkey_pages::require_discoverable` sets
`authenticator_selection.resident_key = Some(Required)` and `require_resident_key = true` on the
`CreationChallengeResponse` before it is sent, and fails (500, logged) if the library built no
authenticator selection to change. `webauthn-rs-core` ignores both fields when it verifies a
registration (`require_resident_key: _` in `register_credential`), so nothing in the library notices
whether the override is there. The tests do: the software authenticator keeps a credential
discoverable (with a user handle) only if the options asked for one, so after a registration through
the first button its store must hold a discoverable credential, and after one through the second it
must not, and dropping the override fails them. The sign-in tests find that registration through a
sign-in that names no user, which only a discoverable credential allows. A library upgrade that
begins checking `residentKey` against a response fails the same tests. The one check the server
makes itself is the browser's own report: a registration that asked for a discoverable credential
and whose `credProps.rk` says `false` is refused with a pointer to the other button, and nothing is
stored.

**Ceremonies.** The state between the two requests stays in memory (`Ceremonies`, a `Pending` table
from `pending.rs` that any state of this kind can share): the library's serialized form is behind a
`danger-` feature. Each ceremony has a random id (128 bits), is kept for 6 minutes (the browser is
told 5), and finishes once: `finish` removes the entry whether or not the caller matches it. An
entry is bound to a string the finishing request must repeat, and for a registration that is the
user and the CSRF token of their login, so no other login can finish it. At most 10,000 are kept; at
the cap the expired go first, then the oldest. A restart forgets them, and the page says to try
again.

**User handles.** A discoverable passkey tells the server who it is for by a user handle.
`passkey::user_handle` is 16 bytes of a SHA-256 of the user's id, as a UUID: the same for every
passkey of a user, as authenticators expect, and it needs no column.

**Signing in.** The login page ([The login page](#the-login-page)) offers passkeys when the relying
party is available. A sign-in is two requests with state between them, and it can start two ways:

1. Discoverable, with no username. `POST /login/passkey/discover` calls
   `start_discoverable_authentication` and answers `{ceremony, options}`. The options carry
   `mediation: "conditional"`. The page uses them twice: on load, as a conditional request, so the
   browser offers the user's passkeys in the autofill of the username field
   (`autocomplete="username webauthn"`), and behind the **Sign in with a passkey** button, which
   sends the same request with `mediation` removed so the browser opens its own dialog. A browser
   allows one WebAuthn request at a time, so the script aborts the pending conditional request
   before it starts another, and starts it again if the other one doesn't sign in. It replaces the
   conditional request every 5 minutes, before the server forgets its ceremony. When the server
   refuses the answer of the conditional request (a passkey whose counter went backwards, say), the
   script waits before the next one, 3 seconds and doubling up to 30, and after 3 refusals in a row
   it stops until the user presses a button. Without that, the browser would offer the refused
   passkey again at once, as fast as the server answers.
2. By username, for a non-resident key, which the browser can use only if the server names it.
   `POST /login/passkey/username` `{username}` answers `{ceremony, options}` with the user's
   credentials in `allowCredentials` (`start_passkey_authentication`).

`POST /login/passkey/finish` `{nonce, ceremony, credential}` ends both. It takes the ceremony out
and, for a discoverable one, identifies the user: `identify_discoverable_authentication` gives the
user handle and credential id, the credential is looked up by id in `user_passkeys`, and the handle
must be the owner's ([User handles](#passkeys)), because the library doesn't check that. Then
`finish_discoverable_authentication_v2` (with the one stored credential) or
`finish_passkey_authentication` checks the signature, the challenge, the origin, the RP ID hash, and
user verification. A by-username ceremony also requires that the credential belongs to the user it
was started for, and that a user handle in the answer, if there is one, is theirs. The owner must be
able to sign in (active, with `web:<id>`). A success stores the credential again with its counter
and backup flags updated (`Passkey::update_credential`) and `last_used_at`, calls
`login::begin_session` with the method `passkey`, so a new session replaces the browser's old cookie
session, and answers `{"redirect": "/"}` with the session cookie and a deleting nonce cookie. Every
refusal is one 401 with one message, so the answer doesn't say what failed: an unknown or foreign
credential, a bad signature, a user handle that isn't the owner's, a user who can't sign in, a
ceremony of made-up ids. A ceremony that is gone (expired, used, or another browser's) is a 400 that
says to try again.

**Signature counters.** `webauthn-rs` refuses an assertion whose counter is not greater than the
stored one when either is nonzero (`CredentialPossibleCompromise`): the authenticator may have been
cloned. Doozers refuses the sign-in like any other failure, logs a warning naming the user and the
passkey, and leaves the passkey as it is, neither deleting it nor changing its counter: a genuine
sign-in whose counter has passed the stored one works again, and the user decides what to do with a
key that may have been copied. Passkeys that don't count (most synced ones send 0) are never refused
for it.

**The username step doesn't say who exists.** A name that isn't a user, a user with no passkeys, and
a user who can't sign in all get an answer made of credential ids that
`webauthn_rs::fake::WebauthnFakeCredentialGenerator` makes up, with `FakePasskeyDistribution`, which
has the distribution of credentials real accounts have. The ids come from an HMAC of the username
(trimmed and ASCII-lowercased, as the lookup compares it, and not cut: a longer name must not share
ids with a shorter one) under a 32-byte key stored in `passkey_fake_key`, made at first use, so a
name gets the same ids every time, and a restart doesn't change them. The made-up options are the
discoverable request's with `allowCredentials` replaced, so that every other field is what a real
answer has, and a real user's answer has its transports removed, since a made-up id has none to
give. The passkeys are read from the store for every name, a user or not. A name the generator gives
no ids still gets a full answer, which a browser treats as a request for any discoverable passkey.

This makes guessing usernames harder. It doesn't hide which usernames exist from a prober who asks
about many of them, and it isn't meant to. Made-up ids are sized from the library's list of
authenticator types (16, 20, 32, 64, or 65 bytes), which needn't match the authenticators users
have: a Chrome passkey id is 32 bytes, and a made-up answer with one credential never has that
length. A real user's number of credentials needn't follow the library's distribution (a real user
with a passkey has at least one, and about 16% of made-up names get none). Comparing lengths and
counts over many names shows the difference. The decision to use `webauthn_rs::fake` stands, since
the alternative, an answer that says "no such user", is certain to reveal it. Finishing a made-up
ceremony (`Ceremony::Fake`) fails like a wrong passkey. The tests compare the shape of each kind of
answer and check that the ids are stable across case, repeated requests, and a new `WebApp` on the
same database.

**Nonce and limits.** The sign-in POSTs carry the sign-in nonce ([Sign-in CSRF](#sign-in-csrf)) in
their JSON, the login page puts it in `data-nonce`, and `check_nonce` runs first, on the starts too:
a start keeps state in memory. A ceremony is bound to the nonce of the browser that started it.
Starts are limited to 60 per client address per 15 minutes ([Rate limits](#rate-limits)), counted by
a second `LoginLimits` with no limit per name. A passkey sign-in can't be guessed, so the limit only
keeps one client from filling the ceremony table, and a finish needs a ceremony a start made. A
loopback peer of the main listener has no limit, as for passwords.

**The login page.** When passkeys are available `password_collapsed()` is true, and the page has the
`#passkey-signin` section above the password form, which sits in `<details>` as "Use a password
instead" (see [The login page](#the-login-page)). The section is `hidden` and `passkey.js` shows it,
so without script a visitor sees only the closed "Use a password instead" row and opens it (the
script opens the `<details>` itself where WebAuthn is missing or the page isn't at the relying
party's origin). Where passkeys aren't available there is no passkey markup and no script, and the
password form is shown as it was.

**Script.** `assets/passkey.js` is the one first-party script that has to run: WebAuthn can be
called only from script ([Web interface design](web-ui.md#principles)). It decodes the server's
base64url options into buffers, calls `navigator.credentials.create` or `get`, and sends back only
the fields the server reads (`id`, `rawId`, `type`, the response's fields, the transports, and
`credProps`), as base64url. Where the browser has no WebAuthn or isn't at the RP's origin
(`data-origin`), the passkey controls stay hidden and the page says why. The CSP needed no change
(`script-src 'self'`, no inline script; [HTTP server](http.md)).

### OpenID Connect

An admin adds identity providers on the [admin pages](#identity-provider-pages) (they are rows of
`oidc_providers`: [OpenID Connect providers](storage.md#openid-connect-providers) holds what is
stored), and each user connects one account at a provider to their Doozers user. A sign-in is the
authorization code flow with PKCE (S256), `state`, and `nonce`. Code: `oidc.rs` is the protocol and
`oidc_pages.rs` the routes and the entries on the login and setup pages; `pending.rs` holds the
sign-ins in flight.

**The library.** The protocol is the [`openidconnect`](https://docs.rs/openidconnect) crate: it
reads the discovery document and compares its issuer, builds the authorization request, exchanges
the code, and verifies the ID token (signature, `iss`, `aud`, `exp`, `nonce`). The alternative was
to build on the OAuth code of the MCP toolsets and write a JWT and JWK verifier. A verifier is the
part of a sign-in that goes wrong in ways nobody sees until they are attacked (algorithm confusion,
key selection by `kid`, audience rules), and a maintained crate with its own tests carries less
security-critical code of ours than a new one would. What Doozers writes is small: an HTTP adapter,
the checks listed below, and the flow state. The crate costs a few dependencies that the build
already had (`oauth2`, `rsa`, `p256`, `p384`, `ed25519-dalek`, `chrono`), and none of its `reqwest`
support, which would add a second HTTP client: it is built without the `reqwest` feature, and
`oidc::Http` implements its `AsyncHttpClient` over the client of [`net`](http.md).

**Requests.** Everything goes through `net::tls_client_builder` (the bundled roots, no redirects)
with a 15-second timeout and a 1 MiB limit on the body. The issuer must be an `https` URL without a
query or fragment, or `http` on a loopback host, which is for trying Doozers out. For an `https`
issuer, the adapter refuses every request that isn't `https`, and discovery refuses a document with
an authorization endpoint, token endpoint, or key set that isn't. Doozers doesn't limit the
addresses a provider's endpoints resolve to, because a provider on a private network is a normal
setup, and the admin who adds it is the one who trusts it.

**Discovery.** `Oidc::discover` fetches `<issuer>/.well-known/openid-configuration`. The document's
`issuer` must equal the configured one exactly, so a document served for another issuer is refused,
and the provider must have a token endpoint, support the `code` response type and (when it lists
methods at all) PKCE with S256, and list a signing algorithm Doozers accepts. The result and the
provider's keys are kept in memory for an hour, by issuer. The admin pages fetch a fresh one each
time they save a provider (`Oidc::fetch`), so a provider that can't be used is refused with the
reason before it is stored. The token endpoint's authentication is HTTP Basic, the specification's
default, unless the document lists only `client_secret_post`.

**ID token.** `Oidc::complete` exchanges the code with the PKCE verifier and the client secret, and
verifies the ID token with the library's public-client verifier:

- Only RSA (`RS*`, `PS*`), `ES256`, `ES384`, and `EdDSA` are allowed. HMAC is refused, because the
  client secret isn't a signing key and a provider with no secret would hand the key to anyone, and
  the library refuses `alg: none` whatever it is told.
- The signature is checked against the provider's keys. The `kid` and `alg` of the header choose
  among them only as far as the library allows, and a key that doesn't fit the algorithm doesn't
  count.
- `iss` must equal the configured issuer, and `aud` must contain the client id and nothing else (a
  token with another audience is refused: no other audience is trusted). `azp`, when it is there,
  must be the client id, and is required if there were several audiences.
- `exp` is judged against the time of the sign-in minus a 60-second skew, and `iat` may not be more
  than 60 seconds ahead.
- `nonce` must be the one of this sign-in (compared as digests), and a token with none is refused.
- The subject must not be empty.

A callback that carries `iss` (RFC 9207) must carry the configured issuer.

**Keys.** A token whose `kid` isn't in the cached key set refreshes the set, and is checked again.
The refresh happens at most once a minute for a provider, so a stream of forged `kid`s can't make
Doozers hit the provider, and a provider that rotates keys needs no restart. It is made under a lock
and remembers when the keys the token was checked against were fetched. Of several tokens signed
with the same new key, the first fetches, and the others find the keys already fetched and check
again, instead of being refused because the fetch was a moment ago.

**The flow.** Both ways in start with a `GET` that only redirects, so the page's
`form-action 'self'` doesn't stop it, and end at the one callback of the provider,
`<public_url>/auth/oidc/<id>/callback` (the address links use when there is no `public_url`):

- `GET /login/oidc/<id>?nonce=` signs in. The login page's link carries the page's sign-in nonce,
  which must match the browser's `SameSite=Strict` nonce cookie (`auth::check_nonce`), checked
  before the start is counted or anything is kept. Another site can send a visitor to a start
  address, but not with that cookie, so it can't use up the visitor's limit on starts (and that of
  everyone behind the same address) or fill the table of sign-ins in flight.
- `GET /setup/oidc/<id>?csrf=` connects an account to the user who is logged in. The setup page's
  link carries the CSRF token of the login, and the login must be allowed to change sign-in methods
  (`may_change_sign_in_methods`), the same rule as for a password or a passkey.
- `GET /account/reverify/oidc/<id>?csrf=&next=` confirms it's the user, with the same checks of the
  CSRF token and the cookie login, and without needing to be allowed to change anything. The flow
  has the purpose `Reverify` (the user, the CSRF token, and the page to go on to). The callback
  finishes it only in the login that began it, and only if the account that signed in is connected
  to that user (`user_oidc_user`): another user's account, or one nobody connected, is refused with
  a page, though the provider vouches for it just as well.

A start discovers the provider, makes a `Flow` (the nonce, the PKCE verifier, the provider, its
issuer then, and whether it is a sign-in or a connection by which user), keeps it in a `Pending`
table for 10 minutes under 128 random bits, which is the `state`, and sends the browser to the
authorization endpoint with the scopes (`openid` and the provider's). At most 10,000 are kept.
Starts are limited like passkey starts: 60 per 15 minutes per client address. The flow is bound to
the browser by the `doozers_oidc` cookie (a random 128-bit value, reused if the browser has one, so
a second tab, or a restart of the sign-in, still works): `HttpOnly`, the whole site (the start
routes read it too), 10 minutes, `Secure` over HTTPS, and `SameSite=Lax`, not `Strict`, because the
provider's redirect back is a cross-site navigation, which a `Strict` cookie doesn't ride. That is
what stops a callback URL from an attacker's own sign-in from finishing in the victim's browser
(login CSRF); the sign-in nonce can't do it, because it is `Strict`. A finished sign-in leaves the
cookie, which says which browser and nothing more, so another tab's sign-in is still bound to it.

The callback takes the flow out of the table first, whatever else is wrong: the `state` must exist,
be unexpired, and have been bound to this browser's cookie, and it is gone afterwards, so a
replayed, forged, late, or foreign callback is refused before the code is used, and a guess costs
the entry it guessed. The flow's provider must be the one in the path, and the provider must still
exist and be enabled. A connection must also come back to the login that began it (the same user and
CSRF token). Then the client secret is read from the secret store (the name in
`oidc_providers.client_secret`), the code is exchanged, and the token is verified.

**Sign-in.** The user is whoever connected the account, found by (provider, issuer, subject) and
nothing else: no email address, name, or username is read. An account nobody connected, a user who
is disabled, and a user who can't sign in to the web chat get the same 403, so the page says nothing
about who has an account, and no account is ever created by a sign-in. The user gets a 30-day
session with method `oidc` through `login::start_session`, which logs out the browser's old session,
so a login can't be fixed in advance. The setup page doesn't offer to connect an account from such a
session.

**Connect.** `Store::user_oidc_connect` is the core operation: it connects the account to the user,
replacing the user's account at that provider, and refuses an account that is another user's. The
setup page uses it now, and the account page will use it for a user who is already logged in.

**Failures.** The pages that refuse say little: the provider's `error` is logged, and the user is
told it didn't sign them in. A provider that can't be reached when a sign-in starts, or whose client
secret can't be read, gets a 502 and a line in the log. Nothing in a URL, a page, or a log line
holds a code, a token, or a secret.

### Step-up re-verification

A 30-day cookie that was stolen must not be enough to take over an account's way in. So adding,
changing, or removing a sign-in method (a password, a passkey, a connection to an identity provider)
is allowed from a session only when `WebApp::sign_in_change` says so (`signin.rs`):

- `Link`: the session's `method` is `invite` or `recovery`, and it began less than `LINK_WINDOW` (30
  minutes) ago. A user who signed in with a link has no other way to prove who they are, so the link
  is enough to set up with. The window is short because the session lasts 30 days: a link's session
  that was stolen gets no more than any other session after it. After it, such a session confirms
  like any other, and a user with nothing to confirm with asks for a new link.
- `Reverified`: the user proved who they are again, in that session, with one of their own methods
  in the last `STEP_UP_TTL` (10 minutes). The time is `web_sessions.reverified_at`
  ([Web sessions](storage.md#web-sessions)), written by the server when a check succeeds and never
  taken from the client.
- Otherwise `Refused`, and so is a proxy login, which has no session to store it on.

A refused request gets a 403 page, "Confirm it's you first", with a link to
`/account/reverify?next=<page>` (a JSON answer with the same words for `passkey.js`). The page,
`reverify.rs`, offers only the user's own methods that can sign them in on this install: a password
form if they have a password, a passkey button if they have a passkey and the relying party works,
and a link for each enabled provider with a connection of theirs whose issuer is still the
provider's. A user with none is told to ask for a recovery link.

- **Password.** `POST /account/reverify/password` checks the user's own password with the same
  `Passwords::verify` as login, a user with no password against the dummy hash. It counts against
  `LoginLimits` under the user's username and the client's address, the same counters as
  `POST /login/password`, so holding a stolen cookie is no way around the limits on guessing. Over a
  limit is a 429 with `Retry-After` and nothing hashed.
- **Passkey.** `passkey.js` posts to `/account/reverify/passkey/start`, which calls
  `start_passkey_authentication` with the user's own readable passkeys and keeps the state in the
  ceremony table as `Ceremony::Reverify`, bound to the user and the CSRF token of the login. The
  finish takes the ceremony (once; another login can't), lets the library check the answer, requires
  the credential to be one of the user's own, records its use as a sign-in does (`record_use`), and
  confirms. Every failure is one 401 message.
- **Identity provider.** See [OpenID Connect](#openid-connect). The start asks the provider for
  `max_age=0`, so it authenticates the user again even if its own session is still there, and the
  callback refuses an ID token whose `auth_time` is missing or more than 60 seconds before the flow
  began (the leeway is for a provider clock that differs from ours).

Each success calls `WebApp::reverified`, which sets the time on the session's own row. Another
session of the same user has none. `next` is one of four pages (`/account`, `/setup`,
`/setup/password`, `/setup/passkey`) and anything else means `/account`, so it can't send the
browser elsewhere.

### Rate limits

PLAN section 3.4 rate-limits sign-ins that can be guessed. Links and session tokens carry 256 random
bits, so guessing is hopeless at any rate, and a limit on them would only let a local process lock
the owner out, so only passwords are limited. `limits.rs` counts password attempts in memory (a
restart forgets them, and each process counts for itself):

| Limit | Attempts per 15 minutes | Counts |
| ----- | ----------------------- | ------ |
| Username | 10 | The username as typed, ASCII-lowercased and cut to 64 characters, whether or not the user exists. |
| Client address | 30 | Every username from one address. An IPv6 address counts by its /64. A loopback peer of the main listener has none. |

`LoginLimits::begin` counts an attempt before the password is checked, so attempts that arrive
together can't all take the last place, and refuses it, without counting, when either limit is used
up: the window starts at the first attempt after the last one ended and never moves, so refused
attempts don't extend a lockout. The wait it reports is the longer of the two. A successful sign-in
forgets the username's count and gives the address its attempt back; an attempt that never checked a
password (the server was busy or failed) is given back to both. Each map holds at most 10,000 keys.
At the cap, windows that ended go first, then the entry with the fewest attempts, and never one that
is at its limit, so sending 10,000 fresh names can't push a locked username out. If every entry is
at its limit, a new key is refused like any other limited attempt until a window ends. Memory is
bounded however many names someone tries. The first refusal after a key reaches its limit is logged
as a warning, and the rest as debug lines, so a flood can't fill the log.

While a username is over its limit, not even its right password signs in. A limit that doesn't
depend on guessing right has that cost: whoever knows a username can lock it for the rest of the
window, although the address limit means that costs them more attempts than it costs the user, who
can still use an invite or recovery link.

The client is `limits::client_key`. On the main listener it is the IP address of the connection's
peer (`ConnectInfo<PeerAddr>`, [HTTP server](http.md)), and an `X-Forwarded-For` header is never
read there: any local process can connect, and a client could use the header to count as someone
else. A loopback peer there (`ClientKey::Loopback`) is a reverse proxy on this machine, such as one
in front of a loopback `core.http_listen`, and stands for every visitor, so it has no address limit:
counting by it would let anyone lock everyone out. Only the username limit applies to it. The
trusted-proxy listener is the way to get address limits behind a proxy. On the trusted-proxy
listener the peer is the proxy, so, as with the identity header and `X-Forwarded-Proto`, the proxy
is believed: the client is the last address in `X-Forwarded-For`, the one the proxy added (what
comes before it is whatever the client sent). A proxy that doesn't send the header, or a request
with no peer address, counts as one shared client, so a proxy has to forward addresses for the limit
to tell its clients apart.

## Inbound: events and acks

`WebChannel::post(user, chat, text, now)` builds a `ChannelEvent::Message` (event id `web-` plus 128
random bits, also used as the `message_id`), queues it, and shows the message on the chat's open
pages. The queue is in memory:

- An event stays pending until `ack`ed. `events()` returns a new stream that yields every pending
  event first, then new ones; opening a stream ends the previous one (its sender is dropped). So
  when the session manager reopens a stream after a database error, the unacked events come again,
  which is the at-least-once contract of PLAN section 8.5. Event ids are unique, so the inbox's
  dedupe makes redelivery harmless.
- The first pending event is a `Status { Connected }`, so the channel's status is recorded
  (`Sessions::channel_status`) like a plugin's.
- At most `MAX_PENDING` (256) events wait; `post` then returns `Busy`, and the form shows "too many
  messages are waiting" (503 for a plain post). A restart loses pending events, but only ones the
  core never stored, i.e. posts the user saw accepted while Doozers wasn't taking events.

Validation: the text must not be blank and is at most `MAX_MESSAGE_BYTES` (32 KiB). htmx posts get
the form back (empty on success, with the text and an error otherwise, as 200 because htmx swaps
only successful responses); plain posts get a 303 back to the chat, or an error page.

## Outbound and live output

Each chat has a hub (`hub.rs`): an event log every open page of the chat follows. A hub event is an
HTML fragment for the page's message list. Fragments that change an element already on the page
carry `hx-swap-oob="true"` (`render::out_of_band`), so htmx replaces the element with the same id
instead of appending.

Hubs are made on first use and dropped when idle: making a new hub first drops every hub that has no
open event stream, no live message, no unanswered prompt, no `Arc` held elsewhere, and no use
(publish, snapshot, subscription, lookup) for `HUB_IDLE` (10 minutes). So memory follows the chats
in use, not every chat since the start.

- `Channel::send(OutboundMessage)` appends a whole message: the Markdown through
  `doozers_plugin_sdk::render::html` with `is_local_image` (raw HTML escaped, remote images
  dropped), then the attachments (raster images inline from `/blobs/<id>`, anything else as a link).
  `SendTarget::Recipient` is `InvalidRequest`: the channel can't start conversations.
- `Channel::deltas()` returns the channel itself as a `DeltaSink` (the contract is in
  [Message Model and Traits](message-model.md#channel)). `start(conversation)` appends an empty live
  message ("Writing…"); `push` re-renders the whole text so far and replaces the element, at most
  every `LIVE_UPDATE_INTERVAL` (50 ms; pushes in between are coalesced, and the final text is always
  sent); `finish` replaces it with the final rendering. A writer dropped unfinished leaves its text,
  marked "Stopped before the end."
- `DeltaSink::tool_call(conversation, LiveToolCall { id, name, input })` appends tool call element
  `t-<id>` ("Running…"), where `id` is the call's result-slot row id, and records it in the hub
  (`Hub::tool_start`). The writer's `result(content, is_error)` replaces the element with the
  output; a writer dropped without one marks it "Stopped before the end."

The agent loop's output delivery (`agent_loop::output`, see [Agent loop](agent-loop.md)) streams
every assistant text block of a turn in a web chat through this hook: `start` at the block's first
non-whitespace delta, `push` for each delta, and `finish` once the response is stored. It shows each
tool call once the assistant message is stored, and its result once that's stored in the slot.
Whole-message `send` keeps working on this channel either way (notices, restart notices).

### Tool calls

`render::tool_call(element, name, input, state)` renders a call the same way live and in the
history. The name comes first. The input follows as text (`render::tool_input`): an object's fields
one per line as `key: value`, a string value as is and anything else as compact JSON, in key order.
The output is the result's text blocks joined by newlines, with trailing whitespace cut. Media
blocks show as attachments, without their references. Input and output are escaped plain text in
`<pre>`, never Markdown.

Collapsing needs no script. The element holds a `<details>` whose `<summary>` is the whole collapsed
bubble: the name and status, the input's first line (with " …" if more follow), and a hint such as
"Output, 14 lines, 1 attachment". Opened, the summary keeps only the name and status, and the body
shows the whole input, the whole output, and the media. A call with no input, output, or media has
nothing to open and renders without `<details>`. A failed result has the `error` class and
"Failed."; an empty one says "No output.". A live element replaced by its result closes again.

### Event streams and cursors

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
`Store::conversation_transcript(instance, chat, user, HISTORY_LIMIT)` (the last 200 rows across
every session of the conversation that is the user's, any agent, summarized messages included, and
the inbox rows still waiting; see [Storage](storage.md#conversations)). `render::history` shows text
blocks (joined by blank lines) and media of user and assistant messages, and drops reasoning and
provider data (PLAN section 6.9); a message left with nothing to show is skipped. After its
message's text, each `ToolUse` block shows as tool call `t-<slot row id>`, with the result from its
slot row, or "Running…" while the slot is empty. A call whose slot row isn't among the loaded rows
is left out. A user message's time prefix (`[Thu 2026-10-22 14:03 PDT] `, which the agent loop
stores for the model) is cut with `agent_loop::history::strip_time_prefix`, here and in chat titles,
and a media reference stored right before its media (`[media:<id> <name> <type>]`) is left out,
since the media itself is shown. A `send_attachment` call whose result isn't an error also shows as
an assistant message with its caption and the media it sent (looked up by id among the media of the
loaded rows). A "New session" marker separates sessions (`/new`, idle reset). Messages sent with
`send` that the agent loop doesn't store (notices) are only on pages that were open at the time. The
chat list is `Store::channel_conversations(instance, user, archived, 200)`, titled with the first
line of the first user message (80 characters at most). A chat page lists the archived chats when
its chat is archived (`Store::conversation_archived_at`), and the others otherwise.

The page is assembled so that a message or tool call is shown once, whatever happens while it loads:

1. The hub snapshot (cursor, live messages, prompts). Everything published later is replayed by the
   stream.
2. The posts the core hasn't acked (`WebChannel::unacked`).
3. The transcript and the waiting inbox rows, in one read transaction. A post acked after step 2 was
   inserted into the inbox before its ack, so it's in this read. The history leaves out tool calls
   the stream will replay (`Hub::tools_after(cursor)`). The agent loop publishes a call only after
   storing its assistant message, and a result only after storing it, so an event the snapshot
   covers is always in this read. A result published after the snapshot replaces the element again
   with the same content.
4. The user's messages not in the transcript yet (`render::waiting`): waiting rows, then unacked
   posts, without the posts the stream will replay (`Hub::posted_after(cursor)`), and without any
   the transcript already has. A started row is in the transcript if a user row has its receive time
   and its text (without the prefix); an unacked post if one has its text and was stored at or after
   it was sent. They're rendered exactly as the post showed them live (`m-<event id>`).
5. Live messages, except one the transcript already has (`render::is_stored`: an assistant row
   stored after the live message started contains its text so far): it ended and was stored after
   the snapshot, and the stream's replay of its end finds no element and changes nothing.
6. Prompts: unanswered ones with their buttons (and a `csrf` field), answered ones with what was
   sent.

A post the router drops (no route) isn't in the inbox, so after a reload it's gone, like any dropped
message.

## Archiving and deleting chats

In `chat_actions.rs`. A chat page has the buttons once its chat has something stored (transcript
rows or waiting inbox rows); before that there is nothing to archive or delete, and both posts
answer 404 "No such chat". Neither post checks the chat's agent, so a chat whose agent was removed
can still be deleted.

- **Archive** (`Store::archive_conversation`) and **Unarchive** (`Store::unarchive_conversation`)
  move the user's chat between the lists and change nothing else. Both take the user and do nothing
  for a chat of someone else's. They are part of `install`. Archiving isn't refused while the agent
  works: the turn finishes and its reply is stored in the archived chat. The chat's workspace is
  left to the sweep, which stops and deletes it like any idle one. `POST /chats/<id>/messages`
  unarchives before it posts, so writing in an archived chat brings it back. Nothing else does: a
  job's reply into an archived chat leaves it archived.
- **Delete** is installed separately, by `web.install_chat_deletion(builder, activity, workspaces)`,
  because it needs the session manager and the workspace service, which `serve` has only after the
  web app is built. `ChatActivity::is_busy(instance, chat)` is the seam; the daemon's is `Sessions`
  (`Sessions::conversation_busy`: a live session of the conversation, with any agent, that has a
  turn, a holder, or a driver about to run). The post is refused with 409 while the channel holds a
  post of the chat the core hasn't acked, or `is_busy` says so. Then `Store::delete_conversation`
  deletes the conversation in one transaction, refusing (409 too) while an inbox row of it is queued
  or started. Last, for each agent the conversation's sessions were with, `Workspaces::delete`
  deletes the workspace of the conversation and the user who deleted it (a chat is one user's) with
  `DeleteReason::ChatDeleted`. A store error there is a 500 after the chat is gone; the sweep
  deletes the leftover workspace later, since its owner has no activity. The answer is 303 to
  `/chats/archived` for a chat that was archived and to `/` otherwise.

What deleting leaves: usage rows (without their session), memories the agent saved (their
`source_session` is only recorded), and runtime jobs scheduled in the chat. Such a job's next run
opens a session in the conversation again, which brings the chat back. The chat's hub, if one is
open, is dropped at once, which ends its open streams, so whoever next posts with the id can't
replay its events.

The confirmation is a `<dialog>` on the chat page ([Confirmations](web-ui.md#confirmations)).

## Approval prompts (hook for 2.7)

`WebChannel::send_approval(chat, code, markdown) -> Result<MessageId>` shows a prompt with three
buttons in `/chats/<id>/approvals` forms: Allow (`yes`), Allow for this session (`yes!`), Deny
(`no`), each with the code in a hidden field. The prompt Markdown is rendered like any message. The
code must be 1 to 32 ASCII letters and digits (what `commands::parse` accepts), else
`InvalidRequest`. The prompt stays in the hub, so page loads show it, until it's resolved; a restart
forgets it, as it does the approval.

The POST checks that the chat is the user's (`own_chat`: 404 otherwise, before anything is posted or
shown as sent), CSRF, the answer (`yes`, `yes!`, `no`), and the code, then
`answer_approval(user, chat, code, answer, now)` posts `<answer> <code>` as an ordinary message from
the user (so the core's command parsing and pending-approval check decide what it means) and
replaces the prompt's buttons with what was sent ("Sent: yes!"), not an outcome: the prompt may have
expired. htmx gets 204; plain posts a 303.

`WebChannel::resolve_approval(chat, code, ApprovalOutcome) -> bool` retires a prompt however it
ended: every open page replaces it with the outcome (`Allowed`, `AllowedForSession`, `Denied`,
`Expired`), and page loads no longer show it. `false` if the chat doesn't show it (resolved already,
or forgotten by a restart or with an idle hub); it never makes a hub.

How the approvals code uses it: `approvals::ChannelPresenter` sends prompts for a conversation on
the web instance (`registry.web_channel()` whose `instance()` is `reply.instance()`) with
`send_approval` instead of `Reply::send`, which carries the typed-reply version of the prompt on
other channels, and calls `resolve_approval` however the approval ends (answer, typed reply,
timeout, `/stop`, or denial by a new message, PLAN section 6.4). `ApprovalOutcome` is
`approvals::ApprovalOutcome`, re-exported here ([Approvals](approvals.md)).

## Blobs

`GET /blobs/<sha256>` (PLAN section 9, 1.7's contract): the id must be 64 lowercase hex digits; the
user must own a message that refers to the blob (`BlobStore::reference_of`, below), else the answer
is the same 404 as for a blob that isn't stored; then the blob is streamed from the blob store (hash
verified at the end; a mismatch aborts the response). Headers: the stored media type (or
`application/octet-stream` if it isn't a valid header value), `Content-Length`,
`Content-Security-Policy: sandbox` (next to the base policy), `nosniff` (from the security headers),
`Content-Disposition: inline` for raster images (PNG, JPEG, GIF, WebP, AVIF, the set the renderer
keeps) and `attachment; filename*=UTF-8''<name>` for anything else, and
`Cache-Control: private, max-age=31536000, immutable` (the URL is the content hash). An HTML or SVG
blob therefore downloads, and even opened directly it runs in a sandboxed opaque origin.

Blobs are stored once per content, whoever sent them, so the blob store knows no owner and the check
is a question to the database: does a message of this user's refer to it? A reference is media in
the content of a message in one of the user's sessions (an attachment, or media in a tool result),
an attachment of an inbound event of theirs still in the inbox, or an attachment of an outbound
message for them still in the outbox. The same bytes sent by two users are each one's own. Only that
structure counts: a hash in text, in a tool call's input, or in an inbound message's other fields
doesn't, since the user or the model could write the hash of a file somebody else has and be served
it. A search for the hash finds the candidate rows, and each is parsed. An admin has no wider
access, because no admin page shows another user's media. The check runs before the blob store is
asked, so a 404 doesn't say whether the blob exists. The media type and file name in the headers are
those of the user's own reference (`reference_of` returns it), not the blob row's, which keeps the
first sender's.

## Jobs page

In `jobs.rs` (`jobs::routes`, merged into the chat pages' `WebApp::routes`), so it's behind the
authentication middleware but not `may_administer`-gated. It lists, in one table, every job that
delivers to the signed-in user, across all agents, with the agent as a column, and has the forms
that schedule, edit, disable, re-enable, and delete the user's runtime jobs. Config jobs are shown
but change only in `agent.toml` ([Scheduler](scheduler.md)).

- **Whose jobs**: the runtime jobs the user created (`Store::jobs(None, Some(user))`, every agent),
  and for the user `owner` every agent's config jobs, which run as that user and deliver to them
  (their data, not a privilege of any admin, until runtime jobs replace them). Both become a
  `JobDef` (`from_config`, `from_runtime`), so the page reads them the way the scheduler does. A
  stored spec `from_runtime` refuses, and a runtime job of an agent that isn't configured, show a
  "Doesn't run" note and no next run.
- **Times**: in the zone the job runs in (`message_timezone`), formatted like the `schedule` tool's
  `list` (`core_tools::schedule::show_time`; prompts are cut with its `short`). The next run is
  `next_run`: a one-shot job's time (it's in the table until it has run, so it's shown even once it
  has passed), or the next cron time inside the job's active hours, trying at most a week of
  every-minute times (`ACTIVE_HOURS_SEARCH`) before it says there is none. The last run is the job's
  `job_runs` row (`Store::job_last_run`, one read per job).
- **Order**: soonest next run first, then by agent id; jobs without a next run (disabled ones too)
  come last.
- **Creating**: `POST /jobs` takes `agent` (one of `agent_choices`, the agents the user may chat
  with), `repeat` (`cron` or `once`), `cron` or `at` (`YYYY-MM-DDTHH:MM` from
  `<input type="datetime-local">`, read in the agent's zone by the `schedule` tool's `parse_at`; RFC
  3339 with an offset works too), `prompt` (trimmed, CRLF made LF), and `session` (`isolated` or
  `dm`). `JobForm::spec` refuses a blank prompt, an invalid cron pattern or one that never matches
  again, and a time that isn't in the future; past `MAX_RUNTIME_JOBS_PER_USER` (20) jobs with the
  agent it's a 409. The row is a `RuntimeSpec` created by the user, as the `schedule` tool writes
  it, without a `conversation`, so it delivers to the user's default identity.
- **Changing**: every route with a job id loads it with `own_job`, which answers 404 unless the user
  created it, so another user's job is never shown or changed. An edit writes a new spec
  (`Store::job_update_spec`) that keeps the job's agent, `disabled`, and `conversation`, with
  `changed_at` now. Disable sets `disabled`; enable clears it and sets `changed_at`, and refuses
  (409) a one-shot job whose time has passed, which the scheduler would otherwise delete at once
  without running. A spec that can't be read can be replaced by an edit or deleted; disable and
  enable answer 409 for it. After each change `WebApp` calls `JobChanges::notify` (`runtime.jobs`,
  passed to `WebApp::new`), so the scheduler restarts the job's task
  ([Scheduler](scheduler.md#timing)).
- Every answer is `no-store` (a `map_response` route layer). A post without the CSRF token is a
  403. A form the page can't accept is shown again with why and what was typed. A store error is a
  logged 500.

## Admin pages

PLAN sections 8.7 and 10; code in `admin.rs` (pages) and `qr.rs` (QR codes). The admin pages are a
client of the admin service ([Admin service](admin.md)), like `doozers status`, but in-process:
`WebApp::install_admin(builder, admin: Arc<dyn AdminService>)` takes the generated
`doozers.admin.v1` server trait, and `doozers serve` passes the same `Arc<AdminService>` it serves
on the admin socket (`AdminListener::serve` takes the `Arc`). So the pages show exactly what the CLI
shows, and there's no token to handle: the socket's token keeps local processes (plugins included)
out of the service, and these routes are guarded by the web login instead. Tests pass a fake
implementation of the trait.

- Who: the routes sit behind the same authentication middleware as the chat pages, so no login is
  the 401 login page; then the `WebUser` must be an admin (`may_administer`: `WebUser::role` is
  `admin`), else a 403 page. `authenticate` sets the role from the stored users for each request (a
  cookie or proxy login), and only an active user is authenticated, so an admin who is demoted has a
  member's pages on the next request and one who is disabled is signed out. All admins are equal.
- `GET /admin` calls `Status`: version, start time, the host's sandbox, the configuration error
  (always unset in v1), every configured channel with its runtime and last status (and when it
  arrived), and a table of the plugins (instance, kind, one-word state, and for a toolset the number
  of tools it offers) linking to the plugins page.
- `GET /admin/plugins` calls `ListPlugins`: per plugin its state with details (pid, start time,
  retry time, restarts), last error, channel status, location, version, the number of tools a
  toolset offers (`tool_count`), sandbox, what the sandbox lacks on this host, and the grants it
  requested. A plugin with a known state (supervision reports it) has a **Restart** form: channel
  plugins and toolsets alike, since `serve` merges both hosts into one `PluginControl`. A failed
  toolset's state says only a restart retries it (a failed channel plugin's also names a changed
  binary).
- `POST /admin/plugins/<kind>/<instance>/restart` checks the role, then CSRF (`check_csrf`: the
  `csrf` field or the `X-CSRF-Token` header), then calls `RestartPlugin` with the kind (`channel`,
  `toolset`, or `provider`; any other is a 404) and the instance, and answers 303 to
  `/admin/plugins`. Each plugin's form names its own kind. Errors show the service's message:
  `NOT_FOUND` as 404, `FAILED_PRECONDITION` as 409, `UNAVAILABLE` as 503, `INVALID_ARGUMENT` as 400;
  anything else is logged and a generic 500.
- Every answer on the admin routes (pages, error pages, the 401 login page, redirects) is
  `Cache-Control: no-store`: a `map_response` route layer outside the authentication middleware
  applies `pages::no_store`, the helper the login routes use too. Times are UTC (`<time datetime>`
  carries RFC 3339).

A channel whose status is `NeedsAuth` gets a linking box under it: a QR code, a pair code, or a URL
(an `<a>` only for `http`/`https`, with `rel="noreferrer noopener"`; anything else, such as
`javascript:`, is shown as code), and the challenge's expiry. While any channel needs auth the page
carries `<meta http-equiv="refresh" content="5">` (`ADMIN_REFRESH_SECS`; `layout.html` has a `head`
block for it), so it follows the plugin's newest code without JavaScript; once every channel is past
`NeedsAuth` the page stops reloading.

`qr::svg(payload)` encodes the payload with the `qrcode` crate (error correction level M, no default
features: no image crate) and draws the modules itself as one `<path>` of rectangles, one per
horizontal run of dark modules, on a white square with the four-module quiet zone, so it scans on a
dark page too. The `<svg>` is inlined in the page (not an `<img>`: it's the code the page was
rendered with), and uses presentation attributes only, which the CSP allows. The payload never
appears in the markup; a payload too long for a QR code shows a note pointing at `doozers status`,
which prints it.

Escaping: every string from the admin service (plugin errors, versions, channel errors, pair codes,
URLs, locations) goes through askama's HTML escaping; the QR `<svg>` is the only `|safe` value. The
service never returns secrets: plugin errors and stderr are scrubbed by the plugin host, configs
aren't part of `Status`, and remote URLs can't carry credentials (config validation).

## Agent pages

In `agents.rs`: `web.install_agents(builder, Arc<agents::AgentManager>)`, which `serve` calls after
`install_admin` with a manager over the runtime's registry (so the chat list, reading the same
`LiveConfig`, offers a new agent at once). Like the admin pages: the same authentication middleware,
`may_administer` (admins only; a member gets 403, no login 401) on every route including the POST
(except the memory page's, see [Memory page](#memory-page)), `no-store` on every answer, and
`check_csrf` on the POST. Unlike them, they call the core directly (`doozers_core::agents`) rather
than the admin service: there are no agent RPCs yet (PLAN section 8.7).

- The list reads the running configuration: id, label (`Name (id)`), models, and links to the page
  and to `/chats/new?agent=<id>` ([Choosing an agent](#choosing-an-agent)).
- An agent's page looks the id up in the running configuration (anything else, `..` included, is a
  404) and lists its directory with `agents::list_files(&agent, &viewer)` on a blocking thread. The
  walk never follows a link: entries come from `read_dir` with `file_type()` (a link is listed
  as one), files are opened with `O_NOFOLLOW | O_NONBLOCK`, and the descriptor's path
  (`F_GETPATH` on macOS, `/proc/self/fd` on Linux, the helper `load_skill` uses) must be inside
  the canonical agent directory. A directory swapped for a link mid-walk can still be listed
  (the names and sizes below it, since `read_dir` follows it), but no contents from elsewhere
  are shown. The walk stops once it has `MAX_FILES` entries. Dot names are skipped. Caps:
  `MAX_FILES` (200) entries, `MAX_DEPTH` (8), the first `MAX_FILE_BYTES` (64 KiB) of each file,
  `MAX_TOTAL_BYTES` (1 MiB) of contents per page. Text is UTF-8 without NUL (a character split
  by the cut is dropped); anything else is listed with its size (so `memory.db` is listed by
  its size and its contents are never shown; the memories have their own page,
  [Memory page](#memory-page)). Contents are shown escaped in `<pre>`. Each entry has a
  `FileKind` (`Config`, `Soul`, `JobPrompt` from the agent's `prompt_file`s, `Skill`, `Other`)
  that orders and labels it.
- Who sees which file is one hook, `FileKind::visible_to(viewer)`, applied before a file is read or
  counted. It allows everything now: only admins can open the pages. It is where a user would be
  limited to the files that are their own.
- The form offers `AgentManager::known_models()` as a `<datalist>`: every registered provider's
  `list_models()`, 5 s each, failures logged at `debug` with secrets scrubbed; any
  `<provider>/<model>` can still be typed. Lists are split at commas and whitespace
  (`agents::split_list`).
- Tools are chosen with checkboxes (`agents::tool_picker`), shared by the create and edit forms.
  `agent.toml` keeps `tools` as a list of globs; a box stands for one entry: a core tool's name
  (`CORE_TOOLS`, with a one-line note each) or `<instance>__*` for each toolset in the running
  configuration, of `doozers.toml` or stored. A box is posted as the field `tool:<entry>` (browsers
  send only ticked boxes, so the forms read them through a flattened map). Any other entry is the
  "Other tools" text (`tools`): `split` separates the two when the form is filled in, `compose`
  joins them on save (boxes in the picker's order, then the text as written, without repeats), so a
  hand-written `github__create_*` or `memory*` survives a save. An entry in the text that covers a
  box (`glob::matches(entry, <box's entry>)`; for a toolset the probe is `<instance>__*` itself, so
  `*` covers it and `github__create_*` doesn't) shows the box ticked and disabled, and `compose`
  leaves that box out. A posted `tool:` field that no box stands for is ignored. Saving rewrites
  `tools` only if the resulting list differs from the file's.
- Environments are chosen with checkboxes too (`agents::environment_picker`), between Models and
  Tools, on both forms. A box is posted as `environment:<instance>`, read from the same flattened
  map as the tool boxes (`picked_environments`). The boxes are the instances of the running
  configuration (`Config::environments`, so a rename shows at once), each with its `description`
  escaped like any text. A name `agent.toml` lists that isn't configured gets a ticked box marked
  "not configured", so the form never hides what the file says (as the Models text shows an unknown
  provider): saving it is refused by `validate_agent` like a hand-written file, and unticking it
  removes it. The field is left out only if no instance is configured and no name is ticked. The
  posted names become `NewAgent::environments` / `AgentEdit::environments`, sorted
  (`environment_picker::compose`).
- Because the session offers the environment tools (`ENVIRONMENT_TOOLS`) only to an agent with an
  environment, the forms carry two `notice` paragraphs under the Environments boxes:
  `EnvironmentPicker::notes` decides which the server shows (an environment ticked but no
  environment tool allowed, which counts a box that "Other tools" covers; an environment tool
  allowed but no environment ticked), and the other has `hidden`. A tool box for an environment tool
  carries `data-environment-tool`, and `assets/agent-form.js` re-decides on every change of a box
  and every keystroke in "Other tools", matching the typed globs like `agent_loop::glob`. The notes
  inform; nothing blocks a save, and the runtime gate (`SessionTools::for_agent`) is unchanged.
  `CORE_TOOL_NOTES` ends the note of each environment tool with "Needs an environment.", which a
  test checks.
- The form asks for the name before the id. The id is derived from the name by
  `agents::agent_id_from_name`, a pure function: lowercase (`str::to_lowercase`), each space to `-`,
  every other character outside `a-z`, `0-9`, `-` removed. Nothing is trimmed or collapsed, ids are
  ASCII so `Café` gives `caf`, and whitespace other than a plain space is removed like a symbol. The
  result can be empty or `new`; creation refuses both as before. The Id input stays a visible,
  editable field and isn't `required`: `POST /admin/agents` with an empty (or blank) id derives it
  from the name, so a form posted without scripts, or by `curl`, works, and an invalid form comes
  back showing the id that was used.
- Live in the browser, `assets/agent-form.js` (embedded like the stylesheet, at
  `/static/agent-form-<hash>.js`, loaded `defer` through the layout's `head` block of both forms
  because the CSP is `script-src 'self'`) also keeps the environment notes current, and writes the
  same derivation into the Id as the owner types in the Name:
  `text.toLowerCase().replace(/ /g, '-').replace(/[^a-z0-9-]/g, '')`. It follows while the Id is
  empty or equals what the name derives (also on a form shown again after an error); typing in the
  Id stops it, and emptying the Id resumes it. The expression is the Rust function written in
  JavaScript (`to_lowercase` and `toLowerCase` agree on every character the filter keeps, U+212A
  KELVIN SIGN and U+0130 included); a unit test pins the expression in the file. After changing
  either, run the table in `agents/create.rs`'s tests through the script (for example with `bun` or
  `node`) by hand: this isn't automated.
- `CreateForm` (`agents.rs`) starts `soul` as `agents::DEFAULT_SOUL`, so the first `GET` shows it
  and a post with no `soul` field gets it; a `soul` that was posted empty stays empty, also when the
  form is shown again after an error.
- **The "Advanced" section.** Both forms share `admin_agent_fields.html`, which ends in a
  `<details>` holding `compaction_model`, `max_iterations`, `max_iterations_jobs`, `busy_mode`,
  `memory_cap`, `approval_timeout`, and `idle_reset` (`agents::AdvancedSettings`, posted as
  `AdvancedForm`, flattened into `CreateForm` and `EditForm`). It is rendered `open` when any of its
  fields has a value, which includes every post refused for one of them (an empty field is never
  invalid). An empty field is an unset key; `busy_mode` is a `<select>` whose first option,
  "Default", posts empty. The handlers parse the three number fields as `u32` before calling the
  manager (anything else is a 400 with `<key>: must be a number of at least 1`); the rest, and `0`,
  are checked by `Config::validate_agent` like any other key, so their messages are the startup
  ones.
- `AgentManager::create` (one at a time, under a lock): builds `agent.toml` with the TOML serializer
  (`agents::agent_toml`, unset values left out), validates it with `Config::validate_agent` (the
  startup checks), refuses `RESERVED_AGENT_IDS` (`new`, the form's address) and an id taken ignoring
  ASCII case, then writes the files into a staging directory, `agents/.<id>.creating/` (created
  exclusively, mode 0700; config loading skips dot names, and it's a sibling, so no agent's page
  lists it): `SOUL.md` then `agent.toml`, each as a 0600 temporary file that is fsynced and renamed.
  It fsyncs the staging directory, renames it to `agents/<id>/` (refused if that exists and isn't
  empty), and fsyncs `agents/`, so the agent's directory appears whole or not at all; on any failure
  the staging directory is removed. Then `LiveConfig::add_agent`; if that refuses (another creator
  took the id, ignoring case, meanwhile) the new directory is removed. A staging directory left by a
  crash breaks nothing (startup skips it) and is removed when `serve` builds its `AgentManager` at
  the next start. Of two creations with the same id one wins; the other is told the id is taken, as
  is one whose directory exists on disk. An invalid form comes back with `CreateError::form_message`
  (key and reason, never a file path): 409 for a taken id, else 400. Write failures are logged and
  answered 500. The handler stores `SOUL.md` with `\n` line breaks: browsers submit a textarea's as
  `\r\n`.

### Editing

`AgentManager::edit_form(id)` and `AgentManager::edit(&AgentEdit)` (`agents/edit.rs`), behind `GET`
and `POST /admin/agents/<agent>/edit` (`EditForm`, `edit_page`, `edit` in `agents.rs`). The id in
the path must be a configured agent, exactly (anything else, `..` included, is a 404); the form has
no id field.

- **What the form is rendered from.** The files on disk, not the running configuration: `agent.toml`
  is read with `toml_edit` for `name`, `models`, `tools`, `ask`, `environments`, `timezone`, and the
  "Advanced" keys (a key that isn't a string, list of strings, or for the number keys an integer
  that fits a `u32`, shows empty), and `SOUL.md` (CRLF shown as LF). `AgentForm::version` is the
  SHA-256 of both files (name, whether it exists, length, and bytes of each), carried in a hidden
  field. `read_text` refuses what the form must not write through: a symbolic link (the rename would
  replace the link), anything but a regular file, more than `MAX_EDIT_FILE_BYTES` (1 MiB), text that
  isn't UTF-8, and TOML that doesn't parse (`EditError::Unsupported`, a 409 page on `GET`).
- **`edited_toml`.** A pure function from the text and the edit to the new text, with `toml_edit`. A
  managed key whose value already equals the field is not touched (so an unchanged form gives the
  file back byte for byte, and the text is returned as is rather than re-serialized when nothing
  changed); a changed one gets a new value that copies the decor of the old one (the space and the
  comment after it), keeping its place; a missing one is appended to the top-level table
  (`toml_edit` prints key-values before sub-tables, so never inside `[[jobs]]`); an empty one (every
  key but `models`) is removed. The number keys are written as TOML integers. `environments` is
  compared as a set (`set_members`): the names that stay keep the file's order and new ones follow
  them, so a hand-written order survives a save that changes nothing. `models` is always written,
  even empty, so validation says "at least one model". Removing a key takes its leading comment with
  it, except at the first key of the file (a header: kept) and up to the last blank line above the
  key (standalone notes: kept); what is kept moves to the next key's decor.
- **Line ends.** `toml_edit` reads every `\r\n` as `\n`, so `edited_toml` edits a file with CRLF
  line ends as LF and writes the result back with CRLF; the rest of the file keeps its line ends.
- **`edit`** (under the manager's one lock, shared with creation and removal): reads the files and
  returns `EditError::Stale` if `version` differs; checks `SOUL.md`'s length only if it changed
  (`SoulTooLong`; a hand-written longer file can still have its settings edited); builds the text
  and validates it with `Config::validate_agent` (`Invalid`); writes only the files that changed,
  through `commit`: each to `.<name>.edit.tmp` in the agent's directory (fsynced, the mode of the
  file it replaces), then reads both files again and compares with what it started from (`Stale`
  again, narrowing the window for a hand edit to the renames), then renames `SOUL.md` and
  `agent.toml` into place and fsyncs the directory. A failed second rename puts the first file back
  from the contents read earlier (`restore`); temporary files are removed on every path. Then
  `LiveConfig::replace_agent`. The form's `\r\n` become `\n` before the comparison, so a `SOUL.md`
  the owner didn't touch isn't rewritten.
- **Failures** come back as the form with the input and the same `version` (400 for invalid values,
  409 for `Stale` and `Unsupported`; `Stale` adds a reload link), or as error pages (404, 500 for
  `Io` and `Task`, which are logged).
- **Taking effect.** `replace_agent` swaps in the validated `AgentConfig` and bumps
  `LiveConfig::subscribe`'s counter but keeps the agent's incarnation. New sessions read the new
  agent (the session prompt is assembled at a session's first turn); the agent loop's cached prompt
  of a running session is untouched, so it keeps its tools and `system` until compaction rebuilds
  `system` (PLAN section 6.3). The scheduler reads the agent again at every job run and restarts the
  agent's config job tasks only if its `[[jobs]]` changed ([Scheduler](scheduler.md#timing)), which
  also covers `[[jobs]]` the owner changed by hand before saving the form (the whole file is
  loaded). Core tools and the session manager's `AgentPolicies` read the live configuration at each
  use.

### Removing

`AgentManager::removal_preview(id)` and `AgentManager::remove(id)` (`agents/remove.rs`), behind
`GET` and `POST /admin/agents/<agent>/remove` (`remove_page`, `remove` in `agents.rs`; templates
`admin_agent_remove.html` and `admin_agent_removed.html`). The manager is built from
`AgentManagerOptions`: the registry, the store, the `MemoryDbs` (the registry of open per-agent
`memory.db` handles, `memory_dbs.rs`, built once in `Runtime::start` and shared), the `Routes`, the
scheduler's `JobChanges`, and an `AgentActivity` (the daemon's `DaemonActivity`: the session
manager's `Sessions::agent_busy` or the scheduler's `Scheduler::agent_busy`). The semantics are in
the module docs; in short, under the manager's lock:

1. `AgentActivity::is_busy`, a first look. `Sessions::agent_busy`: some session of the agent has a
   driver, that is a running turn, a holder (`Sessions::hold`, which `dm` jobs use), or queued rows;
   it reads the agent from the live session's key, or from the database for a driver that hasn't
   loaded its session yet. `Scheduler::agent_busy`: a job run of the agent is in progress. An
   isolated run's turn goes straight to the turn runner, so the session manager can't see it; the
   scheduler counts each run (`JobsRunning`) from the moment a job's time comes, before the run
   reads the agent's configuration, until it ends. Busy is `RemoveError::Busy` (a 409 page).
2. `Routes::hold_unrouted(agent)` takes the routes' change lock, reads the stored routes, and
   returns the guard only if none names the agent (`HoldError::Routed` otherwise, which the page
   lists with links `/admin/routes#route-<id>`). `Routes::add` and `update` now validate (the agent
   exists) *after* taking the same lock, so no route can be made for an agent between the check and
   its removal, and none for a removed agent afterwards.
3. `LiveConfig::remove_agent`, then `is_busy` again. From the take-out on no turn or job run can
   start (a turn reads the agent when it starts; a job run reads it after it has been counted), so
   what this second look sees is all there is. Work seen: the agent is put back with `add_agent` (a
   new incarnation; its config job tasks restart) and the removal is refused. A scheduled job whose
   time came between the take-out and its count finds no agent and its task ends for good (a runtime
   job's too, until a restart); config job tasks of a put-back agent are started again by the
   scheduler's reconcile.
4. `MemoryDbs::close(agent)`: drops the registry's handle to `agents/<agent>/memory.db`, so the last
   handle to go closes the database and, in WAL mode, the writer checkpoints and deletes `-wal` and
   `-shm`; `memory.db` then moves as one complete file. Nothing can reopen it before the move
   (opening is lazy, by `MemoryDbs::get` per use, and no turn can start for an agent that is out of
   the configuration). A caller that kept a `MemoryStore` clone would keep the file open: handles
   are fetched per use. A database that was never opened stays absent (`close` does nothing for it
   and creates no file). If the move below fails nothing needs reopening: the next `get` does it.
5. `move_aside`: creates `trash/agents/` (mode 0700; `DataRoot::trash_dir`), picks
   `<agent>-<YYYYMMDDTHHMMSSZ>` (`-1`, `-2` while the name exists), renames `agents/<agent>/` to it
   (one atomic rename inside the data root; `EXDEV` for a symlinked `agents/` on another file system
   is an `Io` error), and fsyncs both parents (a failure there is logged, since the move happened).
   `memory.db` is in the directory, so it moves with the agent: restoring
   `trash/agents/<id>-<time>/` by moving it back to `agents/<id>/` brings the memories back. If the
   move fails the agent is put back and the answer is a 500 page (logged) saying nothing changed.
   Then the route guard is dropped.
6. `Store::agent_jobs_delete` deletes the agent's runtime jobs and every `job_runs` row of the
   agent, in one transaction; `JobChanges::notify` makes the scheduler stop the runtime jobs' tasks.
   The scheduler stops the agent's config job tasks by itself, because it follows `LiveConfig`
   ([Scheduler](scheduler.md#timing)). A database failure here is reported inside
   `Removed::jobs_error` (the agent is already gone) and logged; the jobs can't run, since
   `job_task` stops for an agent that isn't configured.

Everything that looks agents up reads the running configuration, so the agent leaves the list, the
chat chooser, `/chats/new?agent=`, the pages (404), the router (`AgentPolicies::policy` is `None`,
so a chat started with that agent is dropped as `UnknownAgent`), the core tools, and the agent loop
together. A chat of the removed agent stays in the chat list (labelled by the id) and its page is a
404. Sessions, messages, summaries, usage, and inbox rows are not touched; the id is free (creation
checks the live configuration and the directory, neither of which has it), and creating it again
gives it a new incarnation, so the old sessions' cached prompts are not reused
([Agent loop](agent-loop.md)). Whether to refuse creating an id that has history was decided
against: reattaching the old chats is what the owner who recreates an agent most likely wants, and
the page says so.

`trash/` is outside `agents/`, so config loading, the agent list, and the instance-rename walks
(`agents/*/agent.toml`, `agents/*/groups/*`) never see it, and the backup archive, whose list of
trees is explicit, leaves it out ([Backups](backups.md)); a test pins each.

The new agent is usable at once because everything that looks agents up reads the running
configuration: the agent loop and core tools through `Registry::config()`, the session manager's
`AgentPolicies` and the router through the shared `LiveConfig`. Config jobs follow the live
configuration too (a created, edited, or removed agent's jobs start, change, or stop); general
reload stays deferred (D28).

### Memory page

In `memory.rs`, routed by `install_agents` (its `AgentPages` state, `agents.rs`) under the agent
pages' middleware (authentication, `no-store`). Unlike the other agent pages it is **not**
`may_administer`-gated: it shows each signed-in user what they could ask the agent to recall anyway.
Any signed-in user, member or admin, uses it; the handlers read `WebUser::user` and `WebUser::role`,
and tests sign in as a member and as a second admin.

- **The viewer** is the store's `MemoryViewer`, built by `user_viewer(&WebUser)`:
  `MemoryViewer::new(MemoryOwner::User(user.user), may_administer(user))`, the same rule as
  `memory::viewer` for a tool call: own private memories plus shared ones, writing shared ones only
  for admins. The store enforces everything (visibility, the shared-write rule, the 1,000-character
  limit, the core cap); the forms only leave out what it would refuse (no *shared* box for a member,
  no edit or forget buttons on a shared memory they can't write), and a crafted post gets the
  store's refusal. The cap is the agent's `memory_cap`.
- **The store** comes from `AgentManager::memory_dbs().get(&agent_id)` for each request and is
  dropped with it ([Removing](#removing) needs no handle left open). An agent that isn't configured,
  or whose id isn't a valid name, is a 404 before anything is opened, so such a request creates no
  file. A `GET` for a configured agent that has no `memory.db` yet creates an empty one (that is
  what `get` does) and shows an empty page.
- **The list** is `MemoryStore::list(viewer, PAGE_SIZE + 1)`, `PAGE_SIZE` (200), newest first; the
  extra row says it was cut short. With a non-blank `q` it is
  `MemoryStore::recall(viewer, q, SEARCH_LIMIT + 1)` (50) instead, the same full-text search as
  `memory_recall`: ranked by bm25, words quoted and OR-ed so FTS syntax in the box is text, English
  stemming. Above them the page shows the usage of each core scope (`memory::usage_header` with
  `memory_cap`, from `MemoryStore::core_memories`: the shared scope and the user's own).
- **Group sections** are for admins only (`may_administer`). `MemoryStore::group_owners()` lists the
  groups that own private memories (at most `MAX_GROUPS`, 20, are shown, and the page says if there
  are more); each gets a section from `list(group viewer, GROUP_FETCH)` filtered to the group's own
  private memories (the store also returns the shared ones to a group viewer, which the admin's own
  list already shows), under
  `MemoryViewer::new(MemoryOwner::Group { instance, conversation }, true)`: an admin's right to
  write shared memories. Every form in a group section carries `group_instance` and
  `group_conversation` hidden fields; `MemoryForm::viewer` builds the group viewer from them only
  after `may_administer` (a member gets 403 for any group field, existing group or not, so group ids
  can't be probed) and a well-formed group (400 otherwise, and for a half-named one). Both fields
  empty means the user's own scope. A memory is therefore reachable only through its own group:
  through the user's own scope or another group it is `UnknownMemory`, a 404.
- **Writes** are three plain `POST` forms (`add`, `edit`, `forget`) with the CSRF token in a hidden
  field (`check_csrf`, header or field, checked first, before the agent is looked up).
  `source_session` is `None` for a memory made here. The text's CRLF becomes LF. An edit always
  sends content, `core`, and `shared` (an unticked box is `Some(false)`), so un-ticking *shared* on
  a shared memory makes it the editing user's private one, as `memory_edit` does. Success is a 303
  to `/admin/agents/<agent>/memory?done=added|saved|forgotten[#memory-<id>]`; the page maps the
  fixed `done` codes to a notice and ignores any other value, so a link can't put text on the page.
- **Refusals** re-render the page (nothing was written) with a status and a message, mapped from
  `doozers_store::Error`: `UnknownMemory` 404 (the same for an id that is invisible or gone),
  `SharedWriteForbidden` 403, `MemoryTooLong` and `InvalidRequest` 400, `StaleMemory` and
  `CoreCapExceeded` 409; any other error is a logged 500. An add keeps what was typed in the form;
  an edit shows what was typed in a `<pre>`. `CoreCapExceeded` says how far over the cap the scope
  would be and lists the scope's core memories (from `CoreOverflow::memories`) so the user can trim.
  A form without a parseable id (or, for an edit, `updated`) is a 400.
- **Lost updates.** Each edit form carries the memory's `updated_at` in Unix milliseconds
  (`updated`) from when the page was rendered, and the handler sends it as
  `MemoryEdit::expected_updated_at`. The store compares it with the stored value in the same
  transaction as the write (after the visibility check, before the shared-write rule), so a change
  by the agent or by another tab between render and post is refused as `StaleMemory`; the page then
  shows the current content next to what was typed. The check works on the store's millisecond
  resolution. Forget has no guard: it is deliberate and the user sees what they forget.
- **Running sessions** keep the core memories their prompt was built with; the page says that a
  change reaches a session after it compacts or when a new session starts
  ([Agent loop](agent-loop.md)). Nothing here notifies a session.
- **Escaping.** Content, flags, the search words, and group ids are template values, so Askama
  escapes them; the `done` code is never echoed. Memory text is shown in `<pre>` and in the
  `<textarea>` of its edit form.

## Route pages

In `routes.rs`: `web.install_routes(builder, Arc<routes::Routes>)`, which `serve` calls after the
agent pages, with the runtime's `Routes` (the object whose `RouteList` the router reads). Like the
agent pages: the same authentication middleware, `may_administer` on every route including the
POSTs, `no-store` on every answer, and `check_csrf` (header or field) on every POST. Every change is
one `Routes` call, which validates, writes, and gives the router the new list before it returns
([Identity and Routing](identity-and-routing.md#stored-routes)), so the next inbound message uses
it.

- The list shows each route's position, channel, kind, conversation, and agent (a link to the
  agent's page, or plain text if it's gone), with Edit, Up, Down, and Remove. Up is left out for the
  first route and Down for the last. A route with a problem (`RouteView::problems`: never used,
  channel instance or agent not configured) has a warning row under it. A notice says how many
  `[[routes]]` `doozers.toml` still has (`Config::legacy_routes`).
- Buttons are plain `POST` forms with the CSRF token in a hidden field, so the pages work without
  scripts and removing is never a link. Routes in paths and forms are the store's stable ids
  (`RouteId`), never positions: a page opened before another change can't act on a different route;
  one for a route that's gone answers 404 "No such route". An id that isn't a number is the same
  404.
- The add and edit forms are one template. Channel, kind, and agent are `<select>`s of what exists
  now (the running configuration), plus the route's current value when it's no longer there, so
  editing a route to a vanished agent shows it. The add form also has Position: "At the end" or
  "Before route N (what it matches)". An invalid form comes back with the same values and
  `RouteError`'s message: 400 for an invalid value, 409 for a route that could never be used or
  would hide another; stale ids are 404; database failures are logged and answered 500. Messages
  hold only what the owner typed or ids of agents and instances, escaped by the template.
- A move that would hide a route is refused with the reason, as is an edit; reordering therefore
  passes only through valid lists.

## Workspaces page

In `workspaces.rs`: `web.install_workspaces(builder, Workspaces)`, which `serve` calls last, with
the runtime's `Workspaces`. Like the other admin pages: the authentication middleware,
`may_administer` on every route including the POSTs, `no-store` on every answer, and `check_csrf` on
every POST. [Environment workspaces](environments.md#calls) describes the calls behind it.

- The list is one block per `creating` or `active` workspace, newest first, not a table: the fields
  (environment, owner, created, last used, in use, disk used, SSH keys, description, working
  directory, details) don't fit a table in the page's width. **SSH keys** is the workspace's
  `key_user` (the user whose keys its agent offers, [SSH](ssh.md#how-a-workspace-gets-its-keys)), or
  `none`. The owner is the agent (a link to its page if it is configured), the channel, the
  conversation, which links to `/chats/<id>` when the channel is this web instance and the
  conversation is a chat id, and, for a direct conversation, `for <user>` (two users of one
  conversation id have workspaces of their own); a job run shows its session. Above the list, each
  environment shows its free space, or why it isn't known (not connected, failed, no longer
  configured). A workspace the plugin didn't answer for shows `unknown` for its state, disk use, and
  details, and what Doozers stored of its description. There is no timeout of its own and no cache:
  the page waits for the plugins like any call to them does.
- What a plugin reports (description, working directory, `details`) is the plugin's text: the
  template escapes it and nothing interprets it. `details` is rendered as a nested definition list
  in the plugin's order.
- Stop is a plain `POST` form, except for a workspace a turn is using (`Workspaces::in_use`), where
  the button is a link to `GET .../stop`, a confirmation that says stopping ends the running command
  and background processes. That page redirects to the list for a workspace nothing is using, so a
  stale link doesn't show a warning that no longer applies. Delete always goes through
  `GET .../delete`, which says what deleting does and adds the in-use warning (stop the command,
  then delete) when a turn is using the workspace. The `POST`s don't check for a lease again: the
  warning was the confirmation, and the calls handle either case.
- Ids in paths are workspace ids, so a page opened before another change can't act on a different
  workspace. One that is gone or isn't a valid id is 404 "There is no such workspace"; one still
  being created, or whose environment isn't connected or configured (stop only), is 409; a plugin
  that fails the call is 502; a database failure is logged and answered 500.

## Toolset pages

The web UI calls stored toolsets MCP servers: the sidebar entry, the pages, and their addresses
(`/admin/mcp-servers`). The code and the rest of the docs keep the name toolset.

In `toolsets.rs`: `web.install_toolsets(builder, manager, admin, registry)`, which `serve` calls
last, with the `Arc<ToolsetManager>`, the `Arc<dyn AdminService>` the admin socket serves, and the
runtime's `Arc<Registry>`. Like the route pages: the same authentication middleware,
`may_administer` on every route including the POSTs, `no-store` on every answer, and `check_csrf`
(header or field) on every POST. Every change is one `ToolsetManager` call, which validates the
spec, stores it, and starts, restarts, or stops the server before it returns
([Toolsets](toolsets.md)), so the next conversation has the new tool list. The agent form's tool
picker reads the same `LiveConfig` and shows the new box at once. The pages take the admin service
for what `ListPlugins` knows (state, last error, and tool count, built by the plugins page's
`plugin_row`) and the registry for the tool list, because the manager has neither.

| Route                                    | Does                                                    |
| ---------------------------------------- | ------------------------------------------------------- |
| `GET /admin/mcp-servers`                 | every toolset: name, transport, location, state, tools  |
| `GET /admin/mcp-servers/new`             | the form                                                |
| `POST /admin/mcp-servers`                | `ToolsetManager::add`, then 303 to the toolset's page   |
| `GET /admin/mcp-servers/<name>`          | the settings, the tools (name and description), buttons |
| `GET /admin/mcp-servers/<name>/edit`     | the form, filled in, with `updated_at`                  |
| `POST /admin/mcp-servers/<name>/edit`    | `ToolsetManager::update`, then 303 to the page          |
| `GET /admin/mcp-servers/<name>/remove`   | the confirmation                                        |
| `POST /admin/mcp-servers/<name>/remove`  | `ToolsetManager::remove`, then 303 to the list          |
| `GET /admin/mcp-servers/<name>/rename`   | the form; with `?new=`, `rename_preview` and the button |
| `POST /admin/mcp-servers/<name>/rename`  | `ToolsetManager::rename`, then 303 to the new page      |
| `POST /admin/mcp-servers/<name>/restart` | `RestartPlugin` (`Toolset`), then 303 to the page       |

- **The list** has one row per `ToolsetManager::list` entry, with Edit and Remove for stored
  toolsets only, which are remote servers. A toolset of `doozers.toml` (`ToolsetSource::File`), a
  stdio server, is listed with how it runs (from the running configuration), marked, and read-only,
  and a page for editing, renaming, or removing one is a 409 with the manager's `Unsupported`
  message. The state and tool count come from `ListPlugins` by instance name; a stored toolset that
  failed to start is there as `Failed` with its reason, because `serve` merges the manager into the
  admin service's `PluginControl`. Where the service has nothing, the row shows the manager's
  `failure`.
- **The toolset page** shows the settings from the stored spec (a toolset of `doozers.toml` has no
  spec to show), the state, the tools the viewer's new conversations get (`Registry::tools_for`,
  prefixed name and description), and a Restart form when the service knows the instance. The
  restart route exists next to the others so it can return to this page; `RestartPlugin` is the same
  call as on the plugins page. The route looks the name up in the manager first (any case) and
  answers 404 for a name that isn't a toolset, so it can't restart a channel or a provider.
- **The form** is one template for add and edit, and it describes a remote server only, because the
  manager stores nothing else: name, `url`, `insecure`, the bearer token (none, or the name of a
  secret in `secrets.toml`), `deadline`, `parallel`, and `skills` (one per line). The `insecure` box
  ("Allow insecure connections") applies only to a URL that starts with `http:` (ASCII case
  ignored): the page renders it `hidden` for any other URL, `mcp-server-form.js` shows and hides it
  as the owner types, and `ToolsetForm::spec` drops it for any other URL, so a box left ticked
  behind a changed URL stores nothing. The posted form is read as a map of names to text, and names
  the form doesn't have are ignored, so a post shaped like a stdio server has no `url` and the
  manager refuses it at `url`. `ToolsetForm::spec` turns the text into JSON with the keys of a
  `[toolset.<name>]` table and parses it with `ToolsetSpec::from_json`, so a value of the wrong type
  comes back with the key, as does everything the manager and `validate_toolset` find. The manager
  refuses `new` as a toolset's name, because it is the address of the form.
- **Errors** come back on the form with what was typed and `<key>: <reason>` from the manager or the
  form: 400 for a value that isn't valid, 409 for a name that is taken (`Exists`, an instance of
  `doozers.toml`) or a toolset the manager can't start as a whole, and 409 for a stale edit.
  Anything else is logged and answered 500, and a toolset that is gone is a 404.
- **Stale edits.** The edit form carries the `updated_at` the toolset had when the form was made.
  The post gives it to `ToolsetManager::update` as `expected_updated_at`. A toolset changed
  meanwhile gives `Stale`, shown as a 409 with the typed values, the old `updated_at`, and a link
  that reloads the form, so submitting the same form again can't overwrite the change.
- **Credentials.** A bearer token is the name of a secret, shown by name; nothing reads a secret's
  value. A stored spec whose token isn't a secret reference (only an older database can hold one)
  fills the form with no token, and its token is never shown.
- **Removal.** The confirmation lists the agents whose `tools` or `ask` have an entry that starts
  with `<name>__`. The manager leaves them alone, and the page says so. A stored toolset has no data
  directory, so nothing else is removed.
- **Renaming.** The form is a plain `GET` with the new name in `?new=`, so the preview needs no
  script and no state between steps: `ToolsetManager::rename_preview` runs every check of the rename
  and lists, per agent, the `tools` and `ask` globs that change, and the page shows them with a
  CSRF-checked POST form that carries the name. The POST calls `ToolsetManager::rename` (the checks
  run again under the manager's lock, so a change between the two steps is caught) and answers 303
  to the page under the new name. The Rename link is shown for stored toolsets only. A toolset of
  `doozers.toml` is the same 409 as for edit. What the manager refuses comes back on the form with
  the typed name, or on the error page:

  - On the form, 400 for a name that isn't valid and for `Refused` (a glob that would stop matching,
    or an `agent.toml` that isn't valid TOML or that the edit form can't edit, with the agents and
    globs listed by the manager).
  - On the form, 409 for a name that can't be used: another stored toolset, an instance of
    `doozers.toml`, or the toolset's own name (all `Problem::of`).
  - On the form, 500 for `Failed`, which shows the step, the error, and the undo errors if there are
    any. The preview can show it too, for an agent file that can't be read, and says nothing was
    changed.
  - On the error page, 404 for a toolset that is gone, 409 for a toolset of `doozers.toml`, and 500
    for a database error (`Store`).

  The page says that running conversations keep the old tool names.

## Connections page

In `connections.rs`: `web.install_connections(builder, manager)`, which `serve` calls before the
toolset pages. It's for every logged-in user, not only `may_administer`, and each user sees and
changes only their own sign-ins ([Each user's sign-in](toolsets.md#each-users-sign-in)). Every
answer is `no-store`.

| Route                                  | Does                                                           |
| -------------------------------------- | -------------------------------------------------------------- |
| `GET /connections`                     | the toolsets with `auth.oauth`, and whether the user is signed in |
| `GET /connections/<name>/sign-in?csrf=` | `check_csrf`, `begin_sign_in`, then 303 to the authorization server |
| `GET /connections/<name>/callback`     | `finish_sign_in` for `<name>` with `code`, `state`, and `iss`; 303 back |
| `POST /connections/<name>/sign-out`    | `check_csrf`, then `sign_out`; 303 back                        |

- **GETs that leave Doozers.** The sign-in and the callback are `GET`s, as the redirects are. The
  pages' `form-action 'self'` would stop a form post that redirects to another origin. The sign-in
  link carries the CSRF token as `?csrf=`. Otherwise another site, or a link in an agent's reply,
  could start a sign-in that a server approving silently would finish, replacing the user's tokens.
  Pages send `Referrer-Policy: no-referrer`, so the token doesn't leave with the redirect. The
  callback finishes only a sign-in the same logged-in user started, by `state`, so a forged callback
  signs no one in. The session cookie is `SameSite=Lax`, so it comes with the top-level redirect
  back.
- **The callback URL** is `WebChannel::link_base()` (`public_url`, else the listener's address) plus
  `/connections/<name>/callback`, one per toolset, with the name as stored. The toolset's page shows
  it as **Sign-in callback**, for the owner to register with the server's OAuth app. A rename
  changes it.
- **Errors.** An `error` from the authorization server is a 400 page naming it. A sign-in that can't
  start or finish is a 502 page with the reason, and a toolset that isn't there or isn't running is
  a 404.

## SSH keys pages

In `ssh_keys.rs`: `web.install_ssh_keys(builder, ssh_keys)`, which `serve` calls with
`Runtime::ssh_keys`. The pages are a thin layer over the `SshKeys` service
([SSH keys](ssh.md#permissions)). **SSH keys** is for every logged-in user, who sees and changes
only their own keys, and the admin page is for admins. Every answer is `no-store`, every `POST` is
`check_csrf`-checked, and every form works without JavaScript.

| Route                                | Does                                                              |
| ------------------------------------ | ----------------------------------------------------------------- |
| `GET /ssh-keys`                      | the user's keys with the generate and upload forms; `?notice=` names what just happened |
| `POST /ssh-keys/generate`            | `check_csrf`, then `generate` the chosen `type` with the `label`; 303 back |
| `POST /ssh-keys/upload`              | `multipart/form-data` of `csrf`, `key_text`, `key_file`, `passphrase`, and `label`; `upload` the file, else the text; 303 back |
| `GET`, `POST /ssh-keys/<id>/rename`  | the label form; `check_csrf`, then `rename`; 303 back             |
| `GET`, `POST /ssh-keys/<id>/delete`  | the confirmation; `check_csrf`, then `delete`; 303 back           |
| `GET /admin/ssh-keys`                | every user's keys: details only (`may_administer`)                |
| `GET`, `POST /admin/ssh-keys/<id>/delete` | the confirmation; `check_csrf`, then `delete_any`; 303 to the admin list |

- **No private key.** A key's card shows its public key in `authorized_keys` form
  (`<algorithm> <base64> <label>`) in a read-only field, with a copy button and the fingerprint. The
  admin page shows neither the public key nor a copy button. No route, header, or error carries a
  private key.
- **Copy button.** The button is an icon (`aria-label="Copy public key"`) attached to the field's
  right edge (`.input-group`). `assets/copy-button.js` unhides each `button[data-copy-from]` and
  copies the value of the field it names; for 1.5 s afterwards the button shows a check mark and its
  label is "Copied". Without the script the buttons stay `hidden`, and the field keeps its own
  rounded corners and is text to select. The script is loaded `defer` through the layout's `head`
  block, like the form scripts.
- **The upload.** The route allows a body of about 144 KiB (`UPLOAD_BODY_LIMIT`: twice the key's 64
  KiB, with room for the other fields), and a bigger one is a 413 before the key is read. The
  handler reads every field, then checks the CSRF token, which is a form field here, before it
  parses anything. A file wins over pasted text.
- **Errors.** A form that fails (a bad label or type, an unreadable key, a wrong passphrase, a key
  the user already has) renders the page again with the reason: 400, or 409 for a duplicate. It
  keeps the label and type that were typed, never the key text or the passphrase. A key id that
  isn't the user's, doesn't exist, or isn't a number is a 404 "No such SSH key" on every route, so
  an id of someone else's key reveals nothing. A user id that can't name a secret is a 409 page, and
  anything else is a logged 500. The admin routes answer 403 to anyone but an admin and 404 to an
  unknown id.

## Known SSH hosts page

In `ssh_hosts.rs`: `web.install_ssh_hosts(builder, known_hosts)`, which `serve` calls with
`Workspaces::known_hosts()`. The page is a thin layer over the `KnownHosts` service
([Known hosts](ssh.md#known-hosts)) and is for admins only: every route answers 403 to anyone else,
and the sidebar's **Known SSH hosts** link, under **Admin** after **SSH keys**, is shown to admins
only. Every answer is `no-store`, every `POST` is `check_csrf`-checked, and every form works without
JavaScript.

- **The list.** `KnownHosts::all` is ordered by environment, so the page makes one section for each
  environment that has a key, each with a table of its hosts. A host is written as `ssh` and
  `known_hosts` write it: the name alone on port 22, `[name]:port` on any other. **First seen** is
  the time and then who recorded the key (`by workspace W, agent A, user U`, leaving out what the
  row doesn't have) or `pinned by <admin>`. **Last used** is `never` until a lookup matches the key.
- **Delete.** **Delete** is a link to the confirmation, which names the host, its environment, and
  the key, and says that the next connection from a workspace in that environment trusts whatever
  key the host then presents once no key is left, and that the host's other keys stay. The `POST`
  deletes with `KnownHosts::delete` and redirects with `?notice=deleted`. An id that isn't a number
  or a key is a 404 on both routes.
- **Add.** The form offers the environment instances of the running configuration and one pasted
  line. The environment is checked against the configuration first, because `KnownHosts::pin`
  doesn't know which exist. With no environments configured the page says so and has no form. A line
  the service refuses (`KnownHostsError::Invalid`, with a reason such as hashed names, markers,
  wildcards, or a key type that can't be verified) or an unknown environment is a 400, and a key
  that every host of the line already has is a 409. Both render the page again with the reason, the
  pasted line, and the chosen environment. Success is a 303 to `?pinned=<ids>`. The page reads those
  ids from the store to name what was pinned (`Pinned the ssh-ed25519 key for git.example in box.`),
  so a hand-written URL can't put text on the page.

## Identity provider pages

In `oidc_admin.rs`, on the state of the sign-in routes (`OidcPages`), so
`web.install_oidc(builder, secrets)` adds both. Admins only: every route answers 403 to anyone else,
and the sidebar's **Identity providers** link, under **Admin** after **Known SSH hosts**, is shown
to admins only. Every answer is `no-store`, every `POST` is `check_csrf`-checked, and every form
works without JavaScript. Doozers has no state of its own here: the pages read and write
`oidc_providers` ([OpenID Connect providers](storage.md#openid-connect-providers)) and the secret
store.

- **The list.** Each provider with its id, client id, whether a secret is stored (never its value),
  its issuer, the redirect URI to register (`<link base>/auth/oidc/<id>/callback`), enabled or
  disabled, and how many accounts are connected with its current issuer. A banner says when the
  channel has no `public_url`, so the redirect URIs use the address Doozers listens on.
- **Adding and editing.** The form has the id (not editable once added: it is in the redirect URI),
  name, issuer, client id, client secret, scopes, and enabled. Saving first checks the settings as
  the store does (`OidcProviderSettings::validate`, the same check, so that nothing else is changed
  that the store would then refuse), and then fetches the issuer's discovery document fresh
  (`Oidc::fetch`), so a provider that can't be used is refused with the reason, as a 422, and
  nothing is stored: no connection to the issuer, a document for another issuer, no code flow or no
  PKCE with S256, no signing algorithm Doozers accepts, an issuer that isn't `https` (or `http` on
  `localhost`). A bad id, a blank field, or characters a scope can't have are a 400, and an id that
  exists is a 409. The scopes are made tidy: `openid` first, and each other scope once.
- **The client secret.** It is typed into a `password` field and never rendered back; the pages say
  only whether one is stored. A new provider's secret goes to the secret store as
  `oidc/<id>/client-secret` and its path into the row; if the store fails, the new row is taken
  back, since a provider without its secret would fail every sign-in. Editing with the field empty
  keeps the stored secret, typing one replaces it, and the box "Remove the stored secret" deletes it
  and clears the name. The row is updated before the secret is written, so a refused edit, or one of
  a provider that was removed meanwhile, writes no secret, and if the secret can't be written the
  row goes back to what it was. An edit that fails validation shows the form again with what was
  typed, except the secret. The reasons are sentences: the store's messages are capitalized and end
  with a full stop.
- **Issuer changes.** Changing a provider's issuer disconnects its accounts: connections are found
  under the provider's current issuer, so the old ones stay in the table but match nothing. The form
  says so.
- **Enable and disable.** One button on the row, a form post of `enabled=true` or `false`. It
  fetches nothing, so a provider that is down can be turned off. A disabled provider isn't on the
  login or setup page, its start routes answer 404, and a sign-in begun before it was disabled is
  refused at the callback.
- **Remove.** The confirmation says how many accounts will be forgotten, all of the provider's,
  whatever issuer each was connected under (`Store::oidc_provider_connection_count`, where the list
  counts only those that can sign in now), and that browsers logged in through the provider stay
  logged in until their sessions end: removing or disabling a provider ends no session, and
  disabling the user does. The `POST` deletes the provider and, by the foreign key, its connections,
  then its secret (a failure to delete the secret is logged and doesn't undo the removal).

Everything a provider's name, client id, or issuer says is escaped by the templates.

## User pages

In `users.rs` (the list and **Remove**) and `users_manage.rs` (everything else):
`web.install_users(builder, users, forgetter)`, which `serve` calls with `Runtime::users` and the
daemon's `Forgetter`. **Users**, in the Admin group after **Agents**, lists the users; admins only
like the other admin pages: every route answers 403 to anyone else, every answer is `no-store`,
every `POST` is `check_csrf`-checked, and every form works without JavaScript. A handler checks the
admin role first, then the CSRF token, then reads the id in the path: an id that isn't a user
(malformed or unknown) is a 404 on every route. `UserPages` holds the `WebApp`, the `Users`, and the
`Forgetter`.

| Route | Does |
| ----- | ---- |
| `GET /admin/users` | The list |
| `GET /admin/invite-user` | The invite form |
| `POST /admin/invite-user` | Creates the user and an invite link and shows it (200); the form again with why (400, 409) |
| `GET /admin/users/<id>` | The user's page, with `?notice=` after a change |
| `POST /admin/users/<id>/details` | Sets the username, display name, and time zone together, 303; the page again with why (400, 409) |
| `POST /admin/users/<id>/role` | Sets the role, 303 (to `/` for an admin who demoted themselves); 409 for the last active admin |
| `POST /admin/users/<id>/proxy-identity` | Sets or, empty, clears the proxy identity, 303; the page again with why (400, 409) |
| `POST /admin/users/<id>/disable` | Disables the user and deletes their logins, 303; 409 for the admin's own account and for the last active admin |
| `POST /admin/users/<id>/enable` | Enables the user, 303 |
| `POST /admin/users/<id>/recovery-link` | Makes a recovery link and shows it (200); 409 for a user who can't sign in |
| `GET /admin/users/<id>/remove` | What removing the user deletes, and the button |
| `POST /admin/users/<id>/remove` | Forgets the user (CSRF-checked), 303 to the list |

The invite form has its own path, `/admin/invite-user`, and not `/admin/users/invite`, so that no
user id can hide behind a fixed segment.

- **The list.** `GET /admin/users` has a row per user, from the live user list (`Users::list`): the
  username (and "(you)" for the admin looking), the display name, the role, the state, when the user
  last signed in, and **Manage** and **Remove** links. The last sign-in is read from the store
  (`Store::users`, `StoredUser::last_sign_in_at`), not from the router's snapshot: a session starts
  without replacing the snapshot, and the column is the time of the newest session any sign-in
  method started ([Storage](storage.md#web-sessions)). A user who only logs in through the trusted
  proxy has none, which the page shows as "Never". It shows `?removed=<username>` as "Removed
  <username>.".
- **Inviting.** The `POST` validates the role (`admin` or `member`), then `Users::create` with a
  fresh random id (`random_hex(16)`; the store forgets a removed user's id, and a random one is
  never one anybody had) and the username and display name trimmed (the checks are `Users`').
  `Users::create_invite(id, Invite, Some(admin), now)` adds the `web:<id>` identity and makes the
  link, and the answer shows `WebChannel::invite_url(token)`, the same URL
  `doozers admin login-link` prints, with the expiry and a copy button. The link is on that answer
  only, because the store keeps its hash. A refused form is the form again with what was typed and
  nothing created. If the user was created and the link couldn't be made, the answer is a 500 that
  says so and points to the recovery link on the user's page.
- **A user's page.** `GET /admin/users/<id>` shows the username, id, role, state, and last sign-in,
  and one form for each of the details, the role, and the proxy identity, and the buttons for a
  recovery link and **Remove**. The proxy identity is `User::proxy_identity`, the user's `web`
  identity other than `web:<id>`. A form that is refused renders the page again with the error above
  the forms and the typed values in the form that was posted (the others show what is stored), with
  the status of the refusal: 400 for a value that isn't valid, 409 for a username or identity
  another user has and for the last active admin. The details form calls `Users::set_details`, which
  checks and writes the three fields in one store transaction, so a refused edit writes none of
  them. The role form calls `Users::set_role`, and the proxy form `Users::set_proxy_identity` (the
  empty field is `None`; a value starting with `web:` is a 400, since the field is what follows it).
  The last-active-admin rule is `Users`' and the store's, so the page only reports it: the role
  form's 409 says the last admin can't be demoted. An admin who demotes themselves while another
  admin remains is sent to `/`, since the admin pages are closed to them.
- **Disabling and enabling.** `POST /admin/users/<id>/disable` refuses the signed-in admin's own id
  with a 409 that says to ask another admin, the conservative choice, since the admin who disabled
  themselves couldn't enable themselves again. Then it calls `Users::set_state(Disabled)`, which
  refuses the last active admin (`UserError::LastAdmin`, a 409 with the last-admin reason, reachable
  by a request whose admin was demoted a moment ago). Then `Store::user_logins_revoke` deletes every
  session and every unspent invite or recovery link of the user. Disabling comes first, so the user
  is locked out at once (`WebChannel::cookie_user` and the proxy path refuse a disabled user, and
  `Store::web_session_create` refuses to start a session for one, which `begin_session` answers with
  a 403 for a sign-in that was under way). A failure to delete the rows is a logged 500 that says
  the user is disabled and that enabling deletes what is left. `POST /admin/users/<id>/enable` does
  that first, for a user who is disabled (a 500 and no enabling if it fails), then
  `Users::set_state(Active)`, so enabling never brings an old login or link back. The user's open
  event streams end by themselves (`WebApp::login_ended` sees the session gone or the user
  disabled). The router already drops a disabled user's messages, the session driver drops the ones
  queued before ([Sessions](sessions.md)), and the scheduler skips runtime jobs whose creator isn't
  active ([Scheduler](scheduler.md)): the change to the users that `set_state` makes wakes it, so
  enabling needs no call of its own to start the jobs that waited. Only a turn already running isn't
  stopped. The page shows **Disable** for an active user and **Enable** for a disabled one, and
  offers no recovery link to a disabled user.
- **The recovery link.** `POST /admin/users/<id>/recovery-link` calls
  `Users::create_invite(id, Recovery, Some(admin), now)` and shows the link once, as the invite
  does. A disabled user can't have one (`LoginUserError::Disabled`, a 409 on the user's page).
- **The confirmation.** `GET /admin/users/<id>/remove` is built from `Forgetter::preview`: one
  sentence per kind of data the user has (chats, group messages, private memories, the profile,
  identities, sign-in methods, jobs, queued messages, approvals, SSH keys and secrets), the
  workspaces one by one with their environment, and what stays. The counts are the store's
  (`UserData`), so the page counts what the removal then deletes. It names why a removal would be
  refused (`Refusal`: the admin's own account, or the last active admin; or a user who is busy) and
  has no button then. An id that isn't a user is a 404.
- **The removal.** `POST /admin/users/<id>/remove` calls `Forgetter::forget` with the signed-in
  admin as the one asking, and answers 303 to `/admin/users?removed=<username>`. A refusal
  (`ForgetError::is_refusal`) shows the confirmation again, which says why once from what is stored
  now, as a 409, and nothing changed. An error after the removal began is a logged 500 that says the
  user is disabled, some of their data may be left, and the removal can be run again. A preview that
  can't be read (the `GET`, or the second preview of a refusal) is its own 500, "Couldn't read the
  user's data", that says nothing was changed. The user's web chats are read before the removal and
  their live state is dropped (`Hubs::remove`), as deleting a chat does, after a removal that
  finished or failed after deleting, so a retry needn't find them. What the removal does is in
  [Storage](storage.md#forgetting-a-user).

The role the page checks is the one the request carried, and `forget` decides again from the stored
users, which is why the last-admin refusal exists for a request whose admin was demoted a moment
ago. Everything a user's name, display name, or identity says is escaped by the templates.

## Account page

In `account.rs`: `web.install_account(builder, users)`, which `serve` calls with `Runtime::users`.
**Account**, the last link of the sidebar's "You" group, is on every page. The page is for every
logged-in user, who sees and changes only their own account: no path or field names another user.
Every answer is `no-store`, every `POST` is `check_csrf`-checked, and every form works without
JavaScript. `AccountPages` holds the `WebApp` and the `Users`, and converts to the `WebApp` with
`FromRef`, so a handler that needs only the app extracts that.

| Route                | Does                                                                       |
| -------------------- | -------------------------------------------------------------------------- |
| `GET /account`       | The profile form and a summary of the user's channel identities           |
| `POST /account/profile` | Sets the display name and time zone, then 303                           |
| `GET /account/sign-in/remove?method=` | Asks before removing a sign-in method; 404 for one the user doesn't have |
| `POST /account/sign-in/remove` | Removes the method (CSRF-checked, and allowed like adding one), 303; 409 for the only one that can sign the user in |
| `POST /account/sign-in/passkey/rename` | Renames one of the user's passkeys (CSRF-checked), 303; 404 for another user's |
| `POST /account/sessions/<id>/revoke` | Ends one of the user's logins (CSRF-checked), 303; 404 for another user's id |

- **Profile.** The form has the username and role as read-only facts, and the display name and time
  zone as fields. The `POST` calls `Users::set_profile`, which checks the display name as creating a
  user does and the time zone as a time zone in `doozers.toml` is (`parse_timezone`), writes, and
  replaces the `UserList` snapshot, so the next request has the new values. An empty time zone
  clears it. A value that isn't valid is the page again with why (400) and what was typed, and
  changes nothing.
- **The display name in the sidebar.** `WebUser::display_name` is read from the snapshot with the
  rest of the user when the request arrives (`login::authenticate`), so a change shows from the next
  request and a proxy login has it too. The footer shows it, and its badge is its first letter.
- **Sign-in methods.** In `account_methods.rs`. The page lists what the user has: the password (when
  it was set), each passkey (name, kind, added, last used, a rename form, and **Remove**), and each
  enabled identity provider with whether and when the user connected an account, plus the
  connections to providers an admin turned off. Adding and changing reuse the existing pages rather
  than repeat their ceremonies: **Set a password** and **Change your password** open
  `/setup/password`, **Add a passkey** opens `/setup/passkey`, and **Connect** opens
  `/setup/oidc/<id>?csrf=`. Those pages lead back to the account page, not the setup page, unless
  the login came from a link (`WebApp::setup_back`). A refused login sees one **Confirm it's you**
  button where the changes would be, and a proxy login is told why it can't change anything. The
  buttons depend on `WebApp::sign_in_change` ([Step-up re-verification](#step-up-re-verification));
  the handlers check it again, so hiding a button is never the protection.
- **Removing.** The confirmation page names the method (`password`, `passkey:<id>`, or
  `oidc:<provider>`; anything else, and a method the user doesn't have, is a 404, as is another
  user's passkey id). The `POST` calls
  `Store::user_sign_in_method_remove(user, method, passkeys_work)`, where `passkeys_work` is whether
  the relying party works on this install, and the store refuses, in the removal's transaction, to
  remove the only method that can sign the user in
  ([Removing a sign-in method](storage.md#removing-a-sign-in-method)): a 409 page. The password and
  the passkeys count where passkeys work, and a connection counts while its provider is enabled with
  the issuer it was made under. The proxy identity doesn't count: it is the admin's, and works only
  behind a trusted proxy. A user with no method, such as one who was just invited, has nothing to
  keep.
- **Renaming a passkey** needs no confirmation: it adds no access. The name is cleaned as when it
  was added (trimmed, no control characters, 64 characters), and an empty one is a 400.
- **Logins.** In `account_sessions.rs`. The page lists the user's live sessions, newest first
  (`Store::web_sessions_of`): how each signed in (`method`: invite link, recovery link, password,
  passkey, or identity provider), when it started, when it was last used, and when it ends. The one
  the request came in on is marked **this login**, found by the token of the request's own cookie. A
  session is named by its opaque `id` (`web_sessions.id`): neither a token nor the hash the store
  keeps of it is ever on a page, in a link, or in a form. **End** posts to
  `/account/sessions/<id>/revoke`, which calls `Store::web_session_revoke(user, id)`: the delete is
  by user and id together, so another user's id, one nobody has, and one that isn't 32 lowercase hex
  digits are all the same 404. Revoking the current session also deletes the cookie and goes to `/`,
  as logging out does. The event streams a login opened (`GET /chats/<id>/events`) end with it: each
  one watches its login (`WebApp::login_ended`, every 5 seconds) and closes when the cookie session
  is revoked, logged out, or expired, or when the login no longer logs its user in: the user is
  disabled or removed, or the identity the login acts as isn't theirs any more, which is the
  `authenticate` check made again (a proxy login, which has no session, ends on that alone, such as
  when an admin changes the user's proxy identity), so a login that was ended stops receiving what
  the user's chats say. It needs no confirmation, because it adds no access. A proxy login has no
  session of its own, so none is marked, and the user's cookie logins are listed.
- **Channel identities.** The page lists the user's identities, marks the default, and links to the
  [Identities page](#identities-page), which stays where it was and does the adding and removing.

## Identities page

In `identities.rs`: `web.install_identities(builder, links)`, which `serve` calls with
`Runtime::links`. The page is a thin layer over the `Links` service
([Linking an identity](identity-and-routing.md#linking-an-identity)). It is for every logged-in
user, who sees and changes only their own identities, and every route is under
`/account/identities`, so it stays a page of the [account area](#account-page): the Account page
summarizes the identities and links to it, and the page links back with **Account**. Every answer is
`no-store`, every `POST` is `check_csrf`-checked, and every form works without JavaScript. No path
or field names another user: the user is always the login's.

| Route                                  | Does                                                           |
| -------------------------------------- | -------------------------------------------------------------- |
| `GET /account/identities`              | The user's identities, the form to add one, and the codes made |
| `POST /account/identities/link`        | Makes a code for the chosen channel and shows it in the answer |
| `POST /account/identities/default`     | Makes one of the user's identities the default, then 303       |
| `GET /account/identities/remove?identity=` | Asks before removing an identity                           |
| `POST /account/identities/remove`      | Removes the identity, then 303                                 |

- **The list.** Each identity of the user as the router sees it (`Links::user`, the `UserList`
  snapshot), with when it was verified and how: `link` is "Linked by you with a code", `imported` is
  "Imported from doozers.toml", and `admin` is "Signs you in to the web chat" for `web:<id>`, "Set
  by an admin for the trusted proxy" for another `web` identity, and "Set by an admin" for anything
  else. The default is marked and the others have **Make default**. **Remove** is offered for every
  identity but those in the `web` namespace (`is_web_identity`): `web:<id>` is how the user signs
  in, and the proxy identity is the admin's to set. The remove page and the `POST` answer 404 for an
  identity the user doesn't have, one that isn't a valid identity, and a `web` one, so the page
  doesn't say which. Removing the default moves it to the oldest identity left, in the store.
- **Adding.** The form lists the namespaces that running channels report except `web`
  (`Links::namespaces`): the registry's channels at the time of the request, so a plugin that isn't
  up is missing. The `POST` calls `Links::start`. On success the answer is the page, status 200,
  with the code and the exact message to send (`/link ABCD2345`, in a read-only field with a copy
  button when scripts run) and when it expires. The code is in no URL and no redirect, and the list
  afterwards never has it: it is stored as a hash, so a reload says only that a code is pending.
  Posting again makes a new code, which replaces the old. A namespace no channel reports is a 400
  that shows the page again with the sentence of `LinkError::UnknownNamespace`, which lists the
  namespaces that exist, and the form keeps what was selected.
- **Codes.** One row per channel the user made a code for and didn't use: **Pending** with when it
  works until, **Expired** with when it ran out, or **Cancelled** after too many wrong codes. A used
  code is gone, and its identity is in the list. An expired code is kept a day so the row can say
  so.

Times are shown in UTC, as on the other pages. Everything an identity says is escaped by the
templates.

## Templates and styles

Templates are in `crates/doozers-core/templates/web/`, and the design they follow is in
[Web interface design](web-ui.md). Every page for a signed-in user extends `app.html`, which extends
`layout.html` and adds the sidebar, the top bar of narrow screens, and the navigation `<dialog>`
(opened by an invoker button, `command="show-modal"`, so it needs no script). `nav.html` is the
navigation and the user footer, included in both the sidebar and the dialog. The pages that render
without a login (`error.html`, `login.html`, `login_required.html`, `invite.html`) extend
`layout.html` directly and are standalone cards, and so does `setup.html`, which a signed-in user
sees right after a link.

`app.html` reads one field, `shell` (`shell::Shell`), that every page's struct has.
`Shell::new(user, nav)` builds it from the `WebUser` and the sidebar link the page belongs to
(`shell::Nav`): the groups of links with that one marked `aria-current="page"`, the Admin group only
when `may_administer`, the user's id and its initial for the footer, and the CSRF token of the
logout form for a cookie login.

The fragments `message.html`, `tool_call.html`, `approval.html`, `compose.html`, and
`attachments.html` are rendered by `render.rs`. Nothing is inline: no scripts, styles, or `hx-on`
handlers (the CSP and `allowEval: false` forbid them). The chat page is `hx-ext="sse"` with
`sse-connect`; `#messages` has `sse-swap="message"` and `hx-swap="beforeend"`. The compose form
replaces itself (`hx-target="this"`, `hx-swap="outerHTML"`); approval forms use `hx-swap="none"`.
Every form also works as a plain form post. Styles are classes in `assets/doozers.css`.

## Tests

`crates/doozers-core/src/channels/web/tests.rs` (router-level with `tower::ServiceExt::oneshot`, a
temporary store; the chat tests inject a `WebUser`, the login tests go through the real middleware),
plus unit tests in `hub.rs`, `render.rs`, `pages.rs`, `auth.rs`, `config.rs`, `invite.rs`,
`proxy.rs`, `admin.rs`, `qr.rs`:

- Every route without a user, but the invite routes and `GET /login`, is a 401 login-required page;
  malformed chat and blob ids are 404s.
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
  replacement, `resolve_approval` outcomes (and `false` for unknown prompts and chats), plain-post
  redirect.
- Archiving and deleting (`tests/chat_actions.rs`, with the fake environment as `box`): archiving
  moves a chat to the archived list, whose page and chat pages say so and offer Unarchive, and keeps
  its workspace; Unarchive and a post bring it back; deleting removes the chat and deletes its
  workspace with `chat_deleted`, then 303s to the list it was in; a busy chat (the activity, or an
  unacked post) isn't deleted; every action needs a login and the CSRF token; a chat with nothing
  stored has no buttons and both posts are 404.
- Two users (`tests/users.rs`): an admin, a second admin, and a member, made through the `Users` API
  and logged in through real sessions (only a cookie and CSRF token, so roles come from the stored
  users). The member names the owner's objects on every route that takes an id and gets 404 (403 on
  the admin pages) with nothing changed: chats and archived chats (page, event stream, post,
  approval button, archive, unarchive, delete), a chat waiting for the core, an approval prompt,
  blobs, jobs, SSH keys, and memories. A deleted chat's id replays nothing of it. Each user sees
  only their own lists, and the same blob is each sender's own, with their own file name. A member
  can start a chat with any agent and post in it as themselves, and schedule a job. On the
  connections page the member signing out leaves the owner's tokens alone (finishing another user's
  sign-in is tested in `tests/toolset_oauth.rs`).
- Jobs page (`tests/jobs.rs`): one list across agents, soonest first, with config jobs for the user
  `owner` only, a "Doesn't run" note for a job of an unconfigured agent, and no other user's jobs.
  The form stores the spec the `schedule` tool would and notifies the scheduler; it refuses a blank
  prompt, a bad cron pattern, a past time, an agent that isn't configured, a missing CSRF token, and
  a job past the cap, keeping what was typed. Edit, disable, enable, and delete change only the
  user's own jobs (404 for another's), an edit keeps `disabled` and `conversation`, and a passed
  one-shot can't be enabled. `jobs.rs` unit tests cover the next run inside active hours.
- History: the time prefix is cut from user messages and chat titles; a post shows once on reload
  while unacked, queued, started, and stored (`render::waiting` unit tests cover replayed and
  already-stored posts).
- Blobs: media type, `nosniff`, `sandbox`, inline vs. attachment, 404s, a member refused.
- End to end: a real `Sessions` with the web channel attached and a fake turn runner that stores the
  messages and replies through `Reply::send`: a post becomes a turn, the reply reaches the open
  page, the events are acked, and a reload shows both from the transcript.
- Live output end to end: the daemon's wiring (`Runtime`, `AgentLoop`, sessions, `WebApp`) with a
  fake provider that streams slowly: a post becomes a turn whose text reaches the open page as a
  live message with out-of-band updates, then its final rendering, and a reload shows the stored
  reply once.
- Login: an invite shows its button and the nonce cookie without being used, sets up a new user once
  with the right cookie flags, a 30-day `web_sessions` row recording `invite`, and `web:<id>` as
  their identity, lands on the setup page, then is 410; a recovery link signs in with `recovery` and
  keeps the user's identities and other sessions; a new session replaces the browser's old cookie
  session; expired, unknown, and malformed links are 410 on the page and the post; a post without
  the matching nonce (no cookie, no field, another value, a cookie that isn't a nonce) is 403 and
  leaves the link unspent; a page reuses the browser's nonce; a disabled user's link is refused and
  left unspent; the setup page needs a login; expired, unknown, and orphaned sessions logged out
  with the cookie deleted; the login page says how to get a link, offers no link method, and sends a
  logged-in browser to `/`; the hourly cleanup; logout with CSRF; a cookie login's CSRF token;
  `public_url` and `X-Forwarded-Proto` making cookies `Secure` (the latter only through the proxy).
- Passkeys (`tests/passkeys.rs`, with a software authenticator from 1Password's `passkey-rs`, the
  `passkey-client` and `passkey-authenticator` crates, behind the real handlers): both buttons add a
  passkey through an invite and the setup page lists them; the options of the first button carry
  `residentKey: required` and the second `discouraged`; the authenticator keeps only the first
  discoverable; a registration the browser says wasn't discoverable is refused; only an invite or
  recovery login with its CSRF token may register, and a registration is finished once by the login
  that started it; names are trimmed, cut, and defaulted; without a secure `public_url` passkeys are
  unavailable and the page says why, while `localhost` works; the script is served under the policy
  and every script on the page is a file. Both kinds then sign in, a discoverable one through the
  first step and a non-resident key through the username step, with a new session whose method is
  `passkey` that replaces the browser's old one, and the discoverable registration is what makes the
  first step find it; the username step answers a real user, a user with no passkeys, a user who
  can't sign in, and a name that isn't a user with the same shape, the made-up ids are the same for
  a name across case, requests, and a restart, and finishing a made-up ceremony fails like a wrong
  passkey; a disabled user, a passkey whose authenticator reports another user's well-formed handle
  (at both steps), and a counter that went back are refused with the same 401 and leave the passkey
  unchanged; every sign-in POST needs its browser's nonce and a ceremony belongs to the nonce that
  started it and signs in once; starts are limited per client address; the login page folds the
  password form behind "Use a password instead" and loads the script only when passkeys are
  available. `passkey.rs` has unit tests for the origin rules, user handles, and which binding a
  ceremony belongs to, and `pending.rs` for the table (single use, binding, expiry, the cap).
- Step-up (`tests/reverify.rs`, with the passkey and identity provider ways in `tests/passkeys.rs`
  and `tests/oidc.rs`): a session that signed in with a password, which is what a stolen cookie is,
  can't set a password, start a passkey registration, or connect an account, and its pages say to
  confirm it's you; a recovery session still can. Confirming with the password stores the time on
  that session only, leads back to one of the three pages and nowhere else, and lets that session
  change methods; another session of the user, and another user's, still can't. A confirmation lasts
  10 minutes, a wrong password confirms nothing, and guessing counts against the same limits as
  logging in, both ways. A passkey confirms only if it is the user's own, in the login that started
  the ceremony, once; an account at a provider only if it is connected to the user. A proxy login
  can't confirm.
- Sign-in methods (also `tests/account.rs`): the page lists the user's own methods and offers
  changes only to a login that may (a recovery link's, or one confirmed lately), with one
  **Confirm it's you** button for another login; each kind is removed through its confirmation page
  until the last one that works is refused with a 409; removal needs the same confirmation as adding
  (a session that was only logged in with a password gets a 403 and nothing changes, until it
  confirms, and again after 10 minutes), the CSRF token, and a login; another user's passkey id, an
  id nobody has, and a malformed method are 404s; where passkeys can't be used the password stays as
  the last way in; a connection to a provider that is off doesn't count; a passkey is renamed
  without confirming and nobody else's is.
- Logins (also `tests/account.rs`): ending a login ends the chat streams it opened, whether it was
  revoked from another browser or logged out; a proxy login marks no login as its own and ends a
  cookie login without touching any cookie; the page lists only the user's own sessions with how
  each signed in and marks the current one, and shows no token or hash of one; ending another login
  ends it and leaves this one; ending this one deletes the cookie and logs out; another user's id,
  an id nobody has, and a malformed one are 404s that leave the sessions alone; the CSRF token is
  needed; the last use is recorded by the first request and not again within 10 minutes.
- Account page (`tests/account.rs`, through the real login middleware with a session cookie, over
  stored users): the page needs a login and shows only the user's own profile and identities; the
  sidebar links it and the footer shows the display name; saving changes the name and time zone the
  next request shows and leaves other users alone; an invalid name or time zone is refused with why
  and what was typed; a post needs the CSRF token; what a user wrote is escaped.
- Identities page (`tests/identities.rs`, over stored users, a `Links` with a fixed set of
  namespaces, and the real login middleware's test user): the page needs a login, lists only the
  user's own identities (and offers no remove for the web sign-in), marks Account current in the
  sidebar, and offers the running channels but not `web`; a post makes a code, which is shown once,
  in the answer and in no redirect, with the exact `/link <code>`, and never again, so the list says
  only that one is pending; sending the code through `Links` adds the identity as a `link`, which
  the page then lists and the router's snapshot resolves, and the code is gone; a new code replaces
  the old, which stops working; a namespace no channel reports (`discord:discord`, blank, another)
  and `web` are 400s that show the page again with the namespaces that exist and make no code; an
  expired code and one cancelled by wrong tries are listed as such; every post needs the CSRF token
  and changes nothing without it; the default changes for the user, and another user's identity, one
  nobody has, and nonsense are 404s; removing asks first, drops messages from the identity, moves
  the default, and can't touch another user's identity, the web sign-in, or an identity nobody has;
  what an identity says is escaped, each kind says how it was added, and a disabled user can't make
  a code.
- OpenID Connect (`tests/oidc.rs`, against `tests/fake_oidc.rs`, a provider in the test process on a
  loopback port with a discovery document, a key set, and a token endpoint that signs ES256 ID
  tokens and checks the client, the redirect URI, the single-use code, and PKCE): an invited user
  connects an account and signs in with it later, with the provider's own request checked (PKCE,
  state, nonce, the callback, the cookie); an account nobody connected, a disabled user, and a
  replaced account are refused alike; a connection is the user's, one account per provider, and
  can't take another user's; each failure of an ID token (a bad signature, another key, an unknown
  `kid`, another issuer or audience, several audiences, another `azp`, expired, issued in the
  future, no or another nonce, `alg: none`, an HMAC with the client secret, no ID token, an empty
  subject) is refused while a good one and the small differences allowed are accepted; a callback
  with no binding, another browser's, a forged, replayed, or other provider's `state`, an `error`,
  or another issuer is refused; a connection finishes only in its own login and only from a link's
  login; a disabled provider is neither offered nor used; the client secret goes in the way the
  provider's document says, and a wrong or missing one signs nobody in; a provider with no secret
  works with PKCE alone; starts need the login page's nonce, which costs nothing when it is missing
  or wrong, and are limited; a second tab keeps the first sign-in working (the test browser sends a
  cookie only to the paths it names); keys are refreshed once a minute for an unknown `kid`, and
  tokens with the same new `kid`, signed in at once, all find the key one of them fetched; and the
  discovery document is kept for an hour and refused for each reason it can be unusable. `oidc.rs`
  has unit tests of the HTTP adapter's rules (an `https` issuer's requests must be `https`, a
  redirect isn't followed, a body over 1 MiB is refused) and of the endpoint schemes a document may
  name.
- Identity provider pages (also `tests/oidc.rs`): an admin adds a provider in the form, the invited
  user connects an account and signs in with it, and an account nobody connected is refused; members
  and anonymous requests get 403 and 401 and every change needs the CSRF token; a provider that
  can't be used (a bad id, blank fields, an `http` issuer, one that can't be reached, another issuer
  in its document) is refused with the reason and nothing, not even the secret, is stored; an id is
  taken once and scopes are made tidy; editing keeps, replaces, or removes the secret and never
  renders it, even after an error; changing the issuer disconnects the old accounts; disabling and
  enabling change the pages; removing asks first, then forgets the connections and the secret; and
  everything a provider's name says is escaped.
- Proxy: the identity header ignored on the main listener and honored on the proxy app, unknown
  identities refused, posts carry the header's identity and need its CSRF token; a TCP listener
  needs the exact secret; binding (socket mode 0600, `InUse`, stale socket replaced, non-socket left
  alone). `crates/doozers/tests/serve.rs` runs the daemon with a proxy socket and checks it over
  real connections, and that the socket is gone after exit.
- Admin pages (`tests/admin.rs`, over a fake admin service): anonymous requests get 401 and a member
  403 on every admin route, with nothing restarted; users made through `Users` (the imported owner,
  a second admin, a member) log in with real sessions, and the role decides: both admins get every
  page and the restart, the member none, a demotion or promotion applies to the next request of the
  same session, a disabled admin is signed out, and the last active admin can't be demoted; the
  status page shows the fake state with plugin strings escaped and no reload; a `NeedsAuth` QR
  channel gets an `<svg>` (a new payload, a new code), the refresh, and never the payload text; pair
  codes and URLs are escaped, and a `javascript:` URL isn't a link; the plugins page shows details
  and a restart form only for supervised plugins, including a running and a failed toolset (state,
  tool count, and error); the status summary has the tool count; restarting needs the CSRF token
  (field or header), works for a toolset, and shows the service's `NOT_FOUND` escaped; every admin
  answer, refusals and redirects included, is `no-store`. `qr.rs` checks that the SVG draws exactly
  the code's dark modules inside the quiet zone, and that an oversized payload is an error.
  `crates/doozers/tests/serve.rs` checks that the daemon serves `/admin` from its own admin service,
  behind the login.
- Route pages (`tests/routes.rs`, over the test configuration): anonymous requests get 401 and a
  member 403 on every route including the POSTs, with nothing changed; every POST needs the CSRF
  token (a wrong or missing one is 403, the header works as well as the field); the list is in order
  with escaped values, the agent links, no Up on the first route and no Down on the last, warnings
  for a route that never matches or points at a gone instance or agent, and the notice for
  `[[routes]]` still in `doozers.toml`; adding stores the route and the router's `RouteList` picks
  it for the next message, at the end or before a chosen route; an invalid form (unknown instance or
  agent, bad kind, control characters) is shown again with the problem, its values and nothing
  stored; a duplicate, a shadowed route, and a route that would hide another are 409; edit, remove
  (not by `GET`), and move change which route wins in the router's list; a move that would hide a
  route is refused; ids that aren't numbers or routes are 404.
- Known SSH hosts page (`tests/ssh_hosts.rs`, over the test configuration and a real store):
  anonymous requests get 401 and a member 403 on every route including the POSTs, with nothing
  changed; every POST needs the CSRF token (field or header); the list groups recorded and pinned
  keys by environment with host (`[host]:port` off port 22), type, fingerprint, who recorded or
  pinned the key, and last used; the empty page and the page without environments; the confirmation
  names the host and what deleting does, the `POST` deletes and redirects, and an unknown id is a
  404; adding pins the key and the list says `pinned by owner`, a line with several hosts pins each,
  every refusal (hashed name, marker, wildcard, certificate key type, garbage, unknown environment)
  is a 400 with the reason, the pasted line, and nothing stored, a known key is a 409, and pasted
  text is escaped; the sidebar entry is under **Admin**, and the shell leaves it out for a member.
- Workspaces page (`tests/workspaces.rs`, over the test configuration and the in-memory fake
  environment connected as `box`, with `far` never connecting): anonymous requests get 401 and a
  member 403 on every route including the POSTs, with nothing changed; every POST needs the CSRF
  token; the list shows the workspace with its live state, disk use, free space, description,
  details, whose SSH keys each workspace gets, agent and web-chat links, and the job-run and
  other-channel owners without a chat link; a failing or disconnected environment shows `unknown`
  and is named, with what Doozers stored still shown; plugin text is escaped; stopping a workspace
  nothing is using needs no confirmation, and stopping one a turn is using asks first (the button is
  a link, the page says what ends), then stops without changing the row; deleting has a plain
  confirmation, or the in-use warning, then deletes with the operator reason (stop first when in
  use, and the turn's next `open` fails), is finished later when the environment is down, and leaves
  the page without the workspace; ids that are gone, invalid, or still being created, and a stop
  with the environment down, answer as described above.
- Toolset pages (`tests/toolsets.rs`, over the test configuration, a real `ToolsetManager` whose
  remote servers never answer, and the admin tests' fake service): anonymous requests get 401 and a
  member 403 on every route including the POSTs, with nothing changed; every POST needs the CSRF
  token (field or header); the list shows state, tool count, and error from the service, in order,
  with escaped values, and the stdio toolset of `doozers.toml` without Edit or Remove; the form has
  only the settings of a remote server; creating stores the spec, adds the toolset to the running
  configuration, and the agent form offers its box; a post shaped like a stdio server is a 400 at
  `url` and stores nothing; an invalid form (a missing secret, a bad duration, a bad URL, plain
  `http` to another machine) is a 400 on the form with the key and the typed values and stores
  nothing; a stored name, a name of `doozers.toml`, and a channel's name are 409, and `new` and an
  invalid name are 400; an edit replaces the settings and one made from an older `updated_at` is a
  409 with a reload link; editing or removing a toolset of `doozers.toml` is a 409; the removal page
  lists the agents that mention the toolset and removing takes the box away; values are escaped on
  every page; a secret's value and a token stored as text never appear, and saving that toolset with
  a secret starts it; Restart goes through the admin service and returns to the page, and refuses a
  name that is not a toolset; a failed stored toolset shows its reason and starts when saved with a
  working setting; a stored spec that can't be read gets an empty form and can be removed.
- Renaming a toolset: the preview lists the agents whose globs change and changes nothing; posting
  renames the toolset, rewrites the agent's file, and redirects to the new name, and the old name is
  a 404; a glob such as `box*` is a 400 with the manager's reason and the typed name, on the preview
  and on the post, and nothing changes; a stored name, a name of `doozers.toml`, and an invalid name
  come back on the form; the name is escaped; a toolset of `doozers.toml` has no Rename link and is
  a 409; every rename route is 401 without a login and 403 for a member, and the POST needs the CSRF
  token. An agent's `agent.toml` made unreadable (skipped when running as root) fails the check: the
  preview and the post are a 500 on the form with the step, the reason, "Nothing was changed", and
  the typed name, and the toolset and the agent are as they were. Undoing a failed step can't be
  provoked from the router, so a unit test in `toolsets.rs` renders the page for a `Failed` error
  with undo errors: the list to put right by hand, escaped, and no "Nothing was changed".
- The user pages (`tests/admin_users.rs`, with the fake environment as `box`): every route is 401
  without a login and 403 for a member; the list shows each user's username, display name, role, and
  state, a Remove link each, and the Users link marked current; the confirmation counts a user's
  chat, messages, memory, profile, identity, job, and workspace, and has the button; a user with
  work running has no button, and posting is a 409 that changes nothing; the post needs the CSRF
  token, then forgets the user (the user, their workspace, and `users/<id>/` are gone) and 303s to
  the list with the notice; an admin can't remove themselves, and a request whose admin is stale
  can't remove the last active admin, each a 409 with the reason and nothing changed. What the
  removal deletes is tested in `forget/tests.rs` against every place that holds data.
