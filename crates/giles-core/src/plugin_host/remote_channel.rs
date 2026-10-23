//! The `RemoteChannel` adapter (PLAN sections 4.3, 8.3, 8.5): a launched plugin's
//! `ChannelService` as a [`Channel`].
//!
//! Inbound events are converted with the SDK's conversions (`giles_plugin_sdk::convert`) after the
//! attachments are taken out: each attachment's bytes are fetched with `GetBlob` and stored in the
//! core's blob store, which hashes them, so a plugin's handle is never taken as a claim about the
//! content. Each event's handles are released (`ReleaseBlob`) right after the event is acked
//! (PLAN section 8.3); the SDK keeps a blob another unacked event still refers to. Outbound
//! attachments are uploaded with `PutBlob` right before the `Send` that uses them.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::channel::mpsc;
use futures::stream::{self, BoxStream, Stream, StreamExt as _};
use giles_plugin_sdk::convert::{event_from_proto, outbound_to_proto, output_mode_from_proto};
use giles_proto::MAX_BLOB_CHUNK_SIZE;
use giles_proto::channel::v1::{
    self as pb, EventAck, EventsRequest, EventsResponse, events_request,
};
use giles_proto::plugin::v1::{
    BlobChunk, GetBlobRequest, GetBlobResponse, PutBlobRequest, ReleaseBlobRequest, blob_chunk,
    describe_response,
};
use giles_proto::types::v1::BlobRef;
use giles_store::{BlobMeta, BlobReader, BlobStore};
use giles_types::{
    Channel, ChannelEvent, ChannelInfo, Error, EventId, InboundMessage, MediaRef, MessageId,
    OutboundMessage, Result, Timestamp,
};
use tokio::io::AsyncReadExt as _;
use tokio_util::io::StreamReader;
use tonic::{Code, Status, Streaming};
use tracing::{Instrument as _, debug, info, warn};

use super::{LaunchedPlugin, PluginConnection, RemotePlugin, Secrets};
use crate::config::InstanceName;

/// How long a `GetBlob` call may wait for its response, and a transfer for each chunk, before
/// the core gives up on it (and leaves the event unacked, to come again).
pub const BLOB_CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a `ReleaseBlob` call may take.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

/// A launched plugin's channel service as a [`Channel`]. One per launch: a restarted plugin gets
/// a new one.
pub struct RemoteChannel {
    info: ChannelInfo,
    shared: Arc<Shared>,
}

impl fmt::Debug for RemoteChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteChannel")
            .field("instance", &self.shared.instance)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// What the channel and its event streams share.
struct Shared {
    instance: InstanceName,
    connection: PluginConnection,
    blobs: BlobStore,
    max_blob: u64,
    secrets: Secrets,
    span: tracing::Span,
    /// The ack half of the newest `Events` stream.
    acks: Mutex<Option<mpsc::UnboundedSender<EventsRequest>>>,
    /// The plugin's blob handles of each message event yielded and not acked yet, by event id.
    unacked: Mutex<HashMap<String, Vec<String>>>,
    /// Handles for the release task.
    releases: mpsc::UnboundedSender<Vec<String>>,
}

/// What a [`RemoteChannel`] is built from: the parts of a [`LaunchedPlugin`] or a
/// [`RemotePlugin`] it uses.
pub(super) struct Parts<'a> {
    pub instance: &'a InstanceName,
    pub connection: &'a PluginConnection,
    pub describe: &'a giles_proto::plugin::v1::DescribeResponse,
    pub identity_namespace: &'a str,
    pub secrets: Secrets,
    pub span: &'a tracing::Span,
}

impl RemoteChannel {
    /// The channel of `plugin`, storing inbound attachments of at most `max_blob` bytes in
    /// `blobs`. Starts the task that releases the plugin's blobs, so it must be called inside a
    /// Tokio runtime; the task ends after the channel and its streams are gone.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if `Describe` has no channel capabilities or no valid output
    /// mode (the launch checks both already).
    pub fn new(plugin: &LaunchedPlugin, blobs: BlobStore, max_blob: u64) -> Result<Self> {
        let parts = Parts {
            instance: &plugin.instance,
            connection: &plugin.connection,
            describe: &plugin.describe,
            identity_namespace: &plugin.identity_namespace,
            secrets: Secrets::new(Arc::clone(&plugin.secrets)),
            span: &plugin.span,
        };
        Self::from_parts(&parts, blobs, max_blob)
    }

