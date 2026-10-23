//! The plugin SDK end to end: launches `giles-channel-echo` the way the core does (its end of a
//! socket pair as fd 3, the token on stdin, a scrubbed environment), checks the handshake line, and
//! drives every RPC over the one connection.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc;
use futures::stream;
use giles_plugin_sdk::convert::event_from_proto;
use giles_plugin_sdk::giles_types::{AuthChallenge, ChannelEvent, ChannelStatus};
use giles_proto::MAX_MESSAGE_SIZE;
use giles_proto::channel::v1::channel_service_client::ChannelServiceClient;
use giles_proto::channel::v1::{
    EventAck, EventsRequest, EventsResponse, OutputMode, SendRequest, events_request, send_request,
};
use giles_proto::handshake::{
    DATA_ENV, EXIT_NO_SHARED_PROTOCOL, HandshakeLine, MAGIC_COOKIE, MAGIC_COOKIE_ENV, PROTOCOLS_ENV,
};
use giles_proto::plugin::v1::plugin_service_client::PluginServiceClient;
use giles_proto::plugin::v1::{
    BlobChunk, ConfigureRequest, DescribeRequest, GetBlobRequest, PluginKind, PutBlobRequest,
    ReleaseBlobRequest, ShutdownRequest, blob_chunk, describe_response,
};
use giles_proto::types::v1::BlobRef;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdin, Command};
use tonic::metadata::{Ascii, MetadataValue};
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::{Code, Request, Status, Streaming};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;

const BINARY: &str = env!("CARGO_BIN_EXE_giles-channel-echo");
const TOKEN: &str = "per-launch-token-5f1d";
/// Upper bound for anything that must not hang.
const WAIT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------------------------
// Launching, as the core does.

/// The environment the core gives a plugin (PLAN section 7.4), minus the sandbox.
fn plugin_env(data: &Path) -> Vec<(&'static str, String)> {
    let data = data.to_str().unwrap().to_owned();
    vec![
        (MAGIC_COOKIE_ENV, MAGIC_COOKIE.into()),
        (PROTOCOLS_ENV, "1".into()),
        (DATA_ENV, data.clone()),
        ("HOME", data.clone()),
        ("TMPDIR", format!("{data}/tmp")),
        ("PATH", "/usr/bin:/bin".into()),
    ]
}

