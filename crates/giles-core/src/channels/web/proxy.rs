//! The trusted-proxy listener (PLAN section 3.4): a reverse proxy that authenticates people
//! (Tailscale, oauth2-proxy, Authelia) connects here and names the user in a header.
//!
//! The header is believed only on this listener, never by source address: any local process,
//! network-enabled plugins included, can connect to the main listener from loopback. The listener
//! is either the Unix socket `run/proxy.sock` (mode 0600, in the 0700 socket directory, so only
//! Giles's own user can connect) or a TCP address where every request must carry the shared
//! secret header the proxy adds. Requests that pass are marked [`TrustedProxy`] and then served by
//! the same application as the main listener.

use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderName, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, UnixListener};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::auth::TrustedProxy;
use super::config::ProxyListen;
use super::pages::WebApp;
use crate::http;
use crate::paths::{DataRoot, PathsError};

/// The trusted-proxy listener could not be opened.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProxyError {
    /// The socket directory is unsafe (its fallback under `$TMPDIR` is shared or not ours).
    #[error("cannot use the socket directory for the trusted-proxy socket")]
    SocketDir(#[source] PathsError),
    /// Another process is serving the socket: another Giles on the same data root.
    #[error("{} is in use: is another giles running with this data root?", .0.display())]
    InUse(PathBuf),
    /// Something other than a socket is at the socket's path.
    #[error("{} exists and is not a socket; remove it", .0.display())]
    NotASocket(PathBuf),
    /// Binding or setting up the listener failed.
    #[error("cannot listen for the trusted proxy on {at}")]
    Bind {
        /// The socket path or TCP address.
        at: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

/// An open trusted-proxy listener ([`WebApp::bind_proxy`]).
#[derive(Debug)]
pub enum ProxyListener {
    /// `run/proxy.sock`.
    Unix {
        /// The socket.
        listener: UnixListener,
        /// Its path, removed when serving ends.
        path: PathBuf,
    },
    /// A TCP listener requiring the shared secret.
    Tcp(TcpListener),
}

impl ProxyListener {
    /// Where the proxy connects, for logs: the socket path or `<ip>:<port>`.
    pub fn describe(&self) -> String {
        match self {
            Self::Unix { path, .. } => path.display().to_string(),
            Self::Tcp(listener) => listener
                .local_addr()
                .map_or_else(|_| "TCP".to_owned(), |addr| addr.to_string()),
        }
    }

    /// Serves `app` ([`WebApp::proxy_app`]) until `shutdown`, like [`http::serve`]; then removes
    /// the socket file.
    ///
    /// # Errors
    ///
    /// Only if accepting connections fails for good.
    pub async fn serve(self, app: Router, shutdown: CancellationToken) -> io::Result<()> {
        match self {
            Self::Unix { listener, path } => {
                let result = http::serve(listener, app, shutdown).await;
                if let Err(error) = fs::remove_file(&path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    warn!(path = %path.display(), %error, "cannot remove the trusted-proxy socket");
                }
                result
            }
            Self::Tcp(listener) => http::serve(listener, app, shutdown).await,
        }
    }
}

/// Opens the Unix socket at `path` with mode 0600, replacing a stale socket a previous run left
/// behind, but never one that another process still serves.
fn bind_unix(path: &Path) -> Result<UnixListener, ProxyError> {
    let bind_error = |source| ProxyError::Bind {
        at: path.display().to_string(),
        source,
    };
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_socket() => {
            return Err(ProxyError::NotASocket(path.to_owned()));
        }
        Ok(_) => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(ProxyError::InUse(path.to_owned()));
            }
            fs::remove_file(path).map_err(bind_error)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(bind_error(error)),
    }
    let listener = std::os::unix::net::UnixListener::bind(path).map_err(bind_error)?;
    // The socket directory is 0700 already; the mode keeps the socket private if it isn't.
    fs::set_permissions(path, Permissions::from_mode(0o600)).map_err(bind_error)?;
    listener.set_nonblocking(true).map_err(bind_error)?;
    UnixListener::from_std(listener).map_err(bind_error)
}

