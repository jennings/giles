//! Serving a [`Channel`] as `giles.channel.v1.ChannelService` (PLAN section 8.5).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::{self, BoxStream, StreamExt as _};
use giles_proto::MAX_MESSAGE_SIZE;
use giles_proto::channel::v1::channel_service_server::{
    self, ChannelService, ChannelServiceServer,
};
use giles_proto::channel::v1::{
    ChannelCapabilities, EventsRequest, EventsResponse, SendRequest, SendResponse, events_request,
};
use giles_proto::handshake::MAGIC_COOKIE_ENV;
use giles_proto::plugin::v1::{DescribeResponse, PluginKind, describe_response};
use giles_types::{Channel, ChannelEvent, MediaId, OutputMode, Result, SendTarget};
use serde::de::DeserializeOwned;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::service::Routes;
use tonic::{Request, Response, Status, Streaming};
use tonic_health::ServingStatus;
use tonic_health::server::HealthReporter;

use crate::base::{Grants, Kind, internal};
use crate::blobs::Blobs;
use crate::cli;
use crate::convert::{
    event_to_proto, outbound_from_proto, output_mode_to_proto, status_from_error,
};
use crate::remote::RemoteServer;
use crate::server::{self, Context};
use crate::spool::Spool;

/// A channel plugin: what it reports in `Describe`, and how it builds its [`Channel`].
///
/// Call [`run`](ChannelPlugin::run) from `main`. It parses the plugin's command line: `socket` does
/// everything a launched plugin must do and returns when the plugin should exit, and `serve` runs
/// the plugin for its operator as a remote plugin.
///
/// ```no_run
/// use std::process::ExitCode;
///
/// use giles_plugin_sdk::giles_types::{
///     Channel, ChannelEvent, ChannelInfo, EventId, MessageId, OutboundMessage, OutputMode,
///     Result,
/// };
/// use giles_plugin_sdk::{ChannelPlugin, Configure, async_trait};
/// use futures::stream::{self, BoxStream, StreamExt as _};
///
/// #[derive(serde::Deserialize)]
/// #[serde(deny_unknown_fields)]
/// struct Config {
///     network: String,
/// }
///
/// struct Example {
///     info: ChannelInfo,
/// }
///
/// #[async_trait]
/// impl Channel for Example {
///     fn info(&self) -> &ChannelInfo {
///         &self.info
///     }
///     async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
///         Ok(stream::pending().boxed())
///     }
///     async fn ack(&self, _event: &EventId) -> Result<()> {
///         Ok(())
///     }
///     async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
///         Ok(MessageId::new("1")?)
///     }
/// }
///
/// #[tokio::main]
/// async fn main() -> ExitCode {
///     ChannelPlugin::new("example", env!("CARGO_PKG_VERSION"))
///         .output(OutputMode::Blocks)
///         .run(|ctx: Configure| async move {
///             let config: Config = ctx.parse_config()?;
///             Ok(Example {
///                 info: ChannelInfo {
///                     identity_namespace: format!("example:{}", config.network),
///                     output: OutputMode::Blocks,
///                     can_initiate: false,
///                 },
///             })
///         })
///         .await
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ChannelPlugin {
    name: String,
    version: String,
    output: OutputMode,
    can_initiate: bool,
    requested_grants: Grants,
}

