# Continuous Integration

GitHub Actions runs `.github/workflows/ci.yml` on every push to `main` and on every pull request.
A newer push to the same pull request cancels the run in progress. Each push to `main` runs in its
own concurrency group, so every `main` commit gets a finished run.

## Jobs

**Rust** runs on `ubuntu-latest` and `macos-latest`, the two platforms Giles supports. Each runner
installs the toolchain pinned in `rust-toolchain.toml` (`rustup toolchain install` with no argument
reads it, so the version is never repeated in the workflow), restores the Cargo cache
(`Swatinem/rust-cache`), and runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

`--locked` fails the build if `Cargo.lock` is out of date instead of silently updating it, and `-D
warnings` turns the workspace lints (see [Cargo Workspace](workspace.md)) into errors.

**Docs** runs on `ubuntu-latest`. It installs prebuilt binaries of pinned versions of `rumdl` (with
the `rvben/rumdl` action) and mdBook (the official release archive from GitHub), then runs:

```sh
rumdl check .
mdbook build Docs
```

`rumdl` reads `.config/rumdl.toml` (100-column lines, reflowed). The tool versions are set once, in
the workflow's `env` block (`RUMDL_VERSION`, `MDBOOK_VERSION`); update them there.

**Protobuf** runs on `ubuntu-latest`. It installs a pinned `buf` (`BUF_VERSION`, with the
`bufbuild/buf-action` action in setup-only mode) and runs, from the repository root:

```sh
buf lint
buf format --diff --exit-code
buf breaking --against ".git#ref=$BASE"
```

`$BASE` is the pull request's base commit, or for a push to `main` the previous tip of `main`
(`github.event.before`), so a push of several commits is checked as a whole. The checkout fetches
full history so the base is available. If the base has no `buf.yaml` (the push that added the first
contracts, or a new branch), there is nothing to compare against and the step only logs a notice.
See [gRPC Protocols](protocols.md) for the rules.

## Running the checks locally

The same commands, from the repository root, reproduce CI on the local platform:

```sh
cargo fmt --all --check \
  && cargo clippy --workspace --all-targets --locked -- -D warnings \
  && cargo test --workspace --locked \
  && rumdl check . \
  && mdbook build Docs \
  && buf lint \
  && buf format --diff --exit-code
```

`rumdl fmt .` fixes most Markdown findings, including reflowing long lines, and `buf format -w`
formats the `.proto` files. Install `rumdl`, `mdbook`, and `buf` at the versions in the workflow so
local results match CI.
