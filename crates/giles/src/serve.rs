//! `giles serve`: the daemon.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use giles_core::agent_loop::AgentLoop;
use giles_core::commands::NoPendingApprovals;
use giles_core::config::Config;
use giles_core::http;
use giles_core::paths::DataRoot;
use giles_core::runtime::Runtime;
use giles_core::sandbox;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::cli::ServeArgs;
use crate::shutdown;

/// Resolves and creates the data root, loads the configuration, then runs the daemon until SIGINT
/// or SIGTERM. An invalid configuration fails before anything starts.
pub fn run(home: Option<&Path>, _args: &ServeArgs) -> anyhow::Result<()> {
    let root = DataRoot::from_env(home)?;
    root.create_layout()?;
    let config = Config::load(&root)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(serve(root, config))
}

async fn serve(root: DataRoot, config: Config) -> anyhow::Result<()> {
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
    // The database, the blob store, and every component the configuration names.
    let config = Arc::new(config);
    let runtime = Runtime::start(root.clone(), config.clone()).await?;

    let listener = http::bind(config.core.http_listen).await?;
    let http_addr = listener
        .local_addr()
        .context("cannot read the HTTP listener's address")?;
    // Startup recovery, then every registered channel's events. The last step that can fail:
    // recovery changes state and resumed turns run, so nothing may abort startup after it.
    let sessions = runtime
        .start_sessions(
            Arc::new(AgentLoop::from_runtime(&runtime)),
            Arc::new(NoPendingApprovals),
            shutdown.clone(),
        )
        .await?;
    let app = http::RouterBuilder::new().build();
    let mut http_server = tokio::spawn(http::serve(listener, app, shutdown.clone()));

    info!(
        version = env!("CARGO_PKG_VERSION"),
        root = %root.root().display(),
        admin_socket = %root.admin_socket().display(),
        http = %format_args!("http://{http_addr}/"),
        agents = config.agents.len(),
        providers = config.providers.len(),
        channels = config.channels.len(),
        toolsets = config.toolsets.len(),
        "giles started",
    );

    // `biased`: once the token is cancelled the server returns promptly, so both branches can be
    // ready together; that is a normal stop, not an early exit.
    let early_exit = tokio::select! {
        biased;
        () = shutdown.cancelled() => None,
        result = &mut http_server => Some(result),
    };
    if let Some(result) = early_exit {
        // The server only returns on shutdown; stop everything else with it.
        shutdown.cancel();
        result
            .context("the HTTP server panicked")?
            .context("the HTTP server failed")?;
        anyhow::bail!("the HTTP server stopped unexpectedly");
    }
    http_server
        .await
        .context("the HTTP server panicked")?
        .context("the HTTP server failed")?;
    // Running turns are cancelled with the token; wait for them and the channel pumps.
    sessions.closed().await;
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
