//! The seam between the channel's logic and the WhatsApp client library.
//!
//! Everything the plugin does with WhatsApp goes through [`Connector`] (start a client),
//! [`Backend`] (use a running one), and the [`WaEvent`]s it reports, all in the plugin's own terms.
//! `real` implements them on `whatsapp-rust`; the tests implement them with a fake, so the plugin's
//! logic runs without a network or a linked device.
//!
//! Chats and senders are plain strings and small values here (JID strings, [`Address`]), not the
//! library's types, except for the media payloads, which the backend alone knows how to download.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use giles_plugin_sdk::async_trait;
use giles_plugin_sdk::giles_types::Timestamp;
use tokio::sync::{mpsc, oneshot};
use whatsapp_rust::waproto::whatsapp::message as wa_message;

/// A running WhatsApp client and the events it reports, from [`Connector::connect`].
pub(crate) struct Connection {
    /// Sends, downloads, and lookups.
    pub backend: Arc<dyn Backend>,
    /// Everything the client reports, in order. Ends when the client has stopped for good.
    pub events: mpsc::UnboundedReceiver<WaEvent>,
}

/// Builds clients. The plugin calls it once at startup, and again (after a delay) if the client
/// stops without being asked to.
#[async_trait]
pub(crate) trait Connector: Send + Sync + 'static {
    /// Opens the session store and starts a client on it, linking a new device if the store has
    /// none.
    async fn connect(&self) -> Result<Connection, String>;
}

/// What a running client can do.
#[async_trait]
pub(crate) trait Backend: Send + Sync {
    /// The linked account's own addresses (its phone number and LID), empty until it's linked.
    fn own_addresses(&self) -> Vec<Address>;

    /// The phone number (digits) the client has learned for `lid` (the user part of a LID), from
    /// traffic or earlier lookups.
    async fn phone_for_lid(&self, lid: &str) -> Option<String>;

    /// The chat JID of `phone` (digits), or `None` if the number isn't on WhatsApp.
    async fn resolve_phone(&self, phone: &str) -> Result<Option<String>, BackendError>;

    /// Sends a text message to the chat `to` (a JID) and returns its message id.
    async fn send_text(&self, to: &str, text: &str) -> Result<String, BackendError>;

    /// Uploads and sends a media message to the chat `to` and returns its message id.
    async fn send_media(&self, to: &str, media: OutgoingMedia) -> Result<String, BackendError>;

    /// Downloads and decrypts an inbound media payload. `max` bounds the plaintext size.
    async fn download(&self, media: &MediaPayload, max: u64) -> Result<Vec<u8>, BackendError>;

    /// Disconnects gracefully, flushing the session store. The client doesn't come back.
    async fn shutdown(&self);
}

/// Why a [`Backend`] call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackendError {
    /// Not now: the client isn't connected, or the server or network failed. Worth retrying.
    Unavailable(String),
    /// The request itself is wrong (an address WhatsApp refuses, content it won't take).
    Rejected(String),
    /// Anything else.
    Other(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (Self::Unavailable(message) | Self::Rejected(message) | Self::Other(message)) = self;
        f.write_str(message)
    }
}

impl std::error::Error for BackendError {}

/// Something the client reports.
pub(crate) enum WaEvent {
    /// A QR code to scan, valid for `timeout`.
    PairingQr { code: String, timeout: Duration },
    /// A pair code to enter on the phone, valid for about `timeout`.
    PairingCode { code: String, timeout: Duration },
    /// The server's QR codes are used up. `closed`: the client shut itself down.
    QrExhausted { closed: bool },
    /// Linking can't finish; the text says why and what to do.
    LinkFailed(String),
    /// The phone accepted the link. The session is saved; `Connected` follows.
    Paired,
    /// Connected and logged in.
    Connected,
    /// The connection dropped. The client reconnects on its own.
    Disconnected(String),
    /// WhatsApp unlinked this device (the user removed it, or the account was logged out). The
    /// session is no good any more.
    LoggedOut(String),
    /// The client can't continue and won't reconnect (outdated version, ban, replaced session).
    Fatal(String),
    /// The client's run loop ended without being asked to.
    Stopped(String),
    /// Decrypted inbound messages, to be committed.
    Messages(Batch),
}

/// Inbound messages the client waits to acknowledge until they're committed.
pub(crate) struct Batch {
    /// The messages, in arrival order.
    pub messages: Vec<Incoming>,
    /// Report the outcome here.
    pub commit: Commit,
}

/// The client's wait for a batch to be durable. Resolve it with [`Commit::done`]; dropping it
/// counts as a failure, so a shutdown never acknowledges a message that wasn't kept.
pub(crate) struct Commit(oneshot::Sender<bool>);

