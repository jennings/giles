//! `giles.toml`: `[core]`, `[owner]`, the instance tables, and `[[routes]]` (PLAN sections 1.1,
//! 6.1, 11.2).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use giles_types::{AgentId, ConversationKind};
use jiff::tz::TimeZone;
use serde::Deserialize;

use super::error::{ConfigError, KeyPath};
use super::instances::{RawChannel, RawProvider, RawToolset};
use super::names::{InstanceName, invalid_name_reason, is_valid_name};
use crate::routes::Route;

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
    /// Names of instances that were removed on purpose and whose data stays. The startup check
    /// for a rename done by hand ignores them (PLAN section 7.2).
    pub retired_instances: Vec<InstanceName>,
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
#[serde(deny_unknown_fields)]
pub(crate) struct RawGiles {
    #[serde(default)]
    core: RawCore,
    owner: Option<RawOwner>,
    #[serde(default)]
    pub provider: BTreeMap<String, RawProvider>,
    #[serde(default)]
    pub channel: BTreeMap<String, RawChannel>,
    #[serde(default)]
    pub toolset: BTreeMap<String, RawToolset>,
    #[serde(default)]
    routes: Vec<RawRoute>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCore {
    http_listen: Option<SocketAddr>,
    #[serde(default)]
    retired_instances: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOwner {
    identities: Vec<String>,
    default: Option<String>,
    timezone: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoute {
    channel: Option<String>,
    kind: Option<ConversationKind>,
    conversation: Option<String>,
    agent: String,
}

impl RawGiles {
    pub(crate) fn core(&self, file: &Path) -> Result<CoreConfig, ConfigError> {
        let path = KeyPath::root().key("core").key("retired_instances");
        let mut retired: Vec<InstanceName> = Vec::new();
        for (i, text) in self.core.retired_instances.iter().enumerate() {
            let err = |reason: String| ConfigError::at(file, path.index(i), reason);
            let name = InstanceName::new(text.as_str()).map_err(err)?;
            if retired.contains(&name) {
                return Err(err(format!("{text:?} is listed twice")));
            }
            let configured = self.provider.contains_key(name.as_str())
                || self.channel.contains_key(name.as_str())
                || self.toolset.contains_key(name.as_str());
            if configured {
                return Err(err(format!(
                    "{text:?} is configured, so it is not retired: remove it from `retired_instances`"
                )));
            }
            retired.push(name);
        }
        Ok(CoreConfig {
            http_listen: self.core.http_listen.unwrap_or(DEFAULT_HTTP_LISTEN),
            retired_instances: retired,
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

    /// The routes, checked against the configured channels and agents.
    pub(crate) fn routes(
        &self,
        file: &Path,
        channels: &BTreeSet<&str>,
        agents: &BTreeSet<&str>,
    ) -> Result<Vec<Route>, ConfigError> {
        self.routes
            .iter()
            .enumerate()
            .map(|(i, raw)| {
                let path = KeyPath::root().key("routes").index(i);
                let err = |key: &str, reason: String| ConfigError::at(file, path.key(key), reason);
                let channel = match &raw.channel {
                    Some(name) => {
                        let name =
                            InstanceName::new(name.as_str()).map_err(|r| err("channel", r))?;
                        if !channels.contains(name.as_str()) {
                            return Err(err(
                                "channel",
                                format!("no channel instance {name:?} (`[channel.{name}]`)"),
                            ));
                        }
                        Some(name)
                    }
                    None => None,
                };
                if raw.conversation.as_deref() == Some("") {
                    return Err(err("conversation", "must not be empty".into()));
                }
                if !is_valid_name(&raw.agent) {
                    return Err(err("agent", invalid_name_reason("agent", &raw.agent)));
                }
                if !agents.contains(raw.agent.as_str()) {
                    return Err(err(
                        "agent",
                        format!("no agent {:?} (agents/{0}/agent.toml)", raw.agent),
                    ));
                }
                let agent =
                    AgentId::new(raw.agent.as_str()).map_err(|e| err("agent", e.to_string()))?;
                Ok(Route {
                    channel,
                    kind: raw.kind,
                    conversation: raw.conversation.clone(),
                    agent,
                })
            })
            .collect()
    }
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
