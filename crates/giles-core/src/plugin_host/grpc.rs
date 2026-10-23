//! The core's gRPC client to a plugin: over its end of the socket pair (PLAN section 7.4,
//! `Docs/technical/plugin-transport.md`) or a remote plugin's TCP connection (section 7.6), with
//! a one-shot connector, 16 MiB limits, and per-call metadata (the bearer token and a W3C
//! `traceparent`).

use std::io;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::{Arc, PoisonError, RwLock};

use giles_proto::MAX_MESSAGE_SIZE;
use giles_proto::channel::v1::channel_service_client::ChannelServiceClient;
use giles_proto::plugin::v1::plugin_service_client::PluginServiceClient;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::{Request, Status};
use tonic_health::pb::health_client::HealthClient;

use crate::config::Secret;

/// The transport every client to a launched plugin uses: its one connection, with
/// [`CallMetadata`] added to every call.
pub type PluginTransport = InterceptedService<Channel, CallMetadata>;

/// The gRPC connection to one launched plugin. Cheap to clone; every client shares the one HTTP/2
/// connection. There is no reconnect: once it breaks, every call fails with `UNAVAILABLE`.
#[derive(Clone)]
pub struct PluginConnection {
    channel: Channel,
    metadata: CallMetadata,
}

impl std::fmt::Debug for PluginConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginConnection").finish_non_exhaustive()
    }
}

impl PluginConnection {
    /// Connects over `core_end`, authenticating every call with `token`.
    pub(crate) async fn connect(
        core_end: StdUnixStream,
        token: &Secret,
    ) -> Result<Self, ConnectError> {
        core_end.set_nonblocking(true).map_err(ConnectError::Io)?;
        let io = UnixStream::from_std(core_end).map_err(ConnectError::Io)?;
        let metadata = CallMetadata::new(token).map_err(ConnectError::Token)?;
        // The URI is never resolved; it only sets `:authority`.
        Self::over(
            Endpoint::from_static("http://plugin"),
            io,
            "the plugin connection is closed; the plugin must be relaunched",
            metadata,
        )
        .await
    }

    /// The one HTTP/2 connection over `io`, made with `endpoint`'s settings. There is no
    /// reconnect: tonic's later connection attempts fail with `closed`.
    pub(crate) async fn over<IO>(
        endpoint: Endpoint,
        io: IO,
        closed: &'static str,
        metadata: CallMetadata,
    ) -> Result<Self, ConnectError>
    where
        IO: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let mut io = Some(io);
        let channel = endpoint
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let io = io.take();
                async move {
                    io.map(TokioIo::new)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, closed))
                }
            }))
            .await
            .map_err(ConnectError::Transport)?;
        Ok(Self { channel, metadata })
    }

    /// The connection with [`CallMetadata`] on every call, for clients this type doesn't build
    /// (give them the 16 MiB limits too).
    pub fn transport(&self) -> PluginTransport {
        InterceptedService::new(self.channel.clone(), self.metadata.clone())
    }

    /// A `giles.plugin.v1.PluginService` client.
    pub fn plugin(&self) -> PluginServiceClient<PluginTransport> {
        PluginServiceClient::new(self.transport())
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
    }

    /// A `giles.channel.v1.ChannelService` client.
    pub fn channel(&self) -> ChannelServiceClient<PluginTransport> {
        ChannelServiceClient::new(self.transport())
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
    }

    /// A `grpc.health.v1.Health` client.
    pub fn health(&self) -> HealthClient<PluginTransport> {
        HealthClient::new(self.transport())
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
    }
}

/// Connecting failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConnectError {
    #[error("{0}")]
    Io(io::Error),
    #[error("{0}")]
    Transport(tonic::transport::Error),
    #[error("the token is not a valid header value")]
    Token(tonic::metadata::errors::InvalidMetadataValue),
}

/// Adds the per-call metadata: `authorization: Bearer <token>` (unless a remote plugin is
/// configured without `auth`) and a W3C `traceparent`.
///
/// The core has no OpenTelemetry layer yet (PLAN section 13.4: export later), so its `tracing`
/// spans carry no trace ids to propagate. Until then every call gets a fresh random trace id and
/// parent id, sampled flag off, and the value is logged at debug level in the caller's span, which
/// ties the core's log of the call to whatever the plugin logs for it. With OpenTelemetry, this
/// interceptor injects the current span's context instead.
#[derive(Clone)]
pub struct CallMetadata {
    authorization: Authorization,
}

