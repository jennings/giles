//! A Discord message to the plugin's `InboundMessage`, or the reason it isn't one.
//!
//! Pure: no connection, no state. The rules are the table in `Docs/technical/discord.md`
//! ("Inbound"): bots (and so the plugin's own messages), webhooks, messages in servers, and system
//! messages are dropped; a message with nothing in it is `Empty`; the rest is a message from a
//! person in a direct conversation.

use giles_plugin_sdk::giles_types::{
    ConversationId, ConversationKind, ConversationRef, EventId, InboundMessage, MessageId,
    NativeId, Timestamp,
};

use crate::client::{Attachment, DiscordMessage};

/// The message type of a plain message.
const DEFAULT: u8 = 0;
/// The message type of a reply: the only other kind a person writes.
const REPLY: u8 = 19;
/// The longest file name or sticker name quoted in the text.
const NAME_LIMIT: usize = 100;

/// Why a message is not delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// Sent in a server. The plugin doesn't ask for server messages; this is a safety net.
    Guild,
    /// Sent by a webhook, which a DM never has.
    Webhook,
    /// Sent by a bot account: no loops between bots.
    Bot,
    /// Sent by this bot (the same account as the plugin's own).
    Own,
    /// A system message (a call, a pin, a new member), not something a person wrote.
    System,
}

impl Dropped {
    /// Whether a message dropped for this reason says anything about who is in the channel: a
    /// system message from a person does, a bot's or a webhook's or a server's doesn't.
    pub fn is_from_a_person(self) -> bool {
        self == Self::System
    }
}

/// A message to deliver: what it is, and the files that have yet to be downloaded into it.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    /// The message, with its text and no attachments yet.
    pub message: InboundMessage,
    /// The files Discord says are attached, in order. [`crate::deliver`] downloads them: a file
    /// that arrives becomes an attachment of `message`, one that doesn't becomes a note in its
    /// text.
    pub files: Vec<Attachment>,
}

/// What a message is.
#[derive(Debug, Clone, PartialEq)]
pub enum Mapped {
    /// A message to deliver.
    Message(Box<Delivery>),
    /// From a person, with no text, no attachment, and no sticker. Nothing to answer, and a sign
    /// that Discord may not be sending message content (`Docs/technical/discord.md`, "Detecting a
    /// Missing Intent").
    Empty,
    /// Not delivered.
    Dropped(Dropped),
}

/// The plugin's view of `message`. `bot` is the plugin's own user id once the Gateway has said it.
pub fn map(message: &DiscordMessage, bot: Option<u64>) -> Mapped {
    if message.author.bot {
        return Mapped::Dropped(Dropped::Bot);
    }
    if Some(message.author.id) == bot {
        return Mapped::Dropped(Dropped::Own);
    }
    if message.webhook {
        return Mapped::Dropped(Dropped::Webhook);
    }
    if message.guild {
        return Mapped::Dropped(Dropped::Guild);
    }
    if message.kind != DEFAULT && message.kind != REPLY {
        return Mapped::Dropped(Dropped::System);
    }
    if message.content.trim().is_empty()
        && message.attachments.is_empty()
        && message.stickers.is_empty()
    {
        return Mapped::Empty;
    }
    Mapped::Message(Box::new(Delivery {
        message: inbound(message),
        files: message.attachments.clone(),
    }))
}

/// The event id of a message: its snowflake, which is unique across Discord and the same after a
/// restart (PLAN section 8.5), so the SDK and the core can recognise a message they have seen.
pub fn event_id(message: u64) -> EventId {
    EventId::new(message.to_string()).expect("a number is never an empty id")
}

fn inbound(message: &DiscordMessage) -> InboundMessage {
    let id = |number: u64| number.to_string();
    InboundMessage {
        // A DM is the conversation with one person, identified by its channel.
        conversation: ConversationRef {
            id: ConversationId::new(id(message.channel_id)).expect("a number is never an empty id"),
            kind: ConversationKind::Direct,
            title: None,
        },
        // The user id, not the name: names change. Discord sets the author id itself. It is the
        // channel's native id: the core adds the identity namespace (`discord:<id>`), so the
        // namespace must not be in it.
        sender: NativeId::new(id(message.author.id)).expect("a number is never an empty id"),
        sender_aliases: Vec::new(),
        // The Gateway connection is authenticated with the bot's token, and the author id of a
        // message on it is Discord's, not the sender's claim.
        sender_authenticated: true,
        sender_display_name: message
            .author
            .global_name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| Some(message.author.username.clone()).filter(|name| !name.is_empty())),
        text: text(message),
        // The files are downloaded into it by `deliver`.
        attachments: Vec::new(),
        message_id: Some(MessageId::new(id(message.id)).expect("a number is never an empty id")),
        reply_to: message
            .reply_to
            .filter(|_| message.kind == REPLY)
            .map(|target| MessageId::new(id(target)).expect("a number is never an empty id")),
        sent_at: Timestamp::from_millisecond(message.timestamp_ms).ok(),
    }
}

/// The text as the sender wrote it, then a line for each sticker.
fn text(message: &DiscordMessage) -> String {
    let mut lines = Vec::new();
    if !message.content.is_empty() {
        lines.push(message.content.clone());
    }
    lines.extend(
        message
            .stickers
            .iter()
            .map(|sticker| note("sticker", sticker)),
    );
    lines.join("\n")
}

/// `name` as it can be quoted in a note: stripped of control characters and brackets, and cut to
/// a sensible length. It comes from the sender and ends up in the agent's prompt, so it must not
/// be able to close its note and start another line of its own. Empty when nothing is left.
pub fn quotable(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '[' | ']'))
        .take(NAME_LIMIT)
        .collect();
    name.trim().to_owned()
}

/// `[kind: name]`, with the name made [`quotable`].
fn note(kind: &str, name: &str) -> String {
    let name = quotable(name);
    if name.is_empty() {
        format!("[{kind}]")
    } else {
        format!("[{kind}: {name}]")
    }
}

#[cfg(test)]
mod tests;
