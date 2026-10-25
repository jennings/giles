//! The backend on `whatsapp-rust`: the only code that talks to the library's client.
//!
//! It is thin on purpose. The decisions (what to report, what to drop, how to split) are in the
//! rest of the crate and are tested against a fake backend; the conversions from the library's
//! types are pure functions in [`convert`] with their own tests. This file is exercised only
//! against the real service, which no test here does.

mod convert;

use std::path::PathBuf;
use std::sync::Arc;

use giles_plugin_sdk::async_trait;
use tokio::sync::mpsc;
use whatsapp_rust::download::DownloadLimits;
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::prelude::*;
use whatsapp_rust::upload::UploadOptions;
use whatsapp_rust::wacore::download::MediaType;
use whatsapp_rust::wacore::store::DevicePropsOverride;
use whatsapp_rust::wacore::types::events::{EventHandler, EventInterest};
use whatsapp_rust::wacore_binary::jid::Jid;
use whatsapp_rust::{InboundDurabilityHook, media};

use crate::backend::{
    Address, Backend, BackendError, Batch, Commit, Connection, Connector, Incoming, MediaPayload,
    OutgoingKind, OutgoingMedia, WaEvent,
};
use crate::config::{Link, Settings};

/// Builds `whatsapp-rust` clients on the session store in the data directory.
pub(crate) struct RealConnector {
    settings: Arc<Settings>,
    store: PathBuf,
}

impl RealConnector {
    /// A connector that keeps the session in the SQLite file `store`.
    pub(crate) fn new(settings: Arc<Settings>, store: PathBuf) -> Self {
        Self { settings, store }
    }
}

#[async_trait]
impl Connector for RealConnector {
    async fn connect(&self) -> Result<Connection, String> {
        let url = self
            .store
            .to_str()
            .ok_or_else(|| "the data directory path is not UTF-8".to_owned())?;
        let store = SqliteStore::open(url)
            .await
            .map_err(|e| format!("opening the session store {}: {e}", self.store.display()))?;
        let (tx, events) = mpsc::unbounded_channel();

        // Giles never imports chat history.
        let mut builder = Bot::builder()
            .with_backend(store)
            .skip_history_sync()
            .with_device_props(
                DevicePropsOverride::new().with_os(self.settings.device_name.clone()),
            )
            .with_event_handler(Forward(tx.clone()))
            .with_inbound_durability_hook(Hook(tx.clone()));
        if let Link::PairCode { phone } = &self.settings.link {
            builder = builder.with_pair_code(PairCodeOptions {
                phone_number: phone.clone(),
                ..Default::default()
            });
        }
        let bot = builder
            .build()
            .await
            .map_err(|e| format!("building the WhatsApp client: {e}"))?;
        let client = bot.client();
        tokio::spawn(async move {
            let reason = bot.run().await;
            let _ = tx.send(WaEvent::Stopped(format!("{reason:?}")));
        });
        Ok(Connection {
            backend: Arc::new(Real { client }),
            events,
        })
    }
}

/// Forwards the client's status events, inline on its event bus and so in order.
struct Forward(mpsc::UnboundedSender<WaEvent>);

impl EventHandler for Forward {
    fn handle_event(&self, event: Arc<Event>) {
        if let Some(event) = convert::event(&event) {
            // Fails only once the plugin is gone.
            let _ = self.0.send(event);
        }
    }

    fn interest(&self) -> EventInterest {
        EventInterest::of(convert::EVENT_KINDS)
    }
}

/// Holds the client's acknowledgement of inbound messages until the plugin has made them durable.
struct Hook(mpsc::UnboundedSender<WaEvent>);

#[async_trait]
impl InboundDurabilityHook for Hook {
    async fn on_messages(
        &self,
        _client: Arc<Client>,
        batch: &[InboundMessage],
    ) -> whatsapp_rust::anyhow::Result<()> {
        hand_over(&self.0, convert::batch(batch)).await
    }
}

/// Gives `messages` to the session and waits until it says they're durable. `Err` (the client
/// then withholds its acknowledgement, and WhatsApp delivers them again) if the session can't
/// keep them or is gone.
async fn hand_over(
    session: &mpsc::UnboundedSender<WaEvent>,
    messages: Vec<Incoming>,
) -> whatsapp_rust::anyhow::Result<()> {
    use whatsapp_rust::anyhow::anyhow;
    let (commit, durable) = Commit::new();
    session
        .send(WaEvent::Messages(Batch { messages, commit }))
        .map_err(|_| anyhow!("the plugin is shutting down"))?;
    match durable.await {
        Ok(true) => Ok(()),
        Ok(false) => Err(anyhow!("the plugin could not keep the messages")),
        Err(_) => Err(anyhow!("the plugin stopped before keeping the messages")),
    }
}

/// A running client.
struct Real {
    client: Arc<Client>,
}

fn jid(text: &str) -> Result<Jid, BackendError> {
    text.parse()
        .map_err(|e| BackendError::Rejected(format!("{text:?} is not a WhatsApp address: {e}")))
}

fn send_error(error: SendError) -> BackendError {
    let message = error.to_string();
    match error {
        SendError::MessageRef(_) | SendError::InvalidSecret(_) | SendError::InvalidRequest(_) => {
            BackendError::Rejected(message)
        }
        SendError::NotLoggedIn
        | SendError::Client(_)
        | SendError::Iq(_)
        | SendError::NoRecipientDevice(_)
        | SendError::PrimaryDeviceRejected(_) => BackendError::Unavailable(message),
        _ => BackendError::Other(message),
    }
}