impl ChannelPlugin {
    /// A channel plugin named `name`, the `<name>` in its binary's name `giles-channel-<name>`, at
    /// `version` (usually `env!("CARGO_PKG_VERSION")`). It reports [`OutputMode::Blocks`], can't
    /// initiate conversations, and requests no grants until told otherwise.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            output: OutputMode::Blocks,
            can_initiate: false,
            requested_grants: Grants::default(),
        }
    }

    /// How the channel wants assistant output delivered. Must match the [`Channel::info`] of the
    /// channel `configure` builds.
    pub fn output(mut self, output: OutputMode) -> Self {
        self.output = output;
        self
    }

    /// Whether the channel can start a conversation with an identity
    /// ([`SendTarget::Recipient`]). Must match the [`Channel::info`] of the channel `configure`
    /// builds.
    pub fn can_initiate(mut self, can_initiate: bool) -> Self {
        self.can_initiate = can_initiate;
        self
    }

    /// Sandbox grants to suggest to the operator. Advisory only.
    pub fn requested_grants(mut self, grants: Grants) -> Self {
        self.requested_grants = grants;
        self
    }

    /// Runs the plugin until it should exit, and returns its exit status: return it from `main`.
    ///
    /// The SDK performs the launch handshake (PLAN section 7.4), serves `PluginService`,
    /// `grpc.health.v1.Health`, and `ChannelService` on fd 3, and calls `configure` when the core
    /// sends the instance config. Errors from `configure` go back to the core:
    /// [`Error::InvalidRequest`](giles_types::Error::InvalidRequest) for a bad config, naming the
    /// key. After a successful `Configure`, `configure` isn't called again: a repeated
    /// `Configure` with the same instance name and config gets the same answer, and one with a
    /// different config fails with `FAILED_PRECONDITION`.
    ///
    /// It returns when the core asks the plugin to shut down or goes away (stdin closes), after
    /// giving open calls a few seconds to finish. Run by hand, it explains that it's a Giles plugin
    /// and returns a failure status. Startup failures are printed to stderr.
    ///
    /// Call it first thing in `main`, before anything opens or closes file descriptors. It uses the
    /// ambient tokio runtime, which must enable I/O and timers (`#[tokio::main]` does).
    pub async fn serve<F, Fut, C>(self, configure: F) -> ExitCode
    where
        F: Fn(Configure) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<C>> + Send + 'static,
        C: Channel + 'static,
    {
        let display_name = format!("giles-channel-{}", self.name);
        let configure = configure_fn(configure);
        server::run(
            &display_name,
            Box::new(move |context| {
                let spool = Spool::open(Arc::clone(&context.dirs), context.blobs.clone())?;
                Ok(self.services(configure, spool, context))
            }),
        )
        .await
    }

    /// Runs the plugin as a remote plugin (PLAN section 7.6) on `server`'s listener until its
    /// shutdown signal (`SIGINT` or `SIGTERM` by default), then gives open calls a few seconds to
    /// finish and returns.
    ///
    /// The services and `configure` work as with [`serve`](Self::serve), with the state in the
    /// server's data directory, except for the lifecycle: one core at a time connects, and it may
    /// disconnect and connect again (a restart, a network failure). Each connection calls
    /// `Describe` and `Configure` again; the channel built by the first `Configure` stays, and
    /// events not acked by the last connection are sent again on the next. `Shutdown` only ends
    /// the core's open `Events` stream: the core is disconnecting, and the plugin keeps serving.
    /// To change the instance's config, restart the plugin.
    ///
    /// Unlike [`serve`](Self::serve), it doesn't need to run first in `main`, and several can
    /// run in one process (each with its own data directory).
    ///
    /// # Errors
    ///
    /// The data directory or the SDK's state in it can't be opened, or the gRPC server failed.
    pub async fn serve_remote<F, Fut, C>(self, server: RemoteServer, configure: F) -> io::Result<()>
    where
        F: Fn(Configure) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<C>> + Send + 'static,
        C: Channel + 'static,
    {
        let configure = configure_fn(configure);
        server::run_remote(
            server,
            Box::new(move |context| {
                let spool = Spool::open(Arc::clone(&context.dirs), context.blobs.clone())?;
                Ok(self.services(configure, spool, context))
            }),
        )
        .await
    }

    /// Runs the plugin as its command line says, and returns its exit status: return it from
    /// `main`. This is what a shipped plugin's `main` calls instead of [`serve`](Self::serve) or
    /// [`serve_remote`](Self::serve_remote).
    ///
    /// - `socket` is how Giles launches the plugin: [`serve`](Self::serve). Run by hand, it
    ///   explains that it's a Giles plugin.
    /// - `serve [options]` is a plugin its operator runs, for a core whose instance has an
    ///   `address`: [`serve_remote`](Self::serve_remote) on a TCP listener, with TLS if the
    ///   options give a certificate, and checking a static bearer token. The options are
    ///   `--listen`, `--data`, `--tls-cert`, `--tls-key`, `--token-file`, and `--no-auth`, or the
    ///   same as keys of a TOML file named by `--config` (a flag wins). The token is the
    ///   `GILES_CHANNEL_<NAME>_TOKEN` environment variable (`<NAME>` is the plugin's name in
    ///   capitals, with `_` for anything but letters and digits) or `--token-file`, a file only
    ///   its owner can access; without either, `serve` refuses to start unless `--no-auth` says
    ///   to accept any caller. It prints `listening on <address>` to stdout once it listens.
    /// - Anything else prints the usage, to stdout for `--help` and to stderr for a mistake (and
    ///   exits with status 2).
    ///
    /// These options are transport only. The plugin's own settings reach `configure` in both
    /// modes, from the instance's `config` in the core's `giles.toml`; `serve` has no file of its
    /// own for them.
    ///
    /// As with [`serve`](Self::serve), call it first thing in `main`, with tokio's runtime
    /// enabling I/O and timers.
    pub async fn run<F, Fut, C>(self, configure: F) -> ExitCode
    where
        F: Fn(Configure) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<C>> + Send + 'static,
        C: Channel + 'static,
    {
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        self.run_with(&args, &|name| std::env::var_os(name), configure)
            .await
    }

    async fn run_with<F, Fut, C>(
        self,
        args: &[OsString],
        env: &dyn Fn(&str) -> Option<OsString>,
        configure: F,
    ) -> ExitCode
    where
        F: Fn(Configure) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<C>> + Send + 'static,
        C: Channel + 'static,
    {
        let binary = format!("giles-channel-{}", self.name);
        match cli::parse(args) {
            Ok(cli::Command::Socket) => self.serve(configure).await,
            Ok(cli::Command::Serve(serve)) => {
                let result = match serve.resolve(&self.name, env) {
                    Ok(settings) => self.serve_settings(settings, configure).await,
                    Err(e) => Err(e.into()),
                };
                match result {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("{binary}: {e}");
                        ExitCode::from(e.exit_code())
                    }
                }
            }
            Ok(cli::Command::Help) => {
                print!("{}", cli::usage(&binary, &self.name));
                ExitCode::SUCCESS
            }
            // A core from before the subcommands starts a plugin with no arguments.
            Err(cli::CliError::NoCommand) if env(MAGIC_COOKIE_ENV).is_some() => {
                eprintln!(
                    "{binary}: Giles launched it without the `socket` argument, so the Giles \
                     that started it is older than this plugin; upgrade Giles"
                );
                ExitCode::from(1)
            }
            Err(cli::CliError::NoCommand) => {
                eprint!("{}", cli::usage(&binary, &self.name));
                ExitCode::from(2)
            }
            Err(e) => {
                eprintln!("{binary}: {e}\nRun `{binary} --help` for usage.");
                ExitCode::from(2)
            }
        }
    }

    /// The `serve` subcommand once its settings are known.
    async fn serve_settings<F, Fut, C>(
        self,
        settings: cli::Settings,
        configure: F,
    ) -> Result<(), cli::RunError>
    where
        F: Fn(Configure) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<C>> + Send + 'static,
        C: Channel + 'static,
    {
        let listener = tokio::net::TcpListener::bind(&settings.listen)
            .await
            .map_err(|source| cli::RunError::Listen {
                address: settings.listen.clone(),
                source,
            })?;
        let server = RemoteServer::new(listener, settings.data, settings.auth);
        #[cfg(feature = "tls")]
        let server = match settings.tls {
            Some((cert, key)) => server.tls(cli::tls_config(&cert, &key)?),
            None => server,
        };
        {
            use std::io::Write as _;
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "listening on {}", server.local_addr()?)?;
            stdout.flush()?;
        }
        Ok(self.serve_remote(server, configure).await?)
    }

    fn services(
        self,
        configure: Box<ConfigureFn>,
        spool: Spool,
        context: &Context,
    ) -> (Arc<dyn Kind>, Routes) {
        let shared = Arc::new(Shared {
            meta: self,
            configure,
            data_dir: context.data_dir.clone(),
            blobs: context.blobs.clone(),
            spool: Arc::new(spool),
            health: context.health.clone(),
            shutdown: context.shutdown.clone(),
            configure_lock: tokio::sync::Mutex::new(()),
            configured: OnceLock::new(),
            pump: tokio::sync::Mutex::new(None),
            stream: Mutex::new(CancellationToken::new()),
        });
        let service = ChannelServiceServer::new(ChannelServer(Arc::clone(&shared)))
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);
        (Arc::new(ChannelKind(shared)), Routes::new(service))
    }
}

