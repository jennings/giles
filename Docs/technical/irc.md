# IRC Channel

`plugins/giles-channel-irc` (`publish = false`) is the IRC channel plugin: a library crate (so tests
drive the channel directly) and a binary that serves it with the plugin SDK. User-facing behavior
and settings are in [IRC](../user/irc.md).

## Libraries

No Rust IRC client library fits, so the client is written on `tokio`, with two libraries:

- **`irc-proto` 1.1** (default features off) parses and serializes IRC messages: IRCv3 tags with
  unescaped values, the source split into nick, user, and host, and typed commands and numerics
  (`CAP`, `AUTHENTICATE`, `ACCOUNT`, `JOIN` with extended-join's account, SASL numerics 900-908).
  It's the protocol half of the `irc` crate, the most used Rust IRC library (maintained; 1.1.0 in
  2025), with one dependency (`thiserror` 1). Its tokio codec is off: the plugin frames lines
  itself.
- **`tokio-rustls` 0.26** (default features off) carries the connection over TLS with the client
  config built from `giles_plugin_sdk::tls::root_store()` (bundled Mozilla roots) and the
  `aws-lc-rs` provider, the same trust store and crypto as the rest of Giles. No native-tls or
  OpenSSL.

The full `irc` crate was rejected: its TLS is native-tls (on by default) or rustls with
`rustls-native-certs` plus `webpki-roots`, with no way to hand it a `ClientConfig`, so it can't use
the bundled roots or a client certificate from a secret; it doesn't negotiate SASL (the application
scripts `CAP` and `AUTHENTICATE` itself either way); and its client hides the per-line control this
plugin needs for flood protection and `WHOIS` lookups. Other crates (`ircv3_parse`, `irctokens`,
`vinezombie`) are parsers or frameworks with few users.

`irc-proto` quirks the plugin works around: `CAP` replies put the capability list in the third or
fourth field depending on whether a `*` continuation marker is present (`conn::Session::on_cap`
normalizes them); numerics it doesn't know (`330 RPL_WHOISACCOUNT`) arrive as `Command::Raw`; its
`strip_formatting` misses italics, strikethrough, monospace, and hex colors, so `inbound` strips
formatting itself.

## Modules

| Module     | Contents                                                                         |
| ---------- | -------------------------------------------------------------------------------- |
| `config`   | `Config` (serde, unknown keys refused) validated into `Settings`; TLS config     |
| `casemap`  | `CaseMapping` from ISUPPORT `CASEMAPPING`: folding nicks, channels, accounts     |
| `inbound`  | `PRIVMSG` to `ChannelEvent`: identities, conversations, text, event ids (pure)   |
| `outbound` | Line budget, rendering with `max_lines`, `Throttle`, recipients, SASL PLAIN (pure) |
| `conn`     | The connection task: connect, register, serve, reconnect                        |
| `lib`      | `Irc` (implements `Channel`), `Timing`, `configure` (the SDK entry point)        |

