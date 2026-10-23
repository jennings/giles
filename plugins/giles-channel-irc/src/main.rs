//! `giles-channel-irc`: the Giles IRC channel plugin. Run by Giles, not by hand.

use std::io;
use std::process::ExitCode;

use giles_plugin_sdk::ChannelPlugin;
use giles_plugin_sdk::giles_types::OutputMode;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_env("GILES_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    ChannelPlugin::new("irc", env!("CARGO_PKG_VERSION"))
        .output(OutputMode::Blocks)
        .can_initiate(true)
        .serve(giles_channel_irc::configure)
        .await
}
