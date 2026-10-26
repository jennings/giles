//! Outbound messages: resolving the target, rendering CommonMark as Discord markup, splitting it at
//! Discord's limit, attaching files, and posting the messages in order.
//!
//! - A `Conversation` target is a DM channel id and is posted to as it is. A `Recipient` target is
//!   an identity's user id (the core hands over the part after `discord:`): the plugin finds the DM
//!   channel in the state file, where inbound messages and earlier sends recorded it, or opens it
//!   with `POST /users/@me/channels` and records it there, so a restart remembers it and the
//!   catch-up of [`ingest`](crate::ingest) covers channels first made for a send.
//! - The text is the SDK's Discord renderer's, in parts of at most [`DISCORD_MAX_CHARS`]. Each part
//!   is one message, sent after the one before it has been accepted. Files are uploaded with the
//!   message API's multipart form, from the blob store, at most [`FILES_PER_MESSAGE`] and
//!   [`REQUEST_LIMIT`] bytes to a message: text that fits one message is the body of the first
//!   message of files, longer text goes first, and files beyond a message's share go in follow-up
//!   messages (`plan`). A file over [`FILE_LIMIT`] or `max_blob` makes the whole send
//!   `InvalidRequest` before anything is posted.
//! - No part can ping anyone or unfurl a link: the renderer defuses mentions, and the client sets
//!   `allowed_mentions: {"parse": []}` and `SUPPRESS_EMBEDS` on every message
//!   ([`Discord::create_message`]). `reply_to` is ignored: in a DM a reply link adds clutter.
//!
//! **What a failure means.** Discord has no idempotency key for a message, and the outbox retries
//! a whole send to a user until it succeeds or its row expires, a day later. So `Outbound`
//! remembers how many parts of an unfinished send to a user Discord accepted ([`unfinished`]) and a
//! retry carries on after them: the user sees each part once, whether the send failed, or the
//! outbox gave up on it at its 60 second timeout. The error says how many parts had gone out. A
//! part that fails every time is retried alone, once per outbox retry, until the row expires. The
//! memory is bounded and in process: after a restart the retry posts the parts again once.
//!
//! A reply into a conversation is not remembered: nothing retries it (the core's reply path only
//! logs a failed send), so there is nobody to carry on for, and an entry would only sit in the map
//! for a day, ready to make an identical later reply skip parts it never posted. Its error says
//! how many parts went out and doesn't promise a retry.
//!
//! `nonce` with `enforce_nonce` was considered and rejected: `twilight-http` 0.17.1 can set
//! `nonce` but has no `enforce_nonce`, Discord only deduplicates for a few minutes against the
//! outbox's day of retries, and a nonce derived from the text would also suppress two genuine,
//! identical replies ("ok", twice).
//!
//! Every REST call is bounded by the client: `twilight-http` queues behind the rate limiter for as
//! long as Discord says and puts a request that got a 429 back through it with no limit on how
//! often, so `RealDiscord` gives up after 60 seconds (5 minutes for a message with files, which
//! takes its time to send) with [`RestError::TimedOut`], which is `Unavailable` here. The
//! outbox gives a whole send 60 seconds, so for a proactive send it is the outbox that gives up
//! first; a send dropped there is as safe as one that failed. (A proactive upload that can't
//! be sent within the outbox's 60 seconds on the owner's line is cut off, and retried, every
//! time until its row expires.)

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use giles_plugin_sdk::giles_types::{Error, MessageId, OutboundMessage, Result, SendTarget};
use giles_plugin_sdk::render::{self, DISCORD_MAX_CHARS};

use crate::client::{Discord, NewFile, NewMessage, RestError};
use crate::media::{self, MediaStore};
use crate::state::StateFile;
use crate::token::TokenState;

mod plan;
mod unfinished;

use plan::Post;
pub use plan::{FILE_LIMIT, FILES_PER_MESSAGE, REQUEST_LIMIT};
use unfinished::{Key, Unfinished};

/// The identity namespace, which the core strips before it hands over a recipient. A recipient
/// that still has it is accepted too.
const NAMESPACE_PREFIX: &str = "discord:";

