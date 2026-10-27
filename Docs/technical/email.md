# Email channel

`plugins/giles-channel-email` is the email channel plugin (PLAN sections 3.3, 8.5, and 10): a binary
on the plugin SDK ([Plugin SDK](plugin-sdk.md)) whose `Channel` reads one IMAP mailbox and sends by
SMTP. `ChannelInfo { identity_namespace: "email", output: Blocks, can_initiate: true }`. User-facing
behavior and config keys: [Email](../user/email.md).

## Libraries

| Crate                  | Use                                                  | Why                                       |
| ---------------------- | ---------------------------------------------------- | ----------------------------------------- |
| `async-imap` (tokio)   | IMAP: login, `SELECT`, `UID SEARCH`/`FETCH`, `IDLE`  | Maintained (Delta Chat), IDLE support, and the caller brings the stream, so TLS is ours |
| `async-smtp` (tokio)   | SMTP: EHLO, STARTTLS, AUTH, send                     | Same author and model: bring your own stream; `starttls()` hands the plain stream back for the upgrade |
| `tokio-rustls`         | TLS on both                                          | With `giles_plugin_sdk::tls::client_config()` (bundled Mozilla roots, aws-lc-rs), like every Giles client |
| `mail-parser`          | Parsing inbound mail (MIME, encodings, HTML to text) | Complete, fast, keeps raw header offsets (needed to read `Authentication-Results` verbatim); `full_encoding` for legacy charsets |
| `mail-builder`         | Building outbound mail                               | Same project as `mail-parser`; `multipart/alternative` + attachments, RFC 2047 headers, no dependencies with default features off |

