//! The OpenAI-compatible provider's configuration: the instance's `config` table (PLAN section
//! 11.2).

use std::collections::BTreeMap;
use std::fmt;

use giles_types::Error;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value};

use crate::providers::http::{self, ApiKey, HttpOptions};
use crate::providers::wire;

/// The API endpoint used when `base_url` is not set.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// The image types assumed for OpenAI and OpenRouter models when the config declares none.
pub const DEFAULT_IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

/// Which server behind the Chat Completions API: the few request fields that differ between them
/// follow it. Taken from the `base_url` host unless the config sets `dialect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Dialect {
    /// OpenAI itself (`api.openai.com`): `max_completion_tokens`, `reasoning_effort`.
    OpenAi,
    /// OpenRouter (`openrouter.ai`): `reasoning: {max_tokens}` and a top-level `cache_control`.
    OpenRouter,
    /// Any other compatible server (Ollama, vLLM, llama.cpp, gateways): only widely supported
    /// fields.
    Generic,
}

impl Dialect {
    /// The dialect of a server by its URL's host.
    pub fn from_base_url(base_url: &str) -> Self {
        let host = reqwest::Url::parse(base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
        match host.as_deref() {
            Some("api.openai.com") => Self::OpenAi,
            Some("openrouter.ai") => Self::OpenRouter,
            _ => Self::Generic,
        }
    }

    /// The media types assumed when the config declares none: images for OpenAI and OpenRouter,
    /// whose current models take them; nothing for other servers, where vision support varies
    /// by model and a wrong guess would make every later request fail.
    pub fn default_media_types(self) -> Vec<String> {
        match self {
            Self::OpenAi | Self::OpenRouter => {
                DEFAULT_IMAGE_TYPES.iter().map(|&t| t.to_owned()).collect()
            }
            Self::Generic => Vec::new(),
        }
    }
}

/// What the provider needs to know about a model that the API doesn't report. Every field is
/// optional: a model's own settings win over the instance's, field by field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelSettings {
    /// Media types the model accepts as input content. Each is an image type, `audio/wav`,
    /// `audio/mpeg`, or `application/pdf`, lowercased.
    pub media_types: Option<Vec<String>>,
    /// The context window in tokens.
    pub context_window: Option<u32>,
    /// The output limit sent when a request doesn't set one.
    pub max_tokens: Option<u32>,
    /// Whether the model reasons: reasoning parameters are sent only to models that do (except on
    /// OpenRouter, which ignores them elsewhere).
    pub reasoning: Option<bool>,
}

impl ModelSettings {
    /// These settings, with unset fields taken from `fallback`.
    pub fn or(&self, fallback: &ModelSettings) -> ModelSettings {
        ModelSettings {
            media_types: self
                .media_types
                .clone()
                .or_else(|| fallback.media_types.clone()),
            context_window: self.context_window.or(fallback.context_window),
            max_tokens: self.max_tokens.or(fallback.max_tokens),
            reasoning: self.reasoning.or(fallback.reasoning),
        }
    }
}

/// Settings of one OpenAI-compatible provider instance.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiConfig {
    /// Sent as `Authorization: Bearer <key>`. Local servers such as Ollama need none.
    pub api_key: Option<ApiKey>,
    /// The API root including the version path and without a trailing slash, e.g.
    /// `https://api.openai.com/v1`.
    pub base_url: String,
    /// Which server this is, for the request fields that differ.
    pub dialect: Dialect,
    /// Extra headers sent with every request, e.g. OpenRouter's `HTTP-Referer` and `X-Title`.
    /// Values are marked sensitive, so `Debug` doesn't show them.
    pub headers: HeaderMap,
    /// Settings for every model of the instance.
    pub defaults: ModelSettings,
    /// Settings for single models by id, over [`OpenAiConfig::defaults`].
    pub models: BTreeMap<String, ModelSettings>,
    /// Connection and read timeouts.
    pub http: HttpOptions,
}