/// The shared secret a TCP proxy listener requires.
struct SharedSecret {
    header: HeaderName,
    /// SHA-256 of the value, so comparing takes the same time however much of it matched.
    digest: [u8; 32],
}

/// Marks requests through the socket as trusted.
async fn mark_trusted(mut request: Request, next: Next) -> Response {
    request.extensions_mut().insert(TrustedProxy);
    next.run(request).await
}

/// Marks requests with the right secret header as trusted, and refuses the rest.
async fn require_secret(
    State(secret): State<Arc<SharedSecret>>,
    mut request: Request,
    next: Next,
) -> Response {
    let given = request
        .headers()
        .get(&secret.header)
        .map(|value| <[u8; 32]>::from(Sha256::digest(value.as_bytes())));
    if given != Some(secret.digest) {
        warn!(
            header = %secret.header,
            "refused a request on the trusted-proxy listener without the shared secret",
        );
        return (
            StatusCode::FORBIDDEN,
            "The trusted-proxy listener needs the shared secret header.\n",
        )
            .into_response();
    }
    request.extensions_mut().insert(TrustedProxy);
    next.run(request).await
}

impl WebApp {
    /// Opens the trusted-proxy listener `[channel.<web>] config.proxy` configures, if any:
    /// `root`'s proxy socket (after [`DataRoot::check_socket_dir`]) or the TCP address. The daemon
    /// calls it before it reports itself started, so a failure is a startup error.
    ///
    /// # Errors
    ///
    /// [`ProxyError`].
    pub async fn bind_proxy(&self, root: &DataRoot) -> Result<Option<ProxyListener>, ProxyError> {
        let Some(proxy) = &self.inner.channel.config().proxy else {
            return Ok(None);
        };
        let listener = match &proxy.listen {
            ProxyListen::Unix => {
                root.check_socket_dir().map_err(ProxyError::SocketDir)?;
                let path = root.proxy_socket();
                ProxyListener::Unix {
                    listener: bind_unix(&path)?,
                    path,
                }
            }
            ProxyListen::Tcp { addr, .. } => {
                ProxyListener::Tcp(TcpListener::bind(addr).await.map_err(|source| {
                    ProxyError::Bind {
                        at: addr.to_string(),
                        source,
                    }
                })?)
            }
        };
        Ok(Some(listener))
    }

    /// `app` (the main listener's application) for the trusted-proxy listener: requests are
    /// marked trusted (`auth::TrustedProxy`), after the shared secret check on a TCP listener.
    /// Without a configured proxy, `app` unchanged.
    pub fn proxy_app(&self, app: Router) -> Router {
        let Some(proxy) = &self.inner.channel.config().proxy else {
            return app;
        };
        match &proxy.listen {
            ProxyListen::Unix => app.layer(axum::middleware::from_fn(mark_trusted)),
            ProxyListen::Tcp {
                secret_header,
                secret,
                ..
            } => {
                let secret = Arc::new(SharedSecret {
                    header: secret_header.clone(),
                    digest: Sha256::digest(secret.expose().as_bytes()).into(),
                });
                app.layer(from_fn_with_state(secret, require_secret))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    #[tokio::test]
    async fn the_socket_is_private_and_replaces_only_a_stale_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy.sock");

        let listener = bind_unix(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        // Served: a second bind refuses to take it over.
        assert!(matches!(bind_unix(&path), Err(ProxyError::InUse(p)) if p == path));
        drop(listener);

        // Left behind by a previous run: replaced.
        assert!(path.exists());
        let _listener = bind_unix(&path).unwrap();

        let file = dir.path().join("file");
        fs::write(&file, "not a socket").unwrap();
        assert!(matches!(bind_unix(&file), Err(ProxyError::NotASocket(_))));
        assert_eq!(fs::read_to_string(&file).unwrap(), "not a socket");
    }
}
