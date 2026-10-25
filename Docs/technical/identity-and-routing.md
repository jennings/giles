# Identity and Routing

Every message a channel receives passes one pure decision before anything is stored: drop it,
handle it as a core command, or hand it to an agent's session ([Plan](../PLAN.md) §1.1, §3.2,
§3.3, §6.1–§6.4). Three modules of `giles-core` implement it; the inbox and sessions code calls
`Router::classify` for every `ChannelEvent::Message`. The user-facing side is in
[Configuration](../user/configuration.md) and [Chat Commands](../user/commands.md).

| Module     | Contents                                                                    |
| ---------- | --------------------------------------------------------------------------- |
| `identity` | `OWNER`, `owner()`, `validate_namespace`, `NamespaceError`, `OwnerIdentities` |
| `commands` | `CoreCommand`, `ApprovalReply`, `Answer`, `parse`, `PendingApprovals`, `NoPendingApprovals` |
| `router`   | `Router`, `ChannelSource`, `Routed`, `Inbound`, `DropReason`                |
| `routes`   | `Route`, `RouteList`                                                        |

## The Decision

```rust
let router = Router::new(&config.owner, RouteList::new(config.routes.clone()), agents);
let source = ChannelSource { instance, component, identity_namespace };
match router.classify(&source, &event_id, &message, &pending_approvals) {
    Inbound::Drop(reason) => { /* already logged; ack the event and forget it */ }
    Inbound::Command { to, command } => { /* /new, /stop, or an approval reply */ }
    Inbound::Message(to) => { /* store in the inbox for the session to.session */ }
}
```

`classify` runs these checks in order; the first failure is the `DropReason`:

1. `InvalidNamespace`: the channel's reported identity namespace fails `validate_namespace`.
2. `GroupConversation`: the conversation kind isn't `Direct`. v1 ignores groups entirely (§1.1);
   any future kind is dropped too until it's supported. This runs before the authentication check
   so unverified chatter in a group (an IRC channel line from a user without a services account)
   logs at `debug`, not `warn`.
3. `Unauthenticated`: `sender_authenticated` is false. This runs before any identity lookup, so a
   forged sender learns nothing (§3.3).
