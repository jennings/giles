//! Durations (`"30s"`, `"10m"`, `"1h30m"`) and byte sizes (`52428800`, `"50 MiB"`), written the
//! way `giles.toml` writes them, so a plugin's `config` table reads like the core's own.
//!
//! ```
//! use giles_plugin_sdk::config::{ByteSize, HumanDuration};
//!
//! #[derive(serde::Deserialize)]
//! struct Config {
//!     poll_interval: HumanDuration,
//!     max_size: ByteSize,
//! }
//!
//! let config: Config =
//!     serde_json::from_value(serde_json::json!({"poll_interval": "1m30s", "max_size": "5 MiB"}))
//!         .unwrap();
//! assert_eq!(config.poll_interval.0.as_secs(), 90);
//! assert_eq!(config.max_size.0, 5 << 20);
//! ```

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

/// Duration units, longest suffix first so `ms` is not read as `m`.
const DURATION_UNITS: [(&str, u64); 5] = [
    ("ms", 1),
    ("s", 1_000),
    ("m", 60_000),
    ("h", 3_600_000),
    ("d", 86_400_000),
];

/// Parses a duration: one or more `<integer><unit>` parts with no spaces, units `ms`, `s`, `m`,
/// `h`, and `d`, added together: `"30s"`, `"10m"`, `"1h30m"`, `"500ms"`, `"0s"`.
///
/// # Errors
///
/// A reason naming the expected format.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let invalid = || {
        format!(
            "invalid duration {text:?}: expected an integer and a unit (ms, s, m, h, d), \
             e.g. \"30s\", \"10m\", or \"1h30m\""
        )
    };
    if text.is_empty() {
        return Err(invalid());
    }
    let mut rest = text;
    let mut millis: u64 = 0;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(invalid());
        }
        let number: u64 = rest[..digits].parse().map_err(|_| invalid())?;
        rest = &rest[digits..];
        let (unit, scale) = DURATION_UNITS
            .iter()
            .find(|(unit, _)| rest.starts_with(unit))
            .ok_or_else(invalid)?;
        rest = &rest[unit.len()..];
        millis = number
            .checked_mul(*scale)
            .and_then(|part| millis.checked_add(part))
            .ok_or_else(|| format!("duration {text:?} is too long"))?;
    }
    Ok(Duration::from_millis(millis))
}

/// A duration in config, written as a string ([`parse_duration`]). The field is the duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_duration(&text).map(Self).map_err(de::Error::custom)
    }
}

/// Byte size units, longest suffix first so `KiB` is not read as `B`.
const SIZE_UNITS: [(&str, u64); 4] = [
    ("KiB", 1 << 10),
    ("MiB", 1 << 20),
    ("GiB", 1 << 30),
    ("B", 1),
];

/// Parses a byte size written as a string: an integer and a unit (`B`, `KiB`, `MiB`, `GiB`),
/// optionally separated by a space: `"50 MiB"`, `"512KiB"`.
///
/// # Errors
///
/// A reason naming the expected format.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let invalid = || {
        format!(
            "invalid size {text:?}: expected a number of bytes or an integer and a unit \
             (B, KiB, MiB, GiB), e.g. \"50 MiB\""
        )
    };
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return Err(invalid());
    }
    let number: u64 = text[..digits].parse().map_err(|_| invalid())?;
    let unit = text[digits..].strip_prefix(' ').unwrap_or(&text[digits..]);
    let (_, scale) = SIZE_UNITS
        .iter()
        .find(|(name, _)| *name == unit)
        .ok_or_else(invalid)?;
    number
        .checked_mul(*scale)
        .ok_or_else(|| format!("size {text:?} is too large"))
}

/// A byte size in config: an integer number of bytes or a string ([`parse_size`]). The field is
/// the number of bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SizeVisitor;

        impl Visitor<'_> for SizeVisitor {
            type Value = ByteSize;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number of bytes or a size such as \"50 MiB\"")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<ByteSize, E> {
                u64::try_from(v)
                    .map(ByteSize)
                    .map_err(|_| E::custom("a size can't be negative"))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(v))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<ByteSize, E> {
                parse_size(v).map(ByteSize).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(SizeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        let cases: [(&str, u64); 9] = [
            ("0s", 0),
            ("500ms", 500),
            ("30s", 30_000),
            ("10m", 600_000),
            ("1h", 3_600_000),
            ("2d", 172_800_000),
            ("1h30m", 5_400_000),
            ("1m1ms", 60_001),
            ("90m", 5_400_000),
        ];
        for (text, millis) in cases {
            assert_eq!(
                parse_duration(text),
                Ok(Duration::from_millis(millis)),
                "{text}"
            );
        }
    }

    #[test]
    fn invalid_durations() {
        for text in [
            "", "30", "s", "10 m", " 10m", "10m ", "1.5h", "-1s", "10x", "1hh", "1H", "10min",
        ] {
            let err = parse_duration(text).unwrap_err();
            assert!(err.contains("invalid duration"), "{text:?}: {err}");
        }
        let err = parse_duration("99999999999999999999d").unwrap_err();
        assert!(err.contains("invalid duration"), "{err}");
        let err = parse_duration("999999999999999d").unwrap_err();
        assert!(err.contains("too long"), "{err}");
    }

    #[test]
    fn sizes() {
        let cases: [(&str, u64); 6] = [
            ("0B", 0),
            ("1 B", 1),
            ("512KiB", 512 << 10),
            ("50 MiB", 50 << 20),
            ("2GiB", 2 << 30),
            ("7 KiB", 7 << 10),
        ];
        for (text, bytes) in cases {
            assert_eq!(parse_size(text), Ok(bytes), "{text}");
        }
        for text in [
            "", "50", "MiB", "50 mib", "50MB", "50  MiB", "-1B", "1.5MiB",
        ] {
            let err = parse_size(text).unwrap_err();
            assert!(err.contains("invalid size"), "{text:?}: {err}");
        }
        let err = parse_size("99999999999999 GiB").unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[derive(Debug, Deserialize)]
    struct Doc {
        d: Option<HumanDuration>,
        s: Option<ByteSize>,
    }

    #[test]
    fn deserialize_from_toml() {
        let doc: Doc = toml::from_str("d = \"1h\"\ns = 1024").unwrap();
        assert_eq!(doc.d, Some(HumanDuration(Duration::from_secs(3600))));
        assert_eq!(doc.s, Some(ByteSize(1024)));
        let doc: Doc = toml::from_str("s = \"1 KiB\"").unwrap();
        assert_eq!(doc.s, Some(ByteSize(1024)));
        let err = toml::from_str::<Doc>("s = -1").unwrap_err();
        assert!(err.message().contains("negative"), "{err}");
        let err = toml::from_str::<Doc>("d = 30").unwrap_err();
        assert!(err.message().contains("expected a string"), "{err}");
        let err = toml::from_str::<Doc>("d = \"30\"").unwrap_err();
        assert!(err.message().contains("invalid duration"), "{err}");
    }
}
