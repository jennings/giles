//! The bearer token check on every RPC: the per-launch token of a launched plugin (PLAN section
//! 7.4, step 4), or a remote plugin's [`RemoteAuth`](crate::RemoteAuth) (section 7.6).

use std::sync::Arc;

use tonic::service::Interceptor;
use tonic::{Request, Status};

/// Decides whether a bearer token is acceptable.
pub(crate) type VerifyFn = dyn Fn(&str) -> bool + Send + Sync;

/// Rejects calls whose `authorization` metadata isn't an acceptable `Bearer <token>`, with
/// `UNAUTHENTICATED`. Layered over the whole server, so it covers every service, health included.
#[derive(Clone)]
pub(crate) enum RequireBearer {
    /// Exactly `Bearer <token>`.
    Exact(Arc<[u8]>),
    /// `Bearer <token>` with a token the function accepts.
    Verify(Arc<VerifyFn>),
    /// Every call, with or without the header.
    Anyone,
}

impl RequireBearer {
    /// Accepts exactly `token`.
    pub(crate) fn new(token: &str) -> Self {
        Self::Exact(format!("Bearer {token}").into_bytes().into())
    }
}

impl Interceptor for RequireBearer {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let header = request.metadata().get("authorization");
        let authorized = match self {
            Self::Exact(expected) => {
                header.is_some_and(|value| constant_time_eq(value.as_bytes(), expected))
            }
            Self::Verify(verify) => header
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .is_some_and(|token| !token.is_empty() && verify(token)),
            Self::Anyone => true,
        };
        if authorized {
            Ok(request)
        } else {
            Err(Status::unauthenticated("missing or invalid plugin token"))
        }
    }
}

/// Compares without an early exit on the first differing byte. The length isn't secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;

    fn call(check: &RequireBearer, header: Option<&str>) -> Result<(), Code> {
        let mut request = Request::new(());
        if let Some(value) = header {
            request
                .metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        check
            .clone()
            .call(request)
            .map(drop)
            .map_err(|status| status.code())
    }

    const WRONG: [Option<&str>; 8] = [
        None,
        Some(""),
        Some("s3cret"),
        Some("Bearer s3cre"),
        Some("Bearer s3cret2"),
        Some("Bearer S3cret"),
        Some("bearer s3cret"),
        Some("Basic s3cret"),
    ];

    #[test]
    fn only_the_exact_bearer_token_passes() {
        let check = RequireBearer::new("s3cret");
        assert_eq!(call(&check, Some("Bearer s3cret")), Ok(()));
        for header in WRONG {
            assert_eq!(
                call(&check, header),
                Err(Code::Unauthenticated),
                "{header:?}"
            );
        }
    }

    #[test]
    fn a_verifier_sees_only_the_token_after_bearer() {
        let check = RequireBearer::Verify(Arc::new(|token| token == "s3cret"));
        assert_eq!(call(&check, Some("Bearer s3cret")), Ok(()));
        for header in WRONG.into_iter().chain([Some("Bearer ")]) {
            assert_eq!(
                call(&check, header),
                Err(Code::Unauthenticated),
                "{header:?}"
            );
        }
        // An empty token never reaches the verifier.
        let anything = RequireBearer::Verify(Arc::new(|_| true));
        assert_eq!(call(&anything, Some("Bearer ")), Err(Code::Unauthenticated));
        assert_eq!(call(&anything, None), Err(Code::Unauthenticated));
        assert_eq!(call(&anything, Some("Bearer x")), Ok(()));
    }

    #[test]
    fn anyone_passes_without_a_header() {
        assert_eq!(call(&RequireBearer::Anyone, None), Ok(()));
    }

    #[test]
    fn comparison_is_exact() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
