# IRC

The IRC channel is the plugin `giles-channel-irc`. Each instance connects to one IRC network, so two
networks are two instances of component `irc`:

```toml
[owner]
identities = ["irc:libera.chat:account:stephen"]

[channel.libera]
component = "irc"

[channel.libera.config]
server = "irc.libera.chat"
network = "libera.chat"
nick = "jeeves"
sasl = { password = { secret = "libera" } }
```

Talk to the agent in a private message (a query) to its nick. Messages count only from a nick logged
in to a services account that `[owner] identities` lists ([Identities](#identities)).

## Settings

The `config` table of the instance. An unknown key is an error, and an invalid value stops the
instance from starting with a message naming the key, e.g.
`invalid config: nick: not a valid IRC nick` in the log and in `giles status`.

| Key                | Default        | Meaning                                                                    |
| ------------------ | -------------- | -------------------------------------------------------------------------- |
| `server`           | required       | Host name or IP address, optionally with `:<port>` (`[<IPv6>]:<port>`)     |
| `port`             | 6697 or 6667   | The port, unless `server` has one: 6697 with TLS, 6667 without             |
| `tls`              | `true`         | Connect with TLS                                                           |
| `insecure`         | `false`        | Allow `tls = false` to a server that isn't on this machine                 |
| `network`          | required       | The network's name: identities are `irc:<network>:...`                    |
| `nick`             | required       | The nick to use                                                            |
| `username`         | the nick       | The user name (ident)                                                      |
| `realname`         | `Giles`        | The real name shown in `WHOIS`                                             |
| `password`         | none           | Server password (`PASS`), e.g. for a bouncer; a secret                     |
| `sasl`             | none           | Log in to a services account ([SASL](#sasl))                               |
| `client_cert`      | none           | A PEM client certificate (chain) to present, e.g. for CertFP; a secret     |
| `client_key`       | none           | The PEM private key of `client_cert`; a secret                             |
| `mask_identities`  | `false`        | Trust `mask:` identities ([Mask Identities](#mask-identities))             |
| `channels`         | `[]`           | Channels to join, e.g. `["#giles"]`                                        |
| `max_lines`        | 30             | The most lines one message may take; the rest is replaced by a note        |
| `max_line_bytes`   | none           | A lower limit on the bytes of one line (at least 16)                       |
| `send_burst`       | 5              | Lines sent at once before the rate limit applies                           |
| `send_interval_ms` | 1000           | Milliseconds between lines after the burst; 0 for no limit                 |

`network` is the identity namespace, not taken from `server`, so changing servers within a network
keeps your identities. It can't contain `:` or spaces.

Use secret references (`{ secret = "libera" }`, see [Secrets](configuration.md#secrets)) for
passwords and keys rather than writing them into `giles.toml`.

### TLS

TLS is on by default and trusts the Mozilla root certificates built into Giles, not the system's, so
a server with a self-signed certificate is refused. `tls = false` is allowed without more ado only
for a server on this machine (`localhost`, `127.0.0.0/8`, `::1`), such as a local bouncer. Plain TCP
to anywhere else needs `insecure = true`: anyone on the network path could then inject messages and
forge account identities. Passwords (`password`, SASL PLAIN) are never sent over plain TCP to
another machine, even with `insecure`.

### SASL

`sasl = { password = { secret = "libera" } }` logs in with SASL PLAIN as the account named by
`sasl.username` (default: the nick). `sasl = { mechanism = "external" }` logs in with the client
certificate (`client_cert` and `client_key`, which must be registered with the network's services,
e.g. NickServ `CERT ADD`).

With `sasl` set, Giles registers only after logging in: if the server doesn't offer SASL or the
mechanism, or the login fails, the connection is dropped and retried later, and `giles status` shows
the reason.

### Rate Limit

Giles sends `send_burst` lines at once, then one line every `send_interval_ms`, across everything it
sends on the connection, so a long reply doesn't get it disconnected for flooding. A reply waits for
its lines to go out. The defaults suit large networks like Libera.Chat; a private server or bouncer
can use `send_interval_ms = 0`.

## Identities

Identities on an instance are in the namespace `irc:<network>`. Giles knows who sent a message only
from the server:

- **`account:<account>`**: the services account the sender is logged in to, which the server tags
  each message with (the IRCv3 `account-tag` capability, which Libera.Chat and other networks
  running modern servers have), e.g. `irc:libera.chat:account:stephen`. This is the identity to use:
  nobody else can log in to your account.
- A sender who isn't logged in is unverified, and Giles drops their messages before they reach an
  agent (they're logged).

Account names and nicks in identities (and in conversation ids) are case-folded the way the server
compares them, so write them folded in `[owner] identities`, or they never match:

- Uppercase letters become lowercase on every network.
- On networks using the default `rfc1459` case mapping, which includes Libera.Chat, `[`, `]`, `\`,
  and `~` also become `{`, `}`, `|`, and `^`: account `Foo[m]` is `irc:libera.chat:account:foo{m}`,
  and nick `Bob~` is `bob^`.
- `rfc1459-strict` folds `[]\` but not `~`; `ascii` folds only letters. Networks that allow
  non-ASCII nicks (other mappings) also lowercase those letters.

A dropped message is logged (`dropped inbound message`) with the sender's identity in exactly the
form to list.

### Mask Identities

On a network without services accounts or account tags, `mask_identities = true` makes senders'
masks count as verified: `mask:<nick>!<user>@<host>`, with the nick case-folded as above and the
host in lowercase (the user name is kept as is), e.g. `irc:example:mask:stephen!~sj@203.0.113.7` for
`Stephen!~sj@203.0.113.7`, or `mask:bob{away}!bob@host.example` for `Bob[away]` on an `rfc1459`
network. With an account tag too, the account stays the identity and the mask is an alias, so either
may be listed in `[owner] identities`.

A mask is only as trustworthy as its parts, and is risky:

- Anyone can use any free nick, and a `~` before the user name means the server couldn't verify it,
  so a mask really rests on its host.
- A home connection's address may change, or pass to someone else; a shared host (a shell server, a
  VPN, a university) has other users who can take the same nick and user name.
- Use it only with a host nobody else can have, such as a network cloak tied to your account, and on
  a network you trust to report hosts honestly.

## Conversations

- A private message to the agent's nick is a direct conversation with that nick: each nick has its
  own session, and replies go back to the nick. A different nick is a different conversation, even
  for the same account.
- A message in a channel is a group conversation, `#channel` (lowercase). Giles v1 doesn't answer
  group conversations: such messages are dropped, so `channels` only makes the agent present there.
- Notices and CTCP requests (`VERSION`, `PING`, ...) are ignored; `/me` actions arrive as
  `/me waves`. Formatting and colors are removed from incoming text.
- The agent's own messages, which servers with `echo-message` send back, are ignored.

Messages Giles sends on its own (job results, approval prompts, login codes) go to an identity. For
`account:<account>`, Giles asks the server (`WHOIS`) which nick is logged in to the account right
now, trying the nick it last saw on the account and then the account name, and sends only to a nick
the server confirms. If the account isn't online, the message waits and is retried until it expires,
so it never goes to whoever has taken a nick you used before. A `mask:` identity gets the message
only if its nick still has that user name and host.

## Output

Replies are plain text with IRC bold and italics. Code blocks are sent as plain lines, lists with
`-` and `1.`, and links as `text <url>`. A line that's too long for IRC is split at a space; the
limit accounts for the agent's own nick and user name and assumes the longest host name, so the
server never cuts a line off. A reply longer than `max_lines` lines is cut there, with a last line
like `[12 more lines not sent]`. Attachments can't be sent over IRC; each becomes a line
`[attachment not sent: <name>]`.

## Connection

The plugin connects when Giles starts and reconnects after a lost connection, waiting 1 second at
first and doubling up to 5 minutes (back to 1 second once a connection has lasted a minute). A
server that's down when Giles starts doesn't stop the instance. `giles status` shows `connected`, or
the last error, e.g. `SASL authentication failed: ...` or `connecting to irc.example.net:6697: ...`.
A reply written while the connection is down waits up to 30 seconds for it to come back.

If the nick is taken while connecting, Giles tries `nick_`, `nick__`, and `nick___`. Giles pings a
server that has been quiet for 2 minutes and reconnects if it doesn't answer within another minute.

Channel plugins may use the network by default. To allow only the IRC port, grant
`sandbox = { network = ["tcp:6697"] }` ([Sandbox](configuration.md#sandbox)).
