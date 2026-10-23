//! The plugin host (PLAN sections 7.3 to 7.6): launching gRPC channel plugins, connecting to
//! remote ones, and connecting them to the core.
//!
//! [`Launcher::launch_channel`] takes one configured plugin channel from discovery to a configured
//! plugin: it finds the binary ([`discover_plugin`]), kills an orphan an earlier core left running
//! ([`orphans`]), prepares the data directory, spawns the plugin through the sandbox trampoline
//! (or directly with `unsandboxed = true`) in its own process group with its end of a socket pair
//! as fd 3 and a per-launch token on stdin, reads the handshake line, connects the gRPC client,
//! and calls `Describe` and `Configure`. The result is a [`LaunchedPlugin`]: the connection, the
//! `Describe` and `Configure` results, and the [`PluginProcess`].
//!
//! [`Launcher::connect_remote`] does the same for a remote plugin channel (one with `address`)
//! that someone else runs: TCP, optionally TLS, a bearer token or OAuth client credentials
//! ([`oauth`]), then `Describe` and `Configure`. The result is a [`RemotePlugin`].
//!
//! [`RemoteChannel`] is a plugin's `ChannelService` as a `giles_types::Channel`. [`PluginHost`]
//! supervises every configured plugin channel ([`supervisor`]): it launches it (or connects to
//! it), registers its `RemoteChannel` with the registry and the session manager
//! ([`ChannelWiring`]), checks its health, relaunches (or reconnects) it with backoff, records a
//! launched plugin's `Describe` answer in the cache ([`DescribeCache`]), and stops it (or
//! disconnects) at shutdown. See `Docs/technical/plugin-host.md`.

mod describe_cache;
mod grpc;
mod oauth;
mod orphans;
mod process;
mod remote;
mod remote_channel;
mod spec;
pub mod supervisor;

use async_trait::async_trait;
use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;
use std::time::{Duration, Instant};

use giles_plugin_sdk::convert::output_mode_from_proto;
use giles_plugin_sdk::tls::rustls::RootCertStore;
use giles_proto::channel::v1::channel_service_server::SERVICE_NAME as CHANNEL_SERVICE;
use giles_proto::handshake::{EXIT_NO_SHARED_PROTOCOL, HandshakeError, HandshakeLine};
use giles_proto::plugin::v1::{
    ConfigureRequest, DescribeRequest, DescribeResponse, PluginKind, SandboxGrants,
    ShutdownRequest, describe_response,
};
use giles_store::BlobStore;
use giles_types::Channel;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use tokio::process::ChildStdout;
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tracing::{Instrument as _, info, warn};

pub use self::describe_cache::{DescribeCache, DescribeInfo, RequestedGrants};
pub use self::grpc::{CallMetadata, PluginConnection, PluginTransport};
pub use self::process::{PluginProcess, STDERR_TARGET};
pub use self::remote::{REMOTE_CONNECT_TIMEOUT, REMOTE_SHUTDOWN_TIMEOUT, RemotePlugin, Secrets};
pub use self::remote_channel::{BLOB_CHUNK_TIMEOUT, RemoteChannel};
use self::spec::{OFFERED_PROTOCOLS, launch_spec};
use self::supervisor::{HEALTH_TIMEOUT, Launch, LaunchFailure, Running, Supervision};
pub use self::supervisor::{PluginState, PluginStatus};
use crate::admin::{PluginControl, PluginRunState, PluginRuntime};
use crate::config::{
    ChannelInstance, ChannelRuntime, Config, InstanceName, Kind, Secret, discover_plugin,
    plugin_binary, scrub,
};
use crate::identity::{NamespaceError, validate_namespace};
use crate::paths::{DataRoot, PathsError};
use crate::registry::Registry;
use crate::sandbox::{self, SpecError, TMP_DIR_NAME};
use crate::sessions::Sessions;