/// "Cannot send messages to this user": the user doesn't take DMs from the bot, or has blocked it.
const CANNOT_MESSAGE_USER: u32 = 50_007;
/// "Cannot send messages to this user due to having no mutual guilds".
const NO_MUTUAL_GUILDS: u32 = 50_278;
/// "Unknown User".
const UNKNOWN_USER: u32 = 10_013;
/// "You are opening direct messages too fast".
const OPENING_DMS_TOO_FAST: u32 = 40_003;
/// "Unknown Channel".
const UNKNOWN_CHANNEL: u32 = 10_003;
/// "Request entity too large".
const REQUEST_TOO_LARGE: u32 = 40_005;
/// "File exceeds the maximum size".
const FILE_TOO_LARGE: u32 = 50_045;
/// "Cannot send an empty message".
const EMPTY_MESSAGE: u32 = 50_006;
/// "Invalid Form Body".
const INVALID_FORM_BODY: u32 = 50_035;

/// Sends the plugin's messages. One per channel, shared by every call of `send`.
pub struct Outbound {
    client: Arc<dyn Discord>,
    token: Arc<TokenState>,
    /// Where the DM channels are recorded (user to channel), shared with the inbound side.
    state: StateFile,
    /// Where the core put the files of a send.
    media: Arc<dyn MediaStore>,
    /// `max_blob`: no file over it is sent, whatever Discord would take.
    max_blob: u64,
    /// What the sends that haven't finished got through, so a retry carries on after it.
    unfinished: Mutex<Unfinished>,
    /// Held for the whole of a send, so the messages of two sends never interleave. A message that
    /// waits behind a rate limit would otherwise let another send's messages overtake it.
    sending: tokio::sync::Mutex<()>,
}

/// Where a send goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Destination {
    /// A channel id.
    Channel(u64),
    /// A user, whose DM channel is looked up or opened.
    User(u64),
}

/// The channel messages go to, and how it was found.
struct Resolved {
    channel: u64,
    /// The user whose cached DM channel this is, until a message to it has been accepted. Only
    /// then can Discord's "unknown channel" mean the cache is stale.
    cached_for: Option<u64>,
}

/// How a failure is told to the caller.
enum Kind {
    /// Worth retrying later.
    Unavailable,
    /// Retrying the same request won't help.
    Invalid,
    /// The channel isn't there.
    NotFound,
}

/// What went wrong, before it is an [`Error`]: the kind and a message with no user text in it.
struct Verdict {
    kind: Kind,
    message: String,
}

impl Verdict {
    fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The error for the caller, after `sent` of `total` parts went out. `carries_on` says that a
    /// retry will resume after them (a send to a user), which an error that can be retried says.
    fn into_error(self, sent: usize, total: usize, carries_on: bool) -> Error {
        let Self { kind, mut message } = self;
        if sent > 0 {
            message.push_str(&format!(
                "; {sent} of {total} messages had already been sent"
            ));
            if carries_on && matches!(kind, Kind::Unavailable) {
                message.push_str(", and a retry carries on after them");
            }
        }
        match kind {
            Kind::Unavailable => Error::unavailable(message),
            Kind::Invalid => Error::InvalidRequest(message),
            Kind::NotFound => Error::NotFound(message),
        }
    }
}

impl Outbound {
    /// A sender that calls `client`, finds DM channels in `state`, reads the files of a send from
    /// `media`, takes none over `max_blob` bytes, and stops when `token` is rejected.
    pub fn new(
        client: Arc<dyn Discord>,
        token: Arc<TokenState>,
        state: StateFile,
        media: Arc<dyn MediaStore>,
        max_blob: u64,
    ) -> Self {
        Self {
            client,
            token,
            state,
            media,
            max_blob,
            unfinished: Mutex::default(),
            sending: tokio::sync::Mutex::new(()),
        }
    }

    /// Sends `msg` and returns the id of the last message it posted.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`]: a target that isn't a Discord id, nothing to send, an
    ///   attachment over [`FILE_LIMIT`] or `max_blob`, or Discord refused the message (too
    ///   large, empty, invalid).
    /// - [`Error::NotFound`]: Discord has no such channel.
    /// - [`Error::Unavailable`]: the user can't be messaged (DMs closed, no shared server, unknown,
    ///   too many DMs opened), the token is rejected, Discord is rate limiting or failing, or it
    ///   didn't answer in time. The message says how many messages had gone out; a retry carries
    ///   on after them.
    pub async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        let OutboundMessage {
            target,
            markdown,
            attachments,
            reply_to: _,
        } = msg;
        let destination = parse_target(&target)?;
        let parts = render::discord(&markdown, DISCORD_MAX_CHARS);
        if parts.is_empty() && attachments.is_empty() {
            return Err(Error::InvalidRequest("nothing to send".to_owned()));
        }
        // Before anything is sent: a file that can't go makes the whole send invalid, so no
        // part of it is posted alone.
        plan::check(&attachments, self.max_blob)?;
        let posts = plan::plan(parts, attachments);

