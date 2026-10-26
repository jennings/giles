//! The seam between the channel's logic and the Discord client library.
//!
//! Everything the plugin does with Discord goes through [`Discord`], in the plugin's own terms.
//! `real` implements it on `twilight`; the tests implement it with a fake, so the plugin's logic
//! runs without a network, a token, or a bot. No library type crosses it, which is also what keeps
//! a library upgrade inside `real`.
//!
//! The design (`Docs/technical/discord.md`) lists six methods. This one has the Gateway's events,
//! a clean close, and the channel history that catch-up reads. `create_message`, `create_dm`, and
//! `download` arrive with the issues that use them (outbound messages, attachments), together
//! with their real implementations, so that no method here is defined and left unimplemented.
//!
//! Discord ids are snowflakes, plain `u64`s here: they sort by creation time, and the Discord
//! API writes them as strings because JSON numbers can't carry them safely.

use std::fmt;

use giles_plugin_sdk::async_trait;

/// Something the Gateway connection reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    /// A new session is established (Identify): the first connection, a restart, or after Discord
    /// invalidated the session. Messages sent while no session existed are not replayed.
    Ready {
        /// The bot's own user id (a snowflake).
        user_id: u64,
    },
    /// The previous session resumed: Discord replayed what was missed, in order.
    Resumed,
    /// The connection ended and the client will try again, unless the close was final. `code` is
    /// the WebSocket close code, or `None` when no close frame came (a failed connect, a
    /// transport error, a heartbeat that went unanswered). Discord's request to reconnect and its
    /// invalid-session reply also end in a close.
    Closed {
        /// The close code, if the connection closed with one.
        code: Option<u16>,
    },
    /// A message was created in a direct message channel (or, if the client's intents ever change,
    /// anywhere else: [`DiscordMessage::guild`] says). Edits and deletes never reach the plugin.
    Message(DiscordMessage),
}

/// A Discord message, as much of it as the plugin uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordMessage {
    /// The message's snowflake, unique across Discord.
    pub id: u64,
    /// The channel it was sent in: the conversation, for a DM.
    pub channel_id: u64,
    /// Who sent it.
    pub author: Author,
    /// Whether it was sent in a server (it has a `guild_id`).
    pub guild: bool,
    /// Whether a webhook sent it (it has a `webhook_id`).
    pub webhook: bool,
    /// The message type: 0 is a plain message and 19 a reply; the rest are system messages.
    pub kind: u8,
    /// The text. Empty for a message with only attachments or stickers.
    pub content: String,
    /// The files attached to it.
    pub attachments: Vec<Attachment>,
    /// The names of the stickers sent with it.
    pub stickers: Vec<String>,
    /// When it was sent, in milliseconds since the Unix epoch.
    pub timestamp_ms: i64,
    /// The message this one replies to, if it is a reply.
    pub reply_to: Option<u64>,
}

/// One page of channel history.
///
/// `messages` are the ones the plugin could read, oldest first. Discord's models are strict, so a
/// message that doesn't deserialize is skipped; `received` and `last` describe the page as Discord
/// sent it, so that paging goes by what was sent and a skipped message neither makes a full page
/// look short nor stops the next request from starting after it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Page {
    /// The messages that could be read, oldest first.
    pub messages: Vec<DiscordMessage>,
    /// How many messages Discord sent, readable or not.
    pub received: usize,
    /// The highest message id Discord sent, readable or not.
    pub last: Option<u64>,
}

impl From<Vec<DiscordMessage>> for Page {
    /// A page in which every message could be read.
    fn from(messages: Vec<DiscordMessage>) -> Self {
        Self {
            received: messages.len(),
            last: messages.iter().map(|message| message.id).max(),
            messages,
        }
    }
}

/// The sender of a [`DiscordMessage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Author {
    /// The user's snowflake: immutable, unlike the names.
    pub id: u64,
    /// Whether the account is a bot.
    pub bot: bool,
    /// The account name.
    pub username: String,
    /// The display name, if the user has set one.
    pub global_name: Option<String>,
}

/// A file attached to a [`DiscordMessage`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// A signed CDN URL: valid when the message arrives, and for a while. A credential in its
    /// query string: never log it above `debug`.
    pub url: String,
    /// The file name.
    pub filename: String,
    /// The media type Discord reports, if it does.
    pub content_type: Option<String>,
    /// The declared size in bytes.
    pub size: u64,
}

/// Why a REST call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestError {
    /// Discord rejected the bot token (401). The client refuses every later request.
    Unauthorized,
    /// Discord answered with an error status. `code` is its JSON error code, when the body has
    /// one (for example 10003, an unknown channel).
    Api {
        /// The HTTP status.
        status: u16,
        /// Discord's error code.
        code: Option<u32>,
    },
    /// A 429 that the client's bounded wait didn't outlast.
    RateLimited,
    /// A file over a limit.
    TooLarge,
    /// The request didn't complete: a network error, a timeout, or an answer the client couldn't
    /// read.
    Transport,
}

impl fmt::Display for RestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("Discord rejected the bot token (401)"),
            Self::Api {
                status,
                code: Some(code),
            } => write!(f, "Discord answered {status} (error code {code})"),
            Self::Api { status, code: None } => write!(f, "Discord answered {status}"),
            Self::RateLimited => f.write_str("rate limited by Discord"),
            Self::TooLarge => f.write_str("the file is too large"),
            Self::Transport => f.write_str("the request to Discord failed"),
        }
    }
}

impl std::error::Error for RestError {}

/// A Discord Gateway connection and the REST calls the plugin makes.
#[async_trait]
pub trait Discord: Send + Sync + 'static {
    /// The next event of one long-lived connection. Connecting, heartbeating, resuming, and
    /// re-identifying are the client's business and happen while this is polled: a connection
    /// that isn't polled doesn't reconnect, which is how the caller paces reconnects.
    ///
    /// `None` after a close the client won't recover from (a rejected token, intents or sharding
    /// Discord refuses); the caller doesn't poll again. Dropping the future loses nothing.
    async fn next_event(&self) -> Option<GatewayEvent>;

    /// `GET /channels/{channel}/messages?after={after}&limit={limit}`: the messages of a channel
    /// created after the message with id `after` (any snowflake works, including one built from a
    /// time), at most `limit` (1 to 100), **oldest first**. A full page may have more after it.
    async fn messages_after(&self, channel: u64, after: u64, limit: u8) -> Result<Page, RestError>;

    /// Closes the connection cleanly (close code 1000, so the bot shows as offline and the
    /// session ends) and waits for Discord to answer. Called once, after the last
    /// [`next_event`](Self::next_event); the caller bounds how long it may take.
    async fn shutdown(&self);
}
