# SSH keys and SSH in workspaces

This page shows how to give Doozers an SSH key, add its public key to GitHub, use the key for `ssh`
and `git` in your agent's workspace, and manage the server host keys that `ssh` there trusts.

Each user keeps their own SSH keys in Doozers, on the **SSH keys** page in the
[web interface](web.md#ssh-keys). Doozers keeps every private key encrypted and never shows it
again, not even one it generated. Only you can use your keys. An admin can see every user's key
details and delete a key, but can't use one.

## Generate a key

To make a new key:

1. Select **SSH keys** in the sidebar.
2. Under **Generate a key**, choose the **Type**. Ed25519 is the default and the best choice unless
   a server needs another type.
3. Enter a **Label** to tell your keys apart, or leave it empty to use `<you>@doozers`.
4. Select **Generate**.

You can generate these types:

- Ed25519
- RSA with 3072 or 4096 bits. RSA keys take a few seconds to generate.
- ECDSA on P-256, P-384, or P-521

## Upload a key

Upload a key you already use if it should work from Doozers too. Doozers keeps its own copy, so
deleting it in Doozers doesn't touch your original.

1. Under **Upload a key**, paste the private key into **Private key**, or choose its file under
   **Key file**.
2. If the key has a passphrase, enter it under **Passphrase**. Doozers decrypts the key once and
   keeps it without the passphrase, because nobody is there to type it later.
3. Enter a **Label**, or leave it empty to use the comment stored in the key.
4. Select **Upload**.

Doozers reads private keys in these formats, as PEM text:

- OpenSSH (`-----BEGIN OPENSSH PRIVATE KEY-----`), the format `ssh-keygen` writes
- PKCS#1 RSA (`-----BEGIN RSA PRIVATE KEY-----`)
- SEC1 EC (`-----BEGIN EC PRIVATE KEY-----`)
- PKCS#8 (`-----BEGIN PRIVATE KEY-----` and `-----BEGIN ENCRYPTED PRIVATE KEY-----`)

Doozers doesn't accept PuTTY `.ppk` files, DSA keys, or security keys (`sk-ssh-ed25519@openssh.com`
and `sk-ecdsa-sha2-nistp256@openssh.com`). To use a PuTTY key, convert it first:

```sh
puttygen key.ppk -O private-openssh -o key
```

A key that is encrypted with another cipher can be converted with `ssh-keygen -p -f key`. You can't
upload a key you already have, but another user can upload the same key.

## Add the public key to GitHub

Each key on the page shows its **Public key**, ready to paste into a service that should accept it.
To add it to GitHub:

1. Select the copy button at the right end of the key's **Public key** field. If there's no button,
   select the text and copy it.
2. On GitHub, open **Settings**, then **SSH and GPG keys**, and select **New SSH key**.
3. Enter a **Title**, paste the public key into **Key**, and select **Add SSH key**.

GitHub lists the key's fingerprint, which starts with `SHA256:`. It matches the **Fingerprint** on
the page. For another server, add the same line to the `~/.ssh/authorized_keys` file of the account
you log in to.

## Rename or delete a key

Select **Rename** to change a key's label. The label is the comment at the end of the public key.
Renaming doesn't change the key, so a copy you registered earlier keeps working.

Select **Delete** and confirm to remove a key from Doozers. Doozers can't bring it back. Remove the
public key from GitHub and your servers too.

## See every user's keys

An admin sees every user's keys under **Admin**, then **SSH keys**: the owner of each key, its
label, type, fingerprint, and when it was created and last used. An admin can delete a key there,
such as one that belonged to a user who left.

## Use SSH in a workspace

A workspace can use your keys for `ssh` and for git remotes such as `git@github.com:you/repo.git`.
The private key never enters the workspace. Programs there ask Doozers to sign, and Doozers signs
with one of your keys. A key you add or delete works or stops working at once, without restarting
anything.

**While your agent works in your workspace, it can use all of your keys. It can sign in to every
server and service that accepts them. Use a key that has only the access your agent needs, such as a
key that you registered on one GitHub repository as a deploy key.**

Which keys a workspace gets depends on the conversation that created it:

- A conversation with you, such as a direct message or your web chat, gets your keys.
- An isolated job run gets the keys of the user who created the job. A job from `doozers.toml` has
  no creator, so its workspace gets none.
- A group conversation gets none, because the commands of several users can't be kept apart.

The workspace's image needs an OpenSSH client, which provides `ssh` and `ssh-add`. Debian, Ubuntu,
and Alpine images have it in the `openssh-client` package. Your agent can install it in the
workspace, or you can use an image that has it.

To check that SSH works, run these in the workspace:

```sh
ssh-add -L
ssh -T git@github.com
```

`ssh-add -L` lists the public keys the workspace gets, with the labels you gave them. `ssh -T`
greets you by your GitHub user name if one of those keys is registered at GitHub. If the first
command prints `The agent has no identities`, the workspace gets no keys. Your agent's
`environment_plugins` and `environment_create` results say which keys it gets and why it has none.