    /// [`RemoteChannel::new`] for a remote plugin.
    ///
    /// # Errors
    ///
    /// As for [`RemoteChannel::new`].
    pub fn from_remote(plugin: &RemotePlugin, blobs: BlobStore, max_blob: u64) -> Result<Self> {
        let parts = Parts {
            instance: &plugin.instance,
            connection: &plugin.connection,
            describe: &plugin.describe,
            identity_namespace: &plugin.identity_namespace,
            secrets: plugin.secrets.clone(),
            span: &plugin.span,
        };
        Self::from_parts(&parts, blobs, max_blob)
    }

    /// [`RemoteChannel::new`] from parts.
    pub(super) fn from_parts(parts: &Parts<'_>, blobs: BlobStore, max_blob: u64) -> Result<Self> {
        let Some(describe_response::Capabilities::Channel(capabilities)) =
            &parts.describe.capabilities
        else {
            return Err(Error::InvalidRequest(
                "the plugin describes no channel capabilities".into(),
            ));
        };
        let info = ChannelInfo {
            identity_namespace: parts.identity_namespace.to_owned(),
            output: output_mode_from_proto(capabilities.output)?,
            can_initiate: capabilities.can_initiate,
        };
        let (releases, release_queue) = mpsc::unbounded();
        let shared = Arc::new(Shared {
            instance: parts.instance.clone(),
            connection: parts.connection.clone(),
            blobs,
            max_blob,
            secrets: parts.secrets.clone(),
            span: parts.span.clone(),
            acks: Mutex::new(None),
            unacked: Mutex::default(),
            releases,
        });
        tokio::spawn(
            release_loop(parts.connection.clone(), release_queue).instrument(parts.span.clone()),
        );
        Ok(Self { info, shared })
    }
}

#[async_trait]
impl Channel for RemoteChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        let shared = Arc::clone(&self.shared);
        let (acks, requests) = mpsc::unbounded();
        let response = shared
            .connection
            .channel()
            .events(requests)
            .instrument(shared.span.clone())
            .await
            .map_err(|status| shared.rpc_error("Events", &status))?;
        *lock(&shared.acks) = Some(acks.clone());
        let inbound = Inbound {
            shared,
            events: response.into_inner(),
            acks,
            done: false,
        };
        Ok(stream::unfold(inbound, |mut inbound| async move {
            let item = inbound.next().await?;
            Some((item, inbound))
        })
        .boxed())
    }

    async fn ack(&self, event: &EventId) -> Result<()> {
        let shared = &self.shared;
        let acks = lock(&shared.acks)
            .clone()
            .ok_or_else(|| Error::unavailable("the plugin's event stream isn't open"))?;
        acks.unbounded_send(ack_request(event.as_str()))
            .map_err(|_| Error::unavailable("the plugin's event stream is closed"))?;
        // Right after the ack (PLAN section 8.3). The ack and the release travel separately, so
        // the release may arrive first; the SDK keeps a blob its unacked events still use.
        let handles = lock(&shared.unacked).remove(event.as_str());
        if let Some(handles) = handles {
            shared.release(handles);
        }
        Ok(())
    }

    async fn send(&self, mut msg: OutboundMessage) -> Result<MessageId> {
        let shared = &self.shared;
        let attachments = std::mem::take(&mut msg.attachments);
        let mut request = outbound_to_proto(msg)?;
        for media in attachments {
            request.attachments.push(shared.put_blob(media).await?);
        }
        let response = shared
            .connection
            .channel()
            .send(request)
            .instrument(shared.span.clone())
            .await
            .map_err(|status| shared.rpc_error("Send", &status))?;
        MessageId::new(response.into_inner().message_id)
            .map_err(|_| Error::other("the plugin's Send returned an empty message id"))
    }
}

/// The ack for event `id`.
fn ack_request(id: &str) -> EventsRequest {
    EventsRequest {
        request: Some(events_request::Request::Ack(EventAck {
            event_id: id.to_owned(),
        })),
    }
}

