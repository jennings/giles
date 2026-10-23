//! In-process gRPC over a socket pair, for unit tests of the services.

use std::io;

use futures::stream::{self, StreamExt as _};
use hyper_util::rt::TokioIo;
use tokio::net::UnixStream;
use tonic::transport::server::Router;
use tonic::transport::{Channel, Endpoint, Uri};

/// Serves `router` on one end of a socket pair, as the plugin does on fd 3, and returns a client
/// channel on the other end. The server runs until the test's runtime ends.
pub(crate) async fn serve(router: Router) -> Channel {
    let (core, plugin) = UnixStream::pair().unwrap();
    let incoming = stream::iter([Ok::<_, io::Error>(plugin)]).chain(stream::pending());
    tokio::spawn(router.serve_with_incoming(incoming));
    let mut core = Some(core);
    Endpoint::from_static("http://plugin")
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let io = core.take();
            async move {
                io.map(TokioIo::new)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "used"))
            }
        }))
        .await
        .unwrap()
}
