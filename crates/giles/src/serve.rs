//! `giles serve`: the daemon.

use std::path::Path;

use anyhow::Context;
use giles_core::paths::DataRoot;
use giles_core::sandbox;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::cli::ServeArgs;
use crate::shutdown;

/// Resolves and creates the data root, then runs the daemon until SIGINT or SIGTERM.
pub fn run(home: Option<&Path>, _args: &ServeArgs) -> anyhow::Result<()> {
    let root = DataRoot::from_env(home)?;
    root.create_layout()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(serve(root))
}

async fn serve(root: DataRoot) -> anyhow::Result<()> {
    let shutdown = CancellationToken::new();
    shutdown::cancel_on_signal(shutdown.clone()).context("cannot install signal handlers")?;

    if root.uses_socket_fallback() {
        warn!(
            socket_dir = %root.socket_dir().display(),
            "the data root path is too long for Unix sockets; they go in a private temporary \
             directory instead",
        );
    }
    check_sandbox();
    info!(
        version = env!("CARGO_PKG_VERSION"),
        root = %root.root().display(),
        admin_socket = %root.admin_socket().display(),
        "giles started",
    );

    shutdown.cancelled().await;
    info!("giles stopped");
    Ok(())
}

/// Logs whether this host can sandbox plugins (PLAN section 12.1). Only a warning for now: no
/// plugin is launched yet. The plugin host (TASKS.md 2.16) turns a failure into one clear error
/// when a configured instance needs the sandbox.
fn check_sandbox() {
    match sandbox::check_host() {
        Ok(report) => info!(%report, "plugin sandbox available"),
        Err(err) => warn!(
            error = %err,
            "plugin sandbox unavailable on this host; sandboxed plugins will not start",
        ),
    }
}
