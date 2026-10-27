# Discord

Giles can talk to you in direct messages through a Discord bot that you create. The channel is the
plugin `giles-channel-discord`.

**Giles logs in as a bot, never as your own account.** Automating a normal user account (a
"self-bot") is against Discord's terms. Your bot is bound by Discord's
[Developer Terms of Service](https://support-dev.discord.com/hc/articles/8562894815383-Discord-Developer-Terms-of-Service).

## Create the bot

1. Open the [Discord Developer Portal](https://discord.com/developers/applications), log in, and
   choose **New Application**. The application's name is only a label. You can change the bot's
   name, which is what you see in Discord, on the **Bot** page.
2. Open the application's **Bot** page. If it offers a **Public Bot** switch, turn it off, so nobody
   else can add your bot to their servers.
3. On the **Bot** page, choose **Reset Token** and copy the token. Discord shows it only once. If
   you lose it, reset it again.

You don't need to turn on any **Privileged Gateway Intents**.

The Developer Portal's labels change from time to time, so they may differ a little from what you
see.

## Invite the bot to a server

You and the bot can only message each other if you share a Discord server. Create a small private
server, or use one you own, and invite the bot:

1. In the Developer Portal, open the application's **OAuth2** page and its **URL Generator**. Older
   layouts of the portal name these differently. You need an invite link with the `bot` scope.
2. Tick the `bot` scope. Leave every permission unticked.
3. Open the generated link, pick the server, and choose **Authorize**.

Your privacy settings for that server must allow direct messages from server members. Otherwise
Discord refuses the bot's messages to you.

## Find your user ID

Giles recognizes you by your Discord user ID, a number, and not by your name.

1. In the Discord app, open **User Settings**, then **Advanced**, and turn on **Developer Mode**.
2. Right-click (or long-press) your own name or avatar and choose **Copy User ID**.

Add it to `[owner] identities` in `giles.toml`:

```toml
[owner]
identities = ["discord:82198898841029460"]
```

Messages from an ID that isn't listed are dropped. The log line `dropped inbound message` shows the
sender in exactly the form to add.

## Store the token

Anyone who has the bot token can act as your bot. Put it in
[`secrets.toml`](configuration.md#secrets) in the data root, and keep it out of `giles.toml`:

```toml
discord = "paste the token here"
```

```sh
chmod 600 ~/.giles/secrets.toml
```

[In a container](container.md#secrets), you can take the token from the environment instead, with
`token = { env = "DISCORD_TOKEN" }`.

If the token leaks, reset it at once with **Reset Token** on the application's **Bot** page. Put the
new token in `secrets.toml` and restart Giles. Restarting only the plugin keeps the old token.

## Configure Giles

Add the channel to `giles.toml`. With your owner entry, it looks like this:

```toml
[owner]
identities = ["discord:82198898841029460"]

[channel.discord]
component = "discord"

[channel.discord.config]
token = { secret = "discord" }
```

The instance name (`discord` here) is up to you, but it's permanent. The plugin keeps its data under
that name.

Start Giles. `giles status` should show the channel as `connected`, and the bot shows as online in
Discord.

## Settings

The `config` table of the instance takes these keys. Only `token` is required.

| Key                | Default    | Meaning                                                                |
| ------------------ | ---------- | ---------------------------------------------------------------------- |
| `token`            | required   | The bot token ([Store the token](#store-the-token)); use a secret reference |
| `max_blob`         | `"50 MiB"` | The largest attachment Giles downloads or sends ([Attachments](#attachments)) |
| `catch_up_max_age` | `"24h"`    | How far back Giles looks for messages it missed ([After downtime](#after-downtime)) |

- `token` must be the token alone, without the `Bot` prefix from Discord's examples.
- `max_blob` should be no higher than the instance's own [`max_blob`](configuration.md#channels),
  which is also 50 MiB by default.
- `catch_up_max_age` is a [duration](configuration.md#value-formats) of at most `"30d"`. `"0s"`
  turns catch-up off.

```toml
[channel.discord]
component = "discord"

[channel.discord.config]
token = { secret = "discord" }
max_blob = "20 MiB"
catch_up_max_age = "12h"
```

## Conversations

Pick the agent for your direct messages on the [Routes page](web.md#routes), at `/admin/routes`:

- Channel: `discord` (the instance's name).
- Kind: `direct`.
- Conversation ID: leave it empty to answer every direct message to the bot. The ID of one DM is its
  channel ID, which isn't your user ID.

Then send the bot a direct message.

Only direct messages to the bot reach Giles. Messages in servers never do, even if they mention the
bot.

Polls and forwarded messages are dropped. Editing or deleting a message after it was delivered
changes nothing.

## Replies

Replies use Discord's Markdown. Discord has no tables, so a table becomes a code block with aligned
columns. [Rendering model output](../technical/rendering.md#discord) has the details.

**Mentions in a reply never ping anybody**, including `@everyone`, `@here`, and user or role
mentions.

A reply longer than 2,000 characters is sent as several messages. Giles doesn't show "typing". It
sends each part of an answer once the agent has finished it.

## Attachments

Files you send reach the agent with your message. Each file can be up to `max_blob` (50 MiB by
default), and a message can carry up to 10 files.

If Giles can't take a file, the message still arrives, with a line saying why:

```text
[attachment not received: photo.png is over the limit of 50 MiB]
```

Replies can carry files up to 20 MiB, or up to `max_blob` if that's smaller. A larger file stops the
whole reply with an error that names the file, and nothing is sent.

[Attachments](../technical/discord.md#attachments) in the technical guide has the details.

## After downtime

Discord keeps your direct messages while Giles is stopped. When the plugin connects again, it
delivers the ones it missed, in order, as far back as `catch_up_max_age` (24 hours by default).
Older messages are skipped.

Giles learns about a conversation when you first message the bot while it's running. If your very
first message to the bot is sent while Giles is stopped, it's delivered only when you write again.

## Status

`giles status` and the [admin page](web.md#admin-pages) show the channel's last status:

| Status | What it means | What to do |
| ------ | ------------- | ---------- |
| `connected` | The bot is logged in to Discord | Nothing |
| `needs login`, with the Developer Portal's address | Discord rejected the bot token: it's wrong, was reset, or was revoked | Choose **Reset Token** on the application's **Bot** page, put the new token in `secrets.toml`, and restart Giles. Until then, no reply goes out |
| `error: disconnected from Discord, reconnecting` | Discord or the network has been unreachable for more than 10 seconds | Check the network. The plugin keeps trying and turns `connected` again by itself |
| `error: the connection to Discord ended and won't be retried; restart this plugin to connect again` | The plugin can't recover the connection | **Restart** the plugin on the [admin page](web.md#admin-pages), or restart Giles |
| `error: Discord requires sharding ...` | The bot is in too many servers | Remove the bot from servers. It needs only one |
| `error: Discord refused the intents ...`, or `error: ... (close 4010)` or `(close 4012)` | A bug, or a change at Discord. Nothing in your setup causes it | Report it |
| `error: Discord is sending direct messages with no text, files, or stickers ...` | Three direct messages in a row arrived empty. Polls and forwards look the same | Send a plain text message. If it arrives empty too, report it. Turning on the Message Content switch in the portal doesn't help |
| The instance doesn't start: `invalid config: token: ...` | The token is missing or malformed | Fix the `config` table ([Settings](#settings)) |

A reply can also fail without changing the status. The log and the failed reply say why:

- The bot can't message you (error 50007). You've blocked it, or your **Privacy Settings** for the
  shared server don't allow direct messages from server members.
- You share no server with the bot (error 50278). [Invite the bot](#invite-the-bot-to-a-server) to a
  server you're in.
- Discord doesn't know the user (error 10013). The ID in `[owner] identities` or `default` is wrong.
- The bot is opening DMs too fast (error 40003). Wait.

Messages Giles sends on its own, such as job results, are retried until they expire.

## Network access

The plugin needs only outbound HTTPS (`tcp:443`). [The sandbox](configuration.md#sandbox) lets
channel plugins use the network by default. To allow only HTTPS, set
`channel.discord.sandbox = { network = ["tcp:443"] }`.
