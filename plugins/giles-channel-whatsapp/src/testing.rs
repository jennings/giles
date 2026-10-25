//! Test doubles: a fake WhatsApp client (the [`Backend`], its events, and a [`Connector`] that
//! hands it out) and an in-memory blob store.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use giles_plugin_sdk::async_trait;
use giles_plugin_sdk::giles_types::{Error, MediaId, MediaRef, Result};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use whatsapp_rust::waproto::whatsapp::message as wa_message;

use crate::backend::{
    Address, Backend, BackendError, Batch, ChatKind, Commit, Connection, Connector, Incoming,
    IncomingMedia, MediaKind, MediaPayload, OutgoingMedia, WaEvent,
};
use crate::media::MediaStore;

/// Upper bound for anything in a test that must not hang.
pub(crate) const WAIT: Duration = Duration::from_secs(10);

/// Polls `check` every 5 ms until it holds, for at most [`WAIT`].
pub(crate) async fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// What a fake backend was asked to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sent {
    /// A text message.
    Text { to: String, text: String },
    /// A media message.
    Media { to: String, media: OutgoingMedia },
}

/// A fake WhatsApp client. Configure it through the public fields before sharing it.
#[derive(Default)]
pub(crate) struct FakeBackend {
    /// What it sent, in order.
    pub sent: Mutex<Vec<Sent>>,
    /// The LID-to-phone map: LID user to phone digits.
    pub learned: Mutex<HashMap<String, String>>,
    /// Numbers registered on WhatsApp: digits to chat JID.
    pub registered: Mutex<HashMap<String, String>>,
    /// Attachments by the `direct_path` of their payload.
    pub downloads: Mutex<HashMap<String, Vec<u8>>>,
    /// The linked account's own addresses.
    pub own: Mutex<Vec<Address>>,
    /// Makes every send and lookup fail with this.
    pub fail: Mutex<Option<BackendError>>,
    /// Set by `shutdown`.
    pub shut_down: AtomicBool,
    /// Makes `shutdown` never finish, like a client stuck closing its socket.
    pub hang_on_shutdown: AtomicBool,
    next_id: AtomicUsize,
    /// How many downloads were attempted.
    pub download_attempts: AtomicUsize,
}

impl FakeBackend {
    pub(crate) fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }

    fn next_id(&self) -> String {
        format!("3EB0{:04}", self.next_id.fetch_add(1, Ordering::SeqCst))
    }

    fn failure(&self) -> Option<BackendError> {
        self.fail.lock().unwrap().clone()
    }
}

#[async_trait]
impl Backend for FakeBackend {
    fn own_addresses(&self) -> Vec<Address> {
        self.own.lock().unwrap().clone()
    }

    async fn phone_for_lid(&self, lid: &str) -> Option<String> {
        self.learned.lock().unwrap().get(lid).cloned()
    }

    async fn resolve_phone(
        &self,
        phone: &str,
    ) -> std::result::Result<Option<String>, BackendError> {
        if let Some(e) = self.failure() {
            return Err(e);
        }
        Ok(self.registered.lock().unwrap().get(phone).cloned())
    }

    async fn send_text(&self, to: &str, text: &str) -> std::result::Result<String, BackendError> {
        if let Some(e) = self.failure() {
            return Err(e);
        }
        self.sent.lock().unwrap().push(Sent::Text {
            to: to.to_owned(),
            text: text.to_owned(),
        });
        Ok(self.next_id())
    }

    async fn send_media(
        &self,
        to: &str,
        media: OutgoingMedia,
    ) -> std::result::Result<String, BackendError> {
        if let Some(e) = self.failure() {
            return Err(e);
        }
        self.sent.lock().unwrap().push(Sent::Media {
            to: to.to_owned(),
            media,
        });
        Ok(self.next_id())
    }

    async fn download(
        &self,
        media: &MediaPayload,
        _max: u64,
    ) -> std::result::Result<Vec<u8>, BackendError> {
        self.download_attempts.fetch_add(1, Ordering::SeqCst);
        self.downloads
            .lock()
            .unwrap()
            .get(&payload_path(media))
            .cloned()
            .ok_or_else(|| BackendError::Other("the media is gone".to_owned()))
    }

    async fn shutdown(&self) {
        self.shut_down.store(true, Ordering::SeqCst);
        if self.hang_on_shutdown.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
}

fn payload_path(media: &MediaPayload) -> String {
    match media {
        MediaPayload::Image(m) => m.direct_path.clone(),
        MediaPayload::Video(m) => m.direct_path.clone(),
        MediaPayload::Audio(m) => m.direct_path.clone(),
        MediaPayload::Document(m) => m.direct_path.clone(),
        MediaPayload::Sticker(m) => m.direct_path.clone(),
    }
    .unwrap_or_default()
}

/// The test's end of a fake client: its backend, and the sender of its events.
pub(crate) struct FakeClient {
    pub backend: Arc<FakeBackend>,
    events: mpsc::UnboundedSender<WaEvent>,
}

impl FakeClient {
    /// A client and the [`Connection`] a connector would return for it.
    pub(crate) fn new() -> (Self, Connection) {
        let backend = Arc::new(FakeBackend::default());
        let (events, rx) = mpsc::unbounded_channel();
        let connection = Connection {
            backend: Arc::clone(&backend) as Arc<dyn Backend>,
            events: rx,
        };
        (Self { backend, events }, connection)
    }

