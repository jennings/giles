# syntax=docker/dockerfile:1

# The giles-environment-docker container image: the Docker environment plugin in `serve` mode, for
# running next to a container engine and pointing a Giles core at it. Build it from the repository
# root with `docker build -f plugins/giles-environment-docker/Dockerfile .`. See
# Docs/technical/container-image.md.

# Build stage: the official Rust image on Debian bookworm, the same glibc as the runtime stage.
FROM rust:1-bookworm AS build
WORKDIR /src

# The toolchain pinned in rust-toolchain.toml, with its musl targets, in its own layer.
COPY rust-toolchain.toml ./
RUN rustup toolchain install && rustc --version

COPY . .

# The plugin, and the static helper it copies into each workspace container. The helper must match
# the engine's architecture, which needn't be this image's (an amd64 image under emulation on an
# arm64 engine), so both are built: musl binaries linked by rust-lld (.cargo/config.toml) cross-build
# from either architecture.
RUN --mount=type=cache,id=giles-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=giles-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=giles-environment-docker-target,target=/src/target,sharing=locked \
    set -eu; \
    cargo build --release --locked -p giles-environment-docker; \
    mkdir /out; \
    cp target/release/giles-environment-docker /out/; \
    for arch in x86_64 aarch64; do \
        cargo build --release --locked -p giles-workspace-helper --target "$arch-unknown-linux-musl"; \
        cp "target/$arch-unknown-linux-musl/release/giles-workspace-helper" \
            "/out/giles-workspace-helper-$arch"; \
    done

# Runtime stage.
FROM debian:bookworm-slim

# `tini` runs as PID 1 and forwards signals to the plugin.
RUN apt-get update \
    && apt-get install -y --no-install-recommends tini \
    && rm -rf /var/lib/apt/lists/*

# The plugin finds the helpers next to its own binary.
COPY --from=build /out/ /usr/local/bin/
COPY LICENSE-MIT LICENSE-APACHE /usr/share/doc/giles-environment-docker/

LABEL org.opencontainers.image.title="giles-environment-docker" \
      org.opencontainers.image.description="The Giles Docker environment plugin, served for a remote Giles core." \
      org.opencontainers.image.source="https://github.com/jennings/giles" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# The plugin keeps no state of its own (workspaces are described by labels on the engine's
# resources), so /data is scratch space and needs no volume.
RUN install -d -m 0700 /data
WORKDIR /data
EXPOSE 7443

# It runs as root: what it may do comes from the mounted engine socket, which a mounted socket's
# owner and mode (root:docker on a Docker host, the user's own for rootless Podman) would otherwise
# keep from an unprivileged uid. Arguments are added to these, e.g. `--config /config.toml`, and the
# token comes from GILES_ENVIRONMENT_DOCKER_TOKEN or `token_file`.
ENTRYPOINT ["/usr/bin/tini", "--", "giles-environment-docker", "serve", "--listen", "0.0.0.0:7443", "--data", "/data"]
