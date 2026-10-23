//! HTTP plumbing shared by the built-in providers: a `reqwest` client that trusts only the bundled
//! Mozilla roots, and the mapping from HTTP failures to [`giles_types::Error`].

use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, TryStreamExt};
use giles_types::Error;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::{Response, StatusCode};

/// How much of an error response body is read to find the error message.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// Connection settings of a provider's HTTP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpOptions {
    /// Limit on establishing a connection, TLS handshake included.
    pub connect_timeout: Duration,
    /// Limit on waiting for any single read: response headers, or the next chunk of a streamed
    /// body. Streams stay open as long as data keeps coming.
    pub read_timeout: Duration,
}

impl Default for HttpOptions {
    /// 10 seconds to connect; 5 minutes between reads, since a model may think for a while
    /// before the response headers arrive.
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(300),
        }
    }
}

/// Builds a client that trusts only the bundled Mozilla root certificates (`webpki-root-certs`),
/// never the OS trust store, and doesn't follow redirects, so an API key header can't be
/// forwarded to another host.
pub fn client(options: &HttpOptions) -> Result<reqwest::Client, Error> {
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| reqwest::Certificate::from_der(der))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Error::other)?;
    reqwest::Client::builder()
        .tls_certs_only(roots)
        .connect_timeout(options.connect_timeout)
        .read_timeout(options.read_timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("giles/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(Error::other)
}

/// The `Retry-After` header as a delay. Only the delay-seconds form is understood (integer or
/// decimal, as model APIs send it); an HTTP date or a malformed value gives `None`.
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let seconds: f64 = value.parse().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

/// Maps an unsuccessful HTTP status to an error: 408, 429, and 5xx are
/// [`Error::Unavailable`] (worth retrying, with the server's `retry_after`), 404 is
/// [`Error::NotFound`], other 4xx are [`Error::InvalidRequest`], anything else is
/// [`Error::Other`].
pub fn status_error(status: StatusCode, retry_after: Option<Duration>, message: String) -> Error {
    match status.as_u16() {
        408 | 429 | 500..=599 => Error::Unavailable {
            message,
            retry_after,
        },
        404 => Error::NotFound(message),
        400..=499 => Error::InvalidRequest(message),
        _ => Error::other(message),
    }
}

/// Maps a transport failure (connect, TLS, timeout, reset) to [`Error::Unavailable`]. The message
/// includes the error's sources, which say what actually went wrong.
pub fn transport_error(error: reqwest::Error) -> Error {
    if error.is_builder() {
        return Error::other(error);
    }
    Error::unavailable(error_chain(&error))
}

/// Reads at most 64 KiB of an error response's body, as text. A body that fails to read gives
/// what was read so far.
pub async fn error_body(mut response: Response) -> String {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let room = MAX_ERROR_BODY - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() == MAX_ERROR_BODY {
            break;
        }
    }
    String::from_utf8_lossy(&body).into_owned()
}

/// A response body as a stream of chunks, with transport failures mapped by [`transport_error`].
pub fn body_stream(response: Response) -> impl Stream<Item = Result<Bytes, Error>> + Send {
    response.bytes_stream().map_err(transport_error)
}

/// `error: source: source...`, so a transport error says what failed underneath.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderValue;

    use super::*;

    fn headers(retry_after: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(retry_after).unwrap());
        headers
    }

    #[test]
    fn retry_after_seconds() {
        assert_eq!(retry_after(&headers("30")), Some(Duration::from_secs(30)));
        assert_eq!(retry_after(&headers(" 0 ")), Some(Duration::ZERO));
        assert_eq!(
            retry_after(&headers("1.5")),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(retry_after(&HeaderMap::new()), None);
        assert_eq!(retry_after(&headers("-1")), None);
        assert_eq!(retry_after(&headers("soon")), None);
        assert_eq!(retry_after(&headers("NaN")), None);
        assert_eq!(retry_after(&headers("Wed, 21 Oct 2015 07:28:00 GMT")), None);
    }

    #[test]
    fn status_classification() {
        let wait = Some(Duration::from_secs(5));
        for code in [408, 429, 500, 502, 503, 529, 599] {
            let error = status_error(StatusCode::from_u16(code).unwrap(), wait, "m".into());
            assert!(
                matches!(error, Error::Unavailable { retry_after, .. } if retry_after == wait),
                "{code}: {error:?}"
            );
        }
        for code in [400, 401, 403, 413, 422] {
            let error = status_error(StatusCode::from_u16(code).unwrap(), wait, "bad".into());
            assert!(
                matches!(&error, Error::InvalidRequest(m) if m == "bad"),
                "{code}"
            );
        }
        assert!(matches!(
            status_error(StatusCode::NOT_FOUND, None, "no model".into()),
            Error::NotFound(m) if m == "no model"
        ));
        let other = status_error(StatusCode::MULTIPLE_CHOICES, None, "redirect".into());
        assert!(matches!(other, Error::Other(_)));
        assert!(!other.is_retryable());
    }

    #[test]
    fn client_builds_with_bundled_roots() {
        client(&HttpOptions::default()).unwrap();
    }

    #[test]
    fn error_chain_skips_repeated_text() {
        #[derive(Debug)]
        struct Outer(std::io::Error);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "send failed")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let error = Outer(std::io::Error::other("connection reset"));
        assert_eq!(error_chain(&error), "send failed: connection reset");
    }
}
