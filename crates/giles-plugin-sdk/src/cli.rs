//! The command line of a plugin binary (`ChannelPlugin::run`): the `socket` subcommand Giles
//! launches plugins with, the `serve` subcommand an operator runs a remote plugin with (PLAN
//! sections 7.4 and 7.6), and the transport settings `serve` reads from flags and a file.
//!
//! The settings here are the plugin's transport only. What the plugin does (the Discord bot
//! token, the IRC server) comes from the instance's `config` in the core's `giles.toml`, in both
//! modes.

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::remote::RemoteAuth;

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// `socket`: serve gRPC on the socket Giles inherits to the plugin.
    Socket,
    /// `serve`: run as a remote plugin.
    Serve(ServeArgs),
    /// `--help`, `-h`, or `help`.
    Help,
}

/// The `serve` flags as given. Each is also a key of the `--config` file, where a flag wins.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ServeArgs {
    config: Option<PathBuf>,
    listen: Option<String>,
    data: Option<PathBuf>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    token_file: Option<PathBuf>,
    no_auth: bool,
}

/// Why the command line, or the settings it names, are unusable.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// No subcommand.
    #[error("no command given")]
    NoCommand,
    /// A word where a subcommand belongs that isn't one.
    #[error("unknown command {0:?}")]
    UnknownCommand(String),
    /// A flag `serve` doesn't have.
    #[error("unknown option {0:?}")]
    UnknownFlag(String),
    /// A flag without its value.
    #[error("{0} needs a value")]
    MissingValue(&'static str),
    /// A flag given twice.
    #[error("{0} given more than once")]
    Repeated(&'static str),
    /// A value that must be text but isn't.
    #[error("{0} must be valid UTF-8")]
    NotUtf8(&'static str),
    /// An argument the subcommand doesn't take.
    #[error("`{command}` takes no argument {argument:?}")]
    Unexpected {
        /// The subcommand.
        command: &'static str,
        /// What followed it.
        argument: String,
    },
    /// A required setting is in neither the flags nor the file.
    #[error("no {key}: set `{key}` in the --config file or pass {flag}")]
    Missing {
        /// The key in the file.
        key: &'static str,
        /// The flag.
        flag: &'static str,
    },
    /// One of `tls_cert` and `tls_key` without the other.
    #[error("`tls_cert` and `tls_key` go together: set both or neither")]
    TlsPair,
    /// TLS asked for in a build without it.
    #[cfg(not(feature = "tls"))]
    #[error("this build has no TLS support (the `tls` feature of giles-plugin-sdk)")]
    NoTls,
    /// No way to authenticate the core, and `--no-auth` isn't there to say that's intended.
    #[error(
        "no bearer token: set {env}, or `token_file` (--token-file), or pass --no-auth to accept \
         any caller"
    )]
    NoToken {
        /// The environment variable.
        env: String,
    },
    /// Two token sources.
    #[error("the token is set both in {env} and by `token_file`: use one")]
    TwoTokens {
        /// The environment variable.
        env: String,
    },
    /// `--no-auth` next to a token.
    #[error("--no-auth contradicts the bearer token configured: use one")]
    NoAuthWithToken,
    /// A token that can't travel in an `authorization` header.
    #[error("the bearer token from {origin} must be one line of visible ASCII, and not empty")]
    BadToken {
        /// Where it came from.
        origin: String,
    },
    /// The token file can be read by others.
    #[error("{path}: group or others can access it (mode {mode:04o}); run `chmod 600 {path}`")]
    TokenFileMode {
        /// The file.
        path: String,
        /// Its permission bits.
        mode: u32,
    },
    /// A file that couldn't be read.
    #[error("reading {path}: {source}")]
    Read {
        /// The file.
        path: String,
        /// Why not.
        source: io::Error,
    },
    /// The `--config` file isn't valid.
    #[error("{path}: {message}")]
    Config {
        /// The file.
        path: String,
        /// What's wrong with it.
        message: String,
    },
}

