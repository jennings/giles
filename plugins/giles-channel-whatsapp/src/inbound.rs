//! Inbound messages: what becomes a `ChannelEvent`, and what is dropped.
//!
//! - **Echoes** are dropped: a message with `from_me` set was sent by the linked account itself,
//!   from the phone or another linked device, or by this plugin. It must never start a turn
//!   (PLAN section 3.3), and an agent that answered its own replies would loop. This includes the
//!   owner's "message yourself" chat, so Giles should have a WhatsApp number of its own.
//! - **Status updates, broadcast lists, and channels** are dropped; only direct chats and groups
//!   are conversations.
//! - **Everything else is authenticated.** WhatsApp messages are end-to-end encrypted with the
//!   Signal protocol: the client decrypts a message only with the session of the device that sent
//!   it, so the sender address on a decrypted message is the one whose keys signed it. (The first
//!   message of a new contact trusts the key the server hands out, as in the WhatsApp app itself.)
//!   Messages that skip decryption (channels) are the ones dropped above.
//! - **Groups** are reported as such, with the group's JID as the conversation. The core drops
//!   them (PLAN section 1.1); the plugin still reports them correctly.

use giles_plugin_sdk::giles_types::{
    ChannelEvent, ConversationId, ConversationKind, ConversationRef, EventId, InboundMessage,
    MediaRef, MessageId, NativeId, Result,
};

use crate::backend::{Backend, ChatKind, Incoming, IncomingMedia};
use crate::identity::SenderIds;
use crate::media::MediaStore;

/// Turns one inbound message into the event to emit, or `None` if it's dropped.
///
/// Downloads its attachment into `store`, unless it's larger than `max_blob` or can't be fetched;
/// then the message still goes on, with a note appended to its text.
///
/// # Errors
///
/// The blob store can't keep the attachment, which a retry may fix: the caller doesn't commit the
/// message, so WhatsApp delivers it again.
pub(crate) async fn event(
    message: Incoming,
    backend: &dyn Backend,
    store: &dyn MediaStore,
    max_blob: u64,
) -> Result<Option<ChannelEvent>> {
    let Incoming {
        id,
        chat,
        kind,
        sender_jid,
        sender,
        sender_alt,
        from_me,
        push_name,
        sent_at,
        mut text,
        reply_to,
        media,
    } = message;
    // `from_me` is the protocol's word for it. The sender being the linked account is the same
    // fact from another angle, checked too in case a message arrives without the flag.
    let own = backend.own_addresses();
    let from_self = sender.iter().chain(&sender_alt).any(|a| own.contains(a));
    if from_me || from_self {
        tracing::debug!(%id, "dropping an echo of a message from the linked account");
        return Ok(None);
    }
    let conversation_kind = match kind {
        ChatKind::Direct => ConversationKind::Direct,
        ChatKind::Group => ConversationKind::Group,
        ChatKind::Other => {
            tracing::debug!(%id, %chat, "dropping a message outside direct chats and groups");
            return Ok(None);
        }
    };
    let learned = match SenderIds::needs_lookup(sender.as_ref(), sender_alt.as_ref()) {
        Some(lid) => backend.phone_for_lid(&lid).await,
        None => None,
    };
    let ids = SenderIds::new(sender.as_ref(), sender_alt.as_ref(), learned.as_deref());
    let Some(canonical) = ids.canonical() else {
        tracing::warn!(%id, %sender_jid, "dropping a message from a sender with no phone or LID");
        return Ok(None);
    };
    let conversation = match conversation_kind {
        ConversationKind::Direct => ids.direct_chat().unwrap_or_else(|| chat.clone()),
        _ => chat.clone(),
    };

    let mut attachments = Vec::new();
    if let Some(media) = media {
        match fetch(media, backend, store, max_blob).await? {
            Ok(media_ref) => attachments.push(media_ref),
            Err(note) => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&note);
            }
        }
    }

    let message = InboundMessage {
        conversation: ConversationRef {
            id: ConversationId::new(conversation)?,
            kind: conversation_kind,
            title: None,
        },
        sender: NativeId::new(canonical)?,
        sender_aliases: ids
            .aliases()
            .into_iter()
            .map(NativeId::new)
            .collect::<std::result::Result<_, _>>()?,
        sender_authenticated: true,
        sender_display_name: push_name,
        text,
        attachments,
        message_id: Some(MessageId::new(id.clone())?),
        reply_to: reply_to.map(MessageId::new).transpose()?,
        sent_at,
    };
    // The key WhatsApp identifies a message by: the chat, the sender, and the id, which is only
    // unique among a sender's messages. The raw JIDs, not the derived identities, so a redelivery
    // gets the same id even after the LID-to-phone map has learned something new.
    Ok(Some(ChannelEvent::Message {
        id: EventId::new(format!("{chat}:{sender_jid}:{id}"))?,
        message,
    }))
}