impl Shared {
    fn scrub(&self, text: &str) -> String {
        self.secrets.scrub(text)
    }

    /// A failed call as a component error, its message scrubbed: `UNAVAILABLE` stays
    /// [`Error::Unavailable`] (the outbox retries it), and so do `CANCELLED`, `UNKNOWN`, and
    /// `DEADLINE_EXCEEDED`, which a call gets when the plugin dies or drains for a restart
    /// mid-call. `UNKNOWN` is also what the SDK sends for a plugin's own generic errors
    /// (`Error::Other`, via `status_from_error`), so those are retryable too. `INVALID_ARGUMENT`
    /// is [`Error::InvalidRequest`], `NOT_FOUND` is [`Error::NotFound`].
    fn rpc_error(&self, call: &str, status: &Status) -> Error {
        let message = self.scrub(status.message());
        match status.code() {
            Code::Unavailable => Error::unavailable(format!("{call}: {message}")),
            code @ (Code::Cancelled | Code::Unknown | Code::DeadlineExceeded) => {
                Error::unavailable(format!("{call} failed: {code:?}: {message}"))
            }
            Code::InvalidArgument => Error::InvalidRequest(format!("{call}: {message}")),
            Code::NotFound => Error::NotFound(format!("{call}: {message}")),
            code => Error::other(format!("{call} failed: {code:?}: {message}")),
        }
    }

    /// Queues `handles` for release.
    fn release(&self, handles: Vec<String>) {
        if !handles.is_empty() {
            // The task only ends after every sender is gone.
            let _ = self.releases.unbounded_send(handles);
        }
    }

    /// Fetches and stores each attachment of a message event, in order. A refused attachment
    /// (too large, a transfer that breaks the protocol, a handle the plugin doesn't have) is left
    /// out and a note about it is appended to the text, so the agent can tell the sender. Returns
    /// every handle, refused ones included, for release after the ack.
    ///
    /// # Errors
    ///
    /// A failure that may pass (the connection, the blob store): the event must stay unacked.
    async fn fetch_attachments(
        &self,
        event: &str,
        blobs: Vec<BlobRef>,
        message: &mut InboundMessage,
    ) -> Result<Vec<String>> {
        let mut handles = Vec::with_capacity(blobs.len());
        for blob in blobs {
            match self.fetch(&blob).await {
                Ok(media) => message.attachments.push(media),
                Err(Fetch::Refused { note, detail }) => {
                    warn!(
                        event,
                        handle = %self.scrub(&blob.id),
                        size = blob.size,
                        detail = %detail,
                        "attachment refused; the message goes on without it",
                    );
                    if !message.text.is_empty() {
                        message.text.push_str("\n\n");
                    }
                    message.text.push_str(&refused_note(&blob, &note));
                }
                Err(Fetch::Failed(error)) => return Err(error),
            }
            handles.push(blob.id);
        }
        Ok(handles)
    }

    /// Fetches one attachment with `GetBlob` into the blob store.
    async fn fetch(&self, blob: &BlobRef) -> Result<MediaRef, Fetch> {
        if blob.size > self.max_blob {
            return Err(self.too_large(blob.size));
        }
        let mut client = self.connection.plugin();
        let call = client.get_blob(GetBlobRequest {
            id: blob.id.clone(),
        });
        let mut chunks = match tokio::time::timeout(BLOB_CHUNK_TIMEOUT, call).await {
            Err(_) => return Err(stalled()),
            Ok(Err(status)) => return Err(self.get_blob_failed(&status)),
            Ok(Ok(response)) => response.into_inner(),
        };
        let header = match tokio::time::timeout(BLOB_CHUNK_TIMEOUT, chunks.message()).await {
            Err(_) => return Err(stalled()),
            Ok(Err(status)) => return Err(self.get_blob_failed(&status)),
            Ok(Ok(Some(GetBlobResponse {
                chunk:
                    Some(BlobChunk {
                        part: Some(blob_chunk::Part::Header(header)),
                    }),
            }))) => header,
            Ok(Ok(_)) => return Err(Fetch::invalid("the transfer doesn't start with a header")),
        };
        // Refused before reading any data: the transfer is dropped, which cancels it.
        if header.size > self.max_blob {
            return Err(self.too_large(header.size));
        }
        let failure = Arc::new(Mutex::new(None));
        let data = self.blob_data(chunks, header.size, Arc::clone(&failure));
        let media_type = if blob.media_type.is_empty() {
            header.media_type
        } else {
            blob.media_type.clone()
        };
        let meta = BlobMeta {
            media_type,
            name: blob.name.clone(),
        };
        let stored = self
            .blobs
            .put(StreamReader::new(data), header.size, meta, Timestamp::now())
            .await;
        stored.map_err(|error| {
            lock(&failure).take().unwrap_or_else(|| match error {
                giles_store::Error::TooLarge { .. } => Fetch::invalid(format!(
                    "more data than the {} bytes in the header",
                    header.size
                )),
                other => {
                    Fetch::Failed(Error::other(format!("cannot store an attachment: {other}")))
                }
            })
        })
    }

