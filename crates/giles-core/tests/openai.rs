//! The OpenAI-compatible provider against a local HTTP server replaying hand-written responses
//! modeled on OpenAI, OpenRouter, and Ollama streams (`tests/fixtures/openai/`). No live calls.
#![cfg(feature = "provider-openai")]

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use giles_core::providers::http::ApiKey;
use giles_core::providers::openai::{OpenAiConfig, OpenAiProvider};
use giles_types::{
    Block, BlockKind, CacheRetention, CompletionEvent, CompletionRequest, Error, GenerationParams,
    MediaId, MediaRef, Message, ModelInfo, Opaque, Provider, Role, StopReason, ToolSpec,
};
use serde_json::{Value, json};
use support::{
    Media, Reply, Server, block_start, blocks, complete, complete_error, events, message, usage,
};

fn fixture(name: &str) -> Vec<u8> {
    support::fixture("openai", name)
}

/// A provider for OpenAI itself (the default dialect) at `<server>/v1`.
fn openai(server: &Server) -> OpenAiProvider {
    let mut config = OpenAiConfig::new(Some(ApiKey::new("sk-test")));
    config.base_url = format!("{}/v1", server.url);
    OpenAiProvider::new(config, Arc::new(Media::default())).unwrap()
}

/// A provider from an instance's `config` table, with `base_url` pointing at `server`.
fn from_config(server: &Server, path: &str, mut config: Value) -> OpenAiProvider {
    config["base_url"] = format!("{}{path}", server.url).into();
    let config = OpenAiConfig::from_json(&config).unwrap();
    OpenAiProvider::new(config, Arc::new(Media::default())).unwrap()
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

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: "weather__get".into(),
        description: "Current weather".into(),
        input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
    }
}

fn text(index: u32, text: &str) -> CompletionEvent {
    CompletionEvent::TextDelta {
        index,
        text: text.into(),
    }
}

fn tool_start(index: u32, id: &str, name: &str) -> CompletionEvent {
    CompletionEvent::BlockStart {
        index,
        kind: BlockKind::ToolUse,
        tool: Some((id.into(), name.into())),
    }
}