/// How long a plugin has to print its handshake line (PLAN section 7.4).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `Describe` may take.
pub const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `Configure` may take: plugins connect to their service in it (an IRC server,
/// WhatsApp).
pub const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long [`LaunchedPlugin::stop`] waits after `Shutdown` before `SIGTERM` by default: the SDK
/// drains for up to 5 seconds.
pub const STOP_GRACE: Duration = Duration::from_secs(10);

/// How long a plugin gets after `SIGTERM` before `SIGKILL`.
pub const TERM_GRACE: Duration = Duration::from_secs(5);

/// The core's minor version of protocol major 1, sent in `Describe` (PLAN section 7.8).
pub const CORE_PROTOCOL_MINOR: u32 = 0;

/// How long the core waits for a plugin that closed stdout before the handshake line to exit.
const EXIT_AFTER_EOF: Duration = Duration::from_secs(2);

/// How long the core waits for a failed plugin's last stderr lines.
const STDERR_DRAIN: Duration = Duration::from_secs(1);

/// The longest handshake line read; anything longer is malformed.
const MAX_HANDSHAKE_LINE: u64 = 256;

/// A launch (or a remote plugin's connection) failed. Every message is safe to log:
/// plugin-provided text has the instance's secrets and the token scrubbed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LaunchError {
    /// The instance isn't a launched plugin (built-in or remote).
    #[error("the instance is not a launched plugin")]
    NotLaunched,
    /// The instance isn't a remote plugin.
    #[error("the instance is not a remote plugin")]
    NotRemote,
    /// No credentials for a remote plugin: the OAuth token request failed, or the bearer token
    /// can't be sent.
    #[error("cannot authenticate to the plugin: {0}")]
    Auth(String),
    /// The binary is neither in `<root>/plugins/` nor on `PATH`.
    #[error("plugin binary {binary} not found in {} or on PATH", .plugins_dir.display())]
    NotFound {
        /// `giles-<kind>-<component>`.
        binary: String,
        /// `<root>/plugins/`.
        plugins_dir: PathBuf,
    },
    /// The host can't sandbox plugins (the startup check failed) and the instance is sandboxed.
    #[error("the plugin sandbox is unavailable on this host: {0}")]
    SandboxUnavailable(String),
    /// Preparing the launch (pid file, data directory, spec file) failed.
    #[error("cannot {action} {}: {source}", .path.display())]
    Prepare {
        /// What failed.
        action: &'static str,
        /// The path it failed on.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// A path for the instance couldn't be built.
    #[error(transparent)]
    Paths(#[from] PathsError),
    /// The sandbox spec built from the grants is invalid.
    #[error(transparent)]
    Spec(#[from] SpecError),
    /// The process couldn't be started.
    #[error("cannot start {}: {source}", .program.display())]
    Spawn {
        /// The trampoline or the plugin.
        program: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The sandbox trampoline failed (exit status 125, or 71 from macOS `sandbox-exec`).
    #[error("the sandbox trampoline failed ({status}){}", last_line(.stderr))]
    Sandbox {
        /// Its exit status.
        status: ExitStatus,
        /// The last stderr lines, scrubbed.
        stderr: Vec<String>,
    },
    /// The plugin shares no protocol major with the core (exit status 76).
    #[error("the plugin shares no protocol version with the core{}", last_line(.stderr))]
    NoSharedProtocol {
        /// The last stderr lines, scrubbed.
        stderr: Vec<String>,
    },
    /// The plugin exited (or closed stdout) before printing the handshake line.
    #[error("the plugin exited before the handshake ({status}){}", last_line(.stderr))]
    Exited {
        /// Its exit status.
        status: ExitStatus,
        /// The last stderr lines, scrubbed.
        stderr: Vec<String>,
    },
    /// No handshake line within the timeout.
    #[error("no handshake line within {timeout:?}{}", last_line(.stderr))]
    HandshakeTimeout {
        /// The timeout.
        timeout: Duration,
        /// The last stderr lines, scrubbed.
        stderr: Vec<String>,
    },
    /// The handshake line was malformed.
    #[error("{error}{}", last_line(.stderr))]
    Handshake {
        /// What's wrong with it.
        error: HandshakeError,
        /// The last stderr lines, scrubbed.
        stderr: Vec<String>,
    },
    /// The plugin chose a protocol major the core didn't offer.
    #[error("the plugin chose protocol {chosen}, which the core didn't offer")]
    Protocol {
        /// The major it chose.
        chosen: u32,
    },
    /// The gRPC connection couldn't be set up.
    #[error("cannot connect to the plugin: {0}")]
    Connect(String),
    /// `Describe` or `Configure` failed.
    #[error("{call} failed: {code:?}: {message}")]
    Rpc {
        /// `Describe` or `Configure`.
        call: &'static str,
        /// The status code (`DeadlineExceeded` for the core's own timeout).
        code: tonic::Code,
        /// The status message, scrubbed.
        message: String,
    },
    /// `Describe` doesn't match the binary: wrong kind, wrong name, or missing capabilities.
    #[error("the plugin doesn't match its binary {binary}: {reason}")]
    Mismatch {
        /// `giles-<kind>-<component>`.
        binary: String,
        /// What doesn't match.
        reason: String,
    },
    /// The identity namespace from `Configure` isn't the plugin's own.
    #[error(transparent)]
    Namespace(#[from] NamespaceError),
}

impl LaunchError {
    /// Whether relaunching with backoff may help. `false` means not until the binary or the
    /// configuration changes: the sandbox trampoline failed, no shared protocol, a `Describe`
    /// mismatch, a config the plugin rejects, a bad namespace, or no binary at all.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Prepare { .. }
            | Self::Spawn { .. }
            | Self::Exited { .. }
            | Self::HandshakeTimeout { .. }
            | Self::Handshake { .. }
            | Self::Connect(_)
            | Self::Auth(_) => true,
            Self::Rpc { call, code, .. } => {
                *call == "Describe"
                    || !matches!(
                        code,
                        tonic::Code::InvalidArgument
                            | tonic::Code::FailedPrecondition
                            | tonic::Code::Internal
                            | tonic::Code::Unimplemented
                    )
            }
            _ => false,
        }
    }

    /// The plugin's last stderr lines (scrubbed), for failures before the handshake completed.
    pub fn stderr(&self) -> &[String] {
        match self {
            Self::Sandbox { stderr, .. }
            | Self::NoSharedProtocol { stderr }
            | Self::Exited { stderr, .. }
            | Self::HandshakeTimeout { stderr, .. }
            | Self::Handshake { stderr, .. } => stderr,
            _ => &[],
        }
    }
}

/// `: <the last non-empty line>`, or nothing.
fn last_line(lines: &[String]) -> String {
    lines
        .iter()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| format!(": {line}"))
        .unwrap_or_default()
}

/// Looks up an environment variable (for `PATH`).
type EnvFn = dyn Fn(&str) -> Option<OsString> + Send + Sync;

/// Launches plugins for one data root, and connects to remote ones.
#[derive(Clone)]
pub struct Launcher {
    root: DataRoot,
    trampoline: PathBuf,
    sandbox: Result<(), String>,
    env: Arc<EnvFn>,
    handshake_timeout: Duration,
    /// Trusted for remote plugins over TLS instead of the bundled roots.
    remote_roots: Option<Arc<RootCertStore>>,
}

impl std::fmt::Debug for Launcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Launcher")
            .field("root", &self.root)
            .field("trampoline", &self.trampoline)
            .field("sandbox", &self.sandbox)
            .finish_non_exhaustive()
    }
}

