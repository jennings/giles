//! The MCP client against the test server as a real stdio child process.
//!
//! The child is this test binary itself: with `GILES_MCP_TEST_SERVER` set, `main` serves MCP on
//! stdin/stdout instead of running the tests. That needs `harness = false` (libtest would print to
//! stdout), so `main` runs the tests in order and panics on the first failure. It also installs a
//! global `tracing` subscriber that records the log, to check that the child's stderr is logged.
#![allow(missing_docs)]

#[path = "support/mcp_server.rs"]
mod mcp_server;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::StreamExt;
use giles_core::config::Secret;
use giles_core::mcp::{DirectCommand, McpMedia, McpToolset, STDERR_TARGET};
use giles_store::{BlobStore, Store};
use giles_types::{
    AgentId, Block, CallContext, Error, ToolCall, ToolCallEvent, ToolCallHandle, Toolset,
};
use rmcp::ServiceExt;
use serde_json::{Value, json};

use self::mcp_server::{TOOLS, TestServer, no_input};

const SERVER_ENV: &str = "GILES_MCP_TEST_SERVER";
/// An env secret the child prints to stderr, which must reach the log redacted.
const SECRET_ENV: &str = "GILES_MCP_TEST_SECRET";
const SECRET: &str = "test-secret-0123456789";
const WAIT: Duration = Duration::from_secs(10);

/// The blob store every connection stores media in, opened by `main`.
static BLOBS: OnceLock<BlobStore> = OnceLock::new();

fn media() -> McpMedia {
    McpMedia::new(BLOBS.get().expect("main opens the blob store").clone())
}

fn main() {
    if std::env::var_os(SERVER_ENV).is_some() {
        return serve();
    }
    let log = Log::default();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .with_writer({
            let log = log.clone();
            move || log.clone()
        })
        .init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let blobs_dir = tempfile::tempdir().unwrap();
    let blobs = runtime.block_on(async {
        let db = Store::open(blobs_dir.path().join("giles.db"))
            .await
            .unwrap();
        BlobStore::open(blobs_dir.path().join("blobs"), db)
            .await
            .unwrap()
    });
    BLOBS.set(blobs).unwrap();
    macro_rules! run {
        ($($test:ident),* $(,)?) => {$(
            print!("test {} ... ", stringify!($test));
            runtime
                .block_on(async { tokio::time::timeout(Duration::from_secs(60), $test(&log)).await })
                .expect("timed out");
            println!("ok");
        )*};
    }
    run!(
        lists_calls_and_logs_stderr,
        cancellation_reaches_the_child,
        a_crashed_server_is_unavailable,
        dropping_the_toolset_kills_the_child,
        dropping_the_toolset_kills_its_process_group,
        a_crashed_server_takes_its_process_group,
        a_missing_command_is_unavailable,
        a_failed_start_reports_the_exit_status,
        a_trampoline_failure_is_not_retryable,
    );
}

/// The child: the test server on stdio. It prints its secret to stderr first. With
/// `GILES_MCP_TEST_SERVER=linger` it doesn't exit when stdin closes, so only a kill stops it; with
/// `exit<N>` it exits with status N at once (`exit125`: like a failing sandbox trampoline); with
/// `with-sleeper` it
/// starts a `sleep` child first (in its process group) and prints `sleeper pid <pid>`.
// The sleeper is meant to outlive any wait: only its process group's death ends it.
#[allow(clippy::zombie_processes)]
fn serve() {
    let mode = std::env::var(SERVER_ENV).unwrap_or_default();
    eprintln!(
        "the secret is {}",
        std::env::var(SECRET_ENV).unwrap_or_default()
    );
    if let Some(code) = mode.strip_prefix("exit") {
        std::process::exit(code.parse().unwrap());
    }
    if mode == "with-sleeper" {
        let sleeper = std::process::Command::new("/bin/sleep")
            .arg("300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        eprintln!("sleeper pid {}", sleeper.id());
    }
    let linger = mode == "linger";
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        eprintln!("test server ready");
        let running = TestServer::default()
            .serve(rmcp::transport::io::stdio())
            .await
            .expect("server handshake");
        let _ = running.waiting().await;
        if linger {
            std::future::pending::<()>().await;
        }
    });
}

