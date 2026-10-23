//! The [`Channel`]: inbound mail from IMAP as an event stream, replies and proactive messages out
//! by SMTP.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Channel, ChannelEvent, ChannelInfo, ChannelStatus, ConversationId, ConversationKind,
    ConversationRef, Error, EventId, InboundMessage, MessageId, NativeId, OutboundMessage,
    OutputMode, Result, SendTarget, Timestamp,
};
use giles_plugin_sdk::tls::rustls::ClientConfig;
use giles_plugin_sdk::{Configure, async_trait, tls};
use tokio::time::Instant;

use crate::address;
use crate::config::{Config, RawConfig};
use crate::imap::{self, Mailbox};
use crate::inbound::{self, Mail};
use crate::media::MediaStore;
use crate::outbound::{self, AutoSubmitted, OutAttachment, Outgoing};
use crate::smtp;
use crate::state::{self, Cursor, CursorStore, Thread, Threads};

/// The identity namespace: `email:<address>`, shared by every email instance (PLAN section 3.2).
pub(crate) const NAMESPACE: &str = "email";
/// The first reconnect delay after a failed connection; it doubles up to [`MAX_RETRY_DELAY`].
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);

fn next_delay(previous: Option<Duration>) -> Duration {
    previous.map_or(FIRST_RETRY_DELAY, |delay| (delay * 2).min(MAX_RETRY_DELAY))
}

/// The email channel.
pub(crate) struct EmailChannel {
    info: ChannelInfo,
    shared: Arc<Shared>,
}

struct Shared {
    config: Config,
    tls: Arc<ClientConfig>,
    media: Arc<dyn MediaStore>,
    cursor: CursorStore,
    threads: Threads,
}

impl EmailChannel {
    /// Builds the channel from the core's `Configure`.
    pub(crate) async fn configure(ctx: Configure) -> Result<Self> {
        let config = ctx.parse_config::<RawConfig>()?.validate()?;
        tracing::info!(
            instance = ctx.instance(),
            address = %config.address,
            imap = %config.imap.server.host,
            smtp = %config.smtp.server.host,
            "email channel configured"
        );
        Self::new(
            config,
            ctx.data_dir(),
            Arc::new(ctx.blobs().clone()),
            Arc::new(tls::client_config()),
        )
        .await
    }

    pub(crate) async fn new(
        config: Config,
        data_dir: &Path,
        media: Arc<dyn MediaStore>,
        tls: Arc<ClientConfig>,
    ) -> Result<Self> {
        let cursor = CursorStore::open(data_dir).await?;
        let threads = Threads::open(data_dir).await?;
        Ok(Self {
            info: ChannelInfo {
                identity_namespace: NAMESPACE.into(),
                output: OutputMode::Blocks,
                can_initiate: true,
            },
            shared: Arc::new(Shared {
                config,
                tls,
                media,
                cursor,
                threads,
            }),
        })
    }
}

/// The `(UIDVALIDITY, UID)` in a message event id `UIDVALIDITY:UID`.
fn parse_event_id(id: &str) -> Option<(u32, u32)> {
    let (validity, uid) = id.split_once(':')?;
    Some((validity.parse().ok()?, uid.parse().ok()?))
}

#[async_trait]
impl Channel for EmailChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        let source = Source {
            shared: Arc::clone(&self.shared),
            mailbox: None,
            pending: VecDeque::new(),
            yielded: None,
            reported: None,
            failures: 0,
            caught_up: false,
            failed_caught_up: false,
            failures_since_working: 0,
            delay: None,
            retry_at: None,
            status_seq: 0,
        };
        Ok(stream::unfold(source, |mut source| async move {
            let event = source.next().await;
            Some((event, source))
        })
        .boxed())
    }

    async fn ack(&self, event: &EventId) -> Result<()> {
        // An acked message is stored by the core, and so is everything before it.
        if let Some((uid_validity, uid)) = parse_event_id(event.as_str()) {
            self.shared.cursor.advance(uid_validity, uid).await?;
        }
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        self.shared.send(msg).await
    }
}