impl OpenAiConfig {
    /// A config for OpenAI with `api_key` and every other setting at its default.
    pub fn new(api_key: Option<ApiKey>) -> Self {
        Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_owned(),
            dialect: Dialect::OpenAi,
            headers: HeaderMap::new(),
            defaults: ModelSettings::default(),
            models: BTreeMap::new(),
            http: HttpOptions::default(),
        }
    }

    /// Validates an instance's `config` table, as JSON with secret references already resolved
    /// (PLAN section 7.7). Keys: `api_key`, `base_url`, `dialect`, `headers`, `max_tokens`,
    /// `media_types`, `context_window`, `reasoning`, and `models` (a table of model ids to
    /// `max_tokens`, `media_types`, `context_window`, `reasoning`). Unknown keys are rejected so a
    /// typo doesn't silently do nothing.
    ///
    /// Errors are [`Error::InvalidRequest`] naming the key; they never include the API key or a
    /// header value.
    pub fn from_json(config: &Value) -> Result<Self, Error> {
        let Value::Object(table) = config else {
            return Err(invalid("the provider config must be a table"));
        };
        check_keys(table, &KEYS, "")?;

        let api_key = match table.get("api_key") {
            Some(Value::String(key)) if key.is_empty() => {
                return Err(invalid("`api_key` must not be empty"));
            }
            Some(Value::String(key)) => {
                if HeaderValue::from_str(&format!("Bearer {key}")).is_err() {
                    return Err(invalid(
                        "`api_key` must be printable ASCII with no line breaks",
                    ));
                }
                Some(ApiKey::new(key.as_str()))
            }
            Some(_) => return Err(invalid("`api_key` must be a string")),
            None => None,
        };
        let mut config = Self::new(api_key);

        if let Some(value) = table.get("base_url") {
            config.base_url = http::base_url(string(value, "base_url")?).map_err(invalid)?;
        }
        config.dialect = match table.get("dialect") {
            Some(value) => match string(value, "dialect")? {
                "openai" => Dialect::OpenAi,
                "openrouter" => Dialect::OpenRouter,
                "generic" => Dialect::Generic,
                _ => {
                    return Err(invalid(
                        "`dialect` must be \"openai\", \"openrouter\", or \"generic\"",
                    ));
                }
            },
            None => Dialect::from_base_url(&config.base_url),
        };
        if let Some(value) = table.get("headers") {
            config.headers = headers(value)?;
        }
        config.defaults = settings(table, "")?;
        if let Some(value) = table.get("models") {
            let Value::Object(models) = value else {
                return Err(invalid("`models` must be a table of model ids"));
            };
            for (id, model) in models {
                let path = format!("models.{id}.");
                let Value::Object(model) = model else {
                    return Err(invalid(format!("`models.{id}` must be a table")));
                };
                if id.is_empty() {
                    return Err(invalid("`models` keys must not be empty"));
                }
                check_keys(model, &MODEL_KEYS, &path)?;
                config.models.insert(id.clone(), settings(model, &path)?);
            }
        }
        Ok(config)
    }

    /// The URL of an API path such as `/chat/completions`.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// The settings of `model`: its own, then the instance's.
    pub fn settings(&self, model: &str) -> ModelSettings {
        match self.models.get(model) {
            Some(own) => own.or(&self.defaults),
            None => self.defaults.clone(),
        }
    }

    /// The media types `model` is sent as content: its declared ones, else the dialect's default.
    pub fn media_types(&self, model: &str) -> Vec<String> {
        self.settings(model)
            .media_types
            .unwrap_or_else(|| self.dialect.default_media_types())
    }
}

/// The keys `from_json` accepts at the top level.
const KEYS: [&str; 9] = [
    "api_key",
    "base_url",
    "context_window",
    "dialect",
    "headers",
    "max_tokens",
    "media_types",
    "models",
    "reasoning",
];

/// The keys of a `models.<id>` table.
const MODEL_KEYS: [&str; 4] = ["context_window", "max_tokens", "media_types", "reasoning"];

/// Headers the provider sets itself, or that only the HTTP client may set.
const RESERVED_HEADERS: [&str; 7] = [
    "authorization",
    "accept",
    "connection",
    "content-length",
    "content-type",
    "host",
    "transfer-encoding",
];

