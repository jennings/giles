//! The inbound decision: what the core does with a message a channel received (PLAN sections 1.1,
//! 3.2, 3.3, 6.1). [`Router::classify`] drops it, recognizes a core command, or routes it to an
//! agent's session. Design: `Docs/technical/identity-and-routing.md`.

use std::fmt;
use std::sync::Arc;

use doozers_store::SessionKey;
use doozers_types::{
    AgentId, ConversationId, ConversationKind, ConversationRef, EventId, InboundMessage, NativeId,
    UserId,
};

use crate::channels::chat_ids;
use crate::commands::{self, CoreCommand, PendingApprovals};
use crate::config::InstanceName;
use crate::identity::{self, NamespaceError};
use crate::routes::RouteList;
use crate::sessions::AgentPolicies;
use crate::users::UserList;

/// The channel instance an event came from.
#[derive(Debug, Clone, Copy)]
pub struct ChannelSource<'a> {
    /// The instance name, e.g. `irc-libera`.
    pub instance: &'a InstanceName,
    /// The instance's component name, e.g. `irc` for `doozers-channel-irc` or `web`.
    pub component: &'a str,
    /// The identity namespace the channel reported (`ChannelInfo::identity_namespace`).
    pub identity_namespace: &'a str,
}

/// Where a message goes: the session and the user who sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    /// The session key: the routed agent, the channel instance, and the conversation.
    pub session: SessionKey,
    /// The sender, who becomes the turn's requester: the user who has the sender's identity.
    pub user: UserId,
}

/// A `/link` message: someone asks to prove that the identity they write from is a user's, by the
/// code the web page showed (PLAN section 3.2). It reaches no session and no agent.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkRequest {
    /// The identity namespace of the channel it came from.
    pub namespace: String,
    /// The sender's id in that namespace: the identity that would be linked.
    pub sender: NativeId,
    /// The conversation the answer goes to.
    pub conversation: ConversationId,
    /// The code as typed; `None` for `/link` alone.
    pub code: Option<String>,
    /// Whether the sender (or an alias) is the identity of a user already.
    pub linked: bool,
}

/// Never shows the code: nothing that logs a request may keep a code that works.
impl fmt::Debug for LinkRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkRequest")
            .field("namespace", &self.namespace)
            .field("sender", &self.sender)
            .field("conversation", &self.conversation)
            .field("code", &self.code.as_ref().map(|_| "<code>"))
            .field("linked", &self.linked)
            .finish()
    }
}

/// What to do with an inbound message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inbound {
    /// Ignore it: no turn, no reply, nothing stored. [`Router::classify`] has logged it.
    Drop(DropReason),
    /// A core command for the session `to`; it never reaches the agent.
    Command {
        /// The session the command acts on and its sender.
        to: Routed,
        /// The command.
        command: CoreCommand,
    },
    /// An ordinary message for the session `to`.
    Message(Routed),
    /// `/link`, from a sender who may have no user: the core answers it on the channel. It is
    /// never stored and never reaches an agent, and no session is made for the sender.
    Link(LinkRequest),
}

/// Why an inbound message was dropped, in the order the checks run.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropReason {
    /// The channel's identity namespace doesn't belong to its component.
    InvalidNamespace(NamespaceError),
    /// A group conversation; v1 supports only direct ones. Checked before authentication so
    /// unverified chatter in a group channel logs at `debug`, not `warn`.
    GroupConversation,
    /// The channel didn't verify the sender (`sender_authenticated` is false).
    Unauthenticated,
    /// Neither the sender nor any alias is the identity of a user.
    UnknownSender,
    /// The sender is a disabled user.
    DisabledUser(UserId),
    /// No route matches the conversation.
    NoRoute,
    /// A web chat started with an agent that isn't configured (any more).
    UnknownAgent(AgentId),
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DropReason::InvalidNamespace(err) => write!(f, "invalid channel: {err}"),
            DropReason::GroupConversation => f.write_str("group conversations are not supported"),
            DropReason::Unauthenticated => f.write_str("sender not authenticated"),
            DropReason::UnknownSender => f.write_str("sender is not the identity of any user"),
            DropReason::DisabledUser(user) => write!(f, "user `{user}` is disabled"),
            DropReason::NoRoute => f.write_str("no route matches the conversation"),
            DropReason::UnknownAgent(agent) => {
                write!(f, "the chat's agent `{agent}` isn't configured")
            }
        }
    }
}

