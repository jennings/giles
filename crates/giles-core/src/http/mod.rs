//! The core's HTTP server (PLAN section 9): web chat, admin pages, and login all share it.
//!
//! [`RouterBuilder`] assembles the application: the core routes (the home page and the embedded
//! static assets), routers merged in by other modules, a 404 page for everything else, and the
//! security headers on every response. [`bind`] opens the `core.http_listen` socket and [`serve`]
//! runs the server until the daemon's shutdown token is cancelled.
//!
//! Design notes, the header set, and the vendored asset versions are in
//! `Docs/technical/http.md`.

pub mod assets;
mod headers;
mod pages;
#[cfg(test)]
mod tests;

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::middleware::map_response;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::warn;

pub use pages::render;

/// How long [`serve`] lets in-flight requests finish after shutdown begins before it returns
/// anyway. Long-lived responses (server-sent events) should end on the shutdown token themselves;
/// this bounds the ones that don't.
pub const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Assembles the HTTP application.
///
/// Other modules add their pages by building a [`Router`] with its state already applied
/// (`Router::new().route(..).with_state(state)`) and passing it to [`merge`](Self::merge). Merged
/// routers must not overlap the core routes or each other (axum panics on a duplicate route) and
/// must not set a fallback. [`build`](Self::build) adds the 404 fallback and the security headers
/// last, so they cover every merged route.
#[derive(Debug)]
pub struct RouterBuilder {
    router: Router,
}

impl RouterBuilder {
    /// The core routes: the home page at `/` and the static assets under `/static/`.
    pub fn new() -> Self {
        Self {
            router: Router::new()
                .route("/", get(pages::index))
                .merge(assets::routes()),
        }
    }

    /// Adds `routes` to the application.
    pub fn merge(mut self, routes: Router) -> Self {
        self.router = self.router.merge(routes);
        self
    }

    /// The finished application: every route so far, a 404 page for any other path, and the
    /// security headers on every response, errors included.
    pub fn build(self) -> Router {
        self.router
            .fallback(pages::not_found)
            .layer(map_response(headers::add_security_headers))
    }
}

impl Default for RouterBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// The HTTP listener could not be opened.
#[derive(Debug, thiserror::Error)]
#[error("cannot listen for HTTP on {addr}")]
pub struct BindError {
    /// The address from `core.http_listen`.
    pub addr: SocketAddr,
    /// Why binding failed (address in use, permission denied, ...).
    #[source]
    pub source: io::Error,
}

/// Opens the HTTP listener on `addr` (`core.http_listen`). The daemon binds before it reports
/// itself started, so a busy port is a startup error.
pub async fn bind(addr: SocketAddr) -> Result<TcpListener, BindError> {
    TcpListener::bind(addr)
        .await
        .map_err(|source| BindError { addr, source })
}

/// Serves `app` on `listener` until `shutdown` is cancelled. Then it stops accepting connections,
/// lets in-flight requests finish for up to [`DRAIN_TIMEOUT`], and returns.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: CancellationToken,
) -> io::Result<()> {
    serve_with_drain(listener, app, shutdown, DRAIN_TIMEOUT).await
}

async fn serve_with_drain(
    listener: TcpListener,
    app: Router,
    shutdown: CancellationToken,
    drain: Duration,
) -> io::Result<()> {
    let server =
        axum::serve(listener, app).with_graceful_shutdown(shutdown.clone().cancelled_owned());
    let deadline = async {
        shutdown.cancelled().await;
        tokio::time::sleep(drain).await;
    };
    tokio::select! {
        result = server.into_future() => result,
        () = deadline => {
            warn!(
                timeout = ?drain,
                "HTTP requests still open at the shutdown deadline; dropping them",
            );
            Ok(())
        }
    }
}
