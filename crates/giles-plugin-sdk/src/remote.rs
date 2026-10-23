//! Remote plugins (PLAN section 7.6): a plugin someone runs themselves, serving gRPC over TCP,
//! optionally with TLS, to a core that connects with a bearer token.
//!
//! The services are the same as a launched plugin's; what differs is the transport, how the core
//! authenticates, where the state lives, and the lifecycle: there is no handshake line and no
//! stdin, the plugin runs until its operator stops it, and `Shutdown` only means that this core
//! is disconnecting.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, Stream};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tonic::transport::server::{Connected, TcpConnectInfo};

use crate::auth::{RequireBearer, VerifyFn};

/// How long a core gets to complete the TLS handshake.
#[cfg(feature = "tls")]
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the accept loop waits after a failed `accept` (out of file descriptors, say).
const ACCEPT_RETRY: Duration = Duration::from_millis(100);

/// How a remote plugin checks the `authorization: Bearer <token>` the core sends on every call
/// (the instance's `auth` in the core's `giles.toml`). Calls that fail the check get
/// `UNAUTHENTICATED`.
#[derive(Clone)]
#[non_exhaustive]
pub enum RemoteAuth {
    /// Accept exactly this token: the instance's `auth = { bearer = ... }`. An empty token
    /// accepts no call (a missing setting must not open the plugin to anyone).
    Bearer(String),
    /// Accept the tokens this function approves, e.g. access tokens from the OAuth server the
    /// instance's `auth = { oauth = ... }` names, checked by signature or introspection. It gets
    /// the token without the `Bearer ` prefix, never an empty one, and runs on every call: cache
    /// anything slow.
    Verify(Arc<dyn Fn(&str) -> bool + Send + Sync>),
    /// No check. Only for a plugin that something else protects (a private network, an
    /// authenticating proxy): anyone who can reach it can read and send the channel's messages.
    None,
}

impl RemoteAuth {
    /// [`RemoteAuth::Verify`] with `verify`.
    pub fn verify(verify: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self::Verify(Arc::new(verify))
    }

    pub(crate) fn check(&self) -> RequireBearer {
        match self {
            Self::Bearer(token) if token.is_empty() => {
                tracing::warn!("the bearer token is empty; refusing every call");
                RequireBearer::Verify(Arc::new(|_| false))
            }
            Self::Bearer(token) => RequireBearer::new(token),
            Self::Verify(verify) => RequireBearer::Verify(Arc::clone(verify) as Arc<VerifyFn>),
            Self::None => RequireBearer::Anyone,
        }
    }
}

impl fmt::Debug for RemoteAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bearer(_) => "Bearer(<redacted>)",
            Self::Verify(_) => "Verify(..)",
            Self::None => "None",
        })
    }
}

/// Where and how a remote plugin serves: its listener, its state directory, how it checks the
/// core's token, TLS, and when it stops. Run it with
/// [`ChannelPlugin::serve_remote`](crate::ChannelPlugin::serve_remote).
///
/// ```no_run
/// # async fn example() -> std::io::Result<()> {
/// use giles_plugin_sdk::{RemoteAuth, RemoteServer};
///
/// let listener = tokio::net::TcpListener::bind("0.0.0.0:7443").await?;
/// let token = std::env::var("PLUGIN_TOKEN").map_err(std::io::Error::other)?;
/// let server = RemoteServer::new(listener, "/var/lib/my-plugin", RemoteAuth::Bearer(token))
///     .tls(giles_plugin_sdk::tls::server_config(
///         &std::fs::read("/etc/my-plugin/cert.pem")?,
///         &std::fs::read("/etc/my-plugin/key.pem")?,
///     )?);
/// # let _ = server;
/// # Ok(())
/// # }
/// ```
pub struct RemoteServer {
    listener: TcpListener,
    data_dir: PathBuf,
    auth: RemoteAuth,
    #[cfg(feature = "tls")]
    tls: Option<Arc<rustls::ServerConfig>>,
    shutdown: BoxFuture<'static, ()>,
}

impl fmt::Debug for RemoteServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("RemoteServer");
        s.field("listener", &self.listener)
            .field("data_dir", &self.data_dir)
            .field("auth", &self.auth);
        #[cfg(feature = "tls")]
        s.field("tls", &self.tls.is_some());
        s.finish_non_exhaustive()
    }
}

impl RemoteServer {
    /// Serves on `listener`, keeping the plugin's state in `data_dir` (created if missing; the
    /// SDK's own state goes in its `giles-sdk/` subdirectory, as with a launched plugin's
    /// `GILES_PLUGIN_DATA`), and accepting the calls `auth` allows. Plain TCP until
    /// [`tls`](Self::tls); the core refuses plain TCP to a host other than its own unless the
    /// instance sets `insecure = true`. Runs until `SIGINT` or `SIGTERM` unless
    /// [`shutdown_on`](Self::shutdown_on) says otherwise.
    pub fn new(listener: TcpListener, data_dir: impl Into<PathBuf>, auth: RemoteAuth) -> Self {
        Self {
            listener,
            data_dir: data_dir.into(),
            auth,
            #[cfg(feature = "tls")]
            tls: None,
            shutdown: Box::pin(termination()),
        }
    }

