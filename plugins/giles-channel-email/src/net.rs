//! TCP connections and TLS (rustls with the SDK's bundled roots) for IMAP and SMTP.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use giles_plugin_sdk::giles_types::{Error, Result};
use giles_plugin_sdk::tls::rustls::ClientConfig;
use giles_plugin_sdk::tls::rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// How long connecting (TCP and the TLS handshake) may take.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A connection, plain or TLS.
pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + fmt::Debug> Io for T {}

pub(crate) type Stream = Box<dyn Io>;

/// Opens a TCP connection to `host:port`.
pub(crate) async fn connect(host: &str, port: u16) -> Result<Stream> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .map_err(|_| Error::unavailable(format!("connecting to {host}:{port} timed out")))?
        .map_err(|e| Error::unavailable(format!("connecting to {host}:{port}: {e}")))?;
    tcp.set_nodelay(true).map_err(Error::other)?;
    Ok(Box::new(tcp))
}

/// Starts TLS on `stream`, verifying the server's certificate for `host`.
pub(crate) async fn tls(stream: Stream, host: &str, config: &Arc<ClientConfig>) -> Result<Stream> {
    let name = ServerName::try_from(host.to_owned())
        .map_err(|e| Error::InvalidRequest(format!("{host:?} is not a TLS server name: {e}")))?;
    let tls = tokio::time::timeout(
        CONNECT_TIMEOUT,
        TlsConnector::from(Arc::clone(config)).connect(name, stream),
    )
    .await
    .map_err(|_| Error::unavailable(format!("the TLS handshake with {host} timed out")))?
    .map_err(|e| Error::unavailable(format!("the TLS handshake with {host}: {e}")))?;
    Ok(Box::new(tls))
}