fn invalid(message: impl fmt::Display) -> Error {
    Error::InvalidRequest(format!("openai provider config: {message}"))
}

fn check_keys(table: &Map<String, Value>, keys: &[&str], path: &str) -> Result<(), Error> {
    match table.keys().find(|key| !keys.contains(&key.as_str())) {
        Some(key) => Err(invalid(format!(
            "unknown key `{path}{key}` (expected one of: {})",
            keys.join(", ")
        ))),
        None => Ok(()),
    }
}

fn string<'v>(value: &'v Value, key: &str) -> Result<&'v str, Error> {
    value
        .as_str()
        .ok_or_else(|| invalid(format!("`{key}` must be a string")))
}

/// The model settings keys of `table`; `path` prefixes key names in errors.
fn settings(table: &Map<String, Value>, path: &str) -> Result<ModelSettings, Error> {
    let positive = |key: &str| -> Result<Option<u32>, Error> {
        let Some(value) = table.get(key) else {
            return Ok(None);
        };
        value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|&n| n > 0)
            .map(Some)
            .ok_or_else(|| invalid(format!("`{path}{key}` must be a positive integer")))
    };
    let reasoning = match table.get("reasoning") {
        None => None,
        Some(Value::Bool(reasoning)) => Some(*reasoning),
        Some(_) => return Err(invalid(format!("`{path}reasoning` must be true or false"))),
    };
    let media_types = match table.get("media_types") {
        None => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|item| media_type(item, path))
                .collect::<Result<_, _>>()?,
        ),
        Some(_) => {
            return Err(invalid(format!(
                "`{path}media_types` must be an array of strings"
            )));
        }
    };
    Ok(ModelSettings {
        media_types,
        context_window: positive("context_window")?,
        max_tokens: positive("max_tokens")?,
        reasoning,
    })
}

/// A declared media type, lowercased without parameters. Only types the Chat Completions API has
/// a content part for are allowed: images (`image_url`), WAV and MP3 audio (`input_audio`), and
/// PDFs (`file`).
fn media_type(value: &Value, path: &str) -> Result<String, Error> {
    let key = format!("{path}media_types");
    let essence = wire::media_essence(string(value, &key)?);
    let image = essence
        .strip_prefix("image/")
        .is_some_and(|subtype| !subtype.is_empty() && subtype.bytes().all(is_token_byte));
    if image
        || matches!(
            essence.as_str(),
            "audio/wav" | "audio/mpeg" | "application/pdf"
        )
    {
        Ok(essence)
    } else {
        Err(invalid(format!(
            "`{key}`: can't send {essence:?} as content (supported: image types, audio/wav, \
             audio/mpeg, application/pdf)"
        )))
    }
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b)
}

