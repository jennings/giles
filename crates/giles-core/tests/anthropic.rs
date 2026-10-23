//! The Anthropic provider against a local HTTP server replaying hand-written responses in the
//! documented wire format (`tests/fixtures/anthropic/`). No live calls.
#![cfg(feature = "provider-anthropic")]

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use giles_core::providers::anthropic::{AnthropicConfig, AnthropicProvider};
use giles_core::providers::http::ApiKey;
use giles_types::{
    Block, BlockKind, CacheRetention, CompletionEvent, CompletionRequest, Error, GenerationParams,
    MediaId, MediaRef, Message, ModelInfo, Opaque, Provider, Role, StopReason, ToolSpec,
};
use serde_json::{Value, json};
use support::{
    Media, Reply, Server, block_start, blocks, complete, complete_error, events, message, usage,
};

fn fixture(name: &str) -> Vec<u8> {
    support::fixture("anthropic", name)
}

fn provider_for(server: &Server) -> AnthropicProvider {
    provider_with(server, |_| {})
}

fn provider_with(
    server: &Server,
    configure: impl FnOnce(&mut AnthropicConfig),
) -> AnthropicProvider {
    let mut config = AnthropicConfig::new(ApiKey::new("sk-ant-test"));
    config.base_url = server.url.clone();
    configure(&mut config);
    AnthropicProvider::new(config, Arc::new(Media::default())).unwrap()
}

fn request(model: &str, messages: Vec<Message>) -> CompletionRequest {
    CompletionRequest {
        model: model.into(),
        system: vec![Block::text("You are Jeeves.")],
        messages,
        tools: vec![],
        params: GenerationParams::default(),
    }
}

fn hello() -> Vec<Message> {
    vec![message(Role::User, vec![Block::text("hi")])]
}

fn anthropic(data: Value) -> Opaque {
    Opaque {
        api: "anthropic".into(),
        data,
    }
}

#[tokio::test]
async fn text_completion_request_and_events() {
    let server = Server::start(|_| Reply::sse(fixture("text.sse"))).await;
    let provider = provider_with(&server, |config| {
        config.betas = vec!["beta-one".into(), "beta-two".into()];
    });
    let mut req = request(
        "claude-opus-5-5",
        vec![
            // Two stored inbound messages in a row become one user turn.
            message(Role::User, vec![Block::text("[09:00] hi")]),
            message(Role::User, vec![Block::text("[09:01] are you there?")]),
        ],
    );
    req.system.push(Block::text("Memory: likes tea."));
    req.params.cache_retention = CacheRetention::Long;

    let events = events(&provider, req).await;
    assert_eq!(
        events,
        [
            usage(21, 1, 12000, 1500),
            block_start(0, BlockKind::Text),
            CompletionEvent::TextDelta {
                index: 0,
                text: "Hello".into()
            },
            CompletionEvent::TextDelta {
                index: 0,
                text: ", Stephen.".into()
            },
            CompletionEvent::BlockStop {
                index: 0,
                block: Block::text("Hello, Stephen.")
            },
            usage(21, 7, 12000, 1500),
            CompletionEvent::Stop(StopReason::EndTurn),
        ]
    );

    let [recorded] = server.requests().try_into().unwrap();
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.uri, "/v1/messages");
    assert_eq!(recorded.header("x-api-key"), Some("sk-ant-test"));
    assert_eq!(recorded.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(recorded.header("anthropic-beta"), Some("beta-one,beta-two"));
    assert_eq!(recorded.header("content-type"), Some("application/json"));
    assert_eq!(recorded.header("accept"), Some("text/event-stream"));
    let one_hour = json!({"type": "ephemeral", "ttl": "1h"});
    assert_eq!(
        recorded.json(),
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 128_000,
            "stream": true,
            "cache_control": one_hour,
            "system": [
                {"type": "text", "text": "You are Jeeves."},
                {"type": "text", "text": "Memory: likes tea.", "cache_control": one_hour},
            ],
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "[09:00] hi"},
                {"type": "text", "text": "[09:01] are you there?"},
            ]}],
        })
    );
}

