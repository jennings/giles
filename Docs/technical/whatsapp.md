# WhatsApp channel

`plugins/giles-channel-whatsapp` (`publish = false`) is the WhatsApp channel plugin (PLAN sections
3.3, 8.5, and 10): a binary on the plugin SDK ([Plugin SDK](plugin-sdk.md)) whose `Channel` is a
linked device on the WhatsApp Web protocol, built on `whatsapp-rust`.
`ChannelInfo { identity_namespace: "whatsapp", output: Blocks, can_initiate: true }`. User-facing
behavior and settings: [WhatsApp](../user/whatsapp.md). Why this library:
[WhatsApp spike](whatsapp-spike.md).

**Verified against a fake client only.** The owner chose to build on `whatsapp-rust` without waiting
for the live device run ([issue #1](https://github.com/jennings/giles/issues/1), still open), so
nothing here has touched the WhatsApp service. Everything above the `real` module is tested against
a fake client; `real` is a thin layer over the library, covered by compilation and tests of its pure
conversion functions, and is verified for real only by the
[live test plan](whatsapp-spike.md#owner-test-plan), which is also how to check this plugin once
someone links an account.

## The library, and why it's a Git dependency

`whatsapp-rust` is pinned to a commit of `main`, not to a release:

```toml
[dependencies.whatsapp-rust]
git = "https://github.com/oxidezap/whatsapp-rust"
rev = "9b9fdb7e5c4211cdc7c837abb6a9ae9c597e35c5"
```

(`Cargo.lock` records the commit too.) The latest release, 0.7.0 (2026-08-07), can't be a member of
this workspace, for the two reasons the spike found:

- Its default `simd` feature needs a nightly compiler (`portable_simd`); the spike turned default
  features off, but that's the lesser problem.
- Its SQLite store (`whatsapp-rust-sqlite-storage` 0.7.0) requires `libsqlite3-sys` 0.37, while
  `giles-store`'s `rusqlite` 0.40 uses 0.38. Cargo allows one package per native `links` name in a
  resolve, so the two can't coexist.

No release after 0.7.0 exists (checked on crates.io on 2026-10-25). `main` already uses
`libsqlite3-sys` 0.38 (its manifest says it's meant to coexist with `rusqlite` 0.40), has no `simd`
feature, and carries the fixes the spike listed as unreleased (DM sends skipping devices without a
session, #1361; Business phones failing to pair, #1439) and the keepalive fix of 2026-09-30. The
pinned commit, `9b9fdb7e5c42`, is the head of `main` on 2026-10-23. The resolve has one
`libsqlite3-sys` (0.38.2), so the plugin is an ordinary workspace member, and `cargo build --locked`
(CI and the container build) works: Cargo fetches the git source with its built-in libgit2 over
HTTPS, and `Cargo.lock` pins the commit.

The crate's version number on `main` still says 0.7.0, which is why Cargo shows
`whatsapp-rust v0.7.0 (https://github.com/oxidezap/whatsapp-rust?rev=...)`: the code is newer than
the release.

Features: default features off, then `sqlite-storage`, `sqlite-storage-bundled` (compile SQLite in
tree, like `giles-store`), `tokio-native`, `tokio-runtime`, `tokio-transport`, `ureq-client`. The
spike's `signal` feature (Ctrl-C handling) is left out: the SDK owns shutdown.

To move the pin: pick a newer `main` commit (or a release, once one is built on `libsqlite3-sys`
0.38), change `rev`, run `cargo update -p whatsapp-rust`, and fix what the API changed. Expect work:
the API moves fast. Between 0.7.0 and the pinned commit it changed the protobuf layer (`prost` to
`buffa`: optional message fields are `MessageField`), the store constructor (`SqliteStore::open`),
the event types, and the media builders (`#[non_exhaustive]` option structs with `with_*` methods).
Only `real/` touches these. On a development machine whose git config rewrites `https://github.com/`
to SSH, fetching the git source needs `CARGO_NET_GIT_FETCH_WITH_CLI=true` and an SSH agent, or
`GIT_CONFIG_GLOBAL=/dev/null` with the same variable.

## Licensing

`giles-channel-whatsapp` is `AGPL-3.0-only`; the rest of the repository is `MIT OR Apache-2.0`.

- **Why.** The dependency tree has one AGPL component: `wacore-libsignal`, a crate of
  `whatsapp-rust` (a port of Signal's `libsignal`). Its 31 source files carry
  `SPDX-License-Identifier: AGPL-3.0-only` and `Copyright Signal Messenger, LLC`, although its
  manifest, like the rest of `whatsapp-rust`, says `MIT`. Checked by searching the pinned checkout
  for `AGPL` and by reading the license field of every crate in `cargo metadata`: no other crate in
  the tree is AGPL (`self_cell` offers `GPL-2.0-only` as an alternative to Apache-2.0, which isn't
  chosen). The binary links that code, so it is a combined work.
- **How it's kept to one crate.** The crate sets its own `license = "AGPL-3.0-only"` instead of
  inheriting the workspace's, and has `LICENSE-AGPL-3.0`, a `README.md` explaining the license, and
  `THIRD-PARTY`, the notices and source offer. `whatsapp-rust` is a dependency of this crate alone,
  not of `[workspace.dependencies]`, and nothing depends on the plugin: it is a binary run as its
  own process, which talks to the core over gRPC (`giles-plugin-sdk`) and is found by name.
  `license_tests` checks the license field, that `THIRD-PARTY` names the pinned revision, and that
  no other manifest mentions the plugin or the library.
- **The image.** It ships the plugin binary, so the OCI license label is
  `(MIT OR Apache-2.0) AND AGPL-3.0-only`, and the license texts and `THIRD-PARTY` (the AGPL source
  offer: the repository at the image's commit, and the pinned library revision) are in
  `/usr/share/doc/giles/` ([Container image](container-image.md)).
- **Not legal advice.** This records how the code is licensed and what the project does about it. A
  decision to relicense the plugin or drop the library would change this section, the crate's
  `license`, the image label, and `ci.yml` together.

## Modules

| Module      | Contents                                                                           |
| ----------- | ---------------------------------------------------------------------------------- |
| `config`    | `Config` (serde, unknown keys refused) validated into `Settings`                   |
| `backend`   | The seam: `Connector`, `Backend`, `WaEvent`, and the domain types (`Incoming`, ...) |
| `real`      | The seam on `whatsapp-rust`: `RealConnector`, the event handler, the durability hook |
| `real::convert` | Pure conversions from the library's events and messages to the seam's types     |
| `identity`  | Phone parsing, `SenderIds` (phone, LID alias), recipients (pure)                   |
| `inbound`   | `Incoming` to `ChannelEvent`: echoes, kinds, identities, media (the drop rules)    |
| `outbound`  | Target resolution, rendering, attachments, mapping errors                          |
| `split`     | Splitting rendered text into messages, keeping code fences balanced (pure)         |
| `status`    | Client events to `ChannelStatus` (pure)                                            |
| `session`   | The task that owns the client: status, batches, the durability handshake, restarts |
| `datadir`   | The session store path, the lock, the logout marker                                |
| `media`     | `MediaStore`: the SDK's `Blobs`, or an in-memory fake in tests                     |
| `channel`   | `WhatsApp` (the `Channel`), `configure` (the SDK entry point)                      |

### The seam

The plugin's logic never sees the library's client. `Connector::connect` starts a client and returns
a `Connection`: a `Backend` (own addresses, LID-to-phone lookup, phone-number resolution, send text,
send media, download media, shutdown) and an unbounded stream of `WaEvent`s (QR code, pair code,
link failure, paired, connected, disconnected, logged out, fatal, stopped, and message batches).
Chats and senders are strings and small values (`Address`: a phone or a LID), not library types; the
one exception is the media payload (the library's protobuf message), which only the backend can
download. Tests implement the seam with `testing::FakeBackend` and `FakeConnector`.

`real::RealConnector` opens the SQLite store (`SqliteStore::open`), builds the `Bot` with
`skip_history_sync` (Giles never imports history), the device name as the OS in
`DevicePropsOverride` (WhatsApp applies it at pairing), a pair code when configured, a raw
`EventHandler`, and the durability hook, and runs it on a task. When that task ends it sends
`WaEvent::Stopped`.

## Event flow and durability

Messages reach the plugin only through the library's inbound durability hook
(`BotBuilder::with_inbound_durability_hook`), not through `Event::Messages`: with the hook
registered, the library acknowledges a message to WhatsApp only after the hook returns `Ok`, and
redelivers it on the next connect after an `Err` (PLAN section 8.5: a push source must not
acknowledge before the event is on disk). The raw `EventHandler` (run inline on the library's event
bus, so in order) sees everything else, and its `interest()` is narrowed to the kinds
`real::convert::event` translates; `Messages` isn't among them.

```text
client ──WaEvent──▶ session ──Outgoing──▶ Channel::events ──▶ SDK spool (persist, then poll)
   ▲                   │                        │
   └── Commit::done ◀──┴──── `persisted` ◀──────┘ fires when the SDK polls the stream again
```

1. The hook converts the batch (`convert::batch` drops messages with no text and no supported media:
   reactions, edits, calls) and sends it to the session as `WaEvent::Messages` with a `Commit`, then
   waits on the commit's receiver. `Ok` only for `true`; a `false` or a dropped `Commit` is an
   error, so the library withholds the acknowledgement.
2. The session turns each message into a `ChannelEvent` (`inbound::event`: echoes and non-chats
   dropped, identities resolved, the attachment downloaded into the SDK's blob store) and sends it
   to the stream's channel, in order. The last event of the batch carries a `persisted` sender.
3. `Channel::events` is a `stream::unfold` whose state holds the `persisted` sender of the event it
   last yielded. The SDK persists an event before it polls the stream for the next one
   ([Plugin SDK](plugin-sdk.md#events-and-persistence)), so the next poll fires it: the event is on
   disk. A stream dropped before the next poll drops the sender instead.
4. The session resolves the `Commit`: `true` when `persisted` fired (or there was nothing to keep: a
   batch of echoes), `false` on a failure, a shutdown, or a dropped sender. The hook returns, and
   the library acknowledges the messages.

What this gives: when WhatsApp is told a message arrived, the event file is in `giles-sdk/events/`
(`sdk_tests` checks that against real files); nothing is acknowledged if the SDK can't persist (it
retries without polling), if the blob store fails (`inbound::event` returns the error, so the whole
batch is redelivered), or if the plugin stops first. Delivery is at least once: a redelivered
message has the same event id (below), which the SDK's spool and the core's inbox deduplicate.

The session handles one thing at a time, so while a batch waits for the SDK, status events queue
behind it. That is deliberate (order matters more) and short: the SDK persists in milliseconds. With
no core connected the SDK doesn't poll at all, so the first message holds the session, and the
library's acknowledgement, until the core opens `Events`; the SDK starts its pump only then.

The hook runs inside the library's receive pipeline, which its documentation warns against blocking
on client operations for the same sender. The plugin only downloads media (the CDN, not a Signal
session) and never replies from inside the hook, so there's no lock to wait on, but a slow download
does hold back inbound processing. Downloads are bounded by `max_blob`.

### Event ids

An event id is `<chat>:<sender>:<message id>`, with the chat and sender JIDs as the library reported
them (devices stripped). WhatsApp message ids are chosen by the sending device and are unique only
per chat and sender, which is what the library's hook documentation says to key on. The raw JIDs are
used, not the derived identities, so a redelivery has the same id even after the LID-to-phone map
learned something new. Status events get `status:<nanos>:<counter>`.

## Status

`status::for_event` maps client events; the session adds the disconnect grace and restarts.

| Client event                                        | Status                                                  |
| --------------------------------------------------- | ------------------------------------------------------- |
| QR code (when `link = "qr"`)                        | `NeedsAuth` `QrCode`, `expires_at` = now + the code's timeout |
| Pair code (when `link = "pair_code"`)               | `NeedsAuth` `PairCode`, `expires_at` = now + its timeout |
| Connected                                           | `Connected`                                             |
| Disconnected, not back within 10 s                  | `Error`: disconnected, the client is reconnecting       |
| QR codes exhausted and the client closed itself     | `Error`: no QR code was scanned in time; restart        |
| Link failure (pair code refused, QR scanned without multi-device) | `Error` with the reason; restart           |
| Logged out                                          | `Error`: logged out; restart links a new device         |
| Connect failure the library retries (503, 500)      | A disconnect: `Error` only after 10 s, like any drop    |
| Outdated, temporary ban, replaced, any other connect failure | `Error` with the reason; final                 |
| Client stopped or its event stream ended            | `Error`: stopped, starting it again                     |
| Client couldn't start                               | `Error`: couldn't start the client                      |

With a pair code configured the client offers QR codes too (both flows run at once); only the
configured one is reported, or the two would replace each other. QR exhaustion with the socket left
up (a pair code outstanding) says nothing. A repeat of the last status isn't sent again; each QR
code differs, so each is. The messages never contain a phone number or message text.

Status events pass through the SDK's spool like any other event, so a core that is away during
linking sees the queued codes in order when it connects; the newest is what it shows.

### Reconnection, and linking on demand

The library reconnects dropped connections by itself, so the session only reports (after the grace
period, since WhatsApp recycles connections) and gates sends: `Backend` is published to a `watch` on
`Connected` and withdrawn on `Disconnected`.

When the client ends, the session decides:

- **Terminal** (logged out, fatal, QR codes exhausted with the client closed): the session stops and
  idles with its error status. Building a new client on the same store would ask WhatsApp for
  pairing again (QR exhaustion) or fail again (a ban, a logout), so it only happens on demand: the
  operator restarts the plugin (the admin page's **Restart**), which is the only trigger a channel
  plugin has. This is the spike's rule, "a new `Client` on the same store only on demand, never in a
  tight loop".
- **The client died by itself** (`Stopped`, its event stream ended, `connect` failed): a new client
  after `restart_min` (5 s), doubling to `restart_max` (5 min), back to the minimum after a client
  has run for `stable_after` (2 min).

A logout also writes the marker `logged-out` to the data directory. `configure` (the next start)
checks it: it renames `whatsapp.db`, `-wal`, and `-shm` to `*.logged-out` (keeping the dead session
for inspection, replacing an older one), removes the marker, and so starts unlinked. Without that, a
restart would reuse the dead session and be logged out again.

### Shutdown

The SDK owns the channel and returns from `run` when the core stops the plugin (or, as a remote
plugin with `serve`, on `SIGINT` or `SIGTERM`), and a process that exited then would cut the session
task off before it disconnects the client (which flushes the session store). `configure` leaves the
session's stop token and `JoinHandle` in a `SessionSlot` that `main` keeps; after `run` returns,
`main` cancels the session and awaits it for `Timing::shutdown_wait` (10 s) plus a second. The
session stops on its next event or wait, also while still connecting, calls `Backend::shutdown`
(itself bounded by `shutdown_wait`), and ends. The core's own kill deadline still applies on top.

## Session store and lock

The library's SQLite store is `<GILES_PLUGIN_DATA>/whatsapp.db`. `DataDir::open` takes an exclusive
`flock` on `<data>/lock` (`File::try_lock`) at `configure`, held for the life of the channel, and
fails with a clear error if another process holds it: two clients on one session are the same device
twice, and WhatsApp would disconnect one with a "replaced" error forever. The operating system
releases the lock when the process exits, however it exits. A remote plugin's data directory works
the same way.

## Identity, echoes, and authentication

- **Identity.** `SenderIds` collects the phone number and the LID from the sender's address and the
  alternate address the server sent with it (`sender_alt`), then asks the backend for the
  LID-to-phone mapping the client learned (`Client::get_lid_pn_entry`) if no number is known yet.
  The canonical `sender` is `+<digits>`; the LID, when known as well, is the only `sender_aliases`
  entry (`<user>@lid`). If the number is unknown, `sender` is the LID and there are no aliases: a
  stable id the owner can list (PLAN section 3.2 keys on the number, so this is the fallback the
  spike asked for). Device suffixes are dropped.
- **Echoes.** A message from the linked account itself is dropped: `from_me` (the protocol's flag
  for the account's own sends from any device), or a sender address equal to the linked account's
  (`Backend::own_addresses`), in case a message arrives without the flag. The decision follows PLAN
  section 3.3 ("WhatsApp delivers copies of messages sent from the linked account"): there is no way
  to tell the plugin's own sends from the owner typing on the phone without remembering sent ids,
  and a rule that accepted "from another device of my own account" would let the agent answer itself
  in the self-chat. So every message of the account is an echo, including the "Message yourself"
  chat, and the user guide says to link Giles to a number of its own. (The spike's `echo=own-send` /
  `other-device` distinction is for the live run to learn whether the server hands a device its own
  sends back; this plugin doesn't need to know.)
- **`sender_authenticated = true`** for every message that gets this far. WhatsApp messages (direct
  and group) are end-to-end encrypted with the Signal protocol, and the client decrypts a message
  only with the session or sender key of the device that sent it, so the sender on a decrypted
  message is the account whose keys it was encrypted with. The residual trust is the first contact,
  where the key comes from WhatsApp's server, as in the app itself. Messages that skip decryption
  (channel posts, which the library doesn't pass through the hook) are dropped with the other
  non-chats, and so is anything from a sender with neither a phone nor a LID address. Placeholder
  recoveries (messages the phone re-sent after a decryption failure) also bypass the hook and aren't
  delivered: nothing authenticates their sender the same way.
- **Conversations.** A direct chat's id is the person's phone JID (`<digits>@s.whatsapp.net`) when
  the number is known, else their LID JID, so one person is one conversation however WhatsApp
  addressed a message. A group's id is the group JID (`@g.us`) with kind `group` (the core drops
  groups, PLAN section 1.1; the plugin still reports them). Status updates, broadcast lists, and
  channels are dropped.
- **Content.** Text (`conversation` or extended text), the caption of an image, video, or document,
  and a reply's `stanza_id` as `reply_to`. `get_base_message` unwraps disappearing and view-once
  wrappers. One attachment per message: image, video, audio, document, or sticker.

## Outbound

`outbound::send`:

1. Renders the CommonMark with `giles_plugin_sdk::render::whatsapp` and splits it into parts of at
   most 4,096 characters (`split::split`): paragraph break, then line, then space, then character
   boundary, with an open code fence closed at the end of a part and reopened at the start of the
   next (8 characters of each part's budget are reserved for that). `reply_to` is ignored: a
   WhatsApp quote needs the quoted message's content, which the plugin doesn't keep.
2. Checks everything that can be refused before anything is sent: nothing to send, an attachment
   over `max_blob`.
3. Resolves the target: a `conversation_id` must be a chat the plugin reports (a `@s.whatsapp.net`,
   `@lid`, or `@g.us` JID, never the status feed, a broadcast list, or a channel) and is used as it
   is; a `recipient` is `+<digits>` (resolved with `contacts().is_on_whatsapp`; a number with no
   account is `UNAVAILABLE`, like an IRC account that isn't online) or `<digits>@lid` (used as it
   is); anything else is `INVALID_ARGUMENT`.
4. Sends the attachments, then the text. The text is the first captionable attachment's caption (not
   audio) when it's one part of at most 1,024 characters. The kind comes from the media type: JPEG,
   PNG, and WebP are images, MP4 and 3GP videos, AAC, MP3, MP4, AMR, and Ogg audio, and everything
   else a document with the blob's name (default `attachment`).
5. Returns the id of the first message sent. Parts go one after another; if a later one fails, the
   earlier ones are delivered and a retry of the whole send repeats them.

Backend errors keep their meaning: `Unavailable` (not connected, network, server) stays
`UNAVAILABLE`, which the outbox retries; `Rejected` (an address WhatsApp refuses) is
`INVALID_ARGUMENT`; anything else is a generic error, which the core also retries
([Plugin Host](plugin-host.md#remotechannel)).

`Channel::send` first needs a connected backend: a reply into a conversation waits up to `send_wait`
(30 s) for a dropped connection to return, a proactive send fails at once (the outbox retries it).

## Media

`inbound::fetch` downloads an attachment into `Blobs::put` before the event is emitted (the SDK
requires the blob to exist first). An attachment whose declared size is over `max_blob`, whose bytes
turn out larger (the download is also bounded by the library with `DownloadLimits`), or that can't
be fetched (an expired CDN link, a decryption failure) doesn't fail the message: its text gets a
note, `[image not received: ...]`, and it is delivered. A blob-store failure does fail it, so it is
redelivered. The core applies its own `max_blob` to what the plugin hands over; the plugin's
`max_blob` can't read it, so the user guide says to keep them in step.

## Configuration

`Settings::new` validates what serde can't, naming the key (`invalid config: <key>: <reason>`):
`link` (`qr` or `pair_code`), `phone_number` (required by `pair_code`, refused by `qr`, 7 to 15
digits without a leading `0`, parsed by `identity::parse_phone`), `device_name` (1 to 64 characters,
no control characters, default `Giles`), `max_blob` (default 50 MiB, not 0). Unknown keys are
refused. The phone number is the only sensitive value and is never logged or put in a status:
`Config` has no `Debug`, `Link`'s `Debug` prints `<redacted>` for it (tested), and the startup log
line names only the mode (`link = "pair_code"`).

## Logging

The binary's default filter is `warn,giles_channel_whatsapp=info,giles_plugin_sdk=info` (`GILES_LOG`
overrides it): the library logs each connection step at `info`, including the pair code, and its
JIDs. The plugin's own logs name no number or text; a dropped message is logged with its id and
chat, not its content.

## Tests

- `inbound`, `outbound`, `identity`, `split`, `status`, `config`, `datadir`: pure logic (config also
  checks that no `Debug` output holds the phone number) and the drop and error rules, against the
  fake backend.
- `channel/tests.rs`: the channel on the fake client, with short timings: linking (QR with its
  expiry, pair code, link failure), `Connected`, disconnects inside and outside the grace period, QR
  exhaustion and logout and fatal errors never retried, restarts after a dead client or a failed
  start, the durability handshake (an event is acknowledged only after the SDK polls again, batch
  order, a stalled persist, a dropped stream, a failed blob store, dropped messages), and sends
  (waiting for the connection, resolving recipients), and shutdown (the client is disconnected and
  awaited, a stuck client is bounded, a stop while connecting ends the session).
- `sdk_tests.rs`: the channel served by the real SDK over loopback gRPC with the fake client:
  `Describe` and `Configure`, the event file being on disk before the client is released, redelivery
  from the spool, an attachment as a fetchable blob until the ack, a send with a blob the core put,
  and `NeedsAuth` over the wire.
- `real::convert` tests build the library's own event and message types and check the conversions;
  `real` tests the hook's wait on the commit.
- `docs_tests`: the TOML examples in the user guide are valid configs.

Not tested, because it needs WhatsApp: the library's own behavior (pairing, reconnection,
decryption, sending, media), the real `Backend` methods (`send_text`, `send_media`, `download`,
`resolve_phone`, `phone_for_lid`, `own_addresses`), and the `real::RealConnector` assembly. They are
short and mostly one library call each. Assumptions in them that the live run must confirm: the hook
delivers the messages Giles should see (including from a linked device that is a Business account),
`sender_alt` and the LID map identify senders in groups and DMs as the spike saw, `is_on_whatsapp`
finds registered numbers, and the media builders produce messages WhatsApp displays.
