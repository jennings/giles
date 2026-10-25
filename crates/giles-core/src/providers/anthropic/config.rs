//! The Anthropic provider's configuration: the instance's `config` table (PLAN section 11.2).

use std::fmt;

use giles_types::Error;
use reqwest::header::HeaderValue;
use serde_json::Value;

use crate::config::KeyPath;
use crate::providers::http::{self, ApiKey, HttpOptions};

/// The API endpoint used when `base_url` is not set.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The `anthropic-version` header sent when `anthropic_version` is not set.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Settings of one Anthropic provider instance.
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicConfig {
    /// Sent as `x-api-key`.
    pub api_key: ApiKey,
    /// The API root, without `/v1` and without a trailing slash, e.g. `https://api.anthropic.com`.
    pub base_url: String,
    /// Sent as `anthropic-version`. From `2026-10-21` on, the API rejects `temperature`, so the
    /// provider leaves it out.
    pub anthropic_version: String,
    /// Sent comma-separated as `anthropic-beta` when not empty.
    pub betas: Vec<String>,
    /// The output limit used when a request doesn't set one, before the model's own maximum.
    pub max_tokens: Option<u32>,
    /// Connection and read timeouts.
    pub http: HttpOptions,
    /// Keys of the config table that this provider doesn't know: ignored, and reported by the
    /// caller. Empty unless [`from_json`](Self::from_json) found some.
    pub unknown_keys: Vec<KeyPath>,
}

impl AnthropicConfig {
    /// A config with `api_key` and every other setting at its default.
    pub fn new(api_key: ApiKey) -> Self {
        Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.to_owned(),
            betas: Vec::new(),
            max_tokens: None,
            http: HttpOptions::default(),
            unknown_keys: Vec::new(),
        }
    }

    /// Validates an instance's `config` table, as JSON with secret references already resolved
    /// (PLAN section 7.7). Keys: `api_key` (required), `base_url`, `anthropic_version`, `beta`,
    /// `max_tokens`. Unknown keys are not errors: they are ignored and listed in
    /// [`unknown_keys`](Self::unknown_keys). A known key with a bad value is an error.
    ///
    /// Errors are [`Error::InvalidRequest`] naming the key; they never include the API key.
    pub fn from_json(config: &Value) -> Result<Self, Error> {
        let Value::Object(table) = config else {
            return Err(invalid("the provider config must be a table"));
        };
        let unknown_keys = table
            .keys()
            .filter(|key| !KEYS.contains(&key.as_str()))
            .map(|key| KeyPath::root().key(key.as_str()))
            .collect();

        let api_key = match table.get("api_key") {
            Some(Value::String(key)) if key.is_empty() => {
                return Err(invalid("`api_key` must not be empty"));
            }
            Some(Value::String(key)) => {
                if HeaderValue::from_str(key).is_err() {
                    return Err(invalid(
                        "`api_key` must be printable ASCII with no line breaks",
                    ));
                }
                ApiKey::new(key.as_str())
            }
            Some(_) => return Err(invalid("`api_key` must be a string")),
            None => return Err(invalid("`api_key` is required")),
        };
        let mut config = Self::new(api_key);
        config.unknown_keys = unknown_keys;

        if let Some(value) = table.get("base_url") {
            config.base_url = http::base_url(string(value, "base_url")?).map_err(invalid)?;
        }
        if let Some(value) = table.get("anthropic_version") {
            config.anthropic_version = header_token(value, "anthropic_version")?;
        }
        if let Some(value) = table.get("beta") {
            let Value::Array(items) = value else {
                return Err(invalid("`beta` must be an array of strings"));
            };
            config.betas = items
                .iter()
                .map(|item| header_token(item, "beta"))
                .collect::<Result<_, _>>()?;
        }
        if let Some(value) = table.get("max_tokens") {
            let max_tokens = value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|&n| n > 0);
            let Some(max_tokens) = max_tokens else {
                return Err(invalid("`max_tokens` must be a positive integer"));
            };
            config.max_tokens = Some(max_tokens);
        }
        Ok(config)
    }

    /// The URL of an API path such as `/v1/messages`.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

/// The keys `from_json` accepts.
const KEYS: [&str; 5] = [
    "api_key",
    "anthropic_version",
    "base_url",
    "beta",
    "max_tokens",
];

fn invalid(message: impl fmt::Display) -> Error {
    Error::InvalidRequest(format!("anthropic provider config: {message}"))
}

fn string<'v>(value: &'v Value, key: &str) -> Result<&'v str, Error> {
    value
        .as_str()
        .ok_or_else(|| invalid(format!("`{key}` must be a string")))
}

