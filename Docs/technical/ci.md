# Continuous Integration

GitHub Actions runs two workflows on every push to `main` and on every pull request:
`.github/workflows/ci.yml` checks the code, and `.github/workflows/docs.yml` checks the
documentation and publishes the book. In both, a newer push to the same pull request cancels the
run in progress, and each push to `main` runs in its own concurrency group, so every `main` commit
gets a finished run.

## CI jobs

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

On Ubuntu the tests run with `GILES_REQUIRE_LANDLOCK=1`: the Linux sandbox tests skip on kernels
without Landlock (like the Apple `container` kernel used locally), and this makes them fail
instead, so CI always runs them (see [Plugin Sandbox](sandbox.md#testing-without-landlock)). A step
before the tests runs `host_probes_run` with `--nocapture`, so the log shows the Landlock ABI the
runner's kernel offers (`host check: Ok(Linux { landlock_abi: N })`), which decides which
ABI-dependent branches the sandbox tests exercise.

**Protobuf** runs on `ubuntu-latest`. It installs a pinned `buf` (`BUF_VERSION`, set in the
workflow's `env` block, with the `bufbuild/buf-action` action in setup-only mode) and runs, from the
repository root:

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

## Docs jobs

**Build** runs on `ubuntu-latest`. It installs prebuilt binaries of pinned versions of `rumdl` (with
the `rvben/rumdl` action) and mdBook (the official release archive from GitHub), then runs:

```sh
rumdl check .
mdbook build Docs
```

`rumdl` reads `.config/rumdl.toml` (100-column lines, reflowed) and lints every Markdown file in the
repository, not only the book, so the workflow is not path-filtered. The tool versions are set once,
in the workflow's `env` block (`RUMDL_VERSION`, `MDBOOK_VERSION`); update them there. On a push to
`main`, the job also uploads `Docs/book/` as the Pages artifact (`actions/upload-pages-artifact`);
pull requests stop after the build.

**Deploy** runs only on pushes to `main`, after Build. It publishes the artifact with
`actions/deploy-pages` to the `github-pages` environment; it is the only job with the `pages: write`
and `id-token: write` permissions (the rest of both workflows get `contents: read`). Deployments
share the `pages` concurrency group without cancelling, so they run one at a time in push order.
The repository's Pages source must be set to "GitHub Actions". The site is served at
<http://jennings.io/giles/>, so `Docs/book.toml` sets `site-url = "/giles/"`, the base path mdBook
needs for its 404 page.

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