/// What `configure` gets: the instance config and the plugin's resources.
#[derive(Debug)]
pub struct Configure {
    instance: String,
    config: serde_json::Value,
    data_dir: PathBuf,
    blobs: Blobs,
}

impl Configure {
    /// The instance name, e.g. `libera` for `[channel.libera]`. For logs and messages only: the
    /// plugin's state belongs in [`data_dir`](Configure::data_dir), which already is per instance.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The instance's `config` table from `giles.toml`, as JSON, with secrets resolved.
    pub fn config(&self) -> &serde_json::Value {
        &self.config
    }

    /// Deserializes [`config`](Configure::config). A mismatch is
    /// [`Error::InvalidRequest`](giles_types::Error::InvalidRequest) naming the offending key, e.g.
    /// `invalid config: tls.verify: invalid type: string "yes", expected a boolean`; the core shows
    /// it to the operator. Use `#[serde(deny_unknown_fields)]` to catch misspelled keys.
    pub fn parse_config<T: DeserializeOwned>(&self) -> Result<T> {
        serde_path_to_error::deserialize(&self.config).map_err(|e| {
            let path = e.path().to_string();
            let error = e.into_inner();
            giles_types::Error::InvalidRequest(if path == "." {
                format!("invalid config: {error}")
            } else {
                format!("invalid config: {path}: {error}")
            })
        })
    }

