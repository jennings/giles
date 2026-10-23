//! Attachments end to end: a channel message with attachments in the blob store, through
//! `Runtime::start`, the agent loop, and the built-in Anthropic provider, to the request body a
//! local server records. No live calls.
#![cfg(feature = "provider-anthropic")]

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use futures::channel::mpsc as fmpsc;
use futures::stream::{self, BoxStream, StreamExt};
use giles_core::agent_loop::AgentLoop;
use giles_core::approvals::ApprovalBoard;
use giles_core::config::{Config, InstanceName, LoadOptions};
use giles_core::paths::DataRoot;
use giles_core::runtime::Runtime;
use giles_store::BlobMeta;
use giles_types::{
    Channel, ChannelEvent, ChannelInfo, ConversationId, ConversationKind, ConversationRef, EventId,
    InboundMessage, MediaRef, MessageId, NativeId, OutboundMessage, OutputMode, Result,
};
use serde_json::{Value, json};
use support::{Reply, Server};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const WAIT: Duration = Duration::from_secs(10);

struct FakeChannel {
    info: ChannelInfo,
    events: Mutex<Option<fmpsc::UnboundedReceiver<Result<ChannelEvent>>>>,
    sent: mpsc::UnboundedSender<String>,
}

#[async_trait]
impl Channel for FakeChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        match self.events.lock().unwrap().take() {
            Some(rx) => Ok(rx.boxed()),
            None => Ok(stream::pending().boxed()),
        }
    }

    async fn ack(&self, _event: &EventId) -> Result<()> {
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        self.sent.send(msg.markdown).unwrap();
        Ok(MessageId::new("sent").unwrap())
    }
}

fn reference(media: &MediaRef) -> String {
    giles_core::media::reference(media)
}

/// `value` without its `cache_control` keys (the provider's breakpoints aren't under test).
fn without_cache_control(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            object.remove("cache_control");
            for item in object.values_mut() {
                *item = without_cache_control(item.take());
            }
        }
        Value::Array(items) => {
            for item in items {
                *item = without_cache_control(item.take());
            }
        }
        _ => {}
    }
    value
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_provider_gets_accepted_media_as_content_and_the_rest_as_references() {
    // The model takes images but not PDFs; the provider can't send audio at all.
    let server = Server::start(|request| {
        if request.uri.starts_with("/v1/models") {
            Reply::json(
                200,
                json!({
                    "data": [{
                        "type": "model",
                        "id": "claude-test",
                        "capabilities": {
                            "image_input": {"supported": true},
                            "pdf_input": {"supported": false},
                        },
                    }],
                    "has_more": false,
                    "last_id": "claude-test",
                })
                .to_string(),
            )
        } else {
            Reply::sse(support::fixture("anthropic", "text.sse"))
        }
    })
    .await;

    let dir = tempfile::tempdir().unwrap();
    let root = DataRoot::new(dir.path(), Path::new("/tmp")).unwrap();
    root.create_layout().unwrap();
    let write = |relative: &str, contents: &str| {
        let path = dir.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    };
    write(
        "giles.toml",
        &format!(
            "[owner]\nidentities = [\"fake:owner\"]\ntimezone = \"UTC\"\n\
             [provider.anthropic]\nconfig = {{ api_key = \"sk-test\", base_url = \"{}\" }}\n\
             [channel.fake]\n\
             [[routes]]\nagent = \"jeeves\"\n",
            server.url
        ),
    );
    write(
        "agents/jeeves/agent.toml",
        "models = [\"anthropic/claude-test\"]\ntools = []\n",
    );
    let config = Config::load_with(
        &root,
        &LoadOptions {
            env: &|_| None,
            giles_exe: None,
            ..LoadOptions::default()
        },
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let runtime = Runtime::start(root, Arc::new(config)).await.unwrap();

    let now = "2026-10-22T16:30:00Z".parse().unwrap();
    let put = |bytes: &'static [u8], media_type: &str, name: &str| {
        let blobs = runtime.blobs.clone();
        let meta = BlobMeta {
            media_type: media_type.to_owned(),
            name: Some(name.to_owned()),
        };
        async move { blobs.put(bytes, 1024, meta, now).await.unwrap() }
    };
    let png = put(b"png bytes", "image/png", "cat.png").await;
    let pdf = put(b"%PDF-1.7", "application/pdf", "q3.pdf").await;
    let ogg = put(b"OggS", "audio/ogg", "voice.ogg").await;

    let (events, events_rx) = fmpsc::unbounded();
    let (sent_tx, mut sent) = mpsc::unbounded_channel();
    runtime
        .registry
        .register_channel(
            &InstanceName::new("fake").unwrap(),
            Arc::new(FakeChannel {
                info: ChannelInfo {
                    identity_namespace: "fake".to_owned(),
                    output: OutputMode::Blocks,
                    can_initiate: false,
                },
                events: Mutex::new(Some(events_rx)),
                sent: sent_tx,
            }),
        )
        .unwrap();
    let shutdown = CancellationToken::new();
    let approvals = Arc::new(ApprovalBoard::default());
    let sessions = runtime
        .start_sessions(
            Arc::new(AgentLoop::from_runtime(&runtime, approvals.clone())),
            approvals,
            shutdown.clone(),
        )
        .await
        .unwrap();

    events
        .unbounded_send(Ok(ChannelEvent::Message {
            id: EventId::new("e1").unwrap(),
            message: InboundMessage {
                conversation: ConversationRef {
                    id: ConversationId::new("owner").unwrap(),
                    kind: ConversationKind::Direct,
                    title: None,
                },
                sender: NativeId::new("owner").unwrap(),
                sender_aliases: Vec::new(),
                sender_authenticated: true,
                sender_display_name: None,
                text: "What are these?".to_owned(),
                attachments: vec![png.clone(), pdf.clone(), ogg.clone()],
                message_id: None,
                reply_to: None,
                sent_at: Some(now),
            },
        }))
        .unwrap();
    let reply = tokio::time::timeout(WAIT, sent.recv()).await.unwrap();
    assert_eq!(reply.as_deref(), Some("Hello, Stephen."));
    shutdown.cancel();
    sessions.closed().await;

    let requests = server.requests();
    let uris: Vec<&str> = requests.iter().map(|r| r.uri.as_str()).collect();
    assert_eq!(uris.len(), 2, "{uris:?}");
    assert!(uris[0].starts_with("/v1/models"), "{uris:?}");
    assert_eq!(uris[1], "/v1/messages");
    let body = without_cache_control(requests[1].json());
    let png_data = base64::engine::general_purpose::STANDARD.encode(b"png bytes");
    assert_eq!(
        body["messages"],
        json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "[Thu 2026-10-22 16:30 UTC] What are these?"},
                {"type": "text", "text": reference(&png)},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": png_data,
                }},
                // The provider could send a PDF, but this model doesn't take one.
                {"type": "text", "text": reference(&pdf)},
                // The reference alone, not repeated by the provider.
                {"type": "text", "text": reference(&ogg)},
            ],
        }])
    );
}