impl Commit {
    /// A commit and the receiver the client waits on: `true` once the batch is durable.
    pub(crate) fn new() -> (Self, oneshot::Receiver<bool>) {
        let (tx, rx) = oneshot::channel();
        (Self(tx), rx)
    }

    /// Reports the outcome: `true` if every message of the batch is durable (or was dropped on
    /// purpose), `false` if the client should get them again.
    pub(crate) fn done(self, durable: bool) {
        // The client gave up waiting (it's shutting down): nothing to tell it.
        let _ = self.0.send(durable);
    }
}

/// How an address names an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum AddressKind {
    /// A phone number (`<digits>@s.whatsapp.net`).
    Phone,
    /// A LID, WhatsApp's account id that doesn't show the number (`<digits>@lid`).
    Lid,
}

/// A user's account address without the device: phone number or LID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Address {
    /// Phone or LID.
    pub kind: AddressKind,
    /// The digits.
    pub user: String,
}

impl Address {
    /// A phone address.
    pub(crate) fn phone(digits: impl Into<String>) -> Self {
        Self {
            kind: AddressKind::Phone,
            user: digits.into(),
        }
    }

    /// A LID address.
    pub(crate) fn lid(user: impl Into<String>) -> Self {
        Self {
            kind: AddressKind::Lid,
            user: user.into(),
        }
    }
}

/// What kind of chat a message arrived in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatKind {
    /// One person.
    Direct,
    /// A group.
    Group,
    /// Status updates, broadcast lists, channels: never conversations with the agent.
    Other,
}

/// A decrypted inbound message in the plugin's terms.
#[derive(Debug, Clone)]
pub(crate) struct Incoming {
    /// The WhatsApp message id (chosen by the sending device).
    pub id: String,
    /// The chat's JID as the client reported it, without device suffix. Stable across
    /// redeliveries, unlike anything the plugin derives from address mappings.
    pub chat: String,
    /// What the chat is.
    pub kind: ChatKind,
    /// The sender's JID as the client reported it, without device suffix.
    pub sender_jid: String,
    /// The sender's address, if it's a phone or LID one.
    pub sender: Option<Address>,
    /// The other form of the sender's address, when the server sent it.
    pub sender_alt: Option<Address>,
    /// Sent by the linked account itself (from any of its devices).
    pub from_me: bool,
    /// The sender's chosen name.
    pub push_name: Option<String>,
    /// When the sender sent it.
    pub sent_at: Option<Timestamp>,
    /// The text or caption.
    pub text: String,
    /// The id of the message this one answers.
    pub reply_to: Option<String>,
    /// The attachment, if the message carries one.
    pub media: Option<IncomingMedia>,
}

/// An attachment of an inbound message, not yet downloaded.
#[derive(Debug, Clone)]
pub(crate) struct IncomingMedia {
    /// What it is, for the note when it can't be fetched.
    pub kind: MediaKind,
    /// Its media type.
    pub media_type: String,
    /// Its file name, for documents.
    pub name: Option<String>,
    /// The size the sender declared.
    pub size: Option<u64>,
    /// What the backend needs to fetch it.
    pub payload: MediaPayload,
}

/// The kinds of media the plugin hands to the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaKind {
    /// A photo.
    Image,
    /// A video.
    Video,
    /// Audio or a voice note.
    Audio,
    /// A file.
    Document,
    /// A sticker.
    Sticker,
}

impl MediaKind {
    /// A word for notes: `image`, `video`, `audio`, `document`, `sticker`.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Sticker => "sticker",
        }
    }
}

/// The protocol message that holds an inbound attachment's keys and location.
#[derive(Debug, Clone)]
pub(crate) enum MediaPayload {
    /// An image message.
    Image(Box<wa_message::ImageMessage>),
    /// A video message.
    Video(Box<wa_message::VideoMessage>),
    /// An audio message.
    Audio(Box<wa_message::AudioMessage>),
    /// A document message.
    Document(Box<wa_message::DocumentMessage>),
    /// A sticker message.
    Sticker(Box<wa_message::StickerMessage>),
}

/// An attachment to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutgoingMedia {
    /// How WhatsApp should present it.
    pub kind: OutgoingKind,
    /// The content.
    pub bytes: Vec<u8>,
    /// Its media type.
    pub media_type: String,
    /// The file name shown for documents.
    pub name: Option<String>,
    /// The text shown with it.
    pub caption: Option<String>,
}

/// How an outbound attachment is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutgoingKind {
    /// A photo.
    Image,
    /// A video.
    Video,
    /// An audio file.
    Audio,
    /// A file.
    Document,
}
