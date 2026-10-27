# Running in a container

Giles can run as a Linux container.

The `latest` image holds the `giles` command and these first-party channel plugins:

- [IRC](irc.md)
- [Email](email.md)
- [WhatsApp](whatsapp.md)
- [Discord](discord.md)

## Create a data volume

Persistent data is stored in `/data` in the container, so you'll want to mount that as a volume. The
directory must be owned by uid/gid 10001.

```sh
mkdir giles-home
$EDITOR giles-home/giles.toml
sudo chown -R 10001:10001 giles-home
```

## Starting the server

```sh
docker run --detach --name giles --restart unless-stopped \
  --volume "$PWD/giles-home:/data" \
  --publish 127.0.0.1:8080:8080 \
  ghcr.io/jennings/giles:latest
```

## The web port

Giles listens on `127.0.0.1:8080` by default, so you'll probably want to bind to all interfaces by
setting `http_listen` in `giles.toml`:

```toml
[core]
http_listen = "0.0.0.0:8080"
```

[The web interface](running.md#the-web-interface) does not handle TLS, and Giles is very new
software. **Do not expose it to the public Internet**. Using [Tailscale](https://www.tailscale.com)
to make it available to your network is recommended.

## Secrets

Secrets come from `/data/secrets.toml` by default.

A secret can instead come from the container's environment ([Secrets](configuration.md#secrets)):
refer to it as `{ env = "ANTHROPIC_API_KEY" }` and pass it with
`docker run --env ANTHROPIC_API_KEY=...` (or `--env-file`).

## Sandboxing

The plugin sandbox works with Docker's default security settings; no `--privileged` or extra
capabilities are needed. It does need Landlock in the host's kernel, since a container shares the
host's kernel ([Plugin sandbox check](running.md#plugin-sandbox-check)). Check the second line of
`docker exec giles giles status`:

```text
Plugin sandbox: Landlock ABI 6 and seccomp filters
```

To run a plugin without sandboxing, set `channel.<name>.unsandboxed = true` in `giles.toml`.
