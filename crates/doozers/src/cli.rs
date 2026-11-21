//! Command-line arguments.

use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Doozers: a personal agent daemon.
#[derive(Debug, Parser)]
#[command(name = "doozers", version)]
pub struct Cli {
    /// The data root. Overrides the DOOZERS_HOME environment variable; the default is ~/.doozers.
    #[arg(long, global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,

    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The `doozers` subcommands. Each variant carries its own argument struct.
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
    /// Manage MCP toolsets: of the running daemon, and, for `rename`, with it stopped.
    Toolset(ToolsetArgs),
    /// Write a consistent snapshot of the data root (database and files) to a directory.
    Backup(BackupArgs),
    /// Manage channels with the daemon stopped.
    Channel(ChannelArgs),
    /// Manage environments with the daemon stopped.
    Environment(EnvironmentArgs),
    /// Manage providers with the daemon stopped.
    Provider(ProviderArgs),
    /// Run a command in the plugin sandbox (internal: the daemon launches plugins through it).
    SandboxExec(SandboxExecArgs),
}

/// Arguments to `doozers serve`.
#[derive(Debug, Args)]
pub struct ServeArgs {}

/// Arguments to `doozers status`.
#[derive(Debug, Args)]
pub struct StatusArgs {}

/// Arguments to `doozers usage`.
#[derive(Debug, Args)]
pub struct UsageArgs {
    /// Only requests since WHEN: a duration ago ("7d", "12h"), a date ("2026-10-01", from local
    /// midnight), or an RFC 3339 time ("2026-10-01T09:00:00Z"). Default: all.
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,
}

/// Arguments to `doozers backup`.
#[derive(Debug, Args)]
pub struct BackupArgs {
    /// The directory to write the backup into. It must exist.
    #[arg(value_name = "DIR")]
    pub dir: PathBuf,
}

/// Arguments to `doozers admin`.
#[derive(Debug, Args)]
pub struct AdminArgs {
    /// What to do.
    #[command(subcommand)]
    pub command: AdminCommand,
}

/// The `doozers admin` subcommands.
#[derive(Debug, Subcommand)]
pub enum AdminCommand {
    /// Print a one-time recovery URL that signs a user in to the web chat. With no users yet, it
    /// creates the first admin.
    LoginLink(LoginLinkArgs),
}

/// Arguments to `doozers admin login-link`.
#[derive(Debug, Args)]
pub struct LoginLinkArgs {
    /// The id of the user to log in, e.g. "owner". Leave it out on a fresh install, which has no
    /// users, to create the first admin.
    #[arg(long, value_name = "USER")]
    pub user: Option<String>,
}

/// Arguments to `doozers toolset`.
#[derive(Debug, Args)]
pub struct ToolsetArgs {
    /// What to do.
    #[command(subcommand)]
    pub command: ToolsetCommand,
}

/// The `doozers toolset` subcommands.
#[derive(Debug, Subcommand)]
pub enum ToolsetCommand {
    /// List every toolset, stored and from doozers.toml.
    List,
    /// Print a toolset's spec as TOML, for `set`.
    Show(ToolsetNameArgs),
    /// Create a remote toolset, or replace the spec of the one with that name, with no restart.
    Set(SetToolsetArgs),
    /// Remove a stored toolset.
    Remove(ToolsetNameArgs),
    /// Rename a toolset, with its tool prefix and the agents' globs. A stored toolset can be
    /// renamed while Doozers runs; a `doozers.toml` toolset needs Doozers stopped.
    Rename(RenameArgs),
    /// Restart a toolset's server now.
    Restart(ToolsetNameArgs),
}

/// A toolset's name, for the `doozers toolset` commands that take only that.
#[derive(Debug, Args)]
pub struct ToolsetNameArgs {
    /// The toolset name.
    #[arg(value_name = "NAME")]
    pub name: String,
}

/// Arguments to `doozers channel rename`, `doozers environment rename`, `doozers provider rename`,
/// and `doozers toolset rename`.
#[derive(Debug, Args)]
pub struct RenameArgs {
    /// The name now.
    #[arg(value_name = "OLD")]
    pub old: String,