/// The users and their identities, the ordered routes, and the agents that exist now.
#[derive(Clone)]
pub struct Router {
    users: UserList,
    routes: RouteList,
    agents: Arc<dyn AgentPolicies>,
}

impl fmt::Debug for Router {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router")
            .field("users", &self.users)
            .field("routes", &self.routes)
            .finish_non_exhaustive()
    }
}

impl Router {
    /// A router for `users` and `routes`, which it reads for every message, so a change to either
    /// list applies to the next one. `agents` says which agents exist now, for web chats started
    /// with a chosen agent (in the daemon, the running configuration, so an agent created in the
    /// web UI is reachable at once).
    pub fn new(users: UserList, routes: RouteList, agents: Arc<dyn AgentPolicies>) -> Self {
        Self {
            users,
            routes,
            agents,
        }
    }

    /// Whether `user` is a user who is disabled now. A user who isn't in the list (removed) isn't:
    /// what removing deletes is gone with them.
    pub fn is_disabled(&self, user: &UserId) -> bool {
        self.users
            .get()
            .user(user)
            .is_some_and(|known| !known.is_active())
    }

    /// The agent for a conversation on `instance`: the first route whose every set key (`channel`,
    /// `kind`, `conversation`) matches. Conversation ids compare exactly.
    pub fn route(
        &self,
        instance: &InstanceName,
        conversation: &ConversationRef,
    ) -> Option<AgentId> {
        self.routes
            .get()
            .iter()
            .find(|route| route.matches(instance, conversation))
            .map(|route| route.agent.clone())
    }

    /// Decides what to do with `message`, event `event` from `source`. Checks, in order: the
    /// channel's identity namespace, the conversation kind, sender authentication, then, for a
    /// message with no attachments that is `/link` from a channel other than the web chat, the
    /// answer [`Inbound::Link`] whether or not the sender has a user (a disabled user's is
    /// dropped), and the users' identities, that the user isn't disabled, then the agent: a web chat started with a chosen agent
    /// ([`chat_ids::chat_agent`]) goes to that agent if it exists, ahead of every route; any
    /// other conversation goes to its [route](Self::route). A routed message is then a core
    /// command if it has no attachments and parses as one ([`commands::parse`]), with `pending`
    /// deciding whether a `yes`/`no` answers an approval; otherwise an ordinary message.
    ///
    /// Drops are logged with the instance, event, and reason (and the sender identity where it
    /// explains the drop), never the message text.
    pub fn classify(
        &self,
        source: &ChannelSource<'_>,
        event: &EventId,
        message: &InboundMessage,
        pending: &dyn PendingApprovals,
    ) -> Inbound {
        let reason = match self.check(source, message) {
            Ok(Checked::Link(request)) => return Inbound::Link(request),
            Ok(Checked::Routed(to)) => {
                let command = if message.attachments.is_empty() {
                    commands::parse(&message.text, |code| pending.is_pending(&to, code))
                } else {
                    None
                };
                return match command {
                    Some(command) => Inbound::Command { to, command },
                    None => Inbound::Message(to),
                };
            }
            Err(reason) => reason,
        };
        log_drop(source, event, message, &reason);
        Inbound::Drop(reason)
    }

    fn check(
        &self,
        source: &ChannelSource<'_>,
        message: &InboundMessage,
    ) -> Result<Checked, DropReason> {
        identity::validate_namespace(source.component, source.identity_namespace)
            .map_err(DropReason::InvalidNamespace)?;
        if message.conversation.kind != ConversationKind::Direct {
            return Err(DropReason::GroupConversation);
        }
        if !message.sender_authenticated {
            return Err(DropReason::Unauthenticated);
        }
        let directory = self.users.get();
        let user = directory.resolve_sender(source.identity_namespace, message);
        // `/link` is how a sender with no user proves an identity, so it passes before the user
        // is required, and only here: after authentication, in a direct conversation. The web chat
        // is left out: its senders are logged in already, and its identities aren't linked.
        if source.component != chat_ids::COMPONENT
            && message.attachments.is_empty()
            && let Some(code) = commands::parse_link(&message.text)
        {
            if let Some(user) = user
                && !user.is_active()
            {
                return Err(DropReason::DisabledUser(user.id.clone()));
            }
            return Ok(Checked::Link(LinkRequest {
                namespace: source.identity_namespace.to_owned(),
                sender: message.sender.clone(),
                conversation: message.conversation.id.clone(),
                code: code.map(str::to_owned),
                linked: user.is_some(),
            }));
        }
        let Some(user) = user else {
            return Err(DropReason::UnknownSender);
        };
        if !user.is_active() {
            return Err(DropReason::DisabledUser(user.id.clone()));
        }
        let chosen = (source.component == chat_ids::COMPONENT)
            .then(|| chat_ids::chat_agent(message.conversation.id.as_str()))
            .flatten();
        let agent = match chosen {
            Some(agent) if self.agents.policy(&agent).is_some() => agent,
            Some(agent) => return Err(DropReason::UnknownAgent(agent)),
            None => self
                .route(source.instance, &message.conversation)
                .ok_or(DropReason::NoRoute)?,
        };
        Ok(Checked::Routed(Routed {
            session: SessionKey {
                agent,
                channel: source.instance.to_string(),
                conversation: message.conversation.id.clone(),
            },
            user: user.id.clone(),
        }))
    }
}