    /// The address it listens on (useful after binding port 0).
    ///
    /// # Errors
    ///
    /// The listener's `local_addr` failed.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Serves TLS with `config` (see [`tls::server_config`](crate::tls::server_config)), with the
    /// `h2` ALPN protocol set. The core checks the certificate against its bundled Mozilla roots
    /// and the host name in the instance's `address`.
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn tls(mut self, mut config: rustls::ServerConfig) -> Self {
        config.alpn_protocols = vec![b"h2".to_vec()];
        self.tls = Some(Arc::new(config));
        self
    }

    /// Stops when `signal` completes instead of on `SIGINT` or `SIGTERM`.
    #[must_use]
    pub fn shutdown_on(mut self, signal: impl Future<Output = ()> + Send + 'static) -> Self {
        self.shutdown = Box::pin(signal);
        self
    }

    /// Starts accepting connections; the stream yields each one ready for gRPC (after its TLS
    /// handshake, if any). Ends when the listener fails for good; the task ends when the stream
    /// is dropped.
    fn incoming(
        listener: TcpListener,
        #[cfg(feature = "tls")] tls: Option<Arc<rustls::ServerConfig>>,
    ) -> impl Stream<Item = io::Result<Conn>> + Send + 'static {
        let (tx, rx) = mpsc::channel(16);
        #[cfg(feature = "tls")]
        let tls = tls.map(tokio_rustls::TlsAcceptor::from);
        tokio::spawn(async move {
            loop {
                let (tcp, peer) = tokio::select! {
                    () = tx.closed() => return,
                    accepted = listener.accept() => match accepted {
                        Ok(accepted) => accepted,
                        Err(e) => {
                            tracing::warn!("cannot accept a connection: {e}");
                            tokio::time::sleep(ACCEPT_RETRY).await;
                            continue;
                        }
                    },
                };
                let _ = tcp.set_nodelay(true);
                #[cfg(feature = "tls")]
                if let Some(tls) = &tls {
                    let (tls, tx) = (tls.clone(), tx.clone());
                    tokio::spawn(async move {
                        match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, tls.accept(tcp)).await {
                            Ok(Ok(stream)) => {
                                tracing::info!(%peer, "connection (TLS)");
                                let _ = tx.send(Ok(Conn::Tls(Box::new(stream)))).await;
                            }
                            Ok(Err(e)) => tracing::info!(%peer, "TLS handshake failed: {e}"),
                            Err(_) => tracing::info!(%peer, "TLS handshake timed out"),
                        }
                    });
                    continue;
                }
                tracing::info!(%peer, "connection");
                if tx.send(Ok(Conn::Plain(tcp))).await.is_err() {
                    return;
                }
            }
        });
        stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|conn| (conn, rx))
        })
    }

    /// Takes the parts the server needs.
    pub(crate) fn into_parts(self) -> Parts {
        Parts {
            #[cfg(feature = "tls")]
            incoming: Box::pin(Self::incoming(self.listener, self.tls)),
            #[cfg(not(feature = "tls"))]
            incoming: Box::pin(Self::incoming(self.listener)),
            data_dir: self.data_dir,
            check: self.auth.check(),
            shutdown: self.shutdown,
        }
    }
}

/// A [`RemoteServer`] taken apart.
pub(crate) struct Parts {
    pub(crate) incoming: Pin<Box<dyn Stream<Item = io::Result<Conn>> + Send>>,
    pub(crate) data_dir: PathBuf,
    pub(crate) check: RequireBearer,
    pub(crate) shutdown: BoxFuture<'static, ()>,
}

/// Completes on `SIGINT` or `SIGTERM` (never, if neither can be watched).
async fn termination() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut int), Ok(mut term)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) else {
        tracing::warn!("cannot watch SIGINT and SIGTERM; running until killed");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = int.recv() => tracing::info!("SIGINT"),
        _ = term.recv() => tracing::info!("SIGTERM"),
    }
}

/// One accepted connection from a core.
pub(crate) enum Conn {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl Connected for Conn {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> Self::ConnectInfo {
        match self {
            Self::Plain(tcp) => tcp.connect_info(),
            #[cfg(feature = "tls")]
            Self::Tls(tls) => tls.get_ref().0.connect_info(),
        }
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(tcp) => Pin::new(tcp).poll_read(cx, buf),
            #[cfg(feature = "tls")]
            Self::Tls(tls) => Pin::new(tls.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(tcp) => Pin::new(tcp).poll_write(cx, buf),
            #[cfg(feature = "tls")]
            Self::Tls(tls) => Pin::new(tls.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(tcp) => Pin::new(tcp).poll_write_vectored(cx, bufs),
            #[cfg(feature = "tls")]
            Self::Tls(tls) => Pin::new(tls.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Self::Plain(tcp) => tcp.is_write_vectored(),
            #[cfg(feature = "tls")]
            Self::Tls(tls) => tls.is_write_vectored(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(tcp) => Pin::new(tcp).poll_flush(cx),
            #[cfg(feature = "tls")]
            Self::Tls(tls) => Pin::new(tls.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(tcp) => Pin::new(tcp).poll_shutdown(cx),
            #[cfg(feature = "tls")]
            Self::Tls(tls) => Pin::new(tls.as_mut()).poll_shutdown(cx),
        }
    }
}
