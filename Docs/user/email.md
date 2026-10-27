# Email

The email channel, `giles-channel-email`, gives an agent a mailbox. It watches one IMAP folder for
new mail and answers by SMTP, in the same thread. Install the plugin (see
[Instances](configuration.md#instances)) and add an instance:

```toml
[owner]
identities = ["email:me@example.com"]

[channel.mail]
component = "email"

[channel.mail.config]
address = "giles@example.com"
name = "Giles"
authserv_id = "mx.example.com"

[channel.mail.config.imap]
host = "imap.example.com"
user = "giles@example.com"
password = { secret = "mail" }

[channel.mail.config.smtp]
host = "smtp.example.com"
user = "giles@example.com"
password = { secret = "mail" }
```

Give Giles a mailbox of its own: it reads every new message in the folder it watches.

Email senders are identities in the `email` namespace: `email:<address>`. The namespace is shared by
every email instance, so `email:me@example.com` in `[owner] identities` covers all of them. The
domain of an address ignores case; the part before the `@` doesn't.

## Settings

| Key                  | Default      | Meaning                                                          |
| -------------------- | ------------ | ---------------------------------------------------------------- |
| `address`            | (required)   | Giles' address: the `From:` of everything it sends               |
| `name`               | none         | The display name in `From:`                                      |
| `authserv_id`        | (required)   | Your mail server's name in `Authentication-Results` (below)      |
| `max_message_size`   | `"50 MiB"`   | Larger messages arrive as their headers and a note, without body |
| `imap.host`          | (required)   | The IMAP server                                                  |
| `imap.port`          | by `tls`     | 993 for `implicit`, 143 otherwise                                |
| `imap.tls`           | `"implicit"` | `"implicit"` (TLS from the start), `"starttls"`, or `"none"`     |
| `imap.user`          | (required)   | The IMAP user name                                               |
| `imap.password`      | (required)   | The IMAP password; use a secret reference                        |
| `imap.mailbox`       | `"INBOX"`    | The folder to watch                                              |
| `imap.idle`          | `true`       | Wait for new mail with IMAP IDLE when the server offers it       |
| `imap.idle_timeout`  | `"10m"`      | Restart IDLE this often (at most `"29m"`)                        |
| `imap.poll_interval` | `"1m"`       | How often to check for mail without IDLE                         |
| `smtp.host`          | (required)   | The SMTP server                                                  |
| `smtp.port`          | by `tls`     | 465 for `implicit`, 587 for `starttls`, 25 for `none`            |
| `smtp.tls`           | `"starttls"` | `"implicit"`, `"starttls"`, or `"none"`                          |
| `smtp.user`          | none         | The SMTP user name (with `smtp.password`)                        |
| `smtp.password`      | none         | The SMTP password (with `smtp.user`)                             |

`tls = "none"` is only allowed for a server on this machine (`localhost`, `127.0.0.0/8`, `::1`),
such as a local mail bridge. TLS certificates are checked against the Mozilla root certificates
built into Giles, not the system's. SMTP logs in with `AUTH PLAIN` or `AUTH LOGIN`. Like any channel
plugin it may use the network; to limit it to the mail ports, set
`sandbox = { network = ["tcp:993", "tcp:587"] }` on the instance
([Sandbox](configuration.md#sandbox)).

A mistake in the settings stops the instance from starting, and the log names the key, for example
`invalid config: imap.tls: "none" is only allowed for a server on this machine`.

## Who Can Write to Giles

Anyone can put any address in `From:`, so Giles believes a sender only when your mail server vouches
for them. Receiving servers record their checks in an `Authentication-Results` header at the top of
each message. Giles reads only the topmost one, and only if it names your server; a header further
down could have been written by the sender. The sender counts as verified if that header says
`dmarc=pass` for the sender's domain (`header.from=`), or `dkim=pass` for the sender's domain or a
parent of it (e.g. a signature by `example.com` for `me@mail.example.com`). Anything else, including
mail without the header, arrives unverified, and Giles drops it before an agent sees it (it never
even gets a pairing code).

### Choosing `authserv_id`

This check is only as good as the server you name. `authserv_id` must be the server that receives
this mailbox's mail from the internet, and that server must

- add an `Authentication-Results` header to **every** message it delivers to the mailbox, including
  mail from its own users and trusted networks, and
- remove any incoming `Authentication-Results` header that carries its name, so a sender can't forge
  one (RFC 8601 section 5 requires this; check that your server or its filter, such as rspamd or
  OpenDMARC, is set up to do it).

Never use the name of a server that only handed the mail on. If Gmail forwards to your own server,
for example, the topmost header may be Gmail's, but anyone can send mail straight to your server
with a forged `Authentication-Results: mx.google.com; dmarc=pass ...` header, and your server won't
remove it. If your own server doesn't check and stamp mail itself, Giles can't verify senders on
that mailbox; use a mailbox at a provider that does.

To see the name, open a message that came from outside in the mailbox and look at its headers (most
mail programs have "Show original" or "View source"); the first word of the topmost
`Authentication-Results` line is the name, if that header was added by the server that received the
message. For a Gmail mailbox it's `mx.google.com`:

```text
Authentication-Results: mx.google.com;
       dkim=pass header.i=@example.com header.s=sel1 header.b=abc123;
       dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=example.com
```

If the configured server doesn't stamp a message, it arrives unverified and is dropped; the log says
why for each message.

## Threads and Replies

Each thread is a conversation, separately for each sender: if two people write in one thread, each
has their own conversation with the agent, and a reply only ever goes to the person it answers.
Replies are sent to the sender's `From:` address (never `Reply-To:`), without the thread's other
recipients, with the subject `Re: <subject>` and the headers that keep them in the thread.

What the agent sees of a message:

- its plain text part, or its HTML part converted to text when there's no plain text;
- the subject, as a first line `Subject: ...`, when the message starts a thread;
- its attachments (subject to the instance's `max_blob`);
- nothing but the headers and a note if the message is larger than `max_message_size`.

Giles ignores mail sent by machines: messages with `Auto-Submitted` (other than `no`),
`Precedence: bulk`, `list`, or `junk`, or a `List-Id` (mailing lists). It also skips its own sent
messages if they land in the watched folder.

Mail Giles sends has a plain text part and an HTML part, both from the agent's Markdown. The HTML
part never shows images, so opening a reply can't make your mail program fetch anything. Replies
carry `Auto-Submitted: auto-replied`, and messages Giles starts on its own (from a scheduled job,
say) `Auto-Submitted: auto-generated`, so vacation responders and other robots don't answer them. A
message Giles starts gets the first line of its text as the subject; your reply to it continues that
conversation.

## The Mailbox

On its first start the channel ignores the mail already in the folder and handles only what arrives
afterwards. It remembers where it got to (in its data directory), so after a restart it picks up
mail that arrived in the meantime, without handling anything twice. If the folder is deleted and
recreated, it again starts with the mail that arrives afterwards.

Giles doesn't mark messages as read, move, or delete them, and doesn't keep copies of what it sends
in a Sent folder.

When the IMAP server can't be reached, refuses the login, or keeps failing a command, `giles status`
shows the error, and Giles keeps trying, waiting longer after each failure (up to 5 minutes). A
connection that just drops is reopened after a second, without an error. If the error mentions
`IDLE`, the server advertises IDLE but doesn't support it properly: set `imap.idle = false` to check
for mail every `poll_interval` instead.

Giles never skips a message because the server failed to hand it over, so one damaged message holds
up the mail behind it. The status and the log then include

```text
IMAP UID FETCH of message 4711 returned nothing, but the message exists
```

4711 is the message's IMAP UID. It's the oldest message in the folder that Giles hasn't handled yet:
everything before it reached the agent or was skipped as automated mail. Move it to another folder
(or delete it) in your mail program, and Giles carries on with the next message at its next attempt.
Most mail programs don't show UIDs; to look at the message by its UID first, `curl` can fetch it:
`curl --user giles@example.com 'imaps://imap.example.com/INBOX;UID=4711'`.

A send that fails because the SMTP server can't be reached or answers "try later" (a `4xx` reply)
counts as temporary, so a scheduled message is tried again later; a permanent refusal (a `5xx`
reply, such as an unknown recipient) isn't retried.
