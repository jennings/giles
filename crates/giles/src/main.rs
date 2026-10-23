//! The `giles` binary: the daemon and its command-line interface.

mod cli;
mod logging;
mod sandbox_exec;
mod serve;
mod shutdown;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // `sandbox-exec` reports a logging failure with its own exit status contract.
    let logging = logging::init();
    match cli.command {
        Command::Serve(args) => {
            logging?;
            serve::run(cli.home.as_deref(), &args)
        }
        Command::SandboxExec(args) => sandbox_exec::run(&args, logging),
    }
}
