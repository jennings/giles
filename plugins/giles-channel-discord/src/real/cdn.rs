//! Attachment downloads from Discord's CDN, on `reqwest`: `twilight-http` talks to the API, not to
//! arbitrary HTTP servers, and the CDN is not the API (no bucket to rate limit, no bot token to
//! send).
//!
//! What is downloaded comes from a message, so the URL is data a stranger controls, and the
//! client is built to be useless for anything but Discord's CDN:
//!
//! - the URL must be `https` (or plain HTTP for the loopback servers of the tests, see
//!   [`CdnPolicy::loopback`]), on a host of [`CdnPolicy::hosts`] exactly (`cdn.discordapp.com` and
//!   `media.discordapp.net`), with no user name or password and no port but 443. That also rules
//!   out every address, so a crafted attachment can't make the plugin request its own host
//!   network (SSRF);
//! - a redirect is followed only to a URL that passes the same test, at most [`MAX_REDIRECTS`]
//!   times; any other redirect is not followed and its response is an error;
//! - no `Authorization` header, cookie, or `Referer` is ever sent: the bot token must never reach
//!   the CDN;
//! - a connect, a pause between reads, and the download as a whole are bounded by timeouts;
//! - a declared length over the limit is refused before the body is read, and the body is handed
//!   over as a stream for the caller to cut off (see [`Discord::download`]).
//!
//! The URL's query string is a credential (`ex`, `is`, `hm`): nothing here logs a URL, or a
//! `reqwest` error (which would print one).
//!
//! [`Discord::download`]: crate::client::Discord::download

use std::time::Duration;

use futures::TryStreamExt as _;
use giles_plugin_sdk::tls;
use reqwest::redirect::{Attempt, Policy};
use reqwest::{Client, Url};

use crate::client::{Body, RestError};

/// The hosts of Discord's CDN: attachments are on the first; the second is the media proxy.
const DISCORD_HOSTS: [&str; 2] = ["cdn.discordapp.com", "media.discordapp.net"];
/// How many redirects a download follows.
pub const MAX_REDIRECTS: usize = 3;

/// Where downloads may go, and how long they may take.
#[derive(Debug, Clone)]
pub struct CdnPolicy {
    /// The only hosts a URL may name (lowercase).
    pub hosts: Vec<String>,
    /// Plain HTTP and any port are allowed: for the tests' servers on loopback, never for Discord.
    pub loopback: bool,
    /// How long connecting may take.
    pub connect_timeout: Duration,
    /// How long the CDN may be silent: before its answer, and between chunks of the body.
    pub read_timeout: Duration,
    /// How long a whole download may take.
    pub total_timeout: Duration,
}

impl CdnPolicy {
    /// Discord's CDN, over `https`, with the timeouts the plugin uses: 10 seconds to connect,
    /// 30 seconds of silence, 5 minutes for a whole file.
    pub fn discord() -> Self {
        Self {
            hosts: DISCORD_HOSTS
                .iter()
                .map(|host| (*host).to_owned())
                .collect(),
            loopback: false,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(300),
        }
    }

    /// A policy for a test server: the hosts `hosts` (for example `127.0.0.1`), over plain HTTP, on
    /// any port.
    pub fn loopback(hosts: &[&str]) -> Self {
        Self {
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            loopback: true,
            ..Self::discord()
        }
    }

    /// Whether `url` may be requested.
    pub fn allows(&self, url: &Url) -> bool {
        let scheme = match url.scheme() {
            "https" => true,
            "http" => self.loopback,
            _ => false,
        };
        scheme
            && url.username().is_empty()
            && url.password().is_none()
            && (self.loopback || matches!(url.port(), None | Some(443)))
            && url
                .host_str()
                .is_some_and(|host| self.hosts.iter().any(|allowed| allowed == host))
    }
}

/// The CDN client.
pub(super) struct Cdn {
    http: Client,
    policy: CdnPolicy,
}

impl Cdn {
    /// A client that follows `policy`.
    pub(super) fn new(policy: CdnPolicy) -> Self {
        let for_redirects = policy.clone();
        let http = Client::builder()
            // The SDK's TLS config: the bundled Mozilla roots and `aws-lc-rs`, like every TLS
            // client of the plugin.
            .tls_backend_preconfigured(tls::client_config())
            .redirect(Policy::custom(move |attempt| {
                redirect(&for_redirects, attempt)
            }))
            .referer(false)
            .no_proxy()
            .connect_timeout(policy.connect_timeout)
            .read_timeout(policy.read_timeout)
            .timeout(policy.total_timeout)
            .user_agent(concat!(
                "DiscordBot (https://github.com/jennings/giles, ",
                env!("CARGO_PKG_VERSION"),
                ")"
            ))
            .build()
            .expect("a client with the bundled roots builds");
        Self { http, policy }
    }

