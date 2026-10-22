//! Names: instance, component, agent, and job names, and how a component name resolves to a
//! built-in component or a plugin binary (PLAN sections 6.5, 7.1, 7.3, 11.2).

use std::borrow::Borrow;
use std::fmt;

use serde::Deserialize;

/// Whether `name` is a valid instance, component, agent, or job name: one or more of
/// `[a-zA-Z0-9-]`. Such a name is a safe path component and never contains `__`, the tool-name
/// separator (PLAN section 6.5).
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The reason given when a name fails [`is_valid_name`].
pub(crate) fn invalid_name_reason(what: &str, name: &str) -> String {
    format!("invalid {what} name {name:?}: use only letters, digits, and `-`")
}

/// The name of a configured component instance: the `<instance>` in `[<kind>.<instance>]`.
///
/// Instance names are permanent ids (PLAN section 7.2): sessions, the inbox, tool prefixes, and
/// the instance's data directory are keyed by them. They are unique across all kinds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct InstanceName(String);

impl InstanceName {
    /// Validates `name` ([`is_valid_name`]).
    ///
    /// # Errors
    ///
    /// The reason, if `name` is not valid.
    pub fn new(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        if is_valid_name(&name) {
            Ok(Self(name))
        } else {
            Err(invalid_name_reason("instance", &name))
        }
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for InstanceName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl fmt::Display for InstanceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for InstanceName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for InstanceName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// A component kind: the `<kind>` in `[<kind>.<instance>]` and in `giles-<kind>-<component>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Kind {
    /// A model provider.
    Provider,
    /// A chat channel.
    Channel,
    /// A toolset (an MCP server).
    Toolset,
}

impl Kind {
    /// The kind as written in config and plugin binary names.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Provider => "provider",
            Kind::Channel => "channel",
            Kind::Toolset => "toolset",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The names of the components compiled into the core, per kind. A configured `component` with
/// one of these names is that built-in; any other name is the plugin `giles-<kind>-<component>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtins {
    /// Built-in providers.
    pub providers: &'static [&'static str],
    /// Built-in channels.
    pub channels: &'static [&'static str],
}

impl Builtins {
    /// Every built-in component v1 has (PLAN section 4.2): the `anthropic` and `openai`
    /// providers and the `web` channel. There are no built-in toolsets.
    pub const V1: Builtins = Builtins {
        providers: &["anthropic", "openai"],
        channels: &["web"],
    };

    /// The built-in names for `kind`.
    pub fn names(&self, kind: Kind) -> &'static [&'static str] {
        match kind {
            Kind::Provider => self.providers,
            Kind::Channel => self.channels,
            Kind::Toolset => &[],
        }
    }

    /// Resolves `component` (already validated with [`is_valid_name`]) for `kind`.
    pub fn resolve(&self, kind: Kind, component: &str) -> Component {
        if self.names(kind).contains(&component) {
            Component::BuiltIn {
                name: component.to_owned(),
            }
        } else {
            Component::Plugin {
                name: component.to_owned(),
                binary: plugin_binary(kind, component),
            }
        }
    }
}

impl Default for Builtins {
    fn default() -> Self {
        Self::V1
    }
}

/// The plugin binary name for a component: `giles-<kind>-<component>` (PLAN section 7.1).
pub fn plugin_binary(kind: Kind, component: &str) -> String {
    format!("giles-{kind}-{component}")
}

/// What an instance's `component` resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Component {
    /// A component compiled into the core.
    BuiltIn {
        /// The component name.
        name: String,
    },
    /// A plugin binary, found in `<root>/plugins/` or on `$PATH` (PLAN section 7.3).
    Plugin {
        /// The component name, e.g. `irc`.
        name: String,
        /// The binary name, e.g. `giles-channel-irc`.
        binary: String,
    },
}

impl Component {
    /// The component name, e.g. `irc` for `giles-channel-irc`.
    pub fn name(&self) -> &str {
        match self {
            Component::BuiltIn { name } | Component::Plugin { name, .. } => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for valid in ["a", "irc-libera", "A9", "-", "x-y-z", "github"] {
            assert!(is_valid_name(valid), "{valid}");
            assert!(InstanceName::new(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "", "a_b", "a__b", "a.b", "a b", "a/b", "é", "..", "a\0", "a:b",
        ] {
            assert!(!is_valid_name(invalid), "{invalid:?}");
            let err = InstanceName::new(invalid).unwrap_err();
            assert!(err.contains("letters, digits, and `-`"), "{err}");
        }
    }

    #[test]
    fn components_resolve_to_builtins_else_plugins() {
        let b = Builtins::V1;
        assert_eq!(
            b.resolve(Kind::Provider, "anthropic"),
            Component::BuiltIn {
                name: "anthropic".into()
            }
        );
        assert_eq!(
            b.resolve(Kind::Channel, "web"),
            Component::BuiltIn { name: "web".into() }
        );
        assert_eq!(
            b.resolve(Kind::Channel, "irc"),
            Component::Plugin {
                name: "irc".into(),
                binary: "giles-channel-irc".into()
            }
        );
        // A built-in of one kind is a plugin name for another.
        assert_eq!(b.resolve(Kind::Channel, "anthropic").name(), "anthropic");
        assert!(matches!(
            b.resolve(Kind::Toolset, "web"),
            Component::Plugin { binary, .. } if binary == "giles-toolset-web"
        ));
        let custom = Builtins {
            providers: &[],
            channels: &["irc"],
        };
        assert!(matches!(
            custom.resolve(Kind::Provider, "anthropic"),
            Component::Plugin { binary, .. } if binary == "giles-provider-anthropic"
        ));
        assert!(matches!(
            custom.resolve(Kind::Channel, "irc"),
            Component::BuiltIn { .. }
        ));
    }
}
