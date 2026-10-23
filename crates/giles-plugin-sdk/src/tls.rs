//! TLS for plugins: `rustls` trusting the bundled Mozilla root certificates, and the server side
//! of a remote plugin.
//!
//! A launched plugin's SDK makes no TLS connections; the core reaches it over a private socket
//! pair. Channel plugins do (IRC, IMAP, SMTP, WhatsApp), and these helpers give them all the same
//! trust store as the core's built-in providers: the roots compiled in from `webpki-root-certs`,
//! never the OS store, which a sandboxed plugin may not be able to read and which would make
//! behavior depend on the host. A remote plugin ([`RemoteServer`](crate::RemoteServer)) serves
//! TLS with [`server_config`]. Enabled by the `tls` feature (on by default).
//!
//! ```
//! let config = giles_plugin_sdk::tls::client_config();
//! assert!(config.alpn_protocols.is_empty());
//! // Hand `std::sync::Arc::new(config)` to tokio-rustls, an IMAP client, ...
//! ```

use std::io;
use std::sync::Arc;

pub use rustls;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{ClientConfig, RootCertStore, ServerConfig};

/// The bundled root certificates.
pub fn root_store() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
    roots
}

/// The `aws-lc-rs` crypto provider, named explicitly so another provider enabled elsewhere in the
/// build can't make it ambiguous.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// A client config with [`root_store`], `rustls`'s safe default protocol versions, the `aws-lc-rs`
/// crypto provider, and no client certificate.
pub fn client_config() -> ClientConfig {
    client_config_with_roots(root_store())
}

/// [`client_config`] trusting `roots` instead of the bundled roots.
pub fn client_config_with_roots(roots: impl Into<Arc<RootCertStore>>) -> ClientConfig {
    ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports the default protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// A server config for a remote plugin: the certificate chain `cert_chain_pem` (the server's
/// certificate first, then any intermediates) and its private key `key_pem` (PKCS #8, PKCS #1, or
/// SEC1), `rustls`'s safe default protocol versions with `aws-lc-rs`, and no client certificates
/// (the core authenticates with a bearer token). [`RemoteServer::tls`](crate::RemoteServer::tls)
/// adds the `h2` ALPN protocol gRPC needs.
///
/// # Errors
///
/// The PEM holds no certificate or no key, or the key doesn't match the certificate.
pub fn server_config(cert_chain_pem: &[u8], key_pem: &[u8]) -> io::Result<ServerConfig> {
    let chain = CertificateDer::pem_slice_iter(cert_chain_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| invalid(format!("the certificate chain: {e}")))?;
    if chain.is_empty() {
        return Err(invalid("the certificate chain holds no certificate"));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| invalid(format!("the private key: {e}")))?;
    ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports the default protocol versions")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| invalid(format!("the certificate and key: {e}")))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_root_is_trusted() {
        let roots = root_store();
        assert_eq!(roots.len(), webpki_root_certs::TLS_SERVER_ROOT_CERTS.len());
        assert!(roots.len() > 100, "only {} roots", roots.len());
    }

    #[test]
    fn client_config_uses_tls_1_2_or_later_without_client_auth() {
        let config = client_config();
        assert!(!config.client_auth_cert_resolver.has_certs());
        assert!(config.crypto_provider().cipher_suites.iter().all(|suite| {
            matches!(
                suite.version().version,
                rustls::ProtocolVersion::TLSv1_2 | rustls::ProtocolVersion::TLSv1_3
            )
        }));
    }
}