`configure` validates the config, builds the TLS config, and starts the connection task; it never
waits for the server, so a server that's down doesn't fail `Configure` (and so doesn't make the
plugin host restart the plugin). `Irc` talks to the task over channels: inbound events (bounded at
256; when full, reading from the server waits, which only happens while the SDK can't persist), send
requests with a oneshot for the result, and a `watch` of "registered". Dropping `Irc` cancels the
task, which sends `QUIT`.

## Configuration

`Settings::new` checks what serde can't, naming the key in the SDK's format
(`invalid config: <key>: <reason>`) and never echoing a value: secrets (`password`, `sasl.password`,
`client_cert`, `client_key`) are wrapped in `Secret`, whose `Debug` is `<redacted>`.

- `server` is `host`, `host:port`, `[v6]`, `[v6]:port`, or a bare IPv6 address; a port in both
  `server` and `port` is an error. The host must be a valid TLS server name (`ServerName`).
- Plain TCP to a non-loopback host needs `insecure = true`, and passwords (server or SASL PLAIN)
  never go over plain TCP to another machine. The account tags that identify senders are only as
  good as the connection that carries them.
- `nick`: RFC 2812 characters (letters and ``[]\`_^{|}``, then digits and `-`), at most 64 bytes.
  `network` must be a valid namespace segment (no `:`, whitespace, or control characters).
  `channels[i]`: `#&+!` prefix, no spaces, commas, `:`, or control characters.
- `sasl.mechanism = "external"` needs `client_cert` and `client_key` and takes no username or
  password; `plain` needs a password. A client certificate needs TLS; `rustls` checks that the key
  matches the certificate (a trial `ClientConfig` build at validation time).

## Connection

`conn::run` loops: connect (TCP, then TLS; `Timing::connect_timeout`), run a session, report the
error, wait, double the delay (`reconnect_min` 1 s up to `reconnect_max` 5 min; reset to the minimum
when the session had been registered for `stable_after`, 1 min). While waiting, send requests are
refused with `Unavailable`. The `registered` watch is false outside a registered session.

A session writes `CAP LS 302`, `PASS` (if set), `NICK`, `USER`, then serves lines, requests, and one
timer (the earliest of: the throttle's next slot, the registration deadline, keepalive, the `WHOIS`
deadline):

- **Capabilities**: from the full `LS` (multi-line replies collected until the last), it requests
  `account-notify`, `account-tag`, `echo-message`, `extended-join`, `message-tags`, and
  `server-time` if offered, plus `sasl` when configured. SASL that isn't offered (or whose `sasl=`
  value lacks the mechanism), a `NAK` with SASL configured, or a welcome (`001`) without a completed
  SASL login (a server that ignored `CAP`) end the session: configured SASL is never skipped.
  `CAP DEL` removes capabilities later.
- **SASL**: `AUTHENTICATE <mechanism>`, then on `AUTHENTICATE +` the payload: PLAIN is base64 of
  `user\0user\0password` in 400-byte chunks (a `+` after a final full chunk); EXTERNAL is `+`.
  `903`/`907` lead to `CAP END`; `902`/`904`/`905`/`906` end the session with the server's text.
- **Registration**: `001` sets our nick from its first parameter, queues `JOIN`s, sets `registered`,
  and reports `Connected`. `005` sets `CASEMAPPING`, `CHANTYPES`, `STATUSMSG`. `433`/`437` before
  registration try `nick_`, `nick__`, `nick___`; `432`, `464`, `465` end the session. Registration
  must finish within `register_timeout` (60 s).
- **Keepalive**: any line resets the quiet clock; after `ping_after` (2 min) quiet the plugin sends
  `PING :giles`, and after `ping_timeout` (1 min) more it gives up. Server `PING`s are answered at
  once.
- **Framing**: lines end in LF (CR optional); invalid UTF-8 is decoded lossily (a stray Latin-1 byte
  in a channel mustn't drop the connection); a line over 8703 bytes (8191 of tags plus 512) is
  skipped. Writes have a `write_timeout` (30 s) so a server that stops reading can't wedge the
  session. Outgoing lines are never logged (they may carry passwords).

Status events (`status:<time>:<n>` ids) are emitted only when the status changes: `Connected` on
registration, `Error { message }` on every failure with a new message.

## Inbound

`inbound::event` maps a `PRIVMSG` (pure, unit-tested):

- **Dropped**: our own messages (source nick equals our current nick: `echo-message` echoes, or a
  bouncer's), CTCP other than `ACTION`, empty text after cleaning, and targets that are neither a
  channel nor our nick. `NOTICE` is never a message.
- **Conversation**: a target starting with a `CHANTYPES` character (after an optional `STATUSMSG`
  prefix, `@#chan`) is `Group` with the folded channel as id and the channel as title; a target
  equal to our nick is `Direct` with the folded sender nick as id (PLAN section 10) and the nick as
  title.
- **Identity**: with the `account-tag` capability on and an `account` tag (not `*`), the sender is
  `account:<folded account>`, authenticated, with `mask:<folded nick>!<user>@<lowercase host>` as an
  alias when `mask_identities` is on. Otherwise the sender is the mask, authenticated only with
  `mask_identities` and a complete source (user and host present). An `account` tag without the
  capability is ignored: only a server that negotiated it vouches for it.
- **Text**: `ACTION` becomes `/me ...`; bold, italics, underline, strikethrough, monospace, reverse,
  reset, and colors (`\x03` with up to two digits per color, `\x04` with up to six hex digits) are
  removed; tabs become spaces; other control characters go.
- **Ids**: the event id is `msgid:<msgid>` when the server sends a `msgid` tag (`message-tags`),
  stable across replays. Otherwise it's `sha256:` and the hex SHA-256 of the `time` tag (or, without
  one, the receive time and a counter), source, target, and text: a bouncer's replay with
  `server-time` dedupes, while two identical messages without tags stay two events. `message_id` is
  the `msgid`, `reply_to` the `+draft/reply` (or `+reply`) tag, `sent_at` the `time` tag.

Events go through the SDK, which persists each before the next is read and resends unacked ones
after a restart (PLAN section 8.5); `ack` has nothing to do.

The session also tracks which nick is logged in to which account, for recipients: from `account`
tags on messages, `extended-join` `JOIN`s, `ACCOUNT` (`account-notify`; `*` forgets), `NICK`
(followed), and `QUIT` (forgotten), at most 10 000 accounts. The map only orders `WHOIS` candidates;
it never decides a recipient.

## Outbound

`send` appends a line `[attachment not sent: <name or type>]` per attachment, then:

- **Conversation**: waits up to `send_wait` (30 s) for `registered`, else `Unavailable`; the target
  must not contain spaces, commas, or control characters or start with `:` (`InvalidRequest`).
- **Recipient**: `account:<name>` or `mask:<nick>!<user>@<host>` (the latter only with
  `mask_identities`; otherwise `InvalidRequest`). Not registered: `Unavailable` at once (the outbox
  retries).

Lines: `outbound::line_budget` is
`512 - 2 - len("PRIVMSG <target> :") - len(":<nick>!~<username>@<host> ")` with our current nick,
the configured user name plus a `~`, and a 63-byte host (`HOST_LEN`, the usual `HOSTLEN`; our cloak
isn't known), capped by `max_line_bytes`. `render::irc` renders and splits to it (formatting
reopened on each line); past `max_lines` the last kept line is replaced by
`[N more lines not sent]`. Each line is a `PRIVMSG`; the send completes (with a `sent:<time>:<n>`
message id) once its last line is written, so the result means "handed to the server". A connection
lost first fails it with `Unavailable`; lines already written stay sent.

Flood protection is `outbound::Throttle`, a virtual clock: a line may go when the clock is at most
`(send_burst - 1) * interval` ahead of now; each line advances it by `send_interval`. It covers
every `PRIVMSG`, `JOIN`, and `WHOIS`; registration, `PONG`, `PING`, and `QUIT` bypass it.

Recipient resolution (PLAN section 8.5: never send to a stale nick): lookups run one at a time.
Candidates for `account:<name>` are the tracked nick for the account, then the account name if it's
a valid nick; for a mask, its nick. For each, the plugin writes `WHOIS <nick>` only once no message
lines are queued and the throttle has a slot, so the confirmed nick's first line follows the reply
within one interval instead of after everything queued before it (time in which the nick could
change hands); `lookup_timeout` (10 s) starts when the `WHOIS` is written. It collects `311` (nick,
user, host) and `330` (account) for that nick until `318` or `401`. An account recipient matches
only if `330` names the account (folded); a mask matches only if nick, user, and host
(case-insensitive) all match. A match queues the message to the nick as the server spells it; no
match tries the next candidate; none left is `Unavailable("account <name> is not online")`; a
timeout is `Unavailable` too. Each `WHOIS` result also corrects the account tracking.

Conversation replies go to the nick in the conversation id without a `WHOIS`: they follow a message
from that nick moments earlier, as on any IRC client.

## Tests

- Unit tests: config validation (every key's errors, secrets never in messages or `Debug`, PEM
  certificates and a mismatched key), case mapping, inbound mapping (identities, mask opt-in,
  conversations, echo and CTCP drops, event id stability, tags, formatting), line budgets (a relayed
  line is exactly 512 bytes), `max_lines`, the throttle, recipient parsing, SASL chunking, and line
  framing (split reads, invalid UTF-8, overlong lines).
- `tests/channel.rs` runs `Irc` against a scripted fake server on 127.0.0.1 (`tests/support`):
  registration and capability requests, a server without `CAP`, SASL PLAIN (payload decoded), SASL
  failure and retry, configured SASL never skipped, account-tag and mask identities, query versus
  channel, echo and noise dropping, splitting within 512 bytes and the rate limit's timing,
  `max_lines`/`max_line_bytes`, attachments, recipient resolution (`WHOIS`, tracked nick first, nick
  changes, stale nick never messaged, mask matching, `WHOIS` timeout, `WHOIS` deferred until queued
  lines are out), bad recipients, sends while disconnected, a reply waiting for the connection,
  reconnecting after `ERROR` and a plain close, keepalive, a taken nick, `QUIT` on drop, TLS with a
  client certificate and SASL EXTERNAL, and a server certificate the bundled roots don't trust. TLS
  fixtures (`tests/fixtures/`: a test CA, server and client certificates valid for 100 years) and
  the commands that made them are described in `tests/support/mod.rs`.
- `tests/docs.rs` checks that the `irc` instance configs in the user guide (this channel's page and
  [Configuration](../user/configuration.md)) are valid.
- `tests/host.rs` launches the binary through the core's plugin host (unsandboxed) with a runtime
  and sessions: the owner's account query becomes a turn and its reply comes back as a `PRIVMSG`,
  group and unauthenticated messages are dropped by the core, the status reaches the core, and a bad
  `nick` fails `Configure` with `INVALID_ARGUMENT` naming the key.
