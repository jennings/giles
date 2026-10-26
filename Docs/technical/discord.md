# Discord Channel

The design of `giles-channel-discord`, the Discord channel plugin (PLAN sections 3.3, 8.5, and 10;
tracked in [issue #12](https://github.com/jennings/giles/issues/12)). A bot account receives direct
messages (DMs) over the Discord Gateway and sends over the REST API, as a separate process on the
plugin SDK ([Plugin SDK](plugin-sdk.md)). `ChannelInfo { identity_namespace: "discord", output:
Blocks, can_initiate: true }`.

**Status: designed, and built as far as issue #15** ([Implementation](#implementation): the
Gateway connection, the status, the configuration). This page records the decisions of issue #13:
the library, the Gateway intents, the catch-up algorithm, the status mapping, the configuration, and
the client seam the tests use. The rest of the plugin is built by issues #16 to #19 and verified
against a real bot by issue #20; those issues extend this page with what they build. Facts about
Discord were checked against the official documentation on 2026-10-25 and link to it; where the
documentation is silent or two pages disagree, the text says so, and [What Needs a Real
Bot](#what-needs-a-real-bot) lists what issue #20 must confirm.

## Decisions

| Question | Decision |
| --- | --- |
| Library | `twilight-gateway`, `twilight-http`, and `twilight-model` 0.17.1 (ISC), default features off, `rustls-webpki-roots` only. Serenity and a hand-written Gateway client are rejected ([The Library](#the-library)) |
| Licences | No copyleft anywhere in the tree; nothing new for the repo's licensing ([Licence Check](#licence-check)). No owner decision needed |
| Intents | `DIRECT_MESSAGES` (`1 << 12`) only. **Not** `MESSAGE_CONTENT`: Discord delivers the content of DMs to the bot without it ([Intents](#gateway-intents)) |
| Missing `MESSAGE_CONTENT` | Not a failure mode. A defensive check reports three DMs in a row with no text, no attachment, and no sticker |
| Servers (guilds) | Not subscribed (no `GUILD_MESSAGES`), so guild messages never arrive and are never reported as groups |
| Conversation id | The DM channel id, kind `Direct` |
| Identity | `discord:<user id>`, `sender_authenticated = true` ([Identity](#identity)) |
| Output mode | `Blocks`; no edits, no typing indicator ([Output](#output)) |
| Catch-up | A cursor per DM channel in the plugin's data directory, REST `after=<id>` paging after every new session, 24 h default age limit ([Catch-Up](#catch-up)) |
| Status | Rejected token (close 4004, REST 401) is `NeedsAuth` with a `Url`; every other failure is `Error` ([Status](#status)) |
| Config keys | `token`, `max_blob`, `catch_up_max_age` ([Configuration](#configuration)) |
| Seam | A `Discord` trait with six methods in the end (two so far, [Implementation](#the-seam-as-built)); the twilight types stay in one `real` module ([The Client Seam](#the-client-seam)) |

## The Library

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

Cold build times are one run each on the development machine (macOS, parallel build, debug
profile, dependencies only, with twilight's `zstd` and `decompression` features that the final
choice turns off), so they show order of magnitude only: none of the three is a compile-time
problem.

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
  documentation asks for, see [Rate Limits](#rate-limits)) come with `twilight-http`.
- **The smallest tree.** Eleven crates are new to `Cargo.lock`: six twilight crates (ISC),
  `ordered-float` 2, `serde-value`, `serde_repr`, `sha1_smol`, and `smallvec` 1. Gateway
  WebSockets reuse `tokio-websockets` 0.13, which the WhatsApp plugin's tree already contains.
- **Modern TLS with no duplicate stack.** `rustls` 0.23.46 and `rustls-webpki` 0.103.16, the
  versions already in the lock. Serenity 0.12.5 is also rejected for its TLS: `tokio-tungstenite`
  0.21 brings `rustls` 0.22.4 and `rustls-webpki` 0.102.8, and the RustSec database (checked
  2026-10-24) lists advisories against both with a fix only in the 0.23 and 0.103 lines:
  RUSTSEC-2026-0346 and -0349 (`rustls`) and RUSTSEC-2026-0049 and -0382 (`rustls-webpki`). The
  plugin would ship a vulnerable TLS stack until serenity 0.13 is released.
- **A narrow, typed surface.** The plugin needs `Intents`, `Event::{Ready, Resumed,
  MessageCreate, GatewayClose}`, `create_private_channel`, `channel_messages().after()`,
  `create_message()` with `allowed_mentions` and attachments, and the typed API error codes
  (`ErrorType::Response { error: ApiError::General(..) }` carries the 50007). That is a few
  hundred lines of glue, all in `real`.

What twilight costs, and how it is contained:

- **Trust store.** `twilight-gateway` builds its own TLS connector (`Connector::new()`, no hook to
  pass a `ClientConfig`), and `twilight-http` takes `webpki-roots`, not `webpki-root-certs`. Both
  are the Mozilla root program compiled in (CDLA-Permissive-2.0, a licence already in the tree),
  never the OS store, so the property that matters, "bundled roots, the same on every host", holds;
  only the crate that carries the list differs from `giles_plugin_sdk::tls`.
- **Crypto provider.** With default features off, `rustls` needs a process-default provider, and
  the lock holds both `ring` and `aws-lc-rs`, so rustls would refuse to guess (`Connector::new()`
  panics without a default provider). The plugin's `main` installs
  `rustls::crypto::aws_lc_rs::default_provider()` before anything connects, the same provider as
  the SDK.
- **Compression off.** `zstd` and `zlib` (Gateway transport compression, optional in Discord's
  protocol) are left out: DM traffic is a few small events, and `zstd` would add `zstd-sys`, a C
  build. The `decompression` feature of `twilight-http` (brotli) is off for the same reason.
- **API churn.** 0.x with a breaking release about yearly. Pin `0.17` (`Cargo.lock` pins the
  exact release) and confine every twilight type to `real`; a move to 0.18 then touches one
  module, as moving the `whatsapp-rust` pin touches only its `real/`.
- **Release lag.** The last release is ten months old while `main` is active. The risk is
  being stuck on 0.17.1 if upstream stalls, not breakage: the Gateway and REST protocol it speaks
  is versioned (API v10, which the plugin pins) and does not change underneath it.
- **A second HTTP client.** `twilight-http` is not a general HTTP client, so downloading a CDN
  attachment ([Attachments](#attachments)) uses `reqwest`, already a workspace dependency, with
  `tls_certs_only` and the SDK's roots like `giles_core::net::tls_client_builder`. The CDN is not
  the API, so it has no bucket to rate limit.
- **Strict models.** `twilight-model` fails a whole event it can't deserialize. A MESSAGE_CREATE
  that doesn't parse is logged by event type only (never the body) and costs the live copy of
  one message; the cursor did not move past it, so the next catch-up fetches it again
  ([Catch-Up](#catch-up)).

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
  framework (`Client`, `EventHandler`, cache, command framework) built around owning the event
  loop, so the Gateway can neither be replaced by a fake nor polled from the plugin's own task
  without adapters; 27 new crates including a second `reqwest` and a second `rustls`; a release
  line declared finished while `next` rewrites the API.
- **A thin client on `tokio-tungstenite` and `reqwest`.** Attractive on paper: five new crates, the
  SDK's TLS config, and one HTTP client. It would also be the IRC plugin's answer (no library fit,
  so it uses `irc-proto` and `tokio-rustls`). It loses here for the protocol-risk reason above: we
  would write and unit-test heartbeats, resume, close-code policy, and bucketed rate limiting
  against our own reading of the documentation and learn about every mistake from a real bot, last.
  If twilight is ever abandoned or its TLS handling becomes a blocker, this is the fallback; the
  trait seam means only `real` changes. Its in-process fake Gateway would be a loopback WebSocket
  server (see [Testing](#testing)).

### Licence Check

How: a scratch crate per candidate; `cargo metadata` for the whole normal-dependency closure,
reading every crate's `license` field; then a search of every crate's source for `AGPL`, `GPL-`,
`GNU General Public`, `LGPL`, and `SSPL`, because the WhatsApp plugin's one AGPL crate declared
`MIT` in its manifest and carried the real licence in its source headers ([WhatsApp
licensing](whatsapp.md#licensing)).

| Licence expression (twilight's 88 crates) | Crates |
| --- | --- |
| `MIT`, `MIT OR Apache-2.0`, `Apache-2.0 OR MIT`, `Apache-2.0 / MIT` | 70 |
| `ISC` | 8 (the six twilight crates, `untrusted`, `rustls-webpki`) |
| `BSD-3-Clause` | 2 (`subtle`, `sha1_smol`) |
| `Apache-2.0 OR ISC OR MIT` | 2 (`rustls`, `hyper-rustls`) |
| `Apache-2.0 AND ISC` | 1 (`ring`) |
| `ISC AND (Apache-2.0 OR ISC)` and the longer composite of `aws-lc-sys` | 2 |
| `CDLA-Permissive-2.0` | 1 (`webpki-roots`) |
| `Unlicense OR MIT` | 1 (`memchr`) |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | 1 (`unicode-ident`) |

- No GPL, AGPL, LGPL, SSPL, or proprietary licence in any of the three candidate trees, in
  manifests or in sources. The source search found one mention: `aws-lc-sys`, whose vendored
  jitterentropy header offers its code under "BSD-3-Clause or GPL-2.0" (the BSD choice applies).
  `aws-lc-sys` is already shipped by the core and the other plugins, so it adds nothing.
- Everything new to the lock is MIT, Apache-2.0, ISC, or BSD-3-Clause, which are permissive and
  keep the repository's `MIT OR Apache-2.0` unchanged. They require their notices to travel with
  binaries, as the rest of the tree's MIT, BSD, and ISC crates already do.
- Avoided on purpose: twilight's `zstd` feature would add `zstd-sys`, whose bundled zstd source is
  "BSD-3-Clause or GPL-2.0-only" (the same dual-licence shape as above, still no copyleft
  obligation), and a C build for nothing.
- Advisories (RustSec database, 2026-10-24) for the crates the plugin adds: none against the
  twilight crates, `tokio-websockets`, `sha1_smol`, `serde-value`, or `serde_repr`;
  `ordered-float` 2.10.1 and `smallvec` 1.16.4 are past the fixed versions of their old advisories.
  The shared TLS crates are at their patched versions in the lock, as of the same date.

The licence check is not legal advice, and it is a snapshot: re-run it when the dependency set
changes (issue #19 states the result in the user guide).

**This is not a close call, so no owner decision is needed.** The library is maintained (see
the commit dates above), builds on stable Rust, uses `rustls` with bundled roots, and has no
copyleft dependency. The one real trade-off, a 0.x API whose last release is ten months old, is
contained by the seam and has a documented fallback.

## Discord Facts

Verified against the official documentation on 2026-10-25, with the page that says it. Prefix
`https://discord.com/developers/docs/` (the HTTP API is `https://discord.com/api/v10/`; the
default API version when none is given is still the deprecated v6, so every URL names `v10`).

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

Not in the API documentation at all, so treated as unverified: that a user can DM a bot only if
they share a server with it, that a bot can DM a user only if they share a server and the user
allows DMs from server members, and any numeric limit on DMs a bot may open. Bots cannot list their
DM channels (the user resource has no such endpoint; there is only Create DM), which shapes the
[catch-up](#catch-up).

## Gateway Intents

The plugin identifies with **`DIRECT_MESSAGES` only**.

- A bot receives DM messages with their text without `MESSAGE_CONTENT` (the facts table above, two
  pages). The Developer Portal toggle is therefore not needed, 4014 cannot be caused by the
  plugin, and the owner has nothing to enable. This changes the premise of issues #15, #16, and #19,
  which assumed the privileged intent.
- Not requesting `MESSAGE_CONTENT` is also the policy-conforming choice: Discord says to
  "only enable a privileged intent if your use case requires it to function".
- `GUILD_MESSAGES` is not requested, so no server message is ever delivered. The core drops group
  conversations (PLAN section 1.1) and the plugin has no reason to receive them: they would arrive
  with empty content anyway (the bot is rarely mentioned), they cost traffic, and a "group" event
  that the core always discards adds nothing. Issue #16 asked to decide this; **guild messages are
  not reported at all.** A `MESSAGE_CREATE` that carries a `guild_id` regardless (the intent set
  changes, a library quirk) is dropped before it becomes an event.
- `MESSAGE_UPDATE` and `MESSAGE_DELETE` arrive with `DIRECT_MESSAGES` (one intent covers all
  three) and are ignored: an edit or a delete after delivery doesn't change what the agent was
  asked ([Inbound](#inbound)).
- No presence, no compression, no `shard`, no `capabilities`. Identify `properties` are the
  library's.

### Detecting a Missing Intent

Issue #16 asked how to detect a missing `MESSAGE_CONTENT`. With the intent set above there is
nothing to detect, but the documentation's DM exception is the one fact the whole design leans on
and it is checked only against documentation, so the plugin guards it cheaply:

- A DM from a person with no text, no attachment, and no sticker is dropped (nothing to answer), at
  `debug`.
- **Three such DMs in a row** (reset by any DM that has text) set `Error { message: "Discord is
  delivering direct messages without their text; enable Message Content under Privileged Gateway
  Intents in the Developer Portal and restart" }`, and the next message with text sets
  `Connected` again. The message is honest about the cause being a guess (polls, forwards, and
  other content the plugin doesn't deliver also arrive empty), and it makes a changed Discord
  behaviour visible instead of silent. The intent is not requested automatically: asking for a
  privileged intent that isn't enabled closes the connection with 4014.

## Identity

- Identity `discord:<user id>`: the author's snowflake from the Gateway event, immutable, unlike
  the username, global name, and the retired discriminator. The owner lists `discord:<id>` in
  `[owner] identities`. `sender_display_name` is `global_name`, falling back to `username`; it is
  display only and never used for routing.
- **`sender_authenticated = true`.** The author id is set by Discord, not by the sender: it
  arrives on the Gateway connection that the bot authenticated with its token over TLS, and in a
  DM the author is either the bot or the one other participant. A message can't claim another
  user's id the way a mail header or an IRC mask can. The assumption it rests on is that the
  bot token stays secret and Discord is honest, the same trust as any channel's server. (Webhook
  messages, which can carry arbitrary names, don't occur in DMs, and the plugin drops any message
  with a `webhook_id` anyway.)
- Namespace `discord`. User ids are global across Discord, so a second bot instance would report
  the same identity for the same person.

## Conversation Id

The DM channel id (a snowflake as a string), `ConversationRef { kind: Direct, title: None }`.
`POST /users/@me/channels` is idempotent per recipient ("if one already exists, it will be
returned instead"), and DM channels persist, so the id is stable per (bot, user) pair in practice;
Discord does not promise it in so many words, and the plugin does not depend on it: it caches the
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
carries its links in `content`); other content (polls, forwards) is not delivered.

Durability is the SDK's: it writes each event to its spool before the plugin's stream is polled
again, and resends unacked events when `Events` reopens
([Plugin SDK](plugin-sdk.md#events-and-persistence)). The plugin keeps no spool of its own, and
only the cursor below.

Status events use ids `status:<nanos>:<counter>`, as the other plugins do, and are only sent when
the status changes.

## Catch-Up

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
  the SDK's spool, or dropped). `user`: the other participant, for resolving `discord:<id>` to a
  channel without asking Discord. Both are strings, because snowflakes exceed what JSON numbers
  carry safely, and ids compare as `u64`.
- Losing the file is harmless: the channels are rediscovered by live messages and by sends, and
  catch-up refetches at most `catch_up_max_age`, which the core deduplicates by event id
  (30 days). Corrupt or unreadable state is renamed `.corrupt` and logged, as the SDK does for
  events, never deleted.

**When it runs.** After `Ready` (a new session: first start, a restart, an invalid session, a
session timed out) for every channel in the state file, one channel at a time. **Not after
`Resumed`**: the Gateway replayed the gap itself, in order. The Gateway session is established
first, so any message created after that moment arrives live, and REST only has to cover what came
before it. A channel seen for the first time (a live message from an unknown channel, or a channel
the plugin just created for a send) gets the same catch-up before its first message is delivered,
with no cursor (below), so a user's earlier messages are not skipped just because the first one
the plugin saw live was the third.

**The algorithm**, per channel:

1. Mark the channel `CatchingUp`. Live events for it are buffered in memory (in order) instead of
   delivered.
2. `after` is the cursor, or, with none, the snowflake of `now − catch_up_max_age`
   (`(ms − 1420070400000) << 22`).
3. `GET /channels/{id}/messages?after=<after>&limit=100`. The page, newest first on the wire, is
   reversed and handled oldest first. Each message goes through the same inbound mapping as a
   live one; a message older than `catch_up_max_age` is dropped (the cursor still passes it).
   After each message the cursor candidate becomes its id. `after` for the next request is the
   largest id of the page. A page with fewer than 100 messages ends the loop; a hard cap of 100
   pages (10 000 messages) also ends it with a warning, and the rest are lost.
4. Drain the buffer: every buffered live event with an id above the channel's high-water mark is
   handled; the rest (already fetched) are duplicates and dropped. Mark the channel `Live`.
5. In `Live`, events with an id at or below the high-water mark are dropped (a replayed duplicate
   after a resume), the others are handled, and the high-water mark moves up.

**Deduplication.** One mark per channel, the highest id handled, decides: ids are time-ordered per
channel, REST returns each message once per page, and the buffer step above makes live events
and fetched ones meet without a gap or a repeat. Across restarts there are two more layers, so
nothing needs to be perfect: the SDK drops an event whose id is already pending in its spool, and
the core's inbox deduplicates (instance, event id) for 30 days. `catch_up_max_age` is validated
at most 30 days for that reason.

**Moving the cursor.** The same rule as the email plugin's UID cursor ([Delivery and the
Cursor](email.md#delivery-and-the-cursor)): the SDK writes each event to disk before polling the
stream again, so the persisted cursor moves to a message's id when the stream is next polled (and
a drop moves it at once). A crash between the SDK's write and the state write refetches that one
message, with the same event id, which the spool or the core deduplicates.

**Bounds, and what is lost.**

- Page size 100; at most 100 pages per channel per pass; `catch_up_max_age` (default 24 hours,
  `"0s"` turns catch-up off): older messages are skipped, with a count logged at `info` (no
  content).
- A failed fetch (network, 5xx, 429 beyond the bounded wait) is retried with backoff while the
  session lasts; live events for the channel stay buffered, to a cap of 1000, after which new live
  events are discarded and the channel is caught up again when the first pass finishes (the cursor
  never passed them). A 401 is the auth failure of [Status](#status).
- Lost for good: messages older than the age limit or beyond the page cap; messages deleted
  before the fetch; a message edited while the plugin was down arrives with its edited text, and
  later edits are ignored; **messages from a user the plugin has never seen, sent while it was
  down, if that user sends nothing afterwards** (the plugin can't list DM channels, and a first
  live message is what reveals the channel). A `users` config key that opens the owner's DM
  channel at start would close that last gap; it needs ids the plugin doesn't have (`[owner]
  identities` is the core's), so it isn't in v1.
- Messages the bot sent while it was offline can't exist; the plugin's own messages in a page are
  dropped by the author check.

## Output

- **`OutputMode::Blocks`.** `Deltas` exists only for the web channel (`Channel::deltas()`, PLAN
  section 6.9). Streaming into Discord would mean creating a message, then editing it repeatedly
  to append text. Edits are rate limited per route with a dynamic bucket (Discord says not to
  hard-code it), a streamed answer crosses the 2000-character limit mid-sentence, and a turn
  that is stopped would leave a half-edited message. Each assistant block is one `send`; long ones
  are split.
- **Rendering and splitting** are the SDK's Discord renderer (issue #14): CommonMark to Discord
  markup, split at 2000 characters without breaking fences. The limit is documented in
  "characters"; whether Discord counts Unicode scalars or UTF-16 units for characters outside the
  Basic Multilingual Plane is not, so issue #20 sends a 2000-character message of emoji and the
  renderer counts scalars until that shows otherwise.
- **No typing indicator.** `POST /channels/{id}/typing` lasts 10 seconds and Discord says bots
  generally should not use it. The `Channel` trait gives the plugin no signal that a turn started
  or ended (it sees an inbound message and later a `send`), so a typing call on arrival would
  show "typing" for ten seconds and stop while the agent works for minutes, which is worse than no
  indicator. Revisit if the core adds a turn signal to the trait.

## Send

`send` for a `Conversation(<DM channel id>)` target posts to that channel. For a
`Recipient(discord:<user id>)` target (the outbox, jobs, proactive sends) it resolves the channel
first: the state file's `user` mapping, else `POST /users/@me/channels`, whose result is stored.

- Each chunk of the rendered text is one `POST /channels/{id}/messages`, in order, sequentially,
  with `allowed_mentions: { "parse": [] }` always (no user, role, or `@everyone` can ping, whatever
  the model wrote and whatever the renderer escaped) and no `message_reference`: `reply_to` is
  ignored, because in a DM a reply link only adds clutter. The returned `MessageId` is the last
  chunk's.
- A caller that sends text with attachments gets the text as the first message's content and the
  files on it ([Attachments](#attachments)); text too long for one message goes in the chunks
  before the files.
- Every REST call is wrapped in a timeout. `twilight-http` queues behind the rate limiter and
  sends a request that got a 429 back through it (the code has no retry count), so the plugin
  supplies the bound: 60 seconds per call.

| Outcome | Result of `send` |
| --- | --- |
| Success | `Ok(message id)` |
| 401 (`ErrorType::Unauthorized`; the library refuses every later request) | `Unavailable` ("Discord rejected the bot token") and the `NeedsAuth` status |
| 50007, 10013, 40003 resolving a recipient or sending | `Unavailable`: closed DMs, no shared server, a user that doesn't exist, DMs opened too fast. The outbox retries on its own schedule; the plugin has no retry loop |
| 10003 on a cached channel | Drop the cache entry, create the DM channel again once, retry the chunk; a second 10003 is `NotFound` |
| 10003 on a `Conversation` target | `NotFound` |
| 40005, 50045 (file too large), 50006, 50035 | `InvalidRequest`, with Discord's message: a plugin bug or an oversize send |
| Malformed `discord:<id>`, an id that isn't a snowflake | `InvalidRequest` |
| 429 not cleared within 60 s, 5xx, transport error, timeout | `Unavailable` |

If a send fails after some chunks went out, the error makes the outbox send the whole message
again and the user sees the first chunks twice. Discord's `nonce` with `enforce_nonce` can
deduplicate identical messages from one author for a few minutes, but a nonce derived from the
text would also suppress two genuine identical replies ("ok", twice), and the SDK gives `send`
no idempotency key, so this is accepted and documented, not worked around.

## Attachments

Both directions are issue #18; the facts it needs are recorded here.

- Inbound: an attachment's `url` is a signed CDN URL (`ex`, `is`, `hm` query parameters) and
  valid when the message arrives live or is fetched, and expires afterwards, so the plugin
  downloads at once. It uses `reqwest` (see [The Library](#the-library)), streams, and counts
  bytes against `max_blob` (default 50 MiB, like the other plugins), aborting at the limit; the
  declared `size` rejects a too-large file before the request. An expired or refused URL, or one
  over the limit, is replaced by a note in the text. URLs are never logged above `debug`
  (the query string is a credential); logs carry the host and file name.
- Outbound: the limit is 20 MiB per file and 25 MiB per request. The bot has no Nitro and a DM
  has no boost tier, so 20 MiB is expected to apply; the plugin refuses a larger file locally with
  `InvalidRequest` before anything is sent (`min(max_blob, 20 MiB)` per file, 25 MiB in total),
  and maps 40005 and 50045 the same way. Files per message: the documentation states no limit
  (10 is the commonly used figure, unverified), so the plugin sends at most 10 per message and
  more in follow-up messages. The upload is `twilight-http`'s `create_message` with `Attachment`s
  (the file bytes are held in memory, bounded by the limit).

## Rate Limits

- **REST.** `twilight-http` handles it: a per-route bucket from `X-RateLimit-Bucket` and the
  limit headers, the 50 requests per second global limit, and a retry after a 429. The plugin's
  traffic is a handful of requests per minute, far below any limit. Discord asks that a 401 is
  never repeated (10 000 invalid requests in 10 minutes means a Cloudflare ban of the IP); the
  library refuses everything after the first 401, and the plugin goes to `NeedsAuth` and stops
  calling REST until it is restarted with a new token.
- **Gateway.** 120 sends per 60 seconds; the shard sends only heartbeats, Identify, and Resume, and
  queues them itself. The 1000 Identifies per 24 hours matter more: exceeding them **resets the
  token**. Resume costs none, so a drop never Identifies unless Discord says the session is gone.
- **Reconnect backoff.** `twilight-gateway` waits `2^attempt` seconds (to about four minutes)
  after a failed connect, but **resets the count to zero on every close**, so a server that
  accepts the connection and closes it again at once (a bad intent set, a flapping edge) would be
  retried every second, and each retry may Identify. The plugin therefore delays: after any
  `GatewayClose`, it waits before polling the shard again, 1 second doubling to 5 minutes, and
  resets the delay after a session has stayed up for a minute. A shard never polled doesn't
  reconnect, so this is a plain `sleep` in the plugin's loop. A fatal close ends the stream
  (`None`); the plugin does not create a new shard for the same reason it does not retry a 401.

## Status

`ChannelStatus` has `Connected`, `NeedsAuth { challenge, expires_at }`, and `Error { message }`;
the challenge is a QR code, a pair code, or a URL, and carries no text. A missing token is not a
status at all: it fails `Configure` with `INVALID_ARGUMENT` ("invalid config: token: ..."), like
every config error.

| Event | Status | Then |
| --- | --- | --- |
| `Ready` (new session) or `Resumed` | `Connected` | Catch-up runs after `Ready` |
| Close **4004** (authentication failed) | `NeedsAuth { Url("https://discord.com/developers/applications") }` | Final: no reconnect until the plugin is restarted with a new token |
| REST **401** on any call | The same `NeedsAuth` | Final; REST calls stop |
| Close **4014** (disallowed intents) or **4013** (invalid intents) | `Error`: "Discord refused the intents the plugin requests (close 4014/4013); this is a bug or a change at Discord, please report it" | Final. With `DIRECT_MESSAGES` only, neither can happen unless Discord changes DM intent rules. Not `NeedsAuth`: the owner can't fix it |
| Close **4010**, **4011**, **4012** | `Error` with the code and its meaning (4011: the bot is in too many servers to run unsharded) | Final |
| Any other close (4000 to 4003, 4005, 4007 to 4009, 1006 and other transport closes, no close frame), Reconnect (op 7), Invalid Session (op 9), a missed heartbeat ACK, a failed connect | Nothing if `Ready` or `Resumed` follows within **10 seconds** (the grace WhatsApp uses); otherwise `Error`: "disconnected from Discord, reconnecting" | The library resumes or re-identifies; the plugin's backoff above applies |
| Three empty DMs in a row | `Error` as in [Detecting a Missing Intent](#detecting-a-missing-intent) | Cleared by a DM with text |
| 429, 5xx, timeout on a send or a catch-up page | None; the send returns `Unavailable` | Retried by the outbox or the pass |
| Plugin stopping | None | The shard is closed with 1000 (the bot goes offline); the stream ends |

The 4004 case also covers a token Discord reset after the Identify limit. The `NeedsAuth` URL is
the Developer Portal's applications list, where the Bot page has "Reset Token"; the user guide
(issue #19) says so, and says how to put the new token into `secrets.toml`. A repeat of the last
status is not sent again. No status carries the token, a user id, or message text.

## Configuration

A `[channel.discord]` instance's `config` table, validated at `Configure` with the key named in the
error (`invalid config: token: ...`), unknown keys refused like the other plugins:

| Key | Default | Meaning |
| --- | --- | --- |
| `token` | required | The bot token (`{ secret = "..." }` in practice). A credential: `Secret`-wrapped, never logged, `<redacted>` in `Debug`, never in a status. Non-empty, no whitespace or control characters (a pasted `Bot` prefix is refused with a message to paste the token alone) |
| `max_blob` | `"50 MiB"` | The largest attachment downloaded or sent; outbound is also capped by Discord's 20 MiB per file |
| `catch_up_max_age` | `"24h"` | How far back catch-up fetches. `"0s"` turns it off; at most `"30d"`, the core's deduplication window |

Nothing else is configurable on purpose: the API version, the intents, the page size, the grace
period, and the backoff are constants. The plugin's sandbox needs outbound TCP 443 only (REST,
the Gateway, and the CDN are all HTTPS or secure WebSocket on that port), plus its data directory.

## The Client Seam

Everything above the `real` module talks to Discord through one trait, in the style of the
WhatsApp plugin's `Backend`. The plugin's own types cross it (`DiscordMessage`, `Attachment`,
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

- `GatewayEvent`: `Ready { user_id }`, `Resumed`, `Message(DiscordMessage)`, and `Closed { code:
  Option<u16>, fatal: bool, reconnecting: bool }`. A `Message` is the plugin's own struct: id,
  channel id, author (`id`, `bot`, `username`, `global_name`), `guild: bool`, `webhook: bool`,
  `kind`, `content`, `attachments` (`url`, `filename`, `content_type`, `size`), sticker names,
  timestamp, and the referenced message id. Update, delete, and every other Gateway event never
  leave `real`.
- `RestError`: `Unauthorized`, `Api { status, code }` (the Discord error code), `RateLimited`
  (a 429 the bound gave up on), `TooLarge`, and `Transport`. `inbound` and `outbound` map these to
  the SDK's errors and to statuses; the table under [Send](#send) is tested against them.
- Time is `tokio::time`, so tests pause and advance it for the grace period, the backoff, and the
  catch-up retry. Data directory and clock are injected like the other plugins'.

`real` implements `Discord` on a `twilight_gateway::Shard` (one task, its events forwarded
through a channel, close frames mapped, the backoff sleep) and a `twilight_http::Client`, plus
`reqwest` for downloads, and the pure conversions from twilight types to the plugin's (tested from
JSON fixtures of the documented payloads, so a model change that breaks parsing fails a test).

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
  retry, 429 bound, no message sent after a 401); and attachments (download within, over, and
  after expiry; upload over the limit; ten files and an eleventh).
- **Through the SDK**: an integration test in the style of the IRC and WhatsApp plugins: the
  plugin served over a socket pair with the fake client, a DM in becomes an event, a `Send` goes
  out, and the core's `Events` reopen redelivers an unacked one.
- **Pure units** next to the logic: snowflake arithmetic, state-file round trip and corruption,
  the status mapping, error mapping, config parsing.
- **`real`, offline.** Its conversions are unit-tested from fixtures. Its loop is driven without
  Discord: `ConfigBuilder::proxy_url` points a shard at `ws://127.0.0.1:<port>`, and a Gateway
  server written in the test plays Hello, Ready, a close with 4004, and so on, which checks the
  glue (close-code mapping, resume, the clean close) without the network. Issue #15 built it
  ([Implementation](#running-the-tests)); a REST counterpart with `ClientBuilder::proxy(..,
  use_http = true)` is for the issues that use REST, and has not been tried.

## Implementation

What issue #15 built: the crate, the configuration, the Gateway connection, and the status. It
receives no message and sends none yet: inbound mapping and catch-up are issue #16, sends are
issue #17, and attachments are issue #18. Each extends this section with what it adds.

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
| `session` | `Session`, `Timing`, and `run`: the task that owns the connection ([State Machine](#state-machine)) |
| `channel` | `DiscordChannel` (the SDK's `Channel`), `configure`, `configure_with`, and the `SessionSlot` that lets `main` stop the session |
| `fake` | `FakeDiscord`, the scripted client for tests |
| `real` | `RealDiscord`, the client on `twilight-gateway`: the only module with twilight types |
| `main` | Logging, `ChannelPlugin::new("discord", ..).output(Blocks).can_initiate(true).serve(..)`, then the session is stopped |

```text
twilight Shard ── real ──GatewayEvent──▶ session ──ChannelEvent::Status──▶ Channel::events ──▶ SDK
      ▲                                     │
      └───── polled again only after ───────┘ the reconnect delay
```

### The Seam as Built

[The Client Seam](#the-client-seam) lists the six methods the finished plugin needs. The trait
today has the two that the Gateway connection needs, `next_event` and `shutdown`. `create_message`
and `create_dm` (issue #17), `messages_after` (issue #16), and `download` (issue #18) are added
by the issue that calls them, together with their `real` implementations, their fake, and their
tests: a method defined now would have no caller, and its real implementation would be a stub that
looks like it works. `twilight-http` and `twilight-model` are likewise added by those issues. Two
deliberate differences from the sketch:

- `GatewayEvent` is `Ready { user_id }`, `Resumed`, and `Closed { code: Option<u16> }`;
  `Message` arrives with issue #16. `Closed` has no `fatal` or `reconnecting` flag: whether a close
  code is final is the plugin's policy (`status::classify`, the table under [Status](#status)),
  tested against the fake, rather than a flag the client sets. The client agrees with it by
  construction: the codes the policy calls final (4004, 4010 to 4014) are the ones
  `twilight-gateway` ends its stream on, and a stream that ends without one is an error status of
  its own.
- `next_event` can be dropped while pending without losing an event (the session does, to stop),
  and a client that isn't polled doesn't reconnect: both are properties of `twilight-gateway`'s
  `Shard` that the trait states as its contract.

### State Machine

`session::run` is one loop over three things: the stop signal, the grace timer, and the pending
`next_event` call. It reacts to what `status::classify` says about each event:

| Event | Reaction |
| --- | --- |
| `Ready` or `Resumed` | Report `Connected` (a repeat of the last status is not sent), clear the drop timer, note when the session came up. `Ready` is a new session and `Resumed` the old one; both are `Connected`, and the log says which. Catch-up (issue #16) will run after `Ready` only |
| `Closed` with a code the client recovers from (or no code) | Start the grace timer if none is running. Reset the delay to 1 s if this session had been up for a minute. Do not poll the client until the delay is over, then double the delay (cap 5 minutes) |
| `Closed` with 4004 | Report `NeedsAuth` (the Developer Portal URL) and stop polling for good |
| `Closed` with 4010 to 4014 | Report an `Error` that says what happened and stop polling for good |
| The stream ended (`None`) with no final close before it | Report an `Error` ("restart this plugin") and stop polling for good: nothing starts another shard, for the reason in [Rate Limits](#rate-limits) |
| The grace timer ran out (10 s after the first close with no `Ready` or `Resumed`) | Report `Error` "disconnected from Discord, reconnecting"; the client is still trying |
| Stop | End the pending poll, call `shutdown` on the client once, bounded by 5 s |

"For good" means until the plugin is restarted: the session keeps its status, polls nothing, and
waits to be stopped. A bad token therefore costs Discord one Identify, not a stream of them, and an
operator who fixes `secrets.toml` and restarts the instance gets a new process, a new client, and
`Connected`. All the delays are in `Timing` (`Default` is what the plugin uses), and the clock is
`tokio::time`, so the tests run on paused time and check the exact delays.

### The Library's Silent Reconnect

`twilight-gateway` reports a close frame, a transport error, a missed heartbeat ACK, and a failed
connect, but not a connection that ends with a plain TCP close and no close frame: its `Shard`
notes the disconnect and starts reconnecting inside the same `poll_next`, and nothing comes out of
the stream. The plugin would see no event to report or to pace, and Discord would see a new
connection a second later. `real` therefore compares `Shard::state()` before and after every poll:
a shard that was connected and is now disconnected, with no message to show for it, is reported as
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

### Shutdown

`ChannelPlugin::serve` owns the channel and gives no hook for its end, and a process that exits
when `serve` returns would cut the session off before it closes the Gateway, leaving the bot shown
online until Discord times the connection out. As in the WhatsApp plugin, `configure` leaves a
`SessionHandle` in a `SessionSlot` that `main` holds; after `serve` returns, `main` stops the
session and waits up to 6 seconds for it. The session ends the pending poll and calls
`Discord::shutdown`, bounded by 5 seconds. `RealDiscord::shutdown` sends close code 1000 (which
ends the session and shows the bot offline) and polls the shard until Discord's answer arrives, if
there is a session to end, for at most 900 milliseconds. The limit is under the second the library
sleeps before every connect: if the peer ends the TCP connection without answering, the shard starts
reconnecting and would Identify, putting the bot back online after it was shut down, and the wait
is over before that can happen (`shutdown_does_not_reconnect_when_the_close_is_never_answered`).
With no session (never connected, a rejected token) it does nothing, and the connection is dropped
with the process.

### Configuration and the Token

`Config` derives `Deserialize` with `deny_unknown_fields`, and `Settings::new` validates it; every
error is `invalid config: <key>: <reason>`. `token` is a `Secret`: no `Display`, `Debug` prints
`<redacted>`, and the one use of its value is `RealDiscord::new` handing it to the library (whose
own `Config` also redacts it). The validation errors never repeat it, a pasted `Bot` prefix gets its
own message, and `tests/logs.rs` captures `tracing` at `TRACE` around two runs and asserts the token
is in no line: the plugin served by the SDK with the fake client, and the real client against the
loopback Gateway through Identify, a dropped connection, Resume, and a rejected token (the library's
own logs are in the capture). `max_blob` and `catch_up_max_age` are validated and logged at startup;
nothing reads them until issues #16 and #18.

`RealDiscord::new` installs `rustls`'s `aws-lc-rs` provider as the process default (a provider
that is already installed is left alone), because `twilight-gateway` panics building its TLS
connector without one. The design said `main` would do it; doing it where the shard is built means
no caller can forget it, and `tests/crypto_provider.rs`, a binary of its own so that no other test
can have installed the provider first, checks it.

### Dependencies

`twilight-gateway` 0.17.1 only, with default features off and `rustls-webpki-roots` (the manifest
says why); no `twilight-http` and no `twilight-model` yet, since nothing here calls REST or names
a model type. New to `Cargo.lock`: `twilight-gateway`, `twilight-gateway-queue`, `twilight-model`,
`ordered-float` 2, `serde-value`, `serde_repr`, and `sha1_smol`.
`tokio-websockets` was already in the tree through the WhatsApp plugin; it gains `fastrand`,
`sha1_smol`, and `webpki-roots` there. All are MIT, Apache-2.0, ISC, BSD-3-Clause, or
CDLA-Permissive-2.0, as [Licence Check](#licence-check) says for the whole tree, so the licensing
of the repository is unchanged. `cargo tree -d` shows one `rustls` (0.23.46), one `rustls-webpki`,
and one `aws-lc-rs` for the plugin, no `ring` in its tree, and no duplicate of anything heavy
(the duplicates it lists, `hashbrown` and `syn`, are in the workspace already). The tests add
`tokio-websockets` with its `server` feature as a dev-dependency.

### Running the Tests

```sh
cargo test -p giles-channel-discord
```

No test contacts Discord, and none needs a token or a network beyond loopback.

- `config`, `status`: unit tests, the validation table and the status mapping.
- `session`: the state machine against `FakeDiscord` on paused time: statuses and the grace period,
  the delay sequence (1, 2, 4, ... 300 s), its reset after a healthy minute (and not after 59
  seconds), an invalid-session loop that must not Identify in a tight loop, a rejected token that is
  never polled again and a corrected restart, final closes, a stream that ends, and shutdown
  (closed once, even mid-delay, and bounded when the client hangs).
- `tests/sdk.rs`: the plugin served over loopback TCP by the real SDK with the fake client:
  `Describe`, `Configure` errors naming the key, a status that survives until the core acks it, and
  `NeedsAuth` over the wire. `tests/logs.rs` is the redaction test, a binary of its own because
  `tracing` caches whether a call site is enabled, which makes a subscriber installed for one test
  unreliable next to tests without one.
- `tests/gateway.rs`: `RealDiscord` against a Gateway server written in the test on loopback
  (`ConfigBuilder::proxy_url`, which the shard also uses for the `ws://` URL of this server; the
  REST override is for the issues that use REST): Hello, Identify (the token and exactly the
  `DIRECT_MESSAGES` intent), heartbeats and ACKs, `Ready`, a close with 4004 and the end of the
  stream, a dropped connection resumed with the same session, an invalid session that Identifies
  again, a reconnect request that resumes, a server that isn't there, and the clean close at
  shutdown (and no reconnect when the close is never answered). The Gateway server is
  `tests/loopback/mod.rs`, shared with `tests/logs.rs`. It plays Discord from its documentation, so
  it shows that the glue does what the library does, not that Discord behaves so (issue #20). The
  shard's one-second delay before each connect makes it take a few seconds.
- `real`: the conversions from the library's events, from a documented READY payload.

## What Needs a Real Bot

Issue #20 confirms, in addition to its own list:

1. A bot **without** the Message Content intent receives the text and attachments of DMs in
   `MESSAGE_CREATE` and in REST history (the premise of the whole design; the documentation says
   so on two pages and a third warns otherwise).
2. `after=<id>` returns the 100 messages **just after** the cursor, not the newest 100, so
   paging forward by the page's largest id has no gap. The documentation says only "newest to
   oldest".
3. The DM upload limit for a bot is 20 MiB per file and 25 MiB per request; how many files a
   message takes; which error a 21 MiB file gets.
4. The 2000-character limit counted on emoji (scalars or UTF-16 units).
5. A DM channel id is stable across a plugin restart, and `POST /users/@me/channels` returns the
   same id.
6. Which error a closed-DM user produces: on Create DM, on the first message, or both (50007);
   and that a user who shares no server with the bot gets the same.
7. A user can open a DM to the bot only after sharing a server, and what "DMs closed" does to the
   bot's replies in an existing channel.
8. Close codes and statuses: a wrong token (4004), a reset token, and that no other close can
   leave the plugin silently disconnected.
9. Resume after a short network drop (no catch-up pass, no duplicate), and a fresh Identify plus
   catch-up after a long one.
10. A signed attachment URL downloaded on arrival, and one refused after its `ex` time.
11. That a message with only a sticker, poll, or forward behaves as the inbound table says.
12. That no `MESSAGE_CREATE` ever arrives for a server channel with this intent set.
13. That Discord answers with a close frame when the shard closes the connection after a Reconnect
    (op 7) or an Invalid Session (op 9), which is how the plugin counts them for its reconnect
    delay ([The Library's Silent Reconnect](#the-librarys-silent-reconnect)), and what the first
    reconnect after a drop really costs in time (the loopback test shows only the library's own
    one-second sleep). Also how Discord ends a connection it drops: with a close frame, with a plain
    TCP close (which the client reports itself), or before Hello (which it cannot see: no status,
    retried about once a second, nothing sent).

What can't be verified at all without a real bot: Discord's real error codes under real
conditions, the rate limits' actual numbers (documented as variable), and the Resume window.
