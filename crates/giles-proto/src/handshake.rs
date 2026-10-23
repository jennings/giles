//! The launch handshake between the core and a gRPC plugin (PLAN section 7.4).
//!
//! These are the values both sides must agree on before any gRPC call: the environment the core
//! sets, the descriptor the plugin serves on, and the one line the plugin prints on stdout once it
//! serves. The core launches the plugin with them; `giles-plugin-sdk` checks them on the plugin
//! side. See `Docs/technical/protocols.md` for the launch sequence.

use std::fmt;

/// Environment variable carrying [`MAGIC_COOKIE`]. Run by hand without it, a plugin says that it's
/// a Giles plugin and exits. A usability check, not security.
pub const MAGIC_COOKIE_ENV: &str = "GILES_PLUGIN_MAGIC";

/// The fixed value of [`MAGIC_COOKIE_ENV`]. It never changes: protocol versions are negotiated
/// separately, through [`PROTOCOLS_ENV`].
pub const MAGIC_COOKIE: &str = "giles-plugin:5b2f0c7e9a1d4836";

/// Environment variable listing the plugin protocol majors the core speaks, comma-separated in any
/// order, e.g. `1` or `1,2`. The plugin picks the highest one it shares and names it in the
/// handshake line.
pub const PROTOCOLS_ENV: &str = "GILES_PLUGIN_PROTOCOLS";

/// Environment variable holding the instance's data directory, the plugin's only writable directory
/// (PLAN section 11.6).
pub const DATA_ENV: &str = "GILES_PLUGIN_DATA";

/// The descriptor of the plugin's end of the socket pair.
pub const PLUGIN_FD: i32 = 3;

/// The protocol major of the packages in this crate (`giles.plugin.v1`, `giles.channel.v1`, ...).
pub const PROTOCOL_MAJOR: u32 = 1;

/// The version of the handshake line's own layout, its first field.
pub const HANDSHAKE_VERSION: u32 = 1;

/// The exit status of a plugin that shares no protocol major with the core (`EX_PROTOCOL` from
/// `sysexits.h`). The plugin also explains it on stderr. Relaunching the same binary can't help, so
/// the core doesn't retry until the binary changes (PLAN section 7.4).
pub const EXIT_NO_SHARED_PROTOCOL: i32 = 76;

/// The line a plugin prints on stdout once it serves gRPC on [`PLUGIN_FD`]:
/// `1|<protocol>|fd|3|grpc`, in go-plugin's field layout (handshake version, chosen protocol major,
/// transport, address, wire protocol).
///
/// `Display` writes the line without its line terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandshakeLine {
    /// The protocol major the plugin chose from [`PROTOCOLS_ENV`].
    pub protocol: u32,
}

impl HandshakeLine {
    /// Parses a line the plugin printed. One trailing `\n` or `\r\n` is accepted; anything else
    /// that differs from the layout above is an error.
    pub fn parse(line: &str) -> Result<Self, HandshakeError> {
        let line = line
            .strip_suffix('\n')
            .map_or(line, |l| l.strip_suffix('\r').unwrap_or(l));
        let fields: Vec<&str> = line.split('|').collect();
        let [version, protocol, transport, address, wire] = fields[..] else {
            return Err(HandshakeError::Malformed(line.to_owned()));
        };
        if line.contains(['\n', '\r']) {
            return Err(HandshakeError::Malformed(line.to_owned()));
        }
        match parse_major(version) {
            None => return Err(HandshakeError::Malformed(line.to_owned())),
            Some(HANDSHAKE_VERSION) => {}
            Some(_) => return Err(HandshakeError::Version(version.to_owned())),
        }
        let protocol =
            parse_major(protocol).ok_or_else(|| HandshakeError::Malformed(line.to_owned()))?;
        if transport != "fd" || address != PLUGIN_FD.to_string() || wire != "grpc" {
            return Err(HandshakeError::Transport(format!(
                "{transport}|{address}|{wire}"
            )));
        }
        Ok(Self { protocol })
    }
}

impl fmt::Display for HandshakeLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{HANDSHAKE_VERSION}|{}|fd|{PLUGIN_FD}|grpc",
            self.protocol
        )
    }
}

/// A protocol major as written in [`PROTOCOLS_ENV`] and the handshake line: decimal digits only,
/// no sign or leading zeros.
pub fn parse_major(text: &str) -> Option<u32> {
    let canonical = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    canonical.then(|| text.parse().ok()).flatten()
}

/// Why a handshake line was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    /// Not five `|`-separated fields, or a protocol that isn't a number.
    Malformed(String),
    /// A handshake version other than [`HANDSHAKE_VERSION`].
    Version(String),
    /// A transport other than `fd|3|grpc`.
    Transport(String),
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(line) => write!(f, "malformed handshake line {line:?}"),
            Self::Version(version) => write!(f, "unsupported handshake version {version:?}"),
            Self::Transport(transport) => {
                write!(
                    f,
                    "unsupported transport {transport:?}, expected \"fd|3|grpc\""
                )
            }
        }
    }
}

impl std::error::Error for HandshakeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_round_trips_in_go_plugin_layout() {
        let line = HandshakeLine { protocol: 1 };
        assert_eq!(line.to_string(), "1|1|fd|3|grpc");
        assert_eq!(HandshakeLine::parse("1|1|fd|3|grpc"), Ok(line));
        assert_eq!(HandshakeLine::parse("1|1|fd|3|grpc\n"), Ok(line));
        assert_eq!(HandshakeLine::parse("1|1|fd|3|grpc\r\n"), Ok(line));
        assert_eq!(HandshakeLine::parse("1|12|fd|3|grpc").unwrap().protocol, 12);
    }

    #[test]
    fn deviations_are_refused() {
        for line in [
            "",
            "1|1|fd|3",
            "1|1|fd|3|grpc|x",
            "1|x|fd|3|grpc",
            "1||fd|3|grpc",
            "1|+1|fd|3|grpc",
            "1|01|fd|3|grpc",
            "1|1|fd|3|grpc\n\n",
            " 1|1|fd|3|grpc",
        ] {
            assert!(
                matches!(
                    HandshakeLine::parse(line),
                    Err(HandshakeError::Malformed(_))
                ),
                "{line:?}"
            );
        }
        assert_eq!(
            HandshakeLine::parse("2|1|fd|3|grpc"),
            Err(HandshakeError::Version("2".into()))
        );
        for line in ["1|1|unix|/tmp/s|grpc", "1|1|fd|4|grpc", "1|1|fd|3|netrpc"] {
            assert!(
                matches!(
                    HandshakeLine::parse(line),
                    Err(HandshakeError::Transport(_))
                ),
                "{line:?}"
            );
        }
    }

    #[test]
    fn majors_are_plain_decimal() {
        assert_eq!(parse_major("0"), Some(0));
        assert_eq!(parse_major("7"), Some(7));
        assert_eq!(parse_major("4294967295"), Some(u32::MAX));
        for text in ["", "-1", "+1", "01", " 1", "1 ", "4294967296", "1.0"] {
            assert_eq!(parse_major(text), None, "{text:?}");
        }
    }
}