/// The `headers` table: valid names that the provider doesn't set itself, and valid values,
/// marked sensitive. Errors name the header, never its value.
fn headers(value: &Value) -> Result<HeaderMap, Error> {
    let Value::Object(table) = value else {
        return Err(invalid("`headers` must be a table of strings"));
    };
    let mut headers = HeaderMap::new();
    for (name, value) in table {
        let header = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid(format!("`headers`: {name:?} is not a header name")))?;
        if header == "authorization" {
            return Err(invalid(
                "`headers`: set the API key with `api_key`, not an Authorization header",
            ));
        }
        if RESERVED_HEADERS.contains(&header.as_str()) {
            return Err(invalid(format!(
                "`headers`: `{name}` is set by the provider"
            )));
        }
        let Value::String(value) = value else {
            return Err(invalid(format!("`headers.{name}` must be a string")));
        };
        let mut value = HeaderValue::from_str(value).map_err(|_| {
            invalid(format!(
                "`headers.{name}` must be printable ASCII with no line breaks"
            ))
        })?;
        value.set_sensitive(true);
        headers.insert(header, value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn error(config: Value) -> String {
        match OpenAiConfig::from_json(&config) {
            Err(Error::InvalidRequest(message)) => message,
            other => panic!("expected InvalidRequest, got {other:?}"),
        }
    }

    #[test]
    fn minimal_config_is_openai_without_a_key() {
        let config = OpenAiConfig::from_json(&json!({})).unwrap();
        assert_eq!(config, OpenAiConfig::new(None));
        assert_eq!(config.dialect, Dialect::OpenAi);
        assert_eq!(
            config.url("/chat/completions"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(config.media_types("gpt-5.1"), DEFAULT_IMAGE_TYPES);
    }

    #[test]
    fn dialect_follows_the_host_unless_set() {
        let dialect = |config: Value| OpenAiConfig::from_json(&config).unwrap().dialect;
        assert_eq!(
            dialect(json!({"base_url": "https://openrouter.ai/api/v1/"})),
            Dialect::OpenRouter
        );
        assert_eq!(
            dialect(json!({"base_url": "https://API.OpenAI.com/v1"})),
            Dialect::OpenAi
        );
        assert_eq!(
            dialect(json!({"base_url": "http://localhost:11434/v1"})),
            Dialect::Generic
        );
        assert_eq!(
            dialect(json!({"base_url": "https://gateway.example/openai/v1"})),
            Dialect::Generic
        );
        assert_eq!(
            dialect(json!({"base_url": "https://gateway.example/v1", "dialect": "openai"})),
            Dialect::OpenAi
        );
        assert_eq!(dialect(json!({"dialect": "generic"})), Dialect::Generic);
    }

    #[test]
    fn full_config() {
        let config = OpenAiConfig::from_json(&json!({
            "api_key": "sk-or-1",
            "base_url": "https://openrouter.ai/api/v1/",
            "headers": {"HTTP-Referer": "https://giles.example", "X-Title": "Giles"},
            "max_tokens": 8192,
            "media_types": ["image/png", "Application/PDF"],
            "context_window": 128_000,
            "reasoning": true,
            "models": {
                "openai/gpt-4o-mini": {"reasoning": false, "media_types": []},
                "qwen/qwen3-235b": {"context_window": 262_144, "max_tokens": 4096},
            },
        }))
        .unwrap();
        assert_eq!(config.api_key, Some(ApiKey::new("sk-or-1")));
        assert_eq!(config.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(config.dialect, Dialect::OpenRouter);
        assert_eq!(config.headers["http-referer"], "https://giles.example");
        assert_eq!(config.headers["x-title"], "Giles");
        assert!(config.headers["x-title"].is_sensitive());
        assert_eq!(
            config.defaults,
            ModelSettings {
                media_types: Some(vec!["image/png".into(), "application/pdf".into()]),
                context_window: Some(128_000),
                max_tokens: Some(8192),
                reasoning: Some(true),
            }
        );
        assert_eq!(
            config.settings("openai/gpt-4o-mini"),
            ModelSettings {
                media_types: Some(vec![]),
                context_window: Some(128_000),
                max_tokens: Some(8192),
                reasoning: Some(false),
            }
        );
        assert_eq!(
            config.settings("qwen/qwen3-235b"),
            ModelSettings {
                context_window: Some(262_144),
                max_tokens: Some(4096),
                ..config.defaults.clone()
            }
        );
        assert_eq!(config.settings("unlisted"), config.defaults);
        assert_eq!(
            config.media_types("openai/gpt-4o-mini"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn generic_servers_default_to_no_media() {
        let config =
            OpenAiConfig::from_json(&json!({"base_url": "http://127.0.0.1:11434/v1"})).unwrap();
        assert_eq!(config.media_types("llama3.2"), Vec::<String>::new());
        let config = OpenAiConfig::from_json(&json!({
            "base_url": "http://127.0.0.1:11434/v1",
            "models": {"qwen2.5vl:7b": {"media_types": ["image/png", "image/jpeg"]}},
        }))
        .unwrap();
        assert_eq!(
            config.media_types("qwen2.5vl:7b"),
            ["image/png", "image/jpeg"]
        );
        assert_eq!(config.media_types("llama3.2"), Vec::<String>::new());
    }

    #[test]
    fn rejections() {
        assert!(error(json!("sk")).contains("must be a table"));
        assert!(error(json!({"api_key": ""})).contains("must not be empty"));
        assert!(error(json!({"api_key": 5})).contains("must be a string"));
        assert!(error(json!({"model": "x"})).contains("unknown key `model`"));
        assert!(error(json!({"base_url": "localhost:11434"})).contains("`base_url`"));
        assert!(error(json!({"base_url": "https://u:p@x"})).contains("credentials"));
        assert!(error(json!({"dialect": "azure"})).contains("`dialect`"));
        assert!(error(json!({"headers": ["X-Title"]})).contains("`headers`"));
        assert!(error(json!({"headers": {"X Title": "a"}})).contains("header name"));
        assert!(error(json!({"headers": {"Authorization": "Bearer k"}})).contains("`api_key`"));
        assert!(error(json!({"headers": {"Content-Type": "text/plain"}})).contains("set by"));
        assert!(error(json!({"headers": {"X-Title": 5}})).contains("`headers.X-Title`"));
        assert!(error(json!({"media_types": "image/png"})).contains("array"));
        for media_type in ["text/csv", "audio/ogg", "image/", "image", "video/mp4"] {
            assert!(
                error(json!({"media_types": [media_type]})).contains("can't send"),
                "{media_type}"
            );
        }
        assert!(error(json!({"reasoning": "yes"})).contains("`reasoning`"));
        for value in [json!(0), json!(-1), json!(1.5), json!("10")] {
            assert!(error(json!({"max_tokens": value})).contains("`max_tokens`"));
            assert!(error(json!({"context_window": value})).contains("`context_window`"));
        }
        assert!(error(json!({"models": ["gpt-4o"]})).contains("`models`"));
        assert!(error(json!({"models": {"gpt-4o": true}})).contains("`models.gpt-4o`"));
        assert!(
            error(json!({"models": {"gpt-4o": {"api_key": "k"}}}))
                .contains("unknown key `models.gpt-4o.api_key`")
        );
        assert!(
            error(json!({"models": {"gpt-4o": {"max_tokens": 0}}}))
                .contains("`models.gpt-4o.max_tokens`")
        );
        assert!(error(json!({"models": {"": {}}})).contains("must not be empty"));
    }

    /// The TOML shapes in `Docs/user/providers.md` (secret references already resolved).
    #[test]
    fn documented_examples_parse() {
        let table: toml::Table = toml::from_str(
            r#"
            [openrouter]
            base_url = "https://openrouter.ai/api/v1"
            api_key = "sk-or-1"
            headers = { "HTTP-Referer" = "https://giles.example", "X-Title" = "Giles" }

            [ollama]
            base_url = "http://localhost:11434/v1"
            models."qwen2.5vl:7b" = { media_types = ["image/png", "image/jpeg"] }
            models."qwen3:8b" = { reasoning = true, context_window = 40960 }
            "#,
        )
        .unwrap();
        let parse = |name: &str| {
            OpenAiConfig::from_json(&serde_json::to_value(&table[name]).unwrap()).unwrap()
        };
        assert_eq!(parse("openrouter").dialect, Dialect::OpenRouter);
        let ollama = parse("ollama");
        assert_eq!(ollama.dialect, Dialect::Generic);
        assert_eq!(
            ollama.media_types("qwen2.5vl:7b"),
            ["image/png", "image/jpeg"]
        );
        assert_eq!(ollama.settings("qwen3:8b").context_window, Some(40960));
    }

    #[test]
    fn secrets_never_show() {
        let message = error(json!({"api_key": "sk-very-secret\n"}));
        assert!(!message.contains("very-secret"), "{message}");
        let message = error(json!({"headers": {"api-key": "very-secret\n"}}));
        assert!(!message.contains("very-secret"), "{message}");

        let config = OpenAiConfig::from_json(&json!({
            "api_key": "sk-very-secret",
            "headers": {"api-key": "also-very-secret"},
        }))
        .unwrap();
        let debug = format!("{config:?}");
        assert!(!debug.contains("very-secret"), "{debug}");
        assert!(debug.contains("ApiKey(<redacted>)"));
        assert!(debug.contains("api-key"));
    }
}
