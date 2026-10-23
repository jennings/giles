//! `ChannelService` and the channel half of `Describe`/`Configure`, in process.

use std::path::Path;
use std::sync::Mutex;

use futures::channel::mpsc;
use giles_proto::channel::v1::channel_service_client::ChannelServiceClient;
use giles_proto::channel::v1::{EventAck, send_request};
use giles_proto::plugin::v1::plugin_service_client::PluginServiceClient;
use giles_proto::plugin::v1::plugin_service_server::PluginServiceServer;
use giles_proto::plugin::v1::{
    BlobChunk, ConfigureRequest, DescribeRequest, PutBlobRequest, ShutdownRequest, blob_chunk,
};
use giles_proto::types::v1::BlobRef;
use giles_types::{
    ChannelInfo, ChannelStatus, ConversationId, ConversationKind, ConversationRef, Error, EventId,
    InboundMessage, MessageId, NativeId, OutboundMessage,
};
use tonic::Code;
use tonic::transport::{Channel as GrpcChannel, Server};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_client::HealthClient;

use super::*;
use crate::base::Base;
use crate::convert::event_from_proto;
use crate::dirs::SdkDirs;
use crate::test_support::serve;

const WAIT: Duration = Duration::from_secs(5);

/// What the fake channel saw, shared with the test.
#[derive(Default)]
struct Seen {
    /// One sender per `events()` call; dropping it ends that stream.
    senders: Vec<futures::channel::mpsc::UnboundedSender<Result<ChannelEvent>>>,
    /// Event files on disk each time the pump polled the stream.
    files_at_poll: Vec<usize>,
    acks: Vec<EventId>,
    sent: Vec<(OutboundMessage, Vec<Vec<u8>>)>,
    /// Sends with markdown `hold` wait for a permit here before reading their attachments.
    gate: Option<Arc<tokio::sync::Semaphore>>,
    /// Sends waiting at the gate.
    holding: usize,
}

struct Fake {
    info: ChannelInfo,
    events_dir: PathBuf,
    blobs: Blobs,
    seen: Arc<Mutex<Seen>>,
}

#[async_trait]
impl Channel for Fake {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        let (tx, rx) = mpsc::unbounded();
        self.seen.lock().unwrap().senders.push(tx);
        let (seen, dir) = (Arc::clone(&self.seen), self.events_dir.clone());
        Ok(stream::unfold(rx, move |mut rx| {
            let files = std::fs::read_dir(&dir).unwrap().count();
            seen.lock().unwrap().files_at_poll.push(files);
            async move { rx.next().await.map(|event| (event, rx)) }
        })
        .boxed())
    }

    async fn ack(&self, event: &EventId) -> Result<()> {
        self.seen.lock().unwrap().acks.push(event.clone());
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        if msg.target == SendTarget::Recipient(NativeId::new("offline").unwrap()) {
            return Err(Error::unavailable("offline is not online"));
        }
        if msg.markdown == "hold" {
            let gate = {
                let mut seen = self.seen.lock().unwrap();
                seen.holding += 1;
                seen.gate.clone().unwrap()
            };
            gate.acquire().await.unwrap().forget();
        }
        let mut attachments = Vec::new();
        for media in &msg.attachments {
            attachments.push(self.blobs.read(media).await?);
        }
        let mut seen = self.seen.lock().unwrap();
        seen.sent.push((msg, attachments));
        Ok(MessageId::new(format!("m{}", seen.sent.len()))?)
    }
}

struct Fixture {
    data: Arc<tempfile::TempDir>,
    seen: Arc<Mutex<Seen>>,
    shutdown: CancellationToken,
    plugin: PluginServiceClient<GrpcChannel>,
    channel: ChannelServiceClient<GrpcChannel>,
    health: HealthClient<GrpcChannel>,
}

