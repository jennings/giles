//! The built-in web channel (PLAN sections 9, 10): chat pages on the core's HTTP server, posted
//! messages as channel events, and live output over server-sent events. Design:
//! `Docs/technical/web-channel.md`.
//!
//! [`WebChannel`] is the [`Channel`]: an in-memory event queue (events stay pending until acked,
//! and are sent again on every new [`Channel::events`] stream), and per-chat [hubs](hub) that
//! every open page of a chat follows. [`WebApp`] is the HTTP side: the pages, the event streams,
//! and `/blobs/<sha256>`.

mod auth;
mod config;
mod hub;
mod pages;
mod render;

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::stream::BoxStream;
use giles_types::{
    Channel, ChannelEvent, ChannelInfo, ChannelStatus, ConversationId, ConversationKind,
    ConversationRef, DeltaSink, DeltaWriter, Error, EventId, InboundMessage, MessageId,
    OutboundMessage, OutputMode, Result, SendTarget, Timestamp,
};
use tokio::time::Instant;

use crate::config::{ChannelInstance, InstanceName};
use crate::identity;

pub use auth::{CSRF_FIELD, CSRF_HEADER, CsrfRejected, WebUser, check_csrf};
pub use config::WebConfig;
pub use pages::WebApp;

/// The component name, and the identity namespace of web senders (`web:<id>` in `[owner]`).
pub const COMPONENT: &str = "web";

/// How many posted messages may wait for the core to take them before posting fails.
pub const MAX_PENDING: usize = 256;

/// The shortest time between two updates of a live message on the page; deltas in between are
/// coalesced (the final text is always sent).
pub const LIVE_UPDATE_INTERVAL: Duration = Duration::from_millis(50);

/// Locks a std mutex, ignoring poisoning: every guarded update is a single push, removal, or
/// replacement.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `bytes` random bytes from the OS, as lowercase hex.
pub(crate) fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0; bytes];
    getrandom::fill(&mut buf).expect("the OS random number generator failed");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// A new chat id: 128 random bits as 32 lowercase hex digits. Web chats are created by the user
/// (`/chats/new`), and the chat id is the conversation id (PLAN section 10).
pub fn new_chat_id() -> ConversationId {
    ConversationId::new(random_hex(16)).expect("hex is a valid id")
}

/// Whether `id` has the form of a chat id ([`new_chat_id`]). Pages refuse anything else.
pub fn is_chat_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `web-` and 128 random bits: event and message ids.
fn new_id() -> String {
    format!("web-{}", random_hex(16))
}

/// An approval prompt's short code: 1 to 32 ASCII letters and digits (what `yes <code>` accepts).
pub fn is_approval_code(code: &str) -> bool {
    (1..=32).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The answers an approval button sends (PLAN section 6.4), as the text of the reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalAnswer {
    /// `yes <code>`.
    Yes,
    /// `yes! <code>`: also for the rest of the session.
    YesForSession,
    /// `no <code>`.
    No,
}

impl ApprovalAnswer {
    /// Parses a button's value: `yes`, `yes!`, or `no`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "yes" => Some(Self::Yes),
            "yes!" => Some(Self::YesForSession),
            "no" => Some(Self::No),
            _ => None,
        }
    }

    /// The reply's first word.
    pub fn word(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::YesForSession => "yes!",
            Self::No => "no",
        }
    }

    /// What the page shows once a button sent it: only that it was sent, since the core decides
    /// what it means (the prompt may have expired). [`WebChannel::resolve_approval`] shows the
    /// outcome.
    fn sent_label(self) -> String {
        format!("Sent: {}", self.word())
    }
}

/// How an approval prompt ended ([`WebChannel::resolve_approval`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalOutcome {
    /// Allowed once.
    Allowed,
    /// Allowed for the rest of the session.
    AllowedForSession,
    /// Denied (by an answer, or by a new message while it was pending, PLAN section 6.4).
    Denied,
    /// Not answered in time.
    Expired,
}

impl ApprovalOutcome {
    /// What the page shows.
    fn label(self) -> &'static str {
        match self {
            Self::Allowed => "Allowed.",
            Self::AllowedForSession => "Allowed for the rest of this session.",
            Self::Denied => "Denied.",
            Self::Expired => "Expired: not answered in time.",
        }
    }
}

/// The core hasn't taken the posted messages waiting for it ([`MAX_PENDING`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("too many messages are waiting for Giles; try again shortly")]
pub struct Busy;

#[derive(Debug, Default)]
struct Inbound {
    /// Events not acked yet, oldest first.
    pending: VecDeque<ChannelEvent>,
    /// The current event stream, if one is open.
    stream: Option<mpsc::UnboundedSender<Result<ChannelEvent>>>,
}

/// The built-in web channel. Cheap to share as `Arc<WebChannel>`; the registry holds it both as
/// `Arc<dyn Channel>` and typed ([`Registry::web_channel`](crate::registry::Registry::web_channel)).
pub struct WebChannel {
    instance: InstanceName,
    info: ChannelInfo,
    inbound: Mutex<Inbound>,
    hubs: hub::Hubs,
}