fn tool_block(id: &str, name: &str, input: Value) -> Block {
    Block::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

#[tokio::test]
async fn text_completion_request_and_events() {
    let server = Server::start(|_| Reply::sse(fixture("text.sse"))).await;
    let mut config = OpenAiConfig::new(Some(ApiKey::new("sk-test")));
    config.base_url = format!("{}/v1", server.url);
    config.defaults.max_tokens = Some(8192);
    let provider = OpenAiProvider::new(config, Arc::new(Media::default())).unwrap();
    let mut req = request(
        "gpt-5.1",
        vec![
            message(Role::User, vec![Block::text("[09:00] hi")]),
            message(Role::User, vec![Block::text("[09:01] are you there?")]),
        ],
    );
    req.system.push(Block::text("Memory: likes tea."));
    req.params.temperature = Some(0.5);

    assert_eq!(
        events(&provider, req).await,
        [
            block_start(0, BlockKind::Text),
            text(0, "Hello"),
            text(0, ", Stephen. ☕"),
            CompletionEvent::BlockStop {
                index: 0,
                block: Block::text("Hello, Stephen. ☕")
            },
            usage(86, 12, 1920, 0),
            CompletionEvent::Stop(StopReason::EndTurn),
        ]
    );

    let [recorded] = server.requests().try_into().unwrap();
    assert_eq!(recorded.method, "POST");
    assert_eq!(recorded.uri, "/v1/chat/completions");
    assert_eq!(recorded.header("authorization"), Some("Bearer sk-test"));
    assert_eq!(recorded.header("content-type"), Some("application/json"));
    assert_eq!(recorded.header("accept"), Some("text/event-stream"));
    assert_eq!(
        recorded.json(),
        json!({
            "model": "gpt-5.1",
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_completion_tokens": 8192,
            "temperature": 0.5,
            "messages": [
                {"role": "system", "content": "You are Jeeves.\n\nMemory: likes tea."},
                {"role": "user", "content": "[09:00] hi"},
                {"role": "user", "content": "[09:01] are you there?"},
            ],
        })
    );
}

#[tokio::test]
async fn parallel_tool_calls_round_trip_with_image_relocation() {
    let server = Server::start(|request| {
        if request.json()["messages"].as_array().unwrap().len() == 2 {
            Reply::sse(fixture("tool_calls.sse"))
        } else {
            Reply::sse(fixture("text.sse"))
        }
    })
    .await;
    let png = MediaRef {
        id: MediaId::from_digest([7; 32]),
        media_type: "image/png".into(),
        size: 4,
        name: Some("radar.png".into()),
    };
    let media = Media(HashMap::from([(
        png.id.clone(),
        Bytes::from_static(b"\x89PNG"),
    )]));
    let mut config = OpenAiConfig::new(Some(ApiKey::new("sk-test")));
    config.base_url = format!("{}/v1", server.url);
    let provider = OpenAiProvider::new(config, Arc::new(media)).unwrap();
    let mut req = request("gpt-5.1", hello());
    req.tools = vec![weather_tool()];

    let first = events(&provider, req.clone()).await;
    let paris = "call_DdmO9pD3xa9XTPNJ32zg2hcA";
    let oslo = "call_q8Vn2LxRk4TzWm1pYs7dEe3B";
    assert_eq!(
        first,
        [
            block_start(0, BlockKind::Text),
            text(0, "Checking both cities."),
            CompletionEvent::BlockStop {
                index: 0,
                block: Block::text("Checking both cities.")
            },
            tool_start(1, paris, "weather__get"),
            CompletionEvent::ToolInputDelta {
                index: 1,
                partial_json: "{\"ci".into()
            },
            CompletionEvent::ToolInputDelta {
                index: 1,
                partial_json: "ty\": \"Paris\"}".into()
            },
            CompletionEvent::BlockStop {
                index: 1,
                block: tool_block(paris, "weather__get", json!({"city": "Paris"})),
            },
            tool_start(2, oslo, "weather__get"),
            CompletionEvent::ToolInputDelta {
                index: 2,
                partial_json: "{\"city\": \"Oslo\"}".into()
            },
            CompletionEvent::BlockStop {
                index: 2,
                block: tool_block(oslo, "weather__get", json!({"city": "Oslo"})),
            },
            usage(120, 58, 0, 0),
            CompletionEvent::Stop(StopReason::ToolUse),
        ]
    );
    assert_eq!(
        server.requests()[0].json()["tools"],
        json!([{"type": "function", "function": {
            "name": "weather__get", "description": "Current weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
        }}])
    );

    // The core stores the blocks and the results; the next request carries them back.
    req.messages.push(message(Role::Assistant, blocks(&first)));
    req.messages.push(message(
        Role::User,
        vec![
            Block::ToolResult {
                tool_use_id: paris.into(),
                content: vec![
                    Block::text("18°C, rain"),
                    Block::Image { media: png.clone() },
                ],
                is_error: false,
            },
            Block::ToolResult {
                tool_use_id: oslo.into(),
                content: vec![Block::text("service down")],
                is_error: true,
            },
        ],
    ));
    events(&provider, req).await;
    let reference = format!("[media:{} radar.png image/png]", png.id);
    assert_eq!(
        server.requests()[1].json()["messages"],
        json!([
            {"role": "system", "content": "You are Jeeves."},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "Checking both cities.", "tool_calls": [
                {"id": paris, "type": "function",
                 "function": {"name": "weather__get", "arguments": "{\"city\":\"Paris\"}"}},
                {"id": oslo, "type": "function",
                 "function": {"name": "weather__get", "arguments": "{\"city\":\"Oslo\"}"}},
            ]},
            {"role": "tool", "tool_call_id": paris,
             "content": format!("18°C, rain\n\n{reference}")},
            {"role": "tool", "tool_call_id": oslo, "content": "Error: service down"},
            {"role": "user", "content": [
                {"type": "text", "text": reference},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw=="}},
            ]},
        ])
    );
}