    /// The new name: letters, digits, and `-`.
    #[arg(value_name = "NEW")]
    pub new: String,
}

/// Arguments to `doozers toolset set`.
#[derive(Debug, Args)]
pub struct SetToolsetArgs {
    /// The toolset name: letters, digits, and `-`.
    #[arg(value_name = "NAME")]
    pub name: String,

    /// A TOML file with the keys of a remote server's `[toolset.<name>]` table (`url`, `auth`,
    /// ...), or `-` for standard input. The output of `doozers toolset show` is one.
    #[arg(long, value_name = "FILE")]
    pub file: PathBuf,
}

/// Arguments to `doozers channel`.
#[derive(Debug, Args)]
pub struct ChannelArgs {
    /// What to do.
    #[command(subcommand)]
    pub command: ChannelCommand,
}

/// The `doozers channel` subcommands.
#[derive(Debug, Subcommand)]
pub enum ChannelCommand {
    /// Rename a channel: its database rows, directories, and config. Stop the daemon first.
    Rename(RenameArgs),
}

/// Arguments to `doozers environment`.
#[derive(Debug, Args)]
pub struct EnvironmentArgs {
    /// What to do.
    #[command(subcommand)]
    pub command: EnvironmentCommand,
}

/// The `doozers environment` subcommands.
#[derive(Debug, Subcommand)]
pub enum EnvironmentCommand {
    /// Rename an environment: its database rows, directories, config, and the agents that list
    /// it. Stop the daemon first.
    Rename(RenameArgs),
}

/// Arguments to `doozers provider`.
#[derive(Debug, Args)]
pub struct ProviderArgs {
    /// What to do.
    #[command(subcommand)]
    pub command: ProviderCommand,
}

/// The `doozers provider` subcommands.
#[derive(Debug, Subcommand)]
pub enum ProviderCommand {
    /// Rename a provider: its database rows, directories, and config. Stop the daemon first.
    Rename(RenameArgs),
}