    /// The data chunks of a `GetBlob` transfer whose header announced `size` bytes. Stops with an
    /// error (and the reason in `failure`) at the first chunk past `size`, a stream that ends
    /// short of it, a second header, an error status, or no chunk for [`BLOB_CHUNK_TIMEOUT`].
    fn blob_data(
        &self,
        chunks: Streaming<GetBlobResponse>,
        size: u64,
        failure: Arc<Mutex<Option<Fetch>>>,
    ) -> impl Stream<Item = io::Result<Bytes>> + Send + Unpin + 'static {
        struct State {
            chunks: Streaming<GetBlobResponse>,
            received: u64,
            done: bool,
        }
        let secrets = self.secrets.clone();
        let state = State {
            chunks,
            received: 0,
            done: false,
        };
        Box::pin(stream::unfold(state, move |mut state| {
            let failure = Arc::clone(&failure);
            let secrets = secrets.clone();
            async move {
                if state.done {
                    return None;
                }
                let fail = |state: State, reason: Fetch| {
                    *lock(&failure) = Some(reason);
                    let error = io::Error::other("the blob transfer failed");
                    Some((
                        Err(error),
                        State {
                            done: true,
                            ..state
                        },
                    ))
                };
                loop {
                    let next = tokio::time::timeout(BLOB_CHUNK_TIMEOUT, state.chunks.message());
                    let part = match next.await {
                        Err(_) => return fail(state, stalled()),
                        Ok(Err(status)) => {
                            return fail(state, get_blob_failed(&secrets, &status));
                        }
                        Ok(Ok(None)) if state.received == size => return None,
                        Ok(Ok(None)) => {
                            let received = state.received;
                            return fail(
                                state,
                                Fetch::invalid(format!(
                                    "the transfer ended after {received} of the {size} bytes in \
                                     the header"
                                )),
                            );
                        }
                        Ok(Ok(Some(response))) => response.chunk.and_then(|chunk| chunk.part),
                    };
                    let Some(blob_chunk::Part::Data(data)) = part else {
                        return fail(state, Fetch::invalid("a second header or an empty chunk"));
                    };
                    let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
                    if data.len() > MAX_BLOB_CHUNK_SIZE || state.received + len > size {
                        return fail(
                            state,
                            Fetch::invalid(format!(
                                "more data than the {size} bytes in the header, or a chunk over \
                                 {MAX_BLOB_CHUNK_SIZE} bytes"
                            )),
                        );
                    }
                    state.received += len;
                    if !data.is_empty() {
                        return Some((Ok(Bytes::from(data)), state));
                    }
                }
            }
        }))
    }

    fn too_large(&self, size: u64) -> Fetch {
        Fetch::Refused {
            note: format!(
                "it is larger than this channel's limit of {} bytes",
                self.max_blob
            ),
            detail: format!("{size} bytes, over max_blob ({} bytes)", self.max_blob),
        }
    }

    fn get_blob_failed(&self, status: &Status) -> Fetch {
        get_blob_failed(&self.secrets, status)
    }

    /// Uploads one outbound attachment with `PutBlob` and returns the plugin's handle. The
    /// header carries the core's id (the content's SHA-256), so a plugin can check the bytes.
    async fn put_blob(&self, media: MediaRef) -> Result<BlobRef> {
        let blob = self
            .blobs
            .get(&media.id)
            .await
            .map_err(|e| Error::other(format!("cannot read attachment {}: {e}", media.id)))?
            .ok_or_else(|| Error::NotFound(format!("attachment {} is not stored", media.id)))?;
        let header = PutBlobRequest {
            chunk: Some(BlobChunk {
                part: Some(blob_chunk::Part::Header(BlobRef {
                    id: media.id.to_string(),
                    media_type: media.media_type,
                    size: blob.media.size,
                    name: media.name,
                })),
            }),
        };
        let read_error = Arc::new(Mutex::new(None));
        let requests = stream::once(async move { header })
            .chain(put_chunks(blob.reader, Arc::clone(&read_error)));
        let response = self
            .connection
            .plugin()
            .put_blob(requests)
            .instrument(self.span.clone())
            .await;
        // A read failure (a damaged blob) cut the upload short; it is the real cause.
        if let Some(error) = lock(&read_error).take() {
            return Err(Error::other(format!(
                "cannot read attachment {}: {error}",
                media.id
            )));
        }
        response
            .map_err(|status| self.rpc_error("PutBlob", &status))?
            .into_inner()
            .blob
            .ok_or_else(|| Error::other("the plugin's PutBlob returned no handle"))
    }
}

