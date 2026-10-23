//! The instance config (`[channel.<name>] config`), validated into [`Settings`].

use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use giles_plugin_sdk::giles_types::{Error, Result};
use giles_plugin_sdk::tls::rustls::pki_types::pem::PemObject as _;
use giles_plugin_sdk::tls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use giles_plugin_sdk::tls::rustls::{self, ClientConfig, RootCertStore};
use serde::Deserialize;

/// The default port with TLS.
pub const TLS_PORT: u16 = 6697;
/// The default port without TLS.
pub const PLAIN_PORT: u16 = 6667;
/// The default for `max_lines`.
pub const DEFAULT_MAX_LINES: usize = 30;
/// The default for `send_burst`.
pub const DEFAULT_SEND_BURST: u32 = 5;
/// The default for `send_interval_ms`.
pub const DEFAULT_SEND_INTERVAL_MS: u64 = 1000;
/// The smallest `max_line_bytes`: room for a few characters and a formatting code.
const MIN_LINE_BYTES: usize = 16;
/// The longest nick, user name, or channel name accepted in the config.
const MAX_NAME: usize = 64;

/// The config as written. Unknown keys are refused. [`Settings::new`] validates it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The server's host name or IP address, optionally with `:<port>` (`[<IPv6>]:<port>`).
    pub server: String,
    /// The port, unless `server` has one. Default 6697 with TLS, 6667 without.
    #[serde(default)]
    pub port: Option<u16>,
    /// Whether to connect with TLS. Default `true`.
    #[serde(default = "yes")]
    pub tls: bool,
    /// Allows `tls = false` to a server that isn't on this machine.
    #[serde(default)]
    pub insecure: bool,
    /// The nick to register.
    pub nick: String,
    /// The user name (ident). Default: the nick.
    #[serde(default)]
    pub username: Option<String>,
    /// The real name (gecos). Default `Giles`.
    #[serde(default)]
    pub realname: Option<String>,
    /// The server password (`PASS`), e.g. for a bouncer. A secret.
    #[serde(default)]
    pub password: Option<String>,
    /// SASL authentication.
    #[serde(default)]
    pub sasl: Option<SaslConfig>,
    /// A PEM client certificate chain to present (CertFP, SASL EXTERNAL). A secret.
    #[serde(default)]
    pub client_cert: Option<String>,
    /// The PEM private key of `client_cert`. A secret.
    #[serde(default)]
    pub client_key: Option<String>,
    /// The network name: the identity namespace is `irc:<network>`.
    pub network: String,
    /// Whether `mask:<nick>!<user>@<host>` identities count as verified.
    #[serde(default)]
    pub mask_identities: bool,
    /// Channels to join.
    #[serde(default)]
    pub channels: Vec<String>,
    /// An upper limit on the text bytes of one message line, below what the protocol allows.
    #[serde(default)]
    pub max_line_bytes: Option<usize>,
    /// The most lines one sent message may take; the rest is replaced by a note.
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
    /// Lines sent at once before the rate limit applies.
    #[serde(default = "default_send_burst")]
    pub send_burst: u32,
    /// Milliseconds between lines once the burst is used up.
    #[serde(default = "default_send_interval_ms")]
    pub send_interval_ms: u64,
}

fn yes() -> bool {
    true
}

fn default_max_lines() -> usize {
    DEFAULT_MAX_LINES
}

fn default_send_burst() -> u32 {
    DEFAULT_SEND_BURST
}

fn default_send_interval_ms() -> u64 {
    DEFAULT_SEND_INTERVAL_MS
}

/// The `sasl` table.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaslConfig {
    /// `plain` (default) or `external`.
    #[serde(default)]
    pub mechanism: Mechanism,
    /// The account to log in as (`plain`). Default: the nick.
    #[serde(default)]
    pub username: Option<String>,
    /// The account password (`plain`). A secret.
    #[serde(default)]
    pub password: Option<String>,
}

/// A SASL mechanism.
#[derive(Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mechanism {
    /// User name and password.
    #[default]
    Plain,
    /// The TLS client certificate.
    External,
}

/// A secret config value. Never shown in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The value.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// SASL, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sasl {
    /// `AUTHENTICATE PLAIN` with these credentials.
    Plain {
        /// The account name.
        username: String,
        /// The password.
        password: Secret,
    },
    /// `AUTHENTICATE EXTERNAL`: the server identifies us by the client certificate.
    External,
}

impl Sasl {
    /// The mechanism name on the wire.
    pub fn mechanism(&self) -> &'static str {
        match self {
            Self::Plain { .. } => "PLAIN",
            Self::External => "EXTERNAL",
        }
    }
}

