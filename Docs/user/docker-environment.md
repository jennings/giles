# Docker environment

This page shows how to give agents a workspace in a Docker container with the Docker environment
plugin, `giles-environment-docker`. In a workspace the agent runs commands and reads, writes, finds,
and searches files without touching your machine.

The plugin works with anything that serves the Docker Engine API: Docker, Podman, Docker Desktop,
OrbStack, and Colima. Each workspace is one container plus one volume, and each conversation gets
its own workspace.

## Choose a container engine

**Use rootless Podman.** The plugin can start any container the engine allows, so access to its
socket is as powerful as the engine.
**Do not give the plugin the socket of a rootful Docker engine on the machine that runs Giles.**
That socket is root on that machine.

If Giles runs in [its container image](container.md), don't mount an engine socket into it.
[Run the plugin on another machine](#run-the-plugin-on-another-machine) that has a rootless engine
instead.

## Set up the plugin on the Giles machine

Add an environment instance to `giles.toml`. The sandbox grant lets the plugin reach the engine's
socket, and the plugin reads its helper files from the directory of its own binary:

```toml
[environment.docker]
description = "Disposable Linux containers for running code and building software."
stop_after = "30m"
delete_after = "30d"

[environment.docker.sandbox]
unix = ["/run/user/1000/podman/podman.sock"]
read = ["/usr/local/bin"]

[environment.docker.config]
socket = "/run/user/1000/podman/podman.sock"
images = ["ghcr.io/acme/tools:*"]
memory = "2g"
cpus = 2.0
pids = 512
```

- `description` is what the model reads when it chooses an environment.
- `stop_after` stops a workspace's container after this long without use. Its files and installed
  packages stay, and the next command starts it again.
- `delete_after` deletes a workspace, with its files, once its conversation has had no messages and
  the workspace has not been used for this long. It can take a few minutes longer.
- `socket` is the engine's socket, and `unix` grants the same path. For rootless Podman that is
  `/run/user/<uid>/podman/podman.sock`.
- `read` is the directory with the plugin and its helper files. It must be outside the Giles data
  directory, so install the plugin in a system directory such as `/usr/local/bin`.

The instance name is `docker`, so the plugin is the component `docker`. Name the instance something
else, such as `[environment.builder]`, and add `component = "docker"` to it.

Without `socket`, a plugin Giles launches uses `/var/run/docker.sock`. A plugin you run on another
machine uses the `unix://` address in `DOCKER_HOST` first.

## Let an agent use it

Agents don't get an environment by default. Name the environments an agent may use in its
`agent.toml`:

```toml
environments = ["docker"]
```

The agent then has tools to create a workspace, run commands, and work with files, described in
[Environments](environments.md). The model chooses the image when it creates a workspace.

## Run the plugin on another machine

Running the plugin on the machine that has the engine keeps the engine's socket away from Giles. It
is the setup to use when Giles runs in its container image. Install `giles-environment-docker` and
the `giles-workspace-helper-*` files that come with it on that machine, and follow
[Running a plugin on another machine](remote-plugins.md) for the token and TLS. For an environment
plugin the settings go in the `[config]` table of the file you pass to `serve --config`:

```toml
listen = "0.0.0.0:7443"
data = "/var/lib/giles-environment-docker"
tls_cert = "/etc/giles/fullchain.pem"
tls_key = "/etc/giles/key.pem"
token_file = "/etc/giles/docker.token"

[config]
socket = "/run/user/1000/podman/podman.sock"
images = ["ghcr.io/acme/tools:*"]
memory = "2g"
```

In `giles.toml`, the instance has only the address and the token. Giles refuses an instance that has
both `address` and `config`, because the plugin's settings live on its own machine:

```toml
[environment.docker]
description = "Disposable Linux containers for running code and building software."
address = "https://builder.example.com:7443"
auth = { bearer = { secret = "docker-remote" } }
```

Registry logins and environment variables stay on the machine that runs the plugin.

## Choose which images are allowed

The model picks an image from a list that you control. Each entry is `[registry/]image[:tag]`:

- `ubuntu` is `ubuntu:latest` from Docker Hub, and `jennings/tools` is `jennings/tools:latest`.
- A first part that contains a `.` or a `:`, or is `localhost`, is a registry, as in
  `ghcr.io/acme/tools:1.4` or `localhost:5000/dev:latest`.
- A tag of `*` allows every tag. It can only be the whole tag, so `python:3.*` is an error.
- An image given as `name@sha256:...` is allowed by a `name:*` entry and by nothing else.

The plugin allows `ubuntu:*`, `debian:*`, `alpine:*`, `python:*`, `node:*`, `golang:*`, `rust:*`,
`ruby:*`, and `eclipse-temurin:*` without any settings. `images` adds entries to that list. To allow
only your own images, set `default_images = false`:

```toml
[environment.docker.config]
default_images = false
images = ["ghcr.io/acme/tools:*", "ghcr.io/acme/builder:2"]
```

The allowed images are in the options the model sees when it creates a workspace. If the model asks
for another image, the plugin refuses and lists the allowed ones.

Every image needs `/bin/sh`. Commands run through it, and the plugin refuses an image without it.

## Log in to a registry

The plugin pulls an image the first time a workspace needs it. Log in to Docker Hub, because
anonymous pulls are rate limited and a limit makes a workspace fail to start. Create an access token
in Docker Hub and add it for the registry:

```toml
[environment.docker.config.registries."docker.io"]
username = "stephen"
password = { secret = "dockerhub" }
```

Add another entry for each registry that needs a login, such as `ghcr.io`. Write the password in
[secrets](configuration.md#secrets) when the plugin runs on the Giles machine. In the settings file
of a plugin on another machine, write it in the file and keep the file readable only by its owner.

## Limit resources and network

These settings apply to every workspace of the instance:

```toml
[environment.docker.config]
memory = "2g"
cpus = 2.0
pids = 512
network = "bridge"
```

The model can't change them.

- `memory` is the most memory a container may use, such as `"512m"` or `"2g"`. There is no limit by
  default.
- `cpus` is the most CPUs a container may use. There is no limit by default.
- `pids` is the most processes a container may run. There is no limit by default.
- `network` is the Docker network the containers join. The default, `"bridge"`, gives them internet
  access, so `git clone` and package installs work. `"none"` gives them no network, and a network
  name joins a network you created.

**With the default network, a workspace can reach services on the engine's host, including Giles's
HTTP port.** If the model shouldn't reach them, set `network = "none"`, or create a network that
your firewall restricts and name it. A workspace without a network can't install packages or clone
repositories.

## Set environment variables

`env` sets variables in every container of the instance:

```toml
[environment.docker.config.env]
CI = "1"
```

**The model can read every variable you set here, for example by running `env`.** Don't put secrets
in it that you wouldn't show the model.

## What a workspace keeps

The volume mounted at `/workspace` holds the files the agent works on, including its home directory
at `/workspace/.home`. Commands start in `/workspace`, and relative paths in file tools start there
too. Files in `/workspace` last until the workspace is deleted.

The rest of the container is disposable:

- When a workspace is idle for `stop_after`, its container stops. The next command starts it again,
  and the agent is told that processes it left running have ended. Files and installed packages are
  still there.
- If the container is removed, for example by `docker container prune`, the next command creates a
  new one from the same image version and the same volume. Files in `/workspace` are still there,
  but packages installed elsewhere, such as with `apt`, are gone, and the agent is told so.

A workspace keeps the version of the image it started from, even when a tag like `latest` moves.
Commands run as root inside the container, which a rootless engine maps to your own user. The plugin
removes an image it pulled when no workspace uses it any more, and leaves alone images that were
already on the engine.

## Settings

These keys go in the `config` table of the instance, or in the `[config]` table of a remote plugin's
file. An unknown key or invalid value stops the instance from starting, and the error names the key.

| Key              | Default                      | Meaning                                                      |
| ---------------- | ---------------------------- | ------------------------------------------------------------ |
| `socket`         | `/var/run/docker.sock` (`DOCKER_HOST` first on another machine) | The engine's Unix socket |
| `images`         | `[]`                         | [Allowed images](#choose-which-images-are-allowed) in addition to the built-in list |
| `default_images` | `true`                       | Allow the built-in list                                      |
| `registries`     | none                         | [Logins](#log-in-to-a-registry) by registry, each with `username` and `password` |
| `memory`         | none                         | The memory limit of a container, such as `"2g"`              |
| `cpus`           | none                         | The CPU limit of a container                                 |
| `pids`           | none                         | The process limit of a container                             |
| `network`        | `"bridge"`                   | The [network](#limit-resources-and-network) containers join  |
| `env`            | none                         | [Variables](#set-environment-variables) set in every container |
| `helper_dir`     | the plugin's own directory   | The directory with the `giles-workspace-helper-*` files      |

## Check that it works

Run `giles status`. The instance shows `running`. Then ask an agent that has the environment to
create a workspace and run `uname -a`.

If the instance doesn't start, the error names the problem:

- A message that no workspace helper exists for your architecture means the plugin can't find
  `giles-workspace-helper-x86_64` or `giles-workspace-helper-aarch64` next to it. Set `helper_dir`
  to the directory that has them, and grant read access to it.
- A message that the Docker socket can't be used means the path is wrong, the engine isn't running,
  or the sandbox grant doesn't include the socket.