    /// The instance's data directory: the plugin's only writable directory, kept across restarts
    /// and upgrades (`GILES_PLUGIN_DATA` for a launched plugin, the [`RemoteServer`]'s for a
    /// remote one). The SDK's own state is in its `giles-sdk/` subdirectory; leave that alone.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The plugin's blob store, for attachments in both directions.
    pub fn blobs(&self) -> &Blobs {
        &self.blobs
    }
}

type ConfigureFn = dyn Fn(Configure) -> BoxFuture<'static, Result<Arc<dyn Channel>>> + Send + Sync;

/// Boxes an author's `configure`.
fn configure_fn<F, Fut, C>(configure: F) -> Box<ConfigureFn>
where
    F: Fn(Configure) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<C>> + Send + 'static,
    C: Channel + 'static,
{
    Box::new(move |context| {
        let built = configure(context);
        Box::pin(async move { Ok(Arc::new(built.await?) as Arc<dyn Channel>) })
    })
}

/// The result of the successful `Configure`.
struct Configured {
    instance: String,
    config: serde_json::Value,
    namespace: String,
    channel: Arc<dyn Channel>,
}

/// State shared by the `Describe`/`Configure` half and `ChannelService`.
struct Shared {
    meta: ChannelPlugin,
    configure: Box<ConfigureFn>,
    data_dir: PathBuf,
    blobs: Blobs,
    spool: Arc<Spool>,
    health: HealthReporter,
    shutdown: CancellationToken,
    /// Serializes `Configure` calls.
    configure_lock: tokio::sync::Mutex<()>,
    configured: OnceLock<Configured>,
    /// Moves events from the channel into the spool; started by the first `Events` call.
    pump: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// Cancels the current `Events` stream when a newer one opens.
    stream: Mutex<CancellationToken>,
}

