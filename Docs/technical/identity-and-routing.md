# Identity and routing

Every message a channel receives passes one pure decision before anything is stored: drop it, handle
it as a core command, or hand it to an agent's session ([Plan](../PLAN.md) §1.1, §3.2, §3.3,
§6.1–§6.4). Four modules of `doozers-core` implement it; the inbox and sessions code calls
`Router::classify` for every `ChannelEvent::Message`. The user-facing side is in
[Configuration](../user/configuration.md) and [Chat commands](../user/commands.md).

| Module     | Contents                                                                    |
| ---------- | --------------------------------------------------------------------------- |
| `identity` | `OWNER`, `owner()`, `validate_namespace`, `NamespaceError`                  |
| `users`    | `User`, `UserIdentity`, `UserList`, `UserDirectory`, `Users`, `UsersError`, `UserError`, `LoginUserError` |
| `commands` | `CoreCommand`, `ApprovalReply`, `Answer`, `parse`, `PendingApprovals`, `NoPendingApprovals` |
| `router`   | `Router`, `ChannelSource`, `Routed`, `Inbound`, `DropReason`                |
| `routes`   | `Route`, `RouteList`                                                        |

## The decision

```rust
let router = Router::new(registry.users(), RouteList::new(routes), agents);
let source = ChannelSource { instance, component, identity_namespace };
match router.classify(&source, &event_id, &message, &pending_approvals) {
    Inbound::Drop(reason) => { /* already logged; ack the event and forget it */ }
    Inbound::Command { to, command } => { /* /new, /stop, /env reset, or an approval reply */ }
    Inbound::Message(to) => { /* store in the inbox for the session to.session */ }
}
```

`classify` runs these checks in order; the first failure is the `DropReason`:

1. `InvalidNamespace`: the channel's reported identity namespace fails `validate_namespace`.
2. `GroupConversation`: the conversation kind isn't `Direct`. v1 ignores groups entirely (§1.1); any
   future kind is dropped too until it's supported. This runs before the authentication check so
   unverified chatter in a group (an IRC channel line from a user without a services account) logs
   at `debug`, not `warn`.
3. `Unauthenticated`: `sender_authenticated` is false. This runs before any identity lookup, so a
   forged sender learns nothing (§3.3).
4. `UnknownSender`: neither `sender` nor any `sender_aliases` entry is the identity of a user.
5. `DisabledUser`: the user the sender resolves to is `disabled`. Their messages, commands included,
   are dropped until an admin enables them again.
