//! Routing rules (PLAN section 6.1): [`Route`], and [`RouteList`], the ordered list the
//! [router](crate::router::Router) reads for every inbound message.

use std::sync::{Arc, PoisonError, RwLock};

use giles_types::{AgentId, ConversationKind, ConversationRef};

use crate::config::InstanceName;

/// A routing rule. It matches a conversation when every key it sets matches; rules are tried in
/// order and the first match wins.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Route {
    /// Match this channel instance.
    pub channel: Option<InstanceName>,
    /// Match this conversation kind.
    pub kind: Option<ConversationKind>,
    /// Match this conversation id, as the channel reports it (e.g. `#giles`).
    pub conversation: Option<String>,
    /// The agent that handles matching conversations.
    pub agent: AgentId,
}

impl Route {
    /// Whether the rule matches a conversation on `instance`. Conversation ids compare exactly.
    pub fn matches(&self, instance: &InstanceName, conversation: &ConversationRef) -> bool {
        self.channel.as_ref().is_none_or(|c| c == instance)
            && self.kind.is_none_or(|k| k == conversation.kind)
            && self
                .conversation
                .as_deref()
                .is_none_or(|c| c == conversation.id.as_str())
    }
}

/// The ordered routes the router uses. Cheap to clone; clones share the list.
///
/// [`get`](Self::get) returns a snapshot, so a message is routed by one list even if the list
/// changes meanwhile.
#[derive(Debug, Clone, Default)]
pub struct RouteList {
    current: Arc<RwLock<Arc<[Route]>>>,
}

impl RouteList {
    /// Starts with `routes`, first match first.
    pub fn new(routes: Vec<Route>) -> Self {
        Self {
            current: Arc::new(RwLock::new(routes.into())),
        }
    }

    /// The routes now.
    pub fn get(&self) -> Arc<[Route]> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the routes; messages routed after this returns use them.
    pub fn set(&self, routes: Vec<Route>) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = routes.into();
    }
}