impl Launcher {
    /// A launcher for `root` whose sandbox trampoline is `trampoline` (the `giles` binary, run as
    /// `giles sandbox-exec`). `sandbox` is the result of the host's sandbox check
    /// ([`sandbox::check_host`], which `serve` runs once at startup): an error refuses every
    /// sandboxed launch with that one message (PLAN section 12.1). It finds binaries on the
    /// process's `PATH`, waits [`HANDSHAKE_TIMEOUT`] for the handshake line, and checks remote
    /// plugins' TLS certificates against the bundled Mozilla roots.
    pub fn new(root: DataRoot, trampoline: PathBuf, sandbox: Result<(), String>) -> Self {
        Self {
            root,
            trampoline,
            sandbox,
            env: Arc::new(|name| std::env::var_os(name)),
            handshake_timeout: HANDSHAKE_TIMEOUT,
            remote_roots: None,
        }
    }

    /// Looks up `PATH` (for discovery) with `env` instead of the process environment.
    #[must_use]
    pub fn env(mut self, env: impl Fn(&str) -> Option<OsString> + Send + Sync + 'static) -> Self {
        self.env = Arc::new(env);
        self
    }

    /// Waits `timeout` for the handshake line instead of [`HANDSHAKE_TIMEOUT`].
    #[must_use]
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Checks remote plugins' TLS certificates against `roots` instead of the bundled Mozilla
    /// roots (tests with their own certificate authority).
    #[must_use]
    pub fn remote_tls_roots(mut self, roots: RootCertStore) -> Self {
        self.remote_roots = Some(Arc::new(roots));
        self
    }

    /// The data root.
    pub fn root(&self) -> &DataRoot {
        &self.root
    }

    /// The binary a launch of channel component `component` would run now, links resolved (the
    /// same lookup as the launch), or `None` if there is none.
    pub fn find_binary(&self, component: &str) -> Option<PathBuf> {
        let found = discover_plugin(&self.root, Kind::Channel, component, &*self.env)?;
        fs::canonicalize(found).ok()
    }

    /// Launches the plugin channel `instance`, up to a successful `Configure`. Logs under a
    /// `plugin{instance=...}` span, which also carries the plugin's stderr.
    ///
    /// # Errors
    ///
    /// [`LaunchError`]; the plugin is stopped (its whole process group killed) first.
    pub async fn launch_channel(
        &self,
        instance: &ChannelInstance,
    ) -> Result<LaunchedPlugin, LaunchError> {
        let ChannelRuntime::Plugin(launch) = &instance.runtime else {
            return Err(LaunchError::NotLaunched);
        };
        let span = tracing::info_span!("plugin", instance = %instance.name);
        self.launch(instance, &launch.binary, &launch.sandbox, span.clone())
            .instrument(span)
            .await
    }

    async fn launch(
        &self,
        instance: &ChannelInstance,
        binary_name: &str,
        sandbox: &crate::config::Sandbox,
        span: tracing::Span,
    ) -> Result<LaunchedPlugin, LaunchError> {
        let name = instance.name.as_str();
        let sandboxed = !sandbox.unsandboxed;
        if sandboxed && let Err(reason) = &self.sandbox {
            return Err(LaunchError::SandboxUnavailable(reason.clone()));
        }
        let found = discover_plugin(&self.root, Kind::Channel, &instance.component, &*self.env)
            .ok_or_else(|| LaunchError::NotFound {
                binary: binary_name.to_owned(),
                plugins_dir: self.root.plugins_dir(),
            })?;
        // The file granted, recorded, and run are the same: the binary with links resolved.
        let binary = fs::canonicalize(&found).map_err(|source| LaunchError::Prepare {
            action: "resolve",
            path: found.clone(),
            source,
        })?;

        let pid_file = self.root.pid_file(name)?;
        match orphans::cleanup(&pid_file, &binary).await {
            Ok(orphans::Cleanup::Killed(pid)) => {
                warn!(pid, binary = %binary.display(), "killed an orphaned plugin process");
            }
            Ok(_) => {}
            Err(source) => {
                return Err(LaunchError::Prepare {
                    action: "clean up the pid file",
                    path: pid_file,
                    source,
                });
            }
        }

        let data_dir = self.root.plugin_data_dir(name)?;
        prepare_data_dir(&data_dir)?;
        let spec = launch_spec(&data_dir, &binary, sandbox);
        spec.validate()?;

        let token = Secret::new(grpc::hex(&random_bytes::<32>().map_err(|source| {
            LaunchError::Prepare {
                action: "generate a token for",
                path: binary.clone(),
                source,
            }
        })?));
        let secrets: Arc<[Secret]> = instance
            .config
            .secrets()
            .cloned()
            .chain([token.clone()])
            .collect();

        let spec_file;
        let (program, args, env) = if sandboxed {
            spec_file = self.root.sandbox_spec_file(name)?;
            write_private(&spec_file, spec.to_json()?.as_bytes()).map_err(|source| {
                LaunchError::Prepare {
                    action: "write the sandbox spec",
                    path: spec_file.clone(),
                    source,
                }
            })?;
            let args = vec![
                OsString::from("sandbox-exec"),
                spec_file.clone().into(),
                "--".into(),
                binary.clone().into(),
            ];
            (self.trampoline.as_path(), args, None)
        } else {
            warn!(
                binary = %binary.display(),
                "running the plugin unsandboxed (`unsandboxed = true`)",
            );
            let env = sandbox::scrubbed_env(&spec, &|name| std::env::var_os(name));
            (
                binary.as_path(),
                Vec::new(),
                Some((env, data_dir.as_path())),
            )
        };

        let spawned = process::spawn(
            process::SpawnSpec {
                program,
                args,
                env,
                token: &token,
                pid_file: &pid_file,
                binary: &binary,
                secrets: secrets.clone(),
            },
            &span,
        )
        .await
        .map_err(|source| LaunchError::Spawn {
            program: program.to_path_buf(),
            source,
        })?;
        info!(
            pid = spawned.process.pid(),
            sandboxed, "plugin process started"
        );

        let mut process = spawned.process;
        let result = self
            .connect(
                instance,
                spawned.stdout,
                spawned.core_end,
                &token,
                &secrets,
                sandboxed,
                &mut process,
            )
            .await;
        match result {
            Ok((connection, describe, identity_namespace)) => {
                info!(
                    pid = process.pid(),
                    version = %describe.version,
                    identity_namespace,
                    "plugin launched",
                );
                Ok(LaunchedPlugin {
                    instance: instance.name.clone(),
                    component: instance.component.clone(),
                    binary,
                    sandboxed,
                    describe,
                    identity_namespace,
                    connection,
                    process,
                    secrets,
                    span,
                })
            }
            Err(error) => {
                if let Err(kill) = process.kill().await {
                    warn!(error = %kill, "cannot kill the failed plugin");
                }
                Err(error)
            }
        }
    }