/// Arguments to `doozers sandbox-exec`.
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
            ["doozers", "--home", "/h", "serve"],
            ["doozers", "serve", "--home", "/h"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(cli.home.as_deref(), Some(Path::new("/h")));
            assert!(matches!(cli.command, Command::Serve(_)));
        }
    }

    #[test]
    fn a_subcommand_is_required() {
        assert!(Cli::try_parse_from(["doozers"]).is_err());
        assert!(Cli::try_parse_from(["doozers", "--home", "/h"]).is_err());
    }

    #[test]
    fn sandbox_exec_takes_everything_after_the_separator() {
        let cli = Cli::try_parse_from([
            "doozers",
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
            &["doozers", "sandbox-exec"][..],
            &["doozers", "sandbox-exec", "/spec"],
            &["doozers", "sandbox-exec", "/spec", "--"],
            &["doozers", "sandbox-exec", "/spec", "/bin/plugin"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn admin_commands_parse() {
        let cli =
            Cli::try_parse_from(["doozers", "usage", "--since", "7d", "--home", "/h"]).unwrap();
        let Command::Usage(args) = cli.command else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(args.since.as_deref(), Some("7d"));
        assert_eq!(cli.home.as_deref(), Some(Path::new("/h")));

        let cli =
            Cli::try_parse_from(["doozers", "admin", "login-link", "--user", "owner"]).unwrap();
        let Command::Admin(AdminArgs {
            command: AdminCommand::LoginLink(args),
        }) = cli.command
        else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(args.user.as_deref(), Some("owner"));

        assert!(matches!(
            Cli::try_parse_from(["doozers", "status"]).unwrap().command,
            Command::Status(_)
        ));
        // Without a user, a fresh install makes its first admin.
        let cli = Cli::try_parse_from(["doozers", "admin", "login-link"]).unwrap();
        let Command::Admin(AdminArgs {
            command: AdminCommand::LoginLink(args),
        }) = cli.command
        else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(args.user, None);
    }

    #[test]
    fn toolset_commands_parse() {
        let toolset = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Command::Toolset(ToolsetArgs { command }) => command,
            other => panic!("{other:?}"),
        };
        assert!(matches!(
            toolset(&["doozers", "toolset", "list"]),
            ToolsetCommand::List
        ));
        assert!(matches!(
            toolset(&["doozers", "toolset", "show", "files"]),
            ToolsetCommand::Show(args) if args.name == "files"
        ));
        assert!(matches!(
            toolset(&["doozers", "toolset", "remove", "files"]),
            ToolsetCommand::Remove(args) if args.name == "files"
        ));
        assert!(matches!(
            toolset(&["doozers", "toolset", "restart", "files"]),
            ToolsetCommand::Restart(args) if args.name == "files"
        ));
        assert!(matches!(
            toolset(&["doozers", "toolset", "rename", "files", "docs"]),
            ToolsetCommand::Rename(args) if args.old == "files" && args.new == "docs"
        ));
        let ToolsetCommand::Set(args) =
            toolset(&["doozers", "toolset", "set", "files", "--file", "-"])
        else {
            panic!("not set");
        };
        assert_eq!(args.name, "files");
        assert_eq!(args.file, Path::new("-"));
        let cli = Cli::try_parse_from(["doozers", "toolset", "list", "--home", "/h"]).unwrap();
        assert_eq!(cli.home.as_deref(), Some(Path::new("/h")));

        for args in [
            &["doozers", "toolset"][..],
            &["doozers", "toolset", "show"],
            &["doozers", "toolset", "show", "a", "b"],
            &["doozers", "toolset", "list", "a"],
            &["doozers", "toolset", "set", "files"],
            &["doozers", "toolset", "set", "--file", "x.toml"],
            &["doozers", "toolset", "remove"],
            &["doozers", "toolset", "restart"],
            &["doozers", "toolset", "rename"],
            &["doozers", "toolset", "rename", "files"],
            &["doozers", "toolset", "rename", "files", "docs", "more"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn backup_takes_one_directory() {
        let cli =
            Cli::try_parse_from(["doozers", "backup", "/mnt/backups", "--home", "/h"]).unwrap();
        let Command::Backup(args) = cli.command else {
            panic!("{:?}", cli.command);
        };
        assert_eq!(args.dir, Path::new("/mnt/backups"));
        assert_eq!(cli.home.as_deref(), Some(Path::new("/h")));
        assert!(Cli::try_parse_from(["doozers", "backup"]).is_err());
        assert!(Cli::try_parse_from(["doozers", "backup", "a", "b"]).is_err());
    }

    #[test]
    fn channel_environment_and_provider_rename_take_two_names() {
        let cli = Cli::try_parse_from(["doozers", "channel", "rename", "libera", "oftc"]).unwrap();
        let Command::Channel(ChannelArgs {
            command: ChannelCommand::Rename(args),
        }) = cli.command
        else {
            panic!("{:?}", cli.command);
        };
        assert_eq!((args.old.as_str(), args.new.as_str()), ("libera", "oftc"));
        let cli =
            Cli::try_parse_from(["doozers", "provider", "rename", "ollama", "local"]).unwrap();
        let Command::Provider(ProviderArgs {
            command: ProviderCommand::Rename(args),
        }) = cli.command
        else {
            panic!("{:?}", cli.command);
        };
        assert_eq!((args.old.as_str(), args.new.as_str()), ("ollama", "local"));
        let cli = Cli::try_parse_from(["doozers", "environment", "rename", "box", "dev"]).unwrap();
        let Command::Environment(EnvironmentArgs {
            command: EnvironmentCommand::Rename(args),
        }) = cli.command
        else {
            panic!("{:?}", cli.command);
        };
        assert_eq!((args.old.as_str(), args.new.as_str()), ("box", "dev"));
        for kind in ["channel", "environment", "provider"] {
            for args in [
                &["doozers", kind][..],
                &["doozers", kind, "rename"],
                &["doozers", kind, "rename", "a"],
                &["doozers", kind, "rename", "a", "b", "c"],
            ] {
                assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
            }
        }
    }

    #[test]
    fn there_is_no_instance_command() {
        let args = ["doozers", "instance", "rename", "libera", "oftc"];
        assert!(Cli::try_parse_from(args).is_err());
    }
}
