//! The instance's `config` table: parsed by the SDK's `Configure::parse_config` (serde errors name
//! the key path), then validated here with errors in the same form, `invalid config: <key>: ...`.

use std::fmt;
use std::time::Duration;

use giles_plugin_sdk::config::{ByteSize, HumanDuration};
use giles_plugin_sdk::giles_types::{Error, Result};
use serde::Deserialize;

use crate::address;

/// The default largest message downloaded whole, matching the core's default `max_blob`.
const DEFAULT_MAX_MESSAGE_SIZE: u64 = 50 << 20;
/// The default IDLE restart interval, well under RFC 2177's 29 minutes.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// The longest IDLE allowed before it must be restarted (RFC 2177).
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(29 * 60);
/// The default polling interval for servers without IDLE (or with `idle = false`).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How a connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Tls {
    /// TLS from the first byte (IMAP 993, SMTP 465).
    Implicit,
    /// Plain text upgraded with `STARTTLS` before logging in (IMAP 143, SMTP 587).
    Starttls,
    /// No TLS: only to a server on this machine (e.g. a local bridge).
    None,
}

/// A password: never shown by `Debug`.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub(crate) struct Secret(String);

impl Secret {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The `config` table as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConfig {
    authserv_id: String,
    address: String,
    #[serde(default)]
    name: Option<String>,
    imap: RawImap,
    smtp: RawSmtp,
    #[serde(default)]
    max_message_size: Option<ByteSize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImap {
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    tls: Option<Tls>,
    user: String,
    password: Secret,
    #[serde(default)]
    mailbox: Option<String>,
    #[serde(default)]
    idle: Option<bool>,
    #[serde(default)]
    idle_timeout: Option<HumanDuration>,
    #[serde(default)]
    poll_interval: Option<HumanDuration>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSmtp {
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    tls: Option<Tls>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<Secret>,
}

/// A validated config.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// The receiving server's `authserv-id` whose `Authentication-Results` are trusted.
    pub authserv_id: String,
    /// Our address (normalized): `From:` of everything sent.
    pub address: String,
    /// Our display name in `From:`.
    pub name: Option<String>,
    pub imap: Imap,
    pub smtp: Smtp,
    /// Messages larger than this are delivered as headers plus a note.
    pub max_message_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Server {
    pub host: String,
    pub port: u16,
    pub tls: Tls,
}

#[derive(Debug, Clone)]
pub(crate) struct Imap {
    pub server: Server,
    pub user: String,
    pub password: Secret,
    pub mailbox: String,
    /// Use IDLE when the server offers it.
    pub idle: bool,
    pub idle_timeout: Duration,
    pub poll_interval: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct Smtp {
    pub server: Server,
    pub credentials: Option<(String, Secret)>,
}

/// An `invalid config: <key>: <reason>` error.
fn invalid(key: &str, reason: impl fmt::Display) -> Error {
    Error::InvalidRequest(format!("invalid config: {key}: {reason}"))
}

#[cfg(test)]
impl Config {
    /// Parses and validates a config without the SDK (whose `parse_config` adds the key path
    /// to serde's errors).
    pub(crate) fn from_json(value: &serde_json::Value) -> Result<Self> {
        serde_json::from_value::<RawConfig>(value.clone())
            .map_err(|e| Error::InvalidRequest(format!("invalid config: {e}")))?
            .validate()
    }
}

impl RawConfig {
    /// Checks the values serde can't, and applies the defaults.
    pub(crate) fn validate(self) -> Result<Config> {
        let authserv_id = self.authserv_id.trim().to_owned();
        if authserv_id.is_empty() || authserv_id.contains(char::is_whitespace) {
            return Err(invalid(
                "authserv_id",
                "must be the receiving server's authserv-id, e.g. \"mx.example.com\"",
            ));
        }
        let address = address::normalize(&self.address)
            .ok_or_else(|| invalid("address", "not an email address"))?;
        if let Some(name) = &self.name
            && name.chars().any(char::is_control)
        {
            return Err(invalid("name", "contains control characters"));
        }
        let imap = self.imap.validate()?;
        let smtp = self.smtp.validate()?;
        let max_message_size = self
            .max_message_size
            .map_or(DEFAULT_MAX_MESSAGE_SIZE, |size| size.0);
        if max_message_size == 0 {
            return Err(invalid("max_message_size", "must be more than 0"));
        }
        Ok(Config {
            authserv_id,
            address,
            name: self.name,
            imap,
            smtp,
            max_message_size,
        })
    }
}

/// Checks a server's host, port, and TLS mode, with the port defaulting by mode.
fn server(
    section: &str,
    host: String,
    port: Option<u16>,
    tls: Tls,
    default_ports: [u16; 3],
) -> Result<Server> {
    let host = host.trim().to_owned();
    if host.is_empty() || host.contains(|c: char| c.is_whitespace() || c.is_control()) {
        return Err(invalid(&format!("{section}.host"), "must be a host name"));
    }
    if tls == Tls::None && !address::is_loopback_host(&host) {
        return Err(invalid(
            &format!("{section}.tls"),
            "\"none\" is only allowed for a server on this machine (localhost, 127.0.0.0/8, \
             ::1); use \"implicit\" or \"starttls\"",
        ));
    }
    let [implicit, starttls, none] = default_ports;
    let port = port.unwrap_or(match tls {
        Tls::Implicit => implicit,
        Tls::Starttls => starttls,
        Tls::None => none,
    });
    if port == 0 {
        return Err(invalid(&format!("{section}.port"), "must be 1 to 65535"));
    }
    Ok(Server { host, port, tls })
}

impl RawImap {
    fn validate(self) -> Result<Imap> {
        let server = server(
            "imap",
            self.host,
            self.port,
            self.tls.unwrap_or(Tls::Implicit),
            [993, 143, 143],
        )?;
        if self.user.is_empty() {
            return Err(invalid("imap.user", "must not be empty"));
        }
        let mailbox = self.mailbox.unwrap_or_else(|| "INBOX".into());
        if mailbox.is_empty() || mailbox.chars().any(char::is_control) {
            return Err(invalid("imap.mailbox", "must be a mailbox name"));
        }
        let idle_timeout = self.idle_timeout.map_or(DEFAULT_IDLE_TIMEOUT, |d| d.0);
        if idle_timeout.is_zero() || idle_timeout > MAX_IDLE_TIMEOUT {
            return Err(invalid(
                "imap.idle_timeout",
                "must be more than 0s and at most 29m (RFC 2177)",
            ));
        }
        let poll_interval = self.poll_interval.map_or(DEFAULT_POLL_INTERVAL, |d| d.0);
        if poll_interval.is_zero() {
            return Err(invalid("imap.poll_interval", "must be more than 0s"));
        }
        Ok(Imap {
            server,
            user: self.user,
            password: self.password,
            mailbox,
            idle: self.idle.unwrap_or(true),
            idle_timeout,
            poll_interval,
        })
    }
}

impl RawSmtp {
    fn validate(self) -> Result<Smtp> {
        let server = server(
            "smtp",
            self.host,
            self.port,
            self.tls.unwrap_or(Tls::Starttls),
            [465, 587, 25],
        )?;
        let credentials = match (self.user, self.password) {
            (Some(user), Some(password)) if !user.is_empty() => Some((user, password)),
            (None, None) => None,
            (Some(_), Some(_)) => return Err(invalid("smtp.user", "must not be empty")),
            (Some(_), None) => return Err(invalid("smtp.password", "is required with smtp.user")),
            (None, Some(_)) => return Err(invalid("smtp.user", "is required with smtp.password")),
        };
        Ok(Smtp {
            server,
            credentials,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn minimal() -> Value {
        json!({
            "authserv_id": "mx.example.com",
            "address": "Giles@Example.com",
            "imap": { "host": "imap.example.com", "user": "giles", "password": "hunter22" },
            "smtp": { "host": "smtp.example.com", "user": "giles", "password": "hunter22" },
        })
    }

    fn error(value: Value) -> String {
        Config::from_json(&value).unwrap_err().to_string()
    }

    #[test]
    fn defaults_follow_the_tls_mode() {
        let config = Config::from_json(&minimal()).unwrap();
        assert_eq!(config.address, "Giles@example.com");
        assert_eq!(
            config.imap.server,
            Server {
                host: "imap.example.com".into(),
                port: 993,
                tls: Tls::Implicit
            }
        );
        assert_eq!(config.imap.mailbox, "INBOX");
        assert!(config.imap.idle);
        assert_eq!(config.imap.idle_timeout, DEFAULT_IDLE_TIMEOUT);
        assert_eq!(config.smtp.server.port, 587);
        assert_eq!(config.smtp.server.tls, Tls::Starttls);
        assert_eq!(config.max_message_size, 50 << 20);

        let mut value = minimal();
        value["smtp"]["tls"] = json!("implicit");
        value["imap"]["tls"] = json!("starttls");
        let config = Config::from_json(&value).unwrap();
        assert_eq!(
            (config.imap.server.port, config.smtp.server.port),
            (143, 465)
        );
    }

    #[test]
    fn passwords_never_show_in_debug_output() {
        let config = Config::from_json(&minimal()).unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter22"), "{debug}");
        assert!(debug.contains("<redacted>"));
    }

    /// Validation errors name the key; serde's own errors get their key path from the SDK's
    /// `parse_config`, so here they only need to fail.
    #[test]
    fn errors_name_the_key() {
        type Change = fn(&mut Value);
        let cases: Vec<(Change, &str)> = vec![
            (
                |v| {
                    v.as_object_mut()
                        .unwrap()
                        .remove("authserv_id")
                        .map(drop)
                        .unwrap()
                },
                "missing field `authserv_id`",
            ),
            (
                |v| v["authserv_id"] = json!(""),
                "invalid config: authserv_id:",
            ),
            (
                |v| v["address"] = json!("not an address"),
                "invalid config: address:",
            ),
            (
                |v| v["imap"]["port"] = json!(70000),
                "invalid value: integer `70000`",
            ),
            (|v| v["imap"]["tls"] = json!("ssl"), "unknown variant `ssl`"),
            (
                |v| v["imap"]["tls"] = json!("none"),
                "invalid config: imap.tls: \"none\"",
            ),
            (
                |v| v["imap"]["idle_timeout"] = json!("30m"),
                "invalid config: imap.idle_timeout:",
            ),
            (
                |v| v["imap"]["poll_interval"] = json!("0s"),
                "invalid config: imap.poll_interval:",
            ),
            (
                |v| v["imap"]["mailbox"] = json!(""),
                "invalid config: imap.mailbox:",
            ),
            (|v| v["imap"]["colour"] = json!(1), "unknown field `colour`"),
            (
                |v| v["smtp"]["user"] = json!(null),
                "invalid config: smtp.user: is required",
            ),
            (
                |v| v["smtp"]["host"] = json!(" "),
                "invalid config: smtp.host:",
            ),
            (
                |v| v["max_message_size"] = json!("50MB"),
                "invalid size \"50MB\"",
            ),
            (
                |v| v["name"] = json!("Giles\r\nBcc: x@y"),
                "invalid config: name:",
            ),
        ];
        for (change, expected) in cases {
            let mut value = minimal();
            change(&mut value);
            let message = error(value);
            assert!(message.contains(expected), "{message:?} lacks {expected:?}");
            assert!(!message.contains("hunter22"), "{message}");
        }
    }

    #[test]
    fn plain_text_is_only_for_this_machine() {
        let mut value = minimal();
        value["imap"]["host"] = json!("127.0.0.1");
        value["imap"]["tls"] = json!("none");
        value["smtp"]["host"] = json!("localhost");
        value["smtp"]["tls"] = json!("none");
        let config = Config::from_json(&value).unwrap();
        assert_eq!(
            (config.imap.server.port, config.smtp.server.port),
            (143, 25)
        );
    }

    #[test]
    fn smtp_without_credentials_is_allowed() {
        let mut value = minimal();
        value["smtp"] = json!({ "host": "smtp.example.com" });
        assert!(
            Config::from_json(&value)
                .unwrap()
                .smtp
                .credentials
                .is_none()
        );
    }
}
