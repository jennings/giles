# Running in a container

Doozers can run as a Linux container.

The `latest` image holds the `doozers` command and these first-party channel plugins:

- [IRC](irc.md)
- [Email](email.md)
- [WhatsApp](whatsapp.md)
- [Discord](discord.md)

## Create a data volume

Persistent data is stored in `/data` in the container, so you'll want to mount that as a volume. The
directory must be owned by uid/gid 10001.

```sh
mkdir doozers-home
$EDITOR doozers-home/doozers.toml
sudo chown -R 10001:10001 doozers-home
```

## Starting the server

```sh
docker run --detach --name doozers --restart unless-stopped \
  --volume "$PWD/doozers-home:/data" \
  --publish 127.0.0.1:8080:8080 \
  ghcr.io/jennings/doozers:latest
```

## The web port

Doozers listens on `127.0.0.1:8080` by default, so you'll probably want to bind to all interfaces by
setting `http_listen` in `doozers.toml`:

```toml
[core]
http_listen = "0.0.0.0:8080"
```

[The web interface](running.md#the-web-interface) does not handle TLS, and Doozers is very new
software. **Do not expose it to the public Internet**. Using [Tailscale](https://www.tailscale.com)
to make it available to your network is recommended.

## Secrets

Secrets come from `/data/secrets.toml` by default.

A secret can instead come from the container's environment ([Secrets](configuration.md#secrets)):
refer to it as `{ env = "ANTHROPIC_API_KEY" }` and pass it with
`docker run --env ANTHROPIC_API_KEY=...` (or `--env-file`).

The key that unlocks the secrets Doozers stores is `/data/secret.key`, which Doozers creates on
first start. To keep it out of the data volume, move it into a Compose secret:

```yaml
services:
  doozers:
    secrets:
      - doozers_secret_key
secrets:
  doozers_secret_key:
    file: ./doozers-secret.key
```

```toml
[core]
secret_key_file = "/run/secrets/doozers_secret_key"
```

Move `doozers-home/secret.key` to `./doozers-secret.key` before you start Doozers with the setting.
See
[`secret_key_file`](configuration.md#secret_key_file).

## Sandboxing

The plugin sandbox works with Docker's default security settings; no `--privileged` or extra
capabilities are needed. It does need Landlock in the host's kernel, since a container shares the
host's kernel ([Plugin sandbox check](running.md#plugin-sandbox-check)). Check the second line of
`docker exec doozers doozers status`:

```text
Plugin sandbox: Landlock ABI 6 and seccomp filters
```

To run a plugin without sandboxing, set `channel.<name>.unsandboxed = true` in `doozers.toml`.

## Agent workspaces

Don't mount a container engine's socket into the Doozers container. To give agents a workspace, run
the [Docker environment image](docker-environment.md#run-the-plugin-in-its-own-container) next to
Doozers. [Docker Compose](#run-doozers-with-docker-compose) runs the two together.

## Run Doozers with Docker Compose

The repository's `compose.yaml` runs Doozers and the Docker environment plugin in two containers.
Run these steps in a clone of the repository:

```sh
git clone https://github.com/jennings/doozers.git
cd doozers
```

Create `.env` with the engine's socket and a token for Doozers to authenticate to the plugin with:

```sh
cat > .env <<EOF
CONTAINER_ENGINE_SOCKET=/run/user/1000/podman/podman.sock
DOOZERS_ENVIRONMENT_DOCKER_TOKEN=$(openssl rand -hex 32)
EOF
chmod 600 .env
```

[Choose the engine](docker-environment.md#choose-a-container-engine) before you set
`CONTAINER_ENGINE_SOCKET`. The socket above is rootless Podman's for uid 1000.

Create the data directory `doozers-home` with a `doozers.toml` that points the `docker` environment
at the plugin's container:

```toml
[core]
http_listen = "0.0.0.0:8080"

[environment.docker]
description = "Disposable Linux containers for running code and building software."
address = "http://doozers-environment-docker:7443"
insecure = true
auth = { bearer = { env = "DOOZERS_ENVIRONMENT_DOCKER_TOKEN" } }
```

```sh
mkdir doozers-home
$EDITOR doozers-home/doozers.toml
sudo chown -R 10001:10001 doozers-home
```

Start both containers:

```sh
docker compose up --detach
```

This pulls the published images. To build them from your checkout instead, run
`docker compose up --detach --build`. With rootless Podman, run `podman compose` as the user that
owns the socket.

The web interface is on `http://127.0.0.1:8080`. Check that Doozers reaches the plugin with
`docker compose exec doozers doozers status`, which shows the `docker` instance as `running`.

To [change the plugin's settings](docker-environment.md#change-the-plugins-settings), put them in
`doozers-environment-docker.toml` and add the file in `compose.override.yaml`, which Docker Compose
reads after `compose.yaml`:

```yaml
services:
  doozers-environment-docker:
    volumes:
      - ./doozers-environment-docker.toml:/etc/doozers-environment-docker.toml:ro
    command: ["--config", "/etc/doozers-environment-docker.toml"]
```

Then run `docker compose up --detach` again.
