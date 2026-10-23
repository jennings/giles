//! TLS for plugins: `rustls` trusting the bundled Mozilla root certificates.
//!
//! The SDK itself makes no TLS connections; the core reaches a plugin over a private socket pair.
//! Channel plugins do (IRC, IMAP, SMTP, WhatsApp), and these helpers give them all the same trust
//! store as the core's built-in providers: the roots compiled in from `webpki-root-certs`, never
//! the OS store, which a sandboxed plugin may not be able to read and which would make behavior
//! depend on the host. Enabled by the `tls` feature (on by default).
//!
//! ```
//! let config = giles_plugin_sdk::tls::client_config();
//! assert!(config.alpn_protocols.is_empty());
//! // Hand `std::sync::Arc::new(config)` to tokio-rustls, an IMAP client, ...
//! ```

use std::sync::Arc;

pub use rustls;
use rustls::{ClientConfig, RootCertStore};

/// The bundled root certificates.
pub fn root_store() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned());
    roots
}

/// A client config with [`root_store`], `rustls`'s safe default protocol versions, the `aws-lc-rs`
/// crypto provider (named explicitly, so another provider enabled elsewhere in the build can't
/// make it ambiguous), and no client certificate.
pub fn client_config() -> ClientConfig {
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("aws-lc-rs supports the default protocol versions")
        .with_root_certificates(root_store())
        .with_no_client_auth()
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