#[tokio::test]
async fn openrouter_reasoning_headers_and_caching() {
    let server = Server::start(|_| Reply::sse(fixture("openrouter_reasoning.sse"))).await;
    let provider = from_config(
        &server,
        "/api/v1",
        json!({
            "api_key": "sk-or-test",
            "dialect": "openrouter",
            "headers": {"HTTP-Referer": "https://giles.example", "X-Title": "Giles"},
        }),
    );
    let mut req = request("anthropic/claude-sonnet-4.5", hello());
    req.params.reasoning_budget = Some(2048);
    req.params.max_tokens = Some(1024);
    req.params.cache_retention = CacheRetention::Long;

    let events = events(&provider, req).await;
    assert_eq!(
        events,
        [
            block_start(0, BlockKind::Reasoning),
            CompletionEvent::ReasoningDelta {
                index: 0,
                text: "The user".into()
            },
            CompletionEvent::ReasoningDelta {
                index: 0,
                text: " wants a haiku.".into()
            },
            CompletionEvent::BlockStop {
                index: 0,
                block: Block::Reasoning {
                    text: Some("The user wants a haiku.".into()),
                    opaque: Some(Opaque {
                        api: "openai".into(),
                        data: json!({"field": "reasoning"}),
                    }),
                },
            },
            block_start(1, BlockKind::Text),
            text(1, "Autumn moonlight—"),
            text(1, "\na worm digs silently"),
            CompletionEvent::BlockStop {
                index: 1,
                block: Block::text("Autumn moonlight—\na worm digs silently")
            },
            usage(39, 60, 10000, 300),
            CompletionEvent::Stop(StopReason::EndTurn),
        ]
    );

    let [recorded] = server.requests().try_into().unwrap();
    assert_eq!(recorded.uri, "/api/v1/chat/completions");
    assert_eq!(recorded.header("authorization"), Some("Bearer sk-or-test"));
    assert_eq!(
        recorded.header("http-referer"),
        Some("https://giles.example")
    );
    assert_eq!(recorded.header("x-title"), Some("Giles"));
    assert_eq!(
        recorded.json(),
        json!({
            "model": "anthropic/claude-sonnet-4.5",
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": 1024,
            "reasoning": {"max_tokens": 2048},
            "cache_control": {"type": "ephemeral", "ttl": "1h"},
            "messages": [
                {"role": "system", "content": "You are Jeeves."},
                {"role": "user", "content": "hi"},
            ],
        })
    );
}

#[tokio::test]
async fn ollama_without_a_key_sends_reasoning_back() {
    let server = Server::start(|request| {
        if request.json()["messages"].as_array().unwrap().len() == 2 {
            Reply::sse(fixture("ollama_tool.sse"))
        } else {
            Reply::sse(fixture("text.sse"))
        }
    })
    .await;
    // A local URL is a generic server; no key means no Authorization header.
    let provider = from_config(
        &server,
        "/v1",
        json!({"models": {"qwen3:8b": {"reasoning": true}}}),
    );
    let mut req = request("qwen3:8b", hello());
    req.params.reasoning_budget = Some(16_000);

    let first = events(&provider, req.clone()).await;
    // Both calls come with index 0; each id starts its own call.
    let time = "call_k3j2l1m0";
    let weather = "call_p9q8r7s6";
    assert_eq!(
        blocks(&first),
        [
            Block::Reasoning {
                text: Some("Need the time in Paris.".into()),
                opaque: Some(Opaque {
                    api: "openai".into(),
                    data: json!({"field": "reasoning"}),
                }),
            },
            tool_block(time, "time__now", json!({"zone": "Europe/Paris"})),
            tool_block(weather, "weather__get", json!({"city": "Paris"})),
        ]
    );
    assert_eq!(first[first.len() - 2], usage(180, 25, 0, 0));
    assert_eq!(
        first.last(),
        Some(&CompletionEvent::Stop(StopReason::ToolUse))
    );
    let recorded = &server.requests()[0];
    assert_eq!(recorded.uri, "/v1/chat/completions");
    assert_eq!(recorded.header("authorization"), None);
    assert_eq!(recorded.json()["reasoning_effort"], "high");

    req.messages.push(message(Role::Assistant, blocks(&first)));
    let result = |id: &str, text: &str| Block::ToolResult {
        tool_use_id: id.into(),
        content: vec![Block::text(text)],
        is_error: false,
    };
    req.messages.push(message(
        Role::User,
        vec![result(time, "09:14"), result(weather, "18°C")],
    ));
    events(&provider, req).await;
    assert_eq!(
        server.requests()[1].json()["messages"],
        json!([
            {"role": "system", "content": "You are Jeeves."},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "reasoning": "Need the time in Paris.",
             "tool_calls": [
                {"id": time, "type": "function", "function": {
                    "name": "time__now", "arguments": "{\"zone\":\"Europe/Paris\"}"}},
                {"id": weather, "type": "function", "function": {
                    "name": "weather__get", "arguments": "{\"city\":\"Paris\"}"}},
             ]},
            {"role": "tool", "tool_call_id": time, "content": "09:14"},
            {"role": "tool", "tool_call_id": weather, "content": "18°C"},
        ])
    );
}