impl fmt::Debug for WebChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebChannel")
            .field("instance", &self.instance)
            .field("pending", &lock(&self.inbound).pending.len())
            .finish_non_exhaustive()
    }
}

impl WebChannel {
    /// The channel for instance `instance`. Its first event is a `Connected` status.
    pub fn new(instance: InstanceName) -> Self {
        let status = ChannelEvent::Status {
            id: EventId::new(new_id()).expect("a valid id"),
            status: ChannelStatus::Connected,
        };
        Self {
            instance,
            info: ChannelInfo {
                identity_namespace: COMPONENT.to_owned(),
                output: OutputMode::Deltas,
                can_initiate: false,
            },
            inbound: Mutex::new(Inbound {
                pending: VecDeque::from([status]),
                stream: None,
            }),
            hubs: hub::Hubs::new(),
        }
    }

    /// The channel for a configured `[channel.<instance>]` with `component = "web"`.
    ///
    /// # Errors
    ///
    /// The `giles.toml` key under the instance table and the reason, with secret values scrubbed.
    pub fn from_instance(instance: &ChannelInstance) -> Result<Self, (&'static str, String)> {
        // Nothing to keep yet; parsing checks the table has no unknown keys.
        WebConfig::from_json(&instance.config.to_json())
            .map_err(|reason| ("config", instance.config.scrub(&reason)))?;
        let channel = Self::new(instance.name.clone());
        identity::validate_namespace(&instance.component, &channel.info.identity_namespace)
            .map_err(|e| ("component", e.to_string()))?;
        Ok(channel)
    }

    /// The instance name: the `channel` of its sessions.
    pub fn instance(&self) -> &InstanceName {
        &self.instance
    }

    /// Queues `event` for the core and hands it to the open stream, if any.
    fn enqueue(&self, event: ChannelEvent) -> Result<(), Busy> {
        let mut inbound = lock(&self.inbound);
        if inbound.pending.len() >= MAX_PENDING {
            return Err(Busy);
        }
        if let Some(stream) = &inbound.stream
            && stream.unbounded_send(Ok(event.clone())).is_err()
        {
            inbound.stream = None;
        }
        inbound.pending.push_back(event);
        Ok(())
    }

    /// Posts `text` from `user` into chat `chat`: a `ChannelEvent::Message` for the core (pending
    /// until acked), and the message on every open page of the chat.
    ///
    /// # Errors
    ///
    /// [`Busy`] if [`MAX_PENDING`] events are waiting.
    pub fn post(
        &self,
        user: &WebUser,
        chat: &ConversationId,
        text: &str,
        now: Timestamp,
    ) -> Result<EventId, Busy> {
        let id = EventId::new(new_id()).expect("a valid id");
        let message = InboundMessage {
            conversation: ConversationRef {
                id: chat.clone(),
                kind: ConversationKind::Direct,
                title: None,
            },
            sender: user.sender.clone(),
            sender_aliases: Vec::new(),
            sender_authenticated: true,
            sender_display_name: None,
            text: text.to_owned(),
            attachments: Vec::new(),
            message_id: Some(MessageId::new(id.as_str()).expect("a valid id")),
            reply_to: None,
            sent_at: Some(now),
        };
        self.enqueue(ChannelEvent::Message {
            id: id.clone(),
            message,
        })?;
        self.hubs
            .get(chat)
            .append_posted(&id, render::posted(&id, text));
        Ok(id)
    }

    /// Shows an approval prompt with Allow / Allow for this session / Deny buttons in chat
    /// `chat`: the hook for the approvals code (TASKS.md 2.7), which sends approval prompts here
    /// instead of through [`Channel::send`] on this channel. A button posts `yes <code>`,
    /// `yes! <code>`, or `no <code>` from the logged-in user, exactly as if they typed it. The
    /// prompt stays on the page (also across reloads, until a restart) until
    /// [`resolve_approval`](Self::resolve_approval) retires it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] if `code` isn't 1 to 32 ASCII letters and digits.
    pub fn send_approval(
        &self,
        chat: &ConversationId,
        code: &str,
        markdown: &str,
    ) -> Result<MessageId> {
        if !is_approval_code(code) {
            return Err(Error::InvalidRequest(format!(
                "approval code {code:?} is not 1 to 32 ASCII letters and digits"
            )));
        }
        let prompt = render::markdown(markdown);
        let html = render::approval(chat.as_str(), code, &prompt, None);
        self.hubs.get(chat).approval_add(
            hub::Approval {
                code: code.to_owned(),
                prompt,
                sent: None,
            },
            html,
        );
        Ok(MessageId::new(new_id()).expect("a valid id"))
    }

