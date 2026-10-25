# WhatsApp

The WhatsApp channel is the plugin `giles-channel-whatsapp`. It links to a WhatsApp account as a
*linked device*, the way WhatsApp Web and WhatsApp Desktop do, and talks to the agent in that
account's chats:

```toml
[owner]
identities = ["whatsapp:+15551234567"]

[channel.whatsapp]
component = "whatsapp"
```

Start Giles, then link the account from the web chat's admin pages ([Linking](#linking)). Send a
message from your own WhatsApp to the account's number, and give the conversation an agent
([Conversations](#conversations)).

> **Risk: this is an unofficial client.** Giles speaks the WhatsApp Web protocol through the
> open-source library [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust), not through
> Meta's WhatsApp Business API. Meta's terms don't allow unofficial clients, and WhatsApp may
> suspend or ban an account that uses one. Use a WhatsApp number that you can afford to lose, not
> your personal one. It should be a separate number anyway ([Giles' own number](#giles-own-number)).
>
> **Not yet run against a real account.** The plugin's logic is tested against a fake WhatsApp
> client, and the library calls it makes are the ones a working spike used, but nobody has linked
> a real account with this plugin yet. Expect rough edges, and look at the
> [live test plan](../technical/whatsapp-spike.md#owner-test-plan) before relying on it.

## Licensing

The repository is `MIT OR Apache-2.0`, except `plugins/giles-channel-whatsapp`, which is
`AGPL-3.0-only`. The plugin is built on `whatsapp-rust`, whose Signal protocol code
(`wacore-libsignal`, from Signal's `libsignal`) is licensed `AGPL-3.0-only`, so the plugin binary
is a combined work under that license. The Giles core and the other plugins aren't affected: the
plugin is a separate process that talks to Giles over gRPC, and Giles doesn't link it.

What that means in practice: you may run the plugin and use it with Giles as you like. If you
distribute the binary (as the container image does), you must offer its source under the AGPL; the
image does so in `/usr/share/doc/giles/THIRD-PARTY`. If you modify the plugin and other people use
your modified version over a network, the AGPL requires you to offer them your source. This is a
description of how the code is licensed, not legal advice.

## Settings

The `config` table of the instance. All keys are optional. An unknown key is an error, and an
invalid value stops the instance from starting with a message naming the key, e.g. `invalid config:
phone_number: starts with 0` in the log and in `giles status`.

| Key            | Default      | Meaning                                                                  |
| -------------- | ------------ | ------------------------------------------------------------------------ |
| `link`         | `"qr"`       | How to link a new device: `"qr"` or `"pair_code"` ([Linking](#linking))  |
| `phone_number` | none         | The account's number with country code; required for `"pair_code"`      |
| `device_name`  | `"Giles"`    | The name WhatsApp shows for this device under Linked devices            |
| `max_blob`     | `"50 MiB"`   | The largest attachment Giles downloads or sends ([Media](#media))        |

`link` and `phone_number` only matter while there is no linked session; once linked, the plugin
reuses its session and ignores them. `link = "pair_code"` needs `phone_number`, and `phone_number`
is refused with `link = "qr"`.

```toml
[channel.whatsapp]
component = "whatsapp"

[channel.whatsapp.config]
link = "pair_code"
phone_number = "+1 555 123 4567"
device_name = "Giles at home"
max_blob = "20 MiB"
```

The plugin keeps its session (the keys that make it a trusted device) in the instance's data
directory, `data/plugins/<instance>/`, in the SQLite file `whatsapp.db`. It's part of
[backups](backups.md), and it's a credential: anyone who can read it can act as the linked device.
Only one process may use a session, so a second Giles (or a plugin started by hand) on the same
data directory refuses to start with `another process is using the WhatsApp session`.

## Linking

While no device is linked, the instance reports that it needs linking, with a code that's good for a
short time. The code is on the web chat's admin page
([Linking a Channel](web.md#linking-a-channel)), which draws a QR code as an image and reloads
itself, so it always shows the newest one. `giles status` shows the same code as text (a QR code's
payload, which isn't something to type).

**With a QR code** (the default): on the phone, open WhatsApp, then **Settings**, **Linked
devices**, **Link a device**, and scan the code on the admin page. WhatsApp replaces the code every
20 seconds or so. After about two and a half minutes without a scan it stops offering codes, and
the status says `no QR code was scanned in time`.

**With a pair code**, for when you can't scan a screen (the phone is the only device at hand): set
`link = "pair_code"` and `phone_number` to the account's number. The admin page shows an
eight-character code. On the phone: **Linked devices**, **Link a device**, **Link with phone number
instead**, and enter it. The code is good for about three minutes, and WhatsApp may also send a
notification on the phone.

Either way, the status turns to `connected` when it works, and the phone lists the new device under
**Linked devices** with the name from `device_name` (WhatsApp may show its own label for the
device's kind next to it).

When linking doesn't finish (the codes ran out, the phone refused the code, the number wasn't
accepted), the status says why, and Giles doesn't keep asking WhatsApp for new codes on its own:
every request counts against the account. **Restart** the plugin on the admin page
([Admin Pages](web.md#admin-pages)), or restart Giles, to get a fresh code.

### Logging Out

If you remove the device under **Linked devices** on the phone (or WhatsApp logs it out), the
status says `WhatsApp logged this device out`. The saved session is no good any more. **Restart**
the plugin: it moves the old session aside (to `whatsapp.db.logged-out` in the data directory,
replacing an earlier one, so you can look into an unexpected logout) and offers a new code, like a
first start.

## Giles' Own Number

Link Giles to a WhatsApp number of its own, and message that number from yours. WhatsApp shows
Giles' own messages to every device of the account, so the plugin must treat everything that comes
from the linked account itself as an echo and drop it (otherwise the agent would answer its own
replies). That includes what you type yourself if you linked Giles to your personal account:
those messages, and the "Message yourself" chat, never reach the agent.

## Identities

An identity in the `whatsapp` namespace is the sender's phone number with country code, `+` and
digits only: `whatsapp:+15551234567`. All WhatsApp instances share the namespace, so one entry in
`[owner] identities` covers all of them.

WhatsApp doesn't always tell a recipient the sender's number. It addresses accounts by phone number
or by an internal id, the *LID* (`100000012345678@lid`), and a message may carry only one of them.
The plugin looks for the number in the message, then in the map the WhatsApp client learns from its
traffic. When it finds both, the number is the identity and the LID an alias, so either works in
`identities`:

```toml
[owner]
identities = ["whatsapp:+15551234567", "whatsapp:100000012345678@lid"]
```

When the number is unknown, the LID is the identity. Listing the LID too is the safe way to cover
that. A message from an identity that isn't listed is dropped and logged
(`dropped inbound message`) with the sender in exactly the form to list.

Every message the plugin delivers counts as verified: WhatsApp messages are end-to-end encrypted,
and the sender's device is the one whose keys the message was encrypted with, so the sender can't
be forged. (The first message from a contact trusts the key WhatsApp's server hands out, as the
WhatsApp app itself does.)

## Conversations

- A chat with one person is a **direct** conversation, with the chat's address as its id:
  `15551234567@s.whatsapp.net` (the number, without `+`), or `100000012345678@lid` when the number
  is unknown. A person is one conversation however WhatsApp addressed their messages.
- A **group** chat is a group conversation with the group's id, `120363012345678901@g.us`. Giles v1
  doesn't answer group conversations: such messages are dropped, and the group chat only shows
  that the plugin reports them correctly.
- Status updates, broadcast lists, and channels are ignored.
- Reactions, edits, deletions, polls, contacts, locations, calls, and other messages with no text
  or supported attachment are ignored. A reply to an earlier message arrives with the id of the
  message it replies to.

Pick the agent for a chat on the **Routes** page ([Routes](web.md#routes)): channel `whatsapp`,
kind `direct`, and optionally the conversation id of one chat, `15551234567@s.whatsapp.net`. A
route for channel `whatsapp` and kind `direct` alone answers every direct chat.

Messages Giles sends on its own (job results, approval prompts, login codes) go to an identity,
`whatsapp:+15551234567`. Giles first asks WhatsApp whether the number has an account. If it
doesn't, or WhatsApp can't be reached, the message waits and is retried until it expires.

## Output

Replies are written in WhatsApp's markup: `*bold*`, `_italic_`, `~strikethrough~`, `` `code` ``,
code blocks fenced with three backticks, lists with `-` and `1.`, and quotes with `>`. WhatsApp
has no way to escape its markup characters, so `*`, `_`, `~`, and `` ` `` in ordinary text are
replaced with look-alike characters; links are written as `text <url>`. See
[Rendering Model Output](../technical/rendering.md#whatsapp) for the rules.

A reply longer than 4,096 characters is sent as several messages, split at a paragraph break if it
can, else at a line or a space. A code block cut in the middle is closed in one message and
reopened in the next. WhatsApp itself would take far longer messages, but a reply that long is
hard to read in one bubble.

Giles doesn't quote the message it answers, so replies show as ordinary messages.

## Media

Images, videos, voice notes and other audio, documents, and stickers arrive as attachments of the
message, with the caption as its text. Giles downloads each (WhatsApp encrypts them separately from
the message) and hands it to the agent.

An attachment larger than `max_blob` isn't downloaded, and neither is one WhatsApp no longer has.
The message still arrives, with a line such as `[image not received: it is 80000000 bytes, over
the limit of 52428800 bytes]` after its text. Set `max_blob` in the plugin's `config` no higher
than the instance's own `max_blob` ([Channels](configuration.md#channels), also 50 MiB by
default), which is the limit Giles applies to what the plugin hands over; the plugin can't see it.

Replies can carry attachments. A JPEG, PNG, or WebP image is sent as a photo, an MP4 or 3GP video
as a video, AAC, MP3, MP4, AMR, and Ogg audio as audio, and anything else as a document with its
file name. The reply's text becomes the caption of the first attachment that can have one (not
audio) if it's under 1,024 characters, and separate messages after the attachments otherwise.
An attachment over `max_blob` refuses the whole reply, and nothing is sent.

## Connection

The plugin starts connecting when Giles starts, and a WhatsApp that can't be reached doesn't stop
the instance. The library reconnects after a lost connection by itself. A drop is reported in the
status (`disconnected from WhatsApp (...); the client is reconnecting`) only if it lasts more than
10 seconds, since WhatsApp recycles connections now and then. A reply written while the connection
is down waits up to 30 seconds for it to come back.

If the WhatsApp client stops by itself, the plugin starts it again after 5 seconds, doubling the
wait up to 5 minutes (back to 5 seconds after a client has run for 2 minutes). When WhatsApp's
servers refuse a connection because they're down or failing (the client keeps retrying by itself),
that counts as a dropped connection, reported after 10 seconds like any other. Some endings are
final, and the status says so: a logout, a ban (`WhatsApp temporarily banned this account`), a
refusal the client doesn't retry (such as a locked account), a client version WhatsApp refuses as
outdated, or another client taking over the device.

Giles acknowledges a message to WhatsApp only after it has stored it, so a crash or a stop loses
no messages: WhatsApp delivers again what Giles hadn't acknowledged. While the plugin is stopped,
WhatsApp holds undelivered messages for a limited time, which Giles doesn't control.

The WhatsApp library is quiet in the log (warnings only) because its connection logs can include
accounts, chats, and pairing codes. To see more while debugging, run Giles with
`GILES_LOG=info,whatsapp_rust=info,wacore=info` and redact what you share.

Channel plugins may use the network by default. The plugin needs outbound HTTPS (`tcp:443`) only,
to `web.whatsapp.com` and WhatsApp's media servers; to allow just that, grant
`sandbox = { network = ["tcp:443"] }` ([Sandbox](configuration.md#sandbox)).
