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
    /// Show the running daemon's state: its sandbox, channels, and plugins.
    Status(StatusArgs),
    /// Show token usage per agent and model, with the prompt cache hit rate.
    Usage(UsageArgs),
    /// Administer the running daemon.
    Admin(AdminArgs),
    /// Manage the MCP toolsets of the running daemon.
    Toolset(ToolsetArgs),
    /// Write a consistent snapshot of the data root (database and files) to a directory.
    Backup(BackupArgs),
    /// Manage instances with the daemon stopped.
    Instance(InstanceArgs),
    /// Run a command in the plugin sandbox (internal: the daemon launches plugins through it).
    SandboxExec(SandboxExecArgs),
}

/// Arguments to `giles serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {}

/// Arguments to `giles status`.
#[derive(Debug, Args)]
pub struct StatusArgs {}

/// Arguments to `giles usage`.
#[derive(Debug, Args)]
pub struct UsageArgs {
    /// Only requests since WHEN: a duration ago ("7d", "12h"), a date ("2026-10-01", from local
    /// midnight), or an RFC 3339 time ("2026-10-01T09:00:00Z"). Default: all.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,
}

/// Arguments to `giles backup`.
#[derive(Debug, Args)]
pub struct BackupArgs {
    /// The directory to write the backup into. It must exist.
    #[arg(value_name = "DIR")]
    pub dir: PathBuf,
}

/// Arguments to `giles admin`.
#[derive(Debug, Args)]
pub struct AdminArgs {
    /// What to do.
    #[command(subcommand)]
    pub command: AdminCommand,
}

/// The `giles admin` subcommands.
#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Print a one-time login URL for the web chat.
    LoginLink(LoginLinkArgs),
}

/// Arguments to `giles admin login-link`.
#[derive(Debug, Args)]
pub struct LoginLinkArgs {
    /// The user to log in, e.g. "owner".
    #[arg(long, value_name = "USER")]
    pub user: String,
}

/// Arguments to `giles toolset`.
#[derive(Debug, Args)]
pub struct ToolsetArgs {
    /// What to do.
    #[command(subcommand)]
    pub command: ToolsetCommand,
}

/// The `giles toolset` subcommands.
#[derive(Debug, Subcommand)]
pub enum ToolsetCommand {
    /// List every toolset, stored and from giles.toml.
    List,
    /// Print a toolset's spec as TOML, for `set`.
    Show(ToolsetNameArgs),
    /// Create a remote toolset, or replace the spec of the one with that name, with no restart.
    Set(SetToolsetArgs),
    /// Remove a stored toolset.
    Remove(ToolsetNameArgs),
    /// Rename a stored toolset, with its tool prefix and the agents' globs.
    Rename(RenameToolsetArgs),
    /// Restart a toolset's server now.
    Restart(ToolsetNameArgs),
}

/// A toolset's name, for the `giles toolset` commands that take only that.
#[derive(Debug, Args)]
pub struct ToolsetNameArgs {
    /// The toolset name.
    #[arg(value_name = "NAME")]
    pub name: String,
}

/// Arguments to `giles toolset rename`.
#[derive(Debug, Args)]
pub struct RenameToolsetArgs {
    /// The toolset's name now.
    #[arg(value_name = "OLD")]
    pub old: String,

    /// The new name: letters, digits, and `-`.
    #[arg(value_name = "NEW")]
    pub new: String,
}

/// Arguments to `giles toolset set`.
#[derive(Debug, Args)]
pub struct SetToolsetArgs {
    /// The toolset name: letters, digits, and `-`.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// A TOML file with the keys of a remote server's `[toolset.<name>]` table (`url`, `auth`,
    /// ...), or `-` for standard input. The output of `giles toolset show` is one.
    #[arg(long, value_name = "FILE")]
    pub file: PathBuf,
}

/// Arguments to `giles instance`.
#[derive(Debug, Args)]
pub struct InstanceArgs {
    /// What to do.
    #[command(subcommand)]
    pub command: InstanceCommand,
}

/// The `giles instance` subcommands.
#[derive(Debug, Subcommand)]
pub enum InstanceCommand {
    /// Rename an instance: its database rows, directories, and config. Stop the daemon first.
    Rename(RenameArgs),
}

