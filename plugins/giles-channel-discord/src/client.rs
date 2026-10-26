//! The seam between the channel's logic and the Discord client library.
//!
//! Everything the plugin does with Discord goes through [`Discord`], in the plugin's own terms.
//! `real` implements it on `twilight`; the tests implement it with a fake, so the plugin's logic
//! runs without a network, a token, or a bot. No library type crosses it, which is also what keeps
//! a library upgrade inside `real`.
//!
//! The design (`Docs/technical/discord.md`) lists six methods. This one has them all: the Gateway's
//! events, a clean close, the channel history that catch-up reads, the two REST calls a send
//! needs (`create_message`, `create_dm`), and `download` for an attachment's CDN URL.
//!
//! Discord ids are snowflakes, plain `u64`s here: they sort by creation time, and the Discord
//! API writes them as strings because JSON numbers can't carry them safely.

use std::fmt;

use bytes::Bytes;
use futures::stream::{BoxStream, StreamExt as _};
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

/// The body of a download: chunks as they arrive, ending after an error.
pub type Body = BoxStream<'static, Result<Bytes, RestError>>;

/// `body`, cut off as soon as more than `max` bytes have come through: the chunk that crosses the
/// limit is replaced by [`RestError::TooLarge`] (its bytes are never passed on), and nothing
/// follows, so a body of any length costs at most `max` bytes plus a chunk. Enforced as bytes
/// arrive, whatever the size the sender declared.
pub fn limited(body: Body, max: u64) -> Body {
    body.scan((0_u64, false), move |(seen, over), item| {
        let next = if *over {
            None
        } else {
            Some(match item {
                Ok(chunk) => {
                    *seen = seen.saturating_add(chunk.len() as u64);
                    if *seen > max {
                        *over = true;
                        Err(RestError::TooLarge)
                    } else {
                        Ok(chunk)
                    }
                }
                Err(error) => {
                    *over = true;
                    Err(error)
                }
            })
        };
        std::future::ready(next)
    })
    .boxed()
}

/// Why a REST call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestError {
    /// Discord rejected the bot token (401). The client refuses every later request.
    Unauthorized,
    /// Discord answered with an error status. `code` is its JSON error code, when the body has
    /// one (for example 10003, an unknown channel). A CDN answers with a status and no code.
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
    /// A URL the client won't download from: not `https`, not a Discord CDN host, or with
    /// credentials in it. Nothing was requested.
    BadUrl,
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
            Self::BadUrl => f.write_str("not a URL the plugin downloads from"),
            Self::Transport => f.write_str("the request to Discord failed"),
        }
    }
}

impl std::error::Error for RestError {}

/// A message for [`Discord::create_message`].
///
/// Whatever else a message carries, the client never lets it ping anyone (`allowed_mentions`
/// with nothing to parse) and never lets Discord unfurl a link (`SUPPRESS_EMBEDS`): both are
/// part of the contract of `create_message`, not choices of the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewMessage {
    /// The text, at most 2000 characters (the renderer's parts are).
    pub content: String,
}

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

    /// `POST /channels/{channel}/messages`: posts `message` to the channel, with no mention
    /// allowed to ping and no embed (see [`NewMessage`]), and returns the new message's id.
    ///
    /// A call outlasting the client's bound (a rate limit that doesn't clear) is
    /// [`RestError::RateLimited`].
    async fn create_message(&self, channel: u64, message: NewMessage) -> Result<u64, RestError>;

    /// `POST /users/@me/channels`: the id of the DM channel with `user`. Discord returns the
    /// existing channel if there is one, so calling it again is harmless. Bounded like
    /// [`create_message`](Self::create_message).
    async fn create_dm(&self, user: u64) -> Result<u64, RestError>;

    /// Downloads an attachment from its signed CDN `url`: the status is checked and the body
    /// handed over as it arrives, never buffered whole.
    ///
    /// Only `https` URLs of Discord's CDN hosts are fetched ([`RestError::BadUrl`] for any
    /// other, with nothing requested), no redirect leaves them, no credential is sent, and every
    /// step is bounded by a timeout. A declared length over `max` is [`RestError::TooLarge`]
    /// before the body is read. The body itself is not cut off: the caller wraps it in
    /// [`limited`] with the same `max`, which ends it as soon as more than `max` bytes have
    /// arrived (dropping the body then closes the connection), so a sender can't make the
    /// plugin read or hold more. A refused or expired URL is `Api` with the CDN's status (403,
    /// 404, 410). The URL carries a credential in its query string: implementations never log
    /// it above `debug` (the real one never logs it at all).
    async fn download(&self, url: &str, max: u64) -> Result<Body, RestError>;

    /// Closes the connection cleanly (close code 1000, so the bot shows as offline and the
    /// session ends) and waits for Discord to answer. Called once, after the last
    /// [`next_event`](Self::next_event); the caller bounds how long it may take.
    async fn shutdown(&self);
}