#[tokio::test]
async fn tool_use_streams_partial_json_and_stops_for_tools() {
    let server = Server::start(|_| Reply::sse(fixture("tool_use.sse"))).await;
    let mut req = request("claude-sonnet-4-5-20250929", hello());
    req.tools = vec![ToolSpec {
        name: "weather__get".into(),
        description: "Current weather".into(),
        input_schema: json!({"type": "object", "properties": {"location": {"type": "string"}}}),
    }];
    req.params.reasoning_budget = Some(2048);
    req.params.max_tokens = Some(4096);

    let events = events(&provider_for(&server), req).await;
    let id = "toolu_01T1x1fJ34qAmk2tNTrN7Up6";
    assert_eq!(
        events,
        [
            usage(472, 2, 0, 0),
            block_start(0, BlockKind::Text),
            CompletionEvent::TextDelta {
                index: 0,
                text: "Checking the weather.".into()
            },
            CompletionEvent::BlockStop {
                index: 0,
                block: Block::text("Checking the weather.")
            },
            CompletionEvent::BlockStart {
                index: 1,
                kind: BlockKind::ToolUse,
                tool: Some((id.into(), "weather__get".into())),
            },
            CompletionEvent::ToolInputDelta {
                index: 1,
                partial_json: "{\"location\":".into()
            },
            CompletionEvent::ToolInputDelta {
                index: 1,
                partial_json: " \"San Francisco, CA\", \"unit\"".into()
            },
            CompletionEvent::ToolInputDelta {
                index: 1,
                partial_json: ": \"celsius\"}".into()
            },
            CompletionEvent::BlockStop {
                index: 1,
                block: Block::ToolUse {
                    id: id.into(),
                    name: "weather__get".into(),
                    input: json!({"location": "San Francisco, CA", "unit": "celsius"}),
                },
            },
            usage(472, 89, 0, 0),
            CompletionEvent::Stop(StopReason::ToolUse),
        ]
    );

    let body = server.requests()[0].json();
    assert_eq!(
        body["tools"],
        json!([{"name": "weather__get", "description": "Current weather",
                "input_schema": {"type": "object",
                                 "properties": {"location": {"type": "string"}}}}])
    );
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
}

#[tokio::test]
async fn thinking_and_redacted_thinking_round_trip_unchanged() {
    let server = Server::start(|request| {
        if request.json()["messages"].as_array().unwrap().len() == 1 {
            Reply::sse(fixture("thinking.sse"))
        } else {
            Reply::sse(fixture("text.sse"))
        }
    })
    .await;
    let provider = provider_for(&server);
    let mut req = request("claude-opus-4-7", hello());
    req.params.reasoning_budget = Some(1024);

    let first = events(&provider, req.clone()).await;
    let signature = "EqQBCgIYAhIM1gbcDa9GJwZA2b3hGgxBdjrkzLoky3dl1pkiMOYds";
    let redacted_data = "EmwKAhgBEgy3va3pzix/LafPsn4aDFIT2Xlxh0L5L8rLVyIwxtE3rAFBa8cr3qpPkNR\
                         j2YfWXGmKDxH4mPnZ5sQ7vB9URj2pLmN3kF8/dW5hR7xJ0aP1oLs9yTcMnKVf2wRpEGjH9XZaBt4UvDcPrQ";
    assert_eq!(
        first,
        [
            usage(40, 1, 0, 0),
            block_start(0, BlockKind::Reasoning),
            CompletionEvent::ReasoningDelta {
                index: 0,
                text: "1071 = 2 × 462 + 147".into()
            },
            CompletionEvent::ReasoningDelta {
                index: 0,
                text: "\n462 = 3 × 147 + 21".into()
            },
            CompletionEvent::BlockStop {
                index: 0,
                block: Block::Reasoning {
                    text: Some("1071 = 2 × 462 + 147\n462 = 3 × 147 + 21".into()),
                    opaque: Some(anthropic(json!({"signature": signature}))),
                },
            },
            block_start(1, BlockKind::Opaque),
            CompletionEvent::BlockStop {
                index: 1,
                block: Block::Opaque(anthropic(
                    json!({"type": "redacted_thinking", "data": redacted_data})
                )),
            },
            block_start(2, BlockKind::Text),
            CompletionEvent::TextDelta {
                index: 2,
                text: "The GCD is **21**.".into()
            },
            CompletionEvent::BlockStop {
                index: 2,
                block: Block::text("The GCD is **21**.")
            },
            usage(40, 120, 0, 0),
            CompletionEvent::Stop(StopReason::EndTurn),
        ]
    );

    // The stored blocks, sent back in the next request, reproduce the API's blocks exactly.
    req.messages.push(message(Role::Assistant, blocks(&first)));
    req.messages
        .push(message(Role::User, vec![Block::text("thanks")]));
    events(&provider, req).await;
    let second = server.requests()[1].json();
    assert_eq!(second["thinking"], json!({"type": "adaptive"}));
    assert_eq!(
        second["messages"][1],
        json!({"role": "assistant", "content": [
            {"type": "thinking", "thinking": "1071 = 2 × 462 + 147\n462 = 3 × 147 + 21",
             "signature": signature},
            {"type": "redacted_thinking", "data": redacted_data},
            {"type": "text", "text": "The GCD is **21**."},
        ]})
    );
}