/// `blob`'s data as `PutBlob` chunks of at most [`MAX_BLOB_CHUNK_SIZE`] bytes. A read error ends
/// the stream early (so the plugin refuses the short upload) and is kept in `error`.
fn put_chunks(
    reader: BlobReader,
    error: Arc<Mutex<Option<io::Error>>>,
) -> impl Stream<Item = PutBlobRequest> + Send + 'static {
    stream::unfold(Some(reader), move |reader| {
        let error = Arc::clone(&error);
        async move {
            let mut reader = reader?;
            let mut data = Vec::with_capacity(MAX_BLOB_CHUNK_SIZE);
            let limit = u64::try_from(MAX_BLOB_CHUNK_SIZE).expect("fits in u64");
            match (&mut reader).take(limit).read_to_end(&mut data).await {
                Ok(0) => None,
                Ok(_) => Some((
                    PutBlobRequest {
                        chunk: Some(BlobChunk {
                            part: Some(blob_chunk::Part::Data(data)),
                        }),
                    },
                    Some(reader),
                )),
                Err(e) => {
                    *lock(&error) = Some(e);
                    None
                }
            }
        }
    })
}

/// Why an attachment wasn't stored.
#[derive(Debug)]
enum Fetch {
    /// Retrying won't help: the message goes on without the attachment.
    Refused {
        /// For the agent, in the note added to the message.
        note: String,
        /// For the log.
        detail: String,
    },
    /// It may pass: the event stays unacked and comes again.
    Failed(Error),
}

impl Fetch {
    /// The plugin's transfer broke the protocol.
    fn invalid(detail: impl Into<String>) -> Self {
        Self::Refused {
            note: "the channel sent it incorrectly".into(),
            detail: detail.into(),
        }
    }
}

/// No chunk for [`BLOB_CHUNK_TIMEOUT`].
fn stalled() -> Fetch {
    Fetch::Failed(Error::unavailable(format!(
        "GetBlob: no data for {BLOB_CHUNK_TIMEOUT:?}"
    )))
}

/// A `GetBlob` error status: refused if the plugin says the request or the handle is wrong
/// (asking again gets the same answer), else failed.
fn get_blob_failed(secrets: &Secrets, status: &Status) -> Fetch {
    let detail = format!(
        "GetBlob failed: {:?}: {}",
        status.code(),
        secrets.scrub(status.message())
    );
    match status.code() {
        Code::NotFound => Fetch::Refused {
            note: "the channel no longer has it".into(),
            detail,
        },
        Code::InvalidArgument
        | Code::FailedPrecondition
        | Code::OutOfRange
        | Code::PermissionDenied
        | Code::Unimplemented => Fetch::Refused {
            note: "the channel couldn't send it".into(),
            detail,
        },
        _ => Fetch::Failed(Error::unavailable(detail)),
    }
}