/// The `authorization` header a connection's calls carry.
#[derive(Clone)]
pub(crate) enum Authorization {
    /// None: a remote plugin without `auth`.
    None,
    /// The same `Bearer <token>` on every call: a launched plugin's token, a remote plugin's
    /// static bearer token.
    Fixed(MetadataValue<Ascii>),
    /// The current value, which changes when an OAuth access token is refreshed.
    Shared(Arc<RwLock<MetadataValue<Ascii>>>),
}

impl Authorization {
    /// [`Authorization::Fixed`] with `Bearer <token>`, marked sensitive.
    pub(crate) fn bearer(
        token: &Secret,
    ) -> Result<Self, tonic::metadata::errors::InvalidMetadataValue> {
        let mut value: MetadataValue<Ascii> = format!("Bearer {}", token.expose()).parse()?;
        value.set_sensitive(true);
        Ok(Self::Fixed(value))
    }
}

impl CallMetadata {
    fn new(token: &Secret) -> Result<Self, tonic::metadata::errors::InvalidMetadataValue> {
        Ok(Self::with(Authorization::bearer(token)?))
    }

    /// Metadata with `authorization`.
    pub(crate) fn with(authorization: Authorization) -> Self {
        Self { authorization }
    }
}

impl Interceptor for CallMetadata {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let metadata = request.metadata_mut();
        match &self.authorization {
            Authorization::None => {}
            Authorization::Fixed(value) => {
                metadata.insert("authorization", value.clone());
            }
            Authorization::Shared(value) => {
                let value = value.read().unwrap_or_else(PoisonError::into_inner).clone();
                metadata.insert("authorization", value);
            }
        }
        if let Some(traceparent) = traceparent() {
            tracing::debug!(%traceparent, "plugin call");
            if let Ok(value) = traceparent.parse() {
                metadata.insert("traceparent", value);
            }
        }
        Ok(request)
    }
}

/// A random `traceparent` (version 00, not sampled), or `None` if the OS has no randomness.
fn traceparent() -> Option<String> {
    let mut ids = [0_u8; 24];
    getrandom::fill(&mut ids).ok()?;
    // All-zero ids are invalid; with 128 random bits that can't happen in practice, but stay valid.
    ids[15] |= 1;
    ids[23] |= 1;
    Some(format!("00-{}-{}-00", hex(&ids[..16]), hex(&ids[16..])))
}

/// Lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_has_the_w3c_layout() {
        let value = traceparent().unwrap();
        let parts: Vec<&str> = value.split('-').collect();
        assert_eq!(parts.len(), 4, "{value}");
        assert_eq!(parts[0], "00");
        assert_eq!(parts[1].len(), 32);
        assert_eq!(parts[2].len(), 16);
        assert_eq!(parts[3], "00");
        assert!(
            value[3..]
                .chars()
                .all(|c| c == '-' || c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(traceparent().unwrap(), value);
    }

    #[test]
    fn metadata_carries_the_bearer_token_and_a_traceparent() {
        let mut metadata = CallMetadata::new(&Secret::new("tok-1")).unwrap();
        let request = metadata.call(Request::new(())).unwrap();
        assert_eq!(
            request.metadata().get("authorization").unwrap(),
            "Bearer tok-1"
        );
        assert!(request.metadata().get("traceparent").is_some());
    }

    #[test]
    fn shared_authorization_follows_the_current_value_and_none_sends_none() {
        let shared = Arc::new(RwLock::new("Bearer a".parse().unwrap()));
        let mut metadata = CallMetadata::with(Authorization::Shared(Arc::clone(&shared)));
        let header = |metadata: &mut CallMetadata| {
            let request = metadata.call(Request::new(())).unwrap();
            request
                .metadata()
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned())
        };
        assert_eq!(header(&mut metadata).as_deref(), Some("Bearer a"));
        *shared.write().unwrap() = "Bearer b".parse().unwrap();
        assert_eq!(header(&mut metadata).as_deref(), Some("Bearer b"));
        assert_eq!(header(&mut CallMetadata::with(Authorization::None)), None);
    }
}
