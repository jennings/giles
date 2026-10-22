//! The error type returned by the component traits.

use std::time::Duration;

use crate::id::IdError;

/// A `Result` whose error defaults to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// An error from a component (provider, channel, or toolset).
///
/// The variants tell the core how to react, not what went wrong inside the component:
/// [`Error::Unavailable`] is worth retrying (or falling back to another model, PLAN section
/// 6.10); the others are not.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A value isn't a valid id.
    #[error(transparent)]
    InvalidId(#[from] IdError),
    /// The request can't succeed as made: bad input, an unknown model, a recipient the channel
    /// can't address, a rejected configuration. Retrying it won't help.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The named thing (model, skill, conversation) doesn't exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The component or the service behind it can't serve the request right now: rate limited,
    /// overloaded, a 5xx response, a timeout, a disconnected network, or a recipient that isn't
    /// online. Worth retrying later.
    #[error("unavailable: {message}")]
    Unavailable {
        /// What happened.
        message: String,
        /// How long the service asked the caller to wait, if it said.
        retry_after: Option<Duration>,
    },
    /// Any other failure.
    #[error(transparent)]
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl Error {
    /// An [`Error::Unavailable`] with no retry hint.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
            retry_after: None,
        }
    }

    /// Wraps any other error, or a message, as [`Error::Other`].
    pub fn other(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Other(error.into())
    }

    /// Whether retrying the same request later may succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::EventId;

    #[test]
    fn only_unavailable_is_retryable() {
        assert!(Error::unavailable("overloaded").is_retryable());
        assert!(
            Error::Unavailable {
                message: "rate limited".into(),
                retry_after: Some(Duration::from_secs(30)),
            }
            .is_retryable()
        );
        assert!(!Error::InvalidRequest("bad".into()).is_retryable());
        assert!(!Error::NotFound("skill x".into()).is_retryable());
        assert!(!Error::other("boom").is_retryable());
        assert!(!Error::from(EventId::new("").unwrap_err()).is_retryable());
    }

    #[test]
    fn messages_and_sources() {
        assert_eq!(
            Error::unavailable("overloaded").to_string(),
            "unavailable: overloaded"
        );
        let id_error = Error::from(EventId::new("").unwrap_err());
        assert_eq!(id_error.to_string(), "invalid event id: must not be empty");

        let io = std::io::Error::other("disk full");
        let other = Error::other(io);
        assert_eq!(other.to_string(), "disk full");
    }
}
