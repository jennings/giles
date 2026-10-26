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
only one web channel. Its `config` table is optional; its keys are under
[Logging In](#logging-in). An unknown key is ignored with a
warning at startup, except in `proxy`, where it is an error.

Web senders are identities in the `web` namespace. For messages you send from the browser to reach
an agent, `[owner] identities` must list your web identity, `web:owner`. Messages in a chat
started with **New chat** are routed like any other channel's ([Routes](#routes)): a route can match
the channel `web`. A chat started for a particular agent ([Chats](#chats)) always goes to that agent
and needs no route.

## Chats

The home page lists your chats, most recently active first, each titled with the first line of its
first message. **New chat** opens an empty chat; it appears in the list once you've sent a message
in it. Each chat is its own conversation with its own history, the way each WhatsApp chat is
(`/new` in a chat starts a fresh session in the same chat, shown as "New session").

To talk to a particular agent, pick it under **Chat with** on the chat list and press **Start**.
The list has every configured agent, including one you've just created ([Agents](#agents)), and
that chat always goes to the agent you picked, whatever your
[routes](#routes) say. The chat list and the chat page show which agent such a chat is with. If that
agent is later removed ([Removing an agent](#removing-an-agent)), its chats can't be opened any
more, though they stay in the chat list.

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

## Admin Pages

**Status** on the chat list opens the admin pages, the browser version of `giles status`
([Command Line](cli.md)). Only the owner can open them.

- `/admin` shows the version, when Giles started, whether this machine can sandbox plugins, every
  channel with its last status (connected, an error, or waiting to be linked), and a summary of
  the plugins.
- `/admin/plugins` shows each plugin in detail: its state (running, retrying, failed), restarts,
  last error, channel status, binary or URL, version, and sandbox. **Restart** restarts a plugin
  now, even one that failed for good or is waiting out its retry delay. Toolsets have no button
  yet: Giles doesn't track their state on these pages.

### Linking a Channel

A channel that needs linking or a login (such as WhatsApp before you've linked it, or after it
was logged out) shows what it needs on the status page:

- A QR code: scan it with the app (WhatsApp: **Linked devices**, **Link a device**).
- A pair code: enter it in the app.
- An address to open (a link only if it's `http` or `https`). Discord shows the Developer Portal's
  address this way when it rejects the bot token ([Discord](discord.md#status)).

While any channel waits to be linked, the page reloads itself every 5 seconds, so it always shows
the channel's newest code (WhatsApp replaces its QR code every 20 seconds or so). Once the channel
reports that it's connected, the code disappears and the reloading stops.

## Agents

**Agents** in the admin pages' menu (`/admin/agents`) lists every agent with its models, a link to
its page, **Chat** to start a chat with it, **Edit**, and **Remove**. Only the owner can open these
pages.

An agent's page shows its models, tools, and time zone, then every file in `agents/<agent>/`:
`agent.toml`, `SOUL.md`, `MEMORY.md`, what it remembers about each user
(`users/<user>/MEMORY.md`) and learned in each group (`groups/<instance>/<conversation>/MEMORY.md`,
labeled with the conversation as the channel names it, `#giles` rather than `%23giles`), its jobs'
prompt files, its skills, and anything else there. Contents are shown as plain text. For now you see
every file; once Giles has several users, each user will see only their own memory file of an
agent. Limits keep the page small:

- At most 200 files are listed, at most 8 directories deep; names starting with `.` are skipped.
- Each file shows at most its first 64 KiB, and the page at most 1 MiB of contents in all.
- A file that isn't text (an image, say) is listed with its size, not shown.
- A symbolic link is listed but never followed, so a link can't show a file from outside the
  agent's directory.

**New agent** opens a form that creates one:

- **Name**: the display name. Optional if you give an id.
- **Id**: the directory name, letters, digits, and `-`. It is always a plain, editable field. As
  you type the name, the id fills in from it: lowercase, each space turned into `-`, and
  everything else (punctuation, symbols, other whitespace, and letters outside `a`-`z`, so `é`
  too) removed: "The Butler!" becomes `the-butler`, "Café" becomes `caf`. Once you edit the id
  yourself it stays as you typed it; clear it and it follows the name again. If the id is empty
  when you create the agent (the page's script didn't run, say), Giles makes it from the name the
  same way. An id that's taken (ignoring case) or `new`, and an id that comes out empty, is
  refused.
- **Models**: `<provider>/<model>`, several separated by commas. The field suggests the models
  your providers list; you can type any other.
- **Tools** and **Ask first**: optional tool names or globs, separated by commas or spaces.
- **Time zone**: optional, such as `Europe/London`.
- **SOUL.md**: who the agent is, at most 64 KiB. The form starts with "This is your information.
  Update it as you learn new things about yourself." and "You are a helpful personal assistant."
  as two paragraphs; change it freely, or empty it for an empty file.

Giles checks the agent exactly as it checks a hand-written `agent.toml` at startup (every model
must name a configured provider, tool globs can't be empty, and so on). If something is wrong, the
form comes back with the problem and nothing is written. Otherwise Giles writes
`agents/<id>/agent.toml` and `SOUL.md`, and the agent can be used at once, without a restart: chat
with it from its page or the chat list. It runs with the defaults for everything the form doesn't
ask (see [Configuration](configuration.md)); edit `agent.toml` by hand for the rest, which, like
any other configuration change, takes effect at the next restart.

### Editing an agent

**Edit**, on the list and on an agent's page, opens the same form filled in from the agent's files
(`agent.toml` and `SOUL.md`) as they are on disk now. The **Id** is shown but can't be changed: it
is the agent's directory name, `agents/<id>/`. To use another id, create a new agent and remove the
old one.

Giles checks the edit as it checks a new agent or a hand-written `agent.toml` at startup. If
something is wrong, the form comes back with the problem and what you typed, and the files are left
as they were. Otherwise it saves, and the agent changed without a restart:

- A chat you start afterwards uses the new models, tools, ask list, time zone, and `SOUL.md`, and so
  does the next run of the agent's [jobs](configuration.md#jobs) (they're read when they run). A
  chat already going keeps the tools and instructions it started with; use `/new` to start a fresh
  one, or let it be (`SOUL.md` is read again when a long chat is compacted).
- Giles changes only what the form manages: `name`, `models`, `tools`, `ask`, `timezone`, and
  `SOUL.md`. Everything else in `agent.toml` stays as you wrote it: comments, `[[jobs]]`,
  `compaction_model`, `busy_mode`, `memory_cap`, the iteration limits, `idle_reset`, and any
  setting added later. A setting you leave as it was isn't rewritten at all, and one you empty
  (name, tools, ask first, time zone) is removed from the file. A comment right above a removed
  setting goes with it, except the comments at the very top of the file and any set off by a blank
  line.
- Both files are written, or neither: Giles writes each to a temporary file and moves them into
  place, and puts the first back if the second fails.
- If you edit `agent.toml` or `SOUL.md` by hand while the form is open, saving the form is
  refused ("changed on disk") rather than overwriting your change. The form stays as you typed it,
  with a link to reload the current values.
- A file Giles won't write through is left to you, and the form says so: an `agent.toml` that
  isn't valid TOML, a file bigger than 1 MiB, one that isn't UTF-8, and a `SOUL.md` or `agent.toml`
  that is a symbolic link.
- Changing `[[jobs]]` or other settings by hand still needs a restart to take effect (unless you
  then save this form, which loads the whole file).

### Removing an agent

**Remove**, on the list and on an agent's page, opens a page that says what removing the agent
does, with a **Remove agent** button. Nothing happens until you press it.

- **The files are moved, not deleted.** `agents/<id>/` (`agent.toml`, `SOUL.md`, `MEMORY.md`, the
  memory of each user and group, job prompt files, skills) is renamed to
  `trash/agents/<id>-<date and time in UTC>/` in the data directory, for example
  `trash/agents/jeeves-20261024T120000Z/`. To bring the agent back, move that directory to
  `agents/<id>/` and restart Giles. Giles never reads, empties, or backs up `trash/`
  ([Backups](backups.md)); delete what you don't want, whenever you like.
- **The agent is out of Giles at once**, without a restart: it disappears from the agent list and
  the **Chat with** chooser, a chat can't be started with it, and its existing chats can't be
  opened or continued.
- **Its jobs stop.** The `[[jobs]]` in its `agent.toml` stop running (they stay in the moved file),
  and the jobs it scheduled itself with the `schedule` tool are deleted. The page counts both.
- **Its history stays** in the database, out of sight: chats, messages, and usage. They are not
  deleted. If you later create an agent with the same id, those old chats belong to the new agent
  again (it answers them with its own settings and `SOUL.md`). To start clean, pick another id.
- **The id is free right away**: you can create a new agent with the same id, and removing that
  one too gives it another directory in `trash/` (`-1`, `-2` after the time if two removals fall in
  the same second).

Giles refuses to remove an agent in two cases, and the page says which:

- **Routes still name it.** The page lists them, each linking to the [routes page](#routes). Point
  each at another agent, or remove it, then come back. Giles doesn't change your routes for you.
- **It is working right now**: a chat turn is running or messages are waiting for one, or one of
  its jobs is running (a scheduled job's run counts from the moment its time comes). Stop the
  chat with `/stop` or wait, then try again.

Giles takes the agent out of use and checks for work once more before it moves any files, so a
chat message or job that starts just as you press the button either is seen (the removal is
refused and the agent stays as it was) or finds the agent already gone and does nothing. If the
files can't be moved, the agent is put back as it was. A job whose time comes in that very instant
doesn't run that time; it stops for good until Giles restarts.

## Routes

**Routes** in the admin pages' menu (`/admin/routes`) is where you decide which agent answers each
conversation, on every channel. Only the owner can open these pages. Routes are tried from the top
and the first one that matches wins, so the order matters: put a specific route (one conversation)
before a broader one (a whole channel, or everything). A chat started for a particular agent
([Chats](#chats)) skips the routes. A message in a conversation no route matches is dropped, so a
fresh Giles has no routes and answers nobody until you add one.

The list shows each route's position, channel, kind, conversation, and agent (a link to the agent's
page), with these buttons:

- **Edit** opens the route's form, filled in.
- **Up** and **Down** move the route one place. The first route has no Up and the last no Down.
- **Remove** deletes the route.

**New route** opens a form:

- **Channel**: one of your channel instances, or any channel.
- **Kind**: direct, group, or any (Giles ignores group conversations for now).
- **Conversation**: the conversation id as the channel reports it, such as `#giles` on IRC or a
  phone number's chat on WhatsApp; empty for any. It must match exactly, capitals included.
- **Agent**: required, one of your agents.
- **Position**: at the end, or before a route you pick.

A route that sets nothing but the agent matches every conversation; keep it last.

Giles checks a route like it checks configuration. The channel instance and the agent must exist. It
also refuses a route that nothing could ever reach, and says why: an earlier route already matches
everything it matches (a duplicate is the plainest case), or the change would put a route in front
of one it hides. Adding, editing, and moving all follow this rule, so to put a broad route first you
move the specific ones above it. The form comes back with the problem and nothing is changed. A
route that has stopped working anyway (its agent's directory was removed by hand, or its channel
instance dropped from `giles.toml` before a restart) stays in the list with a warning under it, as
does one that an earlier route hides, which can happen to routes imported from an old `giles.toml`.

A change applies to the next message Giles receives, with no restart.

Routes used to be `[[routes]]` in `giles.toml`. Giles imports them once, at the first start with
this version, and then never reads them again; the Routes page tells you if the file still has
them, and you can delete them. See [Configuration](configuration.md#routes).

## Logging In

Every page needs a login. There are two ways to log in.

### Login Links

`giles admin login-link --user owner` prints a one-time URL. Opening it shows a **Log in** button;
pressing it logs that browser in for 30 days. A link works once and only for 10 minutes, and a
restart of Giles cancels any unused ones. (Opening the link doesn't use it up, so a chat app that
fetches a link to preview it can't spend it.) **Log out** on the chat list ends the login. See
[Command Line](cli.md).

`[owner] identities` must list `web:owner`: messages you send after logging in with a link come
from that identity. If you remove it later, existing logins stop working.

The link starts with the address Giles listens on (`core.http_listen` as bound, so port `0` shows
the port it picked, with `0.0.0.0` shown as `127.0.0.1`). If people reach Giles at another address,
such as through a reverse proxy, set it:

```toml
[channel.web.config]
public_url = "https://giles.example.com"
```

`public_url` is just the scheme, host, and optional port; Giles is always served at the root. With
an `https` URL the login cookie is marked `Secure`, so the browser only sends it over HTTPS.

The login is a cookie (`HttpOnly`, `SameSite=Lax`). Every form also carries a token tied to the
login, so another site can't post messages or approvals as you.

### A Trusted Reverse Proxy

If a reverse proxy already authenticates people (Tailscale Serve, oauth2-proxy, Authelia), Giles
can take the user's identity from a header the proxy sets. Giles believes that header only on a
separate listener meant for the proxy, never on `core.http_listen`: any program on the machine,
plugins included, can connect to that one.

```toml
[channel.web.config.proxy]
identity_header = "Tailscale-User-Login"
```

This creates the Unix socket `run/proxy.sock` (mode 0600: only the user Giles runs as can connect,
so run the proxy as that user). Point the proxy at it. If the data root's path is too long for a
socket, the socket is in a private directory under `$TMPDIR` instead; `giles serve` logs where
("trusted-proxy listener open"). The header's value is your identity in the `web` namespace, so
list it in `[owner] identities`, e.g. `"web:alice@example.com"`. A request through the proxy
without the header falls back to the login cookie; one naming someone not listed is refused.

If the proxy can't use a Unix socket, give the proxy listener a TCP address and a shared secret
that the proxy sends in a header with every request:

```toml
[channel.web.config.proxy]
identity_header = "X-Forwarded-User"
listen = "127.0.0.1:8081"
secret = "a long random string"    # at least 16 bytes
secret_header = "X-Giles-Proxy-Secret"  # the default
```

Requests to that address without the secret are refused. Through the proxy listener, an
`X-Forwarded-Proto: https` header also marks the login cookie `Secure`. There's no logout for a
proxy login: the proxy decides who you are on every request.
