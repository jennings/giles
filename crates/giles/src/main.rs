//! The `giles` binary: the daemon and its command-line interface.

mod admin_client;
mod backup;
mod cli;
mod logging;
mod login_link;
mod rename;
mod sandbox_exec;
mod serve;
mod shutdown;
mod status;
mod toolset;
mod usage;

use clap::Parser;

use giles_core::config::Kind;

use crate::cli::{AdminCommand, ChannelCommand, Cli, Command, ProviderCommand};

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
        Command::Backup(args) => {
            logging?;
            backup::run(home, &args)
        }
        Command::Admin(args) => {
            logging?;
            match args.command {
                AdminCommand::LoginLink(args) => login_link::run(home, &args),
            }
        }
        Command::Toolset(args) => {
            logging?;
            toolset::run(home, &args)
        }
        Command::Channel(args) => {
            logging?;
            match args.command {
                ChannelCommand::Rename(args) => rename::run(home, Kind::Channel, &args),
            }
        }
        Command::Provider(args) => {
            logging?;
            match args.command {
                ProviderCommand::Rename(args) => rename::run(home, Kind::Provider, &args),
            }
        }
        Command::SandboxExec(args) => sandbox_exec::run(&args, logging),
    }
}
