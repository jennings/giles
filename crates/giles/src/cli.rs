//! Command-line arguments.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Giles: a personal agent daemon.
#[derive(Debug, Parser)]
#[command(name = "giles", version)]
pub struct Cli {
    /// The data root. Overrides the GILES_HOME environment variable; the default is ~/.giles.
    #[arg(long, global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The `giles` subcommands. Each variant carries its own argument struct.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon in the foreground until SIGINT or SIGTERM.
    Serve(ServeArgs),
}

/// Arguments to `giles serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn home_is_accepted_before_or_after_the_subcommand() {
        for args in [
            ["giles", "--home", "/h", "serve"],
            ["giles", "serve", "--home", "/h"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(cli.home.as_deref(), Some(Path::new("/h")));
            assert!(matches!(cli.command, Command::Serve(_)));
        }
    }

    #[test]
    fn a_subcommand_is_required() {
        assert!(Cli::try_parse_from(["giles"]).is_err());
        assert!(Cli::try_parse_from(["giles", "--home", "/h"]).is_err());
    }
}