impl Shared {
    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        let config = &self.config;
        let message_id = outbound::new_message_id(address::domain(&config.address));
        let (conversation, thread, subject, in_reply_to, auto_submitted) = match &msg.target {
            SendTarget::Conversation(id) => {
                let thread = self.threads.get(id.as_str()).await?.ok_or_else(|| {
                    Error::NotFound(format!("no email thread for conversation {id:?}"))
                })?;
                let parent = msg
                    .reply_to
                    .as_ref()
                    .and_then(|id| address::clean_id(id.as_str()))
                    .or(thread.last())
                    .map(str::to_owned);
                let subject = outbound::reply_subject(&thread.subject);
                (
                    id.as_str().to_owned(),
                    thread,
                    subject,
                    parent,
                    AutoSubmitted::Replied,
                )
            }
            SendTarget::Recipient(recipient) => {
                let peer = address::normalize(recipient.as_str()).ok_or_else(|| {
                    Error::InvalidRequest(format!("{recipient:?} is not an email address"))
                })?;
                let subject = outbound::new_subject(&msg.markdown);
                let thread = Thread {
                    peer,
                    peer_name: None,
                    subject: subject.clone(),
                    references: Vec::new(),
                };
                let conversation = inbound::conversation_id(&thread.peer, &message_id);
                (
                    conversation,
                    thread,
                    subject,
                    None,
                    AutoSubmitted::Generated,
                )
            }
            other => {
                return Err(Error::InvalidRequest(format!(
                    "unsupported send target {other:?}"
                )));
            }
        };
        let mut attachments = Vec::with_capacity(msg.attachments.len());
        for media in &msg.attachments {
            attachments.push(OutAttachment {
                media_type: media.media_type.clone(),
                name: media.name.clone(),
                bytes: self.media.read(media).await?,
            });
        }
        let raw = outbound::build(&Outgoing {
            from: &config.address,
            from_name: config.name.as_deref(),
            to: &thread.peer,
            to_name: thread.peer_name.as_deref(),
            subject: &subject,
            message_id: &message_id,
            in_reply_to: in_reply_to.as_deref(),
            references: &thread.references,
            markdown: &msg.markdown,
            attachments: &attachments,
            auto_submitted,
            date: Timestamp::now().as_second(),
        });
        smtp::send(&config.smtp, &self.tls, &config.address, &thread.peer, raw).await?;
        tracing::info!(to = %thread.peer, %conversation, %message_id, "sent an email");
        // The mail is gone; failing now would make the core send it again.
        let recorded = self
            .threads
            .update(&conversation, |stored| {
                let mut thread = stored.unwrap_or(thread);
                thread.push(message_id.clone());
                thread
            })
            .await;
        if let Err(e) = recorded {
            tracing::error!(%conversation, "recording a sent email in its thread: {e}");
        }
        MessageId::new(message_id).map_err(Error::from)
    }

    /// The event for a message, after recording its thread (if its sender is authenticated) and
    /// storing its attachments.
    async fn inbound_event(
        &self,
        id: String,
        mut mail: Mail,
        oversized: Option<u32>,
    ) -> Result<ChannelEvent> {
        let conversation = inbound::conversation_id(&mail.sender, &mail.root);
        if let Some(size) = oversized {
            let note = format!(
                "[message body not received: it is {size} bytes, larger than this channel's \
                 limit of {} bytes]",
                self.config.max_message_size
            );
            mail.text = if mail.text.is_empty() {
                note
            } else {
                format!("{}\n\n{note}", mail.text)
            };
            mail.attachments.clear();
        }
        let authenticated = mail.verdict.is_authenticated();
        if authenticated {
            // Only a verified sender may say where replies in a conversation go.
            self.threads
                .update(&conversation, |stored| {
                    let subject = match (&stored, mail.subject.is_empty()) {
                        (Some(stored), true) => stored.subject.clone(),
                        _ => mail.subject.clone(),
                    };
                    let mut references = mail.references.clone();
                    state::cap(&mut references);
                    Thread {
                        peer: mail.sender.clone(),
                        peer_name: mail.display_name.clone(),
                        subject,
                        references,
                    }
                })
                .await?;
        } else {
            tracing::info!(
                event = %id,
                sender = %mail.sender,
                "the sender isn't authenticated ({:?}); the core will drop the message",
                mail.verdict
            );
        }
        let mut attachments = Vec::with_capacity(mail.attachments.len());
        for attachment in mail.attachments {
            attachments.push(
                self.media
                    .put(attachment.bytes, attachment.media_type, attachment.name)
                    .await?,
            );
        }
        let title = (!mail.subject.is_empty()).then_some(mail.subject);
        Ok(ChannelEvent::Message {
            id: EventId::new(id)?,
            message: InboundMessage {
                conversation: ConversationRef {
                    id: ConversationId::new(conversation)?,
                    kind: ConversationKind::Direct,
                    title,
                },
                sender: NativeId::new(mail.sender)?,
                sender_aliases: Vec::new(),
                sender_authenticated: authenticated,
                sender_display_name: mail.display_name,
                text: mail.text,
                attachments,
                message_id: Some(MessageId::new(mail.message_id)?),
                reply_to: mail.in_reply_to.map(MessageId::new).transpose()?,
                sent_at: mail.sent_at,
            },
        })
    }
}