#[tokio::test]
async fn http_errors_map_to_error_kinds_with_retry_after() {
    let server = Server::start(|request| {
        let model = request.json()["model"].as_str().unwrap().to_owned();
        let openai = |kind: &str, code: Value, message: &str| {
            json!({"error": {"message": message, "type": kind, "param": null, "code": code}})
                .to_string()
        };
        match model.as_str() {
            "rate-limited" => {
                let mut reply = Reply::json(
                    429,
                    openai(
                        "tokens",
                        "rate_limit_exceeded".into(),
                        "Rate limit reached for gpt-5.1",
                    ),
                );
                reply.headers.push(("retry-after", "20"));
                reply
            }
            "busy" => {
                let mut reply = Reply::json(503, "upstream unavailable");
                reply.headers.push(("retry-after", "1.5"));
                reply
            }
            "server-error" => Reply::json(500, "<html>oops</html>"),
            "missing" => Reply::json(
                404,
                openai(
                    "invalid_request_error",
                    "model_not_found".into(),
                    "The model `missing` does not exist",
                ),
            ),
            "unauthorized" => Reply::json(
                401,
                openai(
                    "invalid_request_error",
                    "invalid_api_key".into(),
                    "Incorrect API key provided: sk-test.",
                ),
            ),
            // OpenRouter: the status as a numeric code, no type.
            "no-credits" => Reply::json(
                402,
                json!({"error": {"code": 402, "message": "Insufficient credits"}}).to_string(),
            ),
            _ => Reply::json(
                400,
                openai(
                    "invalid_request_error",
                    Value::Null,
                    "messages: at least one message",
                ),
            ),
        }
    })
    .await;
    let provider = openai(&server);
    let error = |model: &'static str| complete_error(&provider, request(model, hello()));

    match error("rate-limited").await {
        Error::Unavailable {
            message,
            retry_after,
        } => {
            assert_eq!(retry_after, Some(Duration::from_secs(20)));
            assert_eq!(
                message,
                "openai 429 Too Many Requests rate_limit_exceeded: Rate limit reached for gpt-5.1"
            );
        }
        other => panic!("{other:?}"),
    }
    match error("busy").await {
        Error::Unavailable { retry_after, .. } => {
            assert_eq!(retry_after, Some(Duration::from_millis(1500)));
        }
        other => panic!("{other:?}"),
    }
    match error("server-error").await {
        Error::Unavailable { message, .. } => {
            assert_eq!(
                message,
                "openai 500 Internal Server Error: <html>oops</html>"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        error("missing").await,
        Error::NotFound(m) if m.contains("model_not_found: The model `missing` does not exist")
    ));
    // The key never appears, even when the server echoes it.
    match error("unauthorized").await {
        Error::InvalidRequest(message) => assert_eq!(
            message,
            "openai 401 Unauthorized invalid_api_key: Incorrect API key provided: <redacted>."
        ),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        error("no-credits").await,
        Error::InvalidRequest(m) if m == "openai 402 Payment Required: Insufficient credits"
    ));
    assert!(matches!(
        error("bad").await,
        Error::InvalidRequest(m)
            if m == "openai 400 Bad Request invalid_request_error: messages: at least one message"
    ));
}

#[tokio::test]
async fn mid_stream_error_ends_the_stream_with_err() {
    let server = Server::start(|_| Reply::sse(fixture("upstream_error.sse"))).await;
    let items = complete(&openai(&server), request("openai/gpt-5.1", hello())).await;
    assert_eq!(items.len(), 3);
    assert_eq!(items[1].as_ref().unwrap(), &text(0, "Partial"));
    let error = items[2].as_ref().unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
    assert_eq!(
        error.to_string(),
        "unavailable: openai 502: Upstream provider disconnected"
    );
}

#[tokio::test]
async fn stream_cut_off_before_a_finish_reason_is_retryable() {
    let server = Server::start(|_| Reply::sse(fixture("cut_off.sse"))).await;
    let items = complete(&openai(&server), request("gpt-5.1", hello())).await;
    assert_eq!(items.len(), 3);
    let error = items.last().unwrap().as_ref().unwrap_err();
    assert!(error.is_retryable(), "{error:?}");
    assert!(error.to_string().contains("ended before a finish_reason"));
}

#[tokio::test]
async fn non_event_stream_success_is_an_error() {
    let server = Server::start(|_| Reply::json(200, "{}")).await;
    let error = complete_error(&openai(&server), request("gpt-5.1", hello())).await;
    assert!(matches!(error, Error::Other(_)), "{error:?}");
    assert!(error.to_string().contains("expected an event stream"));
}

