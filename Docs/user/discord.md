# Discord

The Discord channel is the plugin `giles-channel-discord`. It runs a *bot account* that you create
in the Discord Developer Portal, and talks to the agent in direct messages (DMs) between you and
that bot:

```toml
[owner]
identities = ["discord:82198898841029460"]

[channel.discord]
component = "discord"

[channel.discord.config]
token = { secret = "discord" }
```

Start Giles, then send the bot a direct message and give the conversation an agent
([Conversations](#conversations)). The rest of this page walks through the setup: the bot
([Creating the Bot](#creating-the-bot)), the server that lets you message it
([Inviting the Bot](#inviting-the-bot)), your user id ([Your User ID](#your-user-id)), and the
settings ([Settings](#settings)).

> **Bot accounts only.** Giles logs in as a bot, through Discord's official bot API. Automating a
> normal user account (a "self-bot") is against Discord's terms and isn't supported: the plugin
> takes a bot token from the Developer Portal and nothing else. Use the plugin under Discord's
> [Developer Terms of Service](https://support-dev.discord.com/hc/articles/8562894815383-Discord-Developer-Terms-of-Service).
>
> **Not yet run against a real bot.** The plugin's logic is tested against a fake Discord client and
> against test servers that play Discord's documented protocol, and Discord's behaviour on this page
> comes from its documentation, but nobody has run the plugin against Discord itself yet. Where the
> page depends on that, it says so; the checklist is
> [issue #20](https://github.com/jennings/giles/issues/20). Expect rough edges.

## Creating the Bot

1. Open the [Discord Developer Portal](https://discord.com/developers/applications), log in, and
   choose **New Application**. The application's name is only a label; the bot's name is what you
   see in Discord and can be changed on the **Bot** page.
2. Open the application's **Bot** page. A bot is created with the application. If the portal offers
   a **Public Bot** switch, turn it off, so nobody else can add your bot to their servers.
3. On the **Bot** page, choose **Reset Token** and copy the token it shows. Discord shows it once;
   if you lose it, reset it again. The token is the bot's password ([The Token](#the-token)).

You don't need to turn on any **Privileged Gateway Intents** (Presence, Server Members, Message
Content). The plugin asks Discord for one standard intent, `DIRECT_MESSAGES`, and says nothing about
privileged ones. A bot receives the content of direct messages sent to it without the Message
Content intent: Discord's Gateway documentation lists "Content in DMs with the app" among the
exceptions, and its guide to privileged intents lists "Direct Messages sent to your app". That is
what the design rests on, and it comes from Discord's documentation,
**not yet verified with a real bot** ([issue #20](https://github.com/jennings/giles/issues/20)). If
it turns out wrong, the status says so ([Status](#status)). The Developer Portal's labels change
from time to time, so the names above may differ a little from what you see.

## Inviting the Bot

A bot can only message a person it has a way to reach, and Discord opens that way through a server
the two of you share. This is how Discord's apps behave, but the API documentation doesn't state it,
so treat it as what is expected and not as a guarantee (issue #20 checks it):

- You can send a direct message to the bot only if you share a server with it. Without one,
  Discord's app has no way to start the conversation.
- The bot can send a direct message to you only if you share a server with it **and** your privacy
  settings for that server allow direct messages from server members. If they don't, Discord refuses
  the bot's messages ([Status](#status), the 50007 error).

So make a server for it, or use one you own, and invite the bot:

1. In the Developer Portal, open the application's **OAuth2** page and the **URL Generator** (the
   portal's older and newer layouts name these differently; what you need is an invite link for the
   application with the `bot` scope).
2. Tick the scope `bot`. Leave every permission unticked: the bot needs none to receive and send
   direct messages.
3. Open the generated link, pick the server, and choose **Authorize**.

A small private server that only you and the bot belong to is enough, and keeps the bot out of
conversations that are none of its business. The bot shows as online while Giles runs.

The bot only needs the server as a meeting place. Messages written *in* a server are never delivered
to Giles ([Conversations](#conversations)).

## Your User ID

Giles knows who sent a message by the sender's Discord *user id*, a number such as
`82198898841029460`, not by the name. In the Discord app, open **User Settings**, then **Advanced**,
and turn on **Developer Mode**. Then right-click (or long-press) your own name or avatar anywhere in
the app and choose **Copy User ID**. Put it into `[owner] identities`:

```toml
[owner]
identities = ["discord:82198898841029460"]
```

All Discord instances share the `discord` namespace, and a user's id is the same for every bot, so
one entry covers every Discord instance you run. A message from an id that isn't listed is dropped
and logged (`dropped inbound message`) with the sender in exactly the form to list.

Every message the plugin delivers counts as verified: the sender's id is set by Discord, on a
connection the bot authenticated with its token, so a message can't claim another person's id. That
holds as long as the token stays secret ([The Token](#the-token)).

## Settings

The `config` table of the instance. Only `token` is required. An unknown key is an error, and an
invalid value stops the instance from starting with a message naming the key, e.g. "invalid config:
token: required: the bot token from the Bot page of the application in the Discord Developer Portal"
in the log and in `giles status`.

| Key                | Default    | Meaning                                                                |
| ------------------ | ---------- | ---------------------------------------------------------------------- |
| `token`            | required   | The bot token ([The Token](#the-token)); use a secret reference         |
| `max_blob`         | `"50 MiB"` | The largest attachment Giles downloads or sends ([Attachments](#attachments)) |
| `catch_up_max_age` | `"24h"`    | How far back Giles looks for messages it missed ([After Downtime](#after-downtime)) |

`token` can't be empty, and must be the token alone: printable characters with no spaces or line
breaks. Discord's examples write `Authorization: Bot <token>`, so a pasted `Bot` prefix is refused
with a message to paste the token alone. `max_blob` must be more than 0. `catch_up_max_age` is a
duration ([Value Formats](configuration.md#value-formats)) of at most `"30d"`, the time Giles
remembers an inbound message; `"0s"` turns catch-up off.

```toml
[channel.discord]
component = "discord"

[channel.discord.config]
token = { secret = "discord" }
max_blob = "20 MiB"
catch_up_max_age = "12h"
```

Set `max_blob` in the plugin's `config` no higher than the instance's own `max_blob`
([Channels](configuration.md#channels), also 50 MiB by default), which is the limit Giles applies to
what the plugin hands over. The instance name (`discord` above) is up to you, but it's permanent:
the plugin's data directory, `data/plugins/<instance>/`, is keyed by it.

## The Token

The bot token is a credential: anyone who has it can act as your bot. Keep it in `secrets.toml` in
the data root, with mode 0600, and refer to it with `{ secret = "..." }` as above:

```toml
discord = "paste the token here"
```

```sh
chmod 600 ~/.giles/secrets.toml
```

Never write the token into `giles.toml` itself ([Secrets](configuration.md#secrets)). Giles never
logs it, and neither does the plugin: it's `<redacted>` in `giles status`, the admin pages, and
every error message. In a container you can take the token from the environment instead,
`token = { env = "DISCORD_TOKEN" }`
([Running in a Container](container.md#secrets-from-the-environment)).

If the token leaks (it's in a screenshot, a chat, or a public repository), reset it at once: the
application's **Bot** page, **Reset Token**. Discord stops accepting the old token when you do. Put
the new one in `secrets.toml` and restart Giles. Giles reads `secrets.toml` only when it starts, so
restarting just the plugin on the admin page keeps the old token.

## Conversations

- A DM between you and the bot is a **direct** conversation. Its id is the DM *channel* id, a number
  Discord assigns to the pair, not your user id. A person has one conversation however many times
  they write.
- Messages in **servers** are ignored. The plugin never subscribes to them, so they never arrive at
  Giles, however the bot is mentioned. Giles v1 doesn't answer group conversations anyway.
- Only messages from people count. Messages from bots (including the plugin's own, which Discord
  sends back) and webhooks, and system messages such as "started a call" or a pin notice, are
  dropped.
- A message with no text, no file, and no sticker (a poll or a forwarded message, for example) is
  dropped. A sticker arrives as a line `[sticker: <name>]` after the text. Embeds are ignored (the
  links in your own message are in its text). A reply to an earlier message arrives with the id of
  the message it replies to.
- Editing or deleting a message after it was delivered changes nothing: the agent was already asked.
  A message edited while Giles was down arrives with its edited text.

Pick the agent for the conversation on the **Routes** page ([Routes](web.md#routes), at
`/admin/routes`): channel `discord` (the instance's name), kind `direct`, and optionally the
conversation id of one DM. A route for channel `discord` and kind `direct` alone answers every
direct message to the bot. Routes aren't in `giles.toml`.

Messages Giles sends on its own (job results, approval prompts, login codes) go to an identity,
`discord:82198898841029460`. Giles opens the DM channel with that user if it isn't known yet
(Discord returns the existing one if there is one). If Discord refuses, the message waits and is
retried until it expires; [Status](#status) lists the reasons. Giles remembers the channel across
restarts.

## Output

Replies are written in Discord's Markdown: `**bold**`, `*italic*`, `~~strikethrough~~`,
`` `code` ``, code blocks fenced with three backticks (with their language), headings with `#`,
`##`, and `###` (deeper levels are `###`), lists with `-` and `1.`, and quotes with `>`. Discord has
no tables, so a table is a code block with aligned columns, and there are no underlines or spoilers.
A link is written as a masked link, `[text](<url>)`, and nothing in a reply unfurls into an embed.
See [Rendering Model Output](../technical/rendering.md#discord) for the rules. The renderer follows
Discord's documentation; nobody has looked at its output in the Discord app yet (issue #20).

**Mentions never ping.** A reply can contain `@everyone`, `@here`, `<@123456>` or a role mention
written by the model, and none of them notifies anybody: the text is altered so that Discord doesn't
read it as a mention, and every message is sent with mentions switched off besides.

A reply longer than 2,000 characters, Discord's limit, is sent as several messages, in order: split
at a paragraph break if it can, else at a line or a space. A code block cut in the middle is closed
in one message and reopened in the next. Characters are counted as Unicode characters, so an emoji
counts once.

Giles doesn't use Discord's reply feature (the little quote above a message) and doesn't show
"typing", since it has no way to know when the agent is working. Each part of an answer is sent
whole when the agent has finished it, not word by word as it is written.

## Attachments

Files you send arrive as attachments of the message, with its text. Giles downloads each one as soon
as the message arrives, because Discord's download links stop working after a while, and only from
Discord's own file servers. A file may be up to `max_blob` (50 MiB by default) and a message may
carry up to 10 files.

A file Giles can't take doesn't stop the message. The message still arrives, with a line after its
text (and after its stickers) for each such file, so the agent can tell you:

```text
[attachment not received: photo.png is over the limit of 50 MiB]
```

The reason in the line is one of: `is over the limit of ...` (the limit is `max_blob`),
`is no longer available from Discord` (the link expired or the file was deleted),
`could not be downloaded` (any other failure), `could not be stored`, or
`was one of more than 10 files in a message` (the files after the tenth). Giles doesn't try again
later, since the link expires and the message is delivered once.

Replies can carry attachments. A file larger than 20 MiB, the most Discord takes from a bot in a
direct message, or larger than `max_blob` if that's less, refuses the whole reply with an error that
names the file and its size, and nothing is sent. Giles doesn't shrink or split files. Files go ten
to a message and only as many as fit Discord's 25 MiB limit on one message's upload; the rest follow
in further messages. The reply's text goes with the first files if it fits in one message (2,000
characters). A longer text is sent first, in messages of its own, and the files follow without text.
The number of files Discord accepts in one message isn't stated in its documentation (10 is what its
own apps use), and the 20 MiB figure comes from the documentation too; both are checked in
[issue #20](https://github.com/jennings/giles/issues/20). Details:
[Attachments](../technical/discord.md#attachments).

## After Downtime

While Giles or the plugin is stopped, Discord keeps your direct messages. When the plugin connects
again it asks Discord for what it missed and delivers it, in order, before newer messages, so a
message you sent while Giles restarted still gets an answer.

- A short drop in the connection is repaired by Discord replaying the gap, with nothing to ask for.
- After every start, and after a long outage, the plugin fetches each known conversation's messages
  newer than the last one it handled, as far back as `catch_up_max_age` (24 hours by default). A
  message older than that is skipped, with a count in the log. Giles remembers the last handled
  message of each conversation in `discord-state.json` in the instance's data directory
  (`data/plugins/<instance>/`), which is part of [backups](backups.md). The file also lists the
  conversations Giles knows. If you lose it, nothing is lost for good, but Giles has to find each
  conversation again: it catches one up (as far back as `catch_up_max_age`, dropping what it already
  has) when that person next writes, or when Giles next messages them.
- Messages Giles already handled aren't answered twice.

What isn't recovered:

- **A person who has never written to the bot while the plugin was running** has no conversation
  Giles knows about, and a bot can't ask Discord which DMs it has. If you message the bot for the
  first time while Giles is stopped, that message is delivered only when you write again. If Giles
  messages you first (a job's result, say), it records the conversation then, and fetches what you
  wrote at its next restart or reconnect (a new Gateway session), not at once. Either way,
  everything since `catch_up_max_age` arrives.
- Messages older than `catch_up_max_age`, and more than 10,000 in one conversation (a limit on one
  catch-up).
- Messages deleted before Giles fetched them.
- Edits made after a message was delivered.

## Status

`giles status` and the admin page ([Admin Pages](web.md#admin-pages), `/admin`) show the channel's
last status:

| Status | What it means | What to do |
| ------ | ------------- | ---------- |
| `connected` | The bot is logged in to Discord | Nothing |
| `needs login`, with the Developer Portal's address | Discord rejected the bot token: it's wrong, was reset, or was revoked (Discord also resets a token if a bot logs in more than 1,000 times in a day, which Giles' reconnect delays keep it far from) | Open the application's **Bot** page, **Reset Token**, put the new token in `secrets.toml`, and restart Giles ([The Token](#the-token)). Until then Giles can't send, and a reply fails |
| `error: disconnected from Discord, reconnecting` | The connection has been down for more than 10 seconds (Discord or the network is unreachable). The plugin keeps trying, waiting 1 second at first and doubling up to 5 minutes | Wait; check the network. It turns `connected` again by itself |
| `error: the connection to Discord ended and won't be retried; restart this plugin to connect again` | The connection ended in a way the plugin doesn't recover from | **Restart** the plugin on the admin page ([Admin Pages](web.md#admin-pages)), or restart Giles |
| `error: Discord refused the intents ...` (Discord closed the connection with code 4013 or 4014) | Discord refused the one intent the plugin asks for. That shouldn't happen unless Discord changed its rules, and nothing in your setup can cause it | Report it |
| `error: Discord requires sharding ...` (close code 4011) | The bot is in too many servers for a plugin that doesn't shard | Remove the bot from servers: it needs one |
| `error: ... (close 4010)` or `(close 4012)` | Discord refused the shard or the API version the plugin asks for | Report it |
| `error: Discord is sending direct messages with no text, files, or stickers ...` | Three direct messages in a row arrived empty. Either Discord changed what a bot receives without the Message Content intent, or the messages were polls or forwards, which look the same | Send a plain text message to check. If those are empty too, report it. Turning on the Message Content switch in the portal doesn't help by itself, because the plugin never asks for that intent. A delivered message clears the status |
| the instance doesn't start: `invalid config: token: ...` | The token is missing or malformed | Fix the `config` table ([Settings](#settings)) |

A reply that fails for another reason doesn't change the channel's status; the log and the failed
reply carry the reason, in words rather than numbers:

- Discord won't let the bot message the user (error 50007): they have blocked it, or don't take
  direct messages from members of the server they share. Check **Privacy Settings** for the server
  and allow direct messages from server members.
- Discord won't let the bot message the user because they share no server (error 50278): invite the
  bot to a server you're in ([Inviting the Bot](#inviting-the-bot)).
- Discord doesn't know the user (error 10013): the id in `[owner] identities` or `default` is wrong.
- Discord says the bot is opening DMs too fast (error 40003): wait. This happens only when Giles
  opens many new DM channels at once.
- Discord is rate limiting the bot: Giles waits as Discord asks, up to a minute for a message.

After Discord rejects the token, Giles makes no more requests to it, so no reply goes out until the
plugin has a good token. Fix the token and restart Giles.

## Connection

The plugin connects when Giles starts and reconnects after a lost connection by itself; Discord
usually lets it pick up where it left off. A Discord that can't be reached doesn't stop the
instance. When the plugin stops, it closes the connection properly, so the bot shows as offline at
once.

Channel plugins may use the network by default. The plugin needs outbound HTTPS (`tcp:443`) only:
Discord's REST API, its Gateway (a secure WebSocket), and the CDN that serves attachments. To allow
just that, grant `sandbox = { network = ["tcp:443"] }`
([Sandbox](configuration.md#sandbox)).