`lettre` was the alternative for SMTP; it brings its own TLS setup and certificate store choices,
while `async-smtp` lets the plugin use the SDK's rustls config unchanged. No public suffix list
crate: see [Alignment](#alignment).

## Modules

| Module         | Does                                                                                |
| -------------- | ----------------------------------------------------------------------------------- |
| `config`       | Parses and validates the `config` table; errors are `invalid config: <key>: ...`    |
| `channel`      | `EmailChannel` (the `Channel`), the event stream's state machine, sending           |
| `imap`         | Connect, log in, select; UIDs after the cursor; fetch; wait with IDLE or polling    |
| `smtp`         | One connection per message; maps SMTP failures to `giles_types::Error`              |
| `inbound`      | Raw message to `Mail` or a drop reason: automated mail, sender, threading, text     |
| `auth_results` | `Authentication-Results` parsing (RFC 8601) and sender verification                 |
| `outbound`     | Building the outgoing message; subjects; message ids                                |
| `state`        | `cursor.json` and `threads/` in the data directory                                  |
| `address`      | Address normalization, message id and header text sanitizing, loopback hosts        |
| `media`        | `MediaStore`: the SDK's `Blobs`, or an in-memory fake in tests                      |
| `net`          | TCP with a 30 s connect timeout, TLS                                                |

## Inbound

The event stream is a state machine (`channel::Source`) polled by the SDK's pump:

1. Not connected: connect (implicit TLS, or plain then `STARTTLS`, whose leftover plain-text buffer
   is discarded with the old client so nothing can be injected into the TLS session), `LOGIN`,
   `CAPABILITY`, `SELECT`.
2. The cursor: `{uid_validity, uid}` in `cursor.json`. With none, or a different `UIDVALIDITY` (the
   mailbox was recreated), it starts at `UIDNEXT - 1` (or the highest UID): mail already in the
   mailbox is never replayed.
3. `UID SEARCH UID <cursor+1>:*`, keeping UIDs above the cursor (`n:*` always matches the newest
   message). Nothing new: wait, with `IDLE` when the server offers it and `imap.idle` is on, else
   `poll_interval` then `NOOP`. Any response but a keepalive ends IDLE; the search runs again.
   - `idle_timeout` (default 10 min, at most RFC 2177's 29) is a hard deadline around `async-imap`'s
     wait, whose own timer restarts on every response, keepalives (`* OK Still here`) included, so
     it would never fire on a server that sends them; then `DONE`.
   - An `EXISTS` or `RECENT` that came with an earlier response (mail delivered while the server ran
     the search) lands in `unsolicited_responses` and isn't repeated during IDLE. That queue is
     drained before `IDLE` and right after its continuation; news in it means search again instead
     of waiting.
4. Each new UID in order: `UID FETCH (UID RFC822.SIZE)`, then `BODY.PEEK[]` (or `BODY.PEEK[HEADER]`
   over `max_message_size`, plus a note in the text). `PEEK` leaves `\Seen` alone; the plugin never
   changes the mailbox. `async-imap` ignores a FETCH's tagged status, so a FETCH that returns
   nothing is followed by `UID SEARCH UID <uid>`: the message is skipped only if it's really gone
   (expunged); a `NO` for a message that exists is a failure.
5. `inbound::parse`, then either a drop (logged, cursor advanced) or an event.

Command timeouts are 2 min (IDLE excepted); a timeout is a broken connection.

### Failures and status

A connection failure (connecting, or any command on the connection) drops the connection and
reconnects after 1 s, doubling to 5 min while failures repeat. The delay resets only on progress: a
handled message, or a wait for new mail (IDLE or a poll) that ends normally. A search is never
progress, whatever it finds, so neither a message the server always refuses to fetch nor a server
that fails every IDLE (`BAD`, or a hang-up after `+ idling`) causes a login storm.

`ChannelStatus::Error` is reported for a failure unless it's the first since `Connected` was
reported or the last progress (usually a dropped connection, reopened quietly). `Connected` is
status only and doesn't touch the backoff: it's reported after a successful search when there are no
failures since the last progress, or when the search finds nothing new and the last failure didn't
happen on a caught-up connection (a failed login followed by a good one, say). The second rule keeps
a server that fails every IDLE from flipping between `Error` and `Connected` on each attempt. A
status is only reported when it differs from the last one.

A message the server never hands over (FETCH returns nothing, but `UID SEARCH` says it exists) is
retried forever rather than skipped, so no mail is lost; it holds up later mail until the operator
moves or deletes it. The user guide says how to find it from the error (its UID).

### Delivery and the cursor

Event id: `UIDVALIDITY:UID`, stable across restarts (PLAN section 8.5). The SDK writes every event
to its spool before polling the stream again, so the cursor moves to a message's UID when the stream
is next polled, and also when the core acks it (an acked message and everything before it are
stored). IMAP can replay, so the plugin keeps only this cursor, never a spool of its own. A crash
between the SDK's write and the cursor's re-fetches that message with the same event id, which the
SDK's spool or the core's inbox dedupe. Drops advance the cursor at once.

### Drops

In order, before anything else is looked at:

- unparseable (no headers);
- automated (RFC 3834 and lists): any `Auto-Submitted` whose keyword isn't `no`,
  `Precedence: bulk|list|junk`, or any `List-Id`;
- no sender: not exactly one `From:` header with exactly one valid address;
- our own copy: `From:` is our address and the `Message-ID` starts with `giles.` (every id we make;
  our mail also carries `Auto-Submitted`, so the rule above usually catches it first).

### Sender verification

`auth_results::verify` gets the first (topmost) `Authentication-Results` header's raw, unfolded
value. The tokenizer drops comments (nested, with `\` escapes), unquotes quoted strings, and splits
on `;` and `=`; keys may be written with spaces around `.` (`header . d`). An unclosed comment or
quote, or a malformed authserv-id part, fails the whole header; a malformed result is skipped.
Authenticated iff:

- the authserv-id equals `authserv_id` (ASCII case-insensitive), and
- some `dmarc=pass` whose `header.from` is the `From:` domain (required, so the server and the
  plugin can't disagree about which `From:` they read; common implementations write it), or
- some `dkim=pass` whose signing domain (`header.d`, else the domain of `header.i`) is aligned.

A header below the topmost is never read: the receiving server prepends its own, so everything below
may come from the sender. That holds only if the configured server is the one that receives the
mailbox's mail from the internet, stamps every message it delivers (local submission and trusted
networks included), and removes incoming headers with its authserv-id (RFC 8601 section 5). An
upstream's authserv-id (a forwarder's) is never safe: mail sent straight to the final server can
carry a forged header with it. The user guide says so where it explains the setting. A pass inside a
comment or a quoted `reason` is text, not a result. An unauthenticated message is still reported,
with `sender_authenticated: false`; the core drops it before the unknown-sender policy (PLAN section
3.3).

### Alignment

Relaxed DMARC alignment compares organizational domains, which needs the public suffix list. The
plugin uses a stricter rule that needs none: the `From:` domain must equal the signing domain or be
a subdomain of it, and the signing domain must have at least two labels. Whoever controls a zone
controls its subdomains, so this never trusts a signer that couldn't already publish keys for the
`From:` domain. It refuses a signer that is a subdomain or sibling of the `From:` domain
(`d=mail.example.com` for `me@example.com`); mail like that still passes when the receiving server
reports `dmarc=pass`, which it computes with the real list.

### The event

- `sender`: the `From:` address, domain lowercased (identity `email:<address>`);
  `sender_display_name` from `From:`.
- `conversation`: `Direct`, id `<sender, lowercased>/<thread root>`, title the subject. The thread
  root is the first `References` id, else the first `In-Reply-To` id, else the message's own id. The
  sender is part of the id so each sender in a thread is a separate direct conversation: replies go
  only to the person written to, and someone else writing into the thread (even a verified sender)
  can't redirect replies meant for the owner. Group conversations aren't reported in v1.
- `message_id`: `Message-ID` without brackets (synthesized as `<V>.<U>@giles.invalid` if missing or
  unusable); `reply_to`: the first `In-Reply-To` id.
- `text`: all text bodies (HTML converted to text by `mail-parser` when there's no plain part),
  prefixed with `Subject: ...` for a thread root.
- `attachments`: every attachment part, stored with `Blobs::put` (media type lowercased, file name
  kept).
- `sent_at`: the `Date:` header.

Ids written back into headers later pass `address::clean_id` (printable ASCII, no spaces or angle
brackets, at most 250 bytes); subjects and names are collapsed to one line (`address::one_line`).

## Threads

`threads/<sha256(conversation id)>.json` holds `{peer, peer_name, subject, references}`: where
replies go and how to thread them. Only an authenticated message writes it (an unauthenticated one
could otherwise pick the recipient), from its own `References` chain plus its id. `references` keeps
the root and the newest ids, 20 at most. A send records its own id too, so the next reply answers
it. Updates are read-modify-write under a mutex, written atomically (temporary file, `fsync`,
rename, directory `fsync`), like `cursor.json`.

## Outbound

`send` makes a new id `giles.<128 random bits, hex>@<our domain>` (aws-lc-rs's random generator,
through the SDK's rustls).

- `SendTarget::Conversation`: the stored thread (unknown: `Error::NotFound`). To its `peer`, subject
  `Re: <subject>` (unless it starts with `Re:`), `In-Reply-To` the core's `reply_to` if usable, else
  the thread's newest id, `References` the stored chain, `Auto-Submitted: auto-replied`.
- `SendTarget::Recipient`: the native id is the address (normalized; invalid: `InvalidRequest`).
  Subject: the first line of the plain text, 60 characters at most (plus `…`). No threading headers,
  `Auto-Submitted: auto-generated` (RFC 3834 section 5: not a reply). The thread is stored under
  `<recipient>/<new id>`, the conversation the recipient's answer will have.

The body is `multipart/alternative`: `render::plain_text` and `render::html` with no images allowed
at all (a remote one would leak data; there are no local ones in mail), in a minimal HTML document.
Attachments make it `multipart/mixed`; a media type that isn't a plain `type/subtype` token is sent
as `application/octet-stream`, and file names lose control characters.

SMTP: one connection per message, 2 min overall. EHLO with our domain, `STARTTLS` (the plain buffer
dropped before the handshake) or implicit TLS, then, with credentials, a second EHLO to pick
`AUTH PLAIN` or `AUTH LOGIN`. Errors: a `5xx` reply is `InvalidRequest` (the outbox drops the row);
anything else (`4xx`, network, TLS, timeouts) is `Unavailable` (retried). After the message is
accepted, failing to record it in the thread is logged, not returned, so the core doesn't send it
twice.

## Tests

Unit tests next to each module: config errors naming keys and redacting passwords; the
`Authentication-Results` table (topmost only, authserv-id mismatch, `dmarc=pass` for another domain
or without `header.from`, aligned and unaligned `dkim`, `header.i`, several results, passes hidden
in comments and quoted reasons, unclosed comments); drops; threading; MIME bodies and attachments;
HTML-only mail; the built message parsed back with `mail-parser`; header injection through subjects.

`src/testing.rs` has fake IMAP and SMTP servers on `127.0.0.1` (plain, implicit TLS, or STARTTLS,
with an `rcgen` certificate for `localhost` and a client config trusting only it) and an in-memory
`MediaStore`. The fake IMAP server can also misbehave the way real ones do: refuse to fetch one
message, expunge one as it's fetched, answer IDLE with `BAD`, send IDLE keepalives, announce mail
only in a search response, and send forged plain text after its STARTTLS answer. `channel/tests.rs`
drives `EmailChannel` against them: IDLE and polling, old mail ignored, the cursor across restarts
(acked, persisted, and unconfirmed events), a recreated mailbox, login failures reported then
retried, dropped connections, a message that can't be fetched and a server that answers IDLE with
`BAD` both backing off (spaced logins, an error status), an expunged message skipped, IDLE restarted
on time despite keepalives, mail announced with a search response not missed, injected plain text at
STARTTLS ignored, automated and own mail skipped, attachments to blobs, oversized mail cut to
headers, replies and proactive mail with their headers and MIME structure, forged messages not
redirecting replies, SMTP errors mapped to retryable or not, and both TLS modes for both protocols
(and a certificate the bundled roots refuse).
