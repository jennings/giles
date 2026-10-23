# WhatsApp Spike

TASKS.md 1.13 decides which library the WhatsApp channel plugin (3.6) is built on: `whatsapp-rust`
(PLAN section 10) or the `whatsmeow` fallback. The spike is a small CLI on `whatsapp-rust` in
`spikes/whatsapp/`. It links as a companion device, prints incoming messages, sends a DM, and
reuses its saved session after a restart. Everything that needs no phone has been verified. The
live run, which links a real account, is the owner's: follow the [test plan](#owner-test-plan) and
fill in [Results](#results). 1.13 stays open until then.

**Recommendation:** build 3.6 on `whatsapp-rust`, unless the live run fails one of the
[decision criteria](#decision-criteria). It is actively maintained, MIT licensed, covers everything
Giles needs, and keeps the plugin in Rust, so it can use `giles-plugin-sdk` (gRPC serving, the
`whatsapp()` renderer, unacked-event storage). A Go plugin on `whatsmeow` would have to rebuild all
of that in Go. The risks are its age (one year), a single main author, a fast-moving API, and fixes
that are on `main` but not in a release yet (see [Findings](#findings-for-36)).

## The libraries

Data from crates.io and GitHub on 2026-10-22.

|                     | [`whatsapp-rust`][wr]                                   | [`whatsmeow`][wm]                                   |
| ------------------- | ------------------------------------------------------- | --------------------------------------------------- |
| Language, license   | Rust, MIT                                               | Go, MPL-2.0                                         |
| Age, releases       | Repo 2025-07; 0.1.0 (2025-10) to [0.7.0][wr-crate] (2026-08-07) | Repo 2021-10; no tagged releases (Go pseudo-versions) |
| Activity            | Commits daily; 203 issues filed, 3 open issues/PRs; ~80k downloads/90 d | Last commit 2026-10-17; 85 open issues/PRs |
| Maintainers         | `jlucaso1` (~1,700 commits); next human contributor 19  | `tulir` (~1,270 commits); next contributor 135      |
| Known users         | Mostly the author's: [oxidezap/client][oxc], an ESP32 client, a Baileys port | [mautrix-whatsapp][mw] (Matrix bridge, years in use) |
| Multi-device        | Yes (companion device)                                  | Yes (companion device)                              |
| QR code linking     | Yes; QR rotates 60 s, then 20 s each, 6 codes           | Yes (`GetQRChannel`)                                |
| Pair code linking   | Yes, 8 characters, valid ~180 s, concurrent with QR     | Yes (`PairPhone`)                                   |
| Session store       | SQLite (Diesel, bundled SQLite); pluggable `Backend`    | `sqlstore`: SQLite or Postgres                      |
| DMs, groups         | Send and receive, E2E (Signal protocol)                 | Send and receive, E2E                               |
| LID handling        | `sender_alt`, `addressing_mode`, LID-PN mapping store   | `SenderAlt`, `AddressingMode`, LID map store        |
| Media               | Upload/download with encryption                         | Upload/download                                     |
| Inbound durability  | Opt-in hook: ack only after the app commits             | Opt-in `SynchronousAck`: ack after handlers return  |
| Docs                | [Book][wr-docs] (tracks `main`), rustdoc, examples      | [godoc][wm-docs], bridge code as reference          |

Both are unofficial clients of the WhatsApp Web protocol. `whatsapp-rust` warns that using it may
violate Meta's terms and get the account suspended; the same holds for `whatsmeow`.

## Findings for 3.6

From reading the 0.7.0 sources, building the spike, and one unscanned `link --qr` run.

- **The release needs nightly by default.** 0.7.0's default `simd` feature turns on
  `#![feature(portable_simd)]` in `wacore-binary`, which fails on stable. The spike turns default
  features off and lists the rest (`signal`, `sqlite-storage`, `tokio-native`, `tokio-runtime`,
  `tokio-transport`, `ureq-client`). `main` has no `simd` feature.
- **SQLite version clash with the workspace.** 0.7.0's `whatsapp-rust-sqlite-storage` requires
  `libsqlite3-sys` 0.37; `giles-store`'s `rusqlite` 0.40 uses 0.38. Cargo allows only one package
  per native `links` name in a resolve, so a 0.7.0 plugin can't be a member of the workspace. `main`
  already uses 0.38 and says it is meant to coexist with `rusqlite` 0.40. 3.6 should use the first
  release after 0.7.0 (or pin a `main` commit).
- **Fixes not released yet.** On `main` since 0.7.0: DM sends silently skipping devices without a
  session ([#1361]) and WhatsApp Business phones failing to pair ([#1439]). If the live run hits
  one of these, retest on `main` (see [Testing `main`](#testing-main)) before counting it against
  the library.
- **Messages are acked before the app stores them.** By default an inbound message is
  acknowledged to the server whether or not the app persisted it (at most once). The
  [inbound durability hook][wr-dur] (`BotBuilder::with_inbound_durability_hook`, in 0.7.0) defers
  the ack until the hook returns `Ok`. That is the hook the plugin should use to write unacked
  events to its data directory (PLAN section 8.5) before the server considers them delivered.
- **Event delivery.** `Bot` callbacks run concurrently by default, so their order isn't guaranteed.
  A raw `EventHandler` subscribed with `Client::subscribe_handler` runs inline on the event bus; the
  spike forwards every event through `ChannelEventHandler` (an unbounded channel) and handles them
  in order on one task. The plugin should do the same.
- **Linking maps onto `ChannelStatus::NeedsAuth`.** `Event::PairingQrCode { code, timeout }` gives
  the QR payload (`ref,noise_key,identity_key,adv_secret,client_type`) and its lifetime, so
  `AuthChallenge::QrCode` with `expires_at = now + timeout`. `Event::PairingCode` gives the pair
  code, for `AuthChallenge::PairCode`. A pair code needs the account's phone number up front, so it
  would come from the plugin config. After the 6th QR code expires, the client shuts itself down
  (`PairingQrCodesExhausted { disconnected: true }`) and a new `Client` on the same store must be
  built for more codes; the plugin should only do that on demand, not in a tight loop.
- **Session store.** One SQLite file (`SqliteStore::new(path)`), which goes in
  `GILES_PLUGIN_DATA`. The device identity (`Device::pn`, `Device::lid`) is readable before
  connecting, which tells the plugin whether it is linked. Two processes must never share a store:
  both would connect as the same device. The spike holds an exclusive `flock` on `<data>/lock`.
- **Identities.** A message has `sender`, `sender_alt` (the other form of the sender's address, when
  the server sends it), and `addressing_mode`. The LID-to-phone map learned from traffic is
  persisted and queryable (`Client::get_lid_pn_entry`). The spike reports the phone number as
  `sender_phone` and the LID as `sender_lid`, falling back to the map when `sender_alt` is missing.
  How often the phone number is unknown is one of the things the live run measures.
- **Echoes.** `MessageSource::is_from_me` marks messages sent from the linked account by any of its
  devices. Whether the server ever hands a device its own sends back is not documented; the spike
  logs the ids it sent in `<data>/sent-ids` so the live run can tell. Because every message the
  account sends is an echo, Giles should be linked to its own WhatsApp number: if it were linked to
  the owner's personal account, the owner's own messages would all be dropped as echoes.
- **Message ids.** `MessageInfo::id` is the WhatsApp message id (e.g. `3EB0...`), chosen by the
  sending device. The protocol identifies a message by its `MessageKey` (chat, `from_me`, id, and
  the participant in groups), so the plugin's event id should combine those rather than use the id
  alone.
- **Network.** The client connects to `wss://web.whatsapp.com` and fetches media over HTTPS, so the
  plugin's sandbox needs outbound TCP 443 only.
- **Cost.** A from-scratch debug build of the spike takes about 1.5 minutes on the development
  machine (macOS arm64, 10 cores) and 1m13s in the Linux container (4 jobs), with 2.3 GB of build
  artifacts and a 73 MB debug binary.
- **API churn.** The docs are written against `main` and are full of "Breaking change" notes; each
  release changes some types (for example `Event::PairingQrCode` became a struct). Pin the version
  and expect to adapt the plugin on upgrades.

## The spike

```text
spikes/whatsapp/
  Cargo.toml   standalone crate (outside the workspace), whatsapp-rust 0.7.0
  Cargo.lock
  src/main.rs  CLI, event loops, data directory
  src/ids.rs   pure helpers: phone parsing, sender ids, chat kind, echo classification
```

The root `Cargo.toml` excludes `spikes`, so the spike's dependencies never reach the workspace
build, its lockfile, or CI. Build it from its own directory:

```sh
cd spikes/whatsapp
cargo build --locked
cargo test --locked
./target/debug/whatsapp-spike --help
```

Commands (`--data <DIR>` is required and comes first):

| Command                       | What it does                                                            |
| ----------------------------- | ----------------------------------------------------------------------- |
| `link --qr [--light-terminal]` | Prints QR codes until one is scanned, then stays online 20 s and exits |
| `link --pair-code <PHONE>`    | Requests a pair code for the account's own number, prints it, same exit |
| `listen`                      | Prints one `message ...` line per received message until Ctrl-C         |
| `send <PHONE> <TEXT>`         | Connects, sends a text DM, waits for the server's ack, and exits        |

`link` on a linked data directory prints the linked account and exits; `listen` and `send` on an
unlinked one refuse. Phone numbers take a country code (`+1 555 123 4567`, `15551234567`). All
history sync is skipped, as Giles doesn't import history. `RUST_LOG=info` (or `debug`) shows the
library's logs on stderr.

The data directory (mode 0700) holds `whatsapp.db` (the session store), `sent-ids` (ids of messages
`send` sent), and `lock`. A `listen` line looks like this (wrapped here):

```text
message id=3EB0C431A1B2 at=2026-10-23T09:15:02+00:00 chat=15557654321@s.whatsapp.net kind=direct
  sender=15557654321@s.whatsapp.net sender_phone=+15557654321 sender_lid=100000012345678@lid
  push_name="Stephen" echo=no offline=false type=text media="" text="hello"
```

`kind` is `direct`, `group`, `status`, or `other` (channels, broadcast lists). `echo` is `no`,
`own-send` (an id in `sent-ids`), or `other-device` (sent from the linked account's phone or another
of its devices). `offline=true` marks a message queued on the server while the spike was offline.

### Verified without a phone

- `cargo build --locked` and `cargo test --locked` (12 unit tests: phone parsing bounds and errors,
  sender ids from PN/LID/alt/map, chat kinds, echo classification, sent-log round trip, QR rendering
  in both polarities, data directory mode and exclusive lock) on macOS arm64. Built and tested in
  the Linux container (arm64 Debian bookworm) as well.
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are clean.
- `--help` for every command; clap rejects `link` without a mode and `--qr` with `--pair-code`;
  bad phone numbers fail before any network access; `listen` and `send` refuse an unlinked data
  directory.
- One `link --qr` run, stopped with Ctrl-C after 15 s without scanning: the client connected,
  received pairing refs, printed a QR code valid for 60 s, and shut down cleanly (exit 0). Nothing
  was linked, and the data directory was deleted afterwards.

## Owner test plan

You need the phone with the WhatsApp account to link (ideally the number Giles will use) and a
second account to message it from. Run everything from `spikes/whatsapp` after
`cargo build --locked`:

```sh
S=./target/debug/whatsapp-spike
D=/tmp/whatsapp-spike      # any empty directory; never ~/.giles
A=+15551234567             # the linked account's number
B=+15557654321             # the second account's number
```

Only one spike process can use `$D` at a time, so stop `listen` (Ctrl-C) before running `send`.
Output contains phone numbers: redact them before pasting it anywhere public (this repository is
public).

1. **QR link.** `$S --data $D link --qr` (add `--light-terminal` if the QR code won't scan on a
   light background). On the phone: Settings > Linked devices > Link a device, scan. Expect
   `paired: phone <A>:<n>@s.whatsapp.net lid <...>@lid ...`, `connected; staying online 20 s`, then
   `linked; ...`. The phone lists the new device.
2. **Restart without relink.** `$S --data $D listen`. Expect `reusing session for <A> ...`,
   `listening`, `connected`, and no QR code.
3. **Receive a DM.** From B, send `hello` to A. Expect a line with `kind=direct`,
   `sender_phone=<B>`, a `sender_lid`, `echo=no`, `text="hello"`. Note whether `sender` is a phone
   (`@s.whatsapp.net`) or LID (`@lid`) address, and whether both `sender_phone` and `sender_lid` are
   filled in.
4. **Echo of the linked account.** On A's phone, send a message to B. Expect a line with
   `echo=other-device` (`chat` is B's chat).
5. **Send.** Stop `listen`. `$S --data $D send $B "hi from the spike"`. Expect `sent id=...` and
   `server ack at ...`; B receives the message. Then start `listen` again and note whether that
   message shows up at all (expected: not; if it does, it should say `echo=own-send`).
6. **Offline delivery.** With `listen` stopped, send two DMs from B. Start `listen`: both should
   arrive with `offline=true`, in order.
7. **Reconnect.** While `listen` runs, turn the computer's network off for 2 minutes and back on.
   Expect `disconnected (...)` then `connected`, no logout, and a DM sent from B during the outage
   arrives.
8. **Group (optional).** Add A to a group with B and send from B. Expect `kind=group`,
   `chat=<...>@g.us`; note which of `sender_phone`/`sender_lid` are filled in.
9. **Media (optional).** Send A an image with a caption from B. Expect `type=media`,
   `media="image"`, and the caption as `text`.
10. **Soak.** Leave `$S --data $D listen | tee -a $D/listen.log` running for 24 hours, then send a
    DM from B. Expect it to arrive and no `logged out` or `undecryptable` lines in the log.
11. **Logout.** With `listen` running, remove the device on A's phone (Linked devices > the device >
    Log out). Expect `listen` to stop with `logged out: ...`.
12. **Pair code link.** `rm -rf $D`, then `$S --data $D link --pair-code $A`. Expect `pair code:
    XXXXXXXX`. On the phone: Linked devices > Link a device > Link with phone number instead, and
    enter the code. Expect `paired: ...` as in step 1, then repeat step 2.
13. **Clean up.** Remove the device on the phone and `rm -rf $D`.

If a step fails, rerun it with `RUST_LOG=info` (or `debug`) and keep the stderr log.

### Results

Fill in the result (pass, fail, or skipped) and notes for each step, plus the date, the phone's
platform, and whether A is a consumer or Business account. Then record the decision in TASKS.md
1.13 and tick it.

| Step                  | Result | Notes |
| --------------------- | ------ | ----- |
| 1. QR link            |        |       |
| 2. Restart            |        |       |
| 3. Receive DM         |        |       |
| 4. Echo               |        |       |
| 5. Send               |        |       |
| 6. Offline delivery   |        |       |
| 7. Reconnect          |        |       |
| 8. Group              |        |       |
| 9. Media              |        |       |
| 10. Soak              |        |       |
| 11. Logout            |        |       |
| 12. Pair code link    |        |       |

### Decision criteria

Choose `whatsapp-rust` if steps 1 through 7, 10, 11, and 12 pass, with at most a problem that a
`main` commit fixes (then 3.6 waits for, or pins, that fix).

Switch to `whatsmeow` if any of these hold on both 0.7.0 and `main`:

- Linking fails for the account Giles will use (QR or pair code; both are needed, as the admin page
  offers both).
- The saved session doesn't survive a restart, or the device is logged out without the owner
  removing it.
- DMs are lost, arrive undecryptable, or sends report success but don't arrive.
- The client doesn't recover from a network outage on its own.
- Echoes can't be told apart from other messages.

Not a reason to switch: a DM whose phone number is unknown (only a LID). That is the protocol, not
the library; `whatsmeow` gets the same addresses. Record how often it happens; 3.6 then needs a rule
for it (PLAN section 3.2 keys identities on the phone number).

### Testing `main`

To retest on unreleased fixes, replace the `whatsapp-rust` dependency in
`spikes/whatsapp/Cargo.toml` with a pinned commit (`main` has no `simd` feature, so its default
features work on stable):

```toml
whatsapp-rust = { git = "https://github.com/oxidezap/whatsapp-rust", rev = "<commit>" }
```

and run `cargo update -p whatsapp-rust` before building. Pin a commit from 2026-09-30 or later:
`main` from 2026-09-03 until that day's
[keepalive fix](https://github.com/oxidezap/whatsapp-rust/issues/1567) has a keepalive that never
pings, so a dead connection goes unnoticed (0.7.0 isn't affected). The development machine's git
config rewrites `https://github.com/` to SSH, which Cargo's built-in git can't use; set
`CARGO_NET_GIT_FETCH_WITH_CLI=true` so it fetches with the `git` CLI and your SSH agent. The API on
`main` may differ from 0.7.0; fix compile errors in `src/main.rs` as needed.

[#1361]: https://github.com/oxidezap/whatsapp-rust/issues/1361
[#1439]: https://github.com/oxidezap/whatsapp-rust/issues/1439
[mw]: https://github.com/mautrix/whatsapp
[oxc]: https://github.com/oxidezap/client
[wm]: https://github.com/tulir/whatsmeow
[wm-docs]: https://pkg.go.dev/go.mau.fi/whatsmeow
[wr]: https://github.com/oxidezap/whatsapp-rust
[wr-crate]: https://crates.io/crates/whatsapp-rust
[wr-docs]: https://whatsapp-rust.jlucaso.com
[wr-dur]: https://whatsapp-rust.jlucaso.com/advanced/inbound-durability
