//! `giles serve`: the daemon.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use giles_core::agent_loop::AgentLoop;
use giles_core::commands::NoPendingApprovals;
use giles_core::config::Config;
use giles_core::http;
use giles_core::paths::DataRoot;
use giles_core::plugin_host::{Launcher, PluginHost, STOP_GRACE};
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
    let sandbox = check_sandbox();
    // The sandbox trampoline is this binary, run as `giles sandbox-exec`.
    let giles_exe = std::env::current_exe().context("cannot find the giles binary")?;
    // The database, the blob store, and every component the configuration names.
    let config = Arc::new(config);
    let runtime = Runtime::start(root.clone(), config.clone()).await?;

    let listener = http::bind(config.core.http_listen).await?;
    let http_addr = listener
        .local_addr()
        .context("cannot read the HTTP listener's address")?;
    // The web channel's pages and its trusted-proxy listener, if `giles.toml` configures them.
    #[cfg(feature = "channel-web")]
    let web = giles_core::channels::web::WebApp::from_runtime(&runtime, shutdown.clone());
    #[cfg(feature = "channel-web")]
    let proxy = match &web {
        Some(web) => match web.bind_proxy(&root).await? {
            Some(listener) => {
                info!(at = %listener.describe(), "trusted-proxy listener open");
                Some((listener, web.clone()))
            }
            None => None,
        },
        None => None,
    };
    // Startup recovery, then every registered channel's events. The last step that can fail:
    // recovery changes state and resumed turns run, so nothing may abort startup after it.
    let sessions = runtime
        .start_sessions(
            Arc::new(AgentLoop::from_runtime(&runtime)),
            Arc::new(NoPendingApprovals),
            shutdown.clone(),
        )
        .await?;
    let builder = http::RouterBuilder::new();
    #[cfg(feature = "channel-web")]
    let builder = match &web {
        Some(web) => web.install(builder),
        None => builder,
    };
    let app = builder.build();
    // One task serves every listener. It returns when all have stopped (on shutdown), or at the
    // first failure.
    #[cfg(feature = "channel-web")]
    let proxy_server = {
        let (app, shutdown) = (app.clone(), shutdown.clone());
        async move {
            match proxy {
                Some((listener, web)) => listener
                    .serve(web.proxy_app(app), shutdown)
                    .await
                    .context("the trusted-proxy listener failed"),
                None => Ok(()),
            }
        }
    };
    #[cfg(not(feature = "channel-web"))]
    let proxy_server = async { anyhow::Ok(()) };
    let main_server = {
        let shutdown = shutdown.clone();
        async move {
            http::serve(listener, app, shutdown)
                .await
                .context("the HTTP server failed")
        }
    };
    let mut http_server =
        tokio::spawn(async move { tokio::try_join!(main_server, proxy_server).map(|_| ()) });
    // Every configured plugin channel; one that fails to launch is logged and stays pending. A
    // shutdown during startup abandons the launches still running (killing their plugins).
    let plugins = PluginHost::new(Launcher::new(root.clone(), giles_exe, sandbox));
    tokio::select! {
        () = plugins.launch_all(&config) => {}
        () = shutdown.cancelled() => {}
    }

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
        plugins.stop_all(STOP_GRACE).await;
        result.context("the HTTP server panicked")??;
        anyhow::bail!("the HTTP server stopped unexpectedly");
    }
    http_server.await.context("the HTTP server panicked")??;
    // Running turns are cancelled with the token; wait for them and the channel pumps, then stop
    // the plugins.
    sessions.closed().await;
    plugins.stop_all(STOP_GRACE).await;
    info!("giles stopped");
    Ok(())
}

/// Checks and logs whether this host can sandbox plugins (PLAN section 12.1). Only a warning:
/// unsandboxed instances still start, and the plugin host refuses each sandboxed one with this
/// error.
fn check_sandbox() -> Result<(), String> {
    match sandbox::check_host() {
        Ok(report) => {
            info!(%report, "plugin sandbox available");
            Ok(())
        }
        Err(err) => {
            warn!(
                error = %err,
                "plugin sandbox unavailable on this host; sandboxed plugins will not start",
            );
            Err(err.to_string())
        }
    }
}
