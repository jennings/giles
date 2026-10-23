//! The client side, for the `giles` CLI: connect to the running core's admin socket with its token.

use std::io;
use std::path::PathBuf;

use giles_proto::MAX_MESSAGE_SIZE;
use giles_proto::admin::v1::admin_service_client::AdminServiceClient;
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint, Uri};

use super::auth::{BearerToken, read_token};
use crate::paths::{DataRoot, PathsError};

/// A client of the running core's admin service; every call carries the admin token.
pub type AdminClient = AdminServiceClient<InterceptedService<Channel, BearerToken>>;

/// The admin service couldn't be reached.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// No core is listening on the socket (it's missing, or left behind by a core that died).
    #[error("giles is not running (nothing listens on {})", .socket.display())]
    NotRunning {
        /// The admin socket.
        socket: PathBuf,
    },
    /// The socket directory failed its check (PLAN section 11.1's `$TMPDIR` fallback).
    #[error(transparent)]
    SocketDir(PathsError),
    /// Connecting failed some other way, e.g. permission denied.
    #[error("cannot connect to {}: {source}", .socket.display())]
    Connect {
        /// The admin socket.
        socket: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The token file couldn't be read, or holds no valid token.
    #[error("cannot read the admin token {}: {source}", .path.display())]
    Token {
        /// `run/admin.token`.
        path: PathBuf,
        /// Why.
        source: io::Error,
    },
    /// The gRPC connection couldn't be set up.
    #[error("cannot set up the admin connection: {0}")]
    Transport(#[source] tonic::transport::Error),
}

/// Connects to the admin service of the core running on `root`: checks the socket directory
/// ([`DataRoot::check_socket_dir`]), connects to [`DataRoot::admin_socket`], and reads the token
/// from [`DataRoot::admin_token_file`]. The token is read only after the socket answers, so it's
/// the one the running core wrote.
///
/// # Errors
///
/// [`ConnectError::NotRunning`] when nothing listens; other [`ConnectError`]s otherwise.
pub async fn connect(root: &DataRoot) -> Result<AdminClient, ConnectError> {
    let socket = root.admin_socket();
    match root.check_socket_dir() {
        Ok(()) => {}
        // A fallback socket directory that doesn't exist: the core never ran.
        Err(PathsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ConnectError::NotRunning { socket });
        }
        Err(err) => return Err(ConnectError::SocketDir(err)),
    }
    let stream = match UnixStream::connect(&socket).await {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(ConnectError::NotRunning { socket });
        }
        Err(source) => return Err(ConnectError::Connect { socket, source }),
    };
    let path = root.admin_token_file();
    let bearer = read_token(&path)
        .and_then(|token| BearerToken::new(&token).map_err(io::Error::other))
        .map_err(|source| ConnectError::Token { path, source })?;
    let mut stream = Some(stream);
    // The URI is never resolved; it only sets `:authority`.
    let channel = Endpoint::from_static("http://admin")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let stream = stream.take();
            async move {
                stream.map(TokioIo::new).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotConnected,
                        "the admin connection is closed",
                    )
                })
            }
        }))
        .await
        .map_err(ConnectError::Transport)?;
    Ok(AdminServiceClient::with_interceptor(channel, bearer)
        .max_decoding_message_size(MAX_MESSAGE_SIZE)
        .max_encoding_message_size(MAX_MESSAGE_SIZE))
}
