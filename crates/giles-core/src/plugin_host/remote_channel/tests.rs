//! `RemoteChannel` against a fake plugin served in this process over a socket pair: protocol
//! violations and failures the SDK-based echo plugin can't produce. The echo plugin's end-to-end
//! tests are in `plugins/giles-channel-echo/tests/remote_channel.rs`.

use std::collections::HashMap;
use std::os::unix::net::UnixStream as StdUnixStream;

use giles_proto::channel::v1::channel_service_server::{ChannelService, ChannelServiceServer};
use giles_proto::channel::v1::{ChannelCapabilities, SendRequest, SendResponse};
use giles_proto::plugin::v1::plugin_service_server::{PluginService, PluginServiceServer};
use giles_proto::plugin::v1::{
    ConfigureRequest, ConfigureResponse, DescribeRequest, DescribeResponse, GetBlobRequest,
    PluginKind, PutBlobResponse, ReleaseBlobResponse, ShutdownRequest, ShutdownResponse,
};
use giles_proto::types::v1::{ConversationKind as PbKind, ConversationRef as PbConversationRef};
use giles_store::Store;
use giles_types::{ChannelStatus, ConversationId, MediaId, NativeId, OutputMode, SendTarget};
use sha2::{Digest as _, Sha256};
use tokio::time::Instant;
use tonic::{Request, Response};

use super::*;

/// What the fake plugin does and saw.
#[derive(Default)]
struct State {
    /// `GetBlob` answers by handle: the parts to stream, or an error status.
    transfers: HashMap<String, std::result::Result<Vec<blob_chunk::Part>, Status>>,
    /// Handles asked for with `GetBlob`.
    gets: Vec<String>,
    /// Handles released, in order. (The SDK keeps a blob unacked events still use, so a release
    /// doesn't change what `GetBlob` answers here.)
    released: Vec<String>,
    /// `PutBlob` uploads: the header and the data.
    puts: Vec<(BlobRef, Vec<u8>)>,
    /// `Send` requests.
    sends: Vec<SendRequest>,
    /// What `Send` fails with, if anything.
    send_error: Option<Status>,
}

type Events = BoxStream<'static, std::result::Result<EventsResponse, Status>>;

struct Fake {
    state: Arc<Mutex<State>>,
    events: Mutex<Option<mpsc::UnboundedReceiver<std::result::Result<EventsResponse, Status>>>>,
    acks: mpsc::UnboundedSender<String>,
}

#[derive(Clone)]
struct FakeService(Arc<Fake>);

#[async_trait]
impl PluginService for FakeService {
    type GetBlobStream = BoxStream<'static, std::result::Result<GetBlobResponse, Status>>;

    async fn describe(
        &self,
        _: Request<DescribeRequest>,
    ) -> std::result::Result<Response<DescribeResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }

    async fn configure(
        &self,
        _: Request<ConfigureRequest>,
    ) -> std::result::Result<Response<ConfigureResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }

    async fn shutdown(
        &self,
        _: Request<ShutdownRequest>,
    ) -> std::result::Result<Response<ShutdownResponse>, Status> {
        Err(Status::unimplemented("not in this test"))
    }

    async fn put_blob(
        &self,
        request: Request<Streaming<PutBlobRequest>>,
    ) -> std::result::Result<Response<PutBlobResponse>, Status> {
        let mut chunks = request.into_inner();
        let mut header = None;
        let mut data = Vec::new();
        while let Some(chunk) = chunks.message().await? {
            match chunk.chunk.and_then(|c| c.part) {
                Some(blob_chunk::Part::Header(h)) => header = Some(h),
                Some(blob_chunk::Part::Data(d)) => data.extend(d),
                None => return Err(Status::invalid_argument("empty chunk")),
            }
        }
        let header = header.ok_or_else(|| Status::invalid_argument("no header"))?;
        let mut state = lock(&self.0.state);
        let handle = format!("put-{}", state.puts.len() + 1);
        let blob = BlobRef {
            id: handle,
            ..header.clone()
        };
        state.puts.push((header, data));
        Ok(Response::new(PutBlobResponse { blob: Some(blob) }))
    }

    async fn get_blob(
        &self,
        request: Request<GetBlobRequest>,
    ) -> std::result::Result<Response<Self::GetBlobStream>, Status> {
        let id = request.into_inner().id;
        if id == HANG {
            return std::future::pending().await;
        }
        let mut state = lock(&self.0.state);
        state.gets.push(id.clone());
        let parts = state
            .transfers
            .get(&id)
            .cloned()
            .unwrap_or_else(|| Err(Status::not_found(format!("no blob {id}"))))?;
        let responses = parts.into_iter().map(|part| {
            Ok(GetBlobResponse {
                chunk: Some(BlobChunk { part: Some(part) }),
            })
        });
        Ok(Response::new(stream::iter(responses).boxed()))
    }

    async fn release_blob(
        &self,
        request: Request<ReleaseBlobRequest>,
    ) -> std::result::Result<Response<ReleaseBlobResponse>, Status> {
        lock(&self.0.state)
            .released
            .extend(request.into_inner().ids);
        Ok(Response::new(ReleaseBlobResponse {}))
    }
}

