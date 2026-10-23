//! Command-line arguments.

use std::ffi::OsString;
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
    /// Run a command in the plugin sandbox (internal: the daemon launches plugins through it).
    SandboxExec(SandboxExecArgs),
}

/// Arguments to `giles serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {}

/// Arguments to `giles sandbox-exec`.
#[derive(Debug, Args)]
pub struct SandboxExecArgs {
    /// The sandbox spec: a JSON file written by the daemon.
    #[arg(value_name = "SPEC")]
    pub spec: PathBuf,

    /// The command to run (an absolute path) and its arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

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

    #[test]
    fn sandbox_exec_takes_everything_after_the_separator() {
        let cli = Cli::try_parse_from([
            "giles",
            "sandbox-exec",
            "/run/spec.json",
            "--",
            "/bin/plugin",
            "--home",
            "-x",
        ])
        .unwrap();
        let Command::SandboxExec(args) = cli.command else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(args.spec, Path::new("/run/spec.json"));
        assert_eq!(args.command, ["/bin/plugin", "--home", "-x"]);
        assert_eq!(cli.home, None);
    }

    #[test]
    fn sandbox_exec_needs_a_spec_and_a_command_after_the_separator() {
        for args in [
            &["giles", "sandbox-exec"][..],
            &["giles", "sandbox-exec", "/spec"],
            &["giles", "sandbox-exec", "/spec", "--"],
            &["giles", "sandbox-exec", "/spec", "/bin/plugin"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }
}
