# Web Chat

The built-in web channel lets you talk to your agents in a browser, on the same web server as the
rest of Giles ([The Web Interface](running.md#the-web-interface)). Turn it on with an instance
table in `giles.toml`:

```toml
[owner]
identities = ["web:owner"]

[channel.web]
```

The instance can have another name (`[channel.chat]` with `component = "web"`), but there can be
only one web channel. Its `config` table takes no keys yet; an unknown key is an error.

Web senders are identities in the `web` namespace. For messages you send from the browser to reach
an agent, `[owner] identities` must list your web identity, `web:owner`. Messages are routed like
any other channel's ([Configuration](configuration.md)): a `[[routes]]` rule can match
`channel = "web"`.

## Chats

The home page lists your chats, most recently active first, each titled with the first line of its
first message. **New chat** opens an empty chat; it appears in the list once you've sent a message
in it. Each chat is its own conversation with its own history, the way each WhatsApp chat is
(`/new` in a chat starts a fresh session in the same chat, shown as "New session").

A chat page shows the conversation so far (the last 200 messages) and a box to write in. Replies
appear as the agent writes them, without reloading. Formatting is Markdown. Links open other sites;
images show only if Giles stores them (attachments), never from other sites, so a reply can't make
your browser fetch a remote address. Tool calls and the model's reasoning aren't shown.

Messages you send are kept until Giles has stored them. If too many are waiting (Giles isn't taking
them), sending fails with "too many messages are waiting"; try again shortly. A message longer than
32 KiB is refused.

If the page lost its connection for a while (or Giles restarted), it says "Some messages may be
missing"; reload to see them. Notices such as "Interrupted by a restart" are shown live but aren't
part of the stored conversation, so they're gone after a reload.

## Approvals

When an agent asks before running a tool, the web chat shows the request with three buttons:
**Allow**, **Allow for this session** (the same tool needs no more approval in this session), and
**Deny**. A button sends `yes <code>`, `yes! <code>`, or `no <code>` for you, exactly as if you'd
typed it ([Chat Commands](commands.md)); typing the reply works too. The prompt then shows what
you sent ("Sent: yes"), and once Giles has acted on the approval, how it ended (allowed, denied,
or expired).

## Attachments

Attachments are served at `/blobs/<id>`, only to you while logged in. Images (PNG, JPEG, GIF, WebP,
AVIF) show inline; any other file downloads instead of opening in the browser.

## Logging In

Every page needs a login. Logging in is not available yet (it comes with the web login: one-time
login links and a trusted reverse proxy); until then, every page answers "You're not logged in".