impl Shared {
    fn channel(&self) -> Result<Arc<dyn Channel>, Status> {
        self.configured
            .get()
            .map(|configured| Arc::clone(&configured.channel))
            .ok_or_else(|| Status::failed_precondition("the plugin isn't configured yet"))
    }

    /// Starts the pump unless it's running: on the first `Events` call, and again after the
    /// channel's stream ended.
    async fn ensure_pump(&self, channel: &Arc<dyn Channel>) -> Result<(), Status> {
        let mut pump = self.pump.lock().await;
        if pump.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let events = channel.events().await.map_err(status_from_error)?;
        *pump = Some(tokio::spawn(pump_events(
            events,
            Arc::clone(&self.spool),
            self.shutdown.clone(),
        )));
        Ok(())
    }

    /// A token for a new `Events` stream, cancelling the previous stream's.
    fn new_stream(&self) -> CancellationToken {
        let token = self.shutdown.child_token();
        let previous = std::mem::replace(
            &mut *self.stream.lock().expect("never poisoned"),
            token.clone(),
        );
        previous.cancel();
        token
    }
}

/// The first retry delay after a failure in the pump; it doubles up to [`MAX_RETRY_DELAY`].
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(100);
/// The longest retry delay in the pump.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);

/// The delay after `previous` (none: the first failure in a row).
fn next_delay(previous: Option<Duration>) -> Duration {
    previous.map_or(FIRST_RETRY_DELAY, |delay| (delay * 2).min(MAX_RETRY_DELAY))
}

/// The core's rule for the identity namespace of channel `name` (PLAN section 3.2): `name` itself
/// or `name:...`, every `:`-separated part non-empty and free of whitespace and control
/// characters. Checked here too so a plugin bug shows up at `Configure`, naming the plugin.
fn namespace_is_valid(name: &str, namespace: &str) -> bool {
    let owned = namespace
        .strip_prefix(name)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'));
    let well_formed = namespace
        .split(':')
        .all(|part| !part.is_empty() && !part.chars().any(|c| c.is_whitespace() || c.is_control()));
    owned && well_formed && !name.is_empty()
}

