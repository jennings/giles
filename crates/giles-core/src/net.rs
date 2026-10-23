//! Outbound HTTP shared by the parts of the core that call remote services: the built-in providers
//! and, later, remote MCP servers.

use giles_types::Error;

/// A `reqwest` client builder with the core's fixed outbound settings:
///
/// - TLS is rustls trusting only the bundled Mozilla root certificates (`webpki-root-certs`),
///   never the OS trust store;
/// - redirects aren't followed, so a credential header can't be forwarded to another host;
/// - the user agent is `giles/<version>`.
///
/// Callers add their own timeouts and build.
pub fn tls_client_builder() -> Result<reqwest::ClientBuilder, Error> {
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|der| reqwest::Certificate::from_der(der))
        .collect::<Result<Vec<_>, _>>()
        .map_err(Error::other)?;
    Ok(reqwest::Client::builder()
        .tls_certs_only(roots)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("giles/", env!("CARGO_PKG_VERSION"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_bundled_roots() {
        tls_client_builder().unwrap().build().unwrap();
    }
}
