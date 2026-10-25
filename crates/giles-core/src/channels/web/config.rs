//! The web channel's `config` table (`[channel.<instance>] config = { ... }`).

use std::net::SocketAddr;

use axum::http::HeaderName;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::config::{KeyPath, Secret};

/// The shared-secret header a TCP proxy listener requires unless `secret_header` names another.
pub const DEFAULT_SECRET_HEADER: &str = "x-giles-proxy-secret";

/// The shortest shared secret a TCP proxy listener accepts, in bytes.
pub const MIN_SECRET_LEN: usize = 16;

/// Headers the identity or secret header may not be: they mean something else to Giles or to
/// browsers.
const RESERVED_HEADERS: [&str; 6] = [
    "authorization",
    "cookie",
    "host",
    "x-csrf-token",
    "x-forwarded-proto",
    "last-event-id",
];

/// The web channel's settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebConfig {
    /// The URL people reach Giles at, as an origin without a trailing slash (e.g.
    /// `https://giles.example.com`): login links start with it, and an `https` URL makes the
    /// session cookie `Secure`. `None`: `http://<core.http_listen>`.
    pub public_url: Option<String>,
    /// The trusted-proxy listener, if configured.
    pub proxy: Option<ProxyConfig>,
    /// Keys of the config table that the web channel doesn't know: ignored, and reported by the
    /// caller. Empty unless [`WebConfig::from_json`] found some. The `proxy` table is strict, so
    /// none are ever inside it.
    pub unknown_keys: Vec<KeyPath>,
}

/// A trusted reverse proxy (PLAN section 3.4): it authenticates people and names them in a header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    /// The header carrying the user's native id in the `web` namespace, e.g.
    /// `tailscale-user-login`. Honored only on the proxy listener.
    pub identity_header: HeaderName,
    /// Where the proxy connects.
    pub listen: ProxyListen,
}

/// The dedicated listener for the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyListen {
    /// The Unix socket `run/proxy.sock` (mode 0600).
    Unix,
    /// A TCP address; every request must carry `secret_header: secret`.
    Tcp {
        /// The address.
        addr: SocketAddr,
        /// The header the proxy adds.
        secret_header: HeaderName,
        /// Its value (`Debug` and `Display` redact it).
        secret: Secret,
    },
}

#[derive(Deserialize)]
struct Raw {
    public_url: Option<String>,
    proxy: Option<RawProxy>,
}

/// Strict, unlike the rest of the table: this configures a listener and its shared secret, and a
/// misspelled `secret_header` or `listen` would leave the proxy listener unprotected or elsewhere.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProxy {
    identity_header: String,
    listen: Option<SocketAddr>,
    secret_header: Option<String>,
    secret: Option<String>,
}

impl WebConfig {
    /// Parses the instance's `config` table (`PluginConfig::to_json`). Unknown keys are not errors:
    /// they are ignored and listed in [`unknown_keys`](Self::unknown_keys). The exception is the
    /// `proxy` table, where an unknown key is an error.
    ///
    /// # Errors
    ///
    /// The reason, naming the key at fault.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let mut unknown_keys = Vec::new();
        let mut record = |path: serde_ignored::Path<'_>| unknown_keys.push(KeyPath::from(&path));
        let raw: Raw =
            serde_path_to_error::deserialize(serde_ignored::Deserializer::new(value, &mut record))
                .map_err(|e| {
                    let path = e.path().to_string();
                    if path == "." {
                        e.into_inner().to_string()
                    } else {
                        format!("{path}: {}", e.into_inner())
                    }
                })?;
        Ok(Self {
            public_url: raw.public_url.as_deref().map(public_url).transpose()?,
            proxy: raw.proxy.map(proxy).transpose()?,
            unknown_keys,
        })
    }

    /// Whether the session cookie is `Secure` on every listener.
    pub fn https(&self) -> bool {
        self.public_url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://"))
    }
}

fn public_url(text: &str) -> Result<String, String> {
    let fail = |why: &str| Err(format!("public_url: {why}"));
    let Ok(url) = Url::parse(text) else {
        return fail("not a URL");
    };
    if !matches!(url.scheme(), "http" | "https") {
        return fail("must be an http or https URL");
    }
    if url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        return fail("must name a host and no user");
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return fail("must be just the scheme, host, and port: Giles is served at the root");
    }
    Ok(url.origin().ascii_serialization())
}

fn header(key: &str, name: &str) -> Result<HeaderName, String> {
    let header = HeaderName::try_from(name).map_err(|_| format!("{key}: not a header name"))?;
    if RESERVED_HEADERS.contains(&header.as_str()) {
        return Err(format!("{key}: `{header}` is reserved"));
    }
    Ok(header)
}