/// A fake channel plugin named `fake`, built from config: `{"output": "deltas"}` makes its
/// `Channel::info` disagree with the plugin, `{"namespace": ...}` sets its namespace, and
/// `{"fail": true}` fails `configure`.
async fn fixture_in(data: Arc<tempfile::TempDir>, can_initiate: bool) -> Fixture {
    let dirs = Arc::new(SdkDirs::open(data.path()).unwrap());
    let (health, health_service) = tonic_health::server::health_reporter();
    let context = Context {
        data_dir: data.path().to_owned(),
        blobs: Blobs::new(Arc::clone(&dirs)),
        dirs: Arc::clone(&dirs),
        health,
        shutdown: CancellationToken::new(),
    };
    let seen = Arc::new(Mutex::new(Seen::default()));
    let configure: Box<ConfigureFn> = {
        let (seen, events_dir) = (Arc::clone(&seen), dirs.events.clone());
        Box::new(move |ctx: Configure| {
            let (seen, events_dir) = (Arc::clone(&seen), events_dir.clone());
            Box::pin(async move {
                #[derive(serde::Deserialize)]
                struct Config {
                    #[serde(default)]
                    fail: bool,
                    #[serde(default)]
                    output: Option<OutputMode>,
                    #[serde(default)]
                    namespace: Option<String>,
                }
                let config: Config = ctx.parse_config()?;
                if config.fail {
                    return Err(Error::InvalidRequest("fail: set".into()));
                }
                Ok(Arc::new(Fake {
                    info: ChannelInfo {
                        identity_namespace: config
                            .namespace
                            .unwrap_or_else(|| format!("fake:{}", ctx.instance())),
                        output: config.output.unwrap_or(OutputMode::Blocks),
                        can_initiate,
                    },
                    events_dir,
                    blobs: ctx.blobs().clone(),
                    seen,
                }) as Arc<dyn Channel>)
            })
        })
    };
    let spool = Spool::open(Arc::clone(&dirs)).unwrap();
    let (kind, routes) = ChannelPlugin::new("fake", "0.0.1")
        .can_initiate(can_initiate)
        .requested_grants(Grants {
            network_ports: vec!["tcp:443".into()],
            ..Grants::default()
        })
        .services(configure, spool, &context);
    let base = Base::new(kind, context.blobs.clone(), context.shutdown.clone());
    let grpc = serve(
        Server::builder()
            .add_routes(routes)
            .add_service(PluginServiceServer::new(base))
            .add_service(health_service),
    )
    .await;
    Fixture {
        data,
        seen,
        shutdown: context.shutdown,
        plugin: PluginServiceClient::new(grpc.clone()),
        channel: ChannelServiceClient::new(grpc.clone()),
        health: HealthClient::new(grpc),
    }
}

async fn fixture() -> Fixture {
    fixture_in(Arc::new(tempfile::tempdir().unwrap()), true).await
}

impl Fixture {
    async fn configure(&mut self, config_json: &str) -> Result<String, Status> {
        let response = self
            .plugin
            .configure(ConfigureRequest {
                instance: "home".into(),
                config_json: config_json.into(),
            })
            .await?;
        Ok(response.into_inner().identity_namespace)
    }

    /// Opens `Events`; acks go through the returned sender.
    async fn open_events(
        &mut self,
    ) -> Result<
        (
            mpsc::UnboundedSender<EventsRequest>,
            Streaming<EventsResponse>,
        ),
        Status,
    > {
        let (acks, requests) = mpsc::unbounded();
        let events = self.channel.events(requests).await?.into_inner();
        Ok((acks, events))
    }

    /// Emits an event from the fake channel's latest stream.
    fn emit(&self, event: ChannelEvent) {
        let seen = self.seen.lock().unwrap();
        seen.senders
            .last()
            .unwrap()
            .unbounded_send(Ok(event))
            .unwrap();
    }

    fn event_files(&self) -> usize {
        std::fs::read_dir(self.data.path().join("giles-sdk/events"))
            .unwrap()
            .count()
    }

