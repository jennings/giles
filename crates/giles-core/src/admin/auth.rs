//! The admin token (PLAN section 8.7): generated at every start, written to `run/admin.token`
//! (mode 0600), and required on every call.

use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::Arc;

use tonic::metadata::errors::InvalidMetadataValue;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::{Request, Status};

/// Random bytes in a token: 256 bits.
const TOKEN_BYTES: usize = 32;

/// A fresh random token: 64 lowercase hex digits.
pub(super) fn generate_token() -> io::Result<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(2 * TOKEN_BYTES), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        }))
}

/// Writes `token` to `path` with mode 0600, replacing any earlier file in one rename, so a client
/// never reads a partial token.
pub(super) fn write_token(path: &Path, token: &str) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("the token file has no parent directory"))?;
    // Temporary files are created with mode 0600.
    let mut file = tempfile::Builder::new()
        .prefix(".admin.token.")
        .tempfile_in(dir)?;
    file.write_all(token.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Reads the token the running core wrote to `path`.
pub(super) fn read_token(path: &Path) -> io::Result<String> {
    Ok(std::fs::read_to_string(path)?.trim().to_owned())
}

/// Rejects calls whose `authorization` metadata isn't `Bearer <token>`, with `UNAUTHENTICATED`.
#[derive(Clone)]
pub(super) struct RequireToken {
    expected: Arc<[u8]>,
}

impl RequireToken {
    pub(super) fn new(token: &str) -> Self {
        Self {
            expected: format!("Bearer {token}").into_bytes().into(),
        }
    }
}

impl Interceptor for RequireToken {
    fn call(&mut self, request: Request<()>) -> Result<Request<()>, Status> {
        let authorized = request
            .metadata()
            .get("authorization")
            .is_some_and(|value| constant_time_eq(value.as_bytes(), &self.expected));
        if authorized {
            Ok(request)
        } else {
            Err(Status::unauthenticated("missing or invalid admin token"))
        }
    }
}

/// Compares without an early exit on the first differing byte. The length isn't secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Adds `authorization: Bearer <token>` to every call: the client side of [`RequireToken`].
#[derive(Clone)]
pub struct BearerToken {
    authorization: MetadataValue<Ascii>,
}

impl BearerToken {
    /// The metadata for `token`.
    ///
    /// # Errors
    ///
    /// If `token` can't be a header value.
    pub fn new(token: &str) -> Result<Self, InvalidMetadataValue> {
        let mut authorization: MetadataValue<Ascii> = format!("Bearer {token}").parse()?;
        authorization.set_sensitive(true);
        Ok(Self { authorization })
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerToken").finish_non_exhaustive()
    }
}

impl Interceptor for BearerToken {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.authorization.clone());
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tonic::Code;

    use super::*;

    fn call(header: Option<&str>) -> Result<(), Code> {
        let mut request = Request::new(());
        if let Some(value) = header {
            request
                .metadata_mut()
                .insert("authorization", value.parse().unwrap());
        }
        RequireToken::new("s3cret")
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
    fn the_client_interceptor_satisfies_the_server_one() {
        let request = BearerToken::new("s3cret")
            .unwrap()
            .call(Request::new(()))
            .unwrap();
        assert!(RequireToken::new("s3cret").call(request).is_ok());
    }

    #[test]
    fn comparison_is_exact() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn tokens_are_256_random_bits_in_hex() {
        let a = generate_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_ne!(generate_token().unwrap(), a);
    }

    #[test]
    fn the_token_file_is_private_and_replaced_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.token");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_token(&path, "new-token").unwrap();
        assert_eq!(read_token(&path).unwrap(), "new-token");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Nothing else is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
