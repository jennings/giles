//! `giles-channel-whatsapp`: the Giles WhatsApp channel plugin. Links to WhatsApp as a companion
//! device with `whatsapp-rust`. Run by Giles, not by hand. See `Docs/user/whatsapp.md` and
//! `Docs/technical/whatsapp.md`.

mod backend;
mod channel;
mod config;
mod datadir;
#[cfg(test)]
mod docs_tests;
mod identity;
mod inbound;
mod media;
mod outbound;
mod real;
#[cfg(test)]
mod sdk_tests;
mod session;
mod split;
mod status;
#[cfg(test)]
mod testing;

use std::io;
use std::process::ExitCode;

use giles_plugin_sdk::ChannelPlugin;
use giles_plugin_sdk::giles_types::OutputMode;
use tracing_subscriber::EnvFilter;

/// The log filter when `GILES_LOG` isn't set. The WhatsApp library logs every connection step at
/// `info`, and some of it (a pair code, JIDs) shouldn't sit in a log, so its default is `warn`.
const DEFAULT_LOG: &str = "warn,giles_channel_whatsapp=info,giles_plugin_sdk=info";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_env("GILES_LOG").unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG)),
        )
        .init();
    // The SDK owns the channel, so the session is reached through a slot `configure` fills: after
    // `serve` returns, the client is disconnected (flushing the session store) before exiting.
    let slot = channel::SessionSlot::default();
    let configure_slot = slot.clone();
    let code = ChannelPlugin::new("whatsapp", env!("CARGO_PKG_VERSION"))
        .output(OutputMode::Blocks)
        .can_initiate(true)
        .serve(move |ctx| {
            let slot = configure_slot.clone();
            async move { channel::configure(ctx, &slot).await }
        })
        .await;
    slot.shutdown(channel::shutdown_wait()).await;
    code
}