    /// Answers the approval prompt `code` in chat `chat` for `user`: posts `<answer> <code>` and
    /// replaces the prompt's buttons with what was sent ("Sent: yes"). Posted even if the prompt
    /// isn't shown any more (the core decides whether it's still pending).
    ///
    /// # Errors
    ///
    /// [`Busy`], as [`post`](Self::post).
    pub fn answer_approval(
        &self,
        user: &WebUser,
        chat: &ConversationId,
        code: &str,
        answer: ApprovalAnswer,
        now: Timestamp,
    ) -> Result<EventId, Busy> {
        let id = self.post(user, chat, &format!("{} {code}", answer.word()), now)?;
        self.hubs
            .get(chat)
            .approval_sent(code, answer.sent_label(), |prompt, label| {
                render::approval_answered(code, prompt, label)
            });
        Ok(id)
    }

    /// Retires the approval prompt `code` in chat `chat` with its `outcome`, however it ended
    /// (a button, a typed reply, a timeout, a new message): every page replaces the prompt with
    /// the outcome, and page loads no longer show it. The approvals code (TASKS.md 2.7) calls
    /// this for every prompt it sent with [`send_approval`](Self::send_approval). Returns
    /// `false` if the chat doesn't show the prompt (already resolved, or forgotten by a restart
    /// or with an idle chat's state).
    pub fn resolve_approval(
        &self,
        chat: &ConversationId,
        code: &str,
        outcome: ApprovalOutcome,
    ) -> bool {
        self.hubs.find(chat).is_some_and(|hub| {
            hub.approval_resolve(code, |prompt| {
                render::approval_answered(code, prompt, outcome.label())
            })
        })
    }

    fn hub(&self, chat: &ConversationId) -> Arc<hub::Hub> {
        self.hubs.get(chat)
    }

    /// The posted messages in chat `chat` the core hasn't taken yet (not acked), oldest first.
    fn unacked(&self, chat: &ConversationId) -> Vec<(EventId, InboundMessage)> {
        lock(&self.inbound)
            .pending
            .iter()
            .filter_map(|event| match event {
                ChannelEvent::Message { id, message } if message.conversation.id == *chat => {
                    Some((id.clone(), message.clone()))
                }
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl Channel for WebChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    /// A new stream: every pending event first, then new ones. Opening it ends the previous
    /// stream.
    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        let (tx, rx) = mpsc::unbounded();
        let mut inbound = lock(&self.inbound);
        for event in &inbound.pending {
            // Can't fail: the receiver is right here.
            let _ = tx.unbounded_send(Ok(event.clone()));
        }
        inbound.stream = Some(tx);
        Ok(rx.boxed())
    }

    async fn ack(&self, event: &EventId) -> Result<()> {
        lock(&self.inbound).pending.retain(|e| e.id() != event);
        Ok(())
    }

    /// Shows a whole message (`markdown` and attachments) on every open page of the chat.
    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        let SendTarget::Conversation(chat) = &msg.target else {
            return Err(Error::InvalidRequest(
                "the web channel can't start conversations".to_owned(),
            ));
        };
        let id = new_id();
        let media: Vec<_> = msg.attachments.iter().collect();
        let html = render::message(
            &format!("m-{id}"),
            render::Who::Assistant,
            &render::body(&msg.markdown, &media),
            render::State::Done,
        );
        self.hub(chat).append(html);
        Ok(MessageId::new(id).expect("a valid id"))
    }

    fn deltas(&self) -> Option<&dyn DeltaSink> {
        Some(self)
    }
}

impl DeltaSink for WebChannel {
    fn start(&self, conversation: &ConversationId) -> Result<Box<dyn DeltaWriter>> {
        let id = new_id();
        let element = format!("m-{id}");
        let hub = self.hub(conversation);
        hub.live_start(
            &element,
            render::message(&element, render::Who::Assistant, "", render::State::Live),
        );
        Ok(Box::new(LiveMessage {
            hub,
            id,
            element,
            text: String::new(),
            last_update: Instant::now(),
            finished: false,
        }))
    }
}

/// A live assistant message on the chat's pages ([`DeltaSink::start`]).
struct LiveMessage {
    hub: Arc<hub::Hub>,
    id: String,
    element: String,
    text: String,
    last_update: Instant,
    finished: bool,
}

impl LiveMessage {
    fn html(&self, state: render::State) -> String {
        render::message(
            &self.element,
            render::Who::Assistant,
            &render::markdown(&self.text),
            state,
        )
    }

    fn end(&mut self, state: render::State) {
        self.finished = true;
        self.hub.live_end(&self.element, &self.html(state));
    }
}

#[async_trait]
impl DeltaWriter for LiveMessage {
    fn push(&mut self, text: &str) {
        self.text.push_str(text);
        if self.last_update.elapsed() >= LIVE_UPDATE_INTERVAL {
            self.last_update = Instant::now();
            self.hub
                .live_update(&self.element, self.html(render::State::Live), &self.text);
        }
    }

    async fn finish(mut self: Box<Self>) -> Result<MessageId> {
        self.end(render::State::Done);
        Ok(MessageId::new(self.id.clone()).expect("a valid id"))
    }
}

impl Drop for LiveMessage {
    fn drop(&mut self) {
        if !self.finished {
            self.end(render::State::CutOff);
        }
    }
}