    /// Everything after the spawn: the handshake line, the connection, `Describe`, `Configure`,
    /// and the namespace check.
    #[allow(clippy::too_many_arguments)]
    async fn connect(
        &self,
        instance: &ChannelInstance,
        stdout: ChildStdout,
        core_end: std::os::unix::net::UnixStream,
        token: &Secret,
        secrets: &[Secret],
        sandboxed: bool,
        process: &mut PluginProcess,
    ) -> Result<(PluginConnection, DescribeResponse, String), LaunchError> {
        let mut stdout = BufReader::new(stdout);
        let read = tokio::time::timeout(self.handshake_timeout, read_line(&mut stdout)).await;
        let line = match read {
            Err(_) => {
                return Err(LaunchError::HandshakeTimeout {
                    timeout: self.handshake_timeout,
                    stderr: failed_stderr(process).await,
                });
            }
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None) | Err(_)) => {
                let status = match tokio::time::timeout(EXIT_AFTER_EOF, process.wait()).await {
                    Ok(Ok(status)) => status,
                    _ => process
                        .kill()
                        .await
                        .map_err(|e| LaunchError::Connect(e.to_string()))?,
                };
                let stderr = failed_stderr(process).await;
                return Err(match classify_exit(status, sandboxed) {
                    EarlyExit::Sandbox => LaunchError::Sandbox { status, stderr },
                    EarlyExit::NoSharedProtocol => LaunchError::NoSharedProtocol { stderr },
                    EarlyExit::Other => LaunchError::Exited { status, stderr },
                });
            }
        };
        let handshake = match HandshakeLine::parse(&line) {
            Ok(handshake) => handshake,
            Err(error) => {
                return Err(LaunchError::Handshake {
                    error,
                    stderr: failed_stderr(process).await,
                });
            }
        };
        if !OFFERED_PROTOCOLS.contains(&handshake.protocol) {
            return Err(LaunchError::Protocol {
                chosen: handshake.protocol,
            });
        }
        // After the line, stdout is drained and ignored; the task ends when the plugin closes it.
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await;
        });

        let connection = PluginConnection::connect(core_end, token)
            .await
            .map_err(|e| LaunchError::Connect(e.to_string()))?;
        let (describe, namespace) = describe_and_configure(&connection, instance, secrets).await?;
        Ok((connection, describe, namespace))
    }
}

/// `Describe` (checked against the component), `Configure` with the instance config, and the
/// namespace check, over a new connection to `instance`'s plugin. Plugin text in errors is
/// scrubbed with `secrets`.
async fn describe_and_configure(
    connection: &PluginConnection,
    instance: &ChannelInstance,
    secrets: &[Secret],
) -> Result<(DescribeResponse, String), LaunchError> {
    let secrets: Vec<&Secret> = secrets.iter().collect();
    let rpc_error = |call, status: tonic::Status| LaunchError::Rpc {
        call,
        code: status.code(),
        message: scrub(status.message(), &secrets),
    };

    let request = DescribeRequest {
        core_protocol_minor: CORE_PROTOCOL_MINOR,
        core_features: Vec::new(),
    };
    let describe = tokio::time::timeout(DESCRIBE_TIMEOUT, connection.plugin().describe(request))
        .await
        .unwrap_or_else(|_| Err(timed_out(DESCRIBE_TIMEOUT)))
        .map_err(|status| rpc_error("Describe", status))?
        .into_inner();
    check_describe(&describe, &instance.component).map_err(|reason| LaunchError::Mismatch {
        binary: plugin_binary(Kind::Channel, &instance.component),
        reason: scrub(&reason, &secrets),
    })?;

    let request = ConfigureRequest {
        instance: instance.name.as_str().to_owned(),
        config_json: instance.config.to_json().to_string(),
    };
    let configured =
        tokio::time::timeout(CONFIGURE_TIMEOUT, connection.plugin().configure(request))
            .await
            .unwrap_or_else(|_| Err(timed_out(CONFIGURE_TIMEOUT)))
            .map_err(|status| rpc_error("Configure", status))?
            .into_inner();
    let namespace = configured.identity_namespace;
    validate_namespace(&instance.component, &namespace).map_err(|error| match error {
        // The namespace is plugin text built from config; scrub it like any other.
        NamespaceError::WrongComponent {
            component,
            namespace,
        } => NamespaceError::WrongComponent {
            component,
            namespace: scrub(&namespace, &secrets),
        },
        NamespaceError::Malformed { namespace } => NamespaceError::Malformed {
            namespace: scrub(&namespace, &secrets),
        },
    })?;
    Ok((describe, namespace))
}

/// The plugin's stderr tail once it has ended (bounded).
async fn failed_stderr(process: &mut PluginProcess) -> Vec<String> {
    if process.exit_status().is_none() {
        let _ = process.kill().await;
    }
    process.stderr_closed(STDERR_DRAIN).await;
    process.stderr_tail()
}

fn timed_out(timeout: Duration) -> tonic::Status {
    tonic::Status::deadline_exceeded(format!("no answer within {timeout:?}"))
}

