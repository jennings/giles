//! Outbound messages: resolving the target, rendering CommonMark as WhatsApp markup, splitting,
//! and sending attachments.
//!
//! The reply's text becomes one or more messages of at most [`MAX_TEXT`] characters. Attachments
//! go first, each as its own message; the text is the caption of the first attachment that can
//! have one when it's a single short part, and separate messages otherwise. `reply_to` is not
//! used: WhatsApp quotes need the quoted message's content, which the plugin doesn't keep.

use giles_plugin_sdk::giles_types::{
    Error, MediaRef, MessageId, OutboundMessage, Result, SendTarget,
};
use giles_plugin_sdk::render;

use crate::backend::{Backend, BackendError, OutgoingKind, OutgoingMedia};
use crate::identity::{Recipient, parse_recipient};
use crate::media::MediaStore;
use crate::split;

/// The most characters of text in one WhatsApp message the plugin sends. WhatsApp takes far more,
/// but a reply that long is hard to read in one bubble.
pub(crate) const MAX_TEXT: usize = 4096;
/// The most characters of a caption: WhatsApp's own limit for one.
pub(crate) const MAX_CAPTION: usize = 1024;

/// Sends `msg` through `backend` and returns the id of the first message it sent.
///
/// Several parts are sent one after another; if one fails, the ones before it are already
/// delivered, and a retry of the whole send repeats them.
///
/// # Errors
///
/// - [`Error::InvalidRequest`]: a target that isn't a WhatsApp address, an attachment over
///   `max_blob`, or nothing to send.
/// - [`Error::Unavailable`]: the number isn't on WhatsApp, or WhatsApp can't be reached.
pub(crate) async fn send(
    msg: OutboundMessage,
    backend: &dyn Backend,
    store: &dyn MediaStore,
    max_blob: u64,
) -> Result<MessageId> {
    let OutboundMessage {
        target,
        markdown,
        attachments,
        reply_to: _,
    } = msg;
    let text = render::whatsapp(&markdown);
    let mut parts = split::split(&text, MAX_TEXT);
    if parts.is_empty() && attachments.is_empty() {
        return Err(Error::InvalidRequest("nothing to send".to_owned()));
    }
    // Everything that can be refused is checked before anything is sent.
    for media in &attachments {
        if media.size > max_blob {
            return Err(Error::InvalidRequest(format!(
                "attachment {} is {} bytes, over the limit of {max_blob} bytes",
                media.name.as_deref().unwrap_or(&media.media_type),
                media.size
            )));
        }
    }
    let to = resolve(&target, backend).await?;

    let kinds: Vec<OutgoingKind> = attachments.iter().map(outgoing_kind).collect();
    // The text rides along as a caption when it's one short part.
    let captioned = (parts.len() == 1 && parts[0].chars().count() <= MAX_CAPTION)
        .then(|| kinds.iter().position(|kind| *kind != OutgoingKind::Audio))
        .flatten();
    let mut first = None;
    for (index, (media, kind)) in attachments.iter().zip(kinds).enumerate() {
        let bytes = store.read(media).await?;
        let caption = (captioned == Some(index)).then(|| parts.remove(0));
        let id = backend
            .send_media(
                &to,
                OutgoingMedia {
                    kind,
                    bytes,
                    media_type: media.media_type.clone(),
                    name: (kind == OutgoingKind::Document).then(|| {
                        media
                            .name
                            .clone()
                            .unwrap_or_else(|| "attachment".to_owned())
                    }),
                    caption,
                },
            )
            .await
            .map_err(error)?;
        first.get_or_insert(id);
    }
    for part in parts {
        let id = backend.send_text(&to, &part).await.map_err(error)?;
        first.get_or_insert(id);
    }
    let id = first.ok_or_else(|| Error::InvalidRequest("nothing to send".to_owned()))?;
    Ok(MessageId::new(id)?)
}

