//! The plugin's error type. Its variants mirror what the environment protocol can report, and
//! their messages may be shown to the model.

/// What went wrong.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The request can't succeed as made: refused options, an id in use, a bad glob. Retrying it
    /// won't help.
    #[error("{0}")]
    InvalidRequest(String),
    /// A workspace or a path doesn't exist.
    #[error("{0}")]
    NotFound(String),
    /// The Docker API can't be reached or failed.
    #[error("{0}")]
    Unavailable(String),
    /// Any other failure.
    #[error("{0}")]
    Other(String),
}

/// A `Result` whose error is [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    /// Classifies a Docker API error: a connection or server failure is `Unavailable`, a 404 is
    /// `NotFound`, anything else `Other`. `context` says what was being done.
    pub(crate) fn docker(context: &str, error: &bollard::errors::Error) -> Self {
        use bollard::errors::Error as Api;
        match error {
            Api::DockerResponseServerError {
                status_code: 404,
                message,
            } => Self::NotFound(format!("{context}: {message}")),
            Api::DockerResponseServerError {
                status_code,
                message,
            } if *status_code >= 500 => Self::Unavailable(format!("{context}: {message}")),
            Api::DockerResponseServerError { message, .. } => {
                Self::Other(format!("{context}: {message}"))
            }
            other => Self::Unavailable(format!("{context}: {other}")),
        }
    }
}