/// Arguments to `giles instance rename`.
#[derive(Debug, Args)]
pub struct RenameArgs {
    /// The current name of the instance.
    #[arg(value_name = "OLD")]
    pub old: String,

    /// The new name: letters, digits, and `-`.
    #[arg(value_name = "NEW")]
    pub new: String,
}

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

    #[test]
    fn admin_commands_parse() {
        let cli = Cli::try_parse_from(["giles", "usage", "--since", "7d", "--home", "/h"]).unwrap();
        let Command::Usage(args) = cli.command else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(args.since.as_deref(), Some("7d"));
        assert_eq!(cli.home.as_deref(), Some(Path::new("/h")));

        let cli = Cli::try_parse_from(["giles", "admin", "login-link", "--user", "owner"]).unwrap();
        let Command::Admin(AdminArgs {
            command: AdminCommand::LoginLink(args),
        }) = cli.command
        else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(args.user, "owner");

        assert!(matches!(
            Cli::try_parse_from(["giles", "status"]).unwrap().command,
            Command::Status(_)
        ));
        // The user is required.
        assert!(Cli::try_parse_from(["giles", "admin", "login-link"]).is_err());
    }

    #[test]
    fn toolset_commands_parse() {
        let toolset = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Toolset(ToolsetArgs { command }) => command,
            other => panic!("{other:?}"),
        };
        assert!(matches!(
            toolset(&["giles", "toolset", "list"]),
            ToolsetCommand::List
        ));
        assert!(matches!(
            toolset(&["giles", "toolset", "show", "files"]),
            ToolsetCommand::Show(args) if args.name == "files"
        ));
        assert!(matches!(
            toolset(&["giles", "toolset", "remove", "files"]),
            ToolsetCommand::Remove(args) if args.name == "files"
        ));
        assert!(matches!(
            toolset(&["giles", "toolset", "restart", "files"]),
            ToolsetCommand::Restart(args) if args.name == "files"
        ));
        assert!(matches!(
            toolset(&["giles", "toolset", "rename", "files", "docs"]),
            ToolsetCommand::Rename(args) if args.old == "files" && args.new == "docs"
        ));
        let ToolsetCommand::Set(args) =
            toolset(&["giles", "toolset", "set", "files", "--file", "-"])
        else {
            panic!("not set");
        };
        assert_eq!(args.name, "files");
        assert_eq!(args.file, Path::new("-"));
        let cli = Cli::try_parse_from(["giles", "toolset", "list", "--home", "/h"]).unwrap();
        assert_eq!(cli.home.as_deref(), Some(Path::new("/h")));

        for args in [
            &["giles", "toolset"][..],
            &["giles", "toolset", "show"],
            &["giles", "toolset", "show", "a", "b"],
            &["giles", "toolset", "list", "a"],
            &["giles", "toolset", "set", "files"],
            &["giles", "toolset", "set", "--file", "x.toml"],
            &["giles", "toolset", "remove"],
            &["giles", "toolset", "restart"],
            &["giles", "toolset", "rename"],
            &["giles", "toolset", "rename", "files"],
            &["giles", "toolset", "rename", "files", "docs", "more"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn backup_takes_one_directory() {
        let cli = Cli::try_parse_from(["giles", "backup", "/mnt/backups", "--home", "/h"]).unwrap();
        let Command::Backup(args) = cli.command else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(args.dir, Path::new("/mnt/backups"));
        assert_eq!(cli.home.as_deref(), Some(Path::new("/h")));
        assert!(Cli::try_parse_from(["giles", "backup"]).is_err());
        assert!(Cli::try_parse_from(["giles", "backup", "a", "b"]).is_err());
    }

    #[test]
    fn instance_rename_takes_two_names() {
        let cli = Cli::try_parse_from(["giles", "instance", "rename", "libera", "oftc"]).unwrap();
        let Command::Instance(InstanceArgs {
            command: InstanceCommand::Rename(args),
        }) = cli.command
        else {
            panic!("{:?}", cli.command);
        };
        assert_eq!((args.old.as_str(), args.new.as_str()), ("libera", "oftc"));
        for args in [
            &["giles", "instance"][..],
            &["giles", "instance", "rename"],
            &["giles", "instance", "rename", "a"],
            &["giles", "instance", "rename", "a", "b", "c"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }
}