/// How a plugin that exited before its handshake line failed.
#[derive(Debug, PartialEq, Eq)]
enum EarlyExit {
    /// The trampoline (or macOS `sandbox-exec`) failed: don't retry.
    Sandbox,
    /// Exit status 76: no shared protocol.
    NoSharedProtocol,
    /// Anything else.
    Other,
}

fn classify_exit(status: ExitStatus, sandboxed: bool) -> EarlyExit {
    if status.code() == Some(EXIT_NO_SHARED_PROTOCOL) {
        EarlyExit::NoSharedProtocol
    } else if sandboxed && sandbox::is_trampoline_failure(status) {
        EarlyExit::Sandbox
    } else {
        EarlyExit::Other
    }
}

/// Checks `Describe` against the binary `giles-channel-<component>`: the kind, the name, and the
/// channel capabilities. The error is the reason.
fn check_describe(describe: &DescribeResponse, component: &str) -> Result<(), String> {
    let kind = PluginKind::try_from(describe.kind).unwrap_or(PluginKind::Unspecified);
    if kind != PluginKind::Channel {
        return Err(format!(
            "it describes itself as {}, expected {}",
            kind.as_str_name(),
            PluginKind::Channel.as_str_name()
        ));
    }
    if describe.name != component {
        return Err(format!(
            "it is named {:?}, expected {component:?}",
            describe.name
        ));
    }
    let Some(describe_response::Capabilities::Channel(capabilities)) = &describe.capabilities
    else {
        return Err("it describes no channel capabilities".to_owned());
    };
    if output_mode_from_proto(capabilities.output).is_err() {
        return Err(format!(
            "its output mode {} is unspecified or unknown",
            capabilities.output
        ));
    }
    Ok(())
}

/// Reads one line of at most [`MAX_HANDSHAKE_LINE`] bytes. `None` at end-of-file.
async fn read_line(stdout: &mut BufReader<ChildStdout>) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = stdout
        .take(MAX_HANDSHAKE_LINE)
        .read_until(b'\n', &mut line)
        .await?;
    Ok((read > 0).then(|| String::from_utf8_lossy(&line).into_owned()))
}

/// Creates the data directory and its `tmp/` (mode 0700), and refuses a data directory that is a
/// symbolic link, as the trampoline does.
pub(crate) fn prepare_data_dir(data_dir: &Path) -> Result<(), LaunchError> {
    fn prepare(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> LaunchError {
        let path = path.to_path_buf();
        move |source| LaunchError::Prepare {
            action,
            path,
            source,
        }
    }
    let mut builder = DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder
        .create(data_dir)
        .map_err(prepare("create the data directory", data_dir))?;
    let meta = fs::symlink_metadata(data_dir).map_err(prepare("inspect", data_dir))?;
    if !meta.is_dir() {
        return Err(prepare("use the data directory", data_dir)(
            io::Error::other("it is a symbolic link or not a directory"),
        ));
    }
    let tmp = data_dir.join(TMP_DIR_NAME);
    builder
        .create(&tmp)
        .map_err(prepare("create the temporary directory", &tmp))?;
    Ok(())
}

/// Writes `contents` to `path` atomically with mode 0600.
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("no parent directory"))?;
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(contents)?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes)
}

/// A launched, configured plugin: what supervision (2.17) and the `RemoteChannel` adapter (2.18)
/// build on.
#[derive(Debug)]
#[non_exhaustive]
pub struct LaunchedPlugin {
    /// The instance.
    pub instance: InstanceName,
    /// Its component, `<name>` of `giles-channel-<name>`.
    pub component: String,
    /// The binary that runs, symlinks resolved.
    pub binary: PathBuf,
    /// Whether it runs in the sandbox.
    pub sandboxed: bool,
    /// Its `Describe` answer (kind and name already checked).
    pub describe: DescribeResponse,
    /// The identity namespace `Configure` returned, already checked.
    pub identity_namespace: String,
    /// The gRPC connection.
    pub connection: PluginConnection,
    /// The process (its stderr is being logged).
    pub process: PluginProcess,
    /// The instance's secret values and the launch token: scrub plugin-provided text with them
    /// ([`scrub`]) before logging it.
    pub secrets: Arc<[Secret]>,
    /// The `plugin{instance=...}` span its logs belong under.
    pub span: tracing::Span,
}

impl LaunchedPlugin {
    /// Stops the plugin (PLAN section 7.5): `Shutdown` and closing stdin, up to `grace` for it to
    /// exit, then `SIGTERM` to its process group, [`TERM_GRACE`] later `SIGKILL`; whatever is left
    /// of the group is killed too, and the pid file removed.
    ///
    /// # Errors
    ///
    /// Signalling or waiting for the process failed.
    pub async fn stop(mut self, grace: Duration) -> io::Result<ExitStatus> {
        let span = self.span.clone();
        async move {
            let start = Instant::now();
            if self.process.exit_status().is_none() {
                let mut client = self.connection.plugin();
                let shutdown = client.shutdown(ShutdownRequest {});
                match tokio::time::timeout(grace, shutdown).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(status)) => tracing::debug!(%status, "Shutdown failed"),
                    Err(_) => tracing::debug!("Shutdown timed out"),
                }
                self.process.close_stdin();
                let remaining = grace.saturating_sub(start.elapsed());
                let _ = tokio::time::timeout(remaining, self.process.wait()).await;
            }
            let status = self.process.terminate(TERM_GRACE).await?;
            info!(%status, "plugin stopped");
            Ok(status)
        }
        .instrument(span)
        .await
    }
}

/// Where launched channels go: the registry (lookups, replies) and the session manager (their
/// events), with the blob store for their attachments.
#[derive(Debug, Clone)]
pub struct ChannelWiring {
    /// The registry the channel is registered with.
    pub registry: Arc<Registry>,
    /// The session manager that consumes the channel's events.
    pub sessions: Sessions,
    /// Where inbound attachments are stored and outbound ones read from.
    pub blobs: BlobStore,
}

