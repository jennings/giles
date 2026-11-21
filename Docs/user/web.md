# Web chat

The built-in web channel lets you talk to your agents in a browser. It runs on
[the same web server](running.md#the-web-interface) as the rest of Doozers.

To turn it on, add a `[channel.web]` table to `doozers.toml`:

```toml
[channel.web]
```

You can give the instance another name, such as `[channel.chat]` with `component = "web"`, but you
can have only one web channel.

## Logging in

Every page needs a login. You log in with a passkey, a password, an identity provider, a one-time
link, or through a trusted reverse proxy. A page that needs a login points you to `/login`, which
lists the ways to log in.

### Invite and recovery links

An invite link sets up a new user. A recovery link signs in a user who has lost every other way to
sign in, and it keeps every way the user can already sign in. Both work once, within 72 hours.

Run this to make a recovery link:

```sh
doozers admin login-link
```

On a fresh install, this creates the first admin. With one user, the link is for that user. With
several, name the user with `--user <id>`, such as `--user owner` for the owner you imported from
[`[owner]`](configuration.md#owner). The command prints whose link it is. It works for any active
user, including one who has never signed in.

Open the link and press the button on the page to log that browser in for 30 days. A chat app or
mail client that fetches the link to show a preview doesn't use it up. The next page,
**Set up how you sign in**, lists the ways you can add, such as a [password](#passwords). Until you
add one, the link is the only way to sign in, so ask for a new one when this login ends. **Log out**
at the bottom of the sidebar ends the login. See [Command line](cli.md).

An admin can't make invite links in the web UI yet.

The link starts with the address Doozers listens on. If you reach Doozers at another address, such
as through a reverse proxy, set `public_url`:

```toml
[channel.web.config]
public_url = "https://doozers.example.com"
```

Give only the scheme, host, and optional port. Doozers is always served at the root of that address.

### Passwords

A password lets you log in at `/login` with your username and password, without a link.

To set a password, open an invite or recovery link, then choose **Password** on the page that
follows, or use **Set a password** on your [account](#ways-to-sign-in). To set or change it later,
[confirm it's you](#confirming-its-you) first. A login that came from a password can't change it
without that.

A password has 15 to 128 characters. Spaces and any other characters are fine, and nothing else is
required, so a few random words make a good password. It can't be one character repeated, or contain
your username or the word Doozers.

Nobody can read a password back, including an admin. If you forget it, set a new one with a recovery
link.

**A password sent over plain HTTP can be read by anyone on the network.** When the address starts
with `http://` and isn't `localhost` or another address on your own machine, the login page and the
page that sets a password say so. Reach Doozers over HTTPS, and set
[`public_url`](#invite-and-recovery-links) to the `https` address.

Doozers allows 10 wrong passwords for a username and 30 from one address in 15 minutes. After that
you wait for the rest of the 15 minutes. While a username is waiting, even its right password
doesn't log in, so use a recovery link instead.

Doozers forgets these counts when it restarts.

A disabled user can't log in with a password.

If a reverse proxy on the same machine reaches Doozers at `core.http_listen`, only the limit per
username applies, because every visitor looks like the proxy. Use the
[proxy listener](#a-trusted-reverse-proxy) to get the limit per address too, and have the proxy add
the visitor's address to the `X-Forwarded-For` header. Without that header, Doozers counts every
visitor through the proxy as one address.

### Confirming it's you

Adding, changing, or removing a way to sign in needs more than a login, so someone who got hold of
your browser's login can't take over your account. Doozers asks you to confirm it's you with a way
you already use: your password, a passkey, or the identity provider you connected. A login that came
from an invite or recovery link doesn't need to confirm for its first 30 minutes, because that link
is how you proved who you are. So finish setting up soon after you open the link. After that,
confirm it's you like any other login, or ask for a new link if you have nothing to confirm with.

When you open a page that needs it, Doozers shows **Confirm it's you first** with a button that
leads to **Confirm it's you**. Choose a way, and Doozers takes you back to the page you came from.
The confirmation lasts 10 minutes and applies only to this login, not to your other browsers.

Confirming with a password counts as a login attempt of yours: the limits on wrong
[passwords](#passwords) apply to it. If you have no password, passkey, or connected provider, ask
for a recovery link. A login through a [trusted proxy](#a-trusted-reverse-proxy) can't confirm, so
it can't change how you sign in.

### Passkeys

A passkey lets you log in with your fingerprint, face, PIN, or screen lock, using your phone,
laptop, password manager, or a security key. Nobody can read it back, and it only works at your
Doozers' address.

Passkeys need an address that browsers trust, so set `public_url` to the `https` address you open
Doozers at, or to `localhost` when you run it on your own machine:

```toml
[channel.web.config]
public_url = "https://doozers.example.com"
```

Without a `public_url`, or with one that starts with `http://` and isn't `localhost`, or that is an
IP address, the page that adds a passkey says why you can't.
**Changing the host of `public_url` breaks every passkey**, because a passkey belongs to that host.
Users then log in with a [password](#passwords) or a recovery link and add their passkeys again.

To add a passkey, open an invite or recovery link and choose **Passkey** on the page that follows,
or use **Add a passkey** on your [account](#ways-to-sign-in). Then use one of the two buttons:

- **Add a passkey** is for nearly everyone. Use it for the passkey on your phone, laptop, or
  password manager, or on a security key that has room for it.
- **Add a non-resident key** is only for a hardware security key with few slots for passkeys. It
  uses no slot, but you type your username before you use it. Older keys (CTAP 2.0) can't delete a
  single passkey without a factory reset that erases everything on the key.

Name each one, so you can tell them apart. You can add as many as you like, such as one per device.

A login that didn't come from a link [confirms it's you](#confirming-its-you) first.

To log in with a passkey, open the login page at your `public_url`. Choose the username field, and
your browser offers your passkeys. You can also press **Sign in with a passkey**. If you added a
non-resident key, type your username and press **Continue with my username** first. Everything else
you can do on the login page, such as a password, is under **Use a password instead**.

Doozers asks for your fingerprint, face, PIN, or screen lock every time, because a passkey alone
isn't enough. A disabled user can't log in with a passkey.

When someone types a username that has no passkeys, Doozers answers with made-up credential ids, so
that a stranger can't tell a real username from the answer alone. That makes guessing usernames
harder, but it doesn't hide which usernames exist from someone who is determined: the made-up ids
don't fully match real ones. If a key's counter goes backwards, which can mean it was copied,
Doozers refuses it and records a warning in the log.

### Identity providers

An identity provider lets your users log in with an account they already have, such as Google,
Authentik, or Keycloak, using OpenID Connect. An admin adds each provider, and each user connects
one account at it. Doozers refuses an account nobody connected, because a provider never creates
users.

Doozers needs the provider's **issuer**, a **client id**, and usually a **client secret**. You get
the client id and secret by registering Doozers at the provider as an application, which the steps
below cover.

To add a provider:

1. Open **Identity providers** in the admin part of the sidebar, then **New identity provider**.
2. Choose an **Id**, such as `google`. It is part of the redirect URI and can't change.
3. Register Doozers at the provider as an application, with this redirect URI and your id in place
   of `<id>`: `https://doozers.example.com/auth/oidc/<id>/callback`. It starts with your
   `public_url`. The page shows the exact address for each provider. The provider gives you a client
   id and usually a client secret.
4. Fill in the name users see, the issuer URL, the client id, and the client secret. Leave the
   secret empty for a client that has none. The default scopes are all Doozers needs.
5. Press **Add**.

Doozers reads the provider's settings from `<issuer>/.well-known/openid-configuration` when you
save, and tells you what is wrong if it can't use the provider. The issuer must be an `https`
address. A provider on your own machine can use `http://localhost`. Doozers keeps the secret in its
secret store and never shows it again. To change it, type a new one when you edit the provider.
Leave the field empty to keep the one stored.

A provider works if it supports PKCE and signs its ID tokens with RSA, ECDSA (P-256 or P-384), or
EdDSA. Microsoft's `common` endpoint doesn't work, because its issuer differs for each tenant. Use
your tenant's issuer instead.

**Disable** hides a provider from the login page and stops sign-ins through it, and **Enable** turns
it back on. **Remove** deletes the provider, every account connected to it, and its secret. Changing
a provider's issuer disconnects its accounts, because an account belongs to an issuer.

Browsers that are already logged in through a provider stay logged in after you disable or remove
it, for up to 30 days. To end a user's logins, disable the user.

To connect an account, a user opens an invite or recovery link, then chooses the provider on the
page that follows, or uses **Connect** on their [account](#ways-to-sign-in). The provider asks them
to log in, and Doozers connects that account to them. Choosing the provider again connects a
different account in place of the first. A login that came from a password, a passkey, or a provider
[confirms it's you](#confirming-its-you) first.

To log in later, the user chooses **Sign in with** the provider's name on the login page. Doozers
matches the account the provider vouches for, never its email address or name. A user an admin
disabled can't log in, and the page says nothing about whether the account was connected.

### A trusted reverse proxy

If a reverse proxy already authenticates people, such as Tailscale Serve, oauth2-proxy, or Authelia,
Doozers can take your identity from a header the proxy sets:

```toml
[channel.web.config.proxy]
identity_header = "Tailscale-User-Login"
```

Doozers reads that header only on a separate listener for the proxy. It ignores the header on
`core.http_listen`, because any program on the machine can connect there, plugins included.

By default the proxy listener is the Unix socket `run/proxy.sock` in the data root. Only the user
Doozers runs as can connect to it, so run the proxy as that user and point it at the socket. If the
data root's path is too long for a socket, Doozers puts it under `$TMPDIR` instead and logs where,
in the line `trusted-proxy listener open`.

The header's value is your identity in the `web` namespace, such as `web:alice@example.com`. Doozers
logs you in as the user who has that identity. A header value that no user has, or one of a disabled
user, gets a "Not allowed" page, and the log shows the value. Identities come from the
[`[owner]` import](configuration.md#owner), so list the identity there before the first start. An
admin can't set a user's proxy identity in the web UI yet, and a user can't add it on the
**Identities** page, because the proxy is the admin's own.

If the proxy can't use a Unix socket, give the listener a TCP address and a shared secret. The proxy
must send the secret in a header with every request:

```toml
[channel.web.config.proxy]
identity_header = "X-Forwarded-User"
listen = "127.0.0.1:8081"
secret = "a long random string"    # at least 16 bytes
secret_header = "X-Doozers-Proxy-Secret"  # the default
```

There's no logout for a proxy login. The proxy decides who you are on every request.

## Chats

The home page lists your chats. Each chat is a separate conversation with its own history. The chat
you open shows beside the list. On a narrow screen it replaces the list, and **All chats** goes
back.

Messages in a chat you open with **New chat** go to the agent your [routes](#routes) pick. To talk
to a particular agent instead, pick it under **Chat with** and press **Start**. That chat always
goes to the agent you picked, and needs no route.

Replies appear as the agent writes them. Image attachments show inline, and other attachments
download. Images from other sites aren't loaded. The model's reasoning isn't shown.

Each tool the agent runs shows in the chat as a box with its name and the first line of its input.
Click the box to see the whole input and output, and click its name to fold it again.

If something goes wrong:

- If sending fails with "too many messages are waiting", try again shortly.
- A message longer than 32 KiB is refused.
- If the page says "Some messages may be missing", reload it.

## Archive and delete chats

Archive a chat to take it off the list without losing it. Open the chat and press **Archive**.

- **Archived chats**, below the list, shows your archived chats. Open one to read it.
- Press **Unarchive** in an archived chat to move it back to your chats. Sending a message in it
  moves it back too.
- Replies from [jobs](#scheduled-jobs) scheduled in an archived chat still go to it, and it stays
  archived.

Delete a chat to remove it for good. Open the chat, press **Delete**, and confirm.

- **Deleting can't be undone.** The chat's messages are removed, and so is the agent's
  [workspace](environments.md) for the chat, with its files.
- What the agent saved to [memory](#memory) from the chat stays.
- Jobs scheduled in the chat keep running, and their next reply starts the chat again.
- While the agent is working in the chat, Doozers refuses to delete it. Wait for the reply, or send
  `/stop` in the chat, then try again.

## Approvals

When an agent asks before running a tool, the chat shows the request with three buttons:

- **Allow**
- **Allow for this session**, so the same tool needs no more approval in this session
- **Deny**

The buttons send [the same replies](commands.md) you could type, and typing them works too.

## Scheduled jobs

**Scheduled jobs** in the sidebar shows your jobs with every agent in one table. It lists the jobs
you scheduled here and the ones an agent scheduled when you asked it. The user `owner` also sees the
[`[[jobs]]`](configuration.md#jobs) of every agent, marked `agent.toml`.

Each row shows the agent, the job, its schedule, and its next and last run, soonest next run first.
Times are in the time zone the job runs in. The next run skips times outside the job's
`active_hours`.

### Scheduling a job

**New job** opens a form:

- **Agent**: the agent that runs the job.
- **Runs**: on a cron schedule, or once.
- **Cron schedule**: for a job that repeats, five fields, such as `0 9 * * 1-5` for 09:00 on
  weekdays.
- **Time**: for a job that runs once. The form says which time zone it's in.
- **Prompt**: what the agent does when the job runs. Its reply is sent to your default identity.
- **Runs in**: its own session, or your DM with the agent on your default identity.

You can have up to 20 jobs with each agent. Doozers refuses a job that would never run, such as one
at a time that has passed.

### Changing a job

Each of your jobs has **Edit**, **Disable**, and **Delete** buttons. A change applies at once,
without a restart.

- **Edit** opens the form, filled in. A job an agent scheduled keeps replying in the conversation
  where you asked for it. To move a job to another agent, delete it and schedule a new one.
- **Disable** keeps the job from running until you press **Enable**. The runs it missed while
  disabled aren't made up.
- **Delete** deletes the job, without asking first.

A job that runs once can't be enabled after its time passed. Edit it to pick a new time, or delete
it.

The `[[jobs]]` from `agent.toml` can't be changed here. Edit `agent.toml` to change them.

## Admin pages

The **Admin** group in the sidebar opens the admin pages. Only admins see it and can open them. A
user is an admin or a member. All admins are equal. The user imported from
[`[owner]`](configuration.md#owner) and the first user `doozers admin login-link` creates are
admins.

`/admin` shows what [`doozers status`](cli.md) shows, including each channel's last status.

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

**Agents** in the sidebar lists every agent, with buttons to chat with it, edit it, or remove it. An
agent's page shows its models, tools, and time zone, and the files in `agents/<agent>/` as plain
text.

**New agent** opens a form that creates one:

- **Name**: the display name. Optional if you give an id.
- **Id**: the agent's directory name, made of letters, digits, and `-`. It fills in from the name as
  you type, and you can edit it. You can't change it once the agent exists.
- **Models**: `<provider>/<model>`, several separated by commas.
- **Environments**: tick the [environments](environments.md) the agent may use for a workspace. The
  form lists the environments in `doozers.toml` with their descriptions, and hides this field when
  there are none. The agent has the environment tools only if you tick an environment and also tick
  the tools below. If you tick one without the other, the form says so under the field. It still
  saves.
- **Tools**: tick the tools the agent may use. An agent can't use a tool you leave unticked, so a
  new agent has none until you tick some. Each [MCP server](#mcp-servers) is one box that allows all
  of its tools. The environment tools (`bash`, `read`, and the rest that say they need an
  environment) work only for an agent with an environment.
- **Other tools**: optional names or globs the boxes can't show, such as `github__create_*`,
  separated by commas or spaces. A box that one of these already allows is ticked and greyed out.
- **Ask first**: optional tool names or globs that need your approval before each call, separated by
  commas or spaces.
- **Time zone**: optional, such as `Europe/London`.
- **Advanced**: a collapsed section of settings most agents leave alone. Leave a field empty for its
  default. The section opens by itself when one of them is set.
  - **Compaction model**: the model that summarizes older messages when a chat gets long.
  - **Max iterations** and **Max iterations for jobs**: how many model requests with tools a chat
    turn or a job turn may make.
  - **Messages during a reply**: **Steer** shows the messages you send while the agent works to it
    at its next tool step. **Queue** keeps them for the next turn.
  - **Memory cap**: the most characters of core memories in each scope.
  - **Approval timeout**: how long a tool call in **Ask first** waits for your approval, such as
    `10m`.
  - **Idle reset**: start a new session when a chat has been quiet this long, such as `12h`.
- **SOUL.md**: who the agent is.

The [`agent.toml` reference](configuration.md#agentsagentagenttoml) lists each setting's default.

Doozers checks the agent as it checks a hand-written `agent.toml`. If something is wrong, such as a
model naming a provider you haven't configured, the form shows the problem and nothing is written.

Otherwise Doozers writes `agents/<id>/agent.toml` and `SOUL.md`. You can chat with the agent at
once, without a restart.

The form has every agent setting except [`[[jobs]]`](configuration.md#jobs). To change those, edit
`agent.toml` by hand. Like any other configuration change, that takes effect at the next restart.

### Editing an agent

**Edit** opens the same form, filled in from the agent's files. The **Id** can't be changed. To use
another id, create a new agent and remove the old one.

Saving takes effect without a restart:

- New chats and the next run of the agent's [jobs](configuration.md#jobs) use the new settings. A
  chat already going keeps the settings it started with. Use `/new` to start a fresh one.
- Doozers changes only what the form has. A field you empty removes its setting from `agent.toml`,
  so the default applies. The rest of the file, comments and `[[jobs]]` included, stays as you wrote
  it.
- If you edit `agent.toml` or `SOUL.md` by hand while the form is open, saving is refused with
  "changed on disk". Use the form's link to reload the current values.

An environment that `agent.toml` names but `doozers.toml` doesn't configure stays ticked on the
form, marked as not configured. Saving is refused until you untick it.

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

To bring the agent back, move its directory back to `agents/<id>/` and restart Doozers. Doozers
never empties or backs up `trash/`, so delete what you don't want yourself.

You can reuse the id at once. A new agent with the same id takes over the old agent's chats and
answers them with its own settings. To start clean, pick another id.

Doozers refuses to remove an agent in two cases:

- **Routes still name it.** The page lists them. Point each at another agent or remove it, then try
  again.
- **It is working right now.** A chat turn or one of its jobs is running, or messages are waiting.
  Stop the chat with `/stop` or wait, then try again.

### Memory

**Memory** on an agent's page lets you search, add, edit, and forget the agent's memories.
[How memory works](skills-and-memory.md#memory) is explained on its own page.

You see your own private memories and the shared ones, newest change first. Other users' private
memories are never shown, not even to an admin. The page lists the newest 200, and search finds the
rest. Search works like the agent's `memory_recall` tool.

- **Add** a memory with its text. Tick **Core** to load it into every prompt that can see it. Other
  memories are only found by search.
- **Shared** makes a memory visible to every user and group of the agent. Only an admin sees this
  box, and only an admin can change or forget a shared memory.
- **Edit** changes a memory's text and flags.
- **Forget** deletes a memory, without asking first.

A memory is at most 1,000 characters. Your private core memories together, and the shared core
memories together, must fit in the agent's `memory_cap`. If a change would go over, nothing is
saved. The page says by how much, so you can shorten or forget some, or untick **Core**.

If a memory changed after the page loaded, for example because the agent edited it, your edit is
refused. The page shows what you typed and what the memory says now, so you can redo it.

A change to a core memory reaches a running chat after it compacts, or in a new chat or job run.
`memory_recall` finds the change at once.

Below your own memories, an admin sees a section for each group conversation that has private
memories. Nobody else can see or change a group's memories.

## Routes

**Routes** in the sidebar decides which agent answers each conversation, on every channel. A message
in a conversation that no route matches is dropped. A fresh Doozers has no routes, and answers
nobody until you add one.

Routes are tried from the top, and the first match wins. Put a route for one conversation before a
broader one for a whole channel or everything.

**New route** opens a form:

- **Channel**: one of your channel instances, or any.
- **Kind**: direct, group, or any. Doozers ignores group conversations for now.
- **Conversation**: the conversation id as the channel reports it, such as `#giles` on IRC, or empty
  for any. It must match exactly, capitals included.
- **Agent**: required.
- **Position**: at the end, or before a route you pick.

A route that sets nothing but the agent matches every conversation, so keep it last.

Each route in the list has **Edit**, **Up**, **Down**, and **Remove** buttons. A change applies to
the next message, without a restart.

Doozers refuses a change that would leave a route unreachable, such as a duplicate, or a broad route
moved above a specific one. The form says what is wrong. To move a broad route up, move the specific
routes it would hide above it instead.

A route that stopped working, for example because its agent was removed by hand, stays in the list
with a warning under it.

Doozers imports `[[routes]]` from an older `doozers.toml` once. After that you can delete them from
the file. See [Configuration](configuration.md#routes).

## MCP servers

**MCP servers** in the sidebar lists the MCP servers that give your agents more tools. Each row
shows how the server runs, its state, how many tools it offers, and its last error. A server's page
shows its settings and the name and description of each tool, and has a **Restart** button.

Here you add, edit, rename, and remove remote servers, and the change applies at once, without a
restart. A conversation that is already going keeps the tools it started with. New conversations get
the new list.

A server that Doozers runs on this machine, a stdio server, is a `[toolset.<name>]` table in
`doozers.toml`, which [Configuration](configuration.md#toolsets) describes. It shows in the list
too, but you can only change it in the file. Doozers ignores a `[toolset.<name>]` table with `url`
and logs a warning at every start. Add that server here instead.

### Adding a remote server

A remote server is one you reach over the network. Doozers doesn't run or sandbox it.

1. Open **New MCP server** and enter a **Name**, such as `github`. Its tools are named
   `github__<tool>`.
2. Enter the **URL**, such as `https://api.githubcopilot.com/mcp/`.
3. For **Authentication**, choose **Bearer token** and paste the token.
4. Select **Add**.

The server's page opens and shows its state and its tools. If Doozers can't connect, the page shows
why.

Doozers keeps the token encrypted and never shows it again. When you edit the server, leave the
token field blank to keep the stored token, or type a new one to replace it. If you change the URL
to another server, type the token again. Doozers never sends a stored token to a server it wasn't
given for.

### Adding a server that each user signs in to

Some servers act as the person using them, such as one that reads your GitHub repositories. For
those, choose **OAuth: each user signs in** under **Authentication**. Each user then signs in with
their own account on the [Connections](#connections) page, and an agent's calls to the server use
the sign-in of the person who asked.

1. If the server needs a registered OAuth app, register one with the server's provider. Use the
   **Sign-in callback** URL from the server's page in Doozers as the redirect URI. It's
   `<your Doozers address>/connections/<name>/callback`, such as
   `http://127.0.0.1:8080/connections/github/callback`. Renaming the server changes it.
2. Open **New MCP server**, and enter the **Name** and **URL**.
3. For **Authentication**, choose **OAuth: each user signs in**.
4. Enter the app's **OAuth client ID** and **OAuth client secret**. Leave both blank if the server
   registers clients itself.
5. Optionally, enter **OAuth scopes**, separated by spaces.
6. Select **Add**, then sign in on **Connections**.

The server shows as failed until someone signs in. Once you sign in, your conversations get the
tools your own account offers. If you reach Doozers through a domain name or a reverse proxy, set
[`public_url`](#invite-and-recovery-links) so the callback URL is the one your browser can open.

The form has more settings, all optional:

- **Allow insecure connections** lets Doozers reach another machine over plain `http://`. It shows
  when the URL starts with `http:`. Doozers accepts plain `http://` to this machine without it. Tick
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
**Remove MCP server**. Doozers disconnects from the server and deletes its settings. The page lists
the agents whose tools or ask-first list name the server's tools. Doozers leaves those agents as
they are.

### Connections

**Connections** lists the MCP servers that ask you to sign in with your own account. Select
**Sign in** to go to the server's sign-in page. Once you approve, the server sends you back to
Doozers, and the page shows you as signed in.

- Doozers keeps your sign-in encrypted and refreshes it as needed. You don't have to sign in again
  unless the server revokes it.
- An agent's call to the server on your behalf uses your sign-in. Someone who hasn't signed in gets
  an error in the conversation telling them to sign in, and so does a job you didn't start.
- Select **Sign out** to delete your sign-in from Doozers. To revoke Doozers' access completely,
  also remove it in your account settings on the server's website.

### Renaming an MCP server

The name is the prefix of the server's tools, and agents name those tools in their tools and
ask-first lists, so a rename changes more than the name. To rename an MCP server:

1. Open the server's page and select **Rename**.
2. Enter the **New name** and select **Show the changes**.
3. Check the list of agents. Doozers rewrites the entries in each agent's tools and ask-first lists
   that start with `<old name>__` to start with `<new name>__`.
4. Select **Rename to** `<new name>`.

Doozers disconnects from the server, rewrites the agents, and connects again under the new name. If
a step fails, it undoes the steps before it, and the page says which step failed and why. A
conversation that is already going keeps the old tool names, and its calls to them fail. Start a new
conversation to use the new names.

Doozers refuses the rename, and changes nothing, if the new name is taken, or if an agent has a glob
that matches the server's tools without starting with `<old name>__`, such as `github*`. The page
lists those globs. Rewrite them, for example to `github__*`, and try again.

You can't rename a server that you configure in `doozers.toml` here. Stop Doozers and run
[`doozers toolset rename`](cli.md#rename-a-toolset) for that.

## SSH keys

**SSH keys** in the sidebar lists your own SSH keys, with the forms to generate or upload one, a
button to copy each public key, and **Rename** and **Delete**. An admin also has **SSH keys** under
**Admin**, which lists every user's keys. [SSH keys](ssh.md) covers the page and how to add a public
key to GitHub.

## Account

**Account** in the sidebar is where you manage yourself. It has your profile, the ways you sign in,
and a summary of your channel identities.

Your profile is your display name and time zone.

The display name is what Doozers shows for you, such as at the bottom of the sidebar.

The time zone sets the times agents see in your messages and when your
[scheduled jobs](#scheduled-jobs) run. Use an IANA name, such as `America/Los_Angeles`. Leave it
empty to use the agent's time zone, or else the server's.

The page also shows your username, which you log in with, and your role. You can't change them here.

### Ways to sign in

**Ways to sign in** lists your [password](#passwords), your [passkeys](#passkeys), and the
[identity providers](#identity-providers) you connected an account at.

- **Set a password** or **Change your password** opens the page that sets it.
- **Add a passkey** opens the page with the two buttons. Each passkey shows when you added it and
  last used it, and **Rename** changes its name.
- **Connect** sends you to the provider to log in, and **Connect another account** replaces the
  account you connected.
- **Remove** and **Disconnect** ask first, then remove that way to sign in.

A connection says when an admin turned its provider off, or changed the provider so that the account
no longer signs you in. Connect it again after the change.

Doozers won't remove the last way you can sign in. Add another way first, then remove the one you
don't want. A passkey counts only where [passkeys work](#passkeys), and a connection counts only
while an admin has the provider turned on. A login through a
[trusted proxy](#a-trusted-reverse-proxy) doesn't count as a way to sign in, because an admin sets
it up.

You can add or remove a way to sign in, or change your password, only after you
[confirm it's you](#confirming-its-you), which the page asks for with **Confirm it's you**. A login
that came from an invite or recovery link doesn't need to for its first 30 minutes. Renaming a
passkey doesn't need it.

**Manage identities** opens the **Identities** page.

## Identities

The **Identities** page, from **Account** in the sidebar, lists your channel identities and adds new
ones. An identity is your account on a channel, such as `whatsapp:+15551234567` or
`irc:libera.chat:account:stephen`. Doozers answers messages from your identities as you, and drops
messages from every other account.

For each identity the page shows when you added it and how: **Linked by you** with a code,
**Set by an admin**, or **Imported from doozers.toml**. It marks your default identity, which is
where Doozers sends what you didn't start, such as a scheduled job's reply or an approval it needs.
**Make default** changes it. **Remove** asks first, and then Doozers drops messages from that
account. If you remove the default, your oldest identity becomes the default. You can't remove your
web sign-in here.

### Add an identity

1. Choose the channel the account is on under **Add an identity**, such as `whatsapp`, `discord`,
   `email`, or `irc:libera.chat`, and choose **Make a code**. The list has only the channels that
   are running.
2. The page shows a message such as `/link ABCD2345`. Send it to Doozers in a direct message, from
   the account you want to add, within 10 minutes. Each channel's guide says where to send it:
   [WhatsApp](whatsapp.md#identities), [Discord](discord.md#add-your-identity),
   [email](email.md#adding-an-instance), or [IRC](irc.md#identities).
3. Doozers replies on the channel, and the identity is on the page. Messages from it are yours from
   then on.

Doozers shows the code only once, and it works once. Making another code for the same channel
replaces the first. If you lose it or it runs out of time, make a new one.

Doozers takes the identity from the message you send, so you don't type it. A channel that is not in
the list isn't running. An admin can see why on the [status page](#admin-pages).

### When a code doesn't work

The **Codes** list shows each code you made that wasn't used, as **Pending** until a time,
**Expired**, or **Cancelled**. Make a new code for an expired or cancelled one.

Doozers answers a wrong code the same way whether or not you have a code, and counts it:

- After five wrong codes from one account, Doozers says it locked `/link` for that account and then
  ignores the account's `/link` for an hour.
- A code is cancelled after 20 wrong codes from anyone while it is pending.
- A code works only for the channel you chose, and only for 10 minutes.

An account that belongs to a user already can't be linked again, to the same user or another.
Doozers says so. To move an account to another user, remove it from the first user's **Identities**
page, then link it again with the other user's code. `/link` works only in direct conversations, not
in a group, and only from a sender the channel has verified.

## Known SSH hosts

**Known SSH hosts** under **Admin** lists the SSH servers whose host keys the workspaces trust, for
each environment, with each key's type and fingerprint, when and by whom it was first recorded, and
when it was last used. **Delete** forgets a key after a confirmation, and **Add** trusts a key you
paste before any workspace connects. [SSH keys](ssh.md#known-ssh-hosts) covers what a host's first
connection does and when to delete or add a key. Only an admin can open the page.

## Workspaces

**Workspaces** in the sidebar lists the [workspaces](environments.md) your agents have, so you can
see what they use and stop or delete one. Only an admin can open it.

Each workspace shows:

- **Environment** and **Owner**: the agent and the conversation, which links to the web chat when it
  is one, and the user whose conversation it is (two users who held one IRC nickname at different
  times have workspaces of their own), or the job run.
- **Created** and **Last used**.
- **In use**: whether a running turn is using it now.
- **SSH keys**: the user whose [SSH keys](ssh.md#use-ssh-in-a-workspace) the workspace gets, or
  `none`.
- The state (running or stopped), **Disk used**, and the environment's **Description**,
  **Working directory**, and **Details**. For the [Docker environment](docker-environment.md) the
  details are the image, its digest, the container, the volume, and the limits.

The environments at the top show the free space where their workspaces live. If an environment isn't
connected or fails when the page asks it, the page names it, and what comes from it shows as
unknown. Reload the page to ask again.

**Stop** stops the workspace and keeps its files. The agent's next command starts it again and tells
the agent that it was stopped. If a turn is using the workspace, Doozers asks first, because
stopping ends the command the turn is running and its background processes.

**Delete** opens a page that says what deleting does. Press **Delete workspace** to remove the
workspace and everything in it. If a turn is using the workspace, the page says so: deleting stops
the turn's command and then deletes the workspace. The agent's next workspace call says that an
operator deleted the workspace, and the agent can create another.