/// The chat JID a send goes to.
async fn resolve(target: &SendTarget, backend: &dyn Backend) -> Result<String> {
    match target {
        SendTarget::Conversation(id) => {
            // Only the chats the plugin reports: people (by number or LID) and groups. Never
            // the status feed, a broadcast list, or a channel.
            let jid = id.as_str();
            let known = jid.split_once('@').is_some_and(|(user, server)| {
                !user.is_empty() && matches!(server, "s.whatsapp.net" | "lid" | "g.us")
            });
            if !known {
                return Err(Error::InvalidRequest(format!(
                    "{jid} is not a WhatsApp chat (a JID like 15551234567@s.whatsapp.net)"
                )));
            }
            Ok(jid.to_owned())
        }
        SendTarget::Recipient(id) => {
            match parse_recipient(id.as_str()).map_err(Error::InvalidRequest)? {
                Recipient::Lid(jid) => Ok(jid),
                Recipient::Phone(digits) => {
                    match backend.resolve_phone(&digits).await.map_err(error)? {
                        Some(jid) => Ok(jid),
                        None => Err(Error::unavailable(format!("+{digits} is not on WhatsApp"))),
                    }
                }
            }
        }
        other => Err(Error::InvalidRequest(format!(
            "unsupported send target {other:?}"
        ))),
    }
}

/// How WhatsApp should present an attachment of this media type.
fn outgoing_kind(media: &MediaRef) -> OutgoingKind {
    let essence = media
        .media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match essence.as_str() {
        "image/jpeg" | "image/png" | "image/webp" => OutgoingKind::Image,
        "video/mp4" | "video/3gpp" => OutgoingKind::Video,
        "audio/aac" | "audio/mp4" | "audio/mpeg" | "audio/amr" | "audio/ogg" => OutgoingKind::Audio,
        _ => OutgoingKind::Document,
    }
}

fn error(e: BackendError) -> Error {
    match e {
        BackendError::Unavailable(message) => Error::unavailable(message),
        BackendError::Rejected(message) => Error::InvalidRequest(message),
        BackendError::Other(message) => Error::other(message),
    }
}

#[cfg(test)]
mod tests {
    use giles_plugin_sdk::giles_types::{ConversationId, NativeId};

    use super::*;
    use crate::testing::{FakeBackend, MemoryMedia, PHONE, Sent};

    const MAX: u64 = 1024;
    const CHAT: &str = "15557654321@s.whatsapp.net";

    fn to_chat(markdown: &str, attachments: Vec<MediaRef>) -> OutboundMessage {
        OutboundMessage {
            target: SendTarget::Conversation(ConversationId::new(CHAT).unwrap()),
            markdown: markdown.to_owned(),
            attachments,
            reply_to: None,
        }
    }

    fn to_recipient(id: &str, markdown: &str) -> OutboundMessage {
        OutboundMessage {
            target: SendTarget::Recipient(NativeId::new(id).unwrap()),
            markdown: markdown.to_owned(),
            attachments: vec![],
            reply_to: None,
        }
    }

    async fn run(
        msg: OutboundMessage,
        backend: &FakeBackend,
        media: &MemoryMedia,
    ) -> Result<MessageId> {
        send(msg, backend, media, MAX).await
    }

    fn text(to: &str, text: &str) -> Sent {
        Sent::Text {
            to: to.to_owned(),
            text: text.to_owned(),
        }
    }

    #[tokio::test]
    async fn a_reply_goes_to_the_conversation_as_whatsapp_markup() {
        let backend = FakeBackend::default();
        let id = run(
            to_chat("Hello **bold** and _italic_\n\n- one\n- two", vec![]),
            &backend,
            &MemoryMedia::default(),
        )
        .await
        .unwrap();
        assert_eq!(id.as_str(), "3EB00000");
        assert_eq!(
            backend.sent(),
            [text(CHAT, "Hello *bold* and _italic_\n\n- one\n- two")]
        );
    }

    #[tokio::test]
    async fn a_phone_recipient_is_resolved_to_its_chat() {
        let backend = FakeBackend::default();
        backend
            .registered
            .lock()
            .unwrap()
            .insert(PHONE.to_owned(), CHAT.to_owned());
        run(
            to_recipient("+15557654321", "hi"),
            &backend,
            &MemoryMedia::default(),
        )
        .await
        .unwrap();
        assert_eq!(backend.sent(), [text(CHAT, "hi")]);
    }

    #[tokio::test]
    async fn a_lid_recipient_is_used_as_it_is() {
        let backend = FakeBackend::default();
        run(
            to_recipient("100000012345678@lid", "hi"),
            &backend,
            &MemoryMedia::default(),
        )
        .await
        .unwrap();
        assert_eq!(backend.sent(), [text("100000012345678@lid", "hi")]);
    }

