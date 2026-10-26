//! `giles-channel-discord`: the Giles Discord channel plugin. Run by Giles, not by hand. See
//! `Docs/technical/discord.md`.

use std::io;
use std::process::ExitCode;

use giles_channel_discord::channel::{self, SessionSlot};
use giles_plugin_sdk::ChannelPlugin;
use giles_plugin_sdk::giles_types::OutputMode;
use tracing_subscriber::EnvFilter;

/// The log filter when `GILES_LOG` isn't set. The libraries' own logs are for their authors: only
/// problems show unless asked for.
const DEFAULT_LOG: &str = "warn,giles_channel_discord=info,giles_plugin_sdk=info";

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
    // `serve` returns, the Gateway is closed (the bot goes offline at once) before exiting.
    let slot = SessionSlot::default();
    let configure_slot = slot.clone();
    let code = ChannelPlugin::new("discord", env!("CARGO_PKG_VERSION"))
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