#[tokio::test]
async fn connection_refused_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    drop(listener);
    let mut config = OpenAiConfig::new(None);
    config.base_url = url;
    let provider = OpenAiProvider::new(config, Arc::new(Media::default())).unwrap();
    let error = complete_error(&provider, request("m", hello())).await;
    assert!(error.is_retryable(), "{error:?}");
    let models_error = provider.list_models().await.unwrap_err();
    assert!(models_error.is_retryable(), "{models_error:?}");
}

fn images() -> Vec<String> {
    ["image/jpeg", "image/png", "image/gif", "image/webp"]
        .map(String::from)
        .to_vec()
}

#[tokio::test]
async fn list_models_from_openai_with_configured_capabilities() {
    let server = Server::start(|_| Reply::json(200, fixture("models_openai.json"))).await;
    let mut config = OpenAiConfig::new(Some(ApiKey::new("sk-test")));
    config.base_url = format!("{}/v1", server.url);
    let config_json =
        json!({"models": {"gpt-5.1": {"reasoning": true, "context_window": 400_000}}});
    let configured = OpenAiConfig::from_json(&config_json).unwrap();
    config.models = configured.models;
    let provider = OpenAiProvider::new(config, Arc::new(Media::default())).unwrap();

    assert_eq!(
        provider.list_models().await.unwrap(),
        [
            ModelInfo {
                id: "gpt-5.1".into(),
                display_name: None,
                context_window: Some(400_000),
                max_output_tokens: None,
                media_types: images(),
                reasoning: true,
            },
            ModelInfo {
                id: "gpt-4o-mini".into(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
                media_types: images(),
                reasoning: false,
            },
        ]
    );
    let [recorded] = server.requests().try_into().unwrap();
    assert_eq!(recorded.method, "GET");
    assert_eq!(recorded.uri, "/v1/models");
    assert_eq!(recorded.header("authorization"), Some("Bearer sk-test"));
}

#[tokio::test]
async fn list_models_from_openrouter_uses_reported_details() {
    let server = Server::start(|_| Reply::json(200, fixture("models_openrouter.json"))).await;
    let provider = from_config(&server, "/api/v1", json!({"dialect": "openrouter"}));
    assert_eq!(
        provider.list_models().await.unwrap(),
        [
            ModelInfo {
                id: "anthropic/claude-sonnet-4.5".into(),
                display_name: Some("Anthropic: Claude Sonnet 4.5".into()),
                context_window: Some(1_000_000),
                max_output_tokens: Some(64_000),
                media_types: images(),
                reasoning: true,
            },
            ModelInfo {
                id: "deepseek/deepseek-chat-v3.1".into(),
                display_name: Some("DeepSeek: DeepSeek V3.1".into()),
                context_window: Some(163_840),
                max_output_tokens: None,
                media_types: vec![],
                reasoning: false,
            },
        ]
    );
    assert_eq!(server.requests()[0].uri, "/api/v1/models");
}

#[tokio::test]
async fn list_models_from_ollama() {
    let server = Server::start(|_| Reply::json(200, fixture("models_ollama.json"))).await;
    let provider = from_config(
        &server,
        "/v1",
        json!({"context_window": 32_768,
               "models": {"qwen3:8b": {"reasoning": true, "context_window": 40_960}}}),
    );
    let models = provider.list_models().await.unwrap();
    let summary: Vec<_> = models
        .iter()
        .map(|m| {
            (
                m.id.as_str(),
                m.context_window,
                m.reasoning,
                m.media_types.len(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("qwen3:8b", Some(40_960), true, 0),
            ("llama3.2:latest", Some(32_768), false, 0),
        ]
    );
    let [recorded] = server.requests().try_into().unwrap();
    assert_eq!(recorded.uri, "/v1/models");
    assert_eq!(recorded.header("authorization"), None);
}

#[tokio::test]
async fn list_models_errors_map_like_completions() {
    let server = Server::start(|_| {
        let mut reply = Reply::json(503, "unavailable");
        reply.headers.push(("retry-after", "2"));
        reply
    })
    .await;
    match openai(&server).list_models().await.unwrap_err() {
        Error::Unavailable { retry_after, .. } => {
            assert_eq!(retry_after, Some(Duration::from_secs(2)));
        }
        other => panic!("{other:?}"),
    }

    let server = Server::start(|_| Reply::json(200, "{\"object\": \"list\"}")).await;
    let error = openai(&server).list_models().await.unwrap_err();
    assert!(
        error.to_string().contains("malformed model list"),
        "{error}"
    );
}