        let _sending = self.sending.lock().await;
        // Checked inside the lock: an earlier send may have found the token rejected while this
        // one waited.
        if self.token.is_rejected() {
            return Err(token_rejected());
        }
        // Only a send to a user is retried (by the outbox), so only it has progress to keep.
        let key =
            matches!(destination, Destination::User(_)).then(|| Key::new(destination, &posts));
        let total = posts.len();
        let done = key.as_ref().map_or(0, |key| self.unfinished().resume(key));
        if done > 0 {
            tracing::info!(done, total, "carrying on a send that was cut off");
        }
        let mut at = match self.resolve(destination).await {
            Ok(at) => at,
            Err(verdict) => return Err(self.fail(key.as_ref(), verdict, done, total)),
        };
        let mut last = 0;
        for (index, post) in posts.iter().enumerate().skip(done) {
            match self.post(&mut at, post).await {
                Ok(id) => {
                    last = id;
                    // Right away, with no await in between: a send dropped at its timeout keeps
                    // what Discord accepted.
                    if let Some(key) = key {
                        self.unfinished().record(key, index + 1, total);
                    }
                }
                Err(verdict) => return Err(self.fail(key.as_ref(), verdict, index, total)),
            }
        }
        tracing::debug!(messages = total, "sent a message to Discord");
        Ok(MessageId::new(last.to_string())?)
    }

    /// The error for a send that failed with `verdict` after `sent` of `total` parts. One a retry
    /// can't fix ends the send, which the outbox drops: nothing is remembered of it. `key` is the
    /// send's, if it is remembered.
    fn fail(&self, key: Option<&Key>, verdict: Verdict, sent: usize, total: usize) -> Error {
        let verdict = log(verdict);
        if let Some(key) = key
            && !matches!(verdict.kind, Kind::Unavailable)
        {
            self.unfinished().forget(key);
        }
        verdict.into_error(sent, total, key.is_some())
    }

    /// The channel `destination` is, opening a DM channel if need be.
    async fn resolve(&self, destination: Destination) -> std::result::Result<Resolved, Verdict> {
        match destination {
            Destination::Channel(channel) => Ok(Resolved {
                channel,
                cached_for: None,
            }),
            Destination::User(user) => match self.state.channel_for_user(user) {
                Some(channel) => Ok(Resolved {
                    channel,
                    cached_for: Some(user),
                }),
                None => self.open_dm(user).await,
            },
        }
    }

    /// Asks Discord for the DM channel with `user` and records it.
    async fn open_dm(&self, user: u64) -> std::result::Result<Resolved, Verdict> {
        let channel = self
            .client
            .create_dm(user)
            .await
            .map_err(|failure| self.verdict(failure))?;
        self.state.record_dm(channel, user);
        tracing::debug!("opened a DM channel");
        Ok(Resolved {
            channel,
            cached_for: None,
        })
    }

    /// Posts `post` to `at`, reading its files from the blob store first, and returns the new
    /// message's id. A cached channel that Discord doesn't know any more is forgotten and opened
    /// again, once.
    async fn post(&self, at: &mut Resolved, post: &Post) -> std::result::Result<u64, Verdict> {
        let mut message = self.load(post).await?;
        loop {
            // Kept only while the channel could still turn out to be a stale record.
            let spare = at.cached_for.map(|_| message.clone());
            match self.client.create_message(at.channel, message).await {
                Ok(id) => {
                    at.cached_for = None;
                    return Ok(id);
                }
                Err(failure)
                    if is_unknown_channel(&failure)
                        && let (Some(user), Some(spare)) = (at.cached_for, spare) =>
                {
                    tracing::info!("a remembered DM channel is gone; opening the DM again");
                    self.state.forget_dm(at.channel);
                    *at = self.open_dm(user).await?;
                    message = spare;
                }
                Err(failure) => return Err(self.verdict(failure)),
            }
        }
    }

    /// `post` as a message for the client, its files read from the blob store (and checked
    /// against the limit again by the bytes that came out, not only by the size the core
    /// declared).
    async fn load(&self, post: &Post) -> std::result::Result<NewMessage, Verdict> {
        let limit = plan::file_limit(self.max_blob);
        let mut files = Vec::with_capacity(post.files.len());
        for media in &post.files {
            let bytes = self.media.read(media).await.map_err(|error| {
                Verdict::new(
                    match error {
                        // The core uploaded it for this very send: it isn't coming back.
                        Error::NotFound(_) => Kind::Invalid,
                        _ => Kind::Unavailable,
                    },
                    "an attachment of the message couldn't be read from the blob store",
                )
            })?;
            if bytes.len() as u64 > limit {
                return Err(Verdict::new(
                    Kind::Invalid,
                    format!("an attachment is over the limit of {limit} bytes"),
                ));
            }
            files.push(NewFile {
                name: media::upload_name(media.name.as_deref(), &media.media_type),
                media_type: media::safe_media_type(&media.media_type),
                bytes: Bytes::from(bytes),
            });
        }
        Ok(NewMessage {
            content: post.content.clone(),
            files,
        })
    }

    /// What `failure` means for the caller. A 401 also rejects the token for good: REST stops, and
    /// the session reports `NeedsAuth`.
    fn verdict(&self, error: RestError) -> Verdict {
        let code = match &error {
            RestError::Api { code, .. } => *code,
            _ => None,
        };
        match (&error, code) {
            (RestError::Unauthorized, _) => {
                self.token.reject();
                token_rejected_verdict()
            }
            (_, Some(CANNOT_MESSAGE_USER)) => Verdict::new(
                Kind::Unavailable,
                "Discord won't let the bot message this user (error 50007): they don't take DMs \
                 from it, or have blocked it",
            ),
            (_, Some(NO_MUTUAL_GUILDS)) => Verdict::new(
                Kind::Unavailable,
                "Discord won't let the bot message this user (error 50278): they share no \
                 server with it",
            ),
            (_, Some(UNKNOWN_USER)) => Verdict::new(
                Kind::Unavailable,
                "Discord doesn't know this user (error 10013)",
            ),
            (_, Some(OPENING_DMS_TOO_FAST)) => Verdict::new(
                Kind::Unavailable,
                "Discord says the bot is opening DMs too fast (error 40003)",
            ),
            (_, Some(UNKNOWN_CHANNEL)) => {
                Verdict::new(Kind::NotFound, "Discord has no such channel (error 10003)")
            }
            (RestError::TooLarge, _)
            | (_, Some(REQUEST_TOO_LARGE | FILE_TOO_LARGE | EMPTY_MESSAGE | INVALID_FORM_BODY)) => {
                Verdict::new(Kind::Invalid, error.to_string())
            }
            (RestError::TimedOut, _) => Verdict::new(
                Kind::Unavailable,
                "Discord didn't answer in time (a slow connection, or a rate limit that doesn't clear)",
            ),
            // A server error, no answer, and any error code the plugin doesn't know: the outbox
            // retries until the message expires, which costs a request now and then.
            _ => Verdict::new(Kind::Unavailable, error.to_string()),
        }
    }

    fn unfinished(&self) -> std::sync::MutexGuard<'_, Unfinished> {
        self.unfinished
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

