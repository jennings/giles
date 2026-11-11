//! A plugin binary's `main`, once its kind is known: the command-line dispatch that
//! `ChannelPlugin::run` and `EnvironmentPlugin::run` share.

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;

use giles_proto::handshake::MAGIC_COOKIE_ENV;
use giles_types::Kind;

use crate::cli;
use crate::remote::RemoteServer;

/// Runs the plugin `name` of kind `kind` as `args` (the arguments after the program name) say, and
/// returns the exit status. `env` reads the environment.
///
/// `plugin` is whatever the kind needs to serve (the plugin's description and its `configure`).
/// It goes to `socket` for the `socket` subcommand, which serves the launched plugin, or to
/// `remote` for `serve`, with the listener and the `[config]` table of the `--config` file (`Some`
/// for an environment plugin, `None` for the other kinds).
pub(crate) async fn run<P, SocketFut, RemoteFut>(
    kind: Kind,
    name: &str,
    args: &[OsString],
    env: &dyn Fn(&str) -> Option<OsString>,
    plugin: P,
    socket: impl FnOnce(P) -> SocketFut,
    remote: impl FnOnce(P, RemoteServer, Option<serde_json::Value>) -> RemoteFut,
) -> ExitCode
where
    SocketFut: Future<Output = ExitCode>,
    RemoteFut: Future<Output = io::Result<()>>,
{
    let binary = cli::binary_name(kind, name);
    match cli::parse(args) {
        Ok(cli::Command::Socket) => socket(plugin).await,
        Ok(cli::Command::Serve(serve)) => {
            let result = match serve.resolve(kind, name, env) {
                Ok(settings) => serve_settings(settings, plugin, remote).await,
                Err(e) => Err(e.into()),
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{binary}: {e}");
                    ExitCode::from(e.exit_code())
                }
            }
        }
        Ok(cli::Command::Help) => {
            print!("{}", cli::usage(kind, name));
            ExitCode::SUCCESS
        }
        // A core from before the subcommands starts a plugin with no arguments.
        Err(cli::CliError::NoCommand) if env(MAGIC_COOKIE_ENV).is_some() => {
            eprintln!(
                "{binary}: Giles launched it without the `socket` argument, so the Giles \
                 that started it is older than this plugin; upgrade Giles"
            );
            ExitCode::from(1)
        }
        Err(cli::CliError::NoCommand) => {
            eprint!("{}", cli::usage(kind, name));
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("{binary}: {e}\nRun `{binary} --help` for usage.");
            ExitCode::from(2)
        }
    }
}

/// The `serve` subcommand once its settings are known.
async fn serve_settings<P, RemoteFut>(
    settings: cli::Settings,
    plugin: P,
    remote: impl FnOnce(P, RemoteServer, Option<serde_json::Value>) -> RemoteFut,
) -> Result<(), cli::RunError>
where
    RemoteFut: Future<Output = io::Result<()>>,
{
    let listener = tokio::net::TcpListener::bind(&settings.listen)
        .await
        .map_err(|source| cli::RunError::Listen {
            address: settings.listen.clone(),
            source,
        })?;
    let server = RemoteServer::new(listener, settings.data, settings.auth);
    #[cfg(feature = "tls")]
    let server = match settings.tls {
        Some((cert, key)) => server.tls(cli::tls_config(&cert, &key)?),
        None => server,
    };
    {
        use std::io::Write as _;
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "listening on {}", server.local_addr()?)?;
        stdout.flush()?;
    }
    Ok(remote(plugin, server, settings.config).await?)
}
