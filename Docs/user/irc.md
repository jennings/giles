# IRC

The IRC channel plugin, `giles-channel-irc`, lets you talk to the agent on an IRC network.

Each instance of component `irc` connects to one network. For two networks, add two instances.

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

Talk to the agent in a private message (a query) to its nick. Giles only accepts messages from a
nick that's logged in to a services account listed in `[owner] identities`. See
[Identities](#identities) for how to write them.

## Settings

These keys go in the instance's `config` table.

| Key                | Default      | Meaning                                                                |
| ------------------ | ------------ | ---------------------------------------------------------------------- |
| `server`           | required     | Host name or IP address, optionally with `:<port>` (`[<IPv6>]:<port>`) |
| `port`             | 6697 or 6667 | The port, unless `server` has one: 6697 with TLS, 6667 without         |
| `tls`              | `true`       | Connect with [TLS](#tls)                                               |
| `insecure`         | `false`      | Allow `tls = false` to a server that isn't on this machine             |
| `network`          | required     | The network's name, used in identities as `irc:<network>:...`          |
| `nick`             | required     | The nick to use                                                        |
| `username`         | the nick     | The user name (ident)                                                  |
| `realname`         | `Giles`      | The real name shown in `WHOIS`                                         |
| `password`         | none         | Server password (`PASS`), for example for a bouncer. A secret.         |
| `sasl`             | none         | Log in to a services account with [SASL](#sasl)                        |
| `client_cert`      | none         | A PEM client certificate (chain), for example for CertFP. A secret.    |
| `client_key`       | none         | The PEM private key of `client_cert`. A secret.                        |
| `mask_identities`  | `false`      | Accept [mask identities](#mask-identities)                             |
| `channels`         | `[]`         | Channels to join, such as `["#giles"]`                                 |
| `max_lines`        | 30           | The most lines one message may take. The rest is replaced by a note.   |
| `max_line_bytes`   | none         | A lower limit on the bytes of one line, at least 16                    |
| `send_burst`       | 5            | Lines sent at once before the [rate limit](#rate-limit) applies        |
| `send_interval_ms` | 1000         | Milliseconds between lines after the burst. 0 turns the limit off.     |

An unknown key or invalid value stops the instance from starting. The log and `giles status` name
the key, for example `invalid config: nick: not a valid IRC nick`.

`network` names the network in identities. It isn't taken from `server`, so you can change servers
within a network and keep your identities. It can't contain `:` or spaces.

Put passwords and keys in [secrets](configuration.md#secrets), such as `{ secret = "libera" }`,
rather than writing them into `giles.toml`.

### TLS

TLS is on by default. Giles trusts only the Mozilla root certificates built into it, not the
system's, so it refuses a server with a self-signed certificate.

You can set `tls = false` for a server on this machine (`localhost`, `127.0.0.0/8`, or `::1`), such
as a local bouncer. Plain TCP to any other server also needs `insecure = true`.
**Anyone on the network path could then inject messages and forge account identities.**

Giles never sends passwords (`password` or SASL PLAIN) over plain TCP to another machine, even with
`insecure`.

### SASL

`sasl = { password = { secret = "libera" } }` logs in with SASL PLAIN. The account is
`sasl.username`, which defaults to the nick.

`sasl = { mechanism = "external" }` logs in with the client certificate in `client_cert` and
`client_key`. Register the certificate with the network's services first, for example with NickServ
`CERT ADD`.

With `sasl` set, Giles registers only after it logs in. If the server doesn't offer SASL or the
mechanism, or the login fails, Giles drops the connection and retries later. `giles status` shows
the reason.

### Rate limit

Giles sends up to `send_burst` lines at once, then one line every `send_interval_ms`. This keeps a
long reply from getting it disconnected for flooding.

The defaults suit large networks like Libera.Chat. On a private server or bouncer, you can set
`send_interval_ms = 0`.

## Identities

List your services account as `irc:<network>:account:<account>`, such as
`irc:libera.chat:account:stephen`. Giles drops messages from a sender who isn't logged in.

The server tells Giles the sender's account with the IRCv3 `account-tag` capability. Libera.Chat and
other networks running modern servers have it.

Write account names and nicks the way the server case-folds them, or they never match:

- Uppercase letters become lowercase on every network.
- With the default `rfc1459` case mapping, which Libera.Chat uses, `[`, `]`, `\`, and `~` also
  become `{`, `}`, `|`, and `^`. Account `Foo[m]` is `irc:libera.chat:account:foo{m}`, and nick
  `Bob~` is `bob^`.
- `rfc1459-strict` folds `[]\` but not `~`.
- `ascii` folds only letters.
- Networks that allow non-ASCII nicks use other mappings, which also lowercase those letters.

A dropped message is logged (`dropped inbound message`) with the sender's identity in exactly the
form to list.

### Mask identities

On a network without services accounts or account tags, `mask_identities = true` makes senders'
masks count as verified.

A mask identity is `mask:<nick>!<user>@<host>`. The nick is case-folded as above, the host is
lowercase, and the user name is kept as is. For example, `Stephen!~sj@203.0.113.7` is
`irc:example:mask:stephen!~sj@203.0.113.7`.

If the server also tags the message with an account, the account stays the identity and the mask is
an alias. You can list either in `[owner] identities`.

**Masks are risky.** A mask is only as trustworthy as its parts:

- Anyone can use any free nick, and a `~` before the user name means the server couldn't verify it.
  A mask really rests on its host.
- A home connection's address may change, or pass to someone else.
- A shared host, such as a shell server, a VPN, or a university, has other users who can take the
  same nick and user name.

Use them only with a host nobody else can have, such as a network cloak tied to your account, on a
network you trust to report hosts honestly.

## Conversations

- A private message to the agent's nick is a direct conversation with that nick. Each nick has its
  own session, and replies go back to the nick. A different nick is a different conversation, even
  for the same account.
- A message in a channel is a group conversation, named after the channel in lowercase, such as
  `#giles`. Giles v1 doesn't answer group conversations, so `channels` only makes the agent present
  there.

Messages Giles sends on its own, such as job results, approval prompts, and login codes, go to an
identity. For an `account:` identity, Giles asks the server which nick is logged in to the account
right now, and sends only to that nick. If the account isn't online, the message waits and is
retried until it expires. It never goes to whoever has taken a nick you used before.

A `mask:` identity gets the message only if its nick still has that user name and host.

## Output

Replies are plain text with IRC bold and italics. A reply longer than `max_lines` lines is cut
there, with a last line like `[12 more lines not sent]`.

Attachments can't be sent over IRC. Each becomes a line `[attachment not sent: <name>]`.

## Connection

`giles status` shows `connected`, or the last error, such as `SASL authentication failed: ...` or
`connecting to irc.example.net:6697: ...`.

Giles reconnects after a lost connection. If the nick is taken while connecting, Giles tries
`nick_`, `nick__`, and `nick___`.

## Sandboxing

Channel plugins may use the network by default. To allow only the IRC port, give the instance the
[sandbox](configuration.md#sandbox) grant `sandbox = { network = ["tcp:6697"] }`.
