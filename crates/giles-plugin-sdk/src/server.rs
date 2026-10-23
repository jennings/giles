//! The plugin process from start to exit: for a launched plugin, the startup checks, the gRPC
//! server on fd 3, the handshake line, and the two ways it ends (PLAN sections 7.4 and 7.5); for a
//! remote plugin, the gRPC server on its TCP listener until its operator stops it (section 7.6).

use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use futures::stream::{self, StreamExt as _};
use giles_proto::MAX_MESSAGE_SIZE;
use giles_proto::handshake::{PLUGIN_FD, PROTOCOLS_ENV};
use giles_proto::plugin::v1::plugin_service_server::{self, PluginServiceServer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;
use tonic::service::{InterceptorLayer, Routes};
use tonic::transport::Server;
use tonic::transport::server::Connected;
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

use crate::auth::RequireBearer;
use crate::base::{Base, Kind, OnShutdown};
use crate::blobs::Blobs;
use crate::dirs::SdkDirs;
use crate::remote::RemoteServer;
use crate::startup::{
    SUPPORTED_PROTOCOLS, StartupError, adopt_socket, check_magic, choose_protocol, data_dir,
    watch_stdin, write_handshake,
};

/// How long in-flight calls get to finish once the plugin stops. Long-lived streams (a health
/// `Watch`, a channel's `Events`) would otherwise hold a graceful shutdown open indefinitely.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// What a kind needs from the process to build its services.
pub(crate) struct Context {
    pub(crate) data_dir: PathBuf,
    pub(crate) dirs: Arc<SdkDirs>,
    pub(crate) blobs: Blobs,
    pub(crate) health: HealthReporter,
    /// Cancelled when the plugin starts shutting down.
    pub(crate) shutdown: CancellationToken,
}

/// Builds a kind and its service once the plugin's state is open.
pub(crate) type Build<'a> =
    Box<dyn FnOnce(&Context) -> io::Result<(Arc<dyn Kind>, Routes)> + Send + 'a>;

/// Runs a launched plugin process to the end and returns its exit status. `display_name` is the
/// binary's name, for messages; `build` makes the kind and its service once the startup checks
/// passed.
pub(crate) async fn run(display_name: &str, build: Build<'_>) -> ExitCode {
    match serve(build).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(StartupError::NotLaunchedByGiles) => {
            eprintln!(
                "{display_name} is a Giles plugin. Giles launches it for an instance configured in \
                 giles.toml; it isn't meant to be run by hand."
            );
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("{display_name}: {e}");
            ExitCode::from(e.exit_code())
        }
    }
}

async fn serve(build: Build<'_>) -> Result<(), StartupError> {
    let env = |name: &str| std::env::var_os(name);
    check_magic(&env)?;
    let protocol = choose_protocol(env(PROTOCOLS_ENV), SUPPORTED_PROTOCOLS)?;
    let data_dir = data_dir(&env)?;
    let socket = adopt_socket(PLUGIN_FD).map_err(StartupError::PluginFd)?;
    let (token, stdin_closed) = watch_stdin(io::BufReader::new(io::stdin()));
    let token = token
        .await
        .map_err(|_| StartupError::Token("the stdin thread ended".into()))??;

    let services = Services::open(data_dir, OnShutdown::Exit, build).await?;
    let io = tokio::net::UnixStream::from_std(socket).map_err(StartupError::PluginFd)?;
    // One connection, then a stream that stays open: the end of `incoming` would end the server.
    let incoming = stream::iter([Ok::<_, io::Error>(io)]).chain(stream::pending());
    services
        .serve(
            RequireBearer::new(&token),
            incoming,
            || write_handshake(&mut io::stdout().lock(), protocol),
            async move {
                stdin_closed.cancelled().await;
                tracing::info!("stdin closed: the core is gone; exiting");
            },
        )
        .await
}