/// Why the source stopped working on a message.
enum Failure {
    /// The IMAP connection failed: reconnect.
    Connection(Error),
    /// Something local failed (the data directory, the blob store): tell the SDK, which polls
    /// again after a delay.
    Local(Error),
}

/// The event stream's state.
struct Source {
    shared: Arc<Shared>,
    mailbox: Option<Mailbox>,
    /// UIDs found but not handled yet, ascending.
    pending: VecDeque<u32>,
    /// The message event yielded last, as `(UIDVALIDITY, UID)`: persisted by the SDK once the
    /// stream is polled again.
    yielded: Option<(u32, u32)>,
    /// The status reported last.
    reported: Option<ChannelStatus>,
    /// Connection failures (connecting, or a command on the connection) since the source last
    /// made progress: handled a message, or finished a wait for new mail.
    failures: u32,
    /// The current connection has found nothing new (it's waiting for mail).
    caught_up: bool,
    /// The last failure happened on a connection that had caught up (in IDLE, say).
    failed_caught_up: bool,
    /// Failures since `Connected` was reported or the source last made progress.
    failures_since_working: u32,
    /// The delay before the next reconnect, while failures repeat.
    delay: Option<Duration>,
    retry_at: Option<Instant>,
    status_seq: u64,
}

impl Source {
    async fn next(&mut self) -> Result<ChannelEvent> {
        // The SDK persists an event before polling for the next one.
        if let Some((uid_validity, uid)) = self.yielded.take() {
            self.shared.cursor.advance(uid_validity, uid).await?;
        }
        loop {
            if self.mailbox.is_none() {
                if let Some(at) = self.retry_at {
                    tokio::time::sleep_until(at).await;
                }
                match self.connect().await {
                    // `Connected` is reported once the mailbox is caught up (step).
                    Ok(()) => {}
                    Err(Failure::Connection(e)) => {
                        if let Some(event) = self.failed(&e) {
                            return Ok(event);
                        }
                    }
                    Err(Failure::Local(e)) => return Err(e),
                }
                continue;
            }
            match self.step().await {
                Ok(Some(event)) => return Ok(event),
                Ok(None) => {}
                Err(Failure::Connection(e)) => {
                    self.mailbox = None;
                    self.pending.clear();
                    if let Some(event) = self.failed(&e) {
                        return Ok(event);
                    }
                }
                Err(Failure::Local(e)) => return Err(e),
            }
        }
    }

    /// Schedules the reconnect after a connection failure, backing off while failures repeat
    /// (a message the server always refuses to fetch must not cause a login storm), and reports
    /// the error unless it's the first failure since things worked (usually just a dropped
    /// connection).
    fn failed(&mut self, error: &Error) -> Option<ChannelEvent> {
        self.failures += 1;
        self.failures_since_working += 1;
        self.failed_caught_up = std::mem::take(&mut self.caught_up);
        let delay = next_delay(self.delay);
        self.delay = Some(delay);
        self.retry_at = Some(Instant::now() + delay);
        tracing::warn!("{error}; reconnecting in {delay:?}");
        if self.failures_since_working == 1 && self.reported == Some(ChannelStatus::Connected) {
            return None;
        }
        self.report(ChannelStatus::Error {
            message: error.to_string(),
        })
    }

    /// Notes progress: failures no longer repeat.
    fn progressed(&mut self) {
        self.failures = 0;
        self.failures_since_working = 0;
        self.delay = None;
        self.retry_at = None;
    }

    /// A status event, unless it's the status reported last.
    fn report(&mut self, status: ChannelStatus) -> Option<ChannelEvent> {
        if self.reported.as_ref() == Some(&status) {
            return None;
        }
        if status == ChannelStatus::Connected {
            self.failures_since_working = 0;
        }
        self.reported = Some(status.clone());
        self.status_seq += 1;
        let id = format!(
            "status:{}:{}",
            Timestamp::now().as_millisecond(),
            self.status_seq
        );
        Some(ChannelEvent::Status {
            id: EventId::new(id).expect("not empty"),
            status,
        })
    }