#[async_trait]
impl ChannelService for FakeService {
    type EventsStream = Events;

    async fn events(
        &self,
        request: Request<Streaming<EventsRequest>>,
    ) -> std::result::Result<Response<Events>, Status> {
        let events = lock(&self.0.events)
            .take()
            .ok_or_else(|| Status::unavailable("one stream per test"))?;
        let mut requests = request.into_inner();
        let acks = self.0.acks.clone();
        tokio::spawn(async move {
            while let Ok(Some(request)) = requests.message().await {
                if let Some(events_request::Request::Ack(ack)) = request.request {
                    let _ = acks.unbounded_send(ack.event_id);
                }
            }
        });
        Ok(Response::new(events.boxed()))
    }

    async fn send(
        &self,
        request: Request<SendRequest>,
    ) -> std::result::Result<Response<SendResponse>, Status> {
        let mut state = lock(&self.0.state);
        state.sends.push(request.into_inner());
        match state.send_error.clone() {
            Some(status) => Err(status),
            None => Ok(Response::new(SendResponse {
                message_id: "m-1".into(),
            })),
        }
    }
}

/// A secret of the instance, which must never reach a log or an error.
const SECRET: &str = "hunter2-secret";

struct Harness {
    channel: RemoteChannel,
    state: Arc<Mutex<State>>,
    events: mpsc::UnboundedSender<std::result::Result<EventsResponse, Status>>,
    acks: mpsc::UnboundedReceiver<String>,
    blobs: BlobStore,
    _dir: tempfile::TempDir,
}