/// A command for the echo plugin with exactly `env` and piped stdio.
fn command(env: &[(&str, String)]) -> Command {
    let mut command = Command::new(BINARY);
    command
        .env_clear()
        .envs(env.iter().map(|(k, v)| (*k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// Makes the child's fd 3 a copy of `fd`, without close-on-exec.
#[allow(unsafe_code)]
fn pass_as_fd3(command: &mut Command, fd: RawFd) {
    // SAFETY: the closure runs in the child between fork and exec and only calls `fcntl` and
    // `dup2`, which are async-signal-safe. `fd` is open in the parent until `spawn` returns.
    unsafe {
        command.pre_exec(move || {
            if fd == 3 {
                // `dup2` onto itself would keep the close-on-exec flag.
                let flags = libc::fcntl(3, libc::F_GETFD);
                if flags == -1 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(fd, 3) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Collects a pipe into a string in the background.
fn collect(mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static) -> Arc<Mutex<String>> {
    let text = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&text);
    tokio::spawn(async move {
        let mut buf = vec![0; 4096];
        while let Ok(len @ 1..) = pipe.read(&mut buf).await {
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..len]));
        }
    });
    text
}

/// A launched plugin, past the handshake line.
struct Plugin {
    child: Child,
    stdin: Option<ChildStdin>,
    grpc: Channel,
    stderr: Arc<Mutex<String>>,
}

impl Plugin {
    async fn launch(data: &Path) -> Self {
        Self::launch_with(&plugin_env(data)).await
    }

    async fn launch_with(env: &[(&str, String)]) -> Self {
        let (core_end, plugin_end) = StdUnixStream::pair().unwrap();
        let mut command = command(env);
        pass_as_fd3(&mut command, plugin_end.as_raw_fd());
        let mut child = command.spawn().unwrap();
        drop(plugin_end);

        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(format!("{TOKEN}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
        let stderr = collect(child.stderr.take().unwrap());
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(WAIT, stdout.read_line(&mut line))
            .await
            .expect("a handshake line in time")
            .unwrap();
        assert_eq!(
            HandshakeLine::parse(&line),
            Ok(HandshakeLine { protocol: 1 }),
            "{line:?}; stderr: {}",
            stderr.lock().unwrap()
        );
        assert_eq!(line, "1|1|fd|3|grpc\n");
        // After the line, the core drains stdout and ignores it.
        collect(stdout);

        core_end.set_nonblocking(true).unwrap();
        let mut io = Some(UnixStream::from_std(core_end).unwrap());
        let grpc = Endpoint::from_static("http://plugin")
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let io = io.take();
                async move {
                    io.map(TokioIo::new)
                        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "used"))
                }
            }))
            .await
            .unwrap();
        Self {
            child,
            stdin: Some(stdin),
            grpc,
            stderr,
        }
    }

    fn plugin(&self) -> PluginServiceClient<InterceptedService<Channel, Bearer>> {
        self.plugin_with(TOKEN)
    }

    fn plugin_with(&self, token: &str) -> PluginServiceClient<InterceptedService<Channel, Bearer>> {
        PluginServiceClient::with_interceptor(self.grpc.clone(), Bearer::new(token))
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
    }

    fn channel(&self) -> ChannelServiceClient<InterceptedService<Channel, Bearer>> {
        ChannelServiceClient::with_interceptor(self.grpc.clone(), Bearer::new(TOKEN))
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE)
    }

    fn health_with(&self, token: &str) -> HealthClient<InterceptedService<Channel, Bearer>> {
        HealthClient::with_interceptor(self.grpc.clone(), Bearer::new(token))
    }

    async fn configure(&self, config: Value) -> Result<String, Status> {
        let response = self
            .plugin()
            .configure(ConfigureRequest {
                instance: "test".into(),
                config_json: config.to_string(),
            })
            .await?;
        Ok(response.into_inner().identity_namespace)
    }

    /// Opens `Events`; acks go through the returned sender.
    async fn events(
        &self,
    ) -> (
        mpsc::UnboundedSender<EventsRequest>,
        Streaming<EventsResponse>,
    ) {
        let (acks, requests) = mpsc::unbounded();
        let events = self.channel().events(requests).await.unwrap().into_inner();
        (acks, events)
    }

    async fn health(&self, service: &str) -> ServingStatus {
        let response = self
            .health_with(TOKEN)
            .check(HealthCheckRequest {
                service: service.into(),
            })
            .await
            .unwrap();
        response.into_inner().status()
    }

    async fn exit_status(&mut self) -> ExitStatus {
        tokio::time::timeout(WAIT, self.child.wait())
            .await
            .expect("the plugin exits in time")
            .unwrap()
    }
}

/// Adds `authorization: Bearer <token>`, like the core's clients.
#[derive(Clone)]
struct Bearer(MetadataValue<Ascii>);

impl Bearer {
    fn new(token: &str) -> Self {
        Self(format!("Bearer {token}").parse().unwrap())
    }
}

impl Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.0.clone());
        Ok(request)
    }
}

// ---------------------------------------------------------------------------------------------
// The echo plugin's files.

/// Drops an event into `inject/` the way tests must: written elsewhere, renamed in.
fn inject(data: &Path, name: &str, injection: &Value) {
    let dir = data.join("inject");
    std::fs::create_dir_all(&dir).unwrap();
    let tmp = data.join(format!("{name}.tmp"));
    std::fs::write(&tmp, injection.to_string()).unwrap();
    std::fs::rename(tmp, dir.join(format!("{name}.json"))).unwrap();
}

fn message_event(id: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "id": id,
        "message": {
            "conversation": {"id": "dm-alice", "kind": "direct"},
            "sender": "alice",
            "sender_aliases": [],
            "sender_authenticated": true,
            "text": text,
            "attachments": [],
        },
    })
}

fn read_lines(path: &Path) -> Vec<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
        Err(e) => panic!("{}: {e}", path.display()),
    }
}

async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn next_event(events: &mut Streaming<EventsResponse>) -> ChannelEvent {
    let response = tokio::time::timeout(WAIT, events.message())
        .await
        .expect("an event in time")
        .unwrap()
        .expect("the stream is open");
    event_from_proto(response.event.unwrap()).unwrap()
}

fn ack(id: &str) -> EventsRequest {
    EventsRequest {
        request: Some(events_request::Request::Ack(EventAck {
            event_id: id.into(),
        })),
    }
}