impl ChannelWiring {
    /// Builds `plugin`'s [`RemoteChannel`] (attachments up to `max_blob` bytes), registers it,
    /// and attaches it to the session manager, replacing the channel of an earlier launch.
    ///
    /// # Errors
    ///
    /// The reason the channel can't be registered: its `Describe` capabilities are invalid, or
    /// the instance isn't configured.
    pub fn attach(&self, plugin: &LaunchedPlugin, max_blob: u64) -> Result<(), String> {
        let channel = RemoteChannel::new(plugin, self.blobs.clone(), max_blob)
            .map_err(|error| error.to_string())?;
        self.register(&plugin.instance, &plugin.component, Arc::new(channel))
    }

    /// [`attach`](Self::attach) for a remote plugin, replacing the channel of an earlier
    /// connection.
    ///
    /// # Errors
    ///
    /// As for [`attach`](Self::attach).
    pub fn attach_remote(&self, plugin: &RemotePlugin, max_blob: u64) -> Result<(), String> {
        let channel = RemoteChannel::from_remote(plugin, self.blobs.clone(), max_blob)
            .map_err(|error| error.to_string())?;
        self.register(&plugin.instance, &plugin.component, Arc::new(channel))
    }

    fn register(
        &self,
        instance: &InstanceName,
        component: &str,
        channel: Arc<dyn Channel>,
    ) -> Result<(), String> {
        self.registry
            .register_channel(instance, channel.clone())
            .map_err(|error| error.to_string())?;
        self.sessions
            .attach_channel(instance.clone(), component.to_owned(), channel);
        Ok(())
    }
}

/// The plugin channels of a running core, each supervised ([`supervisor`]): launched (or, if
/// remote, connected to) at startup, registered as channels, health-checked, relaunched (or
/// reconnected) with backoff, and stopped (or disconnected) at shutdown. Its
/// [`statuses`](Self::statuses) are what `giles status` shows.
#[derive(Debug)]
pub struct PluginHost {
    launcher: Launcher,
    wiring: ChannelWiring,
    supervision: Supervision,
}

impl PluginHost {
    /// A host supervising nothing yet that hands launched channels to `wiring`, with the
    /// `Describe` cache of the launcher's data root.
    pub fn new(launcher: Launcher, wiring: ChannelWiring) -> Self {
        let supervision = Supervision::new(DescribeCache::new(launcher.root()));
        Self {
            launcher,
            wiring,
            supervision,
        }
    }

    /// Starts supervising every plugin channel in `config`, launched or remote, and returns once
    /// each has had its first launch (or connection) attempt: up and registered
    /// ([`ChannelWiring::attach`]), or failed (logged, and relaunched with backoff or once its
    /// binary changes; a remote plugin is always retried with backoff).
    ///
    /// Cancel-safe: dropping the future (a shutdown during startup) leaves supervision running
    /// until [`PluginHost::stop_all`], which also abandons launches in progress (killing their
    /// process groups).
    pub async fn start(&self, config: &Config) {
        let attempts: Vec<_> = config
            .channels
            .values()
            .filter_map(|instance| {
                let launch: Arc<dyn Launch> = match &instance.runtime {
                    ChannelRuntime::Plugin(_) => Arc::new(PluginLaunch {
                        launcher: self.launcher.clone(),
                        instance: instance.clone(),
                        wiring: self.wiring.clone(),
                    }),
                    ChannelRuntime::Remote(_) => Arc::new(RemoteLaunch {
                        launcher: self.launcher.clone(),
                        instance: instance.clone(),
                        wiring: self.wiring.clone(),
                    }),
                    ChannelRuntime::BuiltIn => return None,
                };
                Some(self.supervision.supervise(
                    instance.name.clone(),
                    instance.component.clone(),
                    instance.config_hash,
                    launch,
                ))
            })
            .collect();
        futures::future::join_all(attempts).await;
    }

    /// The launcher.
    pub fn launcher(&self) -> &Launcher {
        &self.launcher
    }

    /// Every supervised instance's status, by instance name.
    pub fn statuses(&self) -> Vec<PluginStatus> {
        self.supervision.statuses()
    }

    /// The status of supervised instance `instance`.
    pub fn status(&self, instance: &str) -> Option<PluginStatus> {
        self.supervision.status(instance)
    }

    /// Relaunches `instance` now: stops it if it runs, and skips the backoff or the wait for a
    /// new binary otherwise (the admin service's `RestartPlugin`). A request during a launch
    /// takes effect once that launch is up. `false` if `instance` isn't supervised.
    pub fn restart(&self, instance: &str) -> bool {
        self.supervision.restart(instance)
    }

    /// Stops supervision and every plugin concurrently ([`LaunchedPlugin::stop`] with
    /// [`STOP_GRACE`]), and waits for them. Nothing is relaunched afterwards.
    pub async fn stop_all(&self) {
        self.supervision.stop_all().await;
    }
}

/// What the admin service's `Status`, `ListPlugins`, and `RestartPlugin` see of supervision.
impl PluginControl for PluginHost {
    fn plugins(&self) -> Vec<PluginRuntime> {
        self.statuses().into_iter().map(plugin_runtime).collect()
    }

    fn restart(&self, instance: &str) -> bool {
        PluginHost::restart(self, instance)
    }
}