impl Harness {
    async fn new(max_blob: u64, state: State) -> Self {
        let (core_end, plugin_end) = StdUnixStream::pair().unwrap();
        plugin_end.set_nonblocking(true).unwrap();
        let io = tokio::net::UnixStream::from_std(plugin_end).unwrap();
        let (events, events_rx) = mpsc::unbounded();
        let (acks_tx, acks) = mpsc::unbounded();
        let state = Arc::new(Mutex::new(state));
        let service = FakeService(Arc::new(Fake {
            state: Arc::clone(&state),
            events: Mutex::new(Some(events_rx)),
            acks: acks_tx,
        }));
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(PluginServiceServer::new(service.clone()))
                .add_service(ChannelServiceServer::new(service))
                .serve_with_incoming(
                    stream::iter([Ok::<_, io::Error>(io)]).chain(stream::pending()),
                ),
        );
        let connection = PluginConnection::connect(core_end, &Secret::new("token"))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs"), store)
            .await
            .unwrap();
        let describe = DescribeResponse {
            kind: PluginKind::Channel.into(),
            name: "fake".into(),
            version: "1.0".into(),
            requested_grants: None,
            capabilities: Some(describe_response::Capabilities::Channel(
                ChannelCapabilities {
                    output: pb::OutputMode::Deltas.into(),
                    can_initiate: true,
                },
            )),
        };
        let secrets: Arc<[Secret]> = Arc::from(vec![Secret::new(SECRET)]);
        let parts = Parts {
            instance: &InstanceName::new("fake").unwrap(),
            connection: &connection,
            describe: &describe,
            identity_namespace: "fake:ns",
            secrets: &secrets,
            span: &tracing::Span::none(),
        };
        let channel = RemoteChannel::from_parts(&parts, blobs.clone(), max_blob).unwrap();
        Self {
            channel,
            state,
            events,
            acks,
            blobs,
            _dir: dir,
        }
    }

    fn push(&self, event: EventsResponse) {
        self.events.unbounded_send(Ok(event)).unwrap();
    }

    async fn next_ack(&mut self) -> String {
        tokio::time::timeout(WAIT, self.acks.next())
            .await
            .expect("an ack")
            .unwrap()
    }

    /// Waits until `handles` are released (as a set).
    async fn released(&self, handles: &[&str]) {
        let deadline = Instant::now() + WAIT;
        loop {
            let mut released = lock(&self.state).released.clone();
            released.sort();
            let mut expected: Vec<String> = handles.iter().map(|&h| h.to_owned()).collect();
            expected.sort();
            if released == expected {
                return;
            }
            assert!(Instant::now() < deadline, "released {released:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Upper bound for anything that must not hang.
const WAIT: Duration = Duration::from_secs(10);

/// A handle whose `GetBlob` never answers.
const HANG: &str = "hang";

fn message(id: &str, text: &str, attachments: Vec<BlobRef>) -> EventsResponse {
    EventsResponse {
        event: Some(pb::ChannelEvent {
            event_id: id.into(),
            event: Some(pb::channel_event::Event::Message(pb::InboundMessage {
                conversation: Some(PbConversationRef {
                    id: "c1".into(),
                    kind: PbKind::Direct.into(),
                    title: None,
                }),
                sender: "alice".into(),
                sender_display_name: None,
                text: text.into(),
                attachments,
                reply_to: None,
                sent_at: None,
                sender_authenticated: true,
                sender_aliases: vec![],
                message_id: None,
            })),
        }),
    }
}

fn blob_ref(handle: &str, size: u64) -> BlobRef {
    BlobRef {
        id: handle.into(),
        media_type: "text/plain".into(),
        size,
        name: Some("a.txt".into()),
    }
}

/// A `GetBlob` answer: a header announcing `size` bytes, then `data` chunks.
fn transfer(size: u64, data: &[&[u8]]) -> std::result::Result<Vec<blob_chunk::Part>, Status> {
    let header = blob_chunk::Part::Header(BlobRef {
        id: "ignored".into(),
        media_type: "application/octet-stream".into(),
        size,
        name: None,
    });
    Ok(std::iter::once(header)
        .chain(data.iter().map(|d| blob_chunk::Part::Data(d.to_vec())))
        .collect())
}

fn sha256(bytes: &[u8]) -> MediaId {
    MediaId::from_digest(Sha256::digest(bytes).into())
}

fn into_message(event: ChannelEvent) -> (EventId, InboundMessage) {
    match event {
        ChannelEvent::Message { id, message } => (id, message),
        other => panic!("not a message: {other:?}"),
    }
}

async fn next(events: &mut BoxStream<'static, Result<ChannelEvent>>) -> Result<ChannelEvent> {
    tokio::time::timeout(WAIT, events.next())
        .await
        .expect("an event")
        .expect("the stream is open")
}

#[tokio::test]
async fn info_comes_from_describe_and_configure() {
    let h = Harness::new(100, State::default()).await;
    assert_eq!(
        h.channel.info(),
        &ChannelInfo {
            identity_namespace: "fake:ns".into(),
            output: OutputMode::Deltas,
            can_initiate: true,
        }
    );
}

#[tokio::test]
async fn events_reach_the_core_and_acks_reach_the_plugin() {
    let mut h = Harness::new(100, State::default()).await;
    let mut events = h.channel.events().await.unwrap();
    h.push(message("e1", "hello", vec![]));
    h.push(EventsResponse {
        event: Some(pb::ChannelEvent {
            event_id: "s1".into(),
            event: Some(pb::channel_event::Event::Status(pb::ChannelStatus {
                state: Some(pb::channel_status::State::Connected(pb::Connected {})),
            })),
        }),
    });
    let (id, message) = into_message(next(&mut events).await.unwrap());
    assert_eq!(id.as_str(), "e1");
    assert_eq!(message.text, "hello");
    assert_eq!(message.sender.as_str(), "alice");
    let status = next(&mut events).await.unwrap();
    assert_eq!(
        status,
        ChannelEvent::Status {
            id: EventId::new("s1").unwrap(),
            status: ChannelStatus::Connected,
        }
    );
    // Nothing is acked until the core says so.
    assert!(h.acks.try_recv().is_err());
    h.channel.ack(&id).await.unwrap();
    assert_eq!(h.next_ack().await, "e1");
}

#[tokio::test]
async fn unknown_and_invalid_events_are_acked_and_skipped() {
    let mut h = Harness::new(100, State::default()).await;
    let mut events = h.channel.events().await.unwrap();
    // A variant from a newer protocol decodes as no variant.
    h.push(EventsResponse {
        event: Some(pb::ChannelEvent {
            event_id: "unknown-1".into(),
            event: None,
        }),
    });
    // Invalid: no conversation. Its attachment is released, never fetched.
    let mut invalid = message("invalid-1", "x", vec![blob_ref("h-invalid", 1)]);
    if let Some(pb::channel_event::Event::Message(m)) = &mut invalid.event.as_mut().unwrap().event {
        m.conversation = None;
    }
    h.push(invalid);
    h.push(message("ok-1", "fine", vec![]));
    let (id, _) = into_message(next(&mut events).await.unwrap());
    assert_eq!(id.as_str(), "ok-1");
    assert_eq!(h.next_ack().await, "unknown-1");
    assert_eq!(h.next_ack().await, "invalid-1");
    h.released(&["h-invalid"]).await;
    assert!(lock(&h.state).gets.is_empty());
}

#[tokio::test]
async fn attachments_are_fetched_hashed_and_released_after_the_ack() {
    // The handle claims to be the SHA-256 of other content: it's only a handle.
    let claimed = sha256(b"not these bytes").to_string();
    let mut state = State::default();
    state
        .transfers
        .insert(claimed.clone(), transfer(11, &[b"hello", b" world"]));
    let mut h = Harness::new(100, state).await;
    let mut events = h.channel.events().await.unwrap();
    h.push(message("e1", "see attached", vec![blob_ref(&claimed, 11)]));
    let (id, message) = into_message(next(&mut events).await.unwrap());
    let stored = sha256(b"hello world");
    assert_eq!(
        message.attachments,
        [MediaRef {
            id: stored.clone(),
            media_type: "text/plain".into(),
            size: 11,
            name: Some("a.txt".into()),
        }]
    );
    assert_eq!(message.text, "see attached");
    let blob = h.blobs.stat(&stored).await.unwrap().unwrap();
    assert_eq!(blob.size, 11);
    assert!(
        h.blobs
            .stat(&sha256(b"not these bytes"))
            .await
            .unwrap()
            .is_none()
    );

    // Kept until the core has stored the event and acked it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(lock(&h.state).released.is_empty());
    h.channel.ack(&id).await.unwrap();
    assert_eq!(h.next_ack().await, "e1");
    h.released(&[&claimed]).await;
}

#[tokio::test]
async fn each_event_s_handles_are_released_right_after_its_ack() {
    let mut state = State::default();
    state.transfers.insert("same".into(), transfer(2, &[b"hi"]));
    state
        .transfers
        .insert("other".into(), transfer(2, &[b"yo"]));
    let h = Harness::new(100, state).await;
    let mut events = h.channel.events().await.unwrap();
    h.push(message(
        "e1",
        "one",
        vec![blob_ref("same", 2), blob_ref("other", 2)],
    ));
    h.push(message("e2", "two", vec![blob_ref("same", 2)]));
    let (first, message) = into_message(next(&mut events).await.unwrap());
    assert_eq!(message.attachments.len(), 2);
    h.channel.ack(&first).await.unwrap();
    // Not waiting for anything: the SDK keeps "same" for e2.
    h.released(&["same", "other"]).await;
    let (second, message) = into_message(next(&mut events).await.unwrap());
    assert_eq!(message.attachments.len(), 1, "{}", message.text);
    // Nothing of e2's is released before its ack.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(lock(&h.state).released.len(), 2);
    h.channel.ack(&second).await.unwrap();
    h.released(&["same", "other", "same"]).await;
    assert_eq!(lock(&h.state).gets, ["same", "other", "same"]);
}

#[tokio::test(start_paused = true)]
async fn a_get_blob_that_never_answers_leaves_the_event_unacked() {
    let mut h = Harness::new(100, State::default()).await;
    let mut events = h.channel.events().await.unwrap();
    h.push(message("e1", "hi", vec![blob_ref(HANG, 2)]));
    let start = Instant::now();
    let next = tokio::time::timeout(BLOB_CHUNK_TIMEOUT * 2, events.next()).await;
    let error = next.expect("the call times out").unwrap().unwrap_err();
    assert_eq!(start.elapsed(), BLOB_CHUNK_TIMEOUT);
    assert!(error.is_retryable(), "{error}");
    assert!(h.acks.try_recv().is_err());
}

#[tokio::test]
async fn an_attachment_over_max_blob_is_refused_without_a_transfer() {
    let mut state = State::default();
    state
        .transfers
        .insert("big".into(), transfer(5, &[b"12345"]));
    let h = Harness::new(4, state).await;
    let mut events = h.channel.events().await.unwrap();
    h.push(message("e1", "look", vec![blob_ref("big", 5)]));
    let (id, message) = into_message(next(&mut events).await.unwrap());
    assert!(message.attachments.is_empty());
    assert_eq!(
        message.text,
        "look\n\n[attachment \"a.txt\" (text/plain, 5 bytes) not received: it is larger than \
         this channel's limit of 4 bytes]"
    );
    assert!(lock(&h.state).gets.is_empty());
    h.channel.ack(&id).await.unwrap();
    h.released(&["big"]).await;
}

#[tokio::test]
async fn a_header_over_max_blob_is_refused_before_any_data() {
    let mut state = State::default();
    // The event understates the size; the header tells the truth.
    state
        .transfers
        .insert("big".into(), transfer(5, &[b"12345"]));
    let h = Harness::new(4, state).await;
    let mut events = h.channel.events().await.unwrap();
    h.push(message("e1", "", vec![blob_ref("big", 3)]));
    let (_, message) = into_message(next(&mut events).await.unwrap());
    assert!(message.attachments.is_empty());
    assert!(
        message
            .text
            .ends_with("not received: it is larger than this channel's limit of 4 bytes]"),
        "{}",
        message.text
    );
    assert!(h.blobs.stat(&sha256(b"12345")).await.unwrap().is_none());
}

#[tokio::test]
async fn a_transfer_that_breaks_its_header_is_refused() {
    let mut state = State::default();
    // More data than the header announced.
    state
        .transfers
        .insert("long".into(), transfer(3, &[b"he", b"llo"]));
    // Less.
    state
        .transfers
        .insert("short".into(), transfer(10, &[b"hello"]));
    // No header.
    state.transfers.insert(
        "headless".into(),
        Ok(vec![blob_chunk::Part::Data(b"hello".to_vec())]),
    );
    // Two headers.
    let mut twice = transfer(5, &[b"hello"]).unwrap();
    twice.insert(1, twice[0].clone());
    state.transfers.insert("twice".into(), Ok(twice));
    let h = Harness::new(100, state).await;
    let mut events = h.channel.events().await.unwrap();
    let handles = ["long", "short", "headless", "twice"];
    h.push(message(
        "e1",
        "four",
        handles.iter().map(|&handle| blob_ref(handle, 5)).collect(),
    ));
    let (_, message) = into_message(next(&mut events).await.unwrap());
    assert!(message.attachments.is_empty());
    assert_eq!(
        message
            .text
            .matches("not received: the channel sent it incorrectly]")
            .count(),
        4,
        "{}",
        message.text
    );
    for bytes in [&b"hel"[..], b"hello", b"he"] {
        assert!(h.blobs.stat(&sha256(bytes)).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn a_blob_the_plugin_no_longer_has_leaves_a_note() {
    let h = Harness::new(100, State::default()).await;
    let mut events = h.channel.events().await.unwrap();
    h.push(message("e1", "hi", vec![blob_ref("gone", 2)]));
    let (_, message) = into_message(next(&mut events).await.unwrap());
    assert!(message.attachments.is_empty());
    assert!(
        message
            .text
            .ends_with("not received: the channel no longer has it]"),
        "{}",
        message.text
    );
}

#[tokio::test]
async fn a_transfer_that_may_pass_leaves_the_event_unacked() {
    let mut state = State::default();
    state.transfers.insert(
        "flaky".into(),
        Err(Status::unavailable(format!("disk busy {SECRET}"))),
    );
    let mut h = Harness::new(100, state).await;
    let mut events = h.channel.events().await.unwrap();
    h.push(message("e1", "hi", vec![blob_ref("flaky", 2)]));
    let error = next(&mut events).await.unwrap_err();
    assert!(error.is_retryable(), "{error}");
    assert!(!error.to_string().contains(SECRET), "{error}");
    // The stream ends after the failure, so the core reopens it and the plugin resends.
    assert!(
        tokio::time::timeout(WAIT, events.next())
            .await
            .unwrap()
            .is_none()
    );
    assert!(h.acks.try_recv().is_err());
    assert!(lock(&h.state).released.is_empty());
}

#[tokio::test]
async fn send_uploads_attachments_then_sends_their_handles() {
    let h = Harness::new(100, State::default()).await;
    let meta = BlobMeta {
        media_type: "text/plain".into(),
        name: Some("note.txt".into()),
    };
    let media = h
        .blobs
        .put(&b"attached"[..], 100, meta, Timestamp::now())
        .await
        .unwrap();
    let id = h
        .channel
        .send(OutboundMessage {
            target: SendTarget::Recipient(NativeId::new("bob").unwrap()),
            markdown: "**hi**".into(),
            attachments: vec![media.clone()],
            reply_to: Some(MessageId::new("m-0").unwrap()),
        })
        .await
        .unwrap();
    assert_eq!(id.as_str(), "m-1");
    let state = lock(&h.state);
    let (header, data) = &state.puts[0];
    // The header carries the core's id, so the plugin can check the bytes.
    assert_eq!(header.id, media.id.as_str());
    assert_eq!(header.size, 8);
    assert_eq!(header.name.as_deref(), Some("note.txt"));
    assert_eq!(data, b"attached");
    let sent = &state.sends[0];
    assert_eq!(
        sent.target,
        Some(pb::send_request::Target::Recipient("bob".into()))
    );
    assert_eq!(sent.markdown, "**hi**");
    assert_eq!(sent.reply_to.as_deref(), Some("m-0"));
    assert_eq!(sent.attachments.len(), 1);
    assert_eq!(sent.attachments[0].id, "put-1");
}

#[tokio::test]
async fn send_errors_keep_their_meaning_and_hide_secrets() {
    let state = State {
        send_error: Some(Status::unavailable(format!("bob is offline {SECRET}"))),
        ..State::default()
    };
    let h = Harness::new(100, state).await;
    let send = |target| {
        h.channel.send(OutboundMessage {
            target,
            markdown: "hi".into(),
            attachments: vec![],
            reply_to: None,
        })
    };
    let error = send(SendTarget::Conversation(ConversationId::new("c1").unwrap()))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Unavailable { .. }), "{error:?}");
    assert!(error.to_string().contains("bob is offline"), "{error}");
    assert!(!error.to_string().contains(SECRET), "{error}");

    lock(&h.state).send_error = Some(Status::internal("boom"));
    let error = send(SendTarget::Conversation(ConversationId::new("c1").unwrap()))
        .await
        .unwrap_err();
    assert!(!error.is_retryable(), "{error:?}");

    // An attachment the core doesn't have fails before anything is sent.
    lock(&h.state).send_error = None;
    let missing = MediaRef {
        id: sha256(b"never stored"),
        media_type: "text/plain".into(),
        size: 1,
        name: None,
    };
    let error = h
        .channel
        .send(OutboundMessage {
            target: SendTarget::Conversation(ConversationId::new("c1").unwrap()),
            markdown: "hi".into(),
            attachments: vec![missing],
            reply_to: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    assert_eq!(lock(&h.state).sends.len(), 2);
}

#[tokio::test]
async fn acks_need_an_open_stream() {
    let h = Harness::new(100, State::default()).await;
    // Before any stream, there's nowhere to ack.
    let error = h
        .channel
        .ack(&EventId::new("e1").unwrap())
        .await
        .unwrap_err();
    assert!(error.is_retryable(), "{error}");
}

#[test]
fn refused_notes_name_the_attachment() {
    let mut blob = blob_ref("h", 3);
    assert_eq!(
        refused_note(&blob, "why"),
        "[attachment \"a.txt\" (text/plain, 3 bytes) not received: why]"
    );
    blob.name = None;
    assert_eq!(
        refused_note(&blob, "why"),
        "[attachment (text/plain, 3 bytes) not received: why]"
    );
}
