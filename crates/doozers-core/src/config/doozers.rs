//! `doozers.toml`: `[core]`, the instance tables, and the `[owner]` and `[[routes]]` that are only
//! imported (PLAN sections 1.1, 6.1, 11.2).

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use doozers_store::JournalMode;
use jiff::tz::TimeZone;
use serde::Deserialize;

use super::error::{ConfigError, KeyPath};
use super::instances::{RawChannel, RawEnvironment, RawProvider};
use crate::paths::DataRoot;

/// The HTTP listener when `[core]` sets no `http_listen`: `127.0.0.1:8080`.
pub const DEFAULT_HTTP_LISTEN: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);

/// `[core]`: settings of the core itself.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CoreConfig {
    /// The address of the HTTP server (web chat, admin pages, login). [`DEFAULT_HTTP_LISTEN`]
    /// unless set.
    pub http_listen: SocketAddr,
    /// The SQLite journal mode of `data/doozers.db`: [`JournalMode::Wal`] unless set. The database
    /// is switched to it at startup, from whichever mode it was in.
    pub journal_mode: JournalMode,
    /// The local secret store's key file: `<root>/secret.key` unless set, and relative to the data
    /// root if set relative.
    pub secret_key_file: PathBuf,
}

/// The secret store's key file for `core.secret_key_file = configured`: relative to the data root,
/// or `<root>/secret.key` if unset.
pub fn resolve_secret_key_file(root: &DataRoot, configured: Option<&Path>) -> PathBuf {
    configured.map_or_else(|| root.secret_key_file(), |path| root.root().join(path))
}

/// An identity, `<namespace>:<native id>` (PLAN section 3.2). The namespace may itself contain
/// `:` (`irc:libera.chat:account:stephen`), so the core matches the whole string against
/// `<namespace reported by the channel>:<native id>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Identity(String);

impl Identity {
    /// Validates `text`: a non-empty namespace, a `:`, and a non-empty native id, with no
    /// whitespace or control characters.
    ///
    /// # Errors
    ///
    /// The reason, if `text` is not valid.
    pub fn new(text: impl Into<String>) -> Result<Self, String> {
        let text = text.into();
        let valid = text
            .split_once(':')
            .is_some_and(|(ns, id)| !ns.is_empty() && !id.is_empty())
            && !text.chars().any(|c| c.is_whitespace() || c.is_control());
        if valid {
            Ok(Self(text))
        } else {
            Err(format!(
                "invalid identity {text:?}: write <namespace>:<native id>, e.g. \
                 \"whatsapp:+15551234567\""
            ))
        }
    }

    /// The identity as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawDoozers {
    #[serde(default)]
    core: RawCore,
    /// `[owner]` as written: the first start imports it into the database, and it is never
    /// read again.
    pub owner: Option<toml::Value>,
    #[serde(default)]
    pub provider: BTreeMap<String, RawProvider>,
    #[serde(default)]
    pub channel: BTreeMap<String, RawChannel>,
    #[serde(default)]
    pub environment: BTreeMap<String, RawEnvironment>,
    /// `[toolset.<name>]` tables as written: one with `url` is ignored whatever else it holds
    /// ([`is_remote_toolset`]), and the others are read as specs while loading.
    #[serde(default)]
    pub toolset: BTreeMap<String, toml::Value>,
    #[serde(default)]
    /// Anything: the web UI manages routes, so these are only imported once, and
    /// then not even parsed.
    pub routes: Vec<toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct RawCore {
    http_listen: Option<SocketAddr>,
    journal_mode: Option<String>,
    secret_key_file: Option<PathBuf>,
}

impl RawDoozers {
    pub(crate) fn core(&self, file: &Path, root: &DataRoot) -> Result<CoreConfig, ConfigError> {
        let journal_mode = match &self.core.journal_mode {
            Some(text) => text.parse::<JournalMode>().map_err(|err| {
                ConfigError::at(
                    file,
                    KeyPath::root().key("core").key("journal_mode"),
                    err.to_string(),
                )
            })?,
            None => JournalMode::default(),
        };
        let secret_key_file = match &self.core.secret_key_file {
            Some(path) if path.as_os_str().is_empty() => {
                return Err(ConfigError::at(
                    file,
                    KeyPath::root().key("core").key("secret_key_file"),
                    "must not be empty",
                ));
            }
            configured => resolve_secret_key_file(root, configured.as_deref()),
        };
        Ok(CoreConfig {
            http_listen: self.core.http_listen.unwrap_or(DEFAULT_HTTP_LISTEN),
            journal_mode,
            secret_key_file,
        })
    }
}

/// Whether a `[toolset.<name>]` table has `url`: a remote toolset, which `doozers.toml` no longer
/// configures. Remote toolsets are stored in the database and managed in the web UI, so loading
/// ignores the table and reports it.
pub(crate) fn is_remote_toolset(table: &toml::Value) -> bool {
    table
        .as_table()
        .is_some_and(|table| table.contains_key("url"))
}

/// Looks up an IANA time zone name, e.g. `America/Los_Angeles`, in the system time zone database
/// or, if the system has none, the copy compiled into Doozers.
pub(crate) fn parse_timezone(name: &str) -> Result<TimeZone, String> {
    TimeZone::get(name).map_err(|_| {
        format!("unknown time zone {name:?}: use an IANA name such as \"America/Los_Angeles\"")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities() {
        for ok in [
            "whatsapp:+15551234567",
            "email:me@example.com",
            "irc:libera.chat:account:stephen",
            "web:owner",
        ] {
            assert_eq!(Identity::new(ok).unwrap().as_str(), ok);
        }
        for bad in ["", "whatsapp", ":x", "x:", "a b:c", "a:b c", "a:\tb"] {
            let err = Identity::new(bad).unwrap_err();
            assert!(err.contains("<namespace>:<native id>"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn timezones() {
        assert!(parse_timezone("America/Los_Angeles").is_ok());
        assert!(parse_timezone("UTC").is_ok());
        let err = parse_timezone("Mars/Olympus_Mons").unwrap_err();
        assert!(err.contains("unknown time zone"), "{err}");
    }
}
