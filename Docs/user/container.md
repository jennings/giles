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

## Agent workspaces

Don't mount a container engine's socket into the Giles container. To give agents a workspace, run
the [Docker environment image](docker-environment.md#run-the-plugin-in-its-own-container) next to
Giles. [Docker Compose](#run-giles-with-docker-compose) runs the two together.

## Run Giles with Docker Compose

The repository's `compose.yaml` runs Giles and the Docker environment plugin in two containers. Run
these steps in a clone of the repository:

```sh
git clone https://github.com/jennings/giles.git
cd giles
```

Create `.env` with the engine's socket and a token for Giles to authenticate to the plugin with:

```sh
cat > .env <<EOF
CONTAINER_ENGINE_SOCKET=/run/user/1000/podman/podman.sock
GILES_ENVIRONMENT_DOCKER_TOKEN=$(openssl rand -hex 32)
EOF
chmod 600 .env
```

[Choose the engine](docker-environment.md#choose-a-container-engine) before you set
`CONTAINER_ENGINE_SOCKET`. The socket above is rootless Podman's for uid 1000.

Create the data directory `giles-home` with a `giles.toml` that points the `docker` environment at
the plugin's container:

```toml
[core]
http_listen = "0.0.0.0:8080"

[owner]
identities = ["web:owner"]

[environment.docker]
description = "Disposable Linux containers for running code and building software."
address = "http://giles-environment-docker:7443"
insecure = true
auth = { bearer = { env = "GILES_ENVIRONMENT_DOCKER_TOKEN" } }
```

```sh
mkdir giles-home
$EDITOR giles-home/giles.toml
sudo chown -R 10001:10001 giles-home
```

Start both containers:

```sh
docker compose up --detach
```

This pulls the published images. To build them from your checkout instead, run
`docker compose up --detach --build`. With rootless Podman, run `podman compose` as the user that
owns the socket.

The web interface is on `http://127.0.0.1:8080`. Check that Giles reaches the plugin with
`docker compose exec giles giles status`, which shows the `docker` instance as `running`.

To [change the plugin's settings](docker-environment.md#change-the-plugins-settings), put them in
`giles-environment-docker.toml` and add the file in `compose.override.yaml`, which Docker Compose
reads after `compose.yaml`:

```yaml
services:
  giles-environment-docker:
    volumes:
      - ./giles-environment-docker.toml:/etc/giles-environment-docker.toml:ro
    command: ["--config", "/etc/giles-environment-docker.toml"]
```

Then run `docker compose up --detach` again.