#[tokio::test]
async fn server_tool_blocks_round_trip_and_foreign_opaque_is_not_sent() {
    let server = Server::start(|request| {
        if request.json()["messages"].as_array().unwrap().len() == 1 {
            Reply::sse(fixture("server_tool.sse"))
        } else {
            Reply::sse(fixture("text.sse"))
        }
    })
    .await;
    let provider = provider_for(&server);
    let mut req = request("claude-opus-5-5", hello());
    let first = events(&provider, req.clone()).await;
    // message_delta updates the input count at the end; the last Usage wins.
    assert_eq!(first[first.len() - 2], usage(10682, 510, 0, 0));
    assert_eq!(
        first.last(),
        Some(&CompletionEvent::Stop(StopReason::EndTurn))
    );

    let server_tool_use = json!({"type": "server_tool_use",
        "id": "srvtoolu_014hJH82Qum7Td6UV8gDXThB", "name": "web_search",
        "input": {"query": "weather NYC today"}});
    let result = json!({"type": "web_search_tool_result",
        "tool_use_id": "srvtoolu_014hJH82Qum7Td6UV8gDXThB",
        "content": [{"type": "web_search_result", "title": "Weather in New York City",
                     "url": "https://example.com/nyc",
                     "encrypted_content": "Ev0DCioIAxgCIiQ3NmU4ZmI4OC1k", "page_age": null}]});
    let mut stored = blocks(&first);
    assert_eq!(
        stored,
        [
            Block::Opaque(anthropic(server_tool_use.clone())),
            Block::Opaque(anthropic(result.clone())),
            Block::text("It's sunny."),
        ]
    );

    // A block another provider family produced (after a fallback) is left out.
    stored.push(Block::Opaque(Opaque {
        api: "openai".into(),
        data: json!({"type": "reasoning", "encrypted_content": "x"}),
    }));
    req.messages.push(message(Role::Assistant, stored));
    req.messages
        .push(message(Role::User, vec![Block::text("and tomorrow?")]));
    events(&provider, req).await;
    assert_eq!(
        server.requests()[1].json()["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            {"role": "assistant", "content": [
                server_tool_use, result, {"type": "text", "text": "It's sunny."},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "and tomorrow?"}]},
        ])
    );
}

#[tokio::test]
async fn media_is_sent_inline_from_the_media_source() {
    let server = Server::start(|_| Reply::sse(fixture("text.sse"))).await;
    let png = MediaRef {
        id: MediaId::from_digest([7; 32]),
        media_type: "image/png".into(),
        size: 4,
        name: Some("cat.png".into()),
    };
    let media = Media(HashMap::from([(
        png.id.clone(),
        Bytes::from_static(b"\x89PNG"),
    )]));
    let mut config = AnthropicConfig::new(ApiKey::new("k"));
    config.base_url = server.url.clone();
    let provider = AnthropicProvider::new(config, Arc::new(media)).unwrap();
    let req = request(
        "claude-opus-5-5",
        vec![message(
            Role::User,
            vec![Block::text("what is this?"), Block::Image { media: png }],
        )],
    );
    events(&provider, req).await;
    assert_eq!(
        server.requests()[0].json()["messages"][0]["content"][1],
        json!({"type": "image",
               "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw=="}})
    );
}

#[tokio::test]
async fn http_errors_map_to_error_kinds_with_retry_after() {
    let server = Server::start(|request| {
        let model = request.json()["model"].as_str().unwrap().to_owned();
        let body = |kind: &str, message: &str| {
            json!({"type": "error", "error": {"type": kind, "message": message},
                   "request_id": "req_1"})
            .to_string()
        };
        match model.as_str() {
            "rate-limited" => {
                let mut reply = Reply::json(429, body("rate_limit_error", "Slow down"));
                reply.headers.push(("retry-after", "30"));
                reply
            }
            "overloaded" => Reply::json(529, body("overloaded_error", "Overloaded")),
            "server-error" => Reply::json(500, "<html>oops</html>"),
            "missing" => Reply::json(404, body("not_found_error", "model: missing")),
            "unauthorized" => Reply::json(401, body("authentication_error", "invalid x-api-key")),
            _ => Reply::json(
                400,
                body("invalid_request_error", "messages: at least one message"),
            ),
        }
    })
    .await;
    let provider = provider_for(&server);
    let error = |model: &'static str| {
        let provider = &provider;
        async move {
            match provider.complete(request(model, hello())).await {
                Ok(_) => panic!("{model}: expected an error"),
                Err(error) => error,
            }
        }
    };

    match error("rate-limited").await {
        Error::Unavailable {
            message,
            retry_after,
        } => {
            assert_eq!(retry_after, Some(Duration::from_secs(30)));
            assert_eq!(
                message,
                "anthropic 429 Too Many Requests rate_limit_error: Slow down"
            );
        }
        other => panic!("{other:?}"),
    }
    match error("overloaded").await {
        Error::Unavailable {
            message,
            retry_after: None,
        } => assert!(
            message.contains("overloaded_error: Overloaded"),
            "{message}"
        ),
        other => panic!("{other:?}"),
    }
    match error("server-error").await {
        Error::Unavailable { message, .. } => {
            assert_eq!(
                message,
                "anthropic 500 Internal Server Error: <html>oops</html>"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(error("missing").await, Error::NotFound(m) if m.contains("model: missing")));
    assert!(matches!(
        error("unauthorized").await,
        Error::InvalidRequest(m) if m.contains("authentication_error: invalid x-api-key")
    ));
    assert!(matches!(
        error("bad").await,
        Error::InvalidRequest(m) if m.contains("messages: at least one message")
    ));
}

#[tokio::test]
async fn mid_stream_error_event_ends_the_stream_with_err() {
    let server = Server::start(|_| Reply::sse(fixture("overloaded.sse"))).await;
    let items = complete(&provider_for(&server), request("claude-opus-5-5", hello())).await;
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].as_ref().unwrap(), &usage(9, 1, 0, 0));
    assert_eq!(
        items[2].as_ref().unwrap(),
        &CompletionEvent::TextDelta {
            index: 0,
            text: "Partial".into()
        }
    );
    let error = items[3].as_ref().unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(
        error.to_string(),
        "unavailable: anthropic overloaded_error: Overloaded"
    );
}