/// The validated config.
#[derive(Debug)]
pub struct Settings {
    /// The server host (no brackets around an IPv6 address).
    pub host: String,
    /// The server port.
    pub port: u16,
    /// Whether to use TLS.
    pub tls: bool,
    /// The name the server's certificate must carry.
    pub server_name: ServerName<'static>,
    /// The nick to register.
    pub nick: String,
    /// The user name.
    pub username: String,
    /// The real name.
    pub realname: String,
    /// The server password.
    pub password: Option<Secret>,
    /// SASL.
    pub sasl: Option<Sasl>,
    /// The client certificate chain and key.
    client_identity: Option<ClientIdentity>,
    /// The network name.
    pub network: String,
    /// Whether mask identities count as verified.
    pub mask_identities: bool,
    /// Channels to join.
    pub channels: Vec<String>,
    /// The configured cap on line bytes.
    pub max_line_bytes: Option<usize>,
    /// The most lines per sent message.
    pub max_lines: usize,
    /// Lines sent at once before the rate limit applies.
    pub send_burst: u32,
    /// Time between lines after the burst.
    pub send_interval: Duration,
}

struct ClientIdentity {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("certificates", &self.chain.len())
            .finish_non_exhaustive()
    }
}

/// A config error naming `key`, in the SDK's format. Never includes the value: it may be a secret.
fn invalid(key: &str, reason: impl fmt::Display) -> Error {
    Error::InvalidRequest(format!("invalid config: {key}: {reason}"))
}

impl Settings {
    /// Validates `config`. Errors are [`Error::InvalidRequest`] naming the key, like
    /// `invalid config: nick: ...`, and never contain a secret.
    pub fn new(config: Config) -> Result<Self> {
        let (host, server_port) = split_server(&config.server)?;
        let port = match (server_port, config.port) {
            (Some(_), Some(_)) => return Err(invalid("port", "also given in `server`")),
            (Some(port), None) | (None, Some(port)) => port,
            (None, None) if config.tls => TLS_PORT,
            (None, None) => PLAIN_PORT,
        };
        if port == 0 {
            return Err(invalid("port", "must not be 0"));
        }
        let server_name = ServerName::try_from(host.clone())
            .map_err(|_| invalid("server", "not a valid host name or IP address"))?;
        let local = is_local(&host);
        if !config.tls && !local && !config.insecure {
            return Err(invalid(
                "tls",
                "plain TCP to a server that isn't on this machine lets anyone on the network path \
                 forge messages and identities; set `insecure = true` to allow it",
            ));
        }

        if !is_nick(&config.nick) {
            return Err(invalid("nick", "not a valid IRC nick"));
        }
        let username = config.username.unwrap_or_else(|| config.nick.clone());
        if !is_word(&username, |c| c == '@') {
            return Err(invalid(
                "username",
                "must be 1-64 characters without spaces or `@`",
            ));
        }
        let realname = config.realname.unwrap_or_else(|| "Giles".to_owned());
        if realname.is_empty() || realname.chars().any(char::is_control) {
            return Err(invalid(
                "realname",
                "must be non-empty, without control characters",
            ));
        }
        let password = match config.password {
            Some(password) => Some(secret("password", password)?),
            None => None,
        };
        if password.is_some() && !config.tls && !local {
            return Err(invalid(
                "password",
                "is only sent over TLS or to this machine",
            ));
        }

        let client_identity = match (config.client_cert, config.client_key) {
            (None, None) => None,
            (Some(cert), Some(key)) => Some(client_identity(&cert, &key)?),
            (Some(_), None) => return Err(invalid("client_key", "required with `client_cert`")),
            (None, Some(_)) => return Err(invalid("client_cert", "required with `client_key`")),
        };
        if client_identity.is_some() && !config.tls {
            return Err(invalid("client_cert", "needs `tls = true`"));
        }
        let sasl = match config.sasl {
            None => None,
            Some(sasl) => Some(validate_sasl(
                sasl,
                &config.nick,
                client_identity.is_some(),
                config.tls || local,
            )?),
        };

        if config.network.is_empty()
            || config
                .network
                .chars()
                .any(|c| c == ':' || c.is_whitespace() || c.is_control())
        {
            return Err(invalid(
                "network",
                "must be non-empty, without `:`, spaces, or control characters",
            ));
        }
        for (i, channel) in config.channels.iter().enumerate() {
            if !is_channel(channel) {
                return Err(invalid(
                    &format!("channels[{i}]"),
                    "not a channel name (starts with `#`, `&`, `+`, or `!`; no spaces, commas, or \
                     `:`)",
                ));
            }
        }
        if config
            .max_line_bytes
            .is_some_and(|max| max < MIN_LINE_BYTES)
        {
            return Err(invalid(
                "max_line_bytes",
                format!("must be at least {MIN_LINE_BYTES}"),
            ));
        }
        if config.max_lines == 0 {
            return Err(invalid("max_lines", "must be at least 1"));
        }
        if config.send_burst == 0 {
            return Err(invalid("send_burst", "must be at least 1"));
        }
        let settings = Self {
            host,
            port,
            tls: config.tls,
            server_name,
            nick: config.nick,
            username,
            realname,
            password,
            sasl,
            client_identity,
            network: config.network,
            mask_identities: config.mask_identities,
            channels: config.channels,
            max_line_bytes: config.max_line_bytes,
            max_lines: config.max_lines,
            send_burst: config.send_burst,
            send_interval: Duration::from_millis(config.send_interval_ms),
        };
        // Catches a key that doesn't match the certificate, or that rustls can't use.
        settings.tls_config(RootCertStore::empty())?;
        Ok(settings)
    }

