//! [`AnthropicProvider`]: HTTP requests and responses.

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use giles_types::{CompletionEvent, CompletionRequest, Error, ModelInfo, Provider, Result};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue};
use reqwest::{RequestBuilder, Response};
use serde::Deserialize;

use super::config::AnthropicConfig;
use super::models::{ModelsPage, model_info};
use super::stream::{ApiError, StreamMapper};
use super::{request, stream};
use crate::providers::sse::{self, SseEvent};
use crate::providers::{MediaSource, http};

/// Models requested per `/v1/models` page, the API's maximum.
const MODELS_PAGE_SIZE: &str = "1000";

/// A [`Provider`] for the Anthropic Messages API.
pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: reqwest::Client,
    /// `x-api-key` (marked sensitive), `anthropic-version`, and `anthropic-beta`, built once.
    headers: HeaderMap,
    media: Arc<dyn MediaSource>,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// A provider for `config` that reads attachment bytes from `media`. Builds the HTTP client
    /// and the request headers; sends nothing. [`Error::InvalidRequest`] if a header value from
    /// the config isn't valid (the message never includes the key).
    pub fn new(config: AnthropicConfig, media: Arc<dyn MediaSource>) -> Result<Self> {
        let invalid = |what: &str| {
            Error::InvalidRequest(format!("anthropic provider config: invalid {what} header"))
        };
        let mut api_key =
            HeaderValue::from_str(config.api_key.expose()).map_err(|_| invalid("x-api-key"))?;
        // Hidden from `Debug` and kept out of HTTP/2 header compression tables.
        api_key.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", api_key);
        headers.insert(
            "anthropic-version",
            HeaderValue::from_str(&config.anthropic_version)
                .map_err(|_| invalid("anthropic-version"))?,
        );
        if !config.betas.is_empty() {
            headers.insert(
                "anthropic-beta",
                HeaderValue::from_str(&config.betas.join(","))
                    .map_err(|_| invalid("anthropic-beta"))?,
            );
        }
        let client = http::client(&config.http)?;
        Ok(Self {
            config,
            client,
            headers,
            media,
        })
    }

    /// A request with the authentication, version, and beta headers.
    fn request(&self, builder: RequestBuilder) -> RequestBuilder {
        builder.headers(self.headers.clone())
    }

    /// Sends `builder` and returns the response if it succeeded, else the mapped error.
    async fn send(builder: RequestBuilder) -> Result<Response> {
        let response = builder.send().await.map_err(http::transport_error)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let retry_after = http::retry_after(response.headers());
        let body = http::error_body(response).await;
        let message = match serde_json::from_str::<ErrorBody>(&body) {
            Ok(ErrorBody { error }) => {
                format!("anthropic {status} {}: {}", error.kind, error.message)
            }
            Err(_) => format!("anthropic {status}: {}", body.trim()),
        };
        Err(http::status_error(status, retry_after, message))
    }
}

/// An error response body: `{"type": "error", "error": {"type": ..., "message": ...}}`.
#[derive(Deserialize)]
struct ErrorBody {
    error: ApiError,
}