/// Runs a remote plugin until its shutdown signal, then drains.
pub(crate) async fn run_remote(server: RemoteServer, build: Build<'_>) -> io::Result<()> {
    let parts = server.into_parts();
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&parts.data_dir)?;
    let services = Services::open(parts.data_dir, OnShutdown::Detach, build)
        .await
        .map_err(io::Error::other)?;
    let shutdown = parts.shutdown;
    services
        .serve(parts.check, parts.incoming, || Ok(()), shutdown)
        .await
        .map_err(io::Error::other)
}

/// The plugin's services, built and ready to serve.
struct Services {
    context: Context,
    routes: Routes,
}

impl Services {
    /// Opens the SDK's state in `data_dir`, builds the kind, and adds `PluginService` and health
    /// (`PluginService` serving, the kind's service not yet).
    async fn open(
        data_dir: PathBuf,
        on_shutdown: OnShutdown,
        build: Build<'_>,
    ) -> Result<Self, StartupError> {
        let dirs = Arc::new(SdkDirs::open(&data_dir).map_err(StartupError::State)?);
        let (health, health_service) = tonic_health::server::health_reporter();
        let context = Context {
            data_dir,
            blobs: Blobs::new(Arc::clone(&dirs)),
            dirs,
            health: health.clone(),
            shutdown: CancellationToken::new(),
        };
        let (kind, routes) = build(&context).map_err(StartupError::State)?;
        health
            .set_service_status(plugin_service_server::SERVICE_NAME, ServingStatus::Serving)
            .await;
        health
            .set_service_status(kind.service_name(), ServingStatus::NotServing)
            .await;
        let base = Base::new(
            kind,
            context.blobs.clone(),
            on_shutdown,
            context.shutdown.clone(),
        );
        let routes = routes
            .add_service(
                PluginServiceServer::new(base)
                    .max_decoding_message_size(MAX_MESSAGE_SIZE)
                    .max_encoding_message_size(MAX_MESSAGE_SIZE),
            )
            .add_service(
                health_service
                    .max_decoding_message_size(MAX_MESSAGE_SIZE)
                    .max_encoding_message_size(MAX_MESSAGE_SIZE),
            );
        Ok(Self { context, routes })
    }

    /// Serves the connections from `incoming`, every call checked by `check`; calls `started` once
    /// the server runs, and stops when `ended` completes or a `Shutdown` call ends the plugin.
    /// Calls still open then get [`DRAIN_TIMEOUT`].
    async fn serve<IO>(
        self,
        check: RequireBearer,
        incoming: impl Stream<Item = io::Result<IO>> + Send + 'static,
        started: impl FnOnce() -> Result<(), StartupError>,
        ended: impl Future<Output = ()>,
    ) -> Result<(), StartupError>
    where
        IO: AsyncRead + AsyncWrite + Connected + Unpin + Send + 'static,
    {
        let Self { context, routes } = self;
        let stop = CancellationToken::new();
        let mut server = tokio::spawn(
            Server::builder()
                .layer(InterceptorLayer::new(check))
                .add_routes(routes)
                .serve_with_incoming_shutdown(incoming, stop.clone().cancelled_owned()),
        );
        started()?;

        tokio::select! {
            () = ended => {}
            () = context.shutdown.cancelled() => tracing::info!("shutting down"),
            result = &mut server => return Err(serve_error(result)),
        }
        context.shutdown.cancel();
        context
            .health
            .set_service_status("", ServingStatus::NotServing)
            .await;
        stop.cancel();
        match tokio::time::timeout(DRAIN_TIMEOUT, &mut server).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(result) => Err(serve_error(result)),
            Err(_) => {
                tracing::warn!("calls still open after {DRAIN_TIMEOUT:?}; exiting anyway");
                server.abort();
                Ok(())
            }
        }
    }
}

fn serve_error(
    result: Result<Result<(), tonic::transport::Error>, tokio::task::JoinError>,
) -> StartupError {
    StartupError::Serve(match result {
        Ok(Ok(())) => "the server stopped by itself".into(),
        Ok(Err(e)) => e.to_string(),
        Err(e) => e.to_string(),
    })
}