    /// Connects, and starts the cursor if the mailbox is new to it.
    async fn connect(&mut self) -> Result<(), Failure> {
        let shared = Arc::clone(&self.shared);
        let config = &shared.config.imap;
        let mut mailbox = imap::open(config, &shared.tls)
            .await
            .map_err(Failure::Connection)?;
        let current = shared.cursor.get().await;
        if current.is_none_or(|c| c.uid_validity != mailbox.uid_validity) {
            let uid = match mailbox.uid_next {
                Some(next) => next.saturating_sub(1),
                None => mailbox.last_uid().await.map_err(Failure::Connection)?,
            };
            match current {
                None => tracing::info!(
                    mailbox = %config.mailbox,
                    "first connection: mail already in the mailbox (up to UID {uid}) is ignored"
                ),
                Some(previous) => tracing::warn!(
                    mailbox = %config.mailbox,
                    "UIDVALIDITY changed from {} to {}: the mailbox was recreated, so its \
                     current mail (up to UID {uid}) is ignored",
                    previous.uid_validity,
                    mailbox.uid_validity
                ),
            }
            let cursor = Cursor {
                uid_validity: mailbox.uid_validity,
                uid,
            };
            shared.cursor.set(cursor).await.map_err(Failure::Local)?;
        }
        tracing::info!(
            host = %config.server.host,
            mailbox = %config.mailbox,
            idle = config.idle && mailbox.can_idle,
            "IMAP connected"
        );
        self.mailbox = Some(mailbox);
        self.pending.clear();
        Ok(())
    }

    /// Handles the next message, or finds or waits for new ones.
    async fn step(&mut self) -> Result<Option<ChannelEvent>, Failure> {
        let cursor = self
            .shared
            .cursor
            .get()
            .await
            .ok_or_else(|| Failure::Local(Error::other("no cursor after connecting")))?;
        let Some(mut mailbox) = self.mailbox.take() else {
            return Ok(None);
        };
        let Some(&uid) = self.pending.front() else {
            let uids = mailbox
                .uids_after(cursor.uid)
                .await
                .map_err(Failure::Connection)?;
            let caught_up = uids.is_empty();
            self.pending.extend(uids);
            self.caught_up |= caught_up;
            // Status only: the backoff resets on progress (a handled message, a finished wait),
            // never on a search, or a server that fails every IDLE (or one message's FETCH)
            // would be logged into once a second forever. Reported as working unless failures
            // persist at this same point, which would flip the status on every attempt.
            let working = self.failures == 0 || (caught_up && !self.failed_caught_up);
            if working && let Some(status) = self.report(ChannelStatus::Connected) {
                self.mailbox = Some(mailbox);
                return Ok(Some(status));
            }
            if !caught_up {
                self.mailbox = Some(mailbox);
                return Ok(None);
            }
            let imap = &self.shared.config.imap;
            let mailbox = mailbox
                .wait(imap.idle, imap.idle_timeout, imap.poll_interval)
                .await
                .map_err(Failure::Connection)?;
            self.progressed();
            self.mailbox = Some(mailbox);
            return Ok(None);
        };
        let event = self.shared.message(&mut mailbox, uid).await;
        self.mailbox = Some(mailbox);
        let event = event?;
        self.progressed();
        self.pending.pop_front();
        match event {
            Some(event) => {
                self.yielded = Some((cursor.uid_validity, uid));
                Ok(Some(event))
            }
            None => {
                self.shared
                    .cursor
                    .advance(cursor.uid_validity, uid)
                    .await
                    .map_err(Failure::Local)?;
                Ok(None)
            }
        }
    }
}

impl Shared {
    /// The event for message `uid`; `None` if it's gone or dropped.
    async fn message(
        &self,
        mailbox: &mut Mailbox,
        uid: u32,
    ) -> Result<Option<ChannelEvent>, Failure> {
        let uid_validity = mailbox.uid_validity;
        let Some(size) = mailbox.size(uid).await.map_err(Failure::Connection)? else {
            tracing::debug!(uid, "message gone before it was fetched");
            return Ok(None);
        };
        let oversized = u64::from(size) > self.config.max_message_size;
        let Some(raw) = mailbox
            .fetch(uid, oversized)
            .await
            .map_err(Failure::Connection)?
        else {
            tracing::debug!(uid, "message gone before it was fetched");
            return Ok(None);
        };
        let context = inbound::Context {
            authserv_id: &self.config.authserv_id,
            own_address: &self.config.address,
        };
        let synthetic_id = format!("{uid_validity}.{uid}@giles.invalid");
        let mail = match inbound::parse(&raw, &synthetic_id, context) {
            Ok(mail) => mail,
            Err(drop) => {
                tracing::info!(uid, "dropping a message: {drop}");
                return Ok(None);
            }
        };
        self.inbound_event(
            format!("{uid_validity}:{uid}"),
            mail,
            oversized.then_some(size),
        )
        .await
        .map(Some)
        .map_err(Failure::Local)
    }
}

#[cfg(test)]
mod tests;