fn is_unknown_channel(failure: &RestError) -> bool {
    matches!(
        failure,
        RestError::Api {
            code: Some(UNKNOWN_CHANNEL),
            ..
        }
    )
}

/// Logs a failed send, without the text, and passes the verdict on.
fn log(verdict: Verdict) -> Verdict {
    tracing::warn!(error = %verdict.message, "sending to Discord failed");
    verdict
}

fn token_rejected_verdict() -> Verdict {
    Verdict::new(
        Kind::Unavailable,
        "Discord rejected the bot token: the plugin needs a new one",
    )
}

/// The error of a send that finds the token already rejected: nothing is sent.
fn token_rejected() -> Error {
    token_rejected_verdict().into_error(0, 0, false)
}

/// The destination of `target`, or why it can't be one.
fn parse_target(target: &SendTarget) -> Result<Destination> {
    match target {
        SendTarget::Conversation(id) => snowflake(id.as_str())
            .map(Destination::Channel)
            .ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "{:?} is not a Discord channel id (a number like 319674150115610528)",
                    id.as_str()
                ))
            }),
        SendTarget::Recipient(id) => {
            let native = id
                .as_str()
                .strip_prefix(NAMESPACE_PREFIX)
                .unwrap_or_else(|| id.as_str());
            snowflake(native).map(Destination::User).ok_or_else(|| {
                Error::InvalidRequest(format!(
                    "{:?} is not a Discord user (a number like 82198898841029460, \
                     or discord: and one)",
                    id.as_str()
                ))
            })
        }
        _ => Err(Error::InvalidRequest(
            "this kind of target isn't supported by the Discord plugin".to_owned(),
        )),
    }
}

/// A Discord id: the digits of a nonzero number that fits 64 bits, nothing else (no sign, no
/// spaces).
fn snowflake(text: &str) -> Option<u64> {
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|id| *id != 0)
}

#[cfg(test)]
mod tests;
