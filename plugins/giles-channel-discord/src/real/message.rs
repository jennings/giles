//! Messages from the library's model to the plugin's [`DiscordMessage`]: the Gateway's
//! `MESSAGE_CREATE` and REST history share it.

use twilight_model::channel::Message;

use crate::client::{Attachment, Author, DiscordMessage};

/// The plugin's view of `message`. Drops what the plugin doesn't use (embeds, components,
/// reactions, mentions) without looking at it.
pub(super) fn convert(message: &Message) -> DiscordMessage {
    DiscordMessage {
        id: message.id.get(),
        channel_id: message.channel_id.get(),
        author: Author {
            id: message.author.id.get(),
            bot: message.author.bot,
            username: message.author.name.clone(),
            global_name: message.author.global_name.clone(),
        },
        guild: message.guild_id.is_some(),
        webhook: message.webhook_id.is_some(),
        kind: u8::from(message.kind),
        content: message.content.clone(),
        attachments: message
            .attachments
            .iter()
            .map(|attachment| Attachment {
                url: attachment.url.clone(),
                filename: attachment.filename.clone(),
                content_type: attachment.content_type.clone(),
                size: attachment.size,
            })
            .collect(),
        stickers: message
            .sticker_items
            .iter()
            .map(|sticker| sticker.name.clone())
            .collect(),
        timestamp_ms: message.timestamp.as_micros().div_euclid(1000),
        reply_to: message
            .reference
            .as_ref()
            .and_then(|reference| reference.message_id)
            .map(twilight_model::id::Id::get),
    }
}
