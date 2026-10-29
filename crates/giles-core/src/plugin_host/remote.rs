//! Remote plugins (PLAN section 7.6): channel plugins someone runs themselves, reached over TCP,
//! optionally with TLS, with a bearer token or OAuth client credentials. The core connects, calls
//! `Describe` and `Configure` as it does for a launched plugin, and to stop, calls `Shutdown` and
//! disconnects; it never manages the process.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, PoisonError, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;

use giles_plugin_sdk::tls::rustls::RootCertStore;
use giles_plugin_sdk::tls::rustls::pki_types::ServerName;
use giles_proto::plugin::v1::{DescribeResponse, ShutdownRequest};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_util::sync::{CancellationToken, DropGuard, WaitForCancellationFutureOwned};
use tokio_util::task::AbortOnDropHandle;
use tonic::transport::Endpoint;
use tracing::{Instrument as _, info};
use url::{Host, Url};

use super::grpc::{Authorization, CallMetadata};
use super::oauth::{self, TokenClient};
use super::{LaunchError, PluginConnection, describe_and_configure};
use crate::config::{Auth, ChannelInstance, ChannelRuntime, InstanceName, Kind, Secret, scrub};

/// How long connecting (TCP and the TLS handshake) may take.
pub const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`RemotePlugin::stop`] waits for `Shutdown` by default before disconnecting.
pub const REMOTE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How often an idle connection is pinged (HTTP/2 `PING`), so a dead peer or path is noticed
/// between health checks.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// How long a ping may go unanswered before the connection is closed.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

/// A connected, configured remote plugin: what supervision and the `RemoteChannel` adapter build
/// on. Dropping it (or [`stop`](Self::stop)) disconnects: the TCP connection is closed even
/// while clones of [`connection`](Self::connection) (a registered `RemoteChannel`) remain, and
/// every call through them fails from then on.
#[derive(Debug)]
#[non_exhaustive]
pub struct RemotePlugin {
    /// The instance.
    pub instance: InstanceName,
    /// Its component, `<name>` of `giles-channel-<name>`.
    pub component: String,
    /// The instance's `address`.
    pub url: Url,
    /// Its `Describe` answer (kind and name already checked).
    pub describe: DescribeResponse,
    /// The identity namespace `Configure` returned, already checked.
    pub identity_namespace: String,
    /// The gRPC connection.
    pub connection: PluginConnection,
    /// The instance's secret values and credentials (the bearer token, the OAuth client secret
    /// and the current access tokens, kept up to date as they're refreshed): scrub
    /// plugin-provided text with them before logging it.
    pub secrets: Secrets,
    /// The `plugin{kind=channel instance=...}` span its logs belong under.
    pub span: tracing::Span,
    closed: CancellationToken,
    /// Cancelled when this is dropped: the connection then fails every read and write.
    _disconnect: DropGuard,
    _refresh: Option<AbortOnDropHandle<()>>,
}

impl RemotePlugin {
    /// Resolves once the connection has closed: the plugin or the network ended it, or a
    /// keepalive went unanswered. Cancel-safe.
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }

    /// Disconnects (PLAN section 7.6): calls `Shutdown`, waiting up to `grace` for the answer,
    /// and closes the connection. The plugin keeps running.
    pub async fn stop(self, grace: Duration) {
        let span = self.span.clone();
        async move {
            if !self.closed.is_cancelled() {
                let mut client = self.connection.plugin();
                let shutdown = client.shutdown(ShutdownRequest {});
                match tokio::time::timeout(grace, shutdown).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(status)) => tracing::debug!(%status, "Shutdown failed"),
                    Err(_) => tracing::debug!("Shutdown timed out"),
                }
            }
            drop(self);
            info!("disconnected from the remote plugin");
        }
        .instrument(span)
        .await;
    }
}

/// Secret values to scrub plugin-provided text with, shared so a remote plugin's refreshed OAuth
/// access tokens join them. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Secrets(Arc<RwLock<Arc<[Secret]>>>);

impl Secrets {
    /// `secrets`, fixed.
    pub fn new(secrets: impl Into<Arc<[Secret]>>) -> Self {
        Self(Arc::new(RwLock::new(secrets.into())))
    }

