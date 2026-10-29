# Web chat

The built-in web channel lets you talk to your agents in a browser. It runs on
[the same web server](running.md#the-web-interface) as the rest of Giles.

To turn it on, add a `[channel.web]` table to `giles.toml`. `[owner] identities` must list
`web:owner`, which is the identity you use in the browser.

```toml
[owner]
identities = ["web:owner"]

[channel.web]
```

You can give the instance another name, such as `[channel.chat]` with `component = "web"`, but you
can have only one web channel.

## Logging in

Every page needs a login. You log in with a one-time link, or through a trusted reverse proxy.

### Login links

Run this to get a login link:

```sh
giles admin login-link --user owner
```

Open the link and press **Log in** to log that browser in for 30 days. The link works once, and only
for 10 minutes. **Log out** on the chat list ends the login. See [Command line](cli.md).

The link starts with the address Giles listens on. If you reach Giles at another address, such as
through a reverse proxy, set `public_url`:

```toml
[channel.web.config]
public_url = "https://giles.example.com"
```

Give only the scheme, host, and optional port. Giles is always served at the root of that address.

### A trusted reverse proxy

If a reverse proxy already authenticates people, such as Tailscale Serve, oauth2-proxy, or Authelia,
Giles can take your identity from a header the proxy sets:

```toml
[channel.web.config.proxy]
identity_header = "Tailscale-User-Login"
```

Giles reads that header only on a separate listener for the proxy. It ignores the header on
`core.http_listen`, because any program on the machine can connect there, plugins included.

By default the proxy listener is the Unix socket `run/proxy.sock` in the data root. Only the user
Giles runs as can connect to it, so run the proxy as that user and point it at the socket. If the
data root's path is too long for a socket, Giles puts it under `$TMPDIR` instead and logs where, in
the line `trusted-proxy listener open`.

The header's value is your identity in the `web` namespace. List it in `[owner] identities`, such as
`"web:alice@example.com"`.

If the proxy can't use a Unix socket, give the listener a TCP address and a shared secret. The proxy
must send the secret in a header with every request:

```toml
[channel.web.config.proxy]
identity_header = "X-Forwarded-User"
listen = "127.0.0.1:8081"
secret = "a long random string"    # at least 16 bytes
secret_header = "X-Giles-Proxy-Secret"  # the default
```

There's no logout for a proxy login. The proxy decides who you are on every request.

## Chats

The home page lists your chats. Each chat is a separate conversation with its own history.

Messages in a chat you open with **New chat** go to the agent your [routes](#routes) pick. To talk
to a particular agent instead, pick it under **Chat with** and press **Start**. That chat always
goes to the agent you picked, and needs no route.

Replies appear as the agent writes them. Tool calls and the model's reasoning aren't shown. Image
attachments show inline, and other attachments download. Images from other sites aren't loaded.

If something goes wrong:

- If sending fails with "too many messages are waiting", try again shortly.
- A message longer than 32 KiB is refused.
- If the page says "Some messages may be missing", reload it.

## Approvals

When an agent asks before running a tool, the chat shows the request with three buttons:

- **Allow**
- **Allow for this session**, so the same tool needs no more approval in this session
- **Deny**

The buttons send [the same replies](commands.md) you could type, and typing them works too.

## Admin pages

**Status** on the chat list opens the admin pages. Only the owner can open them.

`/admin` shows what [`giles status`](cli.md) shows, including each channel's last status.

`/admin/plugins` shows each plugin in detail, including its state, restarts, and last error. It
lists MCP toolsets too, with the number of tools each offers. **Restart** restarts a plugin or
toolset now, even one that failed for good or is waiting to retry.

### Linking a channel

A channel that needs linking or a login, such as WhatsApp before you've linked it, shows what it
needs on the status page:

- A QR code: scan it with the app (WhatsApp: **Linked devices**, **Link a device**).
- A pair code: enter it in the app.
- An address to open. [Discord](discord.md#status) shows the Developer Portal's address this way
  when it rejects the bot token.

The page reloads itself until the channel connects, so it always shows the newest code.

## Agents

**Agents** in the admin menu lists every agent, with buttons to chat with it, edit it, or remove it.
An agent's page shows its models, tools, and time zone, and the files in `agents/<agent>/` as plain
text.

**New agent** opens a form that creates one:

- **Name**: the display name. Optional if you give an id.
- **Id**: the agent's directory name, made of letters, digits, and `-`. It fills in from the name as
  you type, and you can edit it. You can't change it once the agent exists.
- **Models**: `<provider>/<model>`, several separated by commas.
- **Tools**: tick the tools the agent may use. An agent can't use a tool you leave unticked, so a
  new agent has none until you tick some. Each [MCP server](#mcp-servers) is one box that allows all
  of its tools.
- **Other tools**: optional names or globs the boxes can't show, such as `github__create_*`,
  separated by commas or spaces. A box that one of these already allows is ticked and greyed out.
- **Ask first**: optional tool names or globs that need your approval before each call, separated by
  commas or spaces.
- **Time zone**: optional, such as `Europe/London`.
- **SOUL.md**: who the agent is.

Giles checks the agent as it checks a hand-written `agent.toml`. If something is wrong, such as a
model naming a provider you haven't configured, the form shows the problem and nothing is written.

Otherwise Giles writes `agents/<id>/agent.toml` and `SOUL.md`. You can chat with the agent at once,
without a restart.

For settings the form doesn't have, edit [`agent.toml`](configuration.md) by hand. Like any other
configuration change, that takes effect at the next restart.

### Editing an agent

**Edit** opens the same form, filled in from the agent's files. The **Id** can't be changed. To use
another id, create a new agent and remove the old one.

Saving takes effect without a restart:

- New chats and the next run of the agent's [jobs](configuration.md#jobs) use the new settings. A
  chat already going keeps the settings it started with. Use `/new` to start a fresh one.
- Giles changes only what the form has: `name`, `models`, `tools`, `ask`, `timezone`, and `SOUL.md`.
  The rest of `agent.toml`, comments included, stays as you wrote it.
- If you edit `agent.toml` or `SOUL.md` by hand while the form is open, saving is refused with
  "changed on disk". Use the form's link to reload the current values.

Changes you make to `agent.toml` by hand, such as to `[[jobs]]`, take effect at the next restart, or
when you next save this form.

### Removing an agent

**Remove** opens a page that says what removing the agent does. Nothing happens until you press
**Remove agent**.

Removing an agent:

- Moves `agents/<id>/`, memories included, to `trash/agents/<id>-<time>/` in the data root, such as
  `trash/agents/jeeves-20261024T120000Z/`.
- Takes the agent out of use at once. You can't start a chat with it, and its existing chats can't
  be opened.
- Stops its `[[jobs]]`, and deletes the jobs it scheduled itself with the `schedule` tool.
- Keeps its chats, messages, and usage in the database, out of sight.

To bring the agent back, move its directory back to `agents/<id>/` and restart Giles. Giles never
empties or backs up `trash/`, so delete what you don't want yourself.

You can reuse the id at once. A new agent with the same id takes over the old agent's chats and
answers them with its own settings. To start clean, pick another id.

Giles refuses to remove an agent in two cases:

- **Routes still name it.** The page lists them. Point each at another agent or remove it, then try
  again.
- **It is working right now.** A chat turn or one of its jobs is running, or messages are waiting.
  Stop the chat with `/stop` or wait, then try again.

### Memory

**Memory** on an agent's page lets you search, add, edit, and forget the agent's memories.
[How memory works](skills-and-memory.md#memory) is explained on its own page.

You see your own private memories and the shared ones, newest change first. Other users' private
memories are never shown, not even to the owner. The page lists the newest 200, and search finds the
rest. Search works like the agent's `memory_recall` tool.

- **Add** a memory with its text. Tick **Core** to load it into every prompt that can see it. Other
  memories are only found by search.
- **Shared** makes a memory visible to every user and group of the agent. Only the owner sees this
  box, and only the owner can change or forget a shared memory.
- **Edit** changes a memory's text and flags.
- **Forget** deletes a memory, without asking first.

A memory is at most 1,000 characters. Your private core memories together, and the shared core
memories together, must fit in the agent's `memory_cap`. If a change would go over, nothing is
saved. The page says by how much, so you can shorten or forget some, or untick **Core**.

If a memory changed after the page loaded, for example because the agent edited it, your edit is
refused. The page shows what you typed and what the memory says now, so you can redo it.

A change to a core memory reaches a running chat after it compacts, or in a new chat or job run.
`memory_recall` finds the change at once.

Below your own memories, the owner sees a section for each group conversation that has private
memories. Nobody else can see or change a group's memories.

## Routes

**Routes** in the admin menu decides which agent answers each conversation, on every channel. A
message in a conversation that no route matches is dropped. A fresh Giles has no routes, and answers
nobody until you add one.

Routes are tried from the top, and the first match wins. Put a route for one conversation before a
broader one for a whole channel or everything.

**New route** opens a form:

- **Channel**: one of your channel instances, or any.
- **Kind**: direct, group, or any. Giles ignores group conversations for now.
- **Conversation**: the conversation id as the channel reports it, such as `#giles` on IRC, or empty
  for any. It must match exactly, capitals included.
- **Agent**: required.
- **Position**: at the end, or before a route you pick.

A route that sets nothing but the agent matches every conversation, so keep it last.

Each route in the list has **Edit**, **Up**, **Down**, and **Remove** buttons. A change applies to
the next message, without a restart.

Giles refuses a change that would leave a route unreachable, such as a duplicate, or a broad route
moved above a specific one. The form says what is wrong. To move a broad route up, move the specific
routes it would hide above it instead.

A route that stopped working, for example because its agent was removed by hand, stays in the list
with a warning under it.

Giles imports `[[routes]]` from an older `giles.toml` once. After that you can delete them from the
file. See [Configuration](configuration.md#routes).

## MCP servers

**MCP servers** in the admin menu lists the MCP servers that give your agents more tools. Each row
shows how the server runs, its state, how many tools it offers, and its last error. A server's page
shows its settings and the name and description of each tool, and has a **Restart** button.

Here you add, edit, rename, and remove remote servers, and the change applies at once, without a
restart. A conversation that is already going keeps the tools it started with. New conversations get
the new list.

A server that Giles runs on this machine, a stdio server, is a `[toolset.<name>]` table in
`giles.toml`, which [Configuration](configuration.md#toolsets) describes. It shows in the list too,
but you can only change it in the file. Giles ignores a `[toolset.<name>]` table with `url` and logs
a warning at every start. Add that server here instead.

### Adding a remote server

A remote server is one you reach over the network. Giles doesn't run or sandbox it.

1. Put the server's token in [`secrets.toml`](configuration.md#secrets), such as
   `github = "<token>"`, and keep the file private with `chmod 600 ~/.giles/secrets.toml`.
2. Open **New MCP server** and enter a **Name**, such as `github`. Its tools are named
   `github__<tool>`.
3. Enter the **URL**, such as `https://api.githubcopilot.com/mcp/`.
4. For **Authentication**, choose **A secret in secrets.toml** and enter `github`.
5. Select **Add**.

The server's page opens and shows its state and its tools. If Giles can't connect, the page shows
why.

The form never shows a token. It shows the name of the secret. Giles reads `secrets.toml` again each
time it connects, so a secret you add later needs no restart.

The form has more settings, all optional:

- **Allow insecure connections** lets Giles reach another machine over plain `http://`. It shows
  when the URL starts with `http:`. Giles accepts plain `http://` to this machine without it. Tick
  it only on a network you trust.
- **Deadline** limits how long one tool call may run. It defaults to `5m`.
- **Tools may run in parallel** lets an agent's calls to the server run at the same time.
- **Skills** lists directories of skills the server offers.

If something is wrong, the form shows the setting and the reason, keeps what you typed, and stores
nothing.

### Editing and removing an MCP server

**Edit** opens the form filled in. The name can't change there. Use **Rename** for that. Saving
restarts the server's connection with the new settings. If someone changed the server after you
opened the form, saving is refused. Use the form's link to reload the current values.

A server that failed to start, for example because its secret was missing, stays in the list with
the reason. Fix the cause and select **Restart**, or change its settings and save.

**Remove** opens a page that says what removing does. Nothing happens until you select
**Remove MCP server**. Giles disconnects from the server and deletes its settings. The page lists
the agents whose tools or ask-first list name the server's tools. Giles leaves those agents as they
are.

### Renaming an MCP server

The name is the prefix of the server's tools, and agents name those tools in their tools and
ask-first lists, so a rename changes more than the name. To rename an MCP server:

1. Open the server's page and select **Rename**.
2. Enter the **New name** and select **Show the changes**.
3. Check the list of agents. Giles rewrites the entries in each agent's tools and ask-first lists
   that start with `<old name>__` to start with `<new name>__`.
4. Select **Rename to** `<new name>`.

Giles disconnects from the server, rewrites the agents, and connects again under the new name. If a
step fails, it undoes the steps before it, and the page says which step failed and why. A
conversation that is already going keeps the old tool names, and its calls to them fail. Start a new
conversation to use the new names.

Giles refuses the rename, and changes nothing, if the new name is taken, or if an agent has a glob
that matches the server's tools without starting with `<old name>__`, such as `github*`. The page
lists those globs. Rewrite them, for example to `github__*`, and try again.

You can't rename a server that you configure in `giles.toml` here. Stop Giles and run
[`giles toolset rename`](cli.md#rename-a-toolset) for that.