    /// Reports an event, as the client would.
    pub(crate) fn emit(&self, event: WaEvent) {
        self.events.send(event).expect("the session is listening");
    }

    /// Reports a batch and returns what the client waits on: `true` once it's durable.
    pub(crate) fn deliver(&self, messages: Vec<Incoming>) -> tokio::sync::oneshot::Receiver<bool> {
        let (commit, durable) = Commit::new();
        self.emit(WaEvent::Messages(Batch { messages, commit }));
        durable
    }

    /// Ends the client's event stream, as a client that stopped for good would.
    pub(crate) fn close(self) {
        drop(self.events);
    }
}

/// A connector that hands out prepared connections in order, then fails.
#[derive(Default)]
pub(crate) struct FakeConnector {
    queue: Mutex<VecDeque<std::result::Result<Connection, String>>>,
    /// How many times `connect` was called.
    pub connects: AtomicUsize,
}

impl FakeConnector {
    pub(crate) fn push(&self, connection: std::result::Result<Connection, String>) {
        self.queue.lock().unwrap().push_back(connection);
    }

    pub(crate) fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Connector for FakeConnector {
    async fn connect(&self) -> std::result::Result<Connection, String> {
        self.connects.fetch_add(1, Ordering::SeqCst);
        self.queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("no more fake connections".to_owned()))
    }
}

/// Blobs in memory, keyed by SHA-256 like the SDK's.
#[derive(Default)]
pub(crate) struct MemoryMedia {
    blobs: Mutex<HashMap<MediaId, Vec<u8>>>,
    /// Makes `put` fail, like a full disk.
    pub fail_puts: AtomicBool,
}

impl MemoryMedia {
    pub(crate) fn get(&self, id: &MediaId) -> Option<Vec<u8>> {
        self.blobs.lock().unwrap().get(id).cloned()
    }

    /// Stores `bytes` as if the core had put them for a send, and returns their reference.
    pub(crate) fn stage(&self, bytes: &[u8], media_type: &str, name: Option<&str>) -> MediaRef {
        let id = MediaId::from_digest(Sha256::digest(bytes).into());
        self.blobs
            .lock()
            .unwrap()
            .insert(id.clone(), bytes.to_vec());
        MediaRef {
            id,
            media_type: media_type.to_owned(),
            size: bytes.len() as u64,
            name: name.map(str::to_owned),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.blobs.lock().unwrap().len()
    }
}

#[async_trait]
impl MediaStore for MemoryMedia {
    async fn put(
        &self,
        bytes: Vec<u8>,
        media_type: String,
        name: Option<String>,
    ) -> Result<MediaRef> {
        if self.fail_puts.load(Ordering::SeqCst) {
            return Err(Error::other("no space left on device"));
        }
        Ok(self.stage(&bytes, &media_type, name.as_deref()))
    }

    async fn read(&self, media: &MediaRef) -> Result<Vec<u8>> {
        self.get(&media.id)
            .ok_or_else(|| Error::NotFound(format!("blob {}", media.id)))
    }
}

// ------------------------------------------------------------------------------------------------
// Messages

/// The sender every test message comes from: a phone number with a LID.
pub(crate) const PHONE: &str = "15557654321";
/// The LID of [`PHONE`].
pub(crate) const LID: &str = "100000012345678";

/// A direct text message from [`PHONE`], addressed by phone with the LID as the alternate.
pub(crate) fn dm(id: &str, text: &str) -> Incoming {
    Incoming {
        id: id.to_owned(),
        chat: format!("{PHONE}@s.whatsapp.net"),
        kind: ChatKind::Direct,
        sender_jid: format!("{PHONE}@s.whatsapp.net"),
        sender: Some(Address::phone(PHONE)),
        sender_alt: Some(Address::lid(LID)),
        from_me: false,
        push_name: Some("Stephen".to_owned()),
        sent_at: Some("2026-10-25T12:00:00Z".parse().unwrap()),
        text: text.to_owned(),
        reply_to: None,
        media: None,
    }
}

/// An image attachment whose bytes the fake backend serves by `path`.
pub(crate) fn image(path: &str, size: Option<u64>) -> IncomingMedia {
    let mut message = wa_message::ImageMessage::default();
    message.direct_path = Some(path.to_owned());
    message.mimetype = Some("image/jpeg".to_owned());
    message.file_length = size;
    IncomingMedia {
        kind: MediaKind::Image,
        media_type: "image/jpeg".to_owned(),
        name: None,
        size,
        payload: MediaPayload::Image(Box::new(message)),
    }
}

/// A document attachment whose bytes the fake backend serves by `path`.
pub(crate) fn document(path: &str, name: &str, size: Option<u64>) -> IncomingMedia {
    let mut message = wa_message::DocumentMessage::default();
    message.direct_path = Some(path.to_owned());
    message.file_name = Some(name.to_owned());
    message.mimetype = Some("application/pdf".to_owned());
    message.file_length = size;
    IncomingMedia {
        kind: MediaKind::Document,
        media_type: "application/pdf".to_owned(),
        name: Some(name.to_owned()),
        size,
        payload: MediaPayload::Document(Box::new(message)),
    }
}
