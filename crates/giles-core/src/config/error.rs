//! Load errors. Every error names the file, the key path inside it (when the problem is at a key),
//! the line and column (when the TOML parser knows them), and the reason.
//!
//! Reasons never quote a value from `secrets.toml` or a value a secret reference resolved to.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// A path to a key inside a TOML document, e.g. `toolset.files.sandbox.read[0]`.
///
/// Displayed as TOML writes it: bare keys joined by `.`, other keys quoted, array indexes in
/// brackets. The empty path is the document root.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyPath(Vec<Segment>);

/// One step of a [`KeyPath`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Segment {
    /// A table key.
    Key(String),
    /// An array index.
    Index(usize),
}

impl KeyPath {
    /// The document root.
    pub fn root() -> Self {
        Self::default()
    }

    /// This path extended by a table key.
    #[must_use]
    pub fn key(&self, key: impl Into<String>) -> Self {
        let mut path = self.clone();
        path.0.push(Segment::Key(key.into()));
        path
    }

    /// This path extended by an array index.
    #[must_use]
    pub fn index(&self, index: usize) -> Self {
        let mut path = self.clone();
        path.0.push(Segment::Index(index));
        path
    }

    /// Whether this is the document root.
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The segments, outermost first.
    pub fn segments(&self) -> &[Segment] {
        &self.0
    }

    /// Whether `self` is `other` or inside it.
    pub fn starts_with(&self, other: &KeyPath) -> bool {
        self.0.starts_with(&other.0)
    }

    /// This path followed by `tail`.
    #[must_use]
    pub fn join(&self, tail: &KeyPath) -> Self {
        let mut path = self.clone();
        path.0.extend(tail.0.iter().cloned());
        path
    }
}

impl From<&serde_path_to_error::Path> for KeyPath {
    fn from(path: &serde_path_to_error::Path) -> Self {
        use serde_path_to_error::Segment as S;
        Self(
            path.iter()
                .filter_map(|segment| match segment {
                    S::Seq { index } => Some(Segment::Index(*index)),
                    S::Map { key } => Some(Segment::Key(key.clone())),
                    S::Enum { variant } => Some(Segment::Key(variant.clone())),
                    S::Unknown => None,
                })
                .collect(),
        )
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for segment in &self.0 {
            match segment {
                Segment::Key(key) => {
                    if !first {
                        f.write_str(".")?;
                    }
                    if is_bare_key(key) {
                        f.write_str(key)?;
                    } else {
                        write_quoted(f, key)?;
                    }
                }
                Segment::Index(index) => write!(f, "[{index}]")?,
            }
            first = false;
        }
        Ok(())
    }
}

/// Whether TOML can write `key` without quotes.
fn is_bare_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Writes `key` as a TOML basic string.
fn write_quoted(f: &mut fmt::Formatter<'_>, key: &str) -> fmt::Result {
    f.write_str("\"")?;
    for c in key.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            c if c.is_control() => write!(f, "\\u{:04X}", u32::from(c))?,
            c => write!(f, "{c}")?,
        }
    }
    f.write_str("\"")
}

/// A line and column in a file, both counted from 1. The column counts characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// The line, from 1.
    pub line: usize,
    /// The column, from 1, in characters.
    pub column: usize,
}

impl Position {
    /// The position of byte `offset` in `source`. An offset past the end, or inside a character,
    /// is clamped to the last character boundary before it.
    pub fn of_offset(source: &str, offset: usize) -> Self {
        let mut offset = offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset -= 1;
        }
        let before = &source[..offset];
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        Self {
            line: before.matches('\n').count() + 1,
            column: before[line_start..].chars().count() + 1,
        }
    }
}

/// A configuration file that can't be loaded: unreadable, not valid TOML, or not a valid
/// configuration.
///
/// Displayed as `<file>:<line>:<column>: <key path>: <reason>`, leaving out what isn't known.
#[derive(Debug)]
#[non_exhaustive]
pub struct ConfigError {
    /// The file with the problem.
    pub file: PathBuf,
    /// Where in the file, if known.
    pub position: Option<Position>,
    /// The key with the problem; the root if the problem is with the file as a whole.
    pub key: KeyPath,
    /// What is wrong. Never contains a secret value.
    pub reason: String,
    /// The I/O error behind the problem, if any.
    pub source: Option<io::Error>,
}

impl ConfigError {
    /// A problem at `key` in `file`.
    pub(crate) fn at(file: &Path, key: KeyPath, reason: impl Into<String>) -> Self {
        Self {
            file: file.to_path_buf(),
            position: None,
            key,
            reason: reason.into(),
            source: None,
        }
    }

    /// A problem with `file` as a whole.
    pub(crate) fn file(file: &Path, reason: impl Into<String>) -> Self {
        Self::at(file, KeyPath::root(), reason)
    }

    /// `file` could not be read or inspected.
    pub(crate) fn io(file: &Path, action: &str, source: io::Error) -> Self {
        Self {
            source: Some(source),
            ..Self::file(file, format!("cannot {action}"))
        }
    }

    /// A TOML syntax error. Uses only the parser's message and position, never the source line,
    /// so a syntax error in `secrets.toml` doesn't print a secret.
    pub(crate) fn syntax(file: &Path, source: &str, error: &toml::de::Error) -> Self {
        Self {
            position: error.span().map(|s| Position::of_offset(source, s.start)),
            ..Self::file(file, format!("invalid TOML: {}", error.message()))
        }
    }