/// The text added to a message for an attachment that was left out.
fn refused_note(blob: &BlobRef, reason: &str) -> String {
    let name = blob
        .name
        .as_deref()
        .map(|name| format!(" {name:?}"))
        .unwrap_or_default();
    format!(
        "[attachment{name} ({}, {} bytes) not received: {reason}]",
        blob.media_type, blob.size
    )
}

/// One `Events` stream from the plugin.
struct Inbound {
    shared: Arc<Shared>,
    events: Streaming<EventsResponse>,
    /// This stream's ack half, for events the adapter acks itself.
    acks: mpsc::UnboundedSender<EventsRequest>,
    done: bool,
}

impl Inbound {
    /// The next event to hand to the core. Events that fail conversion (an unknown variant
    /// included) are logged, acked, and skipped (PLAN section 7.8).
    async fn next(&mut self) -> Option<Result<ChannelEvent>> {
        if self.done {
            return None;
        }
        let span = self.shared.span.clone();
        let item = self.next_inner().instrument(span).await;
        if matches!(item, None | Some(Err(_))) {
            self.done = true;
        }
        item
    }

    async fn next_inner(&mut self) -> Option<Result<ChannelEvent>> {
        loop {
            let response = match self.events.message().await {
                Ok(Some(response)) => response,
                Ok(None) => return None,
                Err(status) => return Some(Err(self.shared.rpc_error("Events", &status))),
            };
            let Some(mut event) = response.event else {
                warn!("an Events response without an event; ignored");
                continue;
            };
            let blobs = match &mut event.event {
                Some(pb::channel_event::Event::Message(message)) => {
                    std::mem::take(&mut message.attachments)
                }
                _ => Vec::new(),
            };
            let event_id = event.event_id.clone();
            let unknown = event.event.is_none();
            let mut event = match event_from_proto(event) {
                Ok(event) => event,
                Err(error) => {
                    let id = self.shared.scrub(&event_id);
                    if unknown {
                        info!(event = %id, "unknown channel event; acked and ignored");
                    } else {
                        let error = self.shared.scrub(&error.to_string());
                        warn!(event = %id, %error, "invalid channel event; acked and dropped");
                    }
                    self.ack_now(&event_id, blobs.into_iter().map(|blob| blob.id).collect());
                    continue;
                }
            };
            if let ChannelEvent::Message { id, message } = &mut event {
                let handles = match self
                    .shared
                    .fetch_attachments(id.as_str(), blobs, message)
                    .await
                {
                    Ok(handles) => handles,
                    Err(error) => return Some(Err(error)),
                };
                if !handles.is_empty() {
                    lock(&self.shared.unacked).insert(id.to_string(), handles);
                }
            }
            debug!(event = %event.id(), "channel event received");
            return Some(Ok(event));
        }
    }

    /// Acks an event the core won't see, and releases its blobs.
    fn ack_now(&self, id: &str, handles: Vec<String>) {
        // A closed stream fails the next read too; the event then comes again.
        if self.acks.unbounded_send(ack_request(id)).is_ok() {
            self.shared.release(handles);
        }
    }
}

/// Releases queued handles in order, batching whatever is queued when a call starts. Failures are
/// only logged: the SDK deletes a blob when the last event that uses it is acked anyway, and its
/// next start sweeps what a crash left, so a lost release costs nothing.
async fn release_loop(
    connection: PluginConnection,
    mut queue: mpsc::UnboundedReceiver<Vec<String>>,
) {
    while let Some(mut ids) = queue.next().await {
        while let Ok(more) = queue.try_recv() {
            ids.extend(more);
        }
        ids.sort_unstable();
        ids.dedup();
        debug!(count = ids.len(), "releasing the plugin's blobs");
        let mut client = connection.plugin();
        let release = client.release_blob(ReleaseBlobRequest { ids });
        match tokio::time::timeout(RELEASE_TIMEOUT, release).await {
            Ok(Ok(_)) => {}
            Ok(Err(status)) => debug!(%status, "ReleaseBlob failed"),
            Err(_) => debug!("ReleaseBlob timed out"),
        }
    }
}

/// Locks a std mutex, ignoring poisoning: every guarded value stays consistent (single inserts,
/// removes, and replacements).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
