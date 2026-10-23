//! [`OpenAiProvider`]: HTTP requests and responses.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use giles_types::{CompletionEvent, CompletionRequest, Error, ModelInfo, Provider, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{RequestBuilder, Response};
use serde::Deserialize;

use super::config::OpenAiConfig;
use super::models::{ModelList, model_info};
use super::request;
use super::stream::{ApiError, StreamMapper};
use crate::providers::{MediaSource, http, sse, wire};

/// A [`Provider`] for an OpenAI-compatible Chat Completions API: OpenAI, OpenRouter, Ollama, and
/// other compatible servers.
pub struct OpenAiProvider {
    config: OpenAiConfig,
    client: reqwest::Client,
    /// `Authorization` (marked sensitive) and the configured extra headers, built once.
    headers: HeaderMap,
    media: Arc<dyn MediaSource>,
}

impl std::fmt::Debug for OpenAiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OpenAiProvider {
    /// A provider for `config` that reads attachment bytes from `media`. Builds the HTTP client
    /// and the request headers; sends nothing. [`Error::InvalidRequest`] if the API key isn't a
    /// valid header value (the message never includes it).
    pub fn new(config: OpenAiConfig, media: Arc<dyn MediaSource>) -> Result<Self> {
        let mut headers = config.headers.clone();
        if let Some(api_key) = &config.api_key {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", api_key.expose()))
                .map_err(|_| {
                    Error::InvalidRequest(
                        "openai provider config: invalid authorization header".into(),
                    )
                })?;
            // Hidden from `Debug` and kept out of HTTP/2 header compression tables.
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
        let client = http::client(&config.http)?;
        Ok(Self {
            config,
            client,
            headers,
            media,
        })
    }

    /// A request with the authorization and extra headers.
    fn request(&self, builder: RequestBuilder) -> RequestBuilder {
        builder.headers(self.headers.clone())
    }

    /// Sends `builder` and returns the response if it succeeded, else the mapped error. A server
    /// that echoes the API key in its error message (some gateways do) has it redacted.
    async fn send(&self, builder: RequestBuilder) -> Result<Response> {
        let response = builder.send().await.map_err(http::transport_error)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = http::retry_after(response.headers());
        let body = http::error_body(response).await;
        let mut message = match serde_json::from_str::<ErrorBody>(&body) {
            Ok(ErrorBody { error }) => match error.kind() {
                Some(kind) => format!("openai {status} {kind}: {}", error.message()),
                None => format!("openai {status}: {}", error.message()),
            },
            Err(_) => format!("openai {status}: {}", body.trim()),
        };
        if let Some(key) = &self.config.api_key {
            message = message.replace(key.expose(), "<redacted>");
        }
        Err(http::status_error(status, retry_after, message))
    }
}

/// An error response body: `{"error": {"message": ..., "type": ..., "code": ...}}`.
#[derive(Deserialize)]
struct ErrorBody {
    error: ApiError,
}

#[async_trait]
impl Provider for OpenAiProvider {
    /// `GET /models`. The endpoint reports little beyond ids, so capabilities come mostly from
    /// the config (see [`OpenAiConfig`]).
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let response = self
            .send(self.request(self.client.get(self.config.url("/models"))))
            .await?;
        let body = response.bytes().await.map_err(http::transport_error)?;
        let list: ModelList = serde_json::from_slice(&body)
            .map_err(|error| Error::other(format!("openai: malformed model list: {error}")))?;
        Ok(list
            .data
            .into_iter()
            .map(|model| model_info(model, &self.config))
            .collect())
    }

    /// `POST /chat/completions` with `stream: true`. Errors before the first event (building the
    /// request, connecting, an error status) are returned here; later ones are stream items.
    async fn complete(
        &self,
        req: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<CompletionEvent>>> {
        let body = request::body(req, &self.config, self.media.as_ref()).await?;
        let body = serde_json::to_vec(&body).map_err(Error::other)?;
        let builder = self
            .request(self.client.post(self.config.url("/chat/completions")))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .body(body);
        let response = self.send(builder).await?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("text/event-stream") {
            return Err(Error::other(format!(
                "openai: expected an event stream, got content type {content_type:?}"
            )));
        }
        let events = sse::events(http::body_stream(response));
        Ok(wire::completion_events(events, StreamMapper::new()).boxed())
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use giles_types::MediaRef;

    use super::*;
    use crate::providers::http::ApiKey;

    struct NoMedia;

    #[async_trait]
    impl MediaSource for NoMedia {
        async fn read(&self, media: &MediaRef) -> Result<Bytes> {
            Err(Error::NotFound(media.id.to_string()))
        }
    }

    fn provider(config: OpenAiConfig) -> Result<OpenAiProvider> {
        OpenAiProvider::new(config, Arc::new(NoMedia))
    }

    #[test]
    fn authorization_header_is_sensitive() {
        let mut config = OpenAiConfig::new(Some(ApiKey::new("sk-very-secret")));
        let mut title = HeaderValue::from_static("Giles");
        title.set_sensitive(true);
        config.headers.insert("x-title", title);
        let provider = provider(config).unwrap();
        let key = &provider.headers[AUTHORIZATION];
        assert!(key.is_sensitive());
        assert_eq!(key, "Bearer sk-very-secret");
        assert_eq!(provider.headers["x-title"], "Giles");
        let debug = format!("{:?} {provider:?}", provider.headers);
        assert!(!debug.contains("very-secret"), "{debug}");
    }

    #[test]
    fn no_api_key_sends_no_authorization() {
        let provider = provider(OpenAiConfig::new(None)).unwrap();
        assert!(provider.headers.is_empty());
    }

    #[test]
    fn invalid_api_key_fails_at_construction_without_showing_it() {
        let error = provider(OpenAiConfig::new(Some(ApiKey::new("sk-very-secret\n")))).unwrap_err();
        assert!(matches!(&error, Error::InvalidRequest(m) if m.contains("authorization")));
        assert!(!error.to_string().contains("very-secret"));
    }
}