4. `UnknownSender`: neither `sender` nor any `sender_aliases` entry is an `[owner]` identity.
5. `UnknownAgent`: a web chat started with a chosen agent names one that isn't configured (see
   [Routing](#routing)).
6. `NoRoute`: no route matches.

A message that passes is `Routed { session: SessionKey { agent, channel, conversation }, user }`.
If it has no attachments and `commands::parse` recognizes its text, it's `Inbound::Command`;
otherwise `Inbound::Message`.

Core commands are recognized after routing, not before it: `/new`, `/stop`, and approvals all act
on a session, and the session key includes the routed agent. They still never reach the agent
(§3.2). A command in a conversation no route matches is dropped like any other message.

### Logging

`classify` logs every drop with the instance, the event id, and the reason, never the text. Levels:
`warn` for an invalid namespace and for unauthenticated senders (a misbehaving channel or a forgery
attempt), `info` for unknown senders (with `<namespace>:<sender>` and the alias count, so the
operator can add a missing identity) and for unrouted conversations and chats with an unknown
agent (with the conversation id), and `debug` for group conversations, which an IRC channel can
produce on every line.

## The Owner

v1 has one implicit user, the owner (§1.1). `identity::OWNER` is its user id, `"owner"`, and
`identity::owner()` returns it as a `UserId`. Everything keyed by user uses it: `Routed::user`
(the turn's requester), session rows, inbox and usage rows, and per-user files
(`agents/<agent>/users/owner/MEMORY.md`, `users/owner/USER.md`). With no roles in v1, routes have
no `user` key: every routed message is from the owner.

## Identity Namespaces

A channel reports its namespace in `ChannelInfo::identity_namespace` (§3.2).
`validate_namespace(component, namespace)` accepts it only if it is the component name itself
(`web`, `whatsapp`) or starts with `<component>:` (`irc:libera.chat` from `giles-channel-irc`).
The match is on whole segments, so component `irc` can't claim `ircx`, and case-sensitive, so it
can't claim `IRC` either. Every `:`-separated segment must be non-empty, without whitespace or
control characters.

Channel registration (plugin host and the built-in web channel) calls `validate_namespace` once
and refuses a channel that fails. `classify` repeats the check per event (a few byte comparisons)
so the decision is complete on its own.

## Matching Identities

An identity is `<namespace>:<native id>` (§3.2). `OwnerIdentities::contains(namespace, id)`
compares that string to each `[owner]` identity without building it: the configured identity must
start with the namespace and `:`, and the remainder must equal the native id. Because a namespace
may contain `:`, this is exactly a comparison of the joined strings; it never splits a configured
identity on its own.

Comparison is exact and case-sensitive, with one exception: in the `email` namespace (first
segment `email`), the domain after the last `@` is compared ASCII case-insensitively, as DNS names
are. The local part stays case-sensitive: RFC 5321 lets a mail server treat it so, and folding it
could match a different mailbox. This fails closed: an owner whose mail arrives as `Me@…` lists
that spelling. Other namespaces rely on the channel to report a stable form (the IRC services
account name, the WhatsApp number in E.164); the operator writes the identity the way the channel
reports it, which the `info` log of an unknown sender shows.

The owner list is tiny, so a linear scan over it, for the sender and then each alias, is cheaper
than building and hashing the joined strings.

## Routing

`Router::route(instance, conversation)` returns the agent of the first route whose every set key
matches: `channel` (the instance name), `kind`, and `conversation` (the id exactly as the channel
reports it). A route with no keys besides `agent` matches everything. The router holds a
`RouteList` (`routes.rs`), a shared, atomically replaced snapshot of the ordered routes, and reads
it once per message, so a change applies to the next message and nothing restarts.

### Stored routes

The routes are the `routes` table (`Docs/technical/storage.md`), not config. `routes::Routes`
(one per core, `Runtime::routes`) owns them:

- **Liveness.** `Routes::list()` is the `RouteList` the router holds. Every successful change
  reloads the stored list and replaces the snapshot. A snapshot, instead of a database read per
  message, keeps `classify` synchronous and off the database, including for messages it drops.
  One async lock spans read, validate (including that the agent exists, checked under the lock),
  write, and replace, so concurrent changes serialize and the snapshot always equals what is
  stored. Only this process writes the table. `hold_unrouted(agent)` takes the same lock for the
  caller if no route names the agent, which is how removing an agent in the web UI keeps a route
  from being made for it in between ([Web Channel](web-channel.md#removing)).
- **Operations.** `add(spec, before)`, `update(id, spec)`, `remove(id)`, `move_route(id, up)`,
  and `view()` (every route with what is wrong with it now). Routes have stable ids, so a stale
  page can't act on another route (`RouteError::NotFound`).
- **Validation**, like config: `RouteSpec` is text (blank is any), checked against the running
  configuration (`LiveConfig`): the channel instance and the agent exist, the kind is `direct` or
  `group`, the conversation has no control characters and is at most 512 bytes, the agent is
  required. A route that no earlier route hides is required too: `Unreachable` if earlier routes
  match everything it does (one route, or for a route with no kind, one covering direct and one
  covering group conversations), `Duplicate` if one matches exactly the same, `Hides` if the change
  would leave a later route unusable. Hiding is judged against the list before the change, so a
  route that was already unreachable (one imported from `giles.toml`, which never checked this)
  doesn't block unrelated changes; `view()` flags it, as it flags a route whose channel instance
  or agent is no longer configured, and `Routes::open` logs both at startup. Refusing is a
  deliberate choice over a warning: a route that can't match is almost always a mistake, and
  reordering keeps every step valid.
- **Import.** `giles.toml`'s `[[routes]]` stay in `Config::legacy_routes`, as written and
  unvalidated. `Routes::open` runs at `Runtime::start`: if the database hasn't recorded the import
  (`routes_imported`), it validates each rule like a route made in the web UI (shadowed routes
  are kept as written, but an empty or blank `channel`, `kind`, or `conversation` is an error, as
  it was in config), stores them in file order, and records the import, all or nothing; an
  invalid rule fails startup with `giles.toml: routes[<i>].<key>: <reason>`. A config with no
  `[[routes]]` still records the import, and so does an import that finds routes stored already.
  After that the rules are never read: not when the file changes, not when the owner has deleted
  every route (the record, not an empty table, says it ran), not when they have become invalid.
  While the file still has `[[routes]]`, startup logs `info` on the importing start and `warn`
  on every later one, saying they're ignored and to remove them. `giles status` doesn't report
  it: its response has no field for a warning (`config_error` is for reload errors), and a new
  proto field wasn't worth it for a line already in the log and on the routes page.

A web chat can be started with a chosen agent (the chat list's picker, TASKS.md 5.1). Its chat id,
which is the conversation id, is then `<agent>.<32 hex digits>` instead of 32 hex digits
(`channels::chat_ids`: `new_agent_chat_id`, `chat_agent`). For an event from the `web` component
with such an id, `classify` takes that agent ahead of every route, if `agents` (an
`AgentPolicies`, in the daemon the running `LiveConfig`) has it; otherwise the message is dropped
as `UnknownAgent`, never sent to a routed agent instead. The rule is only for the web component:
on any other channel such an id is an ordinary conversation id. Because `agents` is the running
configuration, an agent created in the web UI is reachable at once, with no route for it. Only the
owner gets past the sender check, and the web pages offer only the agents the user may use (v1:
the owner may use all of them).

## Core Commands

`commands::parse(text, is_pending)` recognizes a command only when it is the whole message, after
trimming whitespace, with words compared ASCII case-insensitively (phones capitalize):

| Text                                       | Result                                         |
| ------------------------------------------ | ---------------------------------------------- |
| `/new`                                     | `CoreCommand::New`                             |
| `/stop`                                    | `CoreCommand::Stop`                            |
| `yes`, `yes!`, `no`                        | `Approval { answer: Yes/YesForSession/No, code: None }`, if any approval is pending |
| `yes 4821`, `yes! 4821`, `no 4821`         | `Approval { code: Some("4821") }`, if that code is pending |

Anything else is ordinary text, including `/new please`, `/new@giles`, `/link` (no self-linking in
v1), unknown `/words`, `yes please`, and a `yes`/`no` the lookup doesn't claim. Codes are a single
token of ASCII letters and digits, passed to the lookup as typed. A message with attachments is
never a command, so an attachment can't be silently lost.

Whether `yes` is an approval reply depends on state the approvals code owns, so it's an input: the
`PendingApprovals` trait, `is_pending(to: &Routed, code: Option<&str>) -> bool`. It's implemented
for closures of that shape, by `NoPendingApprovals`, and (through `sessions::Approvals`) by
`approvals::ApprovalBoard`, which claims every reply while an approval waits for `to.user` in
`to.session`, and a reply naming one of theirs that closed recently ([Approvals](approvals.md)).
With nothing pending, `yes` and `no` are ordinary messages, so they can answer the agent's own
questions.

The commands' effects belong to the inbox and sessions code (`/new`), turn control (`/stop`), and
approvals (replies); [Sessions and Turns](sessions.md) records each command's event id as a dedupe
key before acting on it, so a redelivered command acts once.
