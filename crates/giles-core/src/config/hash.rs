//! The instance config hash, part of the tool discovery cache key (PLAN section 7.9).
//!
//! It is taken over the instance's table exactly as written, before secret references are
//! resolved, so it never depends on (or reveals) a secret value. It is stable across everything
//! TOML treats as the same value: key order, inline versus standard tables, dotted keys, string
//! quoting, whitespace, comments, and number spelling (`0x10` is `16`).
//!
//! Algorithm, version 1: SHA-256 of the domain tag `giles-config-hash-v1\n` followed by the
//! canonical encoding of the table:
//!
//! | Value    | Encoding                                                                 |
//! | -------- | ------------------------------------------------------------------------ |
//! | string   | `s`, byte length as u64 little-endian, UTF-8 bytes                       |
//! | integer  | `i`, i64 little-endian                                                   |
//! | float    | `f`, IEEE 754 bits as u64 little-endian; `-0.0` as `0.0`, every NaN as `0x7ff8000000000000` |
//! | boolean  | `b`, one byte 0 or 1                                                     |
//! | datetime | `d`, then its TOML text encoded as a string                              |
//! | array    | `a`, item count as u64 little-endian, each item                          |
//! | table    | `t`, entry count as u64 little-endian, then for each entry in byte order of keys: the key as a string, the value |

use std::fmt;

use sha2::{Digest, Sha256};

/// The domain tag hashed before the encoding; a new version of the algorithm gets a new tag.
const DOMAIN: &[u8] = b"giles-config-hash-v1\n";

/// The SHA-256 hash of an instance's table as written ([module docs](self)). Displays as 64
/// lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConfigHash([u8; 32]);

impl ConfigHash {
    /// Hashes `table` with version 1 of the algorithm.
    pub fn of(table: &toml::Table) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(DOMAIN);
        encode_table(&mut hasher, table);
        Self(hasher.finalize().into())
    }

    /// The digest.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ConfigHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for ConfigHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ConfigHash({self})")
    }
}

fn encode_len(hasher: &mut Sha256, len: usize) {
    hasher.update((len as u64).to_le_bytes());
}

fn encode_str(hasher: &mut Sha256, s: &str) {
    hasher.update(b"s");
    encode_len(hasher, s.len());
    hasher.update(s.as_bytes());
}

fn encode_table(hasher: &mut Sha256, table: &toml::Table) {
    // Sort explicitly: `toml::Table` iterates in insertion order if any crate in the build enables
    // toml's `preserve_order` feature.
    let mut entries: Vec<_> = table.iter().collect();
    entries.sort_unstable_by(|(a, _), (b, _)| a.as_bytes().cmp(b.as_bytes()));
    hasher.update(b"t");
    encode_len(hasher, entries.len());
    for (key, value) in entries {
        encode_str(hasher, key);
        encode(hasher, value);
    }
}