/// A supervised instance's status as the admin service reports it.
fn plugin_runtime(status: PluginStatus) -> PluginRuntime {
    let state = match status.state {
        PluginState::Starting => PluginRunState::Starting,
        PluginState::Running { pid, since } => PluginRunState::Running { pid, since },
        PluginState::BackingOff { until } => PluginRunState::BackingOff { until },
        PluginState::Failed => PluginRunState::Failed,
        PluginState::Stopped => PluginRunState::Stopped,
    };
    let (version, requested_grants) = match status.describe {
        Some(describe) => {
            let grants = describe.requested_grants;
            (
                Some(describe.version),
                Some(SandboxGrants {
                    network_all: grants.network_all,
                    network_ports: grants.network_ports,
                    read: grants.read,
                    write: grants.write,
                }),
            )
        }
        None => (None, None),
    };
    PluginRuntime {
        instance: status.instance,
        binary: status.binary,
        state,
        restarts: status.restarts,
        last_error: status.last_error,
        version,
        requested_grants,
    }
}

/// The launches of one configured plugin channel, for its supervisor.
struct PluginLaunch {
    launcher: Launcher,
    instance: ChannelInstance,
    wiring: ChannelWiring,
}

#[async_trait]
impl Launch for PluginLaunch {
    async fn binary(&self) -> Option<(PathBuf, [u8; 32])> {
        let path = self.launcher.find_binary(&self.instance.component)?;
        tokio::task::spawn_blocking(move || {
            let hash = sha256_file(&path)
                .inspect_err(|error| {
                    warn!(path = %path.display(), %error, "cannot hash the plugin binary");
                })
                .ok()?;
            Some((path, hash))
        })
        .await
        .ok()
        .flatten()
    }

    async fn launch(&self) -> Result<Box<dyn Running>, LaunchFailure> {
        let plugin = self
            .launcher
            .launch_channel(&self.instance)
            .await
            .map_err(|error| LaunchFailure {
                message: error.to_string(),
                retryable: error.is_retryable(),
            })?;
        if let Err(reason) = self.wiring.attach(&plugin, self.instance.max_blob) {
            if let Err(error) = plugin.stop(STOP_GRACE).await {
                warn!(%error, "cannot stop the plugin");
            }
            return Err(LaunchFailure {
                message: format!("cannot register the plugin's channel: {reason}"),
                retryable: false,
            });
        }
        Ok(Box::new(Live(plugin)))
    }
}

/// A launched plugin under supervision.
struct Live(LaunchedPlugin);

#[async_trait]
impl Running for Live {
    fn pid(&self) -> Option<u32> {
        Some(self.0.process.pid())
    }

    fn describe(&self) -> Option<DescribeInfo> {
        DescribeInfo::from_describe(&self.0.describe)
    }

    async fn exited(&mut self) -> String {
        match self.0.process.wait().await {
            Ok(status) => format!("exited ({status})"),
            Err(error) => format!("can't be waited for: {error}"),
        }
    }

    async fn check_health(&self) -> Result<(), String> {
        check_health(&self.0.connection, &self.0.secrets).await
    }

    async fn stop(self: Box<Self>) {
        if let Err(error) = self.0.stop(STOP_GRACE).await {
            warn!(%error, "cannot stop the plugin");
        }
    }
}

/// The connections to one configured remote plugin channel, for its supervisor.
struct RemoteLaunch {
    launcher: Launcher,
    instance: ChannelInstance,
    wiring: ChannelWiring,
}

#[async_trait]
impl Launch for RemoteLaunch {
    /// None: the core doesn't run the plugin, so there is no binary or `Describe` cache.
    async fn binary(&self) -> Option<(PathBuf, [u8; 32])> {
        None
    }

    /// Connects. Every failure is retried with backoff: whoever runs the plugin can fix it
    /// (start it, upgrade it, restart it with another config) without the core noticing.
    async fn launch(&self) -> Result<Box<dyn Running>, LaunchFailure> {
        let plugin = self
            .launcher
            .connect_remote(&self.instance)
            .await
            .map_err(|error| LaunchFailure {
                message: error.to_string(),
                retryable: true,
            })?;
        if let Err(reason) = self.wiring.attach_remote(&plugin, self.instance.max_blob) {
            plugin.stop(REMOTE_SHUTDOWN_TIMEOUT).await;
            return Err(LaunchFailure {
                message: format!("cannot register the plugin's channel: {reason}"),
                retryable: true,
            });
        }
        Ok(Box::new(Connected(plugin)))
    }
}

/// A connected remote plugin under supervision.
struct Connected(RemotePlugin);

#[async_trait]
impl Running for Connected {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn describe(&self) -> Option<DescribeInfo> {
        DescribeInfo::from_describe(&self.0.describe)
    }

    async fn exited(&mut self) -> String {
        self.0.closed().await;
        "disconnected".to_owned()
    }

    async fn check_health(&self) -> Result<(), String> {
        check_health(&self.0.connection, &self.0.secrets.get()).await
    }

    async fn stop(self: Box<Self>) {
        self.0.stop(REMOTE_SHUTDOWN_TIMEOUT).await;
    }
}

/// One `grpc.health.v1` check of the plugin's channel service; the error (scrubbed with
/// `secrets`) says why it failed.
async fn check_health(connection: &PluginConnection, secrets: &[Secret]) -> Result<(), String> {
    let request = HealthCheckRequest {
        service: CHANNEL_SERVICE.to_owned(),
    };
    let mut client = connection.health();
    match tokio::time::timeout(HEALTH_TIMEOUT, client.check(request)).await {
        Err(_) => Err(format!("no answer within {HEALTH_TIMEOUT:?}")),
        Ok(Err(status)) => {
            let secrets: Vec<&Secret> = secrets.iter().collect();
            Err(scrub(
                &format!("{:?}: {}", status.code(), status.message()),
                &secrets,
            ))
        }
        Ok(Ok(response)) => match response.into_inner().status() {
            ServingStatus::Serving => Ok(()),
            other => Err(format!("{CHANNEL_SERVICE} is {}", other.as_str_name())),
        },
    }
}

