//! The SQLite journal modes the store supports.

use std::fmt;
use std::str::FromStr;

/// How SQLite makes a write atomic and durable (`PRAGMA journal_mode`).
///
/// Only the modes that keep a journal are here. `memory` and `off` exist in SQLite but give up
/// the rollback journal, so a crash or power loss in the middle of a write can corrupt the
/// database or lose data; they can't be requested ([`FromStr`] refuses them).
///
/// [`Wal`](Self::Wal) is the default and the mode the store is designed for: readers and the
/// writer don't block each other. The other modes use a rollback journal, where a commit has to
/// wait for the readers (see `Docs/technical/storage.md`); they exist for file systems that
/// can't run WAL, which needs shared memory (network file systems).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum JournalMode {
    /// Write-ahead log. Persistent: it is recorded in the database file.
    #[default]
    Wal,
    /// Rollback journal, deleted after each transaction.
    Delete,
    /// Rollback journal, truncated to zero length after each transaction.
    Truncate,
    /// Rollback journal, kept and invalidated after each transaction.
    Persist,
}

impl JournalMode {
    /// Every mode, the default first.
    pub const ALL: [Self; 4] = [Self::Wal, Self::Delete, Self::Truncate, Self::Persist];

    /// The mode's name, which is also its `PRAGMA journal_mode` argument and what SQLite reports
    /// back, in lowercase.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wal => "wal",
            Self::Delete => "delete",
            Self::Truncate => "truncate",
            Self::Persist => "persist",
        }
    }
}

impl fmt::Display for JournalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// [`JournalMode::from_str`] refused its input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseJournalModeError {
    /// `memory` or `off`.
    #[error(
        "journal mode {0:?} is not allowed: it gives up the rollback journal, so a crash or power \
         loss during a write can corrupt the database or lose data. Use \"wal\" (the default), \
         \"delete\", \"truncate\", or \"persist\""
    )]
    Unsafe(String),
    /// Anything else.
    #[error(
        "unknown journal mode {0:?}: use \"wal\" (the default), \"delete\", \"truncate\", or \
         \"persist\""
    )]
    Unknown(String),
}

impl FromStr for JournalMode {
    type Err = ParseJournalModeError;

    /// Parses a mode name, ignoring ASCII case.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if let Some(mode) = Self::ALL
            .into_iter()
            .find(|mode| text.eq_ignore_ascii_case(mode.as_str()))
        {
            Ok(mode)
        } else if ["memory", "off"]
            .iter()
            .any(|unsafe_mode| text.eq_ignore_ascii_case(unsafe_mode))
        {
            Err(ParseJournalModeError::Unsafe(text.to_owned()))
        } else {
            Err(ParseJournalModeError::Unknown(text.to_owned()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_parses_from_its_name_in_any_case() {
        for mode in JournalMode::ALL {
            let name = mode.as_str();
            assert_eq!(name.parse::<JournalMode>(), Ok(mode));
            assert_eq!(name.to_uppercase().parse::<JournalMode>(), Ok(mode));
            let mixed: String = name
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i % 2 == 0 {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    }
                })
                .collect();
            assert_eq!(mixed.parse::<JournalMode>(), Ok(mode), "{mixed}");
            assert_eq!(mode.to_string(), name);
        }
        assert_eq!(JournalMode::default(), JournalMode::Wal);
    }

    #[test]
    fn memory_and_off_are_refused_as_unsafe() {
        for text in ["memory", "OFF", "Memory", "off"] {
            let err = text.parse::<JournalMode>().unwrap_err();
            assert_eq!(err, ParseJournalModeError::Unsafe(text.to_owned()));
            let message = err.to_string();
            assert!(message.contains("corrupt"), "{message}");
            assert!(message.contains("\"wal\""), "{message}");
        }
    }

    #[test]
    fn anything_else_is_unknown_and_the_error_lists_the_modes() {
        for text in ["", "wal2", " wal", "wal ", "rollback", "delete;"] {
            let err = text.parse::<JournalMode>().unwrap_err();
            assert_eq!(err, ParseJournalModeError::Unknown(text.to_owned()));
            let message = err.to_string();
            for mode in JournalMode::ALL {
                assert!(message.contains(mode.as_str()), "{message}");
            }
        }
    }
}