/// Parses the arguments after the program name.
pub(crate) fn parse(args: &[OsString]) -> Result<Command, CliError> {
    let mut args = args.iter();
    let Some(first) = args.next() else {
        return Err(CliError::NoCommand);
    };
    match first.to_str() {
        Some("socket") => match args.next() {
            None => Ok(Command::Socket),
            Some(extra) => Err(CliError::Unexpected {
                command: "socket",
                argument: extra.to_string_lossy().into_owned(),
            }),
        },
        Some("serve") => parse_serve(args).map(|serve| serve.map_or(Command::Help, Command::Serve)),
        Some("-h" | "--help" | "help") => Ok(Command::Help),
        _ => Err(CliError::UnknownCommand(
            first.to_string_lossy().into_owned(),
        )),
    }
}

/// The flags of `serve`; `None` for `--help`.
fn parse_serve<'a>(
    mut args: impl Iterator<Item = &'a OsString>,
) -> Result<Option<ServeArgs>, CliError> {
    let mut serve = ServeArgs::default();
    while let Some(arg) = args.next() {
        let Some(text) = arg.to_str() else {
            return Err(CliError::UnknownFlag(arg.to_string_lossy().into_owned()));
        };
        // `--flag=value` as well as `--flag value`.
        let (flag, inline) = match text.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (text, None),
        };
        let mut value = |name: &'static str| -> Result<OsString, CliError> {
            match inline {
                Some(value) => Ok(value.into()),
                None => args.next().cloned().ok_or(CliError::MissingValue(name)),
            }
        };
        match flag {
            "-h" | "--help" => return Ok(None),
            "--no-auth" => {
                if inline.is_some() {
                    return Err(CliError::UnknownFlag(text.to_owned()));
                }
                if std::mem::replace(&mut serve.no_auth, true) {
                    return Err(CliError::Repeated("--no-auth"));
                }
            }
            "--config" => set(&mut serve.config, "--config", value("--config")?)?,
            "--data" => set(&mut serve.data, "--data", value("--data")?)?,
            "--tls-cert" => set(&mut serve.tls_cert, "--tls-cert", value("--tls-cert")?)?,
            "--tls-key" => set(&mut serve.tls_key, "--tls-key", value("--tls-key")?)?,
            "--token-file" => set(
                &mut serve.token_file,
                "--token-file",
                value("--token-file")?,
            )?,
            "--listen" => {
                let listen = value("--listen")?
                    .into_string()
                    .map_err(|_| CliError::NotUtf8("--listen"))?;
                set(&mut serve.listen, "--listen", listen)?;
            }
            _ => return Err(CliError::UnknownFlag(text.to_owned())),
        }
    }
    Ok(Some(serve))
}

/// Stores a flag's value, refusing a second one.
fn set<T>(slot: &mut Option<T>, flag: &'static str, value: impl Into<T>) -> Result<(), CliError> {
    if slot.is_some() {
        return Err(CliError::Repeated(flag));
    }
    *slot = Some(value.into());
    Ok(())
}

/// The keys of the `--config` file. Relative paths in it are relative to the working directory.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    listen: Option<String>,
    data: Option<PathBuf>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    token_file: Option<PathBuf>,
}

impl ConfigFile {
    fn load(path: &Path) -> Result<Self, CliError> {
        let text = std::fs::read_to_string(path).map_err(|source| read_error(path, source))?;
        toml::from_str(&text).map_err(|e| CliError::Config {
            path: path.display().to_string(),
            message: e.message().to_owned(),
        })
    }
}

/// Everything `serve` needs, once the flags, the file, and the environment are merged and checked.
#[derive(Debug)]
pub(crate) struct Settings {
    /// The address to listen on, `host:port`.
    pub(crate) listen: String,
    /// The plugin's data directory.
    pub(crate) data: PathBuf,
    /// The TLS certificate chain and private key files.
    #[cfg_attr(not(feature = "tls"), allow(dead_code))]
    pub(crate) tls: Option<(PathBuf, PathBuf)>,
    /// How to check the core's token.
    pub(crate) auth: RemoteAuth,
}

