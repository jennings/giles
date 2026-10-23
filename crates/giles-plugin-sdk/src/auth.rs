//! The per-launch token check on every RPC (PLAN section 7.4, step 4).

use std::sync::Arc;

use tonic::service::Interceptor;
use tonic::{Request, Status};

/// Rejects calls whose `authorization` metadata isn't `Bearer <token>`, with `UNAUTHENTICATED`.
/// Layered over the whole server, so it covers every service, health included.
#[derive(Clone)]
pub(crate) struct RequireBearer {
    expected: Arc<[u8]>,
}

impl RequireBearer {
    pub(crate) fn new(token: &str) -> Self {
        Self {
            expected: format!("Bearer {token}").into_bytes().into(),
        }
    }
}

impl Interceptor for RequireBearer {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let authorized = request
            .metadata()
            .get("authorization")
            .is_some_and(|value| constant_time_eq(value.as_bytes(), &self.expected));
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

    fn call(header: Option<&str>) -> Result<(), Code> {
        let mut request = Request::new(());
        if let Some(value) = header {
            request
                .metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        RequireBearer::new("s3cret")
            .call(request)
            .map(drop)
            .map_err(|status| status.code())
    }

    #[test]
    fn only_the_exact_bearer_token_passes() {
        assert_eq!(call(Some("Bearer s3cret")), Ok(()));
        for header in [
            None,
            Some(""),
            Some("s3cret"),
            Some("Bearer s3cre"),
            Some("Bearer s3cret2"),
            Some("Bearer S3cret"),
            Some("bearer s3cret"),
            Some("Basic s3cret"),
        ] {
            assert_eq!(call(header), Err(Code::Unauthenticated), "{header:?}");
        }
    }

    #[test]
    fn comparison_is_exact() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