    #[tokio::test]
    async fn a_number_that_is_not_on_whatsapp_is_unavailable() {
        let backend = FakeBackend::default();
        let error = run(
            to_recipient("+15550001111", "hi"),
            &backend,
            &MemoryMedia::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, Error::Unavailable { .. }), "{error}");
        assert!(
            error
                .to_string()
                .contains("+15550001111 is not on WhatsApp")
        );
        assert!(backend.sent().is_empty());
    }

    #[tokio::test]
    async fn identities_that_are_not_whatsapp_ones_are_invalid_requests() {
        let backend = FakeBackend::default();
        for id in ["account:stephen", "stephen", "+0123456789", "x@lid"] {
            let error = run(to_recipient(id, "hi"), &backend, &MemoryMedia::default())
                .await
                .unwrap_err();
            assert!(matches!(error, Error::InvalidRequest(_)), "{id}: {error}");
        }
        // A conversation that isn't a chat the plugin reports.
        for conversation in ["#giles", "status@broadcast", "1203630@newsletter", "@g.us"] {
            let error = run(
                OutboundMessage {
                    target: SendTarget::Conversation(ConversationId::new(conversation).unwrap()),
                    markdown: "hi".into(),
                    attachments: vec![],
                    reply_to: None,
                },
                &backend,
                &MemoryMedia::default(),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, Error::InvalidRequest(_)),
                "{conversation}: {error}"
            );
        }
        assert!(backend.sent().is_empty());
    }

    #[tokio::test]
    async fn backend_failures_keep_their_meaning() {
        let backend = FakeBackend::default();
        for (failure, retryable) in [
            (BackendError::Unavailable("offline".into()), true),
            (BackendError::Rejected("refused".into()), false),
            (BackendError::Other("odd".into()), false),
        ] {
            *backend.fail.lock().unwrap() = Some(failure);
            let error = run(to_chat("hi", vec![]), &backend, &MemoryMedia::default())
                .await
                .unwrap_err();
            assert_eq!(error.is_retryable(), retryable, "{error}");
        }
        // A rejected address is the caller's mistake.
        *backend.fail.lock().unwrap() = Some(BackendError::Rejected("refused".into()));
        let error = run(to_chat("hi", vec![]), &backend, &MemoryMedia::default())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(_)));
    }

    #[tokio::test]
    async fn a_long_reply_is_split_into_messages_that_fit() {
        let backend = FakeBackend::default();
        let paragraph = "word ".repeat(150);
        let markdown = [paragraph.trim_end(); 12].join("\n\n");
        let id = run(
            to_chat(&markdown, vec![]),
            &backend,
            &MemoryMedia::default(),
        )
        .await
        .unwrap();
        let sent = backend.sent();
        assert!(sent.len() >= 2, "{}", sent.len());
        for message in &sent {
            let Sent::Text { to, text } = message else {
                panic!("not text");
            };
            assert_eq!(to, CHAT);
            assert!(text.chars().count() <= MAX_TEXT);
        }
        // The first message's id is the one reported.
        assert_eq!(id.as_str(), "3EB00000");
    }

    #[tokio::test]
    async fn empty_text_with_no_attachments_is_an_invalid_request() {
        let backend = FakeBackend::default();
        for markdown in ["", "  \n\n "] {
            let error = run(to_chat(markdown, vec![]), &backend, &MemoryMedia::default())
                .await
                .unwrap_err();
            assert!(matches!(error, Error::InvalidRequest(_)), "{error}");
        }
    }

    #[tokio::test]
    async fn a_short_text_is_the_caption_of_the_first_attachment() {
        let backend = FakeBackend::default();
        let media = MemoryMedia::default();
        let photo = media.stage(b"jpeg", "image/jpeg", Some("cat.jpg"));
        let report = media.stage(b"%PDF", "application/pdf", Some("report.pdf"));
        run(
            to_chat("Here **you** go", vec![photo, report]),
            &backend,
            &media,
        )
        .await
        .unwrap();
        assert_eq!(
            backend.sent(),
            [
                Sent::Media {
                    to: CHAT.to_owned(),
                    media: OutgoingMedia {
                        kind: OutgoingKind::Image,
                        bytes: b"jpeg".to_vec(),
                        media_type: "image/jpeg".to_owned(),
                        // Only documents carry a file name.
                        name: None,
                        caption: Some("Here *you* go".to_owned()),
                    }
                },
                Sent::Media {
                    to: CHAT.to_owned(),
                    media: OutgoingMedia {
                        kind: OutgoingKind::Document,
                        bytes: b"%PDF".to_vec(),
                        media_type: "application/pdf".to_owned(),
                        name: Some("report.pdf".to_owned()),
                        caption: None,
                    }
                },
            ]
        );
    }

    #[tokio::test]
    async fn text_that_is_too_long_for_a_caption_follows_the_attachment() {
        let backend = FakeBackend::default();
        let media = MemoryMedia::default();
        let photo = media.stage(b"jpeg", "image/jpeg", None);
        let long = "a".repeat(MAX_CAPTION + 1);
        run(to_chat(&long, vec![photo]), &backend, &media)
            .await
            .unwrap();
        let sent = backend.sent();
        assert_eq!(sent.len(), 2);
        assert!(matches!(&sent[0], Sent::Media { media, .. } if media.caption.is_none()));
        assert_eq!(sent[1], text(CHAT, &long));
    }

    #[tokio::test]
    async fn audio_cannot_carry_a_caption_so_the_text_goes_separately() {
        let backend = FakeBackend::default();
        let media = MemoryMedia::default();
        let voice = media.stage(b"opus", "audio/ogg; codecs=opus", None);
        run(to_chat("listen", vec![voice]), &backend, &media)
            .await
            .unwrap();
        let sent = backend.sent();
        assert!(matches!(
            &sent[0],
            Sent::Media { media, .. } if media.kind == OutgoingKind::Audio && media.caption.is_none()
        ));
        assert_eq!(sent[1], text(CHAT, "listen"));
    }

    #[tokio::test]
    async fn an_attachment_alone_needs_no_text() {
        let backend = FakeBackend::default();
        let media = MemoryMedia::default();
        let clip = media.stage(b"mp4", "video/mp4", None);
        let id = run(to_chat("", vec![clip]), &backend, &media)
            .await
            .unwrap();
        assert_eq!(id.as_str(), "3EB00000");
        assert!(matches!(
            &backend.sent()[0],
            Sent::Media { media, .. } if media.kind == OutgoingKind::Video && media.caption.is_none()
        ));
    }

    #[tokio::test]
    async fn unknown_media_types_are_sent_as_documents_with_a_name() {
        let backend = FakeBackend::default();
        let media = MemoryMedia::default();
        let blob = media.stage(b"zip", "application/zip", None);
        let gif = media.stage(b"gif", "image/gif", Some("loop.gif"));
        run(to_chat("", vec![blob, gif]), &backend, &media)
            .await
            .unwrap();
        let sent = backend.sent();
        assert!(matches!(
            &sent[0],
            Sent::Media { media, .. }
                if media.kind == OutgoingKind::Document
                    && media.name.as_deref() == Some("attachment")
        ));
        assert!(matches!(
            &sent[1],
            Sent::Media { media, .. }
                if media.kind == OutgoingKind::Document
                    && media.name.as_deref() == Some("loop.gif")
        ));
    }

    #[tokio::test]
    async fn an_attachment_over_the_limit_refuses_the_whole_send() {
        let backend = FakeBackend::default();
        let media = MemoryMedia::default();
        let small = media.stage(b"ok", "image/jpeg", None);
        let big = media.stage(
            &vec![0; MAX as usize + 1],
            "application/pdf",
            Some("big.pdf"),
        );
        let error = run(to_chat("files", vec![small, big]), &backend, &media)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(_)), "{error}");
        assert!(error.to_string().contains("big.pdf"), "{error}");
        // Nothing went out, not even the small one.
        assert!(backend.sent().is_empty());
    }

    #[tokio::test]
    async fn a_missing_attachment_blob_fails_the_send() {
        let backend = FakeBackend::default();
        let media = MemoryMedia::default();
        let gone = MediaRef {
            id: giles_plugin_sdk::giles_types::MediaId::from_digest([7; 32]),
            media_type: "image/png".into(),
            size: 3,
            name: None,
        };
        let error = run(to_chat("hi", vec![gone]), &backend, &media)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::NotFound(_)), "{error}");
    }
}