/// What the checks found: a message for a session, or a `/link`.
enum Checked {
    Routed(Routed),
    Link(LinkRequest),
}

fn log_drop(
    source: &ChannelSource<'_>,
    event: &EventId,
    message: &InboundMessage,
    reason: &DropReason,
) {
    let instance = source.instance.as_str();
    let namespace = source.identity_namespace;
    let sender = message.sender.as_str();
    match reason {
        DropReason::InvalidNamespace(_) => {
            tracing::warn!(instance, %event, %reason, "dropped inbound message");
        }
        DropReason::Unauthenticated => tracing::warn!(
            instance,
            %event,
            sender = format_args!("{namespace}:{sender}"),
            %reason,
            "dropped inbound message"
        ),
        DropReason::GroupConversation => {
            tracing::debug!(instance, %event, %reason, "dropped inbound message");
        }
        DropReason::UnknownSender | DropReason::DisabledUser(_) => tracing::info!(
            instance,
            %event,
            sender = format_args!("{namespace}:{sender}"),
            aliases = message.sender_aliases.len(),
            %reason,
            "dropped inbound message"
        ),
        DropReason::NoRoute | DropReason::UnknownAgent(_) => tracing::info!(
            instance,
            %event,
            conversation = message.conversation.id.as_str(),
            %reason,
            "dropped inbound message"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use doozers_types::{ConversationId, MediaId, MediaRef, NativeId};

    use super::*;
    use crate::commands::{Answer, ApprovalReply, NoPendingApprovals};
    use crate::routes::Route;
    use crate::sessions::AgentPolicy;
    use crate::users::User;
    use doozers_store::UserState;

    /// No agents: the routes alone decide.
    fn no_agents() -> Arc<dyn AgentPolicies> {
        Arc::new(HashMap::<AgentId, AgentPolicy>::new())
    }

    fn instance(name: &str) -> InstanceName {
        InstanceName::new(name).unwrap()
    }

    fn owner_config(identities: &[&str]) -> UserList {
        UserList::owner(identities, None)
    }

    fn route(
        channel: Option<&str>,
        kind: Option<ConversationKind>,
        conversation: Option<&str>,
        agent: &str,
    ) -> Route {
        Route {
            channel: channel.map(instance),
            kind,
            conversation: conversation.map(str::to_owned),
            agent: AgentId::new(agent).unwrap(),
        }
    }

    /// A router over the fixed `routes`.
    fn with_routes(users: UserList, routes: &[Route], agents: Arc<dyn AgentPolicies>) -> Router {
        Router::new(users, RouteList::new(routes.to_vec()), agents)
    }

    fn message(sender: &str, text: &str) -> InboundMessage {
        InboundMessage {
            conversation: ConversationRef {
                id: ConversationId::new(sender).unwrap(),
                kind: ConversationKind::Direct,
                title: None,
            },
            sender: NativeId::new(sender).unwrap(),
            sender_aliases: Vec::new(),
            sender_authenticated: true,
            sender_display_name: None,
            text: text.to_owned(),
            attachments: Vec::new(),
            message_id: None,
            reply_to: None,
            sent_at: None,
        }
    }

    /// The owner on WhatsApp and IRC, one route for `#giles` on IRC, and a catch-all.
    fn router() -> Router {
        with_routes(
            owner_config(&["whatsapp:+15551234567", "irc:libera.chat:account:stephen"]),
            &[
                route(Some("irc-libera"), None, Some("#giles"), "butler"),
                route(None, None, None, "jeeves"),
            ],
            no_agents(),
        )
    }

    fn classify(
        router: &Router,
        source: Source,
        message: &InboundMessage,
        pending: &dyn PendingApprovals,
    ) -> Inbound {
        let name = instance(source.0);
        let source = ChannelSource {
            instance: &name,
            component: source.1,
            identity_namespace: source.2,
        };
        router.classify(&source, &EventId::new("e1").unwrap(), message, pending)
    }

    /// A channel source: instance, component, and identity namespace.
    type Source = (&'static str, &'static str, &'static str);

    const WHATSAPP: Source = ("whatsapp", "whatsapp", "whatsapp");
    const OWNER_PHONE: &str = "+15551234567";

    fn routed(agent: &str, channel: &str, conversation: &str) -> Routed {
        Routed {
            session: SessionKey {
                agent: AgentId::new(agent).unwrap(),
                channel: channel.to_owned(),
                conversation: ConversationId::new(conversation).unwrap(),
            },
            user: identity::owner(),
        }
    }

    #[test]
    fn owner_message_is_routed() {
        let got = classify(
            &router(),
            WHATSAPP,
            &message(OWNER_PHONE, "hi"),
            &NoPendingApprovals,
        );
        assert_eq!(
            got,
            Inbound::Message(routed("jeeves", "whatsapp", OWNER_PHONE))
        );
    }

    #[test]
    fn each_message_is_routed_as_the_user_who_has_the_senders_identity() {
        let router = with_routes(
            UserList::new(vec![
                User::member("owner", &["whatsapp:+15551234567"]),
                User::member(
                    "ann",
                    &["whatsapp:+15557654321", "irc:libera.chat:account:ann"],
                ),
            ]),
            &[route(None, None, None, "jeeves")],
            no_agents(),
        );
        let ann = |conversation: &str| Routed {
            user: UserId::new("ann").unwrap(),
            ..routed("jeeves", "whatsapp", conversation)
        };
        assert_eq!(
            classify(
                &router,
                WHATSAPP,
                &message("+15557654321", "hi"),
                &NoPendingApprovals
            ),
            Inbound::Message(ann("+15557654321"))
        );
        assert_eq!(
            classify(
                &router,
                WHATSAPP,
                &message(OWNER_PHONE, "hi"),
                &NoPendingApprovals
            ),
            Inbound::Message(routed("jeeves", "whatsapp", OWNER_PHONE))
        );
        // A command is addressed to the user who sent it, too.
        assert_eq!(
            classify(
                &router,
                WHATSAPP,
                &message("+15557654321", "/new"),
                &NoPendingApprovals
            ),
            Inbound::Command {
                to: ann("+15557654321"),
                command: CoreCommand::New
            }
        );
        let irc = ("irc-libera", "irc", "irc:libera.chat");
        let m = message("account:ann", "hi");
        assert_eq!(
            classify(&router, irc, &m, &NoPendingApprovals),
            Inbound::Message(Routed {
                user: UserId::new("ann").unwrap(),
                ..routed("jeeves", "irc-libera", "account:ann")
            })
        );
    }

    #[test]
    fn a_disabled_users_messages_are_dropped_until_they_are_enabled() {
        let users = UserList::new(vec![
            User::member("owner", &["whatsapp:+15551234567"]),
            User::member("ann", &["whatsapp:+15557654321"]),
        ]);
        let router = with_routes(
            users.clone(),
            &[route(None, None, None, "jeeves")],
            no_agents(),
        );
        let ann = || message("+15557654321", "hi");
        let drop = || Inbound::Drop(DropReason::DisabledUser(UserId::new("ann").unwrap()));
        let set_ann = |state| {
            let mut all = users.get().users().to_vec();
            all[1].state = state;
            users.set(all);
        };
        set_ann(UserState::Disabled);
        // Commands and ordinary messages alike; the owner is unaffected.
        for text in ["hi", "/new", "/stop"] {
            let mut m = ann();
            m.text = text.to_owned();
            assert_eq!(classify(&router, WHATSAPP, &m, &NoPendingApprovals), drop());
        }
        assert_eq!(
            classify(
                &router,
                WHATSAPP,
                &message(OWNER_PHONE, "hi"),
                &NoPendingApprovals
            ),
            Inbound::Message(routed("jeeves", "whatsapp", OWNER_PHONE))
        );
        // The next message after a change sees the change.
        set_ann(UserState::Active);
        assert!(matches!(
            classify(&router, WHATSAPP, &ann(), &NoPendingApprovals),
            Inbound::Message(_)
        ));
    }

    #[test]
    fn drop_rules_and_their_order() {
        let base = message(OWNER_PHONE, "/new");
        let unauthenticated = |m: &mut InboundMessage| m.sender_authenticated = false;
        let group = |m: &mut InboundMessage| m.conversation.kind = ConversationKind::Group;
        let stranger = |m: &mut InboundMessage| m.sender = NativeId::new("+15550000000").unwrap();
        type Edit = fn(&mut InboundMessage);
        let bad_ns = ("whatsapp", "whatsapp", "irc:libera.chat");
        let cases: &[(Source, &[Edit], DropReason)] = &[
            (WHATSAPP, &[unauthenticated], DropReason::Unauthenticated),
            (WHATSAPP, &[group], DropReason::GroupConversation),
            (WHATSAPP, &[stranger], DropReason::UnknownSender),
            // Earlier checks win.
            (bad_ns, &[unauthenticated, group, stranger], invalid_ns()),
            (
                WHATSAPP,
                &[unauthenticated, group, stranger],
                DropReason::GroupConversation,
            ),
            (
                WHATSAPP,
                &[unauthenticated, stranger],
                DropReason::Unauthenticated,
            ),
            // A channel claiming another namespace can't impersonate the owner there.
            (bad_ns, &[], invalid_ns()),
        ];
        for (source, edits, want) in cases {
            let mut m = base.clone();
            for edit in *edits {
                edit(&mut m);
            }
            let got = classify(&router(), *source, &m, &|_: &Routed, _: Option<&str>| true);
            assert_eq!(got, Inbound::Drop(want.clone()), "{source:?} {want}");
        }
    }

    fn invalid_ns() -> DropReason {
        DropReason::InvalidNamespace(
            identity::validate_namespace("whatsapp", "irc:libera.chat").unwrap_err(),
        )
    }

    #[test]
    fn unrouted_conversations_are_dropped_even_for_commands() {
        let router = with_routes(
            owner_config(&["whatsapp:+15551234567"]),
            &[route(Some("irc-libera"), None, None, "jeeves")],
            no_agents(),
        );
        for text in ["hi", "/new", "/stop", "yes"] {
            let got = classify(
                &router,
                WHATSAPP,
                &message(OWNER_PHONE, text),
                &|_: &Routed, _: Option<&str>| true,
            );
            assert_eq!(got, Inbound::Drop(DropReason::NoRoute), "{text}");
        }
    }

    #[test]
    fn aliases_identify_the_owner() {
        let router = with_routes(
            owner_config(&["whatsapp:123456789@lid"]),
            &[route(None, None, None, "jeeves")],
            no_agents(),
        );
        let mut m = message(OWNER_PHONE, "hi");
        assert_eq!(
            classify(&router, WHATSAPP, &m, &NoPendingApprovals),
            Inbound::Drop(DropReason::UnknownSender)
        );
        m.sender_aliases = vec![
            NativeId::new("other@lid").unwrap(),
            NativeId::new("123456789@lid").unwrap(),
        ];
        assert_eq!(
            classify(&router, WHATSAPP, &m, &NoPendingApprovals),
            Inbound::Message(routed("jeeves", "whatsapp", OWNER_PHONE))
        );
    }

    #[test]
    fn namespaces_with_colons() {
        let irc = ("irc-libera", "irc", "irc:libera.chat");
        let m = message("account:stephen", "hi");
        assert_eq!(
            classify(&router(), irc, &m, &NoPendingApprovals),
            Inbound::Message(routed("jeeves", "irc-libera", "account:stephen"))
        );
        // The same account on another network is someone else.
        let oftc = ("irc-oftc", "irc", "irc:oftc");
        assert_eq!(
            classify(&router(), oftc, &m, &NoPendingApprovals),
            Inbound::Drop(DropReason::UnknownSender)
        );
    }

    #[test]
    fn route_precedence() {
        let router = with_routes(
            owner_config(&["web:owner"]),
            &[
                route(Some("web"), None, Some("chat-1"), "first"),
                route(Some("web"), Some(ConversationKind::Group), None, "groups"),
                route(
                    None,
                    Some(ConversationKind::Direct),
                    Some("chat-2"),
                    "second",
                ),
                route(
                    Some("web"),
                    Some(ConversationKind::Direct),
                    None,
                    "web-direct",
                ),
                route(None, None, None, "fallback"),
                route(Some("web"), None, None, "never"),
            ],
            no_agents(),
        );
        let conv = |id: &str, kind| ConversationRef {
            id: ConversationId::new(id).unwrap(),
            kind,
            title: None,
        };
        let (web, other) = (instance("web"), instance("web-2"));
        let direct = ConversationKind::Direct;
        let group = ConversationKind::Group;
        let cases = [
            (&web, conv("chat-1", direct), "first"),
            (&web, conv("chat-1", group), "first"),
            (&web, conv("chat-2", group), "groups"),
            (&web, conv("chat-2", direct), "second"),
            (&other, conv("chat-2", direct), "second"),
            (&other, conv("chat-2", group), "fallback"),
            (&web, conv("chat-3", direct), "web-direct"),
            (&other, conv("chat-1", direct), "fallback"),
            // Conversation ids compare exactly.
            (&web, conv("Chat-1", direct), "web-direct"),
        ];
        for (inst, conversation, want) in cases {
            assert_eq!(
                router
                    .route(inst, &conversation)
                    .as_ref()
                    .map(AgentId::as_str),
                Some(want),
                "{inst} {conversation:?}"
            );
        }
        let none = with_routes(owner_config(&["web:owner"]), &[], no_agents());
        assert_eq!(none.route(&web, &conv("chat-1", direct)), None);
    }

    #[test]
    fn a_web_chat_started_with_an_agent_goes_to_it_ahead_of_routes() {
        let agents = HashMap::from([(AgentId::new("bunter").unwrap(), AgentPolicy::default())]);
        let router = with_routes(
            owner_config(&["web:owner", "whatsapp:+15551234567"]),
            &[route(None, None, None, "jeeves")],
            Arc::new(agents),
        );
        let web: Source = ("chat", chat_ids::COMPONENT, "web");
        let hex = "0123456789abcdef0123456789abcdef";
        let in_chat = |source: Source, chat: &str, text: &str| {
            let mut m = message("owner", text);
            if source.1 == "whatsapp" {
                m.sender = NativeId::new(OWNER_PHONE).unwrap();
            }
            m.conversation.id = ConversationId::new(chat).unwrap();
            classify(&router, source, &m, &NoPendingApprovals)
        };
        let bunter = format!("bunter.{hex}");
        assert_eq!(
            in_chat(web, &bunter, "hi"),
            Inbound::Message(routed("bunter", "chat", &bunter))
        );
        // Commands act on the chosen agent's session too.
        assert_eq!(
            in_chat(web, &bunter, "/new"),
            Inbound::Command {
                to: routed("bunter", "chat", &bunter),
                command: CoreCommand::New,
            }
        );
        // A plain chat follows the routes.
        assert_eq!(
            in_chat(web, hex, "hi"),
            Inbound::Message(routed("jeeves", "chat", hex))
        );
        // An agent that doesn't exist isn't replaced by a routed one.
        let gone = format!("gone.{hex}");
        assert_eq!(
            in_chat(web, &gone, "hi"),
            Inbound::Drop(DropReason::UnknownAgent(AgentId::new("gone").unwrap()))
        );
        // Only web chats choose their agent: elsewhere such an id is just a conversation id.
        let whatsapp: Source = ("whatsapp", "whatsapp", "whatsapp");
        assert_eq!(
            in_chat(whatsapp, &bunter, "hi"),
            Inbound::Message(routed("jeeves", "whatsapp", &bunter))
        );
        // Ownership is still checked first.
        let mut stranger = message("stranger", "hi");
        stranger.conversation.id = ConversationId::new(&bunter).unwrap();
        assert_eq!(
            classify(&router, web, &stranger, &NoPendingApprovals),
            Inbound::Drop(DropReason::UnknownSender)
        );
    }

    #[test]
    fn specific_route_reaches_classify() {
        let irc = ("irc-libera", "irc", "irc:libera.chat");
        let mut m = message("account:stephen", "hi");
        m.conversation.id = ConversationId::new("#giles").unwrap();
        // `#giles` is a channel, but the test owner talks there in a direct conversation.
        assert_eq!(
            classify(&router(), irc, &m, &NoPendingApprovals),
            Inbound::Message(routed("butler", "irc-libera", "#giles"))
        );
    }

    #[test]
    fn core_commands() {
        let to = routed("jeeves", "whatsapp", OWNER_PHONE);
        let cases = [
            ("/new", Some(CoreCommand::New)),
            (" /Stop ", Some(CoreCommand::Stop)),
            ("/env reset", Some(CoreCommand::EnvReset)),
            ("/foo", None),
            ("/new please", None),
        ];
        for (text, want) in cases {
            let got = classify(
                &router(),
                WHATSAPP,
                &message(OWNER_PHONE, text),
                &NoPendingApprovals,
            );
            let want = match want {
                Some(command) => Inbound::Command {
                    to: to.clone(),
                    command,
                },
                None => Inbound::Message(to.clone()),
            };
            assert_eq!(got, want, "{text}");
        }
    }

    #[test]
    fn approval_replies_depend_on_pending_approvals() {
        let to = routed("jeeves", "whatsapp", OWNER_PHONE);
        let asked = RefCell::new(Vec::new());
        let pending = |at: &Routed, code: Option<&str>| {
            asked
                .borrow_mut()
                .push((at.clone(), code.map(str::to_owned)));
            code.is_none_or(|c| c == "4821")
        };
        let yes = classify(&router(), WHATSAPP, &message(OWNER_PHONE, "Yes"), &pending);
        assert_eq!(
            yes,
            Inbound::Command {
                to: to.clone(),
                command: CoreCommand::Approval(ApprovalReply {
                    answer: Answer::Yes,
                    code: None
                }),
            }
        );
        let coded = classify(
            &router(),
            WHATSAPP,
            &message(OWNER_PHONE, "yes! 4821"),
            &pending,
        );
        assert_eq!(
            coded,
            Inbound::Command {
                to: to.clone(),
                command: CoreCommand::Approval(ApprovalReply {
                    answer: Answer::YesForSession,
                    code: Some("4821".into())
                }),
            }
        );
        let stale = classify(
            &router(),
            WHATSAPP,
            &message(OWNER_PHONE, "no 1111"),
            &pending,
        );
        assert_eq!(stale, Inbound::Message(to.clone()));
        assert_eq!(
            asked.into_inner(),
            [
                (to.clone(), None),
                (to.clone(), Some("4821".into())),
                (to.clone(), Some("1111".into()))
            ]
        );
        let none = classify(
            &router(),
            WHATSAPP,
            &message(OWNER_PHONE, "no"),
            &NoPendingApprovals,
        );
        assert_eq!(none, Inbound::Message(to));
    }

    #[test]
    fn messages_with_attachments_are_never_commands() {
        let mut m = message(OWNER_PHONE, "/new");
        m.attachments.push(MediaRef {
            id: MediaId::from_digest([7; 32]),
            media_type: "image/png".into(),
            size: 1,
            name: None,
        });
        let pending = |_: &Routed, _: Option<&str>| -> bool { panic!("not asked") };
        assert_eq!(
            classify(&router(), WHATSAPP, &m, &pending),
            Inbound::Message(routed("jeeves", "whatsapp", OWNER_PHONE))
        );
        m.text = "yes".into();
        assert_eq!(
            classify(&router(), WHATSAPP, &m, &pending),
            Inbound::Message(routed("jeeves", "whatsapp", OWNER_PHONE))
        );
    }

    fn link_request(sender: &str, code: Option<&str>, linked: bool) -> Inbound {
        Inbound::Link(LinkRequest {
            namespace: "whatsapp".to_owned(),
            sender: NativeId::new(sender).unwrap(),
            conversation: ConversationId::new(sender).unwrap(),
            code: code.map(str::to_owned),
            linked,
        })
    }

    #[test]
    fn link_passes_for_an_unknown_sender_after_authentication_and_nowhere_else() {
        const STRANGER: &str = "+15559998888";
        // No route matches and no agent exists: `/link` is for no session.
        let router = with_routes(owner_config(&["whatsapp:+15551234567"]), &[], no_agents());
        let from_stranger = |text: &str| {
            classify(
                &router,
                WHATSAPP,
                &message(STRANGER, text),
                &NoPendingApprovals,
            )
        };
        assert_eq!(
            from_stranger("/link ABCD2345"),
            link_request(STRANGER, Some("ABCD2345"), false)
        );
        // The code goes on as typed, and case and spaces around the command don't matter.
        assert_eq!(
            from_stranger("  /LINK abcd2345 \n"),
            link_request(STRANGER, Some("abcd2345"), false)
        );
        assert_eq!(from_stranger("/link"), link_request(STRANGER, None, false));

        // Nothing else gets through the same door.
        let unknown = Inbound::Drop(DropReason::UnknownSender);
        for text in [
            "/new",
            "/stop",
            "hi",
            "/link A B",
            "/linked ABCD2345",
            "yes",
        ] {
            assert_eq!(from_stranger(text), unknown, "{text:?}");
        }
        let mut with_attachment = message(STRANGER, "/link ABCD2345");
        with_attachment.attachments.push(MediaRef {
            id: MediaId::from_digest([7; 32]),
            media_type: "image/png".into(),
            size: 1,
            name: None,
        });
        assert_eq!(
            classify(&router, WHATSAPP, &with_attachment, &NoPendingApprovals),
            unknown
        );

        // Unauthenticated senders, groups, and channels that lie about their namespace are
        // dropped first, as for any message.
        let mut forged = message(STRANGER, "/link ABCD2345");
        forged.sender_authenticated = false;
        assert_eq!(
            classify(&router, WHATSAPP, &forged, &NoPendingApprovals),
            Inbound::Drop(DropReason::Unauthenticated)
        );
        let mut group = message(STRANGER, "/link ABCD2345");
        group.conversation.kind = ConversationKind::Group;
        assert_eq!(
            classify(&router, WHATSAPP, &group, &NoPendingApprovals),
            Inbound::Drop(DropReason::GroupConversation)
        );
        let lying: Source = ("whatsapp", "whatsapp", "irc:libera.chat");
        assert_eq!(
            classify(
                &router,
                lying,
                &message(STRANGER, "/link ABCD2345"),
                &NoPendingApprovals
            ),
            Inbound::Drop(invalid_ns())
        );
    }

    #[test]
    fn link_from_a_known_sender_is_marked_and_a_disabled_users_is_dropped() {
        let users = UserList::new(vec![
            User::member("owner", &["whatsapp:+15551234567", "whatsapp:100@lid"]),
            User::member("ann", &["whatsapp:+15557654321"]),
        ]);
        let router = with_routes(
            users.clone(),
            &[route(None, None, None, "jeeves")],
            no_agents(),
        );
        assert_eq!(
            classify(
                &router,
                WHATSAPP,
                &message(OWNER_PHONE, "/link ABCD2345"),
                &NoPendingApprovals
            ),
            link_request(OWNER_PHONE, Some("ABCD2345"), true)
        );
        // An alias is the sender's identity too.
        let mut by_lid = message("+15550000000", "/link ABCD2345");
        by_lid.sender_aliases = vec![NativeId::new("100@lid").unwrap()];
        assert_eq!(
            classify(&router, WHATSAPP, &by_lid, &NoPendingApprovals),
            link_request("+15550000000", Some("ABCD2345"), true)
        );
        let mut all = users.get().users().to_vec();
        all[1].state = UserState::Disabled;
        users.set(all);
        assert_eq!(
            classify(
                &router,
                WHATSAPP,
                &message("+15557654321", "/link ABCD2345"),
                &NoPendingApprovals
            ),
            Inbound::Drop(DropReason::DisabledUser(UserId::new("ann").unwrap()))
        );
    }

    #[test]
    fn link_in_the_web_chat_is_an_ordinary_message() {
        let router = with_routes(
            owner_config(&["web:owner"]),
            &[route(None, None, None, "jeeves")],
            no_agents(),
        );
        let web: Source = ("chat", chat_ids::COMPONENT, "web");
        let mut m = message("owner", "/link ABCD2345");
        m.conversation.id = ConversationId::new("0123456789abcdef0123456789abcdef").unwrap();
        assert!(matches!(
            classify(&router, web, &m, &NoPendingApprovals),
            Inbound::Message(_)
        ));
    }

    #[test]
    fn a_link_request_never_shows_its_code() {
        let Inbound::Link(request) = link_request("+15551234567", Some("ABCD2345"), false) else {
            unreachable!()
        };
        let shown = format!("{request:?}");
        assert!(!shown.contains("ABCD2345"), "{shown}");
        assert!(shown.contains("+15551234567"));
    }

    #[test]
    fn drop_reasons_display() {
        assert_eq!(
            DropReason::UnknownSender.to_string(),
            "sender is not the identity of any user"
        );
        assert_eq!(
            DropReason::DisabledUser(UserId::new("ann").unwrap()).to_string(),
            "user `ann` is disabled"
        );
        assert_eq!(
            invalid_ns().to_string(),
            "invalid channel: identity namespace \"irc:libera.chat\" must be \"whatsapp\" or \
             start with \"whatsapp:\""
        );
    }
}