/// The recorded log of this process.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl Log {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    async fn wait_for(&self, needle: &str) {
        tokio::time::timeout(WAIT, async {
            while !self.contents().contains(needle) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{needle:?} never logged; log:\n{}", self.contents()));
    }
}

impl Write for Log {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn connect(instance: &str) -> McpToolset {
    // Every env value is scrubbed as a secret, so the mode must not occur in the log otherwise.
    connect_with(instance, "plain").await
}

async fn connect_with(instance: &str, mode: &str) -> McpToolset {
    try_connect(instance, mode).await.unwrap()
}

async fn try_connect(instance: &str, mode: &str) -> giles_types::Result<McpToolset> {
    try_connect_with(instance, mode, false).await
}

/// Connects to the server in `mode`, as if `command` ran the sandbox trampoline if `trampoline`.
async fn try_connect_with(
    instance: &str,
    mode: &str,
    trampoline: bool,
) -> giles_types::Result<McpToolset> {
    let mut server = DirectCommand::new(std::env::current_exe().unwrap());
    server
        .env
        .insert(SERVER_ENV.to_owned(), Secret::new(mode.to_owned()));
    server
        .env
        .insert(SECRET_ENV.to_owned(), Secret::new(SECRET.to_owned()));
    let (command, secrets) = (server.command(), server.secrets());
    McpToolset::connect_stdio(instance, command, secrets, trampoline, media()).await
}

async fn call(toolset: &McpToolset, name: &str, input: Value) -> ToolCallHandle {
    let call = ToolCall {
        id: format!("toolu_{name}"),
        name: name.to_owned(),
        input,
        media: Vec::new(),
    };
    let ctx = CallContext {
        agent: AgentId::new("agent").unwrap(),
        requester: None,
        conversation: None,
    };
    toolset.call(call, ctx).await.unwrap()
}

async fn next(handle: &mut ToolCallHandle) -> Option<giles_types::Result<ToolCallEvent>> {
    tokio::time::timeout(WAIT, handle.events.next())
        .await
        .expect("timed out waiting for a call event")
}

/// The text of the call's result, which must succeed.
async fn result_text(mut handle: ToolCallHandle) -> String {
    loop {
        match next(&mut handle).await {
            Some(Ok(ToolCallEvent::Progress { .. })) => continue,
            Some(Ok(ToolCallEvent::Result { content, is_error })) => {
                assert!(!is_error);
                match content.as_slice() {
                    [Block::Text { text }] => return text.clone(),
                    other => panic!("unexpected content {other:?}"),
                }
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
}

async fn lists_calls_and_logs_stderr(log: &Log) {
    let toolset = connect("child").await;
    let tools = toolset.list_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, TOOLS);
    let echo = call(&toolset, "echo", json!({"text": "over stdio"})).await;
    assert_eq!(result_text(echo).await, "over stdio");
    // The fmt layer prints the span and target: `mcp{toolset=child}: <target>: <line>`.
    log.wait_for(&format!(
        "mcp{{toolset=child}}: {STDERR_TARGET}: test server ready"
    ))
    .await;
    // The env secret the child echoed is scrubbed.
    log.wait_for(&format!(
        "mcp{{toolset=child}}: {STDERR_TARGET}: the secret is <redacted>"
    ))
    .await;
    assert!(!log.contents().contains(SECRET), "{}", log.contents());
}

async fn cancellation_reaches_the_child(_log: &Log) {
    let toolset = connect("child-cancel").await;
    let mut handle = call(&toolset, "hang", no_input()).await;
    assert!(matches!(
        next(&mut handle).await,
        Some(Ok(ToolCallEvent::Progress { .. }))
    ));
    drop(handle);
    tokio::time::timeout(WAIT, async {
        while result_text(call(&toolset, "cancellations", no_input()).await).await != "1" {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the child never saw the cancellation");
}

async fn a_crashed_server_is_unavailable(log: &Log) {
    let toolset = connect("child-crash").await;
    let mut handle = call(&toolset, "exit", json!({"code": 3})).await;
    assert!(
        matches!(
            next(&mut handle).await,
            Some(Err(Error::Unavailable { .. }))
        ),
        "the call should fail as unavailable"
    );
    assert!(matches!(
        toolset.list_tools().await,
        Err(Error::Unavailable { .. })
    ));
    log.wait_for(
        "mcp{toolset=child-crash}: giles_core::mcp::stdio: MCP server exited status=exit status: 3",
    )
    .await;
    let status = tokio::time::timeout(WAIT, toolset.closed())
        .await
        .expect("closed() never resolved");
    assert_eq!(status.and_then(|status| status.code()), Some(3));
}

/// Whether process `pid` exists (a zombie counts until its parent reaps it).
fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

async fn wait_gone(pid: &str, what: &str) {
    tokio::time::timeout(WAIT, async {
        while alive(pid) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

/// The pid of the `sleep` child that the `with-sleeper` server of `instance` logged.
async fn sleeper_pid(log: &Log, instance: &str) -> String {
    let prefix = format!("mcp{{toolset={instance}}}: {STDERR_TARGET}: sleeper pid ");
    log.wait_for(&prefix).await;
    let contents = log.contents();
    let start = contents.find(&prefix).unwrap() + prefix.len();
    contents[start..].lines().next().unwrap().trim().to_owned()
}

async fn dropping_the_toolset_kills_the_child(_log: &Log) {
    let toolset = connect_with("child-drop", "linger").await;
    let pid = result_text(call(&toolset, "pid", no_input()).await).await;
    assert!(alive(&pid));
    drop(toolset);
    wait_gone(&pid, "the child outlived the toolset").await;
}

async fn dropping_the_toolset_kills_its_process_group(log: &Log) {
    let toolset = connect_with("group-drop", "with-sleeper").await;
    let sleeper = sleeper_pid(log, "group-drop").await;
    assert!(alive(&sleeper));
    drop(toolset);
    wait_gone(&sleeper, "the server's child outlived the toolset").await;
}

async fn a_crashed_server_takes_its_process_group(log: &Log) {
    let toolset = connect_with("group-crash", "with-sleeper").await;
    let sleeper = sleeper_pid(log, "group-crash").await;
    assert!(alive(&sleeper));
    let mut handle = call(&toolset, "exit", json!({"code": 4})).await;
    assert!(matches!(next(&mut handle).await, Some(Err(_))));
    let status = tokio::time::timeout(WAIT, toolset.closed())
        .await
        .expect("closed() never resolved");
    assert_eq!(status.and_then(|status| status.code()), Some(4));
    // The toolset still exists; the server's exit alone took its child.
    wait_gone(&sleeper, "the crashed server's child survived").await;
    drop(toolset);
}

async fn a_missing_command_is_unavailable(_log: &Log) {
    let server = DirectCommand::new("/nonexistent/giles-mcp-server");
    let command = server.command();
    match McpToolset::connect_stdio("missing", command, server.secrets(), false, media()).await {
        Err(Error::Unavailable { message, .. }) => {
            assert!(
                message.starts_with("MCP server missing: can't start"),
                "{message}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

async fn a_failed_start_reports_the_exit_status(_log: &Log) {
    // A plain command exiting 125 is the server's own failure: worth retrying.
    match try_connect("child-125", "exit125").await {
        Err(Error::Unavailable { message, .. }) => {
            assert!(
                message.ends_with("(server process: exit status: 125)"),
                "{message}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

async fn a_trampoline_failure_is_not_retryable(_log: &Log) {
    // Through the sandbox trampoline, 125 means it failed before running the server.
    match try_connect_with("trampoline-125", "exit125", true).await {
        Err(error @ Error::InvalidRequest(_)) => {
            assert!(!error.is_retryable());
            assert!(
                error
                    .to_string()
                    .ends_with("(server process: exit status: 125)"),
                "{error}"
            );
        }
        other => panic!("expected InvalidRequest, got {other:?}"),
    }
    // A crash during the handshake with another status stays retryable.
    match try_connect_with("trampoline-crash", "exit3", true).await {
        Err(Error::Unavailable { message, .. }) => {
            assert!(
                message.ends_with("(server process: exit status: 3)"),
                "{message}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}