    /// See [`Discord::download`](crate::client::Discord::download).
    pub(super) async fn download(&self, url: &str, max: u64) -> Result<Body, RestError> {
        let url = Url::parse(url)
            .ok()
            .filter(|url| self.policy.allows(url))
            .ok_or_else(|| {
                tracing::debug!("refused to download from a URL that isn't the Discord CDN's");
                RestError::BadUrl
            })?;
        let host = url.host_str().unwrap_or_default().to_owned();
        let response = self.http.get(url).send().await.map_err(|error| {
            tracing::debug!(
                host,
                reason = reason(&error),
                "an attachment download failed"
            );
            RestError::Transport
        })?;
        let status = response.status();
        if !status.is_success() {
            tracing::debug!(host, status = status.as_u16(), "the CDN refused a download");
            return Err(RestError::Api {
                status: status.as_u16(),
                code: None,
            });
        }
        if response.content_length().is_some_and(|length| length > max) {
            tracing::debug!(host, "the CDN says a file is over the limit");
            return Err(RestError::TooLarge);
        }
        Ok(Box::pin(response.bytes_stream().map_err(move |error| {
            tracing::debug!(
                host,
                reason = reason(&error),
                "an attachment download broke off"
            );
            RestError::Transport
        })))
    }
}

/// A redirect's fate: followed to an allowed URL, at most [`MAX_REDIRECTS`] times, else not
/// followed (the response is then an error status).
fn redirect(policy: &CdnPolicy, attempt: Attempt<'_>) -> reqwest::redirect::Action {
    if attempt.previous().len() > MAX_REDIRECTS {
        attempt.error("too many redirects")
    } else if policy.allows(attempt.url()) {
        attempt.follow()
    } else {
        attempt.stop()
    }
}

/// What kind of failure `error` is, in words that carry no URL.
fn reason(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "couldn't connect"
    } else if error.is_redirect() {
        "too many redirects"
    } else if error.is_body() || error.is_decode() {
        "the body broke off"
    } else {
        "request failed"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(policy: &CdnPolicy, url: &str) -> bool {
        policy.allows(&Url::parse(url).unwrap())
    }

    #[test]
    fn only_https_urls_of_the_cdn_hosts_are_allowed() {
        let policy = CdnPolicy::discord();
        for good in [
            "https://cdn.discordapp.com/attachments/1/2/a.png?ex=1&is=2&hm=3",
            "https://media.discordapp.net/attachments/1/2/a.png",
            "https://CDN.DISCORDAPP.COM/a",
            "https://cdn.discordapp.com:443/a",
            "https://cdn.discordapp.com",
        ] {
            assert!(allowed(&policy, good), "{good}");
        }
        for bad in [
            "https://cdn.discordapp.com./a",
            "http://cdn.discordapp.com/a.png",
            "ftp://cdn.discordapp.com/a",
            "file:///etc/passwd",
            "https://discordapp.com/a",
            "https://evil.example/a",
            "https://cdn.discordapp.com.evil.example/a",
            "https://evilcdn.discordapp.com/a",
            "https://sub.cdn.discordapp.com/a",
            "https://cdn.discordapp.com@evil.example/a",
            "https://user:pass@cdn.discordapp.com/a",
            "https://user@cdn.discordapp.com/a",
            "https://cdn.discordapp.com:8443/a",
            "https://127.0.0.1/a",
            "https://[::1]/a",
            "https://localhost/a",
            "https://169.254.169.254/latest/meta-data",
            "https://0x7f.1/a",
            "data:text/plain,hi",
        ] {
            assert!(!allowed(&policy, bad), "{bad}");
        }
    }

    #[test]
    fn a_loopback_policy_allows_plain_http_to_its_hosts_only() {
        let policy = CdnPolicy::loopback(&["127.0.0.1"]);
        assert!(allowed(&policy, "http://127.0.0.1:4000/a"));
        assert!(allowed(&policy, "https://127.0.0.1:4000/a"));
        assert!(!allowed(&policy, "http://localhost:4000/a"));
        assert!(!allowed(&policy, "http://cdn.discordapp.com/a"));
        assert!(!allowed(&policy, "http://u:p@127.0.0.1/a"));
        assert!(!allowed(&policy, "ftp://127.0.0.1/a"));
    }
}