async fn get_blob(plugin: &Plugin, id: &str) -> Result<(BlobRef, Vec<u8>), Status> {
    let mut stream = plugin
        .plugin()
        .get_blob(GetBlobRequest { id: id.into() })
        .await?
        .into_inner();
    let mut header = None;
    let mut data = Vec::new();
    while let Some(response) = stream.message().await? {
        match response.chunk.unwrap().part.unwrap() {
            blob_chunk::Part::Header(blob) => header = Some(blob),
            blob_chunk::Part::Data(bytes) => data.extend(bytes),
        }
    }
    Ok((header.expect("a header"), data))
}

// ---------------------------------------------------------------------------------------------
// Tests.

#[tokio::test]
async fn handshake_describe_configure_and_health() {
    let data = tempfile::tempdir().unwrap();
    let plugin = Plugin::launch(data.path()).await;

    let described = plugin
        .plugin()
        .describe(DescribeRequest {
            core_protocol_minor: 0,
            core_features: vec!["channel.status".into()],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(described.kind(), PluginKind::Channel);
    assert_eq!(described.name, "echo");
    assert_eq!(described.version, env!("CARGO_PKG_VERSION"));
    let Some(describe_response::Capabilities::Channel(capabilities)) = described.capabilities
    else {
        panic!("no channel capabilities: {described:?}");
    };
    assert_eq!(capabilities.output(), OutputMode::Blocks);
    assert!(capabilities.can_initiate);

    assert_eq!(plugin.health("").await, ServingStatus::Serving);
    assert_eq!(
        plugin.health("giles.plugin.v1.PluginService").await,
        ServingStatus::Serving
    );
    assert_eq!(
        plugin.health("giles.channel.v1.ChannelService").await,
        ServingStatus::NotServing
    );

    let status = plugin.configure(json!({"ecco": true})).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("unknown field `ecco`"),
        "{status:?}"
    );

    assert_eq!(
        plugin
            .configure(json!({"namespace": "echo:test"}))
            .await
            .unwrap(),
        "echo:test"
    );
    assert_eq!(
        plugin.health("giles.channel.v1.ChannelService").await,
        ServingStatus::Serving
    );
}

#[tokio::test]
async fn a_wrong_token_is_unauthenticated_on_every_service() {
    let data = tempfile::tempdir().unwrap();
    let plugin = Plugin::launch(data.path()).await;
    for token in ["wrong", ""] {
        let status = plugin
            .plugin_with(token)
            .describe(DescribeRequest::default())
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
        let status = plugin
            .health_with(token)
            .check(HealthCheckRequest::default())
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);
    }
    let status = ChannelServiceClient::new(plugin.grpc.clone())
        .send(SendRequest::default())
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated, "no token at all");
    // The rejections didn't hurt the connection.
    plugin
        .plugin()
        .describe(DescribeRequest::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn events_blobs_and_acks_survive_a_crash() {
    let data = tempfile::tempdir().unwrap();
    let mut with_blob = json!({
        "event": message_event("e1", "see attached"),
        "attachments": [{"text": "hello", "media_type": "text/plain", "name": "hi.txt"}],
    });
    inject(data.path(), "1", &with_blob);
    inject(
        data.path(),
        "2",
        &json!({"event": message_event("e2", "second")}),
    );
    inject(
        data.path(),
        "3",
        &json!({"event": {
            "type": "status",
            "id": "s1",
            "status": {"type": "needs_auth", "challenge": {"type": "qr_code", "payload": "2@x"}},
        }}),
    );

    let mut plugin = Plugin::launch(data.path()).await;
    plugin.configure(json!({})).await.unwrap();
    let (acks, mut events) = plugin.events().await;
    let ChannelEvent::Message { id, message } = next_event(&mut events).await else {
        panic!("not a message");
    };
    assert_eq!(id.as_str(), "e1");
    assert_eq!(message.text, "see attached");
    let [media] = &message.attachments[..] else {
        panic!("one attachment: {message:?}");
    };
    assert_eq!(
        media.id.as_str(),
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    assert_eq!((media.size, media.name.as_deref()), (5, Some("hi.txt")));
    assert_eq!(next_event(&mut events).await.id().as_str(), "e2");
    let ChannelEvent::Status { id, status } = next_event(&mut events).await else {
        panic!("not a status");
    };
    assert_eq!(id.as_str(), "s1");
    assert_eq!(
        status,
        ChannelStatus::NeedsAuth {
            challenge: AuthChallenge::QrCode {
                payload: "2@x".into()
            },
            expires_at: None,
        }
    );
    // The echo plugin deletes an injection once the SDK has persisted its event: from here on,
    // only the SDK has them.
    let inject_dir = data.path().join("inject");
    until("the injections to be consumed", || {
        std::fs::read_dir(&inject_dir).unwrap().count() == 0
    })
    .await;

    // The core fetches the attachment, stores it, releases it.
    let (header, bytes) = get_blob(&plugin, media.id.as_str()).await.unwrap();
    assert_eq!(header.media_type, "text/plain");
    assert_eq!(header.size, 5);
    assert_eq!(bytes, b"hello");
    plugin
        .plugin()
        .release_blob(ReleaseBlobRequest {
            ids: vec![media.id.to_string()],
        })
        .await
        .unwrap();
    let status = get_blob(&plugin, media.id.as_str()).await.unwrap_err();
    assert_eq!(status.code(), Code::NotFound);

    acks.unbounded_send(ack("e1")).unwrap();
    let acks_file = data.path().join("acks.jsonl");
    until("the channel to see the ack", || {
        read_lines(&acks_file) == [json!({"event_id": "e1"})]
    })
    .await;

    // A crash: no Shutdown, no end-of-file.
    plugin.child.start_kill().unwrap();
    assert!(!plugin.exit_status().await.success());

    let plugin = Plugin::launch(data.path()).await;
    plugin.configure(json!({})).await.unwrap();
    let (acks, mut events) = plugin.events().await;
    assert_eq!(next_event(&mut events).await.id().as_str(), "e2");
    assert_eq!(next_event(&mut events).await.id().as_str(), "s1");

    // Something new after the resent ones.
    with_blob["event"] = message_event("e4", "fourth");
    inject(data.path(), "4", &with_blob);
    assert_eq!(next_event(&mut events).await.id().as_str(), "e4");
    for id in ["e2", "s1", "e4"] {
        acks.unbounded_send(ack(id)).unwrap();
    }
    until("all acks", || read_lines(&acks_file).len() == 4).await;
    let events_dir = data.path().join("giles-sdk/events");
    assert_eq!(std::fs::read_dir(events_dir).unwrap().count(), 0);
}

#[tokio::test]
async fn send_reads_put_blobs_and_echoes_back() {
    let data = tempfile::tempdir().unwrap();
    let plugin = Plugin::launch(data.path()).await;
    plugin
        .configure(json!({"echo": true, "unavailable": ["bob"]}))
        .await
        .unwrap();
    let (_acks, mut events) = plugin.events().await;

    let chunk = |part| PutBlobRequest {
        chunk: Some(BlobChunk { part: Some(part) }),
    };
    let blob = plugin
        .plugin()
        .put_blob(stream::iter([
            chunk(blob_chunk::Part::Header(BlobRef {
                id: String::new(),
                media_type: "text/markdown".into(),
                size: 11,
                name: Some("notes.md".into()),
            })),
            chunk(blob_chunk::Part::Data(b"# notes".to_vec())),
            chunk(blob_chunk::Part::Data(b"\nhi\n".to_vec())),
        ]))
        .await
        .unwrap()
        .into_inner()
        .blob
        .unwrap();

    let response = plugin
        .channel()
        .send(SendRequest {
            target: Some(send_request::Target::ConversationId("dm-alice".into())),
            markdown: "**hello**".into(),
            attachments: vec![blob.clone()],
            reply_to: Some("m0".into()),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.message_id, "echo-1");
    let sent = read_lines(&data.path().join("sent.jsonl"));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["message_id"], "echo-1");
    assert_eq!(
        sent[0]["message"]["target"],
        json!({"conversation": "dm-alice"})
    );
    assert_eq!(sent[0]["message"]["markdown"], "**hello**");
    assert_eq!(sent[0]["message"]["reply_to"], "m0");
    assert_eq!(sent[0]["attachment_texts"], json!(["# notes\nhi\n"]));

    // The echo comes back as an inbound message, its attachment now the plugin's own blob.
    let ChannelEvent::Message { id, message } = next_event(&mut events).await else {
        panic!("not a message");
    };
    assert_eq!(id.as_str(), "echo-1");
    assert_eq!(message.sender.as_str(), "echo-user");
    assert_eq!(message.text, "**hello**");
    assert_eq!(message.attachments[0].id.as_str(), blob.id);
    let (_, bytes) = get_blob(&plugin, &blob.id).await.unwrap();
    assert_eq!(bytes, b"# notes\nhi\n");

    let status = plugin
        .channel()
        .send(SendRequest {
            target: Some(send_request::Target::Recipient("bob".into())),
            markdown: "hi".into(),
            ..SendRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable);
    assert!(
        status.message().contains("bob is not reachable"),
        "{status:?}"
    );

    let response = plugin
        .channel()
        .send(SendRequest {
            target: Some(send_request::Target::Recipient("carol".into())),
            markdown: "hi".into(),
            ..SendRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.message_id, "echo-2");
    assert_eq!(
        read_lines(&data.path().join("sent.jsonl"))[1]["message"]["target"],
        json!({"recipient": "carol"})
    );
}

#[tokio::test]
async fn the_plugin_exits_when_stdin_closes() {
    let data = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::launch(data.path()).await;
    plugin.configure(json!({})).await.unwrap();
    let (_acks, _events) = plugin.events().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(plugin.child.try_wait().unwrap().is_none(), "still running");
    drop(plugin.stdin.take());
    let status = plugin.exit_status().await;
    assert!(
        status.success(),
        "{status}; stderr: {}",
        plugin.stderr.lock().unwrap()
    );
}

#[tokio::test]
async fn shutdown_drains_and_exits() {
    let data = tempfile::tempdir().unwrap();
    let mut plugin = Plugin::launch(data.path()).await;
    plugin.configure(json!({})).await.unwrap();
    let (_acks, mut events) = plugin.events().await;
    plugin.plugin().shutdown(ShutdownRequest {}).await.unwrap();
    let ended = tokio::time::timeout(WAIT, events.message()).await.unwrap();
    assert!(matches!(ended, Ok(None)), "{ended:?}");
    // Stdin stays open: the plugin leaves because it was asked to.
    let status = plugin.exit_status().await;
    assert!(
        status.success(),
        "{status}; stderr: {}",
        plugin.stderr.lock().unwrap()
    );
    assert!(plugin.stdin.is_some());
}

/// Runs the binary with `env`, stdin `input`, and no fd 3; returns its status, stdout, stderr.
async fn run_bare(env: &[(&str, String)], input: &[u8]) -> (ExitStatus, String, String) {
    let mut child = command(env).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // The plugin may exit before it reads stdin (no shared protocol, say): a closed pipe is fine.
    if let Err(error) = stdin.write_all(input).await {
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe, "{error}");
    }
    drop(stdin);
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("the plugin exits in time")
        .unwrap();
    (
        output.status,
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

#[tokio::test]
async fn run_by_hand_it_says_what_it_is() {
    let (status, stdout, stderr) = run_bare(&[("PATH", "/usr/bin:/bin".into())], b"").await;
    assert_eq!(status.code(), Some(1));
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("giles-channel-echo is a Giles plugin"),
        "{stderr}"
    );
}

#[tokio::test]
async fn no_shared_protocol_exits_with_its_own_status() {
    let data = tempfile::tempdir().unwrap();
    let mut env = plugin_env(data.path());
    env.retain(|(k, _)| *k != PROTOCOLS_ENV);
    env.push((PROTOCOLS_ENV, "2,3".into()));
    let (status, stdout, stderr) = run_bare(&env, b"token\n").await;
    assert_eq!(status.code(), Some(EXIT_NO_SHARED_PROTOCOL));
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("no shared plugin protocol: Giles speaks 2,3, this plugin speaks 1"),
        "{stderr}"
    );
}

#[tokio::test]
async fn a_missing_socket_or_token_fails_before_the_handshake() {
    let data = tempfile::tempdir().unwrap();
    // No fd 3.
    let (status, stdout, stderr) = run_bare(&plugin_env(data.path()), b"token\n").await;
    assert_eq!(status.code(), Some(1));
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("fd 3 is not the plugin's socket"),
        "{stderr}"
    );

    // Fd 3, but stdin closes before the token line.
    let (_core_end, plugin_end) = StdUnixStream::pair().unwrap();
    let mut command = command(&plugin_env(data.path()));
    pass_as_fd3(&mut command, plugin_end.as_raw_fd());
    let mut child = command.spawn().unwrap();
    drop(plugin_end);
    drop(child.stdin.take());
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("reading the token from stdin"), "{stderr}");
}