    /// The current values.
    pub fn get(&self) -> Arc<[Secret]> {
        Arc::clone(&self.0.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// `text` with every current value replaced ([`scrub`]).
    pub fn scrub(&self, text: &str) -> String {
        let secrets = self.get();
        let secrets: Vec<&Secret> = secrets.iter().collect();
        scrub(text, &secrets)
    }

    /// Adds `added` and drops `removed` (an access token two refreshes old).
    pub(crate) fn replace(&self, removed: Option<&Secret>, added: Secret) {
        let mut secrets = self.0.write().unwrap_or_else(PoisonError::into_inner);
        let updated: Arc<[Secret]> = secrets
            .iter()
            .filter(|secret| Some(*secret) != removed)
            .cloned()
            .chain([added])
            .collect();
        *secrets = updated;
    }
}

impl super::Launcher {
    /// Connects to the remote plugin channel `instance` and configures it: the OAuth token if the
    /// instance uses client credentials, TCP and TLS ([`REMOTE_CONNECT_TIMEOUT`]), then
    /// `Describe` and `Configure` as for a launched plugin. Logs under a
    /// `plugin{kind=channel instance=...}` span.
    ///
    /// # Errors
    ///
    /// [`LaunchError`], messages scrubbed of the instance's secrets.
    pub async fn connect_remote(
        &self,
        instance: &ChannelInstance,
    ) -> Result<RemotePlugin, LaunchError> {
        let ChannelRuntime::Remote(remote) = &instance.runtime else {
            return Err(LaunchError::NotRemote);
        };
        let span = tracing::info_span!("plugin", kind = %Kind::Channel, instance = %instance.name);
        async {
            let mut secrets: Vec<Secret> = instance.config.secrets().cloned().collect();
            let mut refresh = None;
            let authorization = match &remote.auth {
                None => Authorization::None,
                Some(Auth::Bearer(token)) => {
                    secrets.push(token.clone());
                    Authorization::bearer(token).map_err(|_| {
                        LaunchError::Auth("the bearer token is not a valid header value".into())
                    })?
                }
                Some(Auth::OAuth(credentials)) => {
                    secrets.push(credentials.client_secret.clone());
                    let scrubbed = |error: String| {
                        let secrets: Vec<&Secret> = secrets.iter().collect();
                        LaunchError::Auth(scrub(&error, &secrets))
                    };
                    let client = TokenClient::new((**credentials).clone()).map_err(scrubbed)?;
                    let token = client.fetch().await.map_err(scrubbed)?;
                    secrets.push(token.value.clone());
                    let header = Arc::new(RwLock::new(token.header()));
                    refresh = Some((client, Arc::clone(&header), token.expires_in, token.value));
                    Authorization::Shared(header)
                }
            };
            let secrets = Secrets::new(secrets);
            let refresh = refresh.map(|(client, header, expires_in, first)| {
                oauth::keep_fresh(client, header, expires_in, first, secrets.clone())
            });

            let target = Target::new(&remote.url, self.remote_roots.clone())
                .map_err(LaunchError::Connect)?;
            let closed = CancellationToken::new();
            let disconnect = CancellationToken::new();
            let disconnect_guard = disconnect.clone().drop_guard();
            let io = tokio::time::timeout(REMOTE_CONNECT_TIMEOUT, target.dial())
                .await
                .map_err(|_| {
                    LaunchError::Connect(format!("no connection within {REMOTE_CONNECT_TIMEOUT:?}"))
                })?
                .map_err(|e| LaunchError::Connect(e.to_string()))?;
            let io = Tracked::new(io, closed.clone(), disconnect);
            let endpoint = Endpoint::from_shared(format!("http://{}", target.authority()))
                .map_err(|e| LaunchError::Connect(e.to_string()))?
                .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
                .keep_alive_timeout(KEEPALIVE_TIMEOUT)
                .keep_alive_while_idle(true);
            let connection = PluginConnection::over(
                endpoint,
                io,
                "the connection to the remote plugin is closed; it must be reconnected",
                CallMetadata::with(authorization),
            )
            .await
            .map_err(|e| LaunchError::Connect(e.to_string()))?;
            let (describe, identity_namespace) =
                describe_and_configure(&connection, instance, &secrets.get()).await?;
            info!(
                url = %remote.url,
                version = %describe.version,
                identity_namespace,
                "remote plugin connected",
            );
            Ok(RemotePlugin {
                instance: instance.name.clone(),
                component: instance.component.clone(),
                url: remote.url.clone(),
                describe,
                identity_namespace,
                connection,
                secrets,
                span: tracing::Span::current(),
                closed,
                _disconnect: disconnect_guard,
                _refresh: refresh,
            })
        }
        .instrument(span)
        .await
    }
}

/// Where a remote plugin is and how to reach it.
struct Target {
    host: Host<String>,
    port: u16,
    /// TLS (for `https`): the connector and the name the certificate must carry.
    tls: Option<(TlsConnector, ServerName<'static>)>,
}

impl Target {
    /// `url` is `http` or `https` with a host (the config loader checked). `https` trusts `roots`,
    /// or the bundled Mozilla roots.
    fn new(url: &Url, roots: Option<Arc<RootCertStore>>) -> Result<Self, String> {
        let host = url
            .host()
            .ok_or_else(|| "the address has no host".to_owned())?
            .to_owned();
        let port = url
            .port_or_known_default()
            .ok_or_else(|| "the address has no port".to_owned())?;
        let tls = match url.scheme() {
            "http" => None,
            "https" => {
                let name = match &host {
                    Host::Domain(domain) => ServerName::try_from(domain.clone())
                        .map_err(|e| format!("the host is not a valid TLS server name: {e}"))?,
                    Host::Ipv4(ip) => ServerName::from(std::net::IpAddr::from(*ip)),
                    Host::Ipv6(ip) => ServerName::from(std::net::IpAddr::from(*ip)),
                };
                let mut config = match roots {
                    Some(roots) => giles_plugin_sdk::tls::client_config_with_roots(roots),
                    None => giles_plugin_sdk::tls::client_config(),
                };
                config.alpn_protocols = vec![b"h2".to_vec()];
                Some((TlsConnector::from(Arc::new(config)), name))
            }
            other => return Err(format!("unsupported scheme {other}")),
        };
        Ok(Self { host, port, tls })
    }

    /// `host:port` for the `:authority` header.
    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Connects TCP (to each address of a domain in turn) and, for `https`, does the TLS
    /// handshake.
    async fn dial(&self) -> io::Result<RemoteIo> {
        let tcp = match &self.host {
            Host::Domain(domain) => TcpStream::connect((domain.as_str(), self.port)).await?,
            Host::Ipv4(ip) => TcpStream::connect(SocketAddr::from((*ip, self.port))).await?,
            Host::Ipv6(ip) => TcpStream::connect(SocketAddr::from((*ip, self.port))).await?,
        };
        tcp.set_nodelay(true)?;
        match &self.tls {
            None => Ok(RemoteIo::Plain(tcp)),
            Some((connector, name)) => {
                let tls = connector.connect(name.clone(), tcp).await?;
                Ok(RemoteIo::Tls(Box::new(tls)))
            }
        }
    }
}

/// A connection to a remote plugin.
enum RemoteIo {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

/// A remote plugin's connection, as hyper uses it. Cancelling `disconnect` (the owning
/// [`RemotePlugin`] stopped or dropped) makes every read and write fail with `ConnectionAborted`
/// and wakes a pending read, so hyper ends the HTTP/2 connection however many clients still
/// share it. Dropping it, which hyper does when the connection ends for any reason, cancels
/// `closed`.
struct Tracked {
    io: RemoteIo,
    disconnect: CancellationToken,
    /// Wakes a pending read once `disconnect` is cancelled.
    disconnected: Pin<Box<WaitForCancellationFutureOwned>>,
    _closed: DropGuard,
}

impl Tracked {
    fn new(io: RemoteIo, closed: CancellationToken, disconnect: CancellationToken) -> Self {
        Self {
            io,
            disconnected: Box::pin(disconnect.clone().cancelled_owned()),
            disconnect,
            _closed: closed.drop_guard(),
        }
    }

    /// The error once the core has disconnected.
    fn check(&self) -> io::Result<()> {
        if self.disconnect.is_cancelled() {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "the core disconnected from the remote plugin",
            ))
        } else {
            Ok(())
        }
    }
}

impl AsyncRead for Tracked {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.disconnected.as_mut().poll(cx).is_ready() {
            return Poll::Ready(this.check());
        }
        match &mut this.io {
            RemoteIo::Plain(tcp) => Pin::new(tcp).poll_read(cx, buf),
            RemoteIo::Tls(tls) => Pin::new(tls.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Tracked {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.check()?;
        match &mut this.io {
            RemoteIo::Plain(tcp) => Pin::new(tcp).poll_write(cx, buf),
            RemoteIo::Tls(tls) => Pin::new(tls.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.check()?;
        match &mut this.io {
            RemoteIo::Plain(tcp) => Pin::new(tcp).poll_write_vectored(cx, bufs),
            RemoteIo::Tls(tls) => Pin::new(tls.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match &self.io {
            RemoteIo::Plain(tcp) => tcp.is_write_vectored(),
            RemoteIo::Tls(tls) => tls.is_write_vectored(),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.check()?;
        match &mut this.io {
            RemoteIo::Plain(tcp) => Pin::new(tcp).poll_flush(cx),
            RemoteIo::Tls(tls) => Pin::new(tls.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.get_mut().io {
            RemoteIo::Plain(tcp) => Pin::new(tcp).poll_shutdown(cx),
            RemoteIo::Tls(tls) => Pin::new(tls.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(url: &str) -> Result<Target, String> {
        Target::new(&url.parse().unwrap(), None)
    }

    #[test]
    fn targets_come_from_the_address() {
        let plain = target("http://127.0.0.1:7000").unwrap();
        assert_eq!(plain.authority(), "127.0.0.1:7000");
        assert!(plain.tls.is_none());

        let tls = target("https://plugins.example.com").unwrap();
        assert_eq!(tls.authority(), "plugins.example.com:443");
        let (_, name) = tls.tls.unwrap();
        assert_eq!(name.to_str(), "plugins.example.com");

        let v6 = target("https://[::1]:7443/").unwrap();
        assert_eq!(v6.authority(), "[::1]:7443");
        assert_eq!(v6.tls.unwrap().1.to_str(), "::1");

        assert_eq!(
            target("http://localhost").unwrap().authority(),
            "localhost:80"
        );
    }

    #[tokio::test]
    async fn a_closed_port_fails_to_dial() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let target = target(&format!("http://{addr}")).unwrap();
        assert!(target.dial().await.is_err());
    }

    #[tokio::test]
    async fn a_disconnect_fails_a_pending_read_and_every_write() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, server) = tokio::join!(
            TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        let (mut server, _) = server.unwrap();
        let (closed, disconnect) = (CancellationToken::new(), CancellationToken::new());
        let mut tracked = Tracked::new(
            RemoteIo::Plain(client.unwrap()),
            closed.clone(),
            disconnect.clone(),
        );
        tracked.write_all(b"ping").await.unwrap();
        let mut buf = [0; 4];
        server.read_exact(&mut buf).await.unwrap();

        // A read waiting for data the peer never sends wakes up when the core disconnects.
        let read = tokio::spawn(async move {
            let mut buf = [0; 1];
            let result = tracked.read(&mut buf).await;
            (tracked, result)
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!read.is_finished());
        disconnect.cancel();
        let (mut tracked, result) = tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::ConnectionAborted);
        let error = tracked.write_all(b"more").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert!(!closed.is_cancelled());
        drop(tracked);
        assert!(
            closed.is_cancelled(),
            "dropping the stream reports it closed"
        );
    }

    #[test]
    fn secrets_replace_old_tokens() {
        let secrets = Secrets::new(vec![Secret::new("client-secret"), Secret::new("token-1")]);
        let copy = secrets.clone();
        secrets.replace(None, Secret::new("token-2"));
        secrets.replace(Some(&Secret::new("token-1")), Secret::new("token-3"));
        assert_eq!(
            copy.scrub("client-secret token-1 token-2 token-3"),
            "<redacted> token-1 <redacted> <redacted>"
        );
    }
}