fn proxy(raw: RawProxy) -> Result<ProxyConfig, String> {
    let identity_header = header("proxy.identity_header", &raw.identity_header)?;
    let listen = match raw.listen {
        None => {
            if raw.secret.is_some() || raw.secret_header.is_some() {
                return Err(
                    "proxy.secret: only for a TCP listener (`proxy.listen`); the Unix socket \
                     needs none"
                        .to_owned(),
                );
            }
            ProxyListen::Unix
        }
        Some(addr) => {
            let Some(secret) = raw.secret else {
                return Err(
                    "proxy.secret: required with `proxy.listen`: any local process can connect to \
                     a TCP port"
                        .to_owned(),
                );
            };
            if secret.len() < MIN_SECRET_LEN {
                return Err(format!(
                    "proxy.secret: must be at least {MIN_SECRET_LEN} bytes"
                ));
            }
            let secret_header = header(
                "proxy.secret_header",
                raw.secret_header
                    .as_deref()
                    .unwrap_or(DEFAULT_SECRET_HEADER),
            )?;
            if secret_header == identity_header {
                return Err(
                    "proxy.secret_header: must differ from `proxy.identity_header`".to_owned(),
                );
            }
            ProxyListen::Tcp {
                addr,
                secret_header,
                secret: Secret::new(secret),
            }
        }
    };
    Ok(ProxyConfig {
        identity_header,
        listen,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn empty_table_is_the_default_and_unknown_keys_are_listed() {
        assert_eq!(WebConfig::from_json(&json!({})), Ok(WebConfig::default()));
        let config = WebConfig::from_json(&json!({
            "bogus": 1,
            "public_url": "https://giles.example.com",
            "nested": {"a": 1},
        }))
        .unwrap();
        let mut keys: Vec<String> = config
            .unknown_keys
            .iter()
            .map(ToString::to_string)
            .collect();
        keys.sort();
        assert_eq!(keys, ["bogus", "nested"]);
        assert_eq!(
            config.public_url.as_deref(),
            Some("https://giles.example.com")
        );
        // A known key with a bad value is an error, even next to an unknown key.
        let err = WebConfig::from_json(&json!({"bogus": 1, "public_url": 5})).unwrap_err();
        assert!(err.starts_with("public_url: "), "{err}");
        // `proxy` stays strict: it configures a listener and a shared secret.
        let err = WebConfig::from_json(&json!({"proxy": {"identity_header": "x-user", "x": 1}}))
            .unwrap_err();
        assert!(err.starts_with("proxy.x: unknown field `x`"), "{err}");
    }

    #[test]
    fn public_url_is_an_origin() {
        let config =
            WebConfig::from_json(&json!({"public_url": "https://Giles.Example.com/"})).unwrap();
        assert_eq!(
            config.public_url.as_deref(),
            Some("https://giles.example.com")
        );
        assert!(config.https());
        let config = WebConfig::from_json(&json!({"public_url": "http://127.0.0.1:8080"})).unwrap();
        assert_eq!(config.public_url.as_deref(), Some("http://127.0.0.1:8080"));
        assert!(!config.https());
        for bad in [
            "giles.example.com",
            "ftp://giles.example.com",
            "https://user@giles.example.com",
            "https://giles.example.com/giles",
            "https://giles.example.com/?x",
        ] {
            let err = WebConfig::from_json(&json!({ "public_url": bad })).unwrap_err();
            assert!(err.starts_with("public_url: "), "{bad}: {err}");
        }
    }

    #[test]
    fn the_proxy_listens_on_the_socket_or_on_tcp_with_a_secret() {
        let config =
            WebConfig::from_json(&json!({"proxy": {"identity_header": "Tailscale-User-Login"}}))
                .unwrap();
        let proxy = config.proxy.unwrap();
        assert_eq!(proxy.identity_header, "tailscale-user-login");
        assert_eq!(proxy.listen, ProxyListen::Unix);

        let config = WebConfig::from_json(&json!({"proxy": {
            "identity_header": "x-user",
            "listen": "127.0.0.1:8081",
            "secret": "0123456789abcdef",
        }}))
        .unwrap();
        assert_eq!(
            config.proxy.unwrap().listen,
            ProxyListen::Tcp {
                addr: "127.0.0.1:8081".parse().unwrap(),
                secret_header: HeaderName::from_static(DEFAULT_SECRET_HEADER),
                secret: Secret::new("0123456789abcdef"),
            }
        );

        for (proxy, error) in [
            (
                json!({"identity_header": "bad header"}),
                "proxy.identity_header: not a header",
            ),
            (json!({"identity_header": "Cookie"}), "`cookie` is reserved"),
            (
                json!({"identity_header": "x-user", "secret": "0123456789abcdef"}),
                "proxy.secret: only for a TCP listener",
            ),
            (
                json!({"identity_header": "x-user", "listen": "127.0.0.1:1"}),
                "proxy.secret: required",
            ),
            (
                json!({"identity_header": "x-user", "listen": "127.0.0.1:1", "secret": "short"}),
                "at least 16 bytes",
            ),
            (
                json!({"identity_header": "x-user", "listen": "127.0.0.1:1",
                       "secret": "0123456789abcdef", "secret_header": "X-User"}),
                "must differ",
            ),
            (
                json!({"identity_header": "x-user", "listen": "nowhere"}),
                "proxy.listen",
            ),
        ] {
            let err = WebConfig::from_json(&json!({ "proxy": proxy })).unwrap_err();
            assert!(err.contains(error), "{proxy}: {err}");
        }
    }
}