/// The environment variable that holds the bearer token of the plugin called `name`, e.g.
/// `GILES_CHANNEL_DISCORD_TOKEN`.
pub(crate) fn token_env(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("GILES_CHANNEL_{name}_TOKEN")
}

impl ServeArgs {
    /// Merges the flags over the `--config` file and checks the result. `plugin` is the plugin's
    /// name (for the token variable) and `env` reads the environment.
    pub(crate) fn resolve(
        self,
        plugin: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Result<Settings, CliError> {
        let file = match &self.config {
            Some(path) => ConfigFile::load(path)?,
            None => ConfigFile::default(),
        };
        let listen = self.listen.or(file.listen).ok_or(CliError::Missing {
            key: "listen",
            flag: "--listen",
        })?;
        let data = self.data.or(file.data).ok_or(CliError::Missing {
            key: "data",
            flag: "--data",
        })?;
        let tls = match (
            self.tls_cert.or(file.tls_cert),
            self.tls_key.or(file.tls_key),
        ) {
            #[cfg(feature = "tls")]
            (Some(cert), Some(key)) => Some((cert, key)),
            #[cfg(not(feature = "tls"))]
            (Some(_), Some(_)) => return Err(CliError::NoTls),
            (None, None) => None,
            _ => return Err(CliError::TlsPair),
        };
        let auth = resolve_auth(
            plugin,
            self.token_file.or(file.token_file).as_deref(),
            self.no_auth,
            env,
        )?;
        Ok(Settings {
            listen,
            data,
            tls,
            auth,
        })
    }
}

/// Where the token comes from: exactly one of `token_file`, the plugin's variable, or no token
/// at all with `--no-auth`.
fn resolve_auth(
    plugin: &str,
    token_file: Option<&Path>,
    no_auth: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<RemoteAuth, CliError> {
    let var = token_env(plugin);
    let from_env = env(&var);
    if no_auth {
        return if token_file.is_some() || from_env.is_some() {
            Err(CliError::NoAuthWithToken)
        } else {
            Ok(RemoteAuth::None)
        };
    }
    let token = match (token_file, from_env) {
        (Some(_), Some(_)) => return Err(CliError::TwoTokens { env: var }),
        (None, None) => return Err(CliError::NoToken { env: var }),
        (Some(path), None) => read_token_file(path)?,
        (None, Some(value)) => value.into_string().map_err(|_| CliError::BadToken {
            origin: var.clone(),
        })?,
    };
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(CliError::BadToken {
            origin: token_file.map_or(var, |path| path.display().to_string()),
        });
    }
    Ok(RemoteAuth::Bearer(token))
}

/// The token in `path`: a regular file that only its owner can access, with the token on one line.
fn read_token_file(path: &Path) -> Result<String, CliError> {
    let meta = std::fs::metadata(path).map_err(|source| read_error(path, source))?;
    if !meta.is_file() {
        return Err(read_error(
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
        ));
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CliError::TokenFileMode {
            path: path.display().to_string(),
            mode,
        });
    }
    let mut token = std::fs::read_to_string(path).map_err(|source| read_error(path, source))?;
    // The newline an editor or `echo` leaves.
    if token.ends_with('\n') {
        token.pop();
        if token.ends_with('\r') {
            token.pop();
        }
    }
    Ok(token)
}

fn read_error(path: &Path, source: io::Error) -> CliError {
    CliError::Read {
        path: path.display().to_string(),
        source,
    }
}

/// Why `serve` failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    /// The command line or the settings are unusable.
    #[error(transparent)]
    Cli(#[from] CliError),
    /// The address couldn't be bound.
    #[error("cannot listen on {address}: {source}")]
    Listen {
        /// The address.
        address: String,
        /// Why not.
        source: io::Error,
    },
    /// The certificate or key isn't usable.
    #[cfg(feature = "tls")]
    #[error("TLS: {0}")]
    Tls(io::Error),
    /// Serving failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl RunError {
    /// The process exit status: 2 for settings the operator must fix, 1 for the rest.
    pub(crate) fn exit_code(&self) -> u8 {
        match self {
            Self::Cli(_) => 2,
            _ => 1,
        }
    }
}

