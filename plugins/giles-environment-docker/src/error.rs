//! Errors are [`giles_types::Error`], whose messages may be shown to the model. This adds the
//! constructors the backend uses.

pub use giles_types::{Error, Result};

/// Constructors for [`Error`] that read like the failure they report.
pub(crate) trait ErrorExt {
    /// The request can't succeed as made: refused options, an id in use, a bad glob.
    fn invalid(message: impl Into<String>) -> Self;

    /// A workspace or a path doesn't exist.
    fn not_found(message: impl Into<String>) -> Self;

    /// Classifies a Docker API error: a connection or server failure is `Unavailable`, a 404 is
    /// `NotFound`, anything else `Other`. `context` says what was being done.
    fn docker(context: &str, error: &bollard::errors::Error) -> Self;
}

impl ErrorExt for Error {
    fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidRequest(message.into())
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    fn docker(context: &str, error: &bollard::errors::Error) -> Self {
        use bollard::errors::Error as Api;
        match error {
            Api::DockerResponseServerError {
                status_code: 404,
                message,
            } => Self::NotFound(format!("{context}: {message}")),
            Api::DockerResponseServerError {
                status_code,
                message,
            } if *status_code >= 500 => Self::unavailable(format!("{context}: {message}")),
            Api::DockerResponseServerError { message, .. } => {
                Self::other(format!("{context}: {message}"))
            }
            other => Self::unavailable(format!("{context}: {other}")),
        }
    }
}
