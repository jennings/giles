//! The MCP client against an in-process `rmcp` server over a duplex pipe: listing, `list_changed`,
//! calls with progress, media results, errors, cancellation, and a server that goes away.
#![allow(missing_docs)]

#[path = "support/mcp_server.rs"]
mod mcp_server;

use std::time::Duration;

use futures::StreamExt;
use giles_core::mcp::{McpMedia, McpToolset};
use giles_store::BlobStore;
use giles_types::{
    AgentId, Block, CallContext, Error, MediaRef, ToolCall, ToolCallEvent, ToolCallHandle, Toolset,
};
use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use tokio::task::JoinHandle;

use self::mcp_server::{TOOLS, TestServer, blob_store, no_input};

const WAIT: Duration = Duration::from_secs(10);

/// The server side of a connection.
struct Server {
    /// The task relaying between client and server; aborting it cuts the connection the way a
    /// crashing server would, without a goodbye.
    relay: JoinHandle<()>,
    /// Where the client stores media.
    blobs: BlobStore,
    _dir: tempfile::TempDir,
}

/// Starts `server` and connects to it through a relay task.
async fn start(server: TestServer) -> (McpToolset, Server) {
    let (client, mut client_relay) = tokio::io::duplex(1 << 16);
    let (server_io, mut server_relay) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.expect("server handshake");
        let _ = running.waiting().await;
    });
    let relay = tokio::spawn(async move {
        let _ = tokio::io::copy_bidirectional(&mut client_relay, &mut server_relay).await;
    });
    let (read, write) = tokio::io::split(client);
    let (dir, blobs) = blob_store().await;
    let media = McpMedia::new(blobs.clone());
    let toolset = McpToolset::connect_io("test", read, write, media)
        .await
        .unwrap();
    let server = Server {
        relay,
        blobs,
        _dir: dir,
    };
    (toolset, server)
}

fn ctx() -> CallContext {
    CallContext {
        agent: AgentId::new("agent").unwrap(),
        requester: None,
        conversation: None,
    }
}

async fn call(toolset: &McpToolset, name: &str, input: Value) -> ToolCallHandle {
    let call = ToolCall {
        id: format!("toolu_{name}"),
        name: name.to_owned(),
        input,
        media: Vec::new(),
    };
    toolset.call(call, ctx()).await.unwrap()
}

async fn next(handle: &mut ToolCallHandle) -> Option<giles_types::Result<ToolCallEvent>> {
    tokio::time::timeout(WAIT, handle.events.next())
        .await
        .expect("timed out waiting for a call event")
}

