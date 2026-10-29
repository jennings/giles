# WhatsApp

The WhatsApp channel lets you talk to Giles in WhatsApp chats. It links to a WhatsApp account as a
*linked device*, the way WhatsApp Web does.

> **Warning: this is an unofficial client.** Giles uses the WhatsApp Web protocol through the
> open-source library [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust). Meta's terms
> don't allow unofficial clients, and WhatsApp may suspend or ban an account that uses one. Use a
> number you can afford to lose, not [your personal one](#giles-own-number).
>
> **Nobody has linked a real account with this plugin yet.** Expect rough edges
> ([issue #1](https://github.com/jennings/giles/issues/1) tracks it). Read the
> [live test plan](../technical/whatsapp-spike.md#owner-test-plan) before relying on it.

The plugin is [licensed under the AGPL](../technical/whatsapp.md#licensing), unlike the rest of
Giles.

## Giles' own number

Link Giles to a WhatsApp number of its own, and message that number from yours.

Giles ignores every message sent from the linked account itself. If you link your personal account,
nothing you type there reaches the agent, including the "Message yourself" chat.

## Setting up

Add the channel to `giles.toml`, with your own WhatsApp number as an owner identity:

```toml
[owner]
identities = ["whatsapp:+15551234567"]

[channel.whatsapp]
component = "whatsapp"
```

Start Giles and [link the account](#linking). Then send a message from your WhatsApp to Giles'
number, and [pick an agent for the chat](#conversations).

## Settings

The settings go in the instance's `config` table. All of them are optional.

| Key            | Default      | Meaning                                                                  |
| -------------- | ------------ | ------------------------------------------------------------------------ |
| `link`         | `"qr"`       | How to link a new device: `"qr"` or `"pair_code"` ([Linking](#linking))  |
| `phone_number` | none         | The account's number with country code; required for `"pair_code"`      |
| `device_name`  | `"Giles"`    | The name WhatsApp shows for this device under Linked devices            |
| `max_blob`     | `"50 MiB"`   | The largest attachment Giles downloads or sends ([Media](#media))        |

`link` and `phone_number` only matter while no device is linked. `phone_number` is required with
`link = "pair_code"` and refused with `link = "qr"`.

```toml
[channel.whatsapp]
component = "whatsapp"

[channel.whatsapp.config]
link = "pair_code"
phone_number = "+1 555 123 4567"
device_name = "Giles at home"
max_blob = "20 MiB"
```

An unknown key or an invalid value stops the instance from starting. The log and `giles status` name
the key, e.g. `invalid config: phone_number: starts with 0`.

## Linking

While no device is linked, the web chat's [admin page](web.md#linking-a-channel) shows a code to
link with. `giles status` shows the same code as text.

### With a QR code

This is the default.

1. On the phone, open WhatsApp, then **Settings**, **Linked devices**, **Link a device**.
2. Scan the code on the admin page.

WhatsApp replaces the code every 20 seconds or so. After about two and a half minutes without a
scan, WhatsApp stops offering codes, and the status says `no QR code was scanned in time`.

### With a pair code

Use this when you can't scan a screen, for example when the phone is the only device at hand.

1. Set `link = "pair_code"` and `phone_number` in the [settings](#settings).
2. On the phone, open **Linked devices**, **Link a device**, **Link with phone number instead**.
3. Enter the eight-character code from the admin page.

The code is good for about three minutes. WhatsApp may also send a notification on the phone.

### When it's linked

The status turns to `connected`, and the phone lists the new device under **Linked devices** with
the name from `device_name`.

The plugin keeps its session in `data/channel/<instance>/whatsapp.db`. It isn't included in
[backups](backups.md), so link the device again after a restore.
**Anyone who can read that file can act as the linked device.**

An earlier release kept the session in `data/plugins/<instance>/`. Giles doesn't move it, so link
the device again after you upgrade. Then remove the old device under **Linked devices** on your
phone and delete `data/plugins/`.

### If linking fails

The status says why. Giles doesn't ask WhatsApp for new codes on its own, because every request
counts against the account. To get a fresh code, **Restart** the plugin on the
[admin page](web.md#admin-pages), or restart Giles.

### Logging out

If you remove the device under **Linked devices** on the phone, or WhatsApp logs it out, the status
says `WhatsApp logged this device out`. **Restart** the plugin to link a new device.

## Identities

A WhatsApp identity is the sender's phone number with country code, written as `+` and digits only,
such as `whatsapp:+15551234567`. One entry in `[owner] identities` covers all WhatsApp instances.

WhatsApp doesn't always tell Giles the sender's number. It may use an internal id instead, the
*LID*. List both, and either one matches:

```toml
[owner]
identities = ["whatsapp:+15551234567", "whatsapp:100000012345678@lid"]
```

A message from an identity that isn't listed is dropped. The log line `dropped inbound message`
shows the sender in exactly the form to list.

WhatsApp messages are end-to-end encrypted, so a sender can't be forged and every message counts as
verified.

## Conversations

A chat with one person is a **direct** conversation. Its id is `15551234567@s.whatsapp.net` (the
number without `+`), or `100000012345678@lid` when the number is unknown.

Pick the agent on the [Routes](web.md#routes) page with channel `whatsapp` and kind `direct`. Add a
conversation id, such as `15551234567@s.whatsapp.net`, to route a single chat. Without one, the
route answers every direct chat.

Giles ignores:

- Group chats. Giles v1 doesn't answer them.
- Status updates, broadcast lists, and channels.
- Reactions, edits, deletions, polls, contacts, locations, calls, and other messages with no text or
  supported attachment.

Messages Giles sends on its own, such as job results, approval prompts, and login codes, go to an
identity like `whatsapp:+15551234567`. If the number has no WhatsApp account, or WhatsApp can't be
reached, the message waits and is retried until it expires.

## Replies

Replies use WhatsApp's formatting: bold, italic, strikethrough, code, lists, and quotes. Links are
written as `text <url>`. [Rendering Model Output](../technical/rendering.md#whatsapp) has the rules.

WhatsApp can't escape its markup characters. Giles replaces `*`, `_`, `~`, and `` ` `` in ordinary
text with look-alike characters.

A reply longer than 4,096 characters is sent as several messages.

## Media

These arrive as attachments, with the caption as the message's text:

- Images
- Videos
- Voice notes and other audio
- Documents
- Stickers

An attachment larger than `max_blob` isn't downloaded, and neither is one WhatsApp no longer has.
The message still arrives, with a line such as
`[image not received: it is 80000000 bytes, over the limit of 52428800 bytes]` after its text.

Keep the plugin's `max_blob` no higher than the instance's own `channel.<name>.max_blob`
([Channels](configuration.md#channels)). Both default to 50 MiB, and Giles applies the instance's
limit to what the plugin hands over.

Replies can carry attachments. Giles sends them as:

- **Photos**: JPEG, PNG, and WebP images.
- **Videos**: MP4 and 3GP.
- **Audio**: AAC, MP3, MP4, AMR, and Ogg.
- **Documents**: anything else, with its file name.

The reply's text becomes the caption of the first attachment that can have one (not audio) if it's
under 1,024 characters. Otherwise it's sent as separate messages after the attachments. If any
attachment is over `max_blob`, nothing in the reply is sent.

## Connection

The plugin reconnects after a lost connection by itself. The status reports a drop only if it lasts
more than 10 seconds, since WhatsApp recycles connections now and then.

Some errors are final, and the status says so:

- A logout
- A ban (`WhatsApp temporarily banned this account`)
- A refusal such as a locked account
- A client version WhatsApp refuses as outdated
- Another client taking over the device

Giles doesn't retry these. Restart the plugin once you've dealt with the cause.

A crash or a stop loses no messages, because WhatsApp delivers again what Giles hadn't stored. While
the plugin is stopped, WhatsApp holds undelivered messages only for a limited time.

To debug the connection, run Giles with `GILES_LOG=info,whatsapp_rust=info,wacore=info`. These logs
can include accounts, chats, and pairing codes, so redact what you share.

## Sandboxing

The plugin needs only outbound HTTPS to `web.whatsapp.com` and WhatsApp's media servers. To limit it
to that, grant `sandbox = { network = ["tcp:443"] }` ([Sandbox](configuration.md#sandbox)).