    /// A deserialization error at the key path serde tracked.
    pub(crate) fn deserialize(
        file: &Path,
        source: &str,
        error: &serde_path_to_error::Error<toml::de::Error>,
    ) -> Self {
        let inner = error.inner();
        Self {
            position: inner.span().map(|s| Position::of_offset(source, s.start)),
            ..Self::at(file, KeyPath::from(error.path()), inner.message())
        }
    }

    /// Fills in the position of the key, for an error found after parsing, if the error is in
    /// `file` (whose contents are `source`) and has no position yet.
    pub(crate) fn located(mut self, file: &Path, source: &str) -> Self {
        if self.position.is_none() && self.file == file && !self.key.is_root() {
            self.position = locate(source, &self.key);
        }
        self
    }
}

/// The position in `source` of the deepest part of `path` it contains: the key of a table entry,
/// or the start of an array item. `None` if `source` doesn't parse or contains no part of `path`.
///
/// Used only on the error path, so `source` is parsed again, keeping spans.
pub(crate) fn locate(source: &str, path: &KeyPath) -> Option<Position> {
    use toml::de::{DeTable, DeValue};

    let root = DeTable::parse(source).ok()?;
    let mut table: Option<&DeTable<'_>> = Some(root.get_ref());
    let mut array: Option<&[toml::Spanned<DeValue<'_>>]> = None;
    let mut found = None;
    for segment in path.segments() {
        let next = match (segment, table, array) {
            (Segment::Key(key), Some(t), _) => t
                .iter()
                .find(|(k, _)| k.get_ref() == key)
                .map(|(k, v)| (k.span().start, v)),
            (Segment::Index(i), _, Some(a)) => {
                let item = a.get(*i);
                item.map(|v| (v.span().start, v))
            }
            _ => None,
        };
        let Some((offset, value)) = next else {
            break;
        };
        found = Some(offset);
        (table, array) = match value.get_ref() {
            DeValue::Table(t) => (Some(t), None),
            DeValue::Array(a) => (None, Some(&a[..])),
            _ => (None, None),
        };
    }
    found.map(|offset| Position::of_offset(source, offset))
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file.display())?;
        if let Some(Position { line, column }) = self.position {
            write!(f, ":{line}:{column}")?;
        }
        if !self.key.is_root() {
            write!(f, ": {}", self.key)?;
        }
        write!(f, ": {}", self.reason)
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_paths_display_as_toml_keys() {
        let path = KeyPath::root()
            .key("toolset")
            .key("files")
            .key("env")
            .key("MY VAR")
            .index(2)
            .key("a\"b");
        assert_eq!(path.to_string(), r#"toolset.files.env."MY VAR"[2]."a\"b""#);
        assert_eq!(KeyPath::root().to_string(), "");
        assert_eq!(KeyPath::root().index(0).key("x").to_string(), "[0].x");
        assert_eq!(KeyPath::root().key("").to_string(), r#""""#);
    }

    #[test]
    fn key_path_prefixes() {
        let config = KeyPath::root().key("provider").key("a").key("config");
        let inner = config.key("x").index(1);
        assert!(inner.starts_with(&config));
        assert!(config.starts_with(&config));
        assert!(!config.starts_with(&inner));
        assert_eq!(config.join(&KeyPath::root().key("x").index(1)), inner);
    }

    #[test]
    fn positions_count_lines_and_characters() {
        let src = "a = 1\nbé = 2\n";
        assert_eq!(Position::of_offset(src, 0), Position { line: 1, column: 1 });
        assert_eq!(Position::of_offset(src, 6), Position { line: 2, column: 1 });
        // "bé" is three bytes; the `=` after "bé " is the fourth character.
        assert_eq!(
            Position::of_offset(src, 10),
            Position { line: 2, column: 4 }
        );
        // Inside the two-byte `é`, and past the end.
        assert_eq!(Position::of_offset(src, 8), Position { line: 2, column: 2 });
        assert_eq!(
            Position::of_offset(src, 99),
            Position { line: 3, column: 1 }
        );
    }

    #[test]
    fn locate_finds_keys_and_items() {
        let src = "a = { b = [1, { c = 2 }] }\n[d]\ne = 3\n";
        let at = |path: KeyPath| locate(src, &path).map(|p| (p.line, p.column));
        let root = KeyPath::root();
        assert_eq!(at(root.key("a")), Some((1, 1)));
        assert_eq!(at(root.key("a").key("b")), Some((1, 7)));
        assert_eq!(at(root.key("a").key("b").index(1)), Some((1, 15)));
        assert_eq!(at(root.key("a").key("b").index(1).key("c")), Some((1, 17)));
        assert_eq!(at(root.key("d").key("e")), Some((3, 1)));
        // Missing parts fall back to the deepest part that's there.
        assert_eq!(at(root.key("d").key("missing")), Some((2, 2)));
        assert_eq!(at(root.key("a").key("b").index(9)), Some((1, 7)));
        assert_eq!(at(root.key("missing")), None);
        assert_eq!(locate("a = ", &root.key("a")), None);
    }

    #[test]
    fn display_leaves_out_unknown_parts() {
        let file = Path::new("/h/giles.toml");
        let at = ConfigError::at(file, KeyPath::root().key("core"), "bad");
        assert_eq!(at.to_string(), "/h/giles.toml: core: bad");
        let whole = ConfigError {
            position: Some(Position { line: 3, column: 7 }),
            ..ConfigError::file(file, "worse")
        };
        assert_eq!(whole.to_string(), "/h/giles.toml:3:7: worse");
    }
}