/// The server TLS config for the certificate chain in `cert` and the private key in `key`.
#[cfg(feature = "tls")]
pub(crate) fn tls_config(cert: &Path, key: &Path) -> Result<rustls::ServerConfig, RunError> {
    let cert_pem = std::fs::read(cert).map_err(|source| read_error(cert, source))?;
    let key_pem = std::fs::read(key).map_err(|source| read_error(key, source))?;
    crate::tls::server_config(&cert_pem, &key_pem).map_err(RunError::Tls)
}

/// The help text for the binary `binary`, the plugin `plugin`.
pub(crate) fn usage(binary: &str, plugin: &str) -> String {
    let token_env = token_env(plugin);
    format!(
        "\
Usage: {binary} <command>

Commands:
  serve [options]  Run as a remote plugin: listen on TCP for a Giles core that has
                   `address` set for this plugin's instance.
  socket           How Giles launches the plugin itself. Not meant to be run by hand.

Options of serve (each is also a key of the --config file, in a TOML table; a flag wins):
  --config <file>      A TOML file with the options below as keys, without the dashes and with
                       `_` for `-`: listen, data, tls_cert, tls_key, token_file.
  --listen <address>   The address to listen on, e.g. 0.0.0.0:7443. Required.
  --data <dir>         The plugin's data directory. Required.
  --tls-cert <file>    The PEM certificate chain to serve TLS with. With --tls-key.
  --tls-key <file>     The PEM private key.
  --token-file <file>  A file holding the bearer token Giles must send, readable only by you.
  --no-auth            Accept any caller, with no token. For a plugin on a private network.

The bearer token is {token_env}, or --token-file. It is the instance's
`auth = {{ bearer = ... }}` in giles.toml. Without one, serve refuses to start unless --no-auth is
given. The plugin's own settings come from `[channel.<name>.config]` in giles.toml.
"
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn serve(words: &[&str]) -> ServeArgs {
        let mut all = vec!["serve"];
        all.extend(words);
        match parse(&args(&all)).unwrap() {
            Command::Serve(serve) => serve,
            other => panic!("{other:?}"),
        }
    }

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    fn token_file(dir: &Path, contents: &str, mode: u32) -> PathBuf {
        let path = dir.join("token");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn the_subcommands() {
        assert_eq!(parse(&args(&["socket"])).unwrap(), Command::Socket);
        for help in [["--help"], ["-h"], ["help"]] {
            assert_eq!(parse(&args(&help)).unwrap(), Command::Help);
        }
        assert_eq!(
            parse(&args(&["serve", "--no-auth", "--help"])).unwrap(),
            Command::Help
        );
    }

    #[test]
    fn serve_flags_take_a_value_after_a_space_or_an_equals_sign() {
        let parsed = serve(&[
            "--listen=0.0.0.0:7443",
            "--data",
            "/var/lib/x",
            "--tls-cert",
            "c.pem",
            "--tls-key=k.pem",
            "--token-file",
            "t",
            "--config",
            "c.toml",
            "--no-auth",
        ]);
        assert_eq!(
            parsed,
            ServeArgs {
                config: Some("c.toml".into()),
                listen: Some("0.0.0.0:7443".into()),
                data: Some("/var/lib/x".into()),
                tls_cert: Some("c.pem".into()),
                tls_key: Some("k.pem".into()),
                token_file: Some("t".into()),
                no_auth: true,
            }
        );
    }

    #[test]
    fn a_value_may_start_with_dashes() {
        assert_eq!(
            serve(&["--data", "--odd"]).data,
            Some(PathBuf::from("--odd"))
        );
    }

    #[test]
    fn bad_command_lines() {
        let err = |words: &[&str]| parse(&args(words)).unwrap_err().to_string();
        assert_eq!(err(&[]), "no command given");
        assert_eq!(err(&["launch"]), "unknown command \"launch\"");
        assert_eq!(err(&["--listen", "x"]), "unknown command \"--listen\"");
        assert_eq!(
            err(&["socket", "--listen"]),
            "`socket` takes no argument \"--listen\""
        );
        assert_eq!(err(&["serve", "--port", "1"]), "unknown option \"--port\"");
        assert_eq!(err(&["serve", "stray"]), "unknown option \"stray\"");
        assert_eq!(err(&["serve", "--listen"]), "--listen needs a value");
        assert_eq!(
            err(&["serve", "--data", "a", "--data=b"]),
            "--data given more than once"
        );
        assert_eq!(
            err(&["serve", "--no-auth", "--no-auth"]),
            "--no-auth given more than once"
        );
        assert_eq!(
            err(&["serve", "--no-auth=yes"]),
            "unknown option \"--no-auth=yes\""
        );
    }

    #[test]
    fn flags_and_the_file_merge_with_flags_winning() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("serve.toml");
        std::fs::write(
            &config,
            "listen = \"0.0.0.0:1\"\ndata = \"/from/file\"\ntls_cert = \"c.pem\"\n\
             tls_key = \"k.pem\"\ntoken_file = \"unused\"\n",
        )
        .unwrap();
        let token = token_file(dir.path(), "hunter2\n", 0o600);
        let settings = serve(&[
            "--config",
            config.to_str().unwrap(),
            "--listen",
            "127.0.0.1:2",
            "--token-file",
            token.to_str().unwrap(),
        ])
        .resolve("foo", &no_env)
        .unwrap();
        assert_eq!(settings.listen, "127.0.0.1:2");
        assert_eq!(settings.data, Path::new("/from/file"));
        assert_eq!(
            settings.tls,
            Some((PathBuf::from("c.pem"), PathBuf::from("k.pem")))
        );
        assert!(matches!(settings.auth, RemoteAuth::Bearer(t) if t == "hunter2"));
    }

    #[test]
    fn the_flags_alone_are_enough() {
        let settings = serve(&["--listen", "[::]:7443", "--data", "d", "--no-auth"])
            .resolve("foo", &no_env)
            .unwrap();
        assert_eq!(settings.listen, "[::]:7443");
        assert!(settings.tls.is_none());
        assert!(matches!(settings.auth, RemoteAuth::None));
    }

    fn resolve_err(words: &[&str], env: &[(&str, &str)]) -> String {
        serve(words)
            .resolve("foo-bar", &env_of(env))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn a_missing_listen_or_data_is_named_with_its_flag() {
        assert_eq!(
            resolve_err(&["--data", "d", "--no-auth"], &[]),
            "no listen: set `listen` in the --config file or pass --listen"
        );
        assert_eq!(
            resolve_err(&["--listen", "x:1", "--no-auth"], &[]),
            "no data: set `data` in the --config file or pass --data"
        );
    }

    #[test]
    fn tls_needs_both_files() {
        let base = ["--listen", "x:1", "--data", "d", "--no-auth"];
        for flag in ["--tls-cert", "--tls-key"] {
            let mut words = base.to_vec();
            words.extend([flag, "f.pem"]);
            assert_eq!(
                resolve_err(&words, &[]),
                "`tls_cert` and `tls_key` go together: set both or neither"
            );
        }
    }

    #[test]
    fn an_unusable_config_file_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("serve.toml");
        let path = config.to_str().unwrap();
        let error = resolve_err(&["--config", path], &[]);
        assert!(error.starts_with(&format!("reading {path}: ")), "{error}");
        std::fs::write(&config, "listen = 7\n").unwrap();
        let error = resolve_err(&["--config", path], &[]);
        assert!(error.starts_with(&format!("{path}: ")), "{error}");
        std::fs::write(&config, "listen = \"x:1\"\nlisten_on = \"y\"\n").unwrap();
        let error = resolve_err(&["--config", path], &[]);
        assert!(error.contains("listen_on"), "{error}");
    }

    #[test]
    fn the_token_comes_from_the_plugins_variable() {
        let words = ["--listen", "x:1", "--data", "d"];
        let settings = serve(&words)
            .resolve(
                "foo-bar",
                &env_of(&[("GILES_CHANNEL_FOO_BAR_TOKEN", "abc")]),
            )
            .unwrap();
        assert!(matches!(settings.auth, RemoteAuth::Bearer(t) if t == "abc"));
        assert_eq!(
            resolve_err(&words, &[("GILES_CHANNEL_FOO_TOKEN", "abc")]),
            "no bearer token: set GILES_CHANNEL_FOO_BAR_TOKEN, or `token_file` (--token-file), \
             or pass --no-auth to accept any caller"
        );
    }

    #[test]
    fn a_token_must_be_a_usable_header_value() {
        let words = ["--listen", "x:1", "--data", "d"];
        for bad in ["", "two words", "tab\t", "caf\u{e9}", "line\nbreak"] {
            let error = resolve_err(&words, &[("GILES_CHANNEL_FOO_BAR_TOKEN", bad)]);
            assert_eq!(
                error,
                "the bearer token from GILES_CHANNEL_FOO_BAR_TOKEN must be one line of visible \
                 ASCII, and not empty"
            );
            assert!(!error.contains(bad) || bad.is_empty());
        }
    }

    #[test]
    fn one_token_source_and_no_contradiction() {
        let dir = tempfile::tempdir().unwrap();
        let file = token_file(dir.path(), "abc", 0o600);
        let file = file.to_str().unwrap();
        let var = [("GILES_CHANNEL_FOO_BAR_TOKEN", "abc")];
        assert_eq!(
            resolve_err(&["--listen=x:1", "--data=d", "--token-file", file], &var),
            "the token is set both in GILES_CHANNEL_FOO_BAR_TOKEN and by `token_file`: use one"
        );
        for env in [&var[..], &[]] {
            let mut words = vec!["--listen=x:1", "--data=d", "--no-auth"];
            if env.is_empty() {
                words.extend(["--token-file", file]);
            }
            assert_eq!(
                resolve_err(&words, env),
                "--no-auth contradicts the bearer token configured: use one"
            );
        }
    }

    #[test]
    fn a_token_file_is_read_up_to_its_newline_and_must_be_private() {
        let dir = tempfile::tempdir().unwrap();
        let words = |file: &Path| {
            vec![
                "--listen=x:1".to_owned(),
                "--data=d".to_owned(),
                format!("--token-file={}", file.display()),
            ]
        };
        let resolve = |contents: &str, mode: u32| {
            let file = token_file(dir.path(), contents, mode);
            let words = words(&file);
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            serve(&words).resolve("foo", &no_env)
        };
        for (contents, token) in [("abc", "abc"), ("abc\n", "abc"), ("abc\r\n", "abc")] {
            let auth = resolve(contents, 0o400).unwrap().auth;
            assert!(
                matches!(auth, RemoteAuth::Bearer(t) if t == token),
                "{contents:?}"
            );
        }
        for contents in ["", "\n", "a\nb\n", "a b"] {
            let error = resolve(contents, 0o600).unwrap_err();
            assert!(
                matches!(error, CliError::BadToken { .. }),
                "{contents:?}: {error}"
            );
        }
        for mode in [0o640, 0o604, 0o644, 0o660] {
            let error = resolve("abc", mode).unwrap_err().to_string();
            assert!(
                error.contains(&format!("mode {mode:04o}")) && error.contains("chmod 600"),
                "{error}"
            );
        }
        let missing = dir.path().join("missing");
        let words = words(&missing);
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        let error = serve(&words)
            .resolve("foo", &no_env)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(&format!("reading {}: ", missing.display())),
            "{error}"
        );
    }

    #[test]
    fn usage_names_the_binary_and_the_token_variable() {
        let text = usage("giles-channel-foo-bar", "foo-bar");
        assert!(text.starts_with("Usage: giles-channel-foo-bar <command>"));
        assert!(text.contains("GILES_CHANNEL_FOO_BAR_TOKEN"));
    }
}