Doozers doesn't ask you to accept a server's host key. It checks the key itself, as the next section
describes.

Doozers refuses to sign for a forwarded agent, so `ssh -A` gives the server you connect to no keys.
The **Workspaces** page shows whose keys each workspace gets under **SSH keys**.

## Host keys in a workspace

`ssh` in a workspace trusts the host keys that Doozers stores. It doesn't read or write
`~/.ssh/known_hosts`, and you don't pass `StrictHostKeyChecking`. Nothing needs setting up, because
the [Docker environment](docker-environment.md) configures `ssh` in each workspace, and the setting
survives a restart or a reset.

- The first time a workspace connects to a server, Doozers fetches the server's host key itself,
  from the machine Doozers' environment plugin runs on, not from the workspace. If it's the key
  `ssh` was shown, Doozers records it and `ssh` connects.
- Every later connection to that server, from any workspace on the same environment, uses the
  recorded key.
- Each environment keeps its own hosts. Environments can sit on different networks, where one name
  can mean different servers, so a host that the `docker` environment trusts isn't trusted by
  another environment until a workspace there connects to it.

The workspace's image needs OpenSSH 8.5 or later for this. The OpenSSH client packages of current
Debian, Ubuntu, Alpine, and Fedora releases are new enough. With an older `ssh`, every connection
fails with `Bad configuration option`. Use an image with a newer one.

Doozers records the keys of the types `ssh-ed25519`, `ecdsa-sha2-nistp256`, `ecdsa-sha2-nistp384`,
`ecdsa-sha2-nistp521`, and `ssh-rsa`. A server may have one key of each. How Doozers checks a key is
in [SSH keys and the agent relay](../technical/ssh.md#known-hosts).

### When a host key changes

If a server shows a different key than the one Doozers recorded, `ssh` fails with
`REMOTE HOST IDENTIFICATION HAS CHANGED` and `Host key verification failed`. Your agent sees those
messages in the command's output and can't get past them.

A server that was reinstalled or that rotated its keys looks like this, and so does someone
intercepting the connection. Ask the server's administrator whether the key changed. If it did,
delete the host on the [Known SSH hosts](#known-ssh-hosts) page. The next connection records the new
key.

### When a first connection can't be verified

A connection to a new server fails with `Host key verification failed`, without the
`REMOTE HOST IDENTIFICATION HAS CHANGED` banner, when Doozers can't confirm the key. Doozers records
nothing in that case. This happens when:

- Doozers can't reach the server from where it checks, which is not the workspace's network. A
  server that only the workspace reaches, such as another container or a private server, is the
  usual case.
- The check fails, for example because the server doesn't support the `curve25519-sha256` key
  exchange.
- The key Doozers sees is different from the one `ssh` was shown.

A server that only the workspace's network reaches always fails this way on the first connection.
Pin its key on the [Known SSH hosts](#known-ssh-hosts) page, which skips the check, and do the same
for any other server you trust. Get the key from the server's administrator, from `ssh-keyscan`, or
from the server's `/etc/ssh/ssh_host_ed25519_key.pub` file, which needs the host name in front of
it.

## Known SSH hosts

An admin sees the hosts Doozers trusts under **Admin**, then **Known SSH hosts**. The page lists
each environment's hosts. A host has one row for each of its keys, with:

- The host and port.
- The key type and its `SHA256:` fingerprint.
- When Doozers first saw the key, and which workspace, agent, and user connected, or `pinned by`
  followed by the admin who added it.
- When a connection last used the key.

Select **Delete** and confirm to remove a key. When a host has no key left, its next connection is
checked and recorded again, as for a new host. Delete a host that rotated its key, once you've
confirmed the change.

To trust a key before any workspace connects, pin it under **Add a host key**:

1. Choose the **Environment** whose workspaces should trust the key.
2. Paste one `known_hosts` line into **Line**, such as one of these:

   ```text
   git.example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA...
   [git.example.com]:2222 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA...
   ```

3. Select **Add**.

A line without a port is for port 22, and `[host]:port` is for another port. You can list several
hosts, separated by commas, and Doozers pins the key for each. Pinning adds the key beside any the
host already has, and `ssh` then trusts both, so delete the old key if the new one replaces it.

Doozers refuses a line that it can't pin, and says why:

- Hashed host names, which start with `|1|`, because Doozers needs to know which host the key is
  for. Run `ssh-keyscan` without `-H`, or ask the server's administrator.
- Markers such as `@cert-authority` and `@revoked`.
- Wildcard and negated patterns, with `*`, `?`, or `!`. Pin one host at a time.
- Key types other than the five above, such as certificates, security keys, and DSA.
- More than one line, a comment, or a key that doesn't match its type.
- A key that is already known for every host in the line.
