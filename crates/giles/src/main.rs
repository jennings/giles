//! The `giles` binary: the daemon and its command-line interface.

mod admin_client;
mod cli;
mod logging;
mod login_link;
mod sandbox_exec;
mod serve;
mod shutdown;
mod status;
mod usage;

use clap::Parser;

use crate::cli::{AdminCommand, Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // `sandbox-exec` reports a logging failure with its own exit status contract.
    let logging = logging::init();
    let home = cli.home.as_deref();
    match cli.command {
        Command::Serve(args) => {
            logging?;
            serve::run(home, &args)
        }
        Command::Status(args) => {
            logging?;
            status::run(home, &args)
        }
        Command::Usage(args) => {
            logging?;
            usage::run(home, &args)
        }
        Command::Admin(args) => {
            logging?;
            match args.command {
                AdminCommand::LoginLink(args) => login_link::run(home, &args),
            }
        }
        Command::SandboxExec(args) => sandbox_exec::run(&args, logging),
    }
}