fn encode(hasher: &mut Sha256, value: &toml::Value) {
    match value {
        toml::Value::String(s) => encode_str(hasher, s),
        toml::Value::Integer(i) => {
            hasher.update(b"i");
            hasher.update(i.to_le_bytes());
        }
        toml::Value::Float(f) => {
            let bits = if f.is_nan() {
                0x7ff8_0000_0000_0000
            } else if *f == 0.0 {
                0
            } else {
                f.to_bits()
            };
            hasher.update(b"f");
            hasher.update(u64::to_le_bytes(bits));
        }
        toml::Value::Boolean(b) => hasher.update([b'b', u8::from(*b)]),
        toml::Value::Datetime(d) => {
            hasher.update(b"d");
            encode_str(hasher, &d.to_string());
        }
        toml::Value::Array(items) => {
            hasher.update(b"a");
            encode_len(hasher, items.len());
            for item in items {
                encode(hasher, item);
            }
        }
        toml::Value::Table(table) => encode_table(hasher, table),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(src: &str) -> ConfigHash {
        ConfigHash::of(&toml::from_str(src).unwrap())
    }

    #[test]
    fn pinned_values() {
        // Computed independently from the documented encoding. Changing these means changing the
        // algorithm: give it a new domain tag and say so in the docs, since every cached tool list
        // is refreshed.
        assert_eq!(
            hash("").to_string(),
            "91d596437c61e4c07cb168d7bfe78f695f877833b56558513e64892c588048ec"
        );
        assert_eq!(
            hash("command = \"/bin/x\"\nargs = [\"-v\", 1, true, 1.5]").to_string(),
            "3f36b1522dc6f5ec44308f3297e9fbd65a34cb6217d6d366f382e305f438e2ad"
        );
    }

    #[test]
    fn same_value_written_differently_hashes_the_same() {
        let groups: [&[&str]; 6] = [
            &[
                "a = 1\nb = \"x\"",
                "b = \"x\"\na = 1",
                "b='x'\n# comment\n   a   =   1   ",
                "\"a\" = 1\n'b' = \"\"\"x\"\"\"",
            ],
            &[
                "sandbox = { read = [\"/a\"], network = true }",
                "[sandbox]\nnetwork = true\nread = [\"/a\"]",
                "sandbox.network = true\nsandbox.read = [ \"/a\", ]",
            ],
            &[
                "n = 16",
                "n = 0x10",
                "n = 0o20",
                "n = 0b10000",
                "n = +16",
                "n = 1_6",
            ],
            &["f = 1.0", "f = 1e0", "f = 10e-1", "f = 1.000"],
            &["z = 0.0", "z = -0.0", "z = +0.0"],
            &["x = nan", "x = -nan", "x = +nan"],
        ];
        for group in groups {
            let first = hash(group[0]);
            for src in &group[1..] {
                assert_eq!(hash(src), first, "{:?} vs {src:?}", group[0]);
            }
        }
    }

    #[test]
    fn any_change_changes_the_hash() {
        let base =
            "command = \"/bin/x\"\nargs = [\"a\", \"b\"]\nconfig = { key = { secret = \"k\" } }";
        let variants = [
            "command = \"/bin/y\"\nargs = [\"a\", \"b\"]\nconfig = { key = { secret = \"k\" } }",
            "command = \"/bin/x\"\nargs = [\"b\", \"a\"]\nconfig = { key = { secret = \"k\" } }",
            "command = \"/bin/x\"\nargs = [\"a\", \"b\", \"c\"]\nconfig = { key = { secret = \"k\" } }",
            "command = \"/bin/x\"\nargs = [\"ab\"]\nconfig = { key = { secret = \"k\" } }",
            "command = \"/bin/x\"\nargs = [\"a\", \"b\"]\nconfig = { key = { secret = \"j\" } }",
            "command = \"/bin/x\"\nargs = [\"a\", \"b\"]\nconfig = { key = { env = \"k\" } }",
            "command = \"/bin/x\"\nargs = [\"a\", \"b\"]\nconfig = { key = \"k\" }",
            "command = \"/bin/x\"\nargs = [\"a\", \"b\"]\nconfig = { key = { secret = \"k\" }, x = 1 }",
            "command = \"/bin/x\"\nargs = [\"a\", \"b\"]",
            "command = \"/bin/x\"\nargs = [\"a\", \"b\"]\nconfig = { key = { secret = \"k\" } }\nparallel = false",
        ];
        let base_hash = hash(base);
        let mut seen = vec![base_hash];
        for src in variants {
            let h = hash(src);
            assert!(!seen.contains(&h), "{src:?} collides");
            seen.push(h);
        }
        // Types are part of the value.
        assert_ne!(hash("v = 1"), hash("v = \"1\""));
        assert_ne!(hash("v = 1"), hash("v = 1.0"));
        assert_ne!(hash("v = true"), hash("v = 1"));
        assert_ne!(hash("v = []"), hash("v = {}"));
        assert_ne!(hash("v = 1979-05-27"), hash("v = \"1979-05-27\""));
        // Lengths are encoded, so concatenations don't collide.
        assert_ne!(hash("v = [\"ab\", \"c\"]"), hash("v = [\"a\", \"bc\"]"));
        assert_ne!(hash("ab = \"c\""), hash("a = \"bc\""));
    }

    #[test]
    fn display_is_lowercase_hex() {
        let h = hash("a = 1");
        let text = h.to_string();
        assert_eq!(text.len(), 64);
        assert!(
            text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{text}"
        );
        assert_eq!(format!("{h:?}"), format!("ConfigHash({text})"));
        let bytes: String = h.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(bytes, text);
    }
}
