# SSH keys and the agent relay

Code: `crates/doozers-core/src/ssh/` (`doozers_core::ssh`), the rows in `doozers-store`
([Storage](storage.md#ssh-keys)), and the pages in `channels/web/ssh_keys.rs`
([Web channel](web-channel.md#ssh-keys-pages)). Design: the decisions on
[#135 SSH agent relay for environment workspaces](https://github.com/jennings/doozers/issues/135),
[#136 Per-user SSH keys](https://github.com/jennings/doozers/issues/136),
[#137 ssh-agent relay](https://github.com/jennings/doozers/issues/137), and
[#138 Known SSH hosts](https://github.com/jennings/doozers/issues/138).

Each user keeps SSH keys in Doozers. The private keys never leave the core: no page, command, or RPC
returns one. This page covers how the keys are stored, generated, read from uploads, and protected,
how a workspace's `ssh` and `git` use them through a relay without ever holding one, and how `ssh`
there decides which server host keys to trust. The [user page](../user/ssh.md) is the guide.

## Storage

A key is a row of `ssh_keys` and a secret.

- **The row** holds the metadata: `id`, `user_id`, `label`, `algorithm`, `public_key`,
  `fingerprint`, `created_at`, and `last_used_at`. `algorithm` is the SSH name of the public key
  (`ssh-ed25519`, `ssh-rsa`, `ecdsa-sha2-nistp256`, `ecdsa-sha2-nistp384`, or
  `ecdsa-sha2-nistp521`). `public_key` is `<algorithm> <base64>` and `fingerprint` is the
  `SHA256:...` form that `ssh-keygen -l` prints. `last_used_at` is null until the key first signs.
- **The secret** is at `users/<user>/ssh-keys/<id>`, in the [secret store](secret-store.md). Its
  value is the private key as unencrypted OpenSSH PEM text with an empty comment. The secret store
  seals it like every other value, so the data key and its key file are the only protection that
  matters, and an uploaded key's own passphrase isn't kept.

`ssh_keys` is the only writer of `users/<user>/ssh-keys/`, which follows the secret store's rule
that a feature owns its paths. The `id` is `AUTOINCREMENT`, so an id is never reused and a new key
never meets the sealed value of an old one.

A user can't hold the same key twice (`UNIQUE (user_id, fingerprint)`), but several users can. Each
upload is its own row and its own secret.

**Order of writes.** Adding a key inserts the row first, so its id names the secret, then writes the
secret. If the secret can't be written, the row is deleted again. Deleting a key deletes the row,
then the secret. A secret that can't be deleted is logged at `warn` and the delete still succeeds: a
sealed value nothing refers to is harmless, while a row without a secret would be a key that is
listed and can't sign.

**Backups.** [Backups](backups.md) include the table, since they take `doozers.db` whole, and the
private keys are in them only as the secret store's ciphertext. Restoring them needs the same
`secret.key`.

## Generating keys

`ssh::KeyType` lists what Doozers generates: Ed25519 (the default), RSA with 3072 or 4096 bits, and
ECDSA on P-256, P-384, or P-521. Keys come from the operating system's randomness through the
`ssh-key` crate. RSA generation takes seconds, so the service runs it with `spawn_blocking`. The
debug profile optimizes the RSA crates (`Cargo.toml`), or the tests would take minutes.

DSA and FIDO security keys are not supported. OpenSSH 10.0 removed DSA, and a security key's private
half stays on hardware Doozers doesn't have.

## Reading uploads

`ssh::formats::parse_private_key` turns the text of an upload into an unencrypted
`ssh_key::PrivateKey`, or a `KeyFileError` whose message says what to do. It reads at most 64 KiB.
It turns a browser's CRLF line breaks into LF and ignores whitespace around the key.

| Format | Header | Read by |
| ------ | ------ | ------- |
| OpenSSH, plain or encrypted (bcrypt-pbkdf with any of `ssh-key`'s ciphers) | `OPENSSH PRIVATE KEY` | `ssh-key` |
| PKCS#1 RSA | `RSA PRIVATE KEY` | `rsa` |
| SEC1 EC | `EC PRIVATE KEY` | the structure by hand, then `p256`, `p384`, or `p521` |
| PKCS#8, plain | `PRIVATE KEY` | `pkcs8`, then `rsa`, `p256`/`p384`/`p521`, or `ed25519-dalek` |
| PKCS#8, encrypted | `ENCRYPTED PRIVATE KEY` | `pkcs8` with PBES2: PBKDF2 (SHA-1 or SHA-2) or scrypt, with AES-CBC |
| Legacy OpenSSL encryption of PKCS#1 and SEC1 | `Proc-Type: 4,ENCRYPTED` | `ssh::formats` |

The legacy encryption is what `ssh-keygen -m PEM -N <passphrase>` writes. The key is OpenSSL's
`EVP_BytesToKey` with MD5, one iteration, and the first 8 bytes of the IV as the salt. Doozers
supports `AES-128-CBC`, `AES-192-CBC`, and `AES-256-CBC`, and refuses any other `DEK-Info` cipher
with a message that says to convert the key with `ssh-keygen -p -f <file>`.

- **SEC1 and explicit parameters.** LibreSSL's `ssh-keygen -m PEM`, the one on macOS, writes the
  curve's full parameters where OpenSSL writes its name, and the `sec1` crate can't read that. So
  `ssh::formats` reads the key's scalar and its named curve itself. For a key without a name, the
  scalar's size picks the curve, and the key is accepted only if its public point is the one that
  scalar gives on that curve. A scalar from another 32-byte curve, such as secp256k1, would not
  match.
- **Passphrases.** A passphrase given for an unencrypted key is ignored. An encrypted key without
  one is refused and the message asks for it. A wrong passphrase is refused with "the passphrase is
  wrong". Decrypting with a wrong key sometimes gets past the padding check, so garbage that then
  fails to parse in an encrypted upload also reads as a wrong passphrase.
- **Key-derivation cost.** The encryption settings come from the uploaded file, so they are capped
  before anything is derived. An encrypted PKCS#8 key is refused if its scrypt settings have `N`
  over 2^20, `r` over 8, or `p` over 1, or its PBKDF2 iteration count is over 10,000,000. An OpenSSH
  key is refused if its bcrypt-pbkdf `rounds` is over 1024 (`ssh-keygen -a` defaults to 16). The
  message says to re-encrypt the key with `ssh-keygen -p -f <file>`. `SshKeys::upload` also parses
  on a blocking thread, as `generate` does, because decrypting is slow on purpose.
- **Refused.** PuTTY files (`PuTTY-User-Key-File-`) with the
  `puttygen key.ppk -O private-openssh -o key` command, DSA keys (in any format), `sk-*` keys,
  public keys, and any other algorithm or curve (Ed448, secp256k1) with the list of what is
  supported.

## Permissions

`SshKeys` is the service. It is held by `Runtime::ssh_keys` over the `Store`, the
`Arc<dyn SecretProvider>`, the clock, and the `UserList`. Every call takes the user who acts.

| Call | Who |
| ---- | --- |
| `list`, `get`, `generate`, `upload`, `rename`, `delete` | The acting user, on their own keys only. A key of someone else's is `NotFound`, the same as a key that doesn't exist. |
| `all_keys`, `any_key`, `delete_any` | An active admin only (`SshKeys::may_administer`, which asks the shared `UserList` at each call, as the web `may_administer` asks the role of the request). Anyone else, a demoted or disabled admin included, gets `Forbidden`. |

An admin can see any user's key details and delete a key. An admin can't use another user's key:
`private_key` takes the user whose key it is, and an admin is only that for their own.

No call returns a private key. The signing path, which is inside the core, is the only reader:

| Call | Use |
| ---- | --- |
| `agent_keys(user)` | `user`'s keys in the order an ssh-agent offers them: most recently used first, never-used keys last and newest first. |
| `private_key(user, id)` | The decrypted `ssh_key::PrivateKey` of one of `user`'s keys, read from the secret store. `NotFound` for a key that isn't `user`'s. |
| `touch_used(id, now)` | Sets `last_used_at`. |
| `ssh::keys::sign(key, rsa_hash, data)` | The SSH signature of `data`: `ssh-ed25519` or `ecdsa-sha2-*` by `ssh-key`, and `rsa-sha2-256` or `rsa-sha2-512` for RSA. Never SHA-1. |

`ssh-key` 0.6.7 can't sign with an RSA key: it builds the `rsa` key from the prime `p` twice. So
`ssh::keys::rsa_private_key` rebuilds the key with `p` and `q`, and `sign` uses it with
`rsa::pkcs1v15::SigningKey`.

## How a workspace gets its keys

A workspace gets the keys of exactly one user, its **key user**, or none. The core decides when it
creates the workspace and stores the answer in `workspaces.key_user`
([Storage](storage.md#workspaces)). `workspaces::key_user(session, requester)` is the one place that
decides, from the creating turn's `Session` and the call's requester:

| Session | Key user |
| ------- | -------- |
| Direct conversation (a DM, or a user's web chat) | The session's user |
| Isolated job run | The requester: the creator of a runtime job. A config job has none, so no key user |
| Group conversation | None |

Several users' keys can't be kept apart in one workspace. Every command can connect to the agent
socket, and an SSH connection opened in one user's turn, such as a `ControlMaster`, stays usable in
another's. A group workspace therefore gets no keys, which matches per-user MCP toolsets.

### Whose workspace it is

A conversation id can pass from one user to the next: an IRC direct conversation is the sender's
nickname, so a member who takes a nickname inherits its conversation once the earlier user's session
has ended ([Sessions and turns](sessions.md#the-inbound-pipeline)). A workspace is therefore owned
by agent, channel instance, conversation, and, for a direct conversation, its user
([Environment workspaces](environments.md#owners-and-ids)), and the key user of a direct
conversation's workspace is always its owner's user. Two users never share a workspace, so the
second user can't run in the first one's files or sign with their keys, and there is nothing to
replace when a conversation changes hands: the second user has no workspace until they create one,
and the first user's stays theirs. Every workspace call, `environment_status`, `environment_create`,
`environment_reset`, `/env reset`, and the relay's own requests included, is made for the owner of
the calling turn's session, so a turn still running after a `/new` and the next user's first message
keeps using its own user's workspace and neither deletes the other's. Jobs can't get into a held
conversation either: a runtime job whose conversation another user holds uses its own user's DM and
default identity instead, and `Sessions::open_direct` refuses to hand a user another user's open
session ([Sessions and turns](sessions.md), [Scheduler](scheduler.md#one-run)).

A workspace keeps its key user for its life. `recreate` and `/env reset` pass the record's on to the
new workspace, while the model's own `environment_reset` creates the new workspace from the current
turn like `environment_create` does. Migration `0013` filled `key_user` for existing conversation
rows from the newest `direct` session of their key and left job rows without one, and migration
`0016` filled each conversation row's owner user (`workspaces.user_id`) from its `key_user`.

## The agent relay

An environment plugin that reports the `ssh_relay` capability puts an ssh-agent socket in each
workspace. The socket is `SSH_AUTH_SOCK`, and nothing it carries includes a private key: requests go
to the core, which signs. [Environment workspaces](environments.md#the-ssh-agent-relay) says when
the core opens and ends a relay.

**The stream.** `EnvironmentService.Relay` is a bidirectional stream in `doozers.environment.v1`.
The core's first message is `RelayOpen` with the workspace id. After that the plugin sends
`RelayRequest`s, each an `AgentRequest` (connection number and one agent message without its length
prefix) or an `AgentClosed`, and the core answers each request with an `AgentReply` for the same
connection. Ending either side ends the relay, and the core opens another on the workspace's next
use. [Protocols](protocols.md) lists the messages. The same stream carries the
[known-hosts lookups](#known-hosts).

**In the workspace.** The `ssh-agent` subcommand of the workspace helper listens on
`/.doozers/run/ssh-agent.sock` and multiplexes every client connection over its standard input and
output as frames of `u32 length | u8 kind | u64 connection | payload`.
[The Docker plugin's page](environment-docker.md#the-ssh-agent-relay) has the format, and the plugin
runs the helper as a long exec.

**In the core.** `ssh::relay::Relays` keeps one task per workspace. Each task holds an `ssh::Agent`
for the workspace: its key user and the state of its clients' connections. The agent answers one
message at a time, so one workspace's requests are handled in order, and a client waiting on the
core's answer is how the agent protocol works anyway.

### Agent requests

`Agent::handle(connection, message)` always answers with a message. The supported requests are:

| Request | Answer |
| ------- | ------ |
| `REQUEST_IDENTITIES` (11) | `IDENTITIES_ANSWER` (12) with the key user's keys, in the order below. The comment of each is its label |
| `SIGN_REQUEST` (13) | `SIGN_RESPONSE` (14) with the signature, or `FAILURE` |
| `EXTENSION` (27) `session-bind@openssh.com` | `SUCCESS` once the signature verifies, else `FAILURE` (a forwarding bind still restricts the connection) |

Everything else, including adding, removing, and locking keys and every other extension, gets
`SSH_AGENT_FAILURE` (5). So does a message that doesn't parse: a truncated string, a length that
overflows, trailing bytes, or no bytes at all. A failure never ends the relay.

- **Keys are read on every request.** `SshKeys::agent_keys` reads the rows each time, so a key
  deleted on the SSH keys page stops working at the next request, and a new key is offered at once.
  A key that is deleted while its signature is being made fails the request, because `touch_used`
  finds no row.
- **Order.** The most recently used key first (`last_used_at` descending, never-used keys last),
  then the newest (`created_at` descending). OpenSSH servers allow six authentication attempts by
  default (`MaxAuthTries`), so a user with many keys would see "Too many authentication failures"
  without this.
- **Whose keys.** Only the key user's. Another user's key is never listed, and a sign request that
  names one fails whether the blob is right or not. A workspace without a key user answers
  `REQUEST_IDENTITIES` with no keys and fails every signature.
- **Signing.** `ssh::keys::sign` makes the signature on a blocking thread. An RSA key needs flag 2
  (`rsa-sha2-256`) or 4 (`rsa-sha2-512`), and 2 wins if both are set. Flag 0, which asks for SHA-1,
  fails. Other flags are ignored for other key types.
- **`session-bind@openssh.com`.** OpenSSH 8.9 and later tell the agent which host a connection is
  for. The agent verifies the destination's signature over the session id with the host key it
  names. If it verifies, it records the host key's `SHA256:` fingerprint and answers `SUCCESS`.
  Whether the bind says the agent is forwarded doesn't depend on that: a bind with `is_forwarding`
  set is recorded even when the host key doesn't parse or the signature doesn't verify, and the
  answer is then `FAILURE`. OpenSSH goes on using the connection after a failed bind, and `ssh-key`
  can't verify every host key OpenSSH accepts: RSA keys under 2048 bits or over 4096, and `ssh-rsa`
  (SHA-1) signatures. A bind without `is_forwarding` that doesn't verify changes nothing.
- **Forwarded connections.** Any bind with `is_forwarding` set marks the connection for good. Such a
  connection is offered no keys and every sign request on it fails. Clients older than 8.9 never
  send a bind, so for them the check can't apply.
- **Logging.** Each signature updates the key's `last_used_at` and logs at `info` the workspace, the
  user, the key's label, and the bound host key's fingerprint, or `unknown` for a connection that
  never bound. Refusals log at `warn` or `debug`. No log line or error contains a key.
- **State.** The agent keeps a connection's state until the plugin sends `AgentClosed` for it or the
  relay ends. The plugin never reuses a connection number within a relay.
- **Untrusted input.** Every frame comes from a process in the container, and root in the workspace
  can write frames of its own to the helper exec's output, so the relay treats connection numbers,
  `AgentClosed`, and every message as untrusted workspace input. The agent keeps state for at most
  256 connections (`ssh::agent::MAX_CONNECTIONS`). When the table is full, a new `session-bind`
  drops the least recently used connection that isn't forwarded. If every connection is forwarded
  the bind is refused, and a connection whose state the agent couldn't record must not become an
  ordinary connection when a slot frees up later. So the agent remembers the highest connection
  number it has heard from when it refuses a bind. Every connection up to that number that has no
  state is treated as forwarded for good (no keys, no signatures), which costs two integers and no
  per-connection memory. Connections after it are ordinary once there is room. The helper never
  reuses a number, which makes this sound. A connection without state also gets no keys while the
  table is full of forwarded ones, since it could be a forwarded one that has no room yet. A
  workspace that can write frames can ask for its key user's keys anyway, since every command in it
  can connect to the socket.

## Known hosts

`ssh` in a workspace doesn't read `known_hosts` files. It asks the core which host keys to trust,
through the same relay as the agent, and the core answers from the `ssh_known_hosts` table. The code
is `ssh::known_hosts` (`KnownHosts`, which `Workspaces::known_hosts()` returns) and the helper's
`known_hosts.rs`. The decisions are on
[#138 Known SSH hosts](https://github.com/jennings/doozers/issues/138).

### The lookup

`KnownHosts::lookup` takes the environment's client, a `Caller` (the environment instance, the
workspace, the agent of its conversation, and its key user), and a `KnownHostsRequest`: `id`,
`host`, `port`, and the key type and key the server offered. It never fails. Anything that stops the
core from vouching for a key gives the empty answer, which makes `ssh` refuse the connection.

The host is normalized first (`canonical_host`): `ssh` gives `%H` as `[host]:port` for a port other
than 22 and as the plain host for 22, and the table stores the lowercased host without brackets and
the port separately. A host that isn't a name or an address (only letters, digits, and `.-_:%`, at
most 253 bytes) gets no key.

| The host has | The offered key | The core |
| ------------ | --------------- | -------- |
| Keys | Is one of them | Returns the stored keys and stamps that key's `last_used_at` |
| Keys | Isn't one of them | Returns the stored keys, so `ssh` fails with `REMOTE HOST IDENTIFICATION HAS CHANGED`, and logs a warning |
| No key | Any key of a type the core records | [Verifies it](#verified-first-use), records it, and returns it, or returns nothing |
| Any | None (the `NONE` call) | Returns the stored keys and records and stamps nothing |

**The `NONE` call.** `ssh` runs `KnownHostsCommand` twice for each connection. The first call only
asks for the keys that are stored, so that `ssh` offers the host key algorithms it has a key for
first. `%t` and `%K` are the literal `NONE` in it, and the helper sends an empty key type and key.
For a host with no key the answer is empty. The second call has the key the server offered, and only
that call records anything. A key that doesn't parse, doesn't match the type given, or has a type
other than Ed25519, ECDSA on P-256, P-384, or P-521, or RSA is never recorded. The core returns the
stored keys for it.

### Verified first use

A host with no key is trusted on first use, but only the key the environment itself sees for the
host. The core calls `Environment::scan_host_key` for the workspace, host, port, and key type, and
waits up to 15 seconds, a little over the 10 the plugin gives the scan. It records the offered key
only if the scanned key is byte for byte equal. A scan that fails, takes too long, or finds another
key records nothing, logs a warning with both fingerprints, and returns no key, so `ssh` shows
`Host key verification failed`. The workspace's model sees that, and an admin can
[pin the key](#admin-page-and-pinning).

Without the scan, a workspace could plant a fake key for `github.com` by sending a lookup of its
own, since root in a workspace can write any frame to the helper's output. Every workspace of the
instance would then trust it. Because the scan is the environment's own, no frame can make the core
record a key.

- **What the scan proves.** The server the environment reaches for the host holds the key. The
  Docker plugin scans from the machine it runs on, not from inside the workspace, because a
  workspace's root can rewrite `/etc/hosts` or `/etc/resolv.conf` and a scan through them would
  confirm a fake key the workspace was offered. A host that only the workspace's container network
  reaches can't be scanned, so its first connection is refused until an admin
  [pins its key](#admin-page-and-pinning).
- **Two first uses at once.** The store records a key only if the host has none
  (`known_host_record_first_use`). The lookup that loses the race is judged like any lookup of a
  known host, against the key that won.
- **Several keys per host.** A host may have one key of each type. Once a host has a key, a
  connection that is offered another key is never a first use. The `NONE` call returns the stored
  keys, so `ssh` prefers their types, but a server that offers a type the host has no key for is
  refused until an admin pins that key or deletes the host.

### The scan

`doozers_plugin_sdk::ssh::scan_host_key(host, port, key_type, timeout)` connects over TCP, from the
machine the plugin runs on, and returns the key blob only after the server's signature over the
exchange hash verifies. The Docker plugin's `Environment::scan_host_key` is that call with a 10
second timeout, after checking that the workspace exists and is running.

- **Key exchange.** `curve25519-sha256` (and its `@libssh.org` alias) only, with an X25519 key from
  `aws-lc-rs`. A server that offers neither fails the scan with the algorithms it lists.
- **Host key algorithm.** Only the ones for the requested type, so the server must answer with a key
  of the type `ssh` was offered. For RSA they are `rsa-sha2-512` and `rsa-sha2-256`, never SHA-1.
- **Time.** 10 seconds in total, connecting included.
- **What it never does.** Authenticate, or send a password or a key.

[Plugin SDK](plugin-sdk.md#ssh-host-key-scan) has the protocol steps, and
[Docker environment](environment-docker.md#host-key-scans) says why the Docker plugin scans from
outside the workspace.

### In the workspace

The Docker plugin writes `/etc/ssh/ssh_config.d/doozers.conf` and makes sure `ssh_config` includes
that directory ([Docker environment](environment-docker.md#ssh-configuration)):

```text
KnownHostsCommand /.doozers/bin/doozers-workspace-helper known-hosts %H %p %t %K
UserKnownHostsFile /dev/null
GlobalKnownHostsFile /dev/null
StrictHostKeyChecking yes
```

`KnownHostsCommand` needs OpenSSH 8.5 or later. A workspace with an older `ssh` can't connect to any
host, because `ssh` doesn't know the option.

`known-hosts HOST PORT TYPE KEY` connects to the helper's second socket,
`/.doozers/run/known-hosts.sock` (mode 0666, like the agent's). It sends one request, prints one
`known_hosts` line, `<%H as given> <key type> <base64>`, for each key in the answer, and exits 0.
Each connection to the socket is one lookup.

Lookups use the same frames as the agent, with kinds 4 and 5 and a separate series of ids. The
layouts are at the top of `crates/doozers-workspace-helper/src/known_hosts.rs` and the frame format
is in [Docker environment](environment-docker.md#the-ssh-agent-relay):

```text
request: u32 BE port | string host | string key type | string key
answer:  u32 BE count | count * string key
string:  u32 BE length | bytes
```

The plugin decodes a kind 4 frame into a `KnownHostsRequest` on the relay stream, and writes the
core's `KnownHostsReply` as a kind 5 frame with the same id.

**Fail closed.** If the helper's socket isn't there or the relay doesn't answer, the command prints
a message on standard error, exits 1, and prints no line. `ssh` then has no key and, with
`StrictHostKeyChecking yes`, refuses the connection. The same happens for an answer the core leaves
empty. With no relay, `ssh` prints the helper's message
(`the Doozers relay isn't reachable at /.doozers/run/known-hosts.sock`), then
`KnownHostsCommand ... failed, status 1` and `KnownHostsCommand failed`, and gives up before it
checks host keys.

### Bounds

Every lookup is workspace input, so each side bounds what it holds:

- The core answers at most 16 lookups of a relay at once (`ssh::relay::MAX_LOOKUPS`). Each runs in
  its own task, so a slow scan doesn't hold up the agent's requests on the same relay. One past the
  limit gets the empty answer at once, with a warning. The tasks end with the relay.
- The helper holds at most 64 lookups waiting for the plugin and gives up on one that gets no answer
  in 60 seconds, which fails the command. A client of the socket has 10 seconds to send its request,
  and a request or answer is at most 2 MiB.

### The table

`ssh_known_hosts` has a row for each key of each host ([Storage](storage.md#known-ssh-hosts)):

- `environment` is the environment instance. Instances can sit on different networks, where one name
  means different servers, so each has its own hosts.
- `host` is lowercased, without brackets or port, and `port` is the port, so the same name on
  another port is another host.
- `key_type`, `public_key` (the base64 of the key blob) and `fingerprint` (`SHA256:...`) describe
  the key.
- `first_seen_at` is when the row was added. `first_workspace`, `first_agent`, and `first_user` say
  which workspace made the connection that recorded it, the agent of its conversation, and the user
  whose keys it had.
- A key an admin pinned has null in all three and has `added_by`, the admin. A key recorded on first
  use has a null `added_by`.
- `last_used_at` is null until a connection is checked against the key. Each such connection stamps
  it.
- `UNIQUE (environment, host, port, public_key)`: a host has several keys, one for each type, and
  never the same key twice.

The id is `AUTOINCREMENT`, so a page that was open before a delete can't delete the next key that is
recorded. The rows move with `doozers environment rename` ([Instance rename](instance-rename.md)).
[Backups](backups.md) include the table, since they take `doozers.db` whole.

### Admin page and pinning

`KnownHosts` has the calls of the **Known SSH hosts** page, `/admin/ssh-hosts`, which only an admin
opens ([Web channel](web-channel.md)): `all()`, `get(id)`, `delete(id)`, and
`pin(admin, environment, line)`. Deleting removes one key. When a host has none left, its next
connection is a first use again.

`pin` takes one `known_hosts` line and stores its key for each host of the line, with `added_by`
set, whether or not the host has other keys. It returns what it added. It doesn't check the
environment name, so the page checks that it names a configured instance. It refuses a line, with a
message the page shows, for:

- no text, more than one line, or a comment
- a marker (`@cert-authority`, `@revoked`)
- a hashed host (`|1|...`), because the core needs the host to look the key up
- a wildcard or negated pattern (`*`, `?`, `!`), or a host or port that doesn't parse
- a key type other than Ed25519, ECDSA on P-256, P-384, and P-521, and RSA, such as certificates,
  `sk-` keys, and DSA
- a key that isn't base64, isn't a valid public key, or isn't of the type the line says

`KnownHostsError::AlreadyKnown` is returned when every key of the line is stored for its host
already. Comma-separated hosts are accepted, and `[host]:port` gives the port.

### Logging

A new host logs at `info`: `trusting a new SSH host on first use`, with the workspace, environment,
host, port, and key fingerprint. These log at `warn`:

- a key that differs from the stored ones (`the SSH host key changed`), with the offered and the
  stored fingerprints
- a scan that failed, took too long, or found another key than the workspace was offered
- a host that isn't a name or an address, and a key that can't be read
- a lookup past the bound
- a store error, which also gives the empty answer

A log line has fingerprints and never a private key.

## Labels

A label is 1 to 100 characters after trimming, with no control characters. Labels needn't be unique.
A generated key's default label is `<user>@doozers`. An uploaded key's is its comment if it has one
that is a valid label, else `<user>@doozers`.

## Tests

`crates/doozers-core/src/ssh/tests.rs` covers the service and every format, and
`channels/web/tests/ssh_keys.rs` the pages. `ssh/agent_tests.rs` covers the agent: key order and
use, other users' keys, every key type's signature verified with the public key, the RSA flags,
deleted and renamed keys between requests, binds (including an RSA host key, and forwarding binds
with bad signatures or host keys), forwarded connections, a flooded connection table, requests it
doesn't serve, and malformed messages. The relay's lifecycle is tested in `workspaces/tests.rs` with
the in-process environment, and the model's text in `core_tools/environment/tests.rs`. The upload
fixtures are in `crates/doozers-core/src/ssh/testdata/`, checked in so the tests are deterministic.
`testdata/generate.sh` says how they were made with `ssh-keygen` and `openssl`.

The tests that compare against OpenSSH run `ssh-keygen -l` for each generated type's fingerprint and
`ssh-keygen -Y verify` for a signature made with a key read back from the secret store. They print
why they stop when `ssh-keygen` isn't installed.

`ssh/known_hosts/tests.rs` covers the lookup: a first use that the environment confirms, a key that
differs from the scan, a scan that fails or takes too long, a known host with the stored key and
with another, the `NONE` call, host normalization, each environment's own hosts, a pinned key, a
deleted host, two first uses at once, and the lines `pin` accepts and refuses. The helper's
`known-hosts` command and sockets are tested in `crates/doozers-workspace-helper/tests/cli.rs`
against a stand-in for the plugin, the scan in `crates/doozers-plugin-sdk/src/ssh/tests.rs` against
a fake server, and the table in `doozers-store`. The Docker plugin's `tests/known_hosts.rs` runs a
real `ssh` in workspaces against an `sshd` container through the whole path: a first use, a second
workspace, a changed key and trust again after a delete, pins, a scan that differs, failing closed
with no relay, and the `ssh_config.d` inclusion on four distributions
([Docker environment](environment-docker.md#tests)).