    /// The identity namespace: `irc:<network>`.
    pub fn namespace(&self) -> String {
        format!("irc:{}", self.network)
    }

    /// The TLS client config trusting `roots` (the bundled ones in production), presenting the
    /// client certificate if there is one. `None` without TLS.
    pub fn tls_config(&self, roots: RootCertStore) -> Result<Option<Arc<ClientConfig>>> {
        if !self.tls {
            return Ok(None);
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let builder = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(Error::other)?
            .with_root_certificates(roots);
        let config = match &self.client_identity {
            None => builder.with_no_client_auth(),
            Some(identity) => builder
                .with_client_auth_cert(identity.chain.clone(), identity.key.clone_key())
                .map_err(|e| invalid("client_key", e))?,
        };
        Ok(Some(Arc::new(config)))
    }
}

/// `host`, `host:port`, `[v6]`, `[v6]:port`, or a bare IPv6 address.
fn split_server(server: &str) -> Result<(String, Option<u16>)> {
    let bad = || {
        invalid(
            "server",
            "expected a host name or IP address, optionally with `:<port>`",
        )
    };
    if server.is_empty() || server.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad());
    }
    let (host, port) = if let Some(rest) = server.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(bad)?;
        match after {
            "" => (host, None),
            _ => (host, Some(after.strip_prefix(':').ok_or_else(bad)?)),
        }
    } else if server.matches(':').count() == 1 {
        let (host, port) = server.split_once(':').ok_or_else(bad)?;
        (host, Some(port))
    } else {
        (server, None)
    };
    if host.is_empty() {
        return Err(bad());
    }
    let port = match port {
        None => None,
        Some(port) => Some(
            port.parse::<u16>()
                .map_err(|_| invalid("server", "the port must be a number from 1 to 65535"))?,
        ),
    };
    Ok((host.to_owned(), port))
}

/// Whether `host` is this machine: `localhost` or a loopback address.
fn is_local(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// RFC 2812 nick characters: a letter or one of ``[]\`_^{|}``, then also digits and `-`.
pub(crate) fn is_nick(nick: &str) -> bool {
    let special = |c: char| "[]\\`_^{|}".contains(c);
    let mut chars = nick.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || special(c))
        && chars.all(|c| c.is_ascii_alphanumeric() || special(c) || c == '-')
        && nick.len() <= MAX_NAME
}

/// A non-empty word of at most [`MAX_NAME`] bytes without whitespace, control characters, or
/// `forbidden` ones.
fn is_word(word: &str, forbidden: impl Fn(char) -> bool) -> bool {
    !word.is_empty()
        && word.len() <= MAX_NAME
        && !word
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || forbidden(c))
}

/// A channel name: a prefix from `#&+!` and a name without spaces, commas, `:`, or control
/// characters.
pub(crate) fn is_channel(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with(['#', '&', '+', '!'])
        && is_word(name, |c| c == ',' || c == ':')
}

fn secret(key: &str, value: String) -> Result<Secret> {
    if value.is_empty() || value.contains(['\r', '\n', '\0']) {
        return Err(invalid(key, "must be non-empty, on one line"));
    }
    Ok(Secret(value))
}

fn validate_sasl(sasl: SaslConfig, nick: &str, has_cert: bool, protected: bool) -> Result<Sasl> {
    match sasl.mechanism {
        Mechanism::Plain => {
            let password = sasl
                .password
                .ok_or_else(|| invalid("sasl.password", "required for mechanism `plain`"))?;
            let password = secret("sasl.password", password)?;
            let username = sasl.username.unwrap_or_else(|| nick.to_owned());
            if !is_word(&username, |c| c == '\0') {
                return Err(invalid(
                    "sasl.username",
                    "must be 1-64 characters without spaces",
                ));
            }
            if !protected {
                return Err(invalid(
                    "sasl",
                    "the password is only sent over TLS or to this machine",
                ));
            }
            Ok(Sasl::Plain { username, password })
        }
        Mechanism::External => {
            if sasl.password.is_some() {
                return Err(invalid(
                    "sasl.password",
                    "not used with mechanism `external`",
                ));
            }
            if sasl.username.is_some() {
                return Err(invalid(
                    "sasl.username",
                    "not used with mechanism `external`",
                ));
            }
            if !has_cert {
                return Err(invalid(
                    "sasl.mechanism",
                    "`external` needs `client_cert` and `client_key`",
                ));
            }
            Ok(Sasl::External)
        }
    }
}

fn client_identity(cert: &str, key: &str) -> Result<ClientIdentity> {
    let chain = CertificateDer::pem_slice_iter(cert.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid("client_cert", "not a PEM certificate chain"))?;
    if chain.is_empty() {
        return Err(invalid("client_cert", "has no PEM certificate"));
    }
    let key = PrivateKeyDer::from_pem_slice(key.as_bytes())
        .map_err(|_| invalid("client_key", "not a PEM private key"))?;
    Ok(ClientIdentity { chain, key })
}

#[cfg(test)]
mod tests;