/// Downloads an attachment into the blob store. The inner `Err` is the note to put in the message
/// instead: too large, or WhatsApp no longer has it.
async fn fetch(
    media: IncomingMedia,
    backend: &dyn Backend,
    store: &dyn MediaStore,
    max_blob: u64,
) -> Result<std::result::Result<MediaRef, String>> {
    let word = media.kind.word();
    if let Some(size) = media.size
        && size > max_blob
    {
        tracing::info!(size, max_blob, "skipping an attachment over the size limit");
        return Ok(Err(format!(
            "[{word} not received: it is {size} bytes, over the limit of {max_blob} bytes]"
        )));
    }
    let bytes = match backend.download(&media.payload, max_blob).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!("{e}");
            return Ok(Err(format!(
                "[{word} not received: WhatsApp would not hand it over]"
            )));
        }
    };
    if bytes.len() as u64 > max_blob {
        return Ok(Err(format!(
            "[{word} not received: it is over the limit of {max_blob} bytes]"
        )));
    }
    let media_ref = store.put(bytes, media.media_type, media.name).await?;
    Ok(Ok(media_ref))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::backend::Address;
    use crate::testing::{FakeBackend, LID, MemoryMedia, PHONE, dm, document, image};

    const MAX: u64 = 1024;

    async fn run(
        message: Incoming,
        backend: &FakeBackend,
        media: &MemoryMedia,
    ) -> Result<Option<ChannelEvent>> {
        event(message, backend, media, MAX).await
    }

    async fn message(message: Incoming) -> (String, InboundMessage) {
        let backend = FakeBackend::default();
        match run(message, &backend, &MemoryMedia::default())
            .await
            .unwrap()
        {
            Some(ChannelEvent::Message { id, message }) => (id.into_string(), message),
            other => panic!("expected a message: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_direct_message_is_authenticated_with_the_phone_and_the_lid_alias() {
        let mut incoming = dm("3EB0AAAA", "hello");
        incoming.reply_to = Some("3EB0PREV".to_owned());
        let (event_id, message) = message(incoming).await;
        assert_eq!(
            event_id,
            format!("{PHONE}@s.whatsapp.net:{PHONE}@s.whatsapp.net:3EB0AAAA")
        );
        assert_eq!(message.sender.as_str(), format!("+{PHONE}"));
        assert_eq!(
            message
                .sender_aliases
                .iter()
                .map(NativeId::as_str)
                .collect::<Vec<_>>(),
            [format!("{LID}@lid")]
        );
        assert!(message.sender_authenticated);
        assert_eq!(message.sender_display_name.as_deref(), Some("Stephen"));
        assert_eq!(message.text, "hello");
        assert_eq!(message.conversation.kind, ConversationKind::Direct);
        assert_eq!(
            message.conversation.id.as_str(),
            format!("{PHONE}@s.whatsapp.net")
        );
        assert_eq!(message.message_id.unwrap().as_str(), "3EB0AAAA");
        assert_eq!(message.reply_to.unwrap().as_str(), "3EB0PREV");
        assert_eq!(
            message.sent_at,
            Some("2026-10-25T12:00:00Z".parse().unwrap())
        );
        assert!(message.attachments.is_empty());
    }

    #[tokio::test]
    async fn a_lid_addressed_message_gets_its_phone_from_the_learned_map() {
        let mut incoming = dm("3EB0BBBB", "hi");
        incoming.chat = format!("{LID}@lid");
        incoming.sender_jid = format!("{LID}@lid");
        incoming.sender = Some(Address::lid(LID));
        incoming.sender_alt = None;
        let backend = FakeBackend::default();
        backend
            .learned
            .lock()
            .unwrap()
            .insert(LID.to_owned(), PHONE.to_owned());
        let Some(ChannelEvent::Message { id, message }) =
            run(incoming, &backend, &MemoryMedia::default())
                .await
                .unwrap()
        else {
            panic!("no message");
        };
        assert_eq!(message.sender.as_str(), format!("+{PHONE}"));
        assert_eq!(message.sender_aliases[0].as_str(), format!("{LID}@lid"));
        // One person is one conversation, however WhatsApp addressed the message; the event id
        // keeps the addresses as they came, so a redelivery has the same id.
        assert_eq!(
            message.conversation.id.as_str(),
            format!("{PHONE}@s.whatsapp.net")
        );
        assert_eq!(id.as_str(), format!("{LID}@lid:{LID}@lid:3EB0BBBB"));
    }

    #[tokio::test]
    async fn a_lid_with_no_known_phone_is_the_identity() {
        let mut incoming = dm("3EB0CCCC", "hi");
        incoming.chat = format!("{LID}@lid");
        incoming.sender_jid = format!("{LID}@lid");
        incoming.sender = Some(Address::lid(LID));
        incoming.sender_alt = None;
        let (_, message) = message(incoming).await;
        assert_eq!(message.sender.as_str(), format!("{LID}@lid"));
        assert!(message.sender_aliases.is_empty());
        assert!(message.sender_authenticated);
        assert_eq!(message.conversation.id.as_str(), format!("{LID}@lid"));
    }

    #[tokio::test]
    async fn echoes_of_the_linked_account_are_dropped() {
        let backend = FakeBackend::default();
        backend
            .own
            .lock()
            .unwrap()
            .push(Address::phone("15551112222"));
        let media = MemoryMedia::default();

        let mut from_me = dm("3EB0DDDD", "my own reply");
        from_me.from_me = true;
        assert!(run(from_me, &backend, &media).await.unwrap().is_none());

        // The sender being the linked account is an echo too, even without the flag.
        let mut own = dm("3EB0EEEE", "from my own number");
        own.sender = Some(Address::phone("15551112222"));
        assert!(run(own, &backend, &media).await.unwrap().is_none());
        let mut own_lid = dm("3EB0FFFF", "from my own lid");
        backend.own.lock().unwrap().push(Address::lid("999"));
        own_lid.sender = Some(Address::lid("999"));
        own_lid.sender_alt = None;
        assert!(run(own_lid, &backend, &media).await.unwrap().is_none());

        // Somebody else's message still comes through.
        assert!(
            run(dm("3EB0GGGG", "hi"), &backend, &media)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_group_message_is_reported_as_a_group() {
        let mut incoming = dm("3EB0HHHH", "hi all");
        incoming.chat = "120363012345678901@g.us".to_owned();
        incoming.kind = ChatKind::Group;
        incoming.sender = Some(Address::lid(LID));
        incoming.sender_alt = Some(Address::phone(PHONE));
        incoming.sender_jid = format!("{LID}@lid");
        let (event_id, message) = message(incoming).await;
        assert_eq!(message.conversation.kind, ConversationKind::Group);
        assert_eq!(message.conversation.id.as_str(), "120363012345678901@g.us");
        // The sender is the participant, not the group.
        assert_eq!(message.sender.as_str(), format!("+{PHONE}"));
        assert_eq!(
            event_id,
            format!("120363012345678901@g.us:{LID}@lid:3EB0HHHH")
        );
    }

    #[tokio::test]
    async fn status_updates_and_channels_are_dropped() {
        let backend = FakeBackend::default();
        let mut status = dm("3EB0IIII", "my status");
        status.chat = "status@broadcast".to_owned();
        status.kind = ChatKind::Other;
        assert!(
            run(status, &backend, &MemoryMedia::default())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_sender_with_no_usable_address_is_dropped() {
        let backend = FakeBackend::default();
        let mut incoming = dm("3EB0JJJJ", "hi");
        incoming.sender = None;
        incoming.sender_alt = None;
        assert!(
            run(incoming, &backend, &MemoryMedia::default())
                .await
                .unwrap()
                .is_none()
        );
    }

    fn with_media(media: IncomingMedia, text: &str) -> Incoming {
        let mut incoming = dm("3EB0KKKK", text);
        incoming.media = Some(media);
        incoming
    }

    #[tokio::test]
    async fn an_image_is_downloaded_into_a_blob_with_its_caption() {
        let backend = FakeBackend::default();
        backend
            .downloads
            .lock()
            .unwrap()
            .insert("/v/img".to_owned(), b"jpeg bytes".to_vec());
        let media = MemoryMedia::default();
        let Some(ChannelEvent::Message { message, .. }) = run(
            with_media(image("/v/img", Some(10)), "look at this"),
            &backend,
            &media,
        )
        .await
        .unwrap() else {
            panic!("no message");
        };
        assert_eq!(message.text, "look at this");
        assert_eq!(message.attachments.len(), 1);
        let attachment = &message.attachments[0];
        assert_eq!(attachment.media_type, "image/jpeg");
        assert_eq!(attachment.size, 10);
        assert_eq!(media.get(&attachment.id).unwrap(), b"jpeg bytes");
    }

    #[tokio::test]
    async fn a_document_keeps_its_file_name() {
        let backend = FakeBackend::default();
        backend
            .downloads
            .lock()
            .unwrap()
            .insert("/v/doc".to_owned(), b"%PDF".to_vec());
        let media = MemoryMedia::default();
        let Some(ChannelEvent::Message { message, .. }) = run(
            with_media(document("/v/doc", "report.pdf", Some(4)), ""),
            &backend,
            &media,
        )
        .await
        .unwrap() else {
            panic!("no message");
        };
        assert_eq!(message.text, "");
        assert_eq!(message.attachments[0].name.as_deref(), Some("report.pdf"));
        assert_eq!(message.attachments[0].media_type, "application/pdf");
    }

    #[tokio::test]
    async fn an_attachment_over_the_limit_is_left_out_with_a_note_and_never_downloaded() {
        let backend = FakeBackend::default();
        let media = MemoryMedia::default();
        let Some(ChannelEvent::Message { message, .. }) = run(
            with_media(image("/v/big", Some(MAX + 1)), "big one"),
            &backend,
            &media,
        )
        .await
        .unwrap() else {
            panic!("no message");
        };
        assert!(message.attachments.is_empty());
        assert_eq!(
            message.text,
            "big one\n\n[image not received: it is 1025 bytes, over the limit of 1024 bytes]"
        );
        assert_eq!(backend.download_attempts.load(Ordering::SeqCst), 0);
        assert_eq!(media.len(), 0);
    }

    #[tokio::test]
    async fn bytes_over_the_limit_despite_the_declared_size_are_left_out() {
        let backend = FakeBackend::default();
        backend
            .downloads
            .lock()
            .unwrap()
            .insert("/v/liar".to_owned(), vec![0; MAX as usize + 1]);
        let media = MemoryMedia::default();
        // Declares nothing, or declares less than it delivers.
        for declared in [None, Some(10)] {
            let Some(ChannelEvent::Message { message, .. }) =
                run(with_media(image("/v/liar", declared), ""), &backend, &media)
                    .await
                    .unwrap()
            else {
                panic!("no message");
            };
            assert!(message.attachments.is_empty());
            assert_eq!(
                message.text,
                "[image not received: it is over the limit of 1024 bytes]"
            );
        }
        assert_eq!(media.len(), 0);
    }

    #[tokio::test]
    async fn an_attachment_that_cannot_be_fetched_is_a_note_and_the_message_goes_on() {
        let backend = FakeBackend::default();
        let media = MemoryMedia::default();
        let Some(ChannelEvent::Message { message, .. }) = run(
            with_media(image("/v/missing", Some(5)), "photo"),
            &backend,
            &media,
        )
        .await
        .unwrap() else {
            panic!("no message");
        };
        assert!(message.attachments.is_empty());
        assert_eq!(
            message.text,
            "photo\n\n[image not received: WhatsApp would not hand it over]"
        );
    }

    #[tokio::test]
    async fn a_blob_store_failure_fails_the_message_so_it_is_delivered_again() {
        let backend = FakeBackend::default();
        backend
            .downloads
            .lock()
            .unwrap()
            .insert("/v/img".to_owned(), b"x".to_vec());
        let media = MemoryMedia::default();
        media.fail_puts.store(true, Ordering::SeqCst);
        let result = run(with_media(image("/v/img", Some(1)), "hi"), &backend, &media).await;
        assert!(result.is_err());
    }
}
