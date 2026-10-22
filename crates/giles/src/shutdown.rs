//! Graceful shutdown: SIGINT and SIGTERM cancel a [`CancellationToken`] that every subsystem
//! watches. A second signal exits immediately.

use std::io;

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Exit status when a second signal cuts a graceful shutdown short (128 + SIGINT, as shells
/// report an interrupted command).
const FORCED_EXIT_STATUS: i32 = 130;

/// Installs SIGINT and SIGTERM handlers. The first signal cancels `token`; a signal received after
/// `token` is cancelled (by a signal or otherwise) exits the process at once with status 130, so a
/// hung shutdown can still be interrupted without SIGKILL.
///
/// The handlers are in place when this returns, so a signal sent after that is never lost and never
/// kills the process outright. Must be called inside a Tokio runtime.
pub fn cancel_on_signal(token: CancellationToken) -> io::Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::spawn(async move {
        tokio::select! {
            name = next_signal(&mut interrupt, &mut terminate) => {
                info!(signal = name, "shutting down");
                token.cancel();
            }
            () = token.cancelled() => {}
        }
        let name = next_signal(&mut interrupt, &mut terminate).await;
        warn!(
            signal = name,
            "second signal during shutdown; exiting immediately"
        );
        std::process::exit(FORCED_EXIT_STATUS);
    });
    Ok(())
}

/// Waits for the next SIGINT or SIGTERM and returns its name.
async fn next_signal(interrupt: &mut Signal, terminate: &mut Signal) -> &'static str {
    tokio::select! {
        _ = interrupt.recv() => "SIGINT",
        _ = terminate.recv() => "SIGTERM",
    }
}