/// Moves events from the channel's stream into the spool, persisting each one before polling the
/// stream for the next. So a channel whose stream is polled again knows the previous event is on
/// disk.
async fn pump_events(
    mut events: BoxStream<'static, Result<ChannelEvent>>,
    spool: Arc<Spool>,
    shutdown: CancellationToken,
) {
    // Backs off while the stream keeps yielding errors, so a broken source doesn't spin.
    let mut error_delay = None;
    loop {
        let next = tokio::select! {
            () = shutdown.cancelled() => return,
            next = events.next() => next,
        };
        let event = match next {
            None => {
                tracing::warn!(
                    "the channel's event stream ended; the next Events call restarts it"
                );
                return;
            }
            Some(Err(e)) => {
                let delay = next_delay(error_delay);
                error_delay = Some(delay);
                tracing::warn!("the channel's event stream: {e}; polling again in {delay:?}");
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                continue;
            }
            Some(Ok(event)) => event,
        };
        error_delay = None;
        let id = event.id().clone();
        let attachments: Vec<MediaId> = match &event {
            ChannelEvent::Message { message, .. } => {
                message.attachments.iter().map(|m| m.id.clone()).collect()
            }
            _ => Vec::new(),
        };
        let wire = match event_to_proto(event) {
            Ok(wire) => wire,
            Err(e) => {
                tracing::error!(%id, "dropping an event the protocol can't carry: {e}");
                if let Err(e) = spool.blobs().abandon(attachments).await {
                    tracing::warn!(%id, "deleting a dropped event's blobs: {e}");
                }
                continue;
            }
        };
        let mut persist_delay = None;
        loop {
            match spool.push(wire.clone()).await {
                Ok(true) => break,
                Ok(false) => {
                    tracing::debug!(%id, "the event is already pending; ignoring the repeat");
                    break;
                }
                Err(e) => {
                    let delay = next_delay(persist_delay);
                    persist_delay = Some(delay);
                    tracing::error!(%id, "persisting an event: {e}; retrying in {delay:?}");
                    tokio::select! {
                        () = shutdown.cancelled() => return,
                        () = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }
}

/// The `Describe` and `Configure` half.
struct ChannelKind(Arc<Shared>);

#[async_trait]
impl Kind for ChannelKind {
    fn service_name(&self) -> &'static str {
        channel_service_server::SERVICE_NAME
    }

    fn describe(&self) -> DescribeResponse {
        let meta = &self.0.meta;
        DescribeResponse {
            kind: PluginKind::Channel.into(),
            name: meta.name.clone(),
            version: meta.version.clone(),
            requested_grants: Some(meta.requested_grants.clone().into()),
            capabilities: Some(describe_response::Capabilities::Channel(
                ChannelCapabilities {
                    output: output_mode_to_proto(meta.output)
                        .expect("the SDK knows every output mode it can build with")
                        .into(),
                    can_initiate: meta.can_initiate,
                },
            )),
        }
    }

    async fn configure(
        &self,
        instance: String,
        config: serde_json::Value,
    ) -> Result<String, Status> {
        let shared = &self.0;
        let _serialized = shared.configure_lock.lock().await;
        if let Some(configured) = shared.configured.get() {
            // A remote plugin's core reconnecting configures it again.
            return if configured.instance == instance && configured.config == config {
                Ok(configured.namespace.clone())
            } else {
                Err(Status::failed_precondition(
                    "the plugin is already configured with another instance name or config; \
                     restart it to change them",
                ))
            };
        }
        tracing::info!(%instance, "configuring");
        let context = Configure {
            instance: instance.clone(),
            config: config.clone(),
            data_dir: shared.data_dir.clone(),
            blobs: shared.blobs.clone(),
        };
        let channel = (shared.configure)(context)
            .await
            .map_err(status_from_error)?;
        let info = channel.info();
        let meta = &shared.meta;
        if info.output != meta.output || info.can_initiate != meta.can_initiate {
            return Err(Status::internal(format!(
                "plugin bug: Channel::info() says output {:?}, can_initiate {}; ChannelPlugin \
                 says output {:?}, can_initiate {}",
                info.output, info.can_initiate, meta.output, meta.can_initiate
            )));
        }
        if !namespace_is_valid(&meta.name, &info.identity_namespace) {
            return Err(Status::internal(format!(
                "plugin bug: identity namespace {:?} must be {:?} or start with \"{}:\", with \
                 non-empty `:`-separated parts free of whitespace and control characters",
                info.identity_namespace, meta.name, meta.name
            )));
        }
        let namespace = info.identity_namespace.clone();
        let _ = shared.configured.set(Configured {
            instance,
            config,
            namespace: namespace.clone(),
            channel,
        });
        shared
            .health
            .set_service_status(channel_service_server::SERVICE_NAME, ServingStatus::Serving)
            .await;
        Ok(namespace)
    }

    fn core_detached(&self) {
        self.0.stream.lock().expect("never poisoned").cancel();
    }
}

/// `ChannelService`.
struct ChannelServer(Arc<Shared>);

#[async_trait]
impl ChannelService for ChannelServer {
    type EventsStream = BoxStream<'static, Result<EventsResponse, Status>>;

    async fn events(
        &self,
        request: Request<Streaming<EventsRequest>>,
    ) -> Result<Response<Self::EventsStream>, Status> {
        let shared = &self.0;
        let channel = shared.channel()?;
        shared.ensure_pump(&channel).await?;
        let stream = shared.new_stream();
        tokio::spawn(read_acks(
            request.into_inner(),
            Arc::clone(&shared.spool),
            channel,
            stream.clone(),
        ));
        Ok(Response::new(pending_events(
            Arc::clone(&shared.spool),
            stream,
        )))
    }

    async fn send(&self, request: Request<SendRequest>) -> Result<Response<SendResponse>, Status> {
        let shared = &self.0;
        let channel = shared.channel()?;
        let message = outbound_from_proto(request.into_inner()).map_err(status_from_error)?;
        // The core's blobs are valid until this RPC ends, however it ends; then it releases its
        // reference to each distinct one (another Send may share the bytes).
        let mut ids: Vec<MediaId> = message.attachments.iter().map(|m| m.id.clone()).collect();
        ids.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        ids.dedup();
        let _outbound = OutboundBlobs {
            blobs: shared.blobs.clone(),
            ids,
        };
        for media in &message.attachments {
            if !shared.blobs.exists(&media.id).await.map_err(internal)? {
                return Err(Status::invalid_argument(format!(
                    "unknown blob handle {}",
                    media.id
                )));
            }
        }
        if matches!(message.target, SendTarget::Recipient(_)) && !shared.meta.can_initiate {
            return Err(Status::invalid_argument(
                "this channel can't start conversations (send to a conversation_id instead)",
            ));
        }
        let message_id = channel.send(message).await.map_err(status_from_error)?;
        Ok(Response::new(SendResponse {
            message_id: message_id.to_string(),
        }))
    }
}

/// Deletes the core's blobs used by a `Send` when dropped.
struct OutboundBlobs {
    blobs: Blobs,
    ids: Vec<MediaId>,
}

impl Drop for OutboundBlobs {
    fn drop(&mut self) {
        self.blobs.drop_outbound(std::mem::take(&mut self.ids));
    }
}

/// Every pending event in order, starting with the ones left unacked by earlier streams and
/// processes, then new ones as they're persisted, until `stop`.
fn pending_events(
    spool: Arc<Spool>,
    stop: CancellationToken,
) -> BoxStream<'static, Result<EventsResponse, Status>> {
    struct State {
        spool: Arc<Spool>,
        pushed: tokio::sync::watch::Receiver<u64>,
        sent_up_to: u64,
        queue: VecDeque<(u64, giles_proto::channel::v1::ChannelEvent)>,
        stop: CancellationToken,
    }
    let state = State {
        pushed: spool.subscribe(),
        spool,
        sent_up_to: 0,
        queue: VecDeque::new(),
        stop,
    };
    stream::unfold(state, |mut state| async move {
        loop {
            if let Some((seq, event)) = state.queue.pop_front() {
                state.sent_up_to = seq;
                let response = EventsResponse { event: Some(event) };
                return Some((Ok(response), state));
            }
            // Mark the current value seen before reading, so a push in between still wakes us.
            state.pushed.borrow_and_update();
            state.queue.extend(state.spool.after(state.sent_up_to));
            if !state.queue.is_empty() {
                continue;
            }
            tokio::select! {
                () = state.stop.cancelled() => return None,
                changed = state.pushed.changed() => changed.ok()?,
            }
        }
    })
    .boxed()
}

/// Applies the core's acks: forgets the event, then tells the channel.
async fn read_acks(
    mut requests: Streaming<EventsRequest>,
    spool: Arc<Spool>,
    channel: Arc<dyn Channel>,
    stop: CancellationToken,
) {
    loop {
        let request = tokio::select! {
            () = stop.cancelled() => return,
            request = requests.message() => request,
        };
        let ack = match request {
            Ok(Some(EventsRequest {
                request: Some(events_request::Request::Ack(ack)),
            })) => ack,
            Ok(Some(_)) => continue,
            Ok(None) => return,
            Err(status) => {
                tracing::debug!("the Events stream from the core ended: {status}");
                return;
            }
        };
        match spool.ack(&ack.event_id).await {
            Ok(false) => tracing::debug!(event_id = ack.event_id, "ack for an unknown event"),
            Ok(true) => {
                let Ok(id) = giles_types::EventId::new(ack.event_id) else {
                    continue;
                };
                if let Err(e) = channel.ack(&id).await {
                    tracing::warn!(%id, "the channel failed to take an ack: {e}");
                }
            }
            Err(e) => tracing::error!(event_id = ack.event_id, "forgetting an acked event: {e}"),
        }
    }
}

#[cfg(test)]
mod tests;