/// The SHA-256 of a file's contents.
fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    use std::io::Read as _;

    use sha2::Digest as _;
    let mut file = fs::File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        match file.read(&mut buf)? {
            0 => return Ok(hasher.finalize().into()),
            n => hasher.update(&buf[..n]),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;

    use giles_proto::channel::v1::ChannelCapabilities;
    use giles_proto::provider::v1::ProviderCapabilities;

    use super::*;

    fn exit(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    #[test]
    fn early_exits_are_classified() {
        assert_eq!(classify_exit(exit(76), true), EarlyExit::NoSharedProtocol);
        assert_eq!(classify_exit(exit(76), false), EarlyExit::NoSharedProtocol);
        assert_eq!(classify_exit(exit(125), true), EarlyExit::Sandbox);
        // Unsandboxed, nothing runs between the core and the plugin: 125 is the plugin's own.
        assert_eq!(classify_exit(exit(125), false), EarlyExit::Other);
        let expected = if cfg!(target_os = "macos") {
            EarlyExit::Sandbox
        } else {
            EarlyExit::Other
        };
        assert_eq!(classify_exit(exit(71), true), expected);
        assert_eq!(classify_exit(exit(1), true), EarlyExit::Other);
        assert_eq!(
            classify_exit(ExitStatus::from_raw(9), true),
            EarlyExit::Other
        );
    }

    fn describe(kind: PluginKind, name: &str, channel: bool) -> DescribeResponse {
        DescribeResponse {
            kind: kind.into(),
            name: name.to_owned(),
            version: "1.0".to_owned(),
            requested_grants: None,
            capabilities: Some(if channel {
                describe_response::Capabilities::Channel(ChannelCapabilities {
                    output: giles_proto::channel::v1::OutputMode::Blocks.into(),
                    can_initiate: false,
                })
            } else {
                describe_response::Capabilities::Provider(ProviderCapabilities::default())
            }),
        }
    }

    #[test]
    fn describe_must_match_the_binary() {
        assert_eq!(
            check_describe(&describe(PluginKind::Channel, "irc", true), "irc"),
            Ok(())
        );
        let err = check_describe(&describe(PluginKind::Provider, "irc", true), "irc").unwrap_err();
        assert!(err.contains("PLUGIN_KIND_PROVIDER"), "{err}");
        let err = check_describe(&describe(PluginKind::Channel, "irk", true), "irc").unwrap_err();
        assert!(err.contains("\"irk\""), "{err}");
        let err = check_describe(&describe(PluginKind::Channel, "irc", false), "irc").unwrap_err();
        assert!(err.contains("capabilities"), "{err}");
        let mut unknown = describe(PluginKind::Channel, "irc", true);
        unknown.kind = 7;
        assert!(check_describe(&unknown, "irc").is_err());
        let mut no_output = describe(PluginKind::Channel, "irc", true);
        no_output.capabilities = Some(describe_response::Capabilities::Channel(
            ChannelCapabilities::default(),
        ));
        let err = check_describe(&no_output, "irc").unwrap_err();
        assert!(err.contains("output mode 0"), "{err}");
    }

    #[test]
    fn retryability_follows_the_failure() {
        let status = exit(1);
        assert!(
            LaunchError::Exited {
                status,
                stderr: vec![]
            }
            .is_retryable()
        );
        assert!(
            !LaunchError::Sandbox {
                status,
                stderr: vec![]
            }
            .is_retryable()
        );
        assert!(!LaunchError::NoSharedProtocol { stderr: vec![] }.is_retryable());
        let rpc = |call, code| LaunchError::Rpc {
            call,
            code,
            message: String::new(),
        };
        assert!(!rpc("Configure", tonic::Code::InvalidArgument).is_retryable());
        assert!(rpc("Configure", tonic::Code::Unavailable).is_retryable());
        assert!(rpc("Describe", tonic::Code::Internal).is_retryable());
    }

    #[test]
    fn errors_show_the_last_stderr_line() {
        let error = LaunchError::Sandbox {
            status: exit(125),
            stderr: vec!["Error: no sandbox".into(), String::new()],
        };
        assert!(
            error.to_string().ends_with("): Error: no sandbox"),
            "{error}"
        );
    }

    #[test]
    fn supervision_status_maps_onto_the_admin_runtime() {
        let until = giles_types::Timestamp::UNIX_EPOCH;
        let status = PluginStatus {
            instance: InstanceName::new("irc").unwrap(),
            component: "irc".into(),
            binary: Some(PathBuf::from("/plugins/giles-channel-irc")),
            state: PluginState::BackingOff { until },
            restarts: 3,
            last_error: Some("the plugin exited (exit status: 1)".into()),
            describe: Some(DescribeInfo {
                version: "2.0".into(),
                output: giles_types::OutputMode::Blocks,
                can_initiate: false,
                requested_grants: RequestedGrants {
                    network_all: false,
                    network_ports: vec!["tcp:6697".into()],
                    read: vec!["/etc/ssl".into()],
                    write: vec!["/var/irc".into()],
                },
            }),
            describe_cached: true,
        };
        let runtime = plugin_runtime(status.clone());
        assert_eq!(runtime.state, PluginRunState::BackingOff { until });
        assert_eq!(runtime.restarts, 3);
        assert_eq!(runtime.binary, status.binary);
        assert_eq!(runtime.last_error, status.last_error);
        assert_eq!(runtime.version.as_deref(), Some("2.0"));
        assert_eq!(
            runtime.requested_grants,
            Some(SandboxGrants {
                network_all: false,
                network_ports: vec!["tcp:6697".into()],
                read: vec!["/etc/ssl".into()],
                write: vec!["/var/irc".into()],
            })
        );

        let running = PluginStatus {
            state: PluginState::Running {
                pid: Some(42),
                since: until,
            },
            describe: None,
            ..status
        };
        let runtime = plugin_runtime(running);
        assert_eq!(
            runtime.state,
            PluginRunState::Running {
                pid: Some(42),
                since: until
            }
        );
        assert_eq!((runtime.version, runtime.requested_grants), (None, None));
    }
}
