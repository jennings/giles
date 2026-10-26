# syntax=docker/dockerfile:1

# The Giles container image (PLAN section 14.2): `giles` and the first-party plugins on Debian
# bookworm. Build it with `docker build .`; nothing is built on the host first. See
# Docs/technical/container-image.md.

# Build stage: the official Rust image on Debian bookworm, the same glibc as the runtime stage.
FROM rust:1-bookworm AS build
WORKDIR /src

# The toolchain pinned in rust-toolchain.toml (`rustup toolchain install` with no argument reads
# it), in its own layer, so it's downloaded again only when the pin changes.
COPY rust-toolchain.toml ./
RUN rustup toolchain install && rustc --version

COPY . .

# The binaries the image ships: the daemon and every first-party plugin meant for real use (the echo
# channel plugin is test-only). Protobuf code generation uses the vendored `protoc`. `target/` is a
# cache mount, which isn't part of the layer, so the binaries are copied out to /out.
RUN --mount=type=cache,id=giles-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=giles-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=giles-target,target=/src/target,sharing=locked \
    set -eu; \
    bins="giles giles-channel-irc giles-channel-email giles-channel-whatsapp giles-channel-discord"; \
    cargo build --release --locked $(for bin in $bins; do printf -- '-p %s ' "$bin"; done); \
    mkdir /out; \
    for bin in $bins; do cp "target/release/$bin" /out/; done

# Runtime stage.
FROM debian:bookworm-slim

# `tini` runs as PID 1: it forwards signals to `giles` and reaps orphaned plugin descendants, which
# would otherwise stay zombies.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tzdata tini \
    && rm -rf /var/lib/apt/lists/*

# A fixed, unprivileged uid and gid outside the range of system and login accounts, so a
# bind-mounted data root can be given to it.
RUN groupadd --gid 10001 giles \
    && useradd --uid 10001 --gid giles --home-dir /data --no-create-home --shell /usr/sbin/nologin \
        giles \
    && install -d -m 0700 -o giles -g giles /data

# Plugin discovery finds the plugins on `$PATH` (PLAN section 7.3), so the data root stays pure data.
COPY --from=build /out/ /usr/local/bin/
# The repository is MIT OR Apache-2.0, except plugins/giles-channel-whatsapp, which is
# AGPL-3.0-only (it links AGPL code): its license text and source offer ship with the binary.
COPY LICENSE-MIT LICENSE-APACHE /usr/share/doc/giles/
COPY plugins/giles-channel-whatsapp/LICENSE-AGPL-3.0 plugins/giles-channel-whatsapp/THIRD-PARTY \
    /usr/share/doc/giles/

LABEL org.opencontainers.image.title="Giles" \
      org.opencontainers.image.description="A personal agent daemon with chat channel plugins." \
      org.opencontainers.image.source="https://github.com/jennings/giles" \
      org.opencontainers.image.licenses="(MIT OR Apache-2.0) AND AGPL-3.0-only"

ENV GILES_HOME=/data
VOLUME ["/data"]
USER giles
WORKDIR /data
# `giles.toml` must set `http_listen = "0.0.0.0:8080"` for this port to be reachable.
EXPOSE 8080

ENTRYPOINT ["/usr/bin/tini", "--", "giles"]
CMD ["serve"]