/// The call's result, skipping progress; checks that the stream ends after it.
async fn result(mut handle: ToolCallHandle) -> giles_types::Result<(Vec<Block>, bool)> {
    loop {
        match next(&mut handle)
            .await
            .expect("stream ended before the result")?
        {
            ToolCallEvent::Progress { .. } => continue,
            ToolCallEvent::Result { content, is_error } => {
                assert!(next(&mut handle).await.is_none(), "events after the result");
                return Ok((content, is_error));
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
}

fn text(text: &str) -> Block {
    Block::Text {
        text: text.to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_every_page_as_bare_sorted_specs() {
    let (toolset, _server) = start(TestServer::default()).await;
    let tools = toolset.list_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, TOOLS);
    let echo = tools.iter().find(|tool| tool.name == "echo").unwrap();
    assert_eq!(echo.description, "Echoes its input");
    assert_eq!(
        echo.input_schema,
        json!({"type": "object", "properties": {"text": {"type": "string"}}})
    );
    assert_eq!(toolset.list_skills().await.unwrap(), []);
    assert!(matches!(
        toolset.get_skill("anything").await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_changed_yields_the_new_list() {
    let (toolset, _server) = start(TestServer::default()).await;
    let mut changes = toolset.tool_changes().expect("listChanged is declared");
    let handle = call(&toolset, "add_tool", json!({"name": "zebra"})).await;
    assert_eq!(result(handle).await.unwrap(), (vec![text("added")], false));
    let tools = tokio::time::timeout(WAIT, changes.next())
        .await
        .expect("timed out waiting for the changed list")
        .expect("the stream ended");
    assert_eq!(tools.len(), TOOLS.len() + 1);
    assert_eq!(tools.last().unwrap().name, "zebra");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_change_stream_without_list_changed() {
    let server = TestServer {
        without_list_changed: true,
        ..TestServer::default()
    };
    let (toolset, _server) = start(server).await;
    assert!(toolset.tool_changes().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_report_progress_then_the_result() {
    let server = TestServer::default();
    let shared = server.shared.clone();
    let (toolset, _server) = start(server).await;
    let mut handle = call(
        &toolset,
        "progress_steps",
        json!({"steps": 2, "gated": true}),
    )
    .await;
    for step in [1.0, 2.0] {
        let event = next(&mut handle).await.unwrap().unwrap();
        assert_eq!(
            event,
            ToolCallEvent::Progress {
                progress: step,
                total: Some(2.0),
                message: Some(format!("step {step}")),
            }
        );
        shared.gate.add_permits(1);
    }
    assert_eq!(result(handle).await.unwrap(), (vec![text("done")], false));
}

/// The bytes of the stored blob `media`.
async fn stored(blobs: &BlobStore, media: &MediaRef) -> Vec<u8> {
    let mut blob = blobs.get(&media.id).await.unwrap().expect("stored");
    let mut bytes = Vec::new();
    blob.reader.read_to_end(&mut bytes).await.unwrap();
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn images_audio_and_blob_resources_are_stored_as_media() {
    let (toolset, server) = start(TestServer::default()).await;
    let (content, is_error) = result(call(&toolset, "media", no_input()).await)
        .await
        .unwrap();
    assert!(!is_error);
    let [text_block, image, audio, document] = content.as_slice() else {
        panic!("unexpected content {content:?}");
    };
    assert_eq!(*text_block, text("Here they are."));
    let (Block::Image { media: image }, Block::Audio { media: audio }) = (image, audio) else {
        panic!("not an image and audio: {content:?}");
    };
    let Block::Document {
        media: document,
        name,
    } = document
    else {
        panic!("not a document: {document:?}");
    };
    assert_eq!((image.media_type.as_str(), image.size), ("image/png", 5));
    assert_eq!(audio.media_type, "audio/ogg");
    assert_eq!(document.media_type, "application/pdf");
    assert_eq!(name.as_deref(), Some("q3.pdf"));
    for media in [image, audio, document] {
        assert_eq!(stored(&server.blobs, media).await, b"hello");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn results_carry_text_and_is_error() {
    let (toolset, _server) = start(TestServer::default()).await;
    let echo = call(&toolset, "echo", json!({"text": "hello"})).await;
    assert_eq!(result(echo).await.unwrap(), (vec![text("hello")], false));
    let fail = call(&toolset, "fail", no_input()).await;
    assert_eq!(result(fail).await.unwrap(), (vec![text("boom")], true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_json_rpc_error_is_the_last_event() {
    let (toolset, _server) = start(TestServer::default()).await;
    let handle = call(&toolset, "nope", no_input()).await;
    match result(handle).await {
        Err(Error::InvalidRequest(message)) => {
            assert_eq!(message, "MCP server test: Unknown tool: nope");
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_must_be_an_object() {
    let (toolset, _server) = start(TestServer::default()).await;
    let call = ToolCall {
        id: "toolu_1".to_owned(),
        name: "echo".to_owned(),
        input: json!(["not", "an", "object"]),
        media: Vec::new(),
    };
    assert!(matches!(
        toolset.call(call, ctx()).await,
        Err(Error::InvalidRequest(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_events_cancels_the_call_on_the_server() {
    let server = TestServer::default();
    let shared = server.shared.clone();
    let (toolset, _server) = start(server).await;
    let mut handle = call(&toolset, "hang", no_input()).await;
    // The first progress event means the server is inside the call.
    assert!(matches!(
        next(&mut handle).await,
        Some(Ok(ToolCallEvent::Progress { .. }))
    ));
    drop(handle);
    tokio::time::timeout(WAIT, async {
        loop {
            let notified = shared.cancelled_notify.notified();
            if *shared.cancelled.lock().unwrap() == 1 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("the server never saw the cancellation");
    // The connection is still usable.
    let echo = call(&toolset, "echo", json!({"text": "still here"})).await;
    assert_eq!(
        result(echo).await.unwrap(),
        (vec![text("still here")], false)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elicitation_is_declined() {
    let (toolset, _server) = start(TestServer::default()).await;
    let handle = call(&toolset, "ask", no_input()).await;
    assert_eq!(
        result(handle).await.unwrap(),
        (vec![text("Decline")], false)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_goes_away_is_unavailable() {
    let (toolset, server) = start(TestServer::default()).await;
    let mut handle = call(&toolset, "hang", no_input()).await;
    assert!(matches!(
        next(&mut handle).await,
        Some(Ok(ToolCallEvent::Progress { .. }))
    ));
    server.relay.abort();
    match next(&mut handle).await {
        Some(Err(error @ Error::Unavailable { .. })) => {
            assert!(error.to_string().contains("MCP server test"), "{error}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert!(next(&mut handle).await.is_none());
    assert!(matches!(
        toolset.list_tools().await,
        Err(Error::Unavailable { .. })
    ));
    // `closed` resolves; there is no exit status without a child process.
    let status = tokio::time::timeout(WAIT, toolset.closed())
        .await
        .expect("closed() never resolved");
    assert_eq!(status, None);
}
