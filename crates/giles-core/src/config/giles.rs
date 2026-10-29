//! `giles.toml`: `[core]`, `[owner]`, the instance tables, and `[[routes]]` (PLAN sections 1.1,
//! 6.1, 11.2).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use giles_store::JournalMode;
use jiff::tz::TimeZone;
use serde::Deserialize;

use super::error::{ConfigError, KeyPath};
use super::instances::{RawChannel, RawProvider};

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
    /// The SQLite journal mode of `data/giles.db`: [`JournalMode::Wal`] unless set. The database
    /// is switched to it at startup, from whichever mode it was in.
    pub journal_mode: JournalMode,
}

/// `[owner]`: the one user of v1 (PLAN section 1.1).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct OwnerConfig {
    /// The owner's identities, `<namespace>:<native id>`, e.g. `whatsapp:+15551234567`. Messages
    /// from any other identity are dropped.
    pub identities: Vec<Identity>,
    /// The identity jobs and proactive messages go to; one of `identities`.
    pub default: Identity,
    /// The owner's time zone, for message time prefixes and job schedules. `None`: the agent's
    /// `timezone`, else the system time zone.
    pub timezone: Option<TimeZone>,
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
pub(crate) struct RawGiles {
    #[serde(default)]
    core: RawCore,
    owner: Option<RawOwner>,
    #[serde(default)]
    pub provider: BTreeMap<String, RawProvider>,
    #[serde(default)]
    pub channel: BTreeMap<String, RawChannel>,
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
}

#[derive(Debug, Deserialize)]
struct RawOwner {
    identities: Vec<String>,
    default: Option<String>,
    timezone: Option<String>,
}

impl RawGiles {
    pub(crate) fn core(&self, file: &Path) -> Result<CoreConfig, ConfigError> {
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
        Ok(CoreConfig {
            http_listen: self.core.http_listen.unwrap_or(DEFAULT_HTTP_LISTEN),
            journal_mode,
        })
    }

    pub(crate) fn owner(&self, file: &Path) -> Result<OwnerConfig, ConfigError> {
        let path = KeyPath::root().key("owner");
        let err = |path: KeyPath, reason: String| ConfigError::at(file, path, reason);
        let Some(raw) = &self.owner else {
            return Err(err(
                path,
                "missing: declare the owner's identities, e.g. \
                 `[owner] identities = [\"whatsapp:+15551234567\"]`"
                    .to_owned(),
            ));
        };
        if raw.identities.is_empty() {
            return Err(err(
                path.key("identities"),
                "must list at least one identity".into(),
            ));
        }
        let mut identities = Vec::with_capacity(raw.identities.len());
        let mut seen = BTreeSet::new();
        for (i, text) in raw.identities.iter().enumerate() {
            let at = path.key("identities").index(i);
            let identity = Identity::new(text.as_str()).map_err(|r| err(at.clone(), r))?;
            if !seen.insert(identity.clone()) {
                return Err(err(at, format!("{identity} is listed twice")));
            }
            identities.push(identity);
        }
        let default = match &raw.default {
            Some(text) => {
                let at = path.key("default");
                let identity = Identity::new(text.as_str()).map_err(|r| err(at.clone(), r))?;
                if !seen.contains(&identity) {
                    return Err(err(at, format!("{identity} is not one of `identities`")));
                }
                identity
            }
            None if identities.len() == 1 => identities[0].clone(),
            None => {
                return Err(err(
                    path.key("default"),
                    "missing: with several identities, name the one jobs and proactive \
                     messages go to"
                        .into(),
                ));
            }
        };
        let timezone = raw
            .timezone
            .as_deref()
            .map(|name| parse_timezone(name).map_err(|r| err(path.key("timezone"), r)))
            .transpose()?;
        Ok(OwnerConfig {
            identities,
            default,
            timezone,
        })
    }
}

/// Whether a `[toolset.<name>]` table has `url`: a remote toolset, which `giles.toml` no longer
/// configures. Remote toolsets are stored in the database and managed in the web UI, so loading
/// ignores the table and reports it.
pub(crate) fn is_remote_toolset(table: &toml::Value) -> bool {
    table
        .as_table()
        .is_some_and(|table| table.contains_key("url"))
}

/// Looks up an IANA time zone name, e.g. `America/Los_Angeles`, in the system time zone database
/// or, if the system has none, the copy compiled into Giles.
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
