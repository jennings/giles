//! The MCP client against an in-process `rmcp` server over a duplex pipe: listing, `list_changed`,
//! calls with progress, errors, cancellation, and a server that goes away.
#![allow(missing_docs)]

#[path = "support/mcp_server.rs"]
mod mcp_server;

use std::time::Duration;

use futures::StreamExt;
use giles_core::mcp::McpToolset;
use giles_types::{
    AgentId, Block, CallContext, Error, ToolCall, ToolCallEvent, ToolCallHandle, Toolset,
};
use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use self::mcp_server::{TOOLS, TestServer, no_input};

const WAIT: Duration = Duration::from_secs(10);

/// Starts `server` and connects to it. The pipe between them goes through a relay task; aborting
/// it cuts the connection the way a crashing server would, without a goodbye.
async fn start(server: TestServer) -> (McpToolset, JoinHandle<()>) {
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
    let toolset = McpToolset::connect_io("test", read, write).await.unwrap();
    (toolset, relay)
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
    server.abort();
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
