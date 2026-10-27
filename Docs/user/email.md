# Email

The email channel, `giles-channel-email`, gives an agent a mailbox. It watches one IMAP folder for
new mail and answers by SMTP, in the same thread.

## Adding an instance

Give Giles a mailbox of its own. It reads every new message in the folder it watches.

[Install the plugin](configuration.md#instances) and add an instance:

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

Email senders are identities of the form `email:<address>`. Every email instance shares this
namespace, so `email:me@example.com` in `[owner] identities` covers all of them. The domain of an
address ignores case, but the part before the `@` doesn't.

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
such as a local mail bridge.

Giles checks TLS certificates against the Mozilla root certificates built into Giles. It doesn't use
the system's certificates. To log in, the SMTP server must offer `AUTH PLAIN` or `AUTH LOGIN`.

To limit the plugin's [network access](configuration.md#sandbox) to the mail ports, set
`sandbox = { network = ["tcp:993", "tcp:587"] }` on the instance.

If a setting is wrong, the instance doesn't start, and the log names the key.

## Who can write to Giles

Anyone can put any address in `From:`. Giles believes a sender only when your mail server vouches
for them.

Receiving servers record their checks in an `Authentication-Results` header at the top of each
message. Giles reads only the topmost one, and only if it names the server in `authserv_id`.

The sender is verified if that header says `dmarc=pass` for the sender's domain, or `dkim=pass` for
the sender's domain or a parent of it.

Giles drops all other mail, including mail without the header, before an agent sees it. An
unverified sender never even gets a pairing code. The log says why each message was dropped.

### Choosing `authserv_id`

This check is only as good as the server you name. `authserv_id` must be the server that receives
this mailbox's mail from the internet. That server must:

- add an `Authentication-Results` header to **every** message it delivers to the mailbox, including
  mail from its own users and trusted networks
- remove any incoming `Authentication-Results` header that carries its name, so a sender can't forge
  one

RFC 8601 section 5 requires the second point. Check that your server or its filter, such as rspamd
or OpenDMARC, is set up to do it.

**Never use the name of a server that only handed the mail on.** If Gmail forwards to your own
server, for example, the topmost header may be Gmail's. But anyone can send mail straight to your
server with a forged `Authentication-Results: mx.google.com; dmarc=pass ...` header, and your server
won't remove it.

If your own server doesn't check and stamp mail itself, Giles can't verify senders on that mailbox.
Use a mailbox at a provider that does.

To find the name, open a message in the mailbox that came from outside and look at its headers. Most
mail programs have "Show original" or "View source". The name is the first word of the topmost
`Authentication-Results` header, if the server that received the message added it. For a Gmail
mailbox it's `mx.google.com`:

```text
Authentication-Results: mx.google.com;
       dkim=pass header.i=@example.com header.s=sel1 header.b=abc123;
       dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=example.com
```

## Threads and replies

Each thread is a conversation, separately for each sender. If two people write in one thread, each
has their own conversation with the agent, and a reply only ever goes to the person it answers.

Replies go to the sender's `From:` address, never `Reply-To:`. They don't go to the thread's other
recipients. Their subject is `Re: <subject>`.

A message Giles starts on its own, from a scheduled job for example, gets the first line of its text
as the subject. Your reply to it continues that conversation.

The agent sees:

- the message's plain text part, or its HTML part converted to text when there's no plain text
- the subject, as a first line `Subject: ...`, when the message starts a thread
- its attachments (subject to the instance's `max_blob`)
- only the headers and a note, if the message is larger than `max_message_size`

Giles ignores mail sent by machines. These are messages with:

- `Auto-Submitted` (other than `no`)
- `Precedence: bulk`, `list`, or `junk`
- a `List-Id` (mailing lists)

It also skips its own sent messages if they land in the watched folder.

## The mailbox

On its first start the channel ignores the mail already in the folder. It handles only what arrives
afterwards. After a restart, it picks up mail that arrived in the meantime, without handling
anything twice.

Giles doesn't mark messages as read, move them, or delete them. It doesn't keep copies of what it
sends in a Sent folder.

## Troubleshooting

When the IMAP server can't be reached, refuses the login, or keeps failing a command, `giles status`
shows the error. Giles keeps trying to connect.

If the error mentions `IDLE`, the server advertises IDLE but doesn't support it properly. Set
`imap.idle = false` to check for mail every `imap.poll_interval` instead.

### A message that can't be fetched

Giles never skips a message because the server failed to hand it over. One damaged message holds up
the mail behind it. `giles status` and the log then show:

```text
IMAP UID FETCH of message 4711 returned nothing, but the message exists
```

4711 is the message's IMAP UID. It's the oldest message in the folder that Giles hasn't handled yet.
Move it to another folder, or delete it, in your mail program. Giles carries on with the next
message at its next attempt.

Most mail programs don't show UIDs. To look at the message by its UID first, fetch it with `curl`:

```sh
curl --user giles@example.com 'imaps://imap.example.com/INBOX;UID=4711'
```