#[tokio::test]
async fn stream_cut_off_before_message_stop_is_retryable() {
    let server = Server::start(|_| Reply::sse(fixture("cut_off.sse"))).await;
    let items = complete(&provider_for(&server), request("claude-opus-5-5", hello())).await;
    assert_eq!(items.len(), 4);
    let error = items.last().unwrap().as_ref().unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
    assert!(error.to_string().contains("ended before message_stop"));
}

#[tokio::test]
async fn non_event_stream_success_is_an_error() {
    let server = Server::start(|_| Reply::json(200, "{}")).await;
    let error = complete_error(&provider_for(&server), request("claude-opus-5-5", hello())).await;
    assert!(matches!(error, Error::Other(_)), "{error:?}");
    assert!(error.to_string().contains("expected an event stream"));
}

#[tokio::test]
async fn connection_refused_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let mut config = AnthropicConfig::new(ApiKey::new("k"));
    config.base_url = url;
    let provider = AnthropicProvider::new(config, Arc::new(Media::default())).unwrap();
    let error = complete_error(&provider, request("m", hello())).await;
    assert!(error.is_retryable(), "{error:?}");
    let models_error = provider.list_models().await.unwrap_err();
    assert!(models_error.is_retryable(), "{models_error:?}");
}

#[tokio::test]
async fn list_models_follows_pages_and_fills_in_limits() {
    let server = Server::start(|request| {
        if request.uri.contains("after_id=") {
            Reply::json(200, fixture("models_page2.json"))
        } else {
            Reply::json(200, fixture("models_page1.json"))
        }
    })
    .await;
    let models = provider_for(&server).list_models().await.unwrap();
    let all_media = [
        "image/jpeg",
        "image/png",
        "image/gif",
        "image/webp",
        "application/pdf",
    ]
    .map(String::from)
    .to_vec();
    assert_eq!(
        models,
        [
            ModelInfo {
                id: "claude-opus-5-5".into(),
                display_name: Some("Claude Opus 5.5".into()),
                context_window: Some(1_000_000),
                max_output_tokens: Some(128_000),
                media_types: all_media.clone(),
                reasoning: true,
            },
            // No limits or capabilities listed: the family table fills them in.
            ModelInfo {
                id: "claude-haiku-4-5-20251001".into(),
                display_name: Some("Claude Haiku 4.5".into()),
                context_window: Some(200_000),
                max_output_tokens: Some(64_000),
                media_types: all_media,
                reasoning: true,
            },
        ]
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].uri, "/v1/models?limit=1000");
    assert_eq!(
        requests[1].uri,
        "/v1/models?limit=1000&after_id=claude-opus-5-5"
    );
    assert_eq!(requests[1].header("x-api-key"), Some("sk-ant-test"));
    assert_eq!(requests[1].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(requests[1].header("anthropic-beta"), None);
}

#[tokio::test]
async fn list_models_errors_map_like_completions() {
    let server = Server::start(|_| {
        let mut reply = Reply::json(503, "unavailable");
        reply.headers.push(("retry-after", "2"));
        reply
    })
    .await;
    match provider_for(&server).list_models().await.unwrap_err() {
        Error::Unavailable { retry_after, .. } => {
            assert_eq!(retry_after, Some(Duration::from_secs(2)));
        }
        other => panic!("{other:?}"),
    }
}
