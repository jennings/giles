//! The kinds of component instance.

use std::fmt;

/// A component kind: the `<kind>` in `[<kind>.<instance>]` and in `giles-<kind>-<component>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A model provider.
    Provider,
    /// A chat channel.
    Channel,
    /// A toolset (an MCP server).
    Toolset,
    /// An environment: isolated workspaces for an agent's workspace tools.
    Environment,
}

impl Kind {
    /// Every kind.
    pub const ALL: [Kind; 4] = [
        Kind::Provider,
        Kind::Channel,
        Kind::Toolset,
        Kind::Environment,
    ];

    /// The kind as written in config and plugin binary names.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Provider => "provider",
            Kind::Channel => "channel",
            Kind::Toolset => "toolset",
            Kind::Environment => "environment",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