#[async_trait]
impl Backend for Real {
    fn own_addresses(&self) -> Vec<Address> {
        let device = self.client.persistence_manager().get_device_snapshot();
        let phone = device.pn.as_ref().map(|pn| Address::phone(pn.user_base()));
        let lid = device.lid.as_ref().map(|lid| Address::lid(lid.user_base()));
        phone.into_iter().chain(lid).collect()
    }

    async fn phone_for_lid(&self, lid: &str) -> Option<String> {
        let entry = self
            .client
            .get_lid_pn_entry(&Jid::lid(lid))
            .await
            .ok()
            .flatten()?;
        Some(entry.phone_number.to_string())
    }

    async fn resolve_phone(&self, phone: &str) -> Result<Option<String>, BackendError> {
        let results = self
            .client
            .contacts()
            .is_on_whatsapp(&[Jid::pn(phone)])
            .await
            .map_err(|e| BackendError::Unavailable(format!("looking up {phone}: {e}")))?;
        Ok(results
            .into_iter()
            .find(|r| r.is_registered)
            .map(|r| r.jid.to_non_ad_string()))
    }

    async fn send_text(&self, to: &str, text: &str) -> Result<String, BackendError> {
        let sent = self
            .client
            .send_text(jid(to)?, text)
            .await
            .map_err(send_error)?;
        Ok(sent.message_id.to_string())
    }

    async fn send_media(&self, to: &str, outgoing: OutgoingMedia) -> Result<String, BackendError> {
        let to = jid(to)?;
        let OutgoingMedia {
            kind,
            bytes,
            media_type,
            name,
            caption,
        } = outgoing;
        let upload_type = match kind {
            OutgoingKind::Image => MediaType::Image,
            OutgoingKind::Video => MediaType::Video,
            OutgoingKind::Audio => MediaType::Audio,
            OutgoingKind::Document => MediaType::Document,
        };
        let upload = self
            .client
            .upload(bytes, upload_type, UploadOptions::new())
            .await
            .map_err(|e| BackendError::Unavailable(format!("uploading the attachment: {e}")))?;
        let message = match kind {
            OutgoingKind::Image => {
                let mut options = media::ImageOptions::default().with_mimetype(media_type);
                if let Some(caption) = caption {
                    options = options.with_caption(caption);
                }
                media::image_message(upload, options)
            }
            OutgoingKind::Video => {
                let mut options = media::VideoOptions::default().with_mimetype(media_type);
                if let Some(caption) = caption {
                    options = options.with_caption(caption);
                }
                media::video_message(upload, options)
            }
            OutgoingKind::Audio => media::audio_message(
                upload,
                media::AudioOptions::default().with_mimetype(media_type),
            ),
            OutgoingKind::Document => {
                let mut options = media::DocumentOptions::default().with_mimetype(media_type);
                if let Some(name) = name {
                    options = options.with_file_name(name);
                }
                if let Some(caption) = caption {
                    options = options.with_caption(caption);
                }
                media::document_message(upload, options)
            }
        };
        let sent = self
            .client
            .send_message(to, message)
            .await
            .map_err(send_error)?;
        Ok(sent.message_id.to_string())
    }

    async fn download(&self, payload: &MediaPayload, max: u64) -> Result<Vec<u8>, BackendError> {
        // The wire form is the plaintext plus padding and a MAC.
        let limits = DownloadLimits::new(max.saturating_add(1024), max);
        let downloadable: &dyn whatsapp_rust::wacore::download::Downloadable = match payload {
            MediaPayload::Image(m) => m.as_ref(),
            MediaPayload::Video(m) => m.as_ref(),
            MediaPayload::Audio(m) => m.as_ref(),
            MediaPayload::Document(m) => m.as_ref(),
            MediaPayload::Sticker(m) => m.as_ref(),
        };
        self.client
            .download(downloadable, limits)
            .await
            .map_err(|e| BackendError::Other(format!("downloading the attachment: {e}")))
    }

    async fn shutdown(&self) {
        self.client.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::dm;

    async fn batch_of(session: &mut mpsc::UnboundedReceiver<WaEvent>) -> Batch {
        match session.recv().await {
            Some(WaEvent::Messages(batch)) => batch,
            _ => panic!("no batch"),
        }
    }

    #[tokio::test]
    async fn the_hook_returns_ok_only_once_the_batch_is_durable() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hook = tokio::spawn(async move { hand_over(&tx, vec![dm("3EB0A", "hi")]).await });
        let batch = batch_of(&mut rx).await;
        assert_eq!(batch.messages.len(), 1);
        tokio::task::yield_now().await;
        assert!(!hook.is_finished(), "returned before the batch was durable");
        batch.commit.done(true);
        hook.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn the_hook_fails_when_the_batch_could_not_be_kept() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hook = tokio::spawn(async move { hand_over(&tx, vec![dm("3EB0A", "hi")]).await });
        batch_of(&mut rx).await.commit.done(false);
        let error = hook.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("could not keep"), "{error}");
    }

    #[tokio::test]
    async fn the_hook_fails_when_the_session_goes_away() {
        // Dropped while waiting: a shutdown must never acknowledge what wasn't kept.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hook = tokio::spawn(async move { hand_over(&tx, vec![dm("3EB0A", "hi")]).await });
        drop(batch_of(&mut rx).await);
        assert!(hook.await.unwrap().is_err());
        // Gone before the batch was even handed over.
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        assert!(hand_over(&tx, vec![dm("3EB0B", "hi")]).await.is_err());
    }
}