/// A non-empty header value with no commas or whitespace, as versions and beta names are.
fn header_token(value: &Value, key: &str) -> Result<String, Error> {
    let text = string(value, key)?;
    let valid = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b',' && b != b'"');
    if !valid {
        return Err(invalid(format!(
            "`{key}` values must be non-empty, without spaces or commas"
        )));
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn error(config: Value) -> String {
        match AnthropicConfig::from_json(&config) {
            Err(Error::InvalidRequest(message)) => message,
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn minimal_config_takes_defaults() {
        let config = AnthropicConfig::from_json(&json!({"api_key": "sk-ant-1"})).unwrap();
        assert_eq!(config, AnthropicConfig::new(ApiKey::new("sk-ant-1")));
        assert_eq!(config.base_url, "https://api.anthropic.com");
        assert_eq!(config.anthropic_version, "2023-06-01");
        assert_eq!(
            config.url("/v1/messages"),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn full_config() {
        let config = AnthropicConfig::from_json(&json!({
            "api_key": "sk-ant-1",
            "base_url": "https://gateway.example/anthropic/",
            "anthropic_version": "2026-10-21",
            "beta": ["files-api-2025-04-14", "context-1m-2025-08-07"],
            "max_tokens": 8192,
        }))
        .unwrap();
        assert_eq!(config.base_url, "https://gateway.example/anthropic");
        assert_eq!(
            config.url("/v1/models"),
            "https://gateway.example/anthropic/v1/models"
        );
        assert_eq!(config.anthropic_version, "2026-10-21");
        assert_eq!(
            config.betas,
            ["files-api-2025-04-14", "context-1m-2025-08-07"]
        );
        assert_eq!(config.max_tokens, Some(8192));
    }

    #[test]
    fn plain_http_base_url_is_allowed_for_local_proxies() {
        let config = AnthropicConfig::from_json(
            &json!({"api_key": "k", "base_url": "http://127.0.0.1:8080"}),
        )
        .unwrap();
        assert_eq!(config.base_url, "http://127.0.0.1:8080");
    }

    #[test]
    fn rejections() {
        assert!(error(json!("sk")).contains("must be a table"));
        assert!(error(json!({})).contains("`api_key` is required"));
        assert!(error(json!({"api_key": ""})).contains("must not be empty"));
        assert!(error(json!({"api_key": 5})).contains("must be a string"));
        for url in [
            "api.anthropic.com",
            "ftp://x",
            "https://u:p@x",
            "https://x?a=1",
            "https://x#f",
        ] {
            assert!(
                error(json!({"api_key": "k", "base_url": url})).contains("`base_url`"),
                "{url}"
            );
        }
        assert!(error(json!({"api_key": "k", "beta": "one"})).contains("array"));
        assert!(error(json!({"api_key": "k", "beta": ["a,b"]})).contains("`beta`"));
        assert!(error(json!({"api_key": "k", "beta": [""]})).contains("`beta`"));
        assert!(error(json!({"api_key": "k", "anthropic_version": "2023 06"})).contains("version"));
        for max in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("10"),
            json!(5_000_000_000_u64),
        ] {
            assert!(error(json!({"api_key": "k", "max_tokens": max})).contains("`max_tokens`"));
        }
    }

    #[test]
    fn unknown_keys_are_listed_not_rejected() {
        let config = AnthropicConfig::from_json(
            &json!({"api_key": "k", "model": "x", "max_tokens": 5, "beta": [], "base_urls": 1}),
        )
        .unwrap();
        let mut keys: Vec<String> = config
            .unknown_keys
            .iter()
            .map(ToString::to_string)
            .collect();
        keys.sort();
        assert_eq!(keys, ["base_urls", "model"]);
        assert_eq!(config.max_tokens, Some(5));
        // A known key with a bad value is still an error, whatever else is unknown.
        let message = error(json!({"api_key": "k", "model": "x", "max_tokens": "5", "zzz": 1}));
        assert!(message.contains("`max_tokens`"), "{message}");
    }

    #[test]
    fn api_key_never_shows() {
        let secret = "sk-ant-very-secret\n";
        let message = error(json!({"api_key": secret}));
        assert!(!message.contains("very-secret"), "{message}");
        let config = AnthropicConfig::new(ApiKey::new("sk-ant-very-secret"));
        let debug = format!("{config:?}");
        assert!(!debug.contains("very-secret"), "{debug}");
        assert!(debug.contains("ApiKey(<redacted>)"));
    }
}
