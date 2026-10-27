# Discord channel

The design of `giles-channel-discord`, the Discord channel plugin (PLAN sections 3.3, 8.5, and 10;
tracked in [issue #12](https://github.com/jennings/giles/issues/12)). A bot account receives direct
messages (DMs) over the Discord Gateway and sends over the REST API, as a separate process on the
plugin SDK ([Plugin SDK](plugin-sdk.md)).
`ChannelInfo { identity_namespace: "discord", output: Blocks, can_initiate: true }`. User-facing
setup, behavior, and settings:
[Discord](../user/discord.md).

**Status: designed (issue #13), built (issues #15 to #18: [Implementation](#implementation)),
packaged and documented (issue #19, [Packaging](#packaging)), and not yet run against Discord: issue #20
does that.** This page records the decisions of issue #13: the library, the Gateway intents, the
catch-up algorithm, the status mapping, the configuration, and the client seam the tests use, and
what each issue built on top of them. Facts about Discord were checked against the official
documentation on 2026-10-25 and link to it; where the documentation is silent or two pages disagree,
the text says so, and [What needs a real bot](#what-needs-a-real-bot) lists what issue #20 must
confirm.

## Decisions

| Question | Decision |
| --- | --- |
| Library | `twilight-gateway`, `twilight-http`, and `twilight-model` 0.17.1 (ISC), default features off, `rustls-webpki-roots` only. Serenity and a hand-written Gateway client are rejected ([The library](#the-library)) |
| Licences | No copyleft anywhere in the tree; nothing new for the repo's licensing. No owner decision needed |
| Intents | `DIRECT_MESSAGES` (`1 << 12`) only. **Not** `MESSAGE_CONTENT`: Discord delivers the content of DMs to the bot without it ([Intents](#gateway-intents)) |
| Missing `MESSAGE_CONTENT` | Not a failure mode. A defensive check reports three DMs in a row with no text, no attachment, and no sticker |
| Servers (guilds) | Not subscribed (no `GUILD_MESSAGES`), so guild messages never arrive and are never reported as groups |
| Conversation id | The DM channel id, kind `Direct` |
| Identity | `discord:<user id>` (the plugin sends the bare `<user id>`; the core adds the namespace), `sender_authenticated = true` ([Identity](#identity)) |
| Output mode | `Blocks`; no edits, no typing indicator ([Output](#output)) |
| Catch-up | A cursor per DM channel in the plugin's data directory, REST `after=<id>` paging after every new session, 24 h default age limit ([Catch-up](#catch-up)) |
| Status | Rejected token (close 4004, REST 401) is `NeedsAuth` with a `Url`; every other failure is `Error` ([Status](#status)) |
| Config keys | `token`, `max_blob`, `catch_up_max_age` ([Configuration](#configuration)) |
| Attachments | In: streamed from the CDN into blobs under `max_blob`, a note in the text for a file that can't be fetched. Out: uploaded as multipart files, 20 MiB each and 10 to a message ([Attachments](#attachments)) |
| Seam | A `Discord` trait with six methods ([Implementation](#the-seam-as-built)); the twilight types stay in one `real` module ([The client seam](#the-client-seam)) |

## The library

Candidates, as resolved on 2026-10-25 with Rust 1.99.1 (the repo's pinned toolchain) in a scratch
crate outside the repository, for `x86_64-unknown-linux-gnu`, normal (non-dev) dependencies only:

| | serenity | twilight | thin client |
| --- | --- | --- | --- |
| Crates | `serenity` | `twilight-gateway`, `-http`, `-model` | `tokio-tungstenite` + `reqwest` + `serde` |
| Latest release | 0.12.5, 2025-12-20: "the last release for the 0.12.x series" (its changelog) | 0.17.1, 2025-12-13 (gateway, http, model) | `tokio-tungstenite` 0.30.0, 2026-07-11 |
| Repository activity | `current` (the 0.12 line): last commit 2026-05-13. `next` (an unreleased, breaking 0.13): commits up to 2026-10-24 | `main`: commits up to 2026-10-20, 83 since the 0.17.1 tag | `tokio-tungstenite` pushed 2026-10-24 |
| MSRV | 1.74 | 1.89 | 1.85 |
| Licence of the crates | ISC | ISC | MIT (and the repo's existing tree) |
| TLS | `rustls_backend`: `reqwest` 0.12 on `ring`, plus `tokio-tungstenite` 0.21 on **rustls 0.22** and `rustls-webpki` 0.102 | `rustls` 0.23 with `webpki-roots`; needs a process-wide crypto provider | Whatever `giles_plugin_sdk::tls` builds: bundled roots, `aws-lc-rs` |
| Crates in the tree | 153 (27 not already in `Cargo.lock`) | 88 (11 new) | 127 (5 new, two of them `tungstenite` and `tokio-tungstenite`) |
| Cold dev build of the dependencies | 24 s | 17 s | 16 s |
| Gateway work done for us | Identify, heartbeat, resume, rate limit, close-code policy, in a `Client` that owns the loop and the event handler | The same, in `Shard`, a plain `Stream` | None: all of it is ours |
| REST rate limits | Built in | Built in (`twilight-http-ratelimiting`), 429 retried | Ours |
| API stability | Breaking `next` on the way; `current` frozen | 0.x, one breaking release a year; `main` is ahead | Ours |

Cold build times are one run each on the development machine (macOS, parallel build, debug profile,
dependencies only, with twilight's `zstd` and `decompression` features that the final choice turns
off), so they show order of magnitude only: none of the three is a compile-time problem.

**Decision: twilight** (`twilight-gateway`, `twilight-http`, and `twilight-model`, all 0.17.1).

- **Protocol correctness we cannot test here.** Nothing in this repository can reach Discord, and
  issue #20 happens last. The Gateway state machine (heartbeat jitter and ACK tracking, zombie
  detection, `resume_gateway_url`, the invalid-session rules, which close codes may reconnect, the
  Identify queue) is the part most likely to be subtly wrong, and a test double written from the
  documentation would only prove that the code agrees with its author. `twilight-gateway` is widely
  used for exactly these details (about 184 000 downloads in the 90 days before 2026-10-25), and a
  `Shard` is a plain `Stream`, so the plugin keeps control of its loop. REST rate limiting
  (per-route buckets from the `X-RateLimit-*` headers, the global limit, 429 retry) and a 401
  lock-out (after one `401 Unauthorized` the client refuses every later request, which the
  documentation asks for, see [Rate limits](#rate-limits)) come with `twilight-http`.
- **The smallest tree.** Eleven crates are new to `Cargo.lock`: six twilight crates (ISC),
  `ordered-float` 2, `serde-value`, `serde_repr`, `sha1_smol`, and `smallvec` 1. Gateway WebSockets
  reuse `tokio-websockets` 0.13, which the WhatsApp plugin's tree already contains.
- **Modern TLS with no duplicate stack.** `rustls` 0.23.46 and `rustls-webpki` 0.103.16, the
  versions already in the lock. Serenity 0.12.5 is also rejected for its TLS: `tokio-tungstenite`
  0.21 brings `rustls` 0.22.4 and `rustls-webpki` 0.102.8, and the RustSec database (checked
  2026-10-24) lists advisories against both with a fix only in the 0.23 and 0.103 lines:
  RUSTSEC-2026-0346 and -0349 (`rustls`) and RUSTSEC-2026-0049 and -0382 (`rustls-webpki`). The
  plugin would ship a vulnerable TLS stack until serenity 0.13 is released.
- **A narrow, typed surface.** The plugin needs `Intents`,
  `Event::{Ready, Resumed, MessageCreate, GatewayClose}`, `create_private_channel`,
  `channel_messages().after()`, `create_message()` with `allowed_mentions` and attachments, and the
  typed API error codes (`ErrorType::Response { error: ApiError::General(..) }` carries the 50007).
  That is a few hundred lines of glue, all in `real`.

What twilight costs, and how it is contained:

- **Trust store.** `twilight-gateway` builds its own TLS connector (`Connector::new()`, no hook to
  pass a `ClientConfig`), and `twilight-http` takes `webpki-roots`, not `webpki-root-certs`. Both
  are the Mozilla root program compiled in (CDLA-Permissive-2.0, a licence already in the tree),
  never the OS store, so the property that matters, "bundled roots, the same on every host", holds;
  only the crate that carries the list differs from `giles_plugin_sdk::tls`.
- **Crypto provider.** With default features off, `rustls` needs a process-default provider, and the
  lock holds both `ring` and `aws-lc-rs`, so rustls would refuse to guess (`Connector::new()` panics
  without a default provider). The plugin's `main` installs
  `rustls::crypto::aws_lc_rs::default_provider()` before anything connects, the same provider as the
  SDK.
- **Compression off.** `zstd` and `zlib` (Gateway transport compression, optional in Discord's
  protocol) are left out: DM traffic is a few small events, and `zstd` would add `zstd-sys`, a C
  build. The `decompression` feature of `twilight-http` (brotli) is off for the same reason.
- **API churn.** 0.x with a breaking release about yearly. Pin `0.17` (`Cargo.lock` pins the exact
  release) and confine every twilight type to `real`; a move to 0.18 then touches one module, as
  moving the `whatsapp-rust` pin touches only its `real/`.
- **Release lag.** The last release is ten months old while `main` is active. The risk is being
  stuck on 0.17.1 if upstream stalls, not breakage: the Gateway and REST protocol it speaks is
  versioned (API v10, which the plugin pins) and does not change underneath it.
- **A second HTTP client.** `twilight-http` is not a general HTTP client, so downloading a CDN
  attachment ([Attachments](#attachments)) uses `reqwest`, already a workspace dependency, with
  `tls_certs_only` and the SDK's roots like `giles_core::net::tls_client_builder`. The CDN is not
  the API, so it has no bucket to rate limit.
- **Strict models.** `twilight-model` fails a whole event it can't deserialize. A MESSAGE_CREATE
  that doesn't parse is logged by event type only (never the body) and costs the live copy of one
  message. A message of a history page that doesn't parse is skipped the same way, and paging goes
  by the raw page (its length and highest id), so one bad message never ends a pass early. Neither
  is fetched again later: the channel's mark moves on with the next message, and the channel id of a
  live event that doesn't parse isn't known to re-fetch it ([Catch-up](#catch-up)).

Feature set (one manifest, `plugins/giles-channel-discord`; nothing joins `[workspace.dependencies]`
except what a second crate would share):

```toml
twilight-gateway = { version = "0.17.1", default-features = false, features = [
    "rustls-webpki-roots",
] }
twilight-http = { version = "0.17.1", default-features = false, features = [
    "rustls-webpki-roots",
] }
twilight-model = "0.17.1"
```

A sketch of the glue that calls every item listed above type-checks against these versions in a
scratch crate outside the repository, so the names are real; it was not run against Discord.

### Rejected

- **serenity 0.12.5.** The TLS advisories above are enough on their own, and the rest agrees: a
  framework (`Client`, `EventHandler`, cache, command framework) built around owning the event loop,
  so the Gateway can neither be replaced by a fake nor polled from the plugin's own task without
  adapters; 27 new crates including a second `reqwest` and a second `rustls`; a release line
  declared finished while `next` rewrites the API.
- **A thin client on `tokio-tungstenite` and `reqwest`.** Attractive on paper: five new crates, the
  SDK's TLS config, and one HTTP client. It would also be the IRC plugin's answer (no library fit,
  so it uses `irc-proto` and `tokio-rustls`). It loses here for the protocol-risk reason above: we
  would write and unit-test heartbeats, resume, close-code policy, and bucketed rate limiting
  against our own reading of the documentation and learn about every mistake from a real bot, last.
  If twilight is ever abandoned or its TLS handling becomes a blocker, this is the fallback; the
  trait seam means only `real` changes. Its in-process fake Gateway would be a loopback WebSocket
  server (see [Testing](#testing)).

## Discord facts

Verified against the official documentation on 2026-10-25, with the page that says it. Prefix
`https://discord.com/developers/docs/` (the HTTP API is `https://discord.com/api/v10/`; the default
API version when none is given is still the deprecated v6, so every URL names `v10`).

| Fact | Source |
| --- | --- |
| Gateway connection: `Hello` (op 10) gives `heartbeat_interval`; first heartbeat after `interval × jitter`; then every interval; each answered by ACK (op 11); no ACK means a zombie connection to close with a code other than 1000 or 1001 and resume. Discord may request a heartbeat (op 1) | [`events/gateway`](https://discord.com/developers/docs/events/gateway#sending-heartbeats) |
| Identify (op 2) carries `token`, `intents`, `properties` (`os`, `browser`, `device`). The reply is `Ready` with `user`, `session_id`, `resume_gateway_url`, and `application` | [`events/gateway`](https://discord.com/developers/docs/events/gateway#identifying), [`gateway-events`](https://discord.com/developers/docs/events/gateway-events#ready) |
| **1000 Identifies per 24 hours** per bot, across shards, not counting Resumes. At the limit all sessions end, **the bot token is reset**, and the owner is emailed. One Identify per 5 seconds (`max_concurrency` 1); beyond that Discord answers Invalid Session (op 9) | [`events/gateway`](https://discord.com/developers/docs/events/gateway#identifying) |
| Resume (op 6) needs `session_id`, the last sequence `seq`, and a connection to `resume_gateway_url` with the original version and encoding. Discord replays missed events in order, then sends `Resumed`. The window is not documented as a number: a dropped connection (any close except 1000 or 1001) "will remain active and timeout after a few minutes". Closing with **1000 or 1001 invalidates the session** and shows the bot offline | [`events/gateway`](https://discord.com/developers/docs/events/gateway#resuming) |
| Invalid Session (op 9) with `d: false` means disconnect and Identify again; `d: true` means resume. Reconnect (op 7) means reconnect and resume | [`gateway-events`](https://discord.com/developers/docs/events/gateway-events#invalid-session) |
| Close codes: **4004** authentication failed (do not reconnect); **4013** invalid intents (no); **4014** disallowed intents (no); 4010 invalid shard, 4011 sharding required, 4012 invalid API version (no); 4000 to 4003, 4005, 4007 to 4009 can reconnect (4007 invalid `seq`, 4009 session timed out: start a new session) | [`topics/opcodes-and-status-codes`](https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-close-event-codes) |
| Gateway send limit 120 events per 60 seconds per connection; payloads at most 4096 bytes | [`events/gateway`](https://discord.com/developers/docs/events/gateway#rate-limiting) |
| `DIRECT_MESSAGES` is `1 << 12` (4096): `MESSAGE_CREATE`, `MESSAGE_UPDATE`, `MESSAGE_DELETE`, `CHANNEL_PINS_UPDATE` for DMs. Standard intent, no portal toggle. `GUILD_MESSAGES` is `1 << 9`. `MESSAGE_CONTENT` is `1 << 15`, privileged: passing it without enabling it in the Developer Portal closes the connection with 4014 | [`events/gateway`](https://discord.com/developers/docs/events/gateway#list-of-intents) |
| **Without `MESSAGE_CONTENT`, a bot still gets the content of "DMs with the app"** (also of its own messages and of messages that mention it). Everything else arrives with empty `content`, `embeds`, `attachments`, and `components`. The intent "permits your app to receive message content data across the APIs", which suggests the exemption covers REST history too (to confirm, [item 1](#what-needs-a-real-bot)) | [`events/gateway`](https://discord.com/developers/docs/events/gateway#message-content-intent), [You Might Not Need a Privileged Intent](https://github.com/discord/discord-api-docs/blob/main/developers/gateway/you-might-not-need-a-privileged-intent.mdx) (the "Exceptions" list names "Direct Messages sent to your app") |
| The message object page carries a general warning that content fields are empty without the intent and does not repeat the DM exception: the two pages do not disagree, but the warning alone would mislead | [`resources/message`](https://discord.com/developers/docs/resources/message#message-object) |
| `POST /users/@me/channels` with `recipient_id` creates a DM channel, and "if one already exists, it will be returned instead"; opening many DMs too fast gets the bot rate limited or blocked (error 40003 "You are opening direct messages too fast") | [`resources/user`](https://discord.com/developers/docs/resources/user#create-dm), [error codes](https://discord.com/developers/docs/topics/opcodes-and-status-codes#json) |
| **50007 "Cannot send messages to this user"** is the error for a user who can't be DMed. Others used here: 10003 unknown channel, 10013 unknown user, 40005 request entity too large, 50001 missing access, 50006 empty message, 50013 missing permissions, 50035 invalid form body, 50045 file exceeds the maximum size | [error codes](https://discord.com/developers/docs/topics/opcodes-and-status-codes#json) |
| Create Message: `content` up to **2000 characters**; at least one of `content`, `embeds`, `sticker_ids`, `components`, `files[n]`, `poll`; `allowed_mentions` field; maximum request size **25 MiB**; files as `multipart/form-data` with `files[n]` and a `payload_json` | [`resources/message`](https://discord.com/developers/docs/resources/message#create-message) |
| Upload limit "applies to each file in a request. The default limit is **20 MiB** for all users, but may be higher for users depending on their Nitro status or by the server's Boost Tier". The documentation does not state a limit on files per message | [`reference`](https://discord.com/developers/docs/reference#uploading-files) |
| `allowed_mentions`: default for regular messages is `parse: ["users", "roles", "everyone"]`; `{"parse": []}` allows no mention to ping. Discord validates it against the content and the bot's permissions | [`resources/message`](https://discord.com/developers/docs/resources/message#allowed-mentions-object) |
| Get Channel Messages: `before`, `after`, `around` are mutually exclusive; `limit` 1 to 100 (default 50); returns "from newest to oldest". Snowflakes are time-ordered: `(id >> 22) + 1420070400000` ms, and "`0` works" as the start of time; a snowflake can be built from a timestamp for pagination | [`resources/message`](https://discord.com/developers/docs/resources/message#get-channel-messages), [`reference`](https://discord.com/developers/docs/reference#snowflake-ids-in-pagination) |
| Attachment URLs are signed (`ex` expiry, `is` issued, `hm` signature) and expire; "when your app receives a payload with a signed URL (like when you fetch a message), it will be valid" | [`reference`](https://discord.com/developers/docs/reference#signed-attachment-cdn-urls) |
| Rate limits: per route, with `X-RateLimit-{Limit,Remaining,Reset,Reset-After,Bucket}`; a 429 carries `Retry-After`, `retry_after`, `global`, and `X-RateLimit-Scope` (`user`, `global`, `shared`). Global limit **50 requests per second**. Do not hard-code limits. Invalid requests (401, 403, 429 except `shared`) beyond 10 000 per 10 minutes get the IP banned by Cloudflare; the page asks apps to stop making requests once a token is invalid | [`topics/rate-limits`](https://discord.com/developers/docs/topics/rate-limits) |
| Typing: `POST /channels/{id}/typing`, expires after **10 seconds**; "Generally bots should **not** use this route" except while answering a command that takes a few seconds | [`resources/channel`](https://discord.com/developers/docs/resources/channel#trigger-typing-indicator) |
| Requests need a `User-Agent` of the form `DiscordBot ($url, $versionNumber)` or may be blocked by Cloudflare | [`reference`](https://discord.com/developers/docs/reference#user-agent) |
| Message types: `0` DEFAULT and `19` REPLY are user-authored text; the rest are system messages (joins, pins, calls, and so on) | [`resources/message`](https://discord.com/developers/docs/resources/message#message-object-message-types) |
| `MESSAGE_CREATE` carries `guild_id` only for guild messages; the author has `id`, `bot`, `username`, and `global_name` | [`gateway-events`](https://discord.com/developers/docs/events/gateway-events#message-create) |
| Bots and terms: "Developers must abide by the terms of service, which includes refraining from automating standard user accounts (generally called self-bots) outside of the OAuth2/bot API" | [`topics/oauth2`](https://discord.com/developers/docs/topics/oauth2#bot-vs-user-accounts), [Developer Terms of Service](https://support-dev.discord.com/hc/articles/8562894815383-Discord-Developer-Terms-of-Service) |

Not in the API documentation at all, so treated as unverified: that a user can DM a bot only if they
share a server with it, that a bot can DM a user only if they share a server and the user allows DMs
from server members, and any numeric limit on DMs a bot may open. Bots cannot list their DM channels
(the user resource has no such endpoint; there is only Create DM), which shapes the
[catch-up](#catch-up).

## Gateway intents

The plugin identifies with **`DIRECT_MESSAGES` only**.

- A bot receives DM messages with their text without `MESSAGE_CONTENT` (the facts table above, two
  pages). The Developer Portal toggle is therefore not needed, 4014 cannot be caused by the plugin,
  and the owner has nothing to enable. This changes the premise of issues #15, #16, and #19, which
  assumed the privileged intent.
- Not requesting `MESSAGE_CONTENT` is also the policy-conforming choice: Discord says to "only
  enable a privileged intent if your use case requires it to function".
- `GUILD_MESSAGES` is not requested, so no server message is ever delivered. The core drops group
  conversations (PLAN section 1.1) and the plugin has no reason to receive them: they would arrive
  with empty content anyway (the bot is rarely mentioned), they cost traffic, and a "group" event
  that the core always discards adds nothing. Issue #16 asked to decide this;
  **guild messages are not reported at all.** A `MESSAGE_CREATE` that carries a `guild_id`
  regardless (the intent set changes, a library quirk) is dropped before it becomes an event.
- `MESSAGE_UPDATE` and `MESSAGE_DELETE` arrive with `DIRECT_MESSAGES` (one intent covers all three)
  and are ignored: an edit or a delete after delivery doesn't change what the agent was asked
  ([Inbound](#inbound)).
- No presence, no compression, no `shard`, no `capabilities`. Identify `properties` are the
  library's.

### Detecting a missing intent

Issue #16 asked how to detect a missing `MESSAGE_CONTENT`. With the intent set above there is
nothing to detect, but the documentation's DM exception is the one fact the whole design leans on
and it is checked only against documentation, so the plugin guards it cheaply:

- A DM from a person with no text, no attachment, and no sticker is dropped (nothing to answer), at
  `debug`.
- **Three such DMs in a row** set `Error` (the text is in [Status](#status)), and the next DM that
  is delivered sets `Connected` again. (The sketch said "reset by a DM with text"; a DM with only a
  file or a sticker proves the same thing, that Discord is sending content, so any delivered DM
  resets the count.) The message is honest about the cause being a guess (polls, forwards, and other
  content the plugin doesn't deliver also arrive empty), and it makes a changed Discord behaviour
  visible instead of silent. It does **not** tell the operator to enable `Message Content` in the
  Developer Portal, as the sketch did: the plugin never requests that privileged intent, so the
  toggle alone would change nothing, and asking for it when it isn't enabled closes the connection
  with 4014. It says to report it.

## Identity

- Identity `discord:<user id>`: the author's snowflake from the Gateway event, immutable, unlike the
  username, global name, and the retired discriminator. The owner lists `discord:<id>` in
  `[owner] identities`. `sender_display_name` is `global_name`, falling back to `username`; it is
  display only and never used for routing.
- **The plugin deals in native ids, the core in identities.** `InboundMessage.sender` is the bare
  user id (`82198898841029460`), as the WhatsApp plugin sends `+15551234567` and not
  `whatsapp:+15551234567`. The core joins it to the namespace `Configure` returned (`discord`) to
  get the identity it matches against `[owner] identities` (`OwnerIdentities::contains`), and strips
  the namespace from an identity before it hands a proactive send's `recipient` to `Send`
  (`outbox::route`). A plugin that put `discord:` in the sender would arrive as
  `discord:discord:<id>` and be dropped as not the owner (issue #22). The same holds in the other
  direction: the `recipient` is the bare user id too.
- **`sender_authenticated = true`.** The author id is set by Discord, not by the sender: it arrives
  on the Gateway connection that the bot authenticated with its token over TLS, and in a DM the
  author is either the bot or the one other participant. A message can't claim another user's id the
  way a mail header or an IRC mask can. The assumption it rests on is that the bot token stays
  secret and Discord is honest, the same trust as any channel's server. (Webhook messages, which can
  carry arbitrary names, don't occur in DMs, and the plugin drops any message with a `webhook_id`
  anyway.)
- Namespace `discord`. User ids are global across Discord, so a second bot instance would report the
  same identity for the same person.

## Conversation id

The DM channel id (a snowflake as a string), `ConversationRef { kind: Direct, title: None }`.
`POST /users/@me/channels` is idempotent per recipient ("if one already exists, it will be returned
instead"), and DM channels persist, so the id is stable per (bot, user) pair in practice; Discord
does not promise it in so many words, and the plugin does not depend on it: it caches the
user-to-channel mapping, and an unknown-channel error (10003) on a send drops the cache entry and
asks Discord again once ([Send](#send)). Issue #20 checks stability across a restart.

## Inbound

One task reads the shard and produces `ChannelEvent`s in order, pure mapping in `inbound`.

| Condition | Result |
| --- | --- |
| `author.bot`, or `author.id` equals the bot's own id (from `Ready`) | Dropped: no echo of the plugin's own messages, no bot-to-bot loops |
| `webhook_id` set, or `guild_id` set | Dropped |
| `type` other than `0` (DEFAULT) and `19` (REPLY) | Dropped (system messages) |
| No text, no attachment, no sticker | Dropped; counts toward the three-in-a-row check above |
| Otherwise | `InboundMessage` |

An `InboundMessage` has: event id and `message_id` both the message id (snowflakes are globally
unique, so unique within the instance and stable across restarts, PLAN section 8.5: the core
deduplicates on it for 30 days), `reply_to` the referenced message id of a REPLY, `sent_at` the
message timestamp, `text` the `content`, `attachments` as blobs ([Attachments](#attachments)).
Stickers become `[sticker: <name>]` appended to the text; embeds are ignored (a user's own message
carries its links in `content`); other content (polls, forwards) is not delivered. Files are
downloaded into blobs, and one that can't be is an `[attachment not received: ...]` line at the end
of the text ([Attachments](#attachments)). Edits and deletes are ignored: an edit or a delete after
delivery doesn't change what the agent was asked, and a message edited while the plugin was down
arrives with its edited text.

Durability is the SDK's: it writes each event to its spool before the plugin's stream is polled
again, and resends unacked events when `Events` reopens
([Plugin SDK](plugin-sdk.md#events-and-persistence)). The plugin keeps no spool of its own, and only
the cursor below.

Status events use ids `status:<nanos>:<counter>`, as the other plugins do, and are only sent when
the status changes.

## Catch-up

The Gateway has no acknowledgement. What survives a drop is Resume: Discord replays everything after
`seq` for "a few minutes" (no number is documented), and the library does this itself. What does not
survive is a restart of the plugin (the session lives in memory; persisting `session_id`, `seq`, and
the resume URL would only help a restart inside those few minutes, and the catch-up the plugin needs
anyway covers the same gap) or an outage longer than the window. The plugin therefore treats
**every new session as possibly having missed messages**, and catches up over REST.

State, in `<data dir>/discord-state.json` (the SDK's own state is `giles-sdk/`; this file is the
plugin's), written by a temp file, `fsync`, and rename:

```json
{
  "version": 1,
  "dm_channels": {
    "319674150115610528": { "cursor": "1234567890123456789", "user": "82198898841029460" }
  }
}
```

- `cursor`: the highest message id of that channel the plugin has finished handling (delivered to
  the SDK's spool, or dropped). `user`: the other participant, for resolving a user id to a channel
  without asking Discord. Both are strings, because snowflakes exceed what JSON numbers carry
  safely, and ids compare as `u64`.
- Losing the file is harmless: the channels are rediscovered by live messages and by sends, and
  catch-up refetches at most `catch_up_max_age`, which the core deduplicates by event id (30 days).
  Corrupt or unreadable state is renamed `.corrupt` and logged, as the SDK does for events, never
  deleted.

**When it runs.** After `Ready` (a new session: first start, a restart, an invalid session, a
session timed out) for every channel in the state file, one channel at a time.
**Not after `Resumed`**: the Gateway replayed the gap itself, in order. The Gateway session is
established first, so any message created after that moment arrives live, and REST only has to cover
what came before it. A channel seen for the first time (a live message from an unknown channel, or a
channel the plugin just created for a send) gets the same catch-up before its first message is
delivered, with no cursor (below), so a user's earlier messages are not skipped just because the
first one the plugin saw live was the third.

**The algorithm**, per channel:

1. Mark the channel `CatchingUp`. Live events for it are buffered in memory (in order) instead of
   delivered.
2. `after` is the later of the channel's high-water mark (which starts at the cursor) and the
   snowflake of `now − catch_up_max_age` (`(ms − 1420070400000) << 22`), so a cursor older than the
   limit starts at the limit and a channel with no cursor starts there too.
3. `GET /channels/{id}/messages?after=<after>&limit=100`. The page, newest first on the wire, is
   handed over oldest first (`real` sorts it). Each message goes through the same inbound mapping as
   a live one; a message older than `catch_up_max_age` (which Discord shouldn't send for this
   `after`) is not delivered, and the cursor passes it. After each message the cursor candidate
   becomes its id. `after` for the next request is the largest id Discord sent in the page
   (unreadable messages included). A page with fewer than 100 messages sent ends the loop; a hard
   cap of 100 pages (10 000 messages) also ends it with a warning, and the messages after them, up
   to the live ones, are skipped (paging goes forward).
4. Drain the buffer: every buffered live event with an id above the channel's high-water mark is
   handled; the rest (already fetched) are duplicates and dropped. Mark the channel `Live`.
5. In `Live`, events with an id at or below the high-water mark are dropped (a replayed duplicate
   after a resume), the others are handled, and the high-water mark moves up.

**Deduplication.** One mark per channel, the highest id handled, decides: ids are time-ordered per
channel, REST returns each message once per page, and the buffer step above makes live events and
fetched ones meet without a gap or a repeat. Across restarts there are two more layers, so nothing
needs to be perfect: the SDK drops an event whose id is already pending in its spool, and the core's
inbox deduplicates (instance, event id) for 30 days. `catch_up_max_age` is validated at most 30 days
for that reason.

**Moving the cursor.** The same rule as the email plugin's UID cursor
([Delivery and the cursor](email.md#delivery-and-the-cursor)): the SDK writes each event to disk
before polling the stream again, so the persisted cursor moves to a message's id when the stream is
next polled. A message that is dropped (a bot's, an empty one) moves the cursor too, but in order:
it goes through the same queue as the events, so the cursor never passes a delivered message the SDK
hasn't persisted. A crash between the SDK's write and the state write refetches that one message,
with the same event id, which the spool or the core deduplicates. An event the SDK took and never
persisted (it was stopping, or its disk failed) leaves the cursor behind it, and the next catch-up
fetches it again.

**Bounds, and what is lost.**

- Page size 100; at most 100 pages per channel per pass; `catch_up_max_age` (default 24 hours,
  `"0s"` turns catch-up off): older messages are skipped, with a count logged at `info` (no
  content).
- A failed fetch (network, 5xx, 429 beyond the bounded wait) is retried while the session lasts,
  after 1 second, then 2, doubling to a minute. The channel's live events stay buffered, to a cap of
  1000, after which new live events are discarded and the channel is caught up again when the pass
  finishes (the cursor never passed them). One channel's failing fetch doesn't hold up the others:
  its retry waits while the next channel's pass goes on, and channels that are already live deliver
  at once. A 4xx that asking again can't fix (a channel that is gone, one the bot can't see) skips
  that channel's pass with a warning and releases its buffer. A 401 is the auth failure of
  [Status](#status): `NeedsAuth`, no more requests, and every channel goes live.
- Lost for good: messages older than the age limit or beyond the page cap; messages deleted before
  the fetch; a message edited while the plugin was down arrives with its edited text, and later
  edits are ignored; **messages from a user the plugin has never seen, sent while it was down, if
  that user sends nothing afterwards** (the plugin can't list DM channels, and a first live message
  is what reveals the channel). A `users` config key that opens the owner's DM channel at start
  would close that last gap; it needs ids the plugin doesn't have (`[owner] identities` is the
  core's), so it isn't in v1.
- Messages the bot sent while it was offline can't exist; the plugin's own messages in a page are
  dropped by the author check.

## Output

- **`OutputMode::Blocks`.** `Deltas` exists only for the web channel (`Channel::deltas()`, PLAN
  section 6.9). Streaming into Discord would mean creating a message, then editing it repeatedly to
  append text. Edits are rate limited per route with a dynamic bucket (Discord says not to hard-code
  it), a streamed answer crosses the 2000-character limit mid-sentence, and a turn that is stopped
  would leave a half-edited message. Each assistant block is one `send`; long ones are split.
- **Rendering and splitting** are the SDK's Discord renderer (issue #14): CommonMark to Discord
  markup, split at 2000 characters without breaking fences. The limit is documented in "characters";
  whether Discord counts Unicode scalars or UTF-16 units for characters outside the Basic
  Multilingual Plane is not, so issue #20 sends a 2000-character message of emoji and the renderer
  counts scalars until that shows otherwise.
- **No typing indicator.** `POST /channels/{id}/typing` lasts 10 seconds and Discord says bots
  generally should not use it. The `Channel` trait gives the plugin no signal that a turn started or
  ended (it sees an inbound message and later a `send`), so a typing call on arrival would show
  "typing" for ten seconds and stop while the agent works for minutes, which is worse than no
  indicator. Revisit if the core adds a turn signal to the trait.

## Send

`send` for a `Conversation(<DM channel id>)` target posts to that channel. For a
`Recipient(<user id>)` target (the outbox, jobs, proactive sends; the core has taken the `discord:`
namespace off the identity) it resolves the channel first: the state file's `user` mapping, else
`POST /users/@me/channels`, whose result is recorded there (so a restart remembers it, and catch-up
covers the channel).

- Each chunk of the rendered text is one `POST /channels/{id}/messages`, in order, sequentially,
  with `allowed_mentions: { "parse": [] }` and the `SUPPRESS_EMBEDS` flag always (no user, role, or
  `@everyone` can ping and no link unfurls, whatever the model wrote and whatever the renderer
  escaped: the renderer is the first layer, these are the second) and no `message_reference`:
  `reply_to` is ignored, because in a DM a reply link only adds clutter. The returned `MessageId` is
  the last chunk's.
- A caller that sends text with attachments gets the text as the content of the first message of
  files when it fits one message, and in the chunks before the files when it doesn't
  ([Attachments](#attachments)).
- Every REST call is bounded: `twilight-http` queues behind the rate limiter and sends a request
  that got a 429 back through it (the code has no retry count), so `RealDiscord` gives up after 60
  seconds per call with `TimedOut` (5 minutes for a message with files, which goes through a second
  `twilight-http` client with that timeout and the main client's rate limiter, so the library's own
  10 second timeout stays what it is for everything else).

| Outcome | Result of `send` |
| --- | --- |
| Success | `Ok(message id)` |
| 401 (`ErrorType::Unauthorized`; the library refuses every later request) | `Unavailable` ("Discord rejected the bot token") and the `NeedsAuth` status |
| 50007, 50278, 10013, 40003 resolving a recipient or sending | `Unavailable`: DMs closed or blocked, no shared server, a user that doesn't exist, DMs opened too fast. The outbox retries on its own schedule; the plugin has no retry loop |
| 10003 on a recorded channel | Forget it (state file), create the DM channel again once, retry the chunk; a second 10003 is `NotFound` |
| 10003 on a `Conversation` target | `NotFound` |
| 40005, 50045 (file too large), 50006, 50035 | `InvalidRequest`, with Discord's status and error code (not its text): a plugin bug or an oversize send |
| A recipient that isn't a bare snowflake (including `discord:<id>`, which the core never sends) | `InvalidRequest` |
| 60 s without an answer (`TimedOut`), 5xx, transport error | `Unavailable` |
| Any other error code (50001, 50013, ...) | `Unavailable` too: the outbox retries until the message expires, which costs a request now and then, and a message is never dropped for a code the plugin doesn't know |

If a send fails after some chunks went out, the outbox sends the whole message again, and Discord
has no idempotency key to make that harmless, so the plugin remembers how many chunks of an
unfinished send Discord accepted and a retry carries on after them ([Sending](#sending)). Discord's
`nonce` with `enforce_nonce` was considered instead and rejected: `twilight-http` 0.17.1 has `nonce`
but no `enforce_nonce` to set, Discord deduplicates only for a few minutes while the outbox retries
for a day, and a nonce derived from the text would also suppress two genuine identical replies
("ok", twice).

## Attachments

Issue #18 built both directions. Discord's facts are in [Discord facts](#discord-facts); how the
code is laid out is in [Attachments in the code](#attachments-in-the-code).

**Limits.** Each is a named constant or a config key, and each is enforced by the plugin before
Discord is asked, because the plugin can say what is wrong and Discord's answer to some of them is
only a status.

| Limit | Value | Where it comes from | Enforced |
| --- | --- | --- | --- |
| File size, in | `max_blob`, default 50 MiB | Config | The declared `size` before the request; the CDN's `Content-Length` before the body; the bytes as they arrive, whatever either said |
| File size, out | `min(max_blob, 20 MiB)` (`FILE_LIMIT`) | "The default limit is 20 MiB for all users", higher only with Nitro or a server's boost tier ([Uploading Files](https://discord.com/developers/docs/reference#uploading-files)); a bot in a DM has neither | The size the core gave the blob, and the bytes read, before anything is posted |
| Request size, out | 25 MiB (`REQUEST_LIMIT`), less 64 KiB kept for the text and headers | "The maximum request size when sending a message is 25 MiB" ([Create Message](https://discord.com/developers/docs/resources/message#create-message)) | Files are grouped into messages that fit it |
| Files per message, out | 10 (`FILES_PER_MESSAGE`) | The documentation states **no limit**; 10 is the figure Discord's own clients and libraries use. Real Discord decides ([item 3](#what-needs-a-real-bot)) | Files beyond it go in follow-up messages |
| Files per message, in | 10 (`MAX_FILES`) | The same figure; a message with more is not Discord's | The rest are notes, not downloaded |
| Time, in | 10 s to connect, 30 s of silence, 5 min in all | The plugin | `reqwest` timeouts |

### Inbound

- An attachment's `url` is a signed CDN URL (`ex`, `is`, `hm` query parameters), valid when the
  message arrives live or is fetched, and expiring afterwards, so the plugin downloads at once:
  before the message is handed to the SDK, so a message and its files are one event.
- `reqwest` (see [The library](#the-library)), with the SDK's bundled roots, streams the body into
  `Blobs::put_reader`: the plugin holds a chunk and the blob store's 64 KiB buffer, never a file,
  and `max_blob` is counted as bytes arrive (`client::limited`), whatever size Discord or the CDN
  declared. A file the declared size or `Content-Length` puts over the limit is never requested or
  read.
- The media type is Discord's `content_type` when it is a `type/subtype` (parameters dropped,
  `application/octet-stream` ignored), else the one the file's extension says (a table of about
  thirty common types), else `application/octet-stream`. The plugin does not sniff content: the
  SDK's other plugins don't either. The blob's name is Discord's file name with control characters
  removed, `/`, `\` and `"` replaced, cut to 100 characters.
- **A file that doesn't arrive is a note**, appended to the text after the words and the stickers
  (`[attachment not received: photo.png is over the limit of 50 MiB]`), and the message goes on:
  over the limit (declared or counted), expired or deleted (403, 404, 410: "is no longer available
  from Discord"), any other failure (a status, a timeout, a body cut short, a URL the client won't
  fetch: "could not be downloaded"), a blob store that can't keep it ("could not be stored"), and
  the eleventh file of a message. A failed download leaves no blob. Retrying would not help (the
  link expires and the message is delivered once), and a note is honest where silence is not. The
  note quotes the name the way stickers are quoted: control characters and brackets removed, so a
  name can't end its note and start a line of its own.
- **Embeds are ignored; stickers are `[sticker: <name>]`** ([Inbound](#inbound)): a user's own links
  are in `content`, and an embed is Discord's preview of one.
- **Never logged:** the URL, its query string, the file name. A log line says "an attachment was not
  received", the CDN host, and the reason in words. `tests/logs.rs` captures `tracing` at `TRACE`,
  with the `log` records of `hyper` and `reqwest` bridged in, around downloads through the plugin
  and through the real client (a success, an expired link, a redirect, an oversize body, a refused
  URL), and checks that no line has the signature, the query, or the name.
- **Only Discord's CDN.** A URL in a message is a stranger's data: the client refuses (`BadUrl`,
  nothing requested) any URL that isn't `https` on `cdn.discordapp.com` or `media.discordapp.net`,
  port 443, with no user name or password; that rules out every address and `localhost`, so a
  crafted attachment can't make the plugin request its own network (SSRF). A redirect is followed to
  such a URL only, at most 3 times; any other is not followed and is an error. No `Authorization`,
  cookie, or `Referer` is ever sent: the bot token must not reach the CDN.

### Outbound

- A send's files are the blobs the core put for it (`PutBlob`), read from the SDK's blob store when
  the message that carries them is about to be posted, never all at once: at most one message's
  files (25 MiB) are in memory, and the retry after the 10003 of a stale channel clones `Bytes`, not
  bytes.
- **Refused before anything is posted**, whole, with `InvalidRequest` naming the file, its size, and
  the limit: a file over `min(max_blob, 20 MiB)` by the size the core gave it. The bytes read are
  checked again (a blob that is larger than it said is the same error). A blob that isn't in the
  store is `InvalidRequest` too (the core uploaded it for this very send; it is not coming back).
- **Messages.** Text that fits one message is the content of the first message of files, so a file
  arrives with its words. Longer text is posted first, one message per part as before, and the files
  follow in messages of their own with no text ("text too long for one message goes before the
  files"). Files go ten to a message and only while they fit 25 MiB (so two files of 12 MiB are one
  message, and three are two). A send of files alone has no text. A send with neither text nor files
  is `InvalidRequest("nothing to send")`.
- **The upload.** `POST /channels/{id}/messages` as `multipart/form-data` with `payload_json` (the
  content, `allowed_mentions: {"parse": []}`, `flags: 4`, and an `attachments` list of ids and file
  names) and a `files[n]` part per file with its name and its media type. Every message of files has
  the same two protections as a text one, in the same place, by the client and not by the caller.
  `twilight-http`'s own multipart writer sets no `Content-Type` on a file part, so the plugin writes
  the body (`real::multipart`) and hands it to the library as a raw request, which keeps
  authorization, rate limiting, the 401 lock-out, and error parsing. The file name is the blob's
  name, cleaned, or `attachment` and the extension of its type; the part's media type is the blob's
  `type/subtype`, or `application/octet-stream` when it isn't one (a header can't carry anything
  else).
- **Order, locking, and what a failure means** are the text path's: one send at a time, each message
  after Discord accepted the one before, and an unfinished send to a user resumes after the messages
  Discord took ([Sending](#sending)). A message of files is a part like any other: the send's
  identity is a hash of every message's text and the content ids, types, and names of its files, so
  the same words with another file are another send. An upload is bounded at 5 minutes (a text call
  at 60 seconds), because 25 MiB takes time on a slow line.
- **Errors.** 413, 40005, and 50045 are `InvalidRequest` (the plugin's own check should have caught
  it: a limit that is not what the documentation says is a finding for issue #20); the rest as in
  the table under [Send](#send).

Two things this design does not do: it doesn't convert or shrink a file that is over the limit (an
image over 20 MiB is refused, not resized: the agent can say so and try again), and it doesn't split
a file across messages.

## Rate limits

- **REST.** `twilight-http` handles it: a per-route bucket from `X-RateLimit-Bucket` and the limit
  headers, the 50 requests per second global limit, and a retry after a 429. The plugin's traffic is
  a handful of requests per minute, far below any limit. Discord asks that a 401 is never repeated
  (10 000 invalid requests in 10 minutes means a Cloudflare ban of the IP); the library refuses
  everything after the first 401, and the plugin goes to `NeedsAuth` and stops calling REST until it
  is restarted with a new token.
- **What `twilight-http` does, read in its source (0.17.1) and shown by `tests/rest.rs`.** Every
  request first takes a permit from the limiter, which holds it back until the route's bucket has
  room (from the `X-RateLimit-*` headers of the earlier answers) and the global 50 per second has
  room. A 429 with the user scope refills the bucket from its headers (`X-RateLimit-Reset-After`)
  and the same request is sent again through the limiter: the test sees the second request arrive
  after the reset and not before. A burst of answers that say "bucket empty" spaces the next
  requests by the reset, so the chunks of a long reply arrive in order, each after the one before
  (the plugin sends them one at a time anyway). Two limits of the library matter: it
  **retries a 429 for as long as Discord keeps answering one**, so the plugin's 60 second bound per
  call (`TimedOut`) is the only bound; and it reads bucket information
  **only from answers that carry `X-RateLimit-Scope`**, so a 429 without it (or with `global`) is
  retried at once, limited only by the global 50 per second. Discord documents the header with the
  others, so a real 429 should have it; issue #20 checks. The library's 10 second timeout per HTTP
  request (not counting the wait for a permit) is left at its default, and a timed out request may
  have created its message, which a retry then repeats.
- **Gateway.** 120 sends per 60 seconds; the shard sends only heartbeats, Identify, and Resume, and
  queues them itself. The 1000 Identifies per 24 hours matter more: exceeding them
  **resets the token**. Resume costs none, so a drop never Identifies unless Discord says the
  session is gone.
- **Reconnect backoff.** `twilight-gateway` waits `2^attempt` seconds (to about four minutes) after
  a failed connect, but **resets the count to zero on every close**, so a server that accepts the
  connection and closes it again at once (a bad intent set, a flapping edge) would be retried every
  second, and each retry may Identify. The plugin therefore delays: after any `GatewayClose`, it
  waits before polling the shard again, 1 second doubling to 5 minutes, and resets the delay after a
  session has stayed up for a minute. A shard never polled doesn't reconnect, so this is a plain
  `sleep` in the plugin's loop. A fatal close ends the stream (`None`); the plugin does not create a
  new shard for the same reason it does not retry a 401.

## Status

`ChannelStatus` has `Connected`, `NeedsAuth { challenge, expires_at }`, and `Error { message }`; the
challenge is a QR code, a pair code, or a URL, and carries no text. A missing token is not a status
at all: it fails `Configure` with `INVALID_ARGUMENT` ("invalid config: token: ..."), like every
config error.

| Event | Status | Then |
| --- | --- | --- |
| `Ready` (new session) or `Resumed` | `Connected` (still `NeedsAuth` if REST rejected the token) | Catch-up runs after `Ready`, not after `Resumed` |
| Close **4004** (authentication failed) | `NeedsAuth { Url("https://discord.com/developers/applications") }` | Final: no reconnect until the plugin is restarted with a new token |
| REST **401** on any call (a send or a history request) | `NeedsAuth` (the same) | Not final: all REST stops (sends refuse, catch-up asks for nothing more and every channel goes live), but the Gateway session goes on, live messages still arrive, and the status stays `NeedsAuth` through drops and reconnects |
| Close **4014** (disallowed intents) or **4013** (invalid intents) | `Error`: "Discord refused the intents the plugin requests (close 4014/4013); this is a bug or a change at Discord, please report it" | Final. With `DIRECT_MESSAGES` only, neither can happen unless Discord changes DM intent rules. Not `NeedsAuth`: the owner can't fix it |
| Close **4010**, **4011**, **4012** | `Error` with the code and its meaning (4011: the bot is in too many servers to run unsharded) | Final |
| Any other close (4000 to 4003, 4005, 4007 to 4009, 1006 and other transport closes, no close frame), Reconnect (op 7), Invalid Session (op 9), a missed heartbeat ACK, a failed connect | Nothing if `Ready` or `Resumed` follows within **10 seconds** (the grace WhatsApp uses); otherwise `Error`: "disconnected from Discord, reconnecting" | The library resumes or re-identifies; the plugin's backoff above applies |
| Three empty DMs in a row | `Error`: "Discord is sending direct messages with no text, files, or stickers, so the plugin can't read them; Discord may have changed what a bot receives without the Message Content intent, please report it (polls and forwards look the same)" | Said again for each further empty DM (not repeated to the SDK); cleared by the next DM that is delivered |
| 429, 5xx, timeout on a send or a catch-up page | None; the send returns `Unavailable` | Retried by the outbox or the pass (1 s doubling to 60 s) |
| Plugin stopping | None | The shard is closed with 1000 (the bot goes offline); the stream ends |

The 4004 case also covers a token Discord reset after the Identify limit. The `NeedsAuth` URL is the
Developer Portal's applications list, where the Bot page has "Reset Token"; the
[user guide](../user/discord.md#status) says so, and says how to put the new token into
`secrets.toml`. The new token takes a restart of Giles, not of the plugin: the core resolves secrets
once, when it starts ([Registry and Startup](registry.md#the-running-configuration)), so a plugin
restart relaunches it with the old token. A repeat of the last status is not sent again. No status
carries the token, a user id, or message text.

## Configuration

A `[channel.discord]` instance's `config` table, validated at `Configure` with the key named in the
error (`invalid config: token: ...`), unknown keys refused like the other plugins:

| Key | Default | Meaning |
| --- | --- | --- |
| `token` | required | The bot token (`{ secret = "..." }` in practice). A credential: `Secret`-wrapped, never logged, `<redacted>` in `Debug`, never in a status. Non-empty, no whitespace or control characters (a pasted `Bot` prefix is refused with a message to paste the token alone) |
| `max_blob` | `"50 MiB"` | The largest attachment downloaded (a larger one becomes a note in the text) or sent (a send with a larger one is refused); sends are also capped by Discord's 20 MiB per file |
| `catch_up_max_age` | `"24h"` | How far back catch-up fetches. `"0s"` turns it off; at most `"30d"`, the core's deduplication window |

Nothing else is configurable on purpose: the API version, the intents, the page size, the grace
period, and the backoff are constants. The plugin's sandbox needs outbound TCP 443 only (REST, the
Gateway, and the CDN are all HTTPS or secure WebSocket on that port), plus its data directory.

## Packaging

The binary is in the container image: `giles-channel-discord` is in the `bins` list of the
`Dockerfile`, and the image smoke test checks that the file is there
([Container image](container-image.md), [Continuous integration](ci.md)). No image-level change is
needed beyond that, because of how plugins start:

- The core launches a plugin only for a configured instance (`[channel.<name>]` with
  `component = "discord"`, [Plugin host](plugin-host.md)), so an image with no Discord instance
  never starts the plugin and never contacts Discord. The CI smoke test configures none.
- The plugin has no configuration of its own outside `giles.toml`: a missing or malformed `token`
  fails `Configure` and the instance stays down with the message in the log and in `giles status`.
- Its one file, `discord-state.json`, lives in the instance's data directory
  (`data/plugins/<instance>/`, which is on the `/data` volume), next to the SDK's own state
  (`giles-sdk/`).

The image itself was not built when this was written (the development machine has no Docker); CI's
image job builds it and runs the smoke test.

## The client seam

Everything above the `real` module talks to Discord through one trait, in the style of the WhatsApp
plugin's `Backend`. The plugin's own types cross it (`DiscordMessage`, `Attachment`,
`RestError`, ...), never twilight's, so the tests can build every case by hand and a library change
touches only `real`.

```rust
#[async_trait]
pub(crate) trait Discord: Send + Sync + 'static {
    /// The Gateway: the next item of one long-lived connection manager (reconnects and resumes
    /// are its business). `None` after a fatal close or `shutdown`.
    async fn next_event(&self) -> Option<GatewayEvent>;
    /// Post one message to a channel, files included, with no mention allowed.
    async fn create_message(&self, channel: ChannelId, message: NewMessage)
        -> Result<MessageId, RestError>;
    /// `POST /users/@me/channels`.
    async fn create_dm(&self, user: UserId) -> Result<ChannelId, RestError>;
    /// `GET /channels/{id}/messages?after=<id>&limit=<n>`, oldest first.
    async fn messages_after(&self, channel: ChannelId, after: MessageId, limit: u8)
        -> Result<Vec<DiscordMessage>, RestError>;
    /// Stream an attachment from the CDN (a signed URL), failing early on a declared size over `max`.
    async fn download(&self, url: &str, max: u64) -> Result<Download, RestError>;
    /// Close the Gateway cleanly (code 1000) and stop.
    async fn shutdown(&self);
}
```

- `GatewayEvent`: `Ready { user_id }`, `Resumed`, `Message(DiscordMessage)`, and
  `Closed { code: Option<u16>, fatal: bool, reconnecting: bool }`. A `Message` is the plugin's own
  struct: id, channel id, author (`id`, `bot`, `username`, `global_name`), `guild: bool`,
  `webhook: bool`, `kind`, `content`, `attachments` (`url`, `filename`, `content_type`, `size`),
  sticker names, timestamp, and the referenced message id. Update, delete, and every other Gateway
  event never leave `real`.
- `RestError`: `Unauthorized`, `Api { status, code }` (the Discord error code), `TimedOut` (a call
  that outlasted the bound: a slow answer or a rate limit that doesn't clear, which the client can't
  tell apart), `TooLarge`, and `Transport`. `inbound` and `outbound` map these to the SDK's errors
  and to statuses; the table under [Send](#send) is tested against them.
- Time is `tokio::time`, so tests pause and advance it for the grace period, the backoff, and the
  catch-up retry. Data directory and clock are injected like the other plugins'.

`real` implements `Discord` on a `twilight_gateway::Shard` (one task, its events forwarded through a
channel, close frames mapped, the backoff sleep) and a `twilight_http::Client`, plus `reqwest` for
downloads, and the pure conversions from twilight types to the plugin's (tested from JSON fixtures
of the documented payloads, so a model change that breaks parsing fails a test).

## Testing

No test may contact Discord, and none needs a token.

- **Fake client** (`FakeDiscord`): a scripted Gateway (a queue of `GatewayEvent`s the test pushes,
  plus a record of `shutdown`) and scripted REST (`messages_after` pages, `create_message` and
  `create_dm` results per call, with injected `RestError`s). Covers, with paused time: config
  validation (table); status transitions for every row of the table above, including the grace
  period; reconnect backoff doubling, capping, and resetting; token redaction in `Debug` and in
  every status and error; shutdown closing the client; the DM mapping and every drop rule; event
  order; persistence before acknowledgement and no cursor move when the SDK fails to persist;
  catch-up (cursor, first sighting, pagination over two pages and a short page, dedup against live
  events buffered during the pass, the age limit, the page cap, a failed page retried, no catch-up
  after `Resumed`); the empty-DM counter; send (chunk order, `allowed_mentions` always empty, the
  recipient path with a cached channel, a closed DM, an unknown user, a malformed id, the 10003
  retry, 429 bound, no message sent after a 401); and attachments (download within, over, and after
  expiry; upload over the limit; ten files and an eleventh).
- **Through the SDK**: an integration test in the style of the IRC and WhatsApp plugins: the plugin
  served over a socket pair with the fake client, a DM in becomes an event, a `Send` goes out, and
  the core's `Events` reopen redelivers an unacked one.
- **Pure units** next to the logic: snowflake arithmetic, state-file round trip and corruption, the
  status mapping, error mapping, config parsing.
- **`real`, offline.** Its conversions are unit-tested from fixtures. Its loop is driven without
  Discord: `ConfigBuilder::proxy_url` points a shard at `ws://127.0.0.1:<port>`, and a Gateway
  server written in the test plays Hello, Ready, a close with 4004, and so on, which checks the glue
  (close-code mapping, resume, the clean close, a DM becoming a message) without the network.
  Issue #15 built it ([Implementation](#running-the-tests)). Its REST counterpart, a plain HTTP
  server on loopback reached with `ClientBuilder::proxy(.., use_http = true)`, checks the history
  requests, their errors, and a 429 (issue #16), and with both servers the whole inbound path runs
  through `RealDiscord` ([Running the tests](#running-the-tests)).

## Implementation

What issues #15 to #18 built: the crate, the configuration, the Gateway connection, and the status
(#15); direct messages in, the state file, and catch-up after downtime (#16); text out, to a
conversation or to a user, and the handling of a rejected token (#17); and attachments in both
directions (#18). Issue #19 packaged and documented the plugin ([Packaging](#packaging)).

`plugins/giles-channel-discord` is a library and a binary (`publish = false`, `MIT OR Apache-2.0`
like the workspace; the new dependencies are listed under [Dependencies](#dependencies)). The
library exists so that the integration tests in `tests/` can reach the channel and the fake client,
as the IRC plugin's do; nothing in it is an API for anyone else.

### Modules

| Module | Contents |
| --- | --- |
| `config` | `Config` as written (unknown keys refused) and `Settings` as validated; `Secret`, whose `Debug` is `<redacted>` |
| `client` | The `Discord` trait and `GatewayEvent`: the seam |
| `status` | `classify`: a Gateway event to `Up`, `Down`, or `Final(ChannelStatus)`; the status messages |
| `inbound` | `map`: a `DiscordMessage` to a `Delivery` (an `InboundMessage` and the files to download into it), `Empty`, or `Dropped(reason)`; pure ([The mapping](#the-mapping)) |
| `ingest` | `Ingest`: the per-channel marks, buffers, and catch-up passes, merging live and fetched messages; no I/O ([Ingest](#ingest)) |
| `state` | `StateFile`: `discord-state.json`, the DM channels, their cursors and users ([The state file](#the-state-file)) |
| `snowflake` | Snowflake arithmetic: the time of an id, the first id of a time |
| `outbound` | `Outbound`: `send` for `DiscordChannel`: target parsing, rendering, the messages in order, finding or opening the DM channel, the error mapping, and the progress of unfinished sends ([Sending](#sending)); `outbound::plan` (the limits, and which files go in which message) |
| `deliver` | The task between the session and the SDK: downloads a message's files into blobs and notes the ones that fail ([Attachments in the code](#attachments-in-the-code)) |
| `media` | The SDK's `Blobs` as a `MediaStore`, and the media types and file names of attachments |
| `session` | `Session`, `Timing`, and `run`: the task that owns the connection and the history request in flight ([State machine](#state-machine)) |
| `token` | `TokenState`: whether Discord rejected the token, shared by the session and everything that calls REST ([A rejected token](#a-rejected-token)) |
| `channel` | `DiscordChannel` (the SDK's `Channel`) and the event stream that moves the cursor, `configure`, `configure_with`, and the `SessionSlot` that lets `main` stop the session |
| `fake` | `FakeDiscord`, the scripted client for tests: a Gateway queue, a REST history per channel, a CDN, and the messages posted; `FakeMedia`, a blob store in memory |
| `real` | `RealDiscord`, the client on `twilight-gateway` and `twilight-http`: the only module with twilight types; `real::cdn` (downloads on `reqwest`) and `real::multipart` (the body of a message with files) |
| `main` | Logging, `ChannelPlugin::new("discord", ..).output(Blocks).can_initiate(true).run(..)`, then the session is stopped |

```text
twilight Shard ── real ──GatewayEvent──▶ session ──Staged──▶ deliver
      ▲                                   │   ▲                │
      └── polled again only after ────────┘   │                ├─ Discord::download ──▶ MediaStore
          the reconnect delay                 │                │   (a message's files into blobs)
                      Ingest: marks, buffers, catch-up passes  │
                      real ◀── messages_after ── session       ▼
                                                    Outgoing ──▶ Channel::events ──▶ SDK
                    (the cursor moves in the state file when the stream is polled again)
```

### The seam as built

[The client seam](#the-client-seam) lists the six methods the finished plugin needs, and the trait
has all six: `next_event` and `shutdown` (the Gateway connection, issue #15), `messages_after`
(channel history for catch-up, issue #16), `create_message` and `create_dm` (sends, issue #17), and
`download` (attachments, issue #18), each added with its `real` implementation, its fake, and its
tests by the issue that calls it. Ids are plain `u64`s (the sketch's `ChannelId`, `MessageId`, and
`UserId` would only rename them). Deliberate differences from the sketch:

- `create_message` takes a `NewMessage` of the text and its files (`NewFile`: a clean name, a
  `type/subtype`, and `Bytes`, which a retry clones without copying).
  `allowed_mentions: { "parse": [] }` and the `SUPPRESS_EMBEDS` flag are part of the contract of
  `create_message`, not fields of `NewMessage`: the real client puts them on every message, with
  files or without, and `tests/rest.rs` and `tests/upload.rs` check the request that goes out. Only
  the `id` is read from the response of `create_message` and `create_dm`: the library's models are
  strict, and a message Discord accepted must not turn into an error, and a duplicate on retry,
  because another field changed. The library refusing the request itself (a text over its limit) is
  `Api { 400, 50035 }` and a 413 is `TooLarge`.
- `download` returns the body as a stream of `Bytes` chunks (`Body`), not a `Download`: the status
  and the declared length are checked before it returns, so the stream is only a body. It is **not**
  cut off at `max` by the client: the caller wraps it in `client::limited(body, max)`, which
  replaces the chunk that crosses the limit with `TooLarge` and ends the stream. One helper enforces
  the limit for every client, the fake included, so the guarantee of the plugin ("never more than
  `max_blob` bytes, whatever the sender says") is tested once against the fake and doesn't depend on
  a client behaving. `RestError` has a variant more than the sketch: `BadUrl`, for a URL the client
  refuses to request.
- `GatewayEvent` is `Ready { user_id }`, `Resumed`, `Message(DiscordMessage)`, and
  `Closed { code: Option<u16> }`. `Closed` has no `fatal` or `reconnecting` flag: whether a close
  code is final is the plugin's policy (`status::classify`, the table under [Status](#status)),
  tested against the fake, rather than a flag the client sets. The client agrees with it by
  construction: the codes the policy calls final (4004, 4010 to 4014) are the ones
  `twilight-gateway` ends its stream on, and a stream that ends without one is an error status of
  its own.
- `messages_after` returns a `Page { messages, received, last }`. `messages` are the ones the plugin
  could read, **oldest first** whatever the order on the wire (`real` sorts them, so no caller has
  to know Discord sends them newest first). `received` and `last` are the count and the highest id
  of the raw JSON list: the library's models are strict, so a message that doesn't deserialize is
  skipped with a warning, and paging goes by what Discord sent. A full page with one unreadable
  message is still full, and the next request starts after its highest id. `RestError` is as the
  sketch says (`Unauthorized`, `Api { status, code }`, `TimedOut`, `TooLarge`, `Transport`); a call
  that outlasts the plugin's 60 seconds (the library queues behind a rate limit and sends a 429
  again without limit) is `TimedOut`, and an error response the library can't parse as Discord's
  JSON error (a proxy's HTML page) is `Transport`, since the library keeps no status for it.
- `next_event` can be dropped while pending without losing an event (the session does, to stop), and
  a client that isn't polled doesn't reconnect: both are properties of `twilight-gateway`'s `Shard`
  that the trait states as its contract.

### The state file

`state::StateFile` is the plugin's one persistent file, `<data dir>/discord-state.json`, in the
format under [Catch-up](#catch-up) (`version` 1; ids as strings). It is a cloneable handle on a map
from DM channel id to `{ cursor, user }`, shared by everything that needs it:

- `advance(channel, message)` moves a cursor forward (never back) and adds the channel if it is new.
- `record_dm(channel, user)` adds a channel, or sets its user, without touching the cursor. It is
  what a send calls after Create DM, so that a channel first made for a send is known to the next
  catch-up pass and to later sends (and to a restart), and what the inbound path calls for the first
  message of a channel it hasn't seen.
- `forget_dm(channel)` removes a channel: Discord said it doesn't exist (10003) when a send used the
  recorded one.
- `channels()`, `get(channel)`, and `channel_for_user(user)` read it.

Every change that changes something is written before the call returns (a call that changes nothing
writes nothing): a temporary file in the same directory, `fsync`, rename over the state file,
`fsync` of the directory. The write is synchronous: the file is a few kilobytes, and the calls come
from the one task that hands events to the SDK and from the sends. A failed write is logged at
`warn` and the change stays in memory, so the next change writes it again; the worst case is a
cursor that is behind after a crash, which costs a few messages fetched twice and deduplicated. A
file that doesn't read, doesn't parse, has another `version`, or holds an id that isn't a number is
renamed to `discord-state.json.corrupt` (replacing an older one) and the plugin starts empty, never
deleting it. Unknown fields are ignored.

### The mapping

`inbound::map(message, bot_id)` applies the table under [Inbound](#inbound) in its order: a bot
author, the plugin's own id (from `Ready`; a message before `Ready` can't be checked against it), a
webhook, a server, and a type other than 0 and 19 are `Dropped` with that reason, then a message
with no text (after trimming), no file, and no sticker is `Empty`, and everything else is an
`InboundMessage`:

- `sender` is the author id alone (the native id: the core adds the `discord:` namespace),
  `sender_authenticated` is `true`, and the display name is the global name or else the username.
  The conversation is the channel id, `Direct`, with no title.
- The event id and `message_id` are the message id, and `reply_to` is the referenced message of a
  type 19 message only (a forward or a crosspost refers to a message too).
- `text` is the content exactly as written, then one line per sticker, `[sticker: <name>]`. Names
  come from the sender: control characters and brackets are removed and the name is cut to 100
  characters, so a name can't close its note and start another line of its own. The files of the
  message travel beside it in the `Delivery`; `deliver` downloads them into `attachments` and
  appends a note for each that fails ([Attachments](#attachments)).

### Ingest

`ingest::Ingest` holds what catch-up needs and has no I/O: the session feeds it Gateway events and
the answers to the history requests it asked for, and it returns what to hand over (`Out`: `Deliver`
an event with the cursor to move once the SDK has persisted it, `Advance` a cursor for a message
that is done with, `Status`). Per DM channel it keeps the high-water mark (the highest id handled,
starting at the state file's cursor) and a mode, `Live` or `CatchingUp` with a buffer of live
messages (at most 1000, then it discards and remembers that it did). The algorithm under
[Catch-up](#catch-up) is this module, with these details:

- **Calls.** `on_ready(bot)` (a new session): every known channel, the ones in the state file and
  the ones the process has seen, becomes `CatchingUp` with a queued pass, lower id first; passes of
  the session before are dropped, buffers are kept. `on_message(message)` (live). `next(now)` says
  what to do about history: `Now(request)`, `Wait(until)` for a request in its retry delay, or
  `Idle`. `on_page(request, result, now)` takes the answer; one that matches no queued request (the
  pass was restarted meanwhile) is ignored.
- **First sight.** A message of a channel the process doesn't know is mapped first: from a bot, the
  plugin itself, a webhook, or a server it is ignored and teaches nothing; from a person (even a
  system message) it records the channel and the user in the state file, marks the channel
  `CatchingUp`, queues a pass, and buffers the message. The pass starts at the state file's cursor
  if the channel has one (a channel a send created has none) and at the age limit if not.
- **A pass.** One request at a time, 100 messages, `after` the later of the mark and the first id of
  `now − catch_up_max_age`. Each message of the page goes through the same handling as a live one;
  the page's largest id is the next `after`; a short page, or the 100th page, ends it. Then the
  buffer is handed over (ids at or below the mark were fetched already), and the channel is `Live`,
  or another pass is queued if live messages were discarded.
- **Handling a message.** At or below the mark: ignored (a replay, or fetched already). Above it:
  the mark moves, and the message is delivered, or counted as empty, or dropped (both move the
  cursor in order, as `Advance`).
- **The empty-message check** lives here too: a count of empty DMs in a row, reset by any message
  that is delivered, and the `Error` and `Connected` statuses ([Status](#status)). It counts across
  channels and across live and fetched messages, in the order they are handled.
- **Failures.** A request that fails for a reason that can go away (transport, 429 that outlasted
  the bound, 5xx) gets a retry time `1 s × 2ⁿ` after the nth failure in a row, at most a minute; the
  queue goes on with the next channel whose retry time has come. A 4xx that won't go away skips the
  pass; a 401 reports `NeedsAuth`, stops all requests (also for channels first seen later), and
  releases every buffer. `Timing` holds the two delays, so tests run on paused time.
- **Time.** The age limit reads an injected `Clock` (milliseconds since the Unix epoch), not
  `tokio::time`: the limit is about when Discord created a message, which paused test time can't
  say.

### State machine

`session::run` is one loop over five things: the stop signal, the grace timer, the history request
in flight, the retry timer of a queued request, and the pending `next_event` call. It reacts to what
`status::classify` says about each event:

| Event | Reaction |
| --- | --- |
| `Message` | Hand it to `Ingest`, and send what that returns to the channel's stream (`Outgoing`: an event, a cursor to move once the SDK has persisted it, or both). A full buffer (the SDK is persisting) holds the session until there is room or until it is stopped; Gateway heartbeats go out only while the client is polled, so a persist that takes a minute drops the connection, which resumes, and replays what was missed |
| `Ready` or `Resumed` | Report `Connected` (a repeat of the last status is not sent), clear the drop timer, note when the session came up. `Ready` is a new session and `Resumed` the old one; both are `Connected`, and the log says which. After `Ready` the request in flight is dropped and `Ingest` starts every channel's pass over; after `Resumed` nothing is caught up |
| History answered | Hand it to `Ingest` and send what that returns. The next request is started at the top of the loop |
| `Closed` with a code the client recovers from (or no code) | Start the grace timer if none is running. Reset the delay to 1 s if this session had been up for a minute. Do not poll the client until the delay is over, then double the delay (cap 5 minutes). A history request in flight goes on: REST doesn't need the Gateway |
| `Closed` with 4004 | Report `NeedsAuth` (the Developer Portal URL) and stop polling for good |
| A send reported a 401 (`TokenState::reject`), in any state, even after another final status | Drop the request in flight, release every channel waiting for history (`Ingest::on_token_rejected`), and report `NeedsAuth`; the Gateway is polled as before. The same as a 401 on a history page |
| `Closed` with 4010 to 4014 | Report an `Error` that says what happened and stop polling for good |
| The stream ended (`None`) with no final close before it | Report an `Error` ("restart this plugin") and stop polling for good: nothing starts another shard, for the reason in [Rate limits](#rate-limits) |
| The grace timer ran out (10 s after the first close with no `Ready` or `Resumed`) | Report `Error` "disconnected from Discord, reconnecting"; the client is still trying |
| Stop | End the pending poll and the request in flight (nothing of it was handled, and the cursor didn't move), call `shutdown` on the client once, bounded by 5 s |

"For good" means until the plugin is restarted: the session keeps its status, polls nothing, and
waits to be stopped. A bad token therefore costs Discord one Identify, not a stream of them, and an
operator who fixes `secrets.toml` and restarts the instance gets a new process, a new client, and
`Connected`. All the delays are in `Timing` (`Default` is what the plugin uses), and the clock is
`tokio::time`, so the tests run on paused time and check the exact delays.

### The library's silent reconnect

`twilight-gateway` reports a close frame, a transport error, a missed heartbeat ACK, and a failed
connect, but not a connection that ends with a plain TCP close and no close frame: its `Shard` notes
the disconnect and starts reconnecting inside the same `poll_next`, and nothing comes out of the
stream. The plugin would see no event to report or to pace, and Discord would see a new connection a
second later. `real` therefore compares `Shard::state()` before and after every poll: a shard that
was connected and is now disconnected, with no message to show for it, is reported as
`Closed { code: None }`. The loopback test `a_dropped_connection_is_resumed_with_the_same_session`
fails without it.

One case the state comparison cannot see: a connection that ends *before Hello*. The shard goes
`Disconnected` to `Identifying` (the WebSocket is up) and back to `Disconnected` inside one
`poll_next`, so the state is the same before and after and nothing is reported: no `Closed`, so no
grace timer and no `Error` status, and the plugin does not delay (the library does, below). The
shard retries about once a second. Nothing is sent to Discord in that loop, since Identify and
Resume answer Hello, so the daily Identify budget is not at risk; the cost is a status that stays as
it was, for a server that accepts connections and hangs up at once.

Also worth knowing, because both interact with the plugin's delay: `Shard` sleeps 1 second before
every connect (`2^attempts` seconds, and `attempts` counts failed connects only), and its Identify
queue spaces Identifies five seconds apart. After a close frame the shard's sleep starts when the
plugin polls again, so the first reconnect takes about 2 seconds (the plugin's 1 second, then the
library's 1). After a plain TCP drop the shard's sleep starts at once, inside the poll that reported
the drop, so it overlaps the plugin's delay and the first reconnect takes about 1 second; the delay
keeps doubling from there. An Identify loop is bounded at one per five seconds by the library, and
by the plugin's delay to one per 5 minutes in the end. Reconnect requests (op 7) and invalid
sessions (op 9) are handled by the shard, which sends a close frame (code 1000 for a session that
can't be resumed, so it Identifies again; 4000 otherwise, so it resumes) and surfaces Discord's
answering close as a `Closed` event; the plugin relies on that answer to count them.

### A rejected token

Discord asks apps to stop making requests once a token is invalid, and `twilight-http` refuses every
request after its first 401. The plugin makes that one state for every REST caller and the Gateway,
a `TokenState` that the channel creates and shares with the session:

- A 401 from a send (`reject`) wakes the session, which drops its history request, lets every
  channel waiting for one go live (`Ingest::on_token_rejected`), and reports `NeedsAuth`. The
  wake-up is stored, so it isn't lost if the session is busy, and it works after another final
  status too (the `NeedsAuth` replaces the `Error`, since the owner can act on it).
- A 401 on a history request makes `Ingest` report `NeedsAuth` and stop fetching, and the session
  marks the token rejected (`note_rejected`), so sends stop too.
- A 4004 on the Gateway is final (the session polls nothing more) and marks the token rejected the
  same way, so sends stop.

So a 401 on REST, of either kind, has one policy: all REST stops and the owner is told, and the
Gateway session, which Discord hasn't said anything against, goes on, so live messages still arrive.
The status is `NeedsAuth` whichever way it comes, and it **stays** `NeedsAuth` while the token is
rejected: a new Gateway session (`Ready` or `Resumed`) does not announce `Connected` over it, and a
drop that outlasts the grace period does not announce "disconnected" over it, since the sends stay
refused until the plugin is restarted with a new token. A repeat of the last status isn't sent
again.

Callers of REST that send check the state before they send, and send nothing once it is set.

### Sending

`channel::DiscordChannel::send` is `outbound::Outbound::send`, a function of the `Discord` client,
the `TokenState`, and the `StateFile` only, so every case below is a test against `FakeDiscord`.

- **Targets.** A `Conversation` id must be a nonzero number that fits 64 bits; a `Recipient` the
  same: the core strips the namespace before it calls `send`, so a recipient is a bare user id.
  Anything else, including an identity with its namespace (`discord:8219...`) and another namespace
  (`whatsapp:+1555...`), is `InvalidRequest` before any request is made. `reply_to` is ignored.
- **Text.** `render::discord(markdown, DISCORD_MAX_CHARS)`; nothing left after rendering (empty or
  whitespace) with no attachment is `InvalidRequest("nothing to send")`; files are
  [Attachments](#attachments). No typing indicator ([Output](#output)).
- **Recipients.** The DM channel with a user is `StateFile::channel_for_user`, which the inbound
  path fills from live messages and catch-up, else `create_dm`, whose answer is recorded with
  `record_dm`: a restart remembers it, and the channel is known to catch-up from then on (the next
  `Ready` fetches what the user wrote before their first live message). A recorded channel is
  trusted until a message to it fails with 10003 on the first part: it is forgotten (`forget_dm`),
  Discord is asked again once, and the part goes to the new channel; a second 10003 is `NotFound`. A
  failed `create_dm` records nothing.
- **Order.** One send runs at a time (a lock held for the whole send, and the token check made
  inside it), the parts one after the other, each only after Discord accepted the one before. A part
  that waits behind a rate limit therefore can't be overtaken by a part of another message, and a
  failure stops the send: later parts are never posted after a missing one. The fake test with two
  concurrent sends fails without the lock.
- **Failure after some parts.** The error is `Unavailable` (or `InvalidRequest` or `NotFound` for an
  error that retrying doesn't fix) and its text says `N of M messages had already been sent` and,
  for a send to a user, which the outbox retries, `a retry carries on after them`. The outbox
  repeats the whole message until it is delivered or its row expires after 24 hours. `Outbound`
  therefore keeps a map of unfinished sends to a user (`outbound/unfinished.rs`), keyed by the
  destination and a hash of the rendered parts, holding the number of parts Discord accepted. It is
  updated right after each accepted part (so a send the outbox drops at its 60 second timeout leaves
  its progress too), removed when the last part is accepted (so a genuine second reply with the same
  text goes out in full), and forgotten when the send fails in a way a retry can't fix
  (`InvalidRequest`, `NotFound`: the outbox drops the row). It holds at most 64 sends, evicting the
  one with the oldest progress, and each entry expires 24 hours after its last progress. A retry
  skips the accepted parts, so the user sees each part once. A part Discord keeps refusing with an
  error the plugin treats as retryable (an unlisted code, such as 240000 for a blocked link) is
  retried alone, one request per outbox retry: about once a minute once the outbox's backoff reaches
  its 60 second cap, until the row expires after 24 hours; the outbox's expiry is the stop.
  **A reply into a conversation is not remembered**: the core's reply path only logs a failed send
  and nothing retries it, so nobody would carry on, and an entry would only hold one of the 64 slots
  for a day and make an identical later reply skip parts it never posted. Its error says how many
  parts went out and promises no retry. What remains: a send that Discord accepted but whose answer
  was lost (a timeout) is posted again by the retry; the map is in memory, so after a restart the
  retry posts the earlier parts once more; and an identical multi-part send to a user made while an
  earlier identical one is still unfinished carries on that one.
- **Bounds.** `RealDiscord` bounds each REST call at 60 seconds (`TimedOut`, `Unavailable` here).
  The outbox gives a whole send 60 seconds as well, so for a proactive send the outbox gives up
  first and drops the future, which is as safe as a failure (after some parts it is the case above).
  The bound is what ends a reply into a conversation, which has no outbox. A send queued behind
  another one waits for it.
- **The token.** A 401 from `create_message` or `create_dm` is `Unavailable` ("Discord rejected the
  bot token") and rejects the `TokenState` ([A rejected token](#a-rejected-token)): the session
  reports `NeedsAuth`, and every later send fails at once with the same error and no request. A 401
  on a history request or a 4004 on the Gateway does the same to sends.
- **Logs.** A failed send logs one `warn` with the error text above and the counts: never the
  message, the user, the channel, or the token. `tests/logs.rs` sends a distinctive text through the
  plugin and through the real client against a server that refuses and rejects it, at `TRACE`, and
  checks that neither the text nor the token is in any line, the library's included.

### Shutdown

`ChannelPlugin::run` owns the channel and gives no hook for its end, and a process that exits when
`run` returns would cut the session off before it closes the Gateway, leaving the bot shown online
until Discord times the connection out. As in the WhatsApp plugin, `configure` leaves a
`SessionHandle` in a `SessionSlot` that `main` holds; after `run` returns, `main` stops the session
and waits up to 6 seconds for it. The session ends the pending poll and calls `Discord::shutdown`,
bounded by 5 seconds. `RealDiscord::shutdown` sends close code 1000 (which ends the session and
shows the bot offline) and polls the shard until Discord's answer arrives, if there is a session to
end, for at most 900 milliseconds. The limit is under the second the library sleeps before every
connect: if the peer ends the TCP connection without answering, the shard starts reconnecting and
would Identify, putting the bot back online after it was shut down, and the wait is over before that
can happen (`shutdown_does_not_reconnect_when_the_close_is_never_answered`). With no session (never
connected, a rejected token) it does nothing, and the connection is dropped with the process.

### Configuration and the token

`Config` derives `Deserialize` with `deny_unknown_fields`, and `Settings::new` validates it; every
error is `invalid config: <key>: <reason>`. `token` is a `Secret`: no `Display`, `Debug` prints
`<redacted>`, and the one use of its value is `RealDiscord::new` handing it to the library (whose
own `Config` also redacts it). The validation errors never repeat it, a pasted `Bot` prefix gets its
own message, and `tests/logs.rs` captures `tracing` at `TRACE` around two runs and asserts the token
is in no line: the plugin served by the SDK with the fake client, and the real client against the
loopback Gateway through Identify, a dropped connection, Resume, and a rejected token (the library's
own logs are in the capture). `max_blob` and `catch_up_max_age` are validated and logged at startup;
`catch_up_max_age` is read by catch-up, and `max_blob` by the downloads and the sends
([Attachments](#attachments)): the limit of both directions, capped for uploads at 20 MiB.

`RealDiscord::new` installs `rustls`'s `aws-lc-rs` provider as the process default (a provider that
is already installed is left alone), because `twilight-gateway` panics building its TLS connector
without one. The design said `main` would do it; doing it where the shard is built means no caller
can forget it, and `tests/crypto_provider.rs`, a binary of its own so that no other test can have
installed the provider first, checks it.

### Attachments in the code

- **Inbound.** `inbound::map` returns a `Delivery`: the `InboundMessage` (text, no attachments) and
  the message's files. `Ingest` passes both on in `Out::Deliver`, the session stages them
  (`deliver::Staged`) into a bounded channel, and the `deliver` task, which `DiscordChannel::start`
  spawns, downloads one file at a time through `Discord::download` and `client::limited` into
  `MediaStore::put_reader` (the SDK's `Blobs::put_reader`, which writes a temporary file, hashes it,
  and deletes it if the reader fails), and then passes the item to the event stream. The session
  task never waits for a download, so the Gateway keeps being polled (and heartbeating) while one
  runs, and the order of messages is the order they were staged. `session::messages::attachments`
  tests it against the fake, including a held download.
- **Outbound.** `Outbound::send` renders the text, checks the files against the limits
  (`outbound::plan::check`), turns text and files into `Post`s (`plan::plan`), and posts them under
  the same lock, with the same resume logic, as the text parts: `unfinished::Key` hashes the posts.
  Each post's files are read just before it is sent. `RealDiscord::create_message` sends a JSON body
  without files and a `real::multipart` body with them.
- **Tests.** `outbound/tests/attachments.rs` (the fake: success, names and types made safe, ten and
  eleven files, the request limit, text before files, oversize by declared size and by bytes,
  `max_blob`, a missing blob, Discord's too-large answers, failure midway and the retry, another
  file being another send, a reply's error, concurrent sends, a 401), `outbound/plan.rs`,
  `media.rs`, `real/multipart.rs`, and `real/cdn.rs` (unit tests), `tests/cdn.rs` (the real client
  against a CDN on loopback: no credentials, other hosts never contacted, redirects, status codes, a
  declared length, an endless body cut at the limit with the connection dropped, a truncated body,
  silence, a stall, a slow drip, a missing server), `tests/upload.rs` (the multipart request:
  headers, authorization, the JSON part, each file byte for byte, 413 and Discord's codes, a 401, a
  429 waited out), `tests/sdk.rs` (the real blob store over the wire: a download fetched with
  `GetBlob` and released by the ack, a note for a dead link, a `PutBlob` send uploaded, an oversize
  one `INVALID_ARGUMENT`), and `tests/logs.rs` (no URL, signature, or file name at `TRACE`, with the
  `hyper` and `reqwest` records bridged in).
- **Dependencies.** `reqwest` (already in the workspace; HTTP/1.1 and TLS with the SDK's roots) for
  downloads, `bytes`, `fastrand` (the multipart boundary, already in the tree through
  `twilight-http`), and `http` (the `Content-Type` header type of the raw request). Dev:
  `tracing-log`.

### Dependencies

`twilight-gateway` and `twilight-http` 0.17.1, with default features off and `rustls-webpki-roots`
(the manifest says why), and `twilight-model` 0.17.1 (the models of both; only `real` names them).
`serde_json` is a normal dependency for the state file and the history pages. New to `Cargo.lock`
with the skeleton: `twilight-gateway`, `twilight-gateway-queue`, `twilight-model`, `ordered-float`
2, `serde-value`, `serde_repr`, and `sha1_smol`; with REST (issue #16): `twilight-http`,
`twilight-http-ratelimiting`, and `twilight-validate`. `tokio-websockets` was already in the tree
through the WhatsApp plugin; it gains `fastrand`, `sha1_smol`, and `webpki-roots` there. All are
MIT, Apache-2.0, ISC, BSD-3-Clause, or CDLA-Permissive-2.0, so the licensing of the repository is
unchanged. `cargo tree -d` shows one `rustls` (0.23.46), one `rustls-webpki`, and one `aws-lc-rs`
for the plugin, no `ring` in its tree, and no duplicate of anything heavy (the duplicates it lists,
`hashbrown` and `syn`, are in the workspace already). The tests add `tokio-websockets` with its
`server` feature as a dev-dependency, and `tokio`'s `io-util` for the REST server of
`tests/loopback/rest.rs`.

### Running the tests

```sh
cargo test -p giles-channel-discord
```

No test contacts Discord, and none needs a token or a network beyond loopback.

- `config`, `status`, `snowflake`: unit tests, the validation table, the status mapping, and the
  arithmetic (against the snowflake in Discord's documentation).
- `state`: the file's shape, a round trip through a new process, a cursor that only moves forward, a
  write that replaces the file whole and leaves nothing behind, a change that changes nothing not
  being written, nine kinds of unusable file set aside and kept as `.corrupt`, a write that fails
  keeping the state in memory, and a forgotten channel gone from memory and file.
- `inbound`: the mapping table (a person, a bot, the plugin itself, a webhook, a server, fifteen
  system message types, empty), reply ids, display names, the order of text, files, and stickers,
  and names that try to break out of their note.
- `ingest`: the algorithm move by move with no I/O and a fixed clock: live messages, replays, a
  cursor from the state file, first sight of a channel (the history before the first message, the
  message in the page and in the buffer once), passes in channel order, the age limit as the start
  and as a filter, paging over 237 messages and over exactly 100, the page cap, the buffer merge,
  the buffer cap and the second pass, a restart not delivering again what the cursor passed,
  catch-up off, retries (1, 2, 4, 8, 8 s with a cap), a failing channel not holding up the others, a
  4xx skipping the channel, a 401 releasing everything (also one a send found), an answer nobody
  asked for, and the empty-message count.
- `session`: the state machine against `FakeDiscord` on paused time: statuses and the grace period,
  the delay sequence (1, 2, 4, ... 300 s), its reset after a healthy minute (and not after 59
  seconds), an invalid-session loop that must not Identify in a tight loop, a rejected token that is
  never polled again and a corrected restart, a REST 401 from a send that reports `NeedsAuth` and
  leaves the Gateway session going (also while waiting to reconnect, and while a history request is
  held), a status that stays `NeedsAuth` through a drop, a grace period, and a new session, final
  closes, a stream that ends, and shutdown (closed once, even mid-delay, and bounded when the client
  hangs). `session::messages` is the whole channel with the fake: a DM reaching the stream, the
  cursor moving only when the stream is polled again (and not when the SDK never persisted), nothing
  delivered twice after a restart, bots dropped, servers not reported, the empty-DM status and its
  recovery, catch-up from the cursor and from the age limit, no catch-up after `Resumed` and one
  after a new session, a live message while a page is held, a full page followed by the next,
  catch-up off, retries with paused-time delays (and live channels flowing meanwhile), a rejected
  token, and stopping while a page is held.
- `tests/sdk.rs`: the plugin served over loopback TCP by the real SDK with the fake client:
  `Describe`, `Configure` errors naming the key, a status that survives until the core acks it, and
  `NeedsAuth` over the wire; a DM that is in the SDK's spool before the core has it, comes again
  with the same id until acked, and moves the plugin's cursor only after the SDK moved on; messages
  in order; an SDK spool that can't be written (permissions on `events/`, skipped where they don't
  bind, as for root) offering nothing and moving no cursor until it recovers; and a new channel's
  history reaching the core before its first message. `tests/logs.rs` is the redaction test, which
  also checks that no message text, name, file name, or signed URL is in any line at `TRACE`, with
  the fake and with the real client; it is a binary of its own because `tracing` caches whether a
  call site is enabled, which makes a subscriber installed for one test unreliable next to tests
  without one.
- `tests/gateway.rs`: `RealDiscord` against a Gateway server written in the test on loopback
  (`ConfigBuilder::proxy_url`, which the shard also uses for the `ws://` URL of this server): Hello,
  Identify (the token and exactly the `DIRECT_MESSAGES` intent), heartbeats and ACKs, `Ready`, a
  close with 4004 and the end of the stream, a dropped connection resumed with the same session, an
  invalid session that Identifies again, a reconnect request that resumes, a server that isn't
  there, and the clean close at shutdown (and no reconnect when the close is never answered). The
  Gateway server is `tests/loopback/mod.rs`, shared with `tests/logs.rs`. It plays Discord from its
  documentation, so it shows that the glue does what the library does, not that Discord behaves so
  (issue #20). The shard's one-second delay before each connect makes it take a few seconds.
- `tests/rest.rs`: `RealDiscord::messages_after` against a REST server written in the test on
  loopback (`ClientBuilder::proxy` with plain HTTP, `tests/loopback/rest.rs`): the request (path,
  `after` and `limit`, the `Bot` authorization, a `Discord` user agent), a page sent newest first
  and handed over oldest first, an id of 0 asked for as 1, API errors with status and code, a body
  that isn't JSON, a 401 (and nothing more sent after it), a 429 waited out with its rate limit
  headers, a message the library can't read, a server that isn't there, and a message's time.
  Sending (issue #17): `create_message` and `create_dm` with the request (method, path,
  `Authorization: Bot <token>`, a JSON body of exactly `content`, `allowed_mentions: {"parse": []}`,
  and `flags: 4`; a DM body of `recipient_id` as a string), only the `id` read from the answer,
  Discord's error codes and statuses mapped to `RestError` (including a 413 and a non-JSON 502), a
  burst whose answers say the bucket is empty (the requests arrive in order, spaced by the reset),
  and a 401 after which the client sends nothing, not even a DM request.
- `tests/catch_up.rs`: the whole inbound path through `RealDiscord` with both servers: 150 messages
  of history paged in two requests `after` the cursor and the last id of the first page, a live
  replay of the last of them and a newer live message, each delivered once in order, the cursor on
  disk at the end; and a 401 on the first page giving `NeedsAuth` with live messages still arriving
  and no second request.
- `outbound`: against `FakeDiscord` on paused time: rendering and masked links, a 6 000-character
  reply split into ordered parts of at most 2 000 characters with nothing lost, mentions defused in
  the text, empty text and attachments refused, a recipient's DM channel opened once and reused (the
  same bare id twice), 50007, 50278, 10013, and 40003 on `create_dm` and on posting (`Unavailable`,
  not remembered), thirteen malformed targets (an identity with its `discord:` namespace among
  them), the whole error table (15 errors, each its kind), a failure after two of several parts of a
  send to a user (the later ones never posted; the retry posting the part that failed and the ones
  after it, and a new identical send then posting in full), a part that fails every time (240000)
  retried alone while the parts before it are never posted again, a send cut off at the outbox's 60
  seconds that leaves its progress, a cut-off reply into a conversation that is not remembered (its
  error promises no retry, and the same text later goes out in full), an invalid part after one went
  out (forgotten, so the same text later goes out in full), a forgotten DM channel opened again once
  and then remembered (and gone from the state file), 10003 twice as `NotFound` without a loop,
  10003 for a reply and for a fresh DM, a DM channel remembered across a restart, one recorded by a
  send and caught up on the next `Ready`, one recorded by the inbound side used without `create_dm`,
  a 401 on posting and on `create_dm` (`NeedsAuth`, then no request of any kind), a 401 after
  another final status, a 4004 and a 401 of a history request each stopping sends, a rate-limited
  call, and two concurrent sends that don't interleave. The map of unfinished sends has unit tests
  of its own: progress kept and cleared, sends told apart by destination and text, eviction of the
  oldest at the bound (and no growth past it over 10 000 sends), the 24 hour expiry moving with
  progress, and expired entries making room first.
- `tests/core.rs` runs the plugin, served over loopback with the fake client, under a real core
  (`PluginHost`, `RemoteChannel`, the router, sessions, the outbox) with `[owner] identities`
  `discord:<id>`: the owner's DM reaches a turn while another user's is dropped, the reply goes back
  to the conversation, and a proactive send to the owner's default identity reaches `send` as the
  bare user id and opens the DM. It is the test that sees the namespace added and stripped
  (issue #22).
- `tests/sdk.rs` also sends over the wire: a recipient (DM opened, message posted, the id back), a
  reply, `UNAVAILABLE` for 50007 and `INVALID_ARGUMENT` for another namespace.
- `real`: the conversions from the library's events (a documented READY payload and a DM with a
  reply, a file, and a sticker), a history page, and the 60 second bound on a REST call.

## What needs a real bot

Issue #20 confirms, in addition to its own list:

1. A bot **without** the Message Content intent receives the text and attachments of DMs in
   `MESSAGE_CREATE` and in REST history (the premise of the whole design; the documentation says so
   on two pages and a third warns otherwise).
2. `after=<id>` returns the 100 messages **just after** the cursor, not the newest 100, so paging
   forward by the page's largest id has no gap. The documentation says only "newest to oldest". Also
   that an `after` built from a time (the age limit) behaves the same, and that a DM channel's
   history still has what the bot was sent while it was offline.
3. The DM upload limit for a bot is 20 MiB per file and 25 MiB per request; how many files a message
   takes (the plugin sends 10; the documentation says nothing); which error a 21 MiB file gets (the
   plugin maps 413, 40005, and 50045 to `InvalidRequest`).
4. The 2000-character limit counted on emoji (scalars or UTF-16 units).
5. A DM channel id is stable across a plugin restart, and `POST /users/@me/channels` returns the
   same id.
6. Which error a closed-DM user produces: on Create DM, on the first message, or both (50007); and
   that a user who shares no server with the bot gets the same.
7. A user can open a DM to the bot only after sharing a server, and what "DMs closed" does to the
   bot's replies in an existing channel.
8. Close codes and statuses: a wrong token (4004), a reset token, and that no other close can leave
   the plugin silently disconnected.
9. Resume after a short network drop (no catch-up pass, no duplicate), and a fresh Identify plus
   catch-up after a long one.
10. A signed attachment URL downloaded on arrival, and one refused after its `ex` time; that every
    attachment URL is on `cdn.discordapp.com` or `media.discordapp.net` (a host outside the list is
    refused by the plugin), whether the CDN ever redirects, and which status an expired link gets
    (the plugin treats 403, 404, and 410 as "no longer available"). Also that a bot's upload with a
    `Content-Type` on the file part and an `attachments` list in `payload_json` is accepted as built
    by `real::multipart`, that Discord uses the part's media type, that ten files in a message are
    accepted and eleven are not, that 25 MiB in one request is accepted, and what status and code an
    oversize request gets.
11. That a message with only a sticker, poll, or forward behaves as the inbound table says, and that
    a DM with nothing but an attachment arrives with its `attachments` (the empty-message count
    depends on it).
12. That no `MESSAGE_CREATE` ever arrives for a server channel with this intent set, and that the
    plugin's own messages come back as events with `bot: true` and the bot's id (they are dropped),
    in the same channel as the user's.
13. That Discord answers with a close frame when the shard closes the connection after a Reconnect
    (op 7) or an Invalid Session (op 9), which is how the plugin counts them for its reconnect delay
    ([The library's silent reconnect](#the-librarys-silent-reconnect)), and what the first reconnect
    after a drop really costs in time (the loopback test shows only the library's own one-second
    sleep). Also how Discord ends a connection it drops: with a close frame, with a plain TCP close
    (which the client reports itself), or before Hello (which it cannot see: no status, retried
    about once a second, nothing sent).
14. That a first message from a new user arrives as a `MESSAGE_CREATE` with no `guild_id` and type
    0, in the channel `POST /users/@me/channels` returns for that user, and what a DM channel's
    first history page looks like (does it hold a system message, such as the start of a call,
    before the first message, and does the plugin drop it as it should).
15. That the REST history of a DM is available to a bot that was never in the channel's server (403
    or 50001 would mean catch-up skips the channel with a warning), and how the 60 second bound
    relates to what Discord actually does under a 429.
16. That a message with `allowed_mentions: {"parse": []}` and `flags: 4` (`SUPPRESS_EMBEDS`) is
    accepted in a DM; that `@everyone`, `<@id>`, and a link in the text then neither ping nor
    unfurl; and that a response always has the `id` the plugin reads.
17. That an answer to a real request carries `X-RateLimit-Scope` and the bucket headers (the library
    reads buckets only when the scope header is there), what a real 429 looks like for messages sent
    in a burst (scope, `Retry-After`, `retry_after`), and that the parts of a long reply arrive in
    order, each after the one before.
18. What a revoked token does on the Gateway while a session is alive (a 4004 close, or nothing
    until the next Identify), since the plugin goes to `NeedsAuth` from a REST 401 on its own.
19. The user guide's walk-through: that the Developer Portal's pages are as it names them (the
    **Bot** page's **Reset Token**, **OAuth2** and its URL Generator with the `bot` scope and no
    permissions, a **Public Bot** switch), that a bot that has joined a server with no permissions
    can exchange direct messages with a member, that Copy User ID gives the id the plugin reports as
    the sender, that the server-level privacy setting for direct messages is what blocks the bot's
    replies (error 50007), and that a reset token's old value is refused at once.

What can't be verified at all without a real bot: Discord's real error codes under real conditions,
the rate limits' actual numbers (documented as variable), and the Resume window.