6. `UnknownAgent`: a web chat started with a chosen agent names one that isn't configured (see
   [Routing](#routing)).
7. `NoRoute`: no route matches.

A message that passes is `Routed { session: SessionKey { agent, channel, conversation }, user }`,
where `user` is the id of the user the sender resolved to: the turn's requester. If it has no
attachments and `commands::parse` recognizes its text, it's `Inbound::Command`; otherwise
`Inbound::Message`.

The router reads no sessions, so one more check follows it: a conversation is held by one user at a
time. The [session pump](sessions.md#the-inbound-pipeline) drops a routed message or command when a
session of its channel instance and conversation belongs to another user: for good on the web, and
only while the session is open on a channel whose conversation ids can change hands, like an IRC
nickname. The session key has no user, and without that check a second user in the conversation
would run in the first one's session.

Core commands are recognized after routing, not before it: `/new`, `/stop`, `/env reset`, and
approvals all act on a session, and the session key includes the routed agent. They still never
reach the agent (§3.2). A command in a conversation no route matches is dropped like any other
message.

### Logging

`classify` logs every drop with the instance, the event id, and the reason, never the text. Levels:
`warn` for an invalid namespace and for unauthenticated senders (a misbehaving channel or a forgery
attempt), `info` for unknown senders (with `<namespace>:<sender>` and the alias count, so the
operator can see the identity the channel reported), for disabled users, and for unrouted
conversations and chats with an unknown agent (with the conversation id), and `debug` for group
conversations, which an IRC channel can produce on every line.

## Users

A user is a row of the `users` table with their identities in `user_identities`
(`Docs/technical/storage.md`): an opaque id, a username, a display name, a role (`admin` or
`member`), a state (`active` or `disabled`), a time zone, a default identity, and the identities
that act as the user, each with when and how it was verified. `users::User` is that row in memory.
Everything keyed by user uses the id: `Routed::user` (the turn's requester), session rows, inbox and
usage rows, per-user files (`users/<id>/USER.md`), and the owner of a user's memories (below).

The user v1 began with, the owner, has the id `identity::OWNER`, `"owner"`, and `identity::owner()`
returns it as a `UserId`. The import (below) gives that id to the user `[owner]` described, so
nothing keyed by it is rewritten. The id marks data, not privilege: no check compares a user to it
(config jobs, which run as the user `owner` until runtime jobs replace them, are the one place that
still keys on it). A second user can be created through `Users::create`, but nothing in the product
does yet. Routes have no `user` key.

### Roles

A user is an `admin` or a `member`. All admins are equal; members use every agent. One test decides
privilege, `User::is_admin`: the role is `admin` **and** the state is `active`. It is read from the
current snapshot (`UserDirectory::is_admin(id)`, `UserList::is_admin(id)`), never cached past a
request or a call, so a demotion or a disabling applies to the next one. A user the list doesn't
know is no admin. What it admits:

| Where                                                    | How it asks                                               |
| -------------------------------------------------------- | --------------------------------------------------------- |
| Web admin pages and their buttons (`may_administer`)     | `WebUser::role`, set when the request is authenticated    |
| Admin calls of `SshKeys` (`all_keys`, `any_key`, `delete_any`) | `SshKeys::may_administer`, over the shared `UserList` |
| Shared-memory writes in a tool call (`memory::viewer`)   | `UserDirectory::is_admin(requester)`                      |
| Shared-memory writes shown in the prompt (`memory::prompt_viewer`) | the same, for the session's user                |
| Shared-memory writes, group memories, and the *shared* box on the memory page | `WebUser::role`                      |

What is not a role check: the web chat's agent picker lists every agent for every user,
`GET /blobs/<sha256>` follows who owns a message that refers to the blob (an admin has no wider
access), and every other per-user page and operation belongs to the user who owns the object,
whatever their role ([Whose data is whose](web-channel.md#whose-data-is-whose)). A config job runs
as the user `owner` and, with no requester, writes shared memories; the web jobs page shows config
jobs to that user, since they are that user's data; approvals go to the turn's requester (the
delivery user for a job), and no role changes that.

The last active admin can't be removed, disabled, or demoted; see
[the last active admin](#stored-users).

### Stored users

`users::Users` (one per core, `Runtime::users`) owns the users and keeps the snapshot the router
reads:

- **Liveness.** `Registry::users()` is the `UserList` the router, the web login, and the lookups
  below share, in the `RouteList` pattern: a snapshot (`UserDirectory`) that every successful change
  replaces as a whole, so the next message is resolved against the new users and nothing restarts.
  `classify` stays synchronous and off the database, including for messages it drops. One async lock
  spans read, validate, write, and replace, so concurrent changes serialize and the snapshot always
  equals what is stored. Only this process changes the tables.
- **Operations.** `create(user, identities, now)`, `set_state(id, state, now)`,
  `set_role(id, role, now)`, `remove(id)`,
  `add_identity(id, namespace, native_id, verification, now)`, `remove_identity(id, identity, now)`,
  and `login_user(requested, now)`. A username is 1 to 64 letters, digits, `.`, `_`, and `-`; the
  store compares it ignoring ASCII case. An identity belongs to one user (`IdentityTaken`).
- **The last active admin.** `set_state` to `disabled`, `set_role` to `member`, and `remove` refuse
  (`LastAdmin`) when the user is the only active admin; a disabled admin doesn't count as one to
  keep. The store makes the check inside the write transaction of the change (`user_set_state`,
  `user_set_role`, `user_delete`), so the check and the change can't be split by another one, and
  the users lock above serializes the callers besides. `remove` deletes the account and its
  identities and nothing else; the operation that forgets a user's data is built on it.
- **Import.** `doozers.toml`'s `[owner]` stays in `Config::legacy_owner`, as written. `Users::open`
  runs at `Runtime::start`: if the database hasn't recorded the import (`users_imported`), it
  validates `[owner]` as `doozers.toml` always did (at least one identity, no identity written
  twice, a `default` that is one of them and required when there are several, a known `timezone`),
  except that identities differing only in the case of an email domain, which the old exact
  comparison accepted as two, are one: the first is kept and a `warn` names the others. It creates
  the user `owner` (username `owner`, display name `Owner`, role `admin`) with its identities stored
  as `imported`, its default identity and its time zone, and records the import, all or nothing. An
  invalid `[owner]` fails startup with `<path to doozers.toml>: owner.<key>: <reason>`. A config
  with no `[owner]` still records the import, so a fresh install's first admin, made by
  `login_user`, is never followed by an import. So does an import that finds users stored already.
  After that `[owner]` is never read: not when it changes, not when it has become invalid, not when
  every user is gone. While the file still has `[owner]`, startup logs `info` on the importing start
  and `warn` on every later one, saying it is ignored and to remove it.
- **The first admin.** `login_user(None, now)` is `doozers admin login-link` with no `--user`. With
  no users it creates the first admin, with the id `owner` (the user config jobs run as, so a fresh
  install and an imported one agree until runtime jobs replace them), the username `admin`, and the
  identity `web:owner`, and returns it. With exactly one user it is that user, so an install that
  has only its owner keeps working with no arguments. With several it asks for one (`NeedUser`,
  whose message lists their ids and usernames). The admin service reports the user it chose, and the
  CLI prints the id and username with the link. `login_user(Some(id), now)` requires an active user.
  The admin service calls it, then `create_invite`, only if the web channel is running, so a daemon
  that can't make a link makes no user.
- **Invites and recovery links.** `create_invite(user, kind, created_by, now)` stores a one-time
  link for an active user (`InviteKind::Invite` or `Recovery`, valid `INVITE_TTL`, 72 hours) and
  returns its token and expiry ([web channel](web-channel.md#invite-and-recovery-links)). It gives
  the user the identity `web:<id>`, verified as `admin`, if they lack it: a web sign-in sends as
  `web:<id>`, so without it the router would drop their messages. If another user has `web:<id>` it
  refuses (`WebIdentityTaken`). It takes nothing away from the user.
- **Time zone and default identity.** `agent_loop::history::message_timezone(users, user, agent)` is
  the user's time zone, else the agent's, else the system's; a config job has no user and runs as
  `owner`. Jobs and proactive messages go to the user's default identity (`OutboxItem::to_user`); a
  user without one gets none.

### Identity in memory

Memory ([Skills and Memory](skills-and-memory.md#memory)) uses identity in three ways, all in
`doozers_core::memory`:

- **A user's memories belong to the internal user id**, not to a channel identity. They follow the
  user's direct and job sessions with the agent on every channel, as `memory_search`'s scope does.
- **Shared memories are written by admins only.** `memory::viewer` gives a tool call
  `can_write_shared` when the turn's requester is an active admin (`UserDirectory::is_admin`), and a
  config job (no requester) too; a runtime job runs as its creator and has that creator's right.
  Shared memories reach every prompt, including the tool-enabled DMs of admins, so a member or a
  group must not be able to plant one. The store checks the flag again in its write transaction, so
  a tool can't skip it; it trusts the flag, because the memory database knows no users, and the core
  decides it from the role at the time of the call.
- **A group's private scope is keyed by channel instance and conversation**
  (`MemoryOwner::Group { instance, conversation }`), taken from the session, not from whoever
  speaks: every participant shares it, and none of their own memories is in it. The conversation id
  alone isn't enough, since the same id can exist on two instances; `doozers channel rename`
  rewrites the instance part of these keys ([Instance rename](instance-rename.md)). v1 drops group
  messages at `classify` (above), so no group session exists yet.

## Identity namespaces

A channel reports its namespace in `ChannelInfo::identity_namespace` (§3.2).
`validate_namespace(component, namespace)` accepts it only if it is the component name itself
(`web`, `whatsapp`) or starts with `<component>:` (`irc:libera.chat` from `doozers-channel-irc`).
The match is on whole segments, so component `irc` can't claim `ircx`, and case-sensitive, so it
can't claim `IRC` either. Every `:`-separated segment must be non-empty, without whitespace or
control characters.

Channel registration (plugin host and the built-in web channel) calls `validate_namespace` once and
refuses a channel that fails. `classify` repeats the check per event (a few byte comparisons) so the
decision is complete on its own.

## Matching identities

An identity is `<namespace>:<native id>` (§3.2). `UserDirectory::resolve(namespace, id)` joins the
two and looks the string up in an index of every user's identities, built when the directory is
made. Because a namespace may contain `:`, this is exactly a comparison of the joined strings; it
never splits an identity on its own. `resolve_sender` tries the message's `sender` and then each
alias in order, and the first that is anyone's identity names the user.

Comparison is exact and case-sensitive, with one exception: in the `email` namespace (first segment
`email`), the domain after the last `@` is compared ASCII case-insensitively, as DNS names are. The
local part stays case-sensitive: RFC 5321 lets a mail server treat it so, and folding it could match
a different mailbox. This fails closed: a user whose mail arrives as `Me@…` has that spelling. Other
namespaces rely on the channel to report a stable form (the IRC services account name, the WhatsApp
number in E.164); the identity is written the way the channel reports it, which the `info` log of an
unknown sender shows.

The store splits an imported identity at its first `:`, since `[owner]` can't say where a namespace
such as `irc:libera.chat` ends: lookups never depend on the split.

The index holds each identity in its canonical form, `doozers_store::canonical_identity`: the joined
identity with the domain of an `email` identity lowercased. The store keeps identities in that form
too, so one sender can't belong to two users by differing in the case of an email domain, and a
lookup builds the same key, so the comparison above is one hash lookup per sender or alias.

## Routing

`Router::route(instance, conversation)` returns the agent of the first route whose every set key
matches: `channel` (the instance name), `kind`, and `conversation` (the id exactly as the channel
reports it). A route with no keys besides `agent` matches everything. The router holds a `RouteList`
(`routes.rs`), a shared, atomically replaced snapshot of the ordered routes, and reads it once per
message, so a change applies to the next message and nothing restarts.

### Stored routes

The routes are the `routes` table (`Docs/technical/storage.md`), not config. `routes::Routes` (one
per core, `Runtime::routes`) owns them:

- **Liveness.** `Routes::list()` is the `RouteList` the router holds. Every successful change
  reloads the stored list and replaces the snapshot. A snapshot, instead of a database read per
  message, keeps `classify` synchronous and off the database, including for messages it drops. One
  async lock spans read, validate (including that the agent exists, checked under the lock), write,
  and replace, so concurrent changes serialize and the snapshot always equals what is stored. Only
  this process writes the table. `hold_unrouted(agent)` takes the same lock for the caller if no
  route names the agent, which is how removing an agent in the web UI keeps a route from being made
  for it in between ([Web Channel](web-channel.md#removing)).
- **Operations.** `add(spec, before)`, `update(id, spec)`, `remove(id)`, `move_route(id, up)`, and
  `view()` (every route with what is wrong with it now). Routes have stable ids, so a stale page
  can't act on another route (`RouteError::NotFound`).
- **Validation**, like config: `RouteSpec` is text (blank is any), checked against the running
  configuration (`LiveConfig`): the channel instance and the agent exist, the kind is `direct` or
  `group`, the conversation has no control characters and is at most 512 bytes, the agent is
  required. A route that no earlier route hides is required too: `Unreachable` if earlier routes
  match everything it does (one route, or for a route with no kind, one covering direct and one
  covering group conversations), `Duplicate` if one matches exactly the same, `Hides` if the change
  would leave a later route unusable (naming the other earlier route when the changed one hides it
  together with another). Hiding is judged against the list before the change, so a route that was
  already unreachable (one imported from `doozers.toml`, which never checked this) doesn't block
  unrelated changes; `view()` flags it, as it flags a route whose channel instance or agent is no
  longer configured, and `Routes::open` logs both at startup. Refusing is a deliberate choice over a
  warning: a route that can't match is almost always a mistake, and reordering keeps every step
  valid.
- **Import.** `doozers.toml`'s `[[routes]]` stay in `Config::legacy_routes`, as written and
  unvalidated. `Routes::open` runs at `Runtime::start`: if the database hasn't recorded the import
  (`routes_imported`), it validates each rule like a route made in the web UI (shadowed routes are
  kept as written, but an empty or blank `channel`, `kind`, or `conversation` is an error, as it was
  in config), stores them in file order, and records the import, all or nothing; an invalid rule
  fails startup with `doozers.toml: routes[<i>].<key>: <reason>`. A config with no `[[routes]]`
  still records the import, and so does an import that finds routes stored already. After that the
  rules are never read: not when the file changes, not when the owner has deleted every route (the
  record, not an empty table, says it ran), not when they have become invalid. While the file still
  has `[[routes]]`, startup logs `info` on the importing start and `warn` on every later one, saying
  they're ignored and to remove them. `doozers status` doesn't report it: its response has no field
  for a warning (`config_error` is for reload errors), and a new proto field wasn't worth it for a
  line already in the log and on the routes page.

A web chat can be started with a chosen agent (the chat list's picker). Its chat id, which is the
conversation id, is then `<agent>.<32 hex digits>` instead of 32 hex digits (`channels::chat_ids`:
`new_agent_chat_id`, `chat_agent`). For an event from the `web` component with such an id,
`classify` takes that agent ahead of every route, if `agents` (an `AgentPolicies`, in the daemon the
running `LiveConfig`) has it; otherwise the message is dropped as `UnknownAgent`, never sent to a
routed agent instead. The rule is only for the web component: on any other channel such an id is an
ordinary conversation id. Because `agents` is the running configuration, an agent created in the web
UI is reachable at once, with no route for it. Only a user whose identity matches gets past the
sender check, and the web pages offer only the agents the user may use (every user may use all of
them for now).

## Core commands

`commands::parse(text, is_pending)` recognizes a command only when it is the whole message, after
trimming whitespace, with words compared ASCII case-insensitively (phones capitalize):

| Text                                       | Result                                         |
| ------------------------------------------ | ---------------------------------------------- |
| `/new`                                     | `CoreCommand::New`                             |
| `/stop`                                    | `CoreCommand::Stop`                            |
| `/env reset`                               | `CoreCommand::EnvReset`                        |
| `yes`, `yes!`, `no`                        | `Approval { answer: Yes/YesForSession/No, code: None }`, if any approval is pending |
| `yes 4821`, `yes! 4821`, `no 4821`         | `Approval { code: Some("4821") }`, if that code is pending |

Anything else is ordinary text, including `/new please`, `/new@doozers`, `/env`, `/env reset now`,
`/link` (no self-linking in v1), unknown `/words`, `yes please`, and a `yes`/`no` the lookup doesn't
claim. Codes are a single token of ASCII letters and digits, passed to the lookup as typed. A
message with attachments is never a command, so an attachment can't be silently lost.

Whether `yes` is an approval reply depends on state the approvals code owns, so it's an input: the
`PendingApprovals` trait, `is_pending(to: &Routed, code: Option<&str>) -> bool`. It's implemented
for closures of that shape, by `NoPendingApprovals`, and (through `sessions::Approvals`) by
`approvals::ApprovalBoard`, which claims every reply while an approval waits for `to.user` in
`to.session`, and a reply naming one of theirs that closed recently ([Approvals](approvals.md)).
With nothing pending, `yes` and `no` are ordinary messages, so they can answer the agent's own
questions.

The commands' effects belong to the inbox and sessions code (`/new`, `/env reset`), turn control
(`/stop`), and approvals (replies); [Sessions and turns](sessions.md) records each command's event
id as a dedupe key before acting on it, so a redelivered command acts once.
