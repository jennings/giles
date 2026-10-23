//! The admin socket: binding `run/admin.sock` and serving the [`AdminService`] on it.

use std::fs::{self, Permissions};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use giles_proto::MAX_MESSAGE_SIZE;
use giles_proto::admin::v1::admin_service_server::AdminServiceServer;
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::Server;
use tracing::warn;

use super::auth::{RequireToken, generate_token, write_token};
use super::lock::InstanceLock;
use super::service::AdminService;
use crate::paths::{DataRoot, PathsError};

/// How long a shutdown waits for open admin calls before dropping them.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the accept loop pauses after a failed `accept` (e.g. out of file descriptors), so it
/// doesn't spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// The bound admin socket, with the token every call must carry.
#[derive(Debug)]
pub struct AdminListener {
    listener: UnixListener,
    socket: PathBuf,
    token_file: PathBuf,
    token: String,
}

/// The admin socket couldn't be set up.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BindError {
    /// The socket directory failed its check.
    #[error(transparent)]
    SocketDir(#[from] PathsError),
    /// The token couldn't be generated or written.
    #[error("cannot write the admin token {}: {source}", .path.display())]
    Token {
        /// `run/admin.token`.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The socket couldn't be bound or made private.
    #[error("cannot bind the admin socket {}: {source}", .path.display())]
    Bind {
        /// The socket path.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
}

impl AdminListener {
    /// Writes a fresh token to `run/admin.token` (mode 0600), removes the socket a core that
    /// didn't stop cleanly left behind, and binds `admin.sock` (mode 0600, in a 0700 directory).
    /// Holding `lock` proves no other core serves this data root, so an existing socket is stale.
    /// Must run inside a Tokio runtime.
    ///
    /// # Errors
    ///
    /// [`BindError`].
    pub fn bind(root: &DataRoot, _lock: &InstanceLock) -> Result<Self, BindError> {
        root.check_socket_dir()?;
        let token_file = root.admin_token_file();
        let token = generate_token()
            .and_then(|token| write_token(&token_file, &token).map(|()| token))
            .map_err(|source| BindError::Token {
                path: token_file.clone(),
                source,
            })?;
        let socket = root.admin_socket();
        let bind_error = |source| BindError::Bind {
            path: socket.clone(),
            source,
        };
        match fs::remove_file(&socket) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(bind_error(err)),
        }
        let listener = UnixListener::bind(&socket).map_err(bind_error)?;
        // The directory is already private; this keeps the socket private wherever it's moved.
        fs::set_permissions(&socket, Permissions::from_mode(0o600)).map_err(bind_error)?;
        Ok(Self {
            listener,
            socket,
            token_file,
            token,
        })
    }

    /// The socket path.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Serves `service` until `shutdown` is cancelled, then waits up to [`DRAIN_TIMEOUT`] for
    /// open calls and removes the socket and the token file. The service is shared: the web
    /// admin pages call the same one in-process.
    ///
    /// # Errors
    ///
    /// If the gRPC server fails.
    pub async fn serve(
        self,
        service: Arc<AdminService>,
        shutdown: CancellationToken,
    ) -> Result<(), tonic::transport::Error> {
        let Self {
            listener,
            socket,
            token_file,
            token,
        } = self;
        let incoming = futures::stream::unfold(listener, |listener| async move {
            let accepted = match listener.accept().await {
                Ok((stream, _)) => Ok(stream),
                Err(err) => {
                    warn!(error = %err, "cannot accept an admin connection");
                    tokio::time::sleep(ACCEPT_BACKOFF).await;
                    Err(err)
                }
            };
            Some((accepted, listener))
        });
        let service = AdminServiceServer::from_arc(service)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);
        let server = Server::builder()
            .add_service(InterceptedService::new(service, RequireToken::new(&token)))
            .serve_with_incoming_shutdown(incoming, shutdown.clone().cancelled_owned());
        let result = tokio::select! {
            result = server => result,
            () = async {
                shutdown.cancelled().await;
                tokio::time::sleep(DRAIN_TIMEOUT).await;
            } => Ok(()),
        };
        for path in [&socket, &token_file] {
            if let Err(err) = fs::remove_file(path)
                && err.kind() != io::ErrorKind::NotFound
            {
                warn!(path = %path.display(), error = %err, "cannot remove");
            }
        }
        result
    }
}
