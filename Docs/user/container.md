# Running in a Container

Giles can run as a Linux container. The image holds `giles` and the first-party channel plugins,
[IRC](irc.md), [Email](email.md), and [WhatsApp](whatsapp.md), so configured instances of them
start without installing anything. The built-in web channel and model providers are part of `giles`
itself.

## Getting the Image

Every commit on the `main` branch is published to the GitHub Container Registry:

```sh
docker pull ghcr.io/jennings/giles:latest
```

`latest` is the newest commit; `sha-<short commit>` tags (for example
`ghcr.io/jennings/giles:sha-1a2b3c4`) pin one. The published image is for `linux/amd64` only.

On other platforms, such as an arm64 machine, build it from a checkout of the repository, with
Docker (BuildKit, the default builder):

```sh
docker build -t ghcr.io/jennings/giles:latest .
```

The build compiles everything inside the container, so nothing but Docker is needed on the host.
The first build takes several minutes; later ones reuse the compiled dependencies.

## The Data Root

In the container, the data root ([The Data Root](running.md#the-data-root)) is `/data`, a volume:
`GILES_HOME` is set to it. Giles runs as the unprivileged user `giles`, uid and gid 10001, so the
data root must belong to that user.

The simplest setup is a directory on the host, mounted at `/data`. Write `giles.toml` (and
`secrets.toml`, mode 0600, if you use one) in it, then give it to uid 10001:

```sh
mkdir giles-home
$EDITOR giles-home/giles.toml
sudo chown -R 10001:10001 giles-home
```

Giles creates the rest of the layout on first start.

## The Web Port

The image exposes port 8080, but Giles listens on `127.0.0.1:8080` by default, which can't be
reached from outside the container. Set `http_listen` in `giles.toml` to listen on every interface
of the container:

```toml
[core]
http_listen = "0.0.0.0:8080"
```

Then publish the port. Publishing it on the host's loopback address, as below, keeps it off the
network; put a reverse proxy that terminates TLS in front of it to reach it from elsewhere
([The Web Interface](running.md#the-web-interface)).

## Starting It

```sh
docker run --detach --name giles --restart unless-stopped \
  --volume "$PWD/giles-home:/data" \
  --publish 127.0.0.1:8080:8080 \
  ghcr.io/jennings/giles:latest
```

The container runs `giles serve`. Its log goes to `docker logs giles`. `docker stop giles` sends
SIGTERM, so Giles shuts down cleanly ([Running the Daemon](running.md#running-the-daemon)).

The other `giles` commands ([Command Line](cli.md)) run inside the container, as the same user:

```sh
docker exec giles giles status
docker exec giles giles admin login-link --user owner
```

Arguments after the image name replace `serve`, so
`docker run --rm ghcr.io/jennings/giles:latest --version` prints the version.

### Secrets From the Environment

Instead of `secrets.toml`, a secret can come from the container's environment
([Secrets](configuration.md#secrets)): refer to it as `{ env = "ANTHROPIC_API_KEY" }` and pass it
with `docker run --env ANTHROPIC_API_KEY=...` (or `--env-file`).

### Time Zone

The image includes the time zone database and runs in UTC. Set the owner's time zone in
`giles.toml` ([`[owner]`](configuration.md#owner)), or the container's with
`--env TZ=America/Los_Angeles`.

## Plugins

The bundled plugins are in `/usr/local/bin/`, where Giles finds them on `$PATH`. Other plugin
binaries go in `/data/plugins/`, which Giles searches first.

The image contains the WhatsApp plugin, which is licensed `AGPL-3.0-only`; the rest of Giles is
`MIT OR Apache-2.0`. The image's license label says `(MIT OR Apache-2.0) AND AGPL-3.0-only`, and
`/usr/share/doc/giles/` has the license texts and `THIRD-PARTY`, which says where the plugin's
source is ([Licensing](whatsapp.md#licensing)).

A plugin only runs when `giles.toml` has an instance of it, so the WhatsApp plugin does nothing,
and never contacts WhatsApp, until you add one. Its session (the credential that makes Giles a
linked device) is kept in `/data/data/plugins/<instance>/`, so it survives the container being
replaced as long as `/data` does. To link it, publish the web port as above and open the admin
page ([Linking a Channel](web.md#linking-a-channel)) for the QR code, or set `link = "pair_code"`
([WhatsApp](whatsapp.md#linking)).

The plugin sandbox works in the container with Docker's default security settings; no
`--privileged` or extra capabilities are needed. It does need Landlock in the host's kernel, since a
container shares the host's kernel ([Plugin Sandbox Check](running.md#plugin-sandbox-check)). Check
the second line of `docker exec giles giles status`:

```text
Plugin sandbox: Landlock ABI 6 and seccomp filters
```

`Plugin sandbox: unavailable: ...` names what's missing, and sandboxed plugins don't start. That
happens under virtual machine kernels built without Landlock, such as Apple's `container` tool on
macOS. There, an instance can run unsandboxed with `unsandboxed = true`
([Instances](configuration.md#instances)), at the cost of the protection the sandbox gives.
