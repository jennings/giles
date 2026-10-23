//! `giles serve`: the daemon.

use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use giles_core::admin::{AdminListener, AdminOptions, AdminService, InstanceLock, PluginControl};
use giles_core::agent_loop::AgentLoop;
use giles_core::approvals::{ApprovalBoard, ChannelPresenter};
use giles_core::config::Config;
use giles_core::http;
use giles_core::paths::DataRoot;
use giles_core::plugin_host::{ChannelWiring, Launcher, PluginHost};
use giles_core::runtime::Runtime;
use giles_core::sandbox::{self, HostReport};
use giles_core::sessions::SystemClock;
use giles_core::toolsets::{ToolsetHost, ToolsetLauncher};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::cli::ServeArgs;
use crate::shutdown;

/// Resolves and creates the data root, takes its single-instance lock, loads the configuration,
/// then runs the daemon until SIGINT or SIGTERM. A second daemon on the same data root and an
/// invalid configuration fail before anything starts.
pub fn run(home: Option<&Path>, _args: &ServeArgs) -> anyhow::Result<()> {
    let root = DataRoot::from_env(home)?;
    root.create_layout()?;
    let lock = InstanceLock::acquire(&root)?;
    let config = Config::load(&root)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(serve(root, config, lock))
}

async fn serve(root: DataRoot, config: Config, lock: InstanceLock) -> anyhow::Result<()> {
    let started_at = jiff::Timestamp::now();
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
    // Every toolset, registered now with its cached tools (so resumed turns see them) and started
    // in the background.
    let toolsets = ToolsetHost::start(
        ToolsetLauncher::new(root.clone(), giles_exe.clone(), sandbox.clone().map(drop)),
        &runtime.registry,
    )
    .await;
    runtime.registry.log_summary();

    let listener = http::bind(config.core.http_listen).await?;
    let http_addr = listener
        .local_addr()
        .context("cannot read the HTTP listener's address")?;
    // Login links point at the address actually bound (`http_listen` may say port 0).
    #[cfg(feature = "channel-web")]
    if let Some(web) = runtime.registry.web_channel() {
        web.set_listen_addr(http_addr);
    }
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
    // A fresh admin token and the admin socket (a stale one is safe to replace: we hold the lock).
    let admin_listener = AdminListener::bind(&root, &lock)?;
    // Startup recovery, then every registered channel's events. The last step that can fail:
    // recovery changes state and resumed turns run, so nothing may abort startup after it.
    // Approval prompts get buttons in web chats and are sent as text everywhere else.
    let approvals = Arc::new(ApprovalBoard::new(Arc::new(ChannelPresenter::new(
        runtime.registry.clone(),
    ))));
    let sessions = runtime
        .start_sessions(
            Arc::new(AgentLoop::from_runtime(&runtime, approvals.clone())),
            approvals,
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
    // Every configured plugin channel, supervised: registered and attached to the sessions as it
    // comes up, relaunched with backoff when it fails. Startup waits for each first attempt and
    // for every toolset's first start (a failed toolset retries in the background); a shutdown
    // during startup stops waiting, and `stop_all` abandons the launches still running (killing
    // their plugins).
    let wiring = ChannelWiring {
        registry: runtime.registry.clone(),
        sessions: sessions.clone(),
        blobs: runtime.blobs.clone(),
    };
    let plugins = Arc::new(PluginHost::new(
        Launcher::new(root.clone(), giles_exe, sandbox.clone().map(drop)),
        wiring,
    ));
    // Login links come from the built-in web channel, if one is configured.
    #[cfg(feature = "channel-web")]
    let login_links = runtime
        .registry
        .web_channel()
        .map(|web| web as Arc<dyn giles_core::admin::LoginLinks>);
    #[cfg(not(feature = "channel-web"))]
    let login_links = None;
    let admin = AdminService::new(AdminOptions {
        config: config.clone(),
        store: runtime.store.clone(),
        sandbox,
        started_at,
        channels: Arc::new(sessions.clone()),
        plugins: Some(plugins.clone() as Arc<dyn PluginControl>),
        login_links,
        clock: Arc::new(SystemClock),
    });
    let admin_server = tokio::spawn(admin_listener.serve(admin, shutdown.clone()));
    tokio::select! {
        _ = async { tokio::join!(plugins.start(&config), toolsets.started()) } => {}
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
        plugins.stop_all().await;
        toolsets.stop().await;
        stop_admin(admin_server).await;
        result.context("the HTTP server panicked")??;
        anyhow::bail!("the HTTP server stopped unexpectedly");
    }
    http_server.await.context("the HTTP server panicked")??;
    // Running turns are cancelled with the token; wait for them and the channel pumps, then stop
    // the plugins and their supervision, and the toolsets.
    sessions.closed().await;
    plugins.stop_all().await;
    toolsets.stop().await;
    stop_admin(admin_server).await;
    info!("giles stopped");
    // Held until here, so a new core can't start while this one still runs plugins.
    drop(lock);
    Ok(())
}

/// Waits for the admin server, which stops with the shutdown token (and removes its socket and
/// token file). A failure only gets logged: the core is stopping anyway.
async fn stop_admin(server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>) {
    match server.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => warn!(error = %err, "the admin server failed"),
        Err(err) => warn!(error = %err, "the admin server panicked"),
    }
}

/// Checks and logs whether this host can sandbox plugins (PLAN section 12.1). Only a warning:
/// unsandboxed instances still start, and the plugin host refuses each sandboxed one with this
/// error. The admin service's `Status` reports the result.
fn check_sandbox() -> Result<HostReport, String> {
    match sandbox::check_host() {
        Ok(report) => {
            info!(%report, "plugin sandbox available");
            Ok(report)
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