    async fn until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn message(id: &str) -> ChannelEvent {
    ChannelEvent::Message {
        id: EventId::new(id).unwrap(),
        message: InboundMessage {
            conversation: ConversationRef {
                id: ConversationId::new("dm-alice").unwrap(),
                kind: ConversationKind::Direct,
                title: None,
            },
            sender: NativeId::new("alice").unwrap(),
            sender_aliases: vec![],
            sender_authenticated: true,
            sender_display_name: None,
            text: format!("text of {id}"),
            attachments: vec![],
            message_id: None,
            reply_to: None,
            sent_at: None,
        },
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

#[tokio::test]
async fn describe_reports_the_channel_capabilities() {
    let mut f = fixture().await;
    let described = f
        .plugin
        .describe(DescribeRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(described.kind(), PluginKind::Channel);
    assert_eq!(described.name, "fake");
    assert_eq!(described.version, "0.0.1");
    assert_eq!(
        described.requested_grants.unwrap().network_ports,
        ["tcp:443"]
    );
    let Some(describe_response::Capabilities::Channel(capabilities)) = described.capabilities
    else {
        panic!("no channel capabilities");
    };
    assert_eq!(
        capabilities.output(),
        giles_proto::channel::v1::OutputMode::Blocks
    );
    assert!(capabilities.can_initiate);
}

#[tokio::test]
async fn nothing_is_served_before_configure() {
    let mut f = fixture().await;
    let status = f.open_events().await.unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
    let status = f
        .channel
        .send(SendRequest {
            target: Some(send_request::Target::ConversationId("c".into())),
            ..SendRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
}

#[tokio::test]
async fn configure_builds_the_channel_once_and_reports_serving() {
    let mut f = fixture().await;
    for (config, code) in [
        (r#"{"fail": true}"#, Code::InvalidArgument),
        (r#"{"unknown": 1, "fail": "yes"}"#, Code::InvalidArgument),
        (r#"{"output": "deltas"}"#, Code::Internal),
        (r#"{"namespace": "other"}"#, Code::Internal),
        (r#"{"namespace": "faker"}"#, Code::Internal),
        (r#"{"fail": "yes"}"#, Code::InvalidArgument),
    ] {
        let status = f.configure(config).await.unwrap_err();
        assert_eq!(status.code(), code, "{config}: {status:?}");
    }
    let health = |service: &str| HealthCheckRequest {
        service: service.into(),
    };
    let check = f
        .health
        .check(health(channel_service_server::SERVICE_NAME))
        .await;
    assert_eq!(
        check.unwrap_err().code(),
        Code::NotFound,
        "not registered yet"
    );

    // A failed Configure leaves the plugin unconfigured, so it can be retried.
    assert_eq!(f.configure("{}").await.unwrap(), "fake:home");
    let status = f
        .health
        .check(health(channel_service_server::SERVICE_NAME))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        status.status(),
        tonic_health::pb::health_check_response::ServingStatus::Serving
    );

    let status = f.configure("{}").await.unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
}

#[tokio::test]
async fn events_are_persisted_before_they_are_sent_and_resent_until_acked() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let (acks, mut events) = f.open_events().await.unwrap();
    f.emit(message("e1"));
    f.emit(message("e2"));
    assert_eq!(next_event(&mut events).await, message("e1"));
    assert_eq!(next_event(&mut events).await, message("e2"));
    assert_eq!(f.event_files(), 2);

    acks.unbounded_send(ack("e1")).unwrap();
    f.until("the ack", |f| f.event_files() == 1).await;
    f.until("the channel's ack", |f| {
        f.seen.lock().unwrap().acks == [EventId::new("e1").unwrap()]
    })
    .await;

    // A reopened stream resends what's still unacked, then carries on with new events.
    drop((acks, events));
    let (acks, mut events) = f.open_events().await.unwrap();
    assert_eq!(next_event(&mut events).await, message("e2"));
    f.emit(message("e3"));
    assert_eq!(next_event(&mut events).await, message("e3"));
    acks.unbounded_send(ack("e2")).unwrap();
    acks.unbounded_send(ack("e3")).unwrap();
    acks.unbounded_send(ack("never-sent")).unwrap();
    f.until("both acks", |f| f.event_files() == 0).await;
    f.until("the channel's acks", |f| {
        f.seen.lock().unwrap().acks.len() == 3
    })
    .await;
    assert_eq!(
        f.seen.lock().unwrap().senders.len(),
        1,
        "one channel stream"
    );
}

#[tokio::test]
async fn each_event_is_on_disk_before_the_channel_stream_is_polled_again() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let (_acks, mut events) = f.open_events().await.unwrap();
    for id in ["e1", "e2", "e3"] {
        f.emit(message(id));
    }
    for _ in 0..3 {
        next_event(&mut events).await;
    }
    f.until("the fourth poll", |f| {
        f.seen.lock().unwrap().files_at_poll.len() == 4
    })
    .await;
    assert_eq!(f.seen.lock().unwrap().files_at_poll, [0, 1, 2, 3]);
}

#[tokio::test]
async fn unacked_events_survive_a_restart() {
    let data = Arc::new(tempfile::tempdir().unwrap());
    let mut f = fixture_in(Arc::clone(&data), true).await;
    f.configure("{}").await.unwrap();
    let (_acks, mut events) = f.open_events().await.unwrap();
    f.emit(message("e1"));
    let status = ChannelEvent::Status {
        id: EventId::new("s1").unwrap(),
        status: ChannelStatus::Connected,
    };
    f.emit(status.clone());
    next_event(&mut events).await;
    next_event(&mut events).await;
    f.shutdown.cancel();

    // A new process over the same data directory: the channel emits nothing new.
    let mut f = fixture_in(data, true).await;
    f.configure("{}").await.unwrap();
    let (_acks, mut events) = f.open_events().await.unwrap();
    assert_eq!(next_event(&mut events).await, message("e1"));
    assert_eq!(next_event(&mut events).await, status);
}

#[tokio::test]
async fn a_repeated_pending_event_is_sent_once() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let (_acks, mut events) = f.open_events().await.unwrap();
    f.emit(message("e1"));
    f.emit(message("e1"));
    f.emit(message("e2"));
    assert_eq!(next_event(&mut events).await, message("e1"));
    assert_eq!(next_event(&mut events).await, message("e2"));
    assert_eq!(f.event_files(), 2);
}

#[tokio::test]
async fn a_new_events_stream_replaces_the_old_one() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let (_old_acks, mut old) = f.open_events().await.unwrap();
    let (_acks, mut new) = f.open_events().await.unwrap();
    let ended = tokio::time::timeout(WAIT, old.message()).await.unwrap();
    assert!(matches!(ended, Ok(None)), "{ended:?}");
    f.emit(message("e1"));
    assert_eq!(next_event(&mut new).await, message("e1"));
}

#[tokio::test]
async fn shutdown_ends_the_events_stream() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let (_acks, mut events) = f.open_events().await.unwrap();
    f.plugin.shutdown(ShutdownRequest {}).await.unwrap();
    let ended = tokio::time::timeout(WAIT, events.message()).await.unwrap();
    assert!(matches!(ended, Ok(None)), "{ended:?}");
}

#[tokio::test]
async fn an_ended_channel_stream_restarts_with_the_next_events_call() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let (_acks, mut events) = f.open_events().await.unwrap();
    f.emit(message("e1"));
    next_event(&mut events).await;
    f.seen.lock().unwrap().senders.clear();
    // The pump notices the end asynchronously; until then, a new stream reuses it.
    let mut events = loop {
        let (_acks, events) = f.open_events().await.unwrap();
        if !f.seen.lock().unwrap().senders.is_empty() {
            break events;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(next_event(&mut events).await, message("e1"), "unacked");
    f.emit(message("e2"));
    assert_eq!(next_event(&mut events).await, message("e2"));
    assert_eq!(f.seen.lock().unwrap().senders.len(), 1, "a second stream");
}

#[tokio::test]
async fn an_erroring_channel_stream_is_polled_with_backoff() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let (_acks, mut events) = f.open_events().await.unwrap();
    {
        let seen = f.seen.lock().unwrap();
        let sender = seen.senders.last().unwrap();
        for _ in 0..3 {
            sender
                .unbounded_send(Err(Error::unavailable("source down")))
                .unwrap();
        }
        sender.unbounded_send(Ok(message("e1"))).unwrap();
    }
    // Without a backoff all four items are taken at once; with 100 ms, then 200 ms, at most the
    // first three polls happen in 250 ms.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let polls = f.seen.lock().unwrap().files_at_poll.len();
    assert!(polls <= 3, "{polls} polls");
    // The good event still gets through after the backoff.
    assert_eq!(next_event(&mut events).await, message("e1"));
}

#[test]
fn retry_delays_double_up_to_ten_seconds() {
    let mut delay = None;
    let delays: Vec<u64> = (0..10)
        .map(|_| {
            delay = Some(next_delay(delay));
            u64::try_from(delay.unwrap().as_millis()).unwrap()
        })
        .collect();
    assert_eq!(
        delays,
        [100, 200, 400, 800, 1600, 3200, 6400, 10_000, 10_000, 10_000]
    );
}

#[test]
fn namespaces_follow_the_cores_rule() {
    for ok in ["echo", "echo:x", "echo:libera.chat", "echo:a:b"] {
        assert!(namespace_is_valid("echo", ok), "{ok}");
    }
    for bad in [
        "",
        "ech",
        "echoer",
        "Echo",
        "echo:",
        "echo::x",
        "echo:a b",
        "echo:\u{7}",
        "x:echo",
    ] {
        assert!(!namespace_is_valid("echo", bad), "{bad:?}");
    }
    assert!(!namespace_is_valid("", ":x"));
}

#[test]
fn config_errors_name_the_key() {
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Config {
        server: String,
        tls: Tls,
    }
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Tls {
        verify: bool,
    }
    let data = tempfile::tempdir().unwrap();
    let context = |config: serde_json::Value| Configure {
        instance: "home".into(),
        config,
        data_dir: data.path().to_owned(),
        blobs: Blobs::new(Arc::new(SdkDirs::open(data.path()).unwrap())),
    };
    let message = |config| match context(config).parse_config::<Config>() {
        Err(Error::InvalidRequest(message)) => message,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        message(serde_json::json!({"server": "s", "tls": {"verify": "yes"}})),
        "invalid config: tls.verify: invalid type: string \"yes\", expected a boolean"
    );
    assert_eq!(
        message(serde_json::json!({"server": 1, "tls": {"verify": true}})),
        "invalid config: server: invalid type: integer `1`, expected a string"
    );
    assert!(
        message(serde_json::json!({"server": "s", "tls": {"verify": true, "x": 1}}))
            .starts_with("invalid config: tls.x: unknown field `x`"),
    );
    assert!(
        message(serde_json::json!({"tls": {"verify": true}})).contains("missing field `server`")
    );
    let config: Config = context(serde_json::json!({"server": "s", "tls": {"verify": true}}))
        .parse_config()
        .unwrap();
    assert!(config.tls.verify);
}

fn send_to(target: send_request::Target, markdown: &str) -> SendRequest {
    SendRequest {
        target: Some(target),
        markdown: markdown.into(),
        attachments: vec![],
        reply_to: None,
    }
}

#[tokio::test]
async fn send_reaches_the_channel_and_maps_its_errors() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let id = f
        .channel
        .send(send_to(
            send_request::Target::ConversationId("dm-alice".into()),
            "**hi**",
        ))
        .await
        .unwrap()
        .into_inner()
        .message_id;
    assert_eq!(id, "m1");
    let id = f
        .channel
        .send(send_to(
            send_request::Target::Recipient("alice".into()),
            "hey",
        ))
        .await
        .unwrap()
        .into_inner()
        .message_id;
    assert_eq!(id, "m2");
    {
        let seen = f.seen.lock().unwrap();
        assert_eq!(
            seen.sent[0].0.target,
            SendTarget::Conversation(ConversationId::new("dm-alice").unwrap())
        );
        assert_eq!(seen.sent[0].0.markdown, "**hi**");
        assert_eq!(
            seen.sent[1].0.target,
            SendTarget::Recipient(NativeId::new("alice").unwrap())
        );
    }

    let status = f
        .channel
        .send(send_to(
            send_request::Target::Recipient("offline".into()),
            "hey",
        ))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable);
    assert!(status.message().contains("offline is not online"));

    let status = f.channel.send(SendRequest::default()).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn recipients_need_can_initiate() {
    let mut f = fixture_in(Arc::new(tempfile::tempdir().unwrap()), false).await;
    f.configure("{}").await.unwrap();
    let status = f
        .channel
        .send(send_to(
            send_request::Target::Recipient("alice".into()),
            "hi",
        ))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(f.seen.lock().unwrap().sent.is_empty());
}

#[tokio::test]
async fn send_reads_blobs_the_core_put_and_frees_them_afterwards() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let chunk = |part| PutBlobRequest {
        chunk: Some(BlobChunk { part: Some(part) }),
    };
    let blob = f
        .plugin
        .put_blob(stream::iter([
            chunk(blob_chunk::Part::Header(BlobRef {
                id: String::new(),
                media_type: "text/plain".into(),
                size: 5,
                name: Some("a.txt".into()),
            })),
            chunk(blob_chunk::Part::Data(b"hello".to_vec())),
        ]))
        .await
        .unwrap()
        .into_inner()
        .blob
        .unwrap();
    let outbound = outbound_file(f.data.path(), &blob.id);
    assert!(outbound.exists());

    let mut request = send_to(
        send_request::Target::ConversationId("dm-alice".into()),
        "see attached",
    );
    request.attachments = vec![blob.clone()];
    f.channel.send(request.clone()).await.unwrap();
    {
        let seen = f.seen.lock().unwrap();
        let (message, attachments) = &seen.sent[0];
        assert_eq!(message.attachments[0].name.as_deref(), Some("a.txt"));
        assert_eq!(attachments, &[b"hello".to_vec()]);
    }
    f.until("the outbound blob to go", |_| !outbound.exists())
        .await;

    // The handle was good for that one Send only.
    let status = f.channel.send(request).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("unknown blob handle"));
}

/// Uploads `hello` as the core does before a `Send`.
async fn put_hello(plugin: &mut PluginServiceClient<GrpcChannel>) -> BlobRef {
    let chunk = |part| PutBlobRequest {
        chunk: Some(BlobChunk { part: Some(part) }),
    };
    plugin
        .put_blob(stream::iter([
            chunk(blob_chunk::Part::Header(BlobRef {
                id: String::new(),
                media_type: "text/plain".into(),
                size: 5,
                name: None,
            })),
            chunk(blob_chunk::Part::Data(b"hello".to_vec())),
        ]))
        .await
        .unwrap()
        .into_inner()
        .blob
        .unwrap()
}

#[tokio::test]
async fn overlapping_sends_sharing_a_blob_keep_it_until_both_end() {
    let mut f = fixture().await;
    f.configure("{}").await.unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    f.seen.lock().unwrap().gate = Some(Arc::clone(&gate));
    // Two uploads of the same bytes, one per Send, as from two conversations.
    let blob = put_hello(&mut f.plugin).await;
    assert_eq!(put_hello(&mut f.plugin).await, blob);

    let send = |markdown: &str| {
        let mut request = send_to(
            send_request::Target::ConversationId("dm-alice".into()),
            markdown,
        );
        request.attachments = vec![blob.clone(), blob.clone()];
        request
    };
    let mut held_client = f.channel.clone();
    let held = tokio::spawn({
        let request = send("hold");
        async move { held_client.send(request).await }
    });
    f.until("the first Send to be in flight", |f| {
        f.seen.lock().unwrap().holding == 1
    })
    .await;
    // The second Send finishes first and lets go of its reference.
    f.channel.send(send("quick")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let outbound = outbound_file(f.data.path(), &blob.id);
    assert!(outbound.exists(), "the held Send still uses it");

    gate.add_permits(1);
    held.await.unwrap().unwrap();
    {
        let seen = f.seen.lock().unwrap();
        assert_eq!(seen.sent.len(), 2);
        for (_, attachments) in &seen.sent {
            assert_eq!(attachments, &[b"hello".to_vec(), b"hello".to_vec()]);
        }
    }
    f.until("the last reference to go", |_| !outbound.exists())
        .await;
}

fn outbound_file(data: &Path, id: &str) -> PathBuf {
    data.join("giles-sdk/outbound").join(id)
}
