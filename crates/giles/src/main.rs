//! The `giles` binary: the daemon and its command-line interface.

mod cli;
mod logging;
mod serve;
mod shutdown;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init()?;
    match cli.command {
        Command::Serve(args) => serve::run(cli.home.as_deref(), &args),
    }
}