#[async_trait]
impl Provider for AnthropicProvider {
    /// All pages of `GET /v1/models`. Limits and capabilities come from the response, falling back
    /// to the built-in family table where the API leaves them out.
    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let mut models = Vec::new();
        let mut after_id: Option<String> = None;
        loop {
            let mut url =
                reqwest::Url::parse(&self.config.url("/v1/models")).map_err(Error::other)?;
            url.query_pairs_mut().append_pair("limit", MODELS_PAGE_SIZE);
            if let Some(after_id) = &after_id {
                url.query_pairs_mut().append_pair("after_id", after_id);
            }
            let response = Self::send(self.request(self.client.get(url))).await?;
            let body = response.bytes().await.map_err(http::transport_error)?;
            let page: ModelsPage = serde_json::from_slice(&body).map_err(|error| {
                Error::other(format!("anthropic: malformed model list: {error}"))
            })?;
            models.extend(page.data.into_iter().map(model_info));
            match page.last_id {
                Some(last_id) if page.has_more && after_id.as_ref() != Some(&last_id) => {
                    after_id = Some(last_id);
                }
                _ => return Ok(models),
            }
        }
    }

    /// `POST /v1/messages` with `stream: true`. Errors before the first event (building the
    /// request, connecting, an error status) are returned here; later ones are stream items.
    async fn complete(
        &self,
        req: CompletionRequest,
    ) -> Result<BoxStream<'static, Result<CompletionEvent>>> {
        let body = request::body(req, &self.config, self.media.as_ref()).await?;
        let body = serde_json::to_vec(&body).map_err(Error::other)?;
        let builder = self
            .request(self.client.post(self.config.url("/v1/messages")))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "text/event-stream")
            .body(body);
        let response = Self::send(builder).await?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !content_type.starts_with("text/event-stream") {
            return Err(Error::other(format!(
                "anthropic: expected an event stream, got content type {content_type:?}"
            )));
        }
        Ok(completion_events(sse::events(http::body_stream(response))).boxed())
    }
}

/// Maps a stream of SSE events to completion events. It ends after `Stop` without reading the
/// rest of the body, or after the first error; a body that ends before `message_stop` yields a
/// retryable error.
fn completion_events<S>(events: S) -> impl Stream<Item = Result<CompletionEvent>> + Send
where
    S: Stream<Item = Result<SseEvent>> + Send,
{
    struct State<S> {
        events: S,
        mapper: StreamMapper,
        ready: VecDeque<Result<CompletionEvent>>,
        done: bool,
    }
    let state = State {
        events: Box::pin(events),
        mapper: StreamMapper::new(),
        ready: VecDeque::new(),
        done: false,
    };
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.ready.pop_front() {
                return Some((item, state));
            }
            if state.done {
                return None;
            }
            match state.events.next().await {
                Some(Ok(event)) => match state.mapper.event(&event) {
                    Ok(events) => {
                        state.ready.extend(events.into_iter().map(Ok));
                        state.done = state.mapper.is_finished();
                    }
                    Err(error) => {
                        state.done = true;
                        state.ready.push_back(Err(error));
                    }
                },
                Some(Err(error)) => {
                    state.done = true;
                    state.ready.push_back(Err(error));
                }
                None => {
                    state.done = true;
                    state.ready.push_back(Err(stream::ended_early()));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use giles_types::MediaRef;

    use super::*;
    use crate::providers::anthropic::ApiKey;

    struct NoMedia;

    #[async_trait]
    impl MediaSource for NoMedia {
        async fn read(&self, media: &MediaRef) -> Result<Bytes> {
            Err(Error::NotFound(media.id.to_string()))
        }
    }

    fn provider(key: &str) -> Result<AnthropicProvider> {
        AnthropicProvider::new(AnthropicConfig::new(ApiKey::new(key)), Arc::new(NoMedia))
    }

    #[test]
    fn api_key_header_is_sensitive() {
        let provider = provider("sk-ant-very-secret").unwrap();
        let key = &provider.headers["x-api-key"];
        assert!(key.is_sensitive());
        assert_eq!(key, "sk-ant-very-secret");
        let debug = format!("{:?} {provider:?}", provider.headers);
        assert!(!debug.contains("very-secret"), "{debug}");
        assert_eq!(provider.headers["anthropic-version"], "2023-06-01");
        assert!(!provider.headers.contains_key("anthropic-beta"));
    }

    #[test]
    fn invalid_api_key_fails_at_construction_without_showing_it() {
        let error = provider("sk-ant-very-secret\n").unwrap_err();
        assert!(matches!(&error, Error::InvalidRequest(m) if m.contains("x-api-key")));
        assert!(!error.to_string().contains("very-secret"));
    }
}
