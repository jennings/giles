//! Routing rules (PLAN sections 6.1 and 11.2): [`Route`], [`RouteList`], the ordered
//! list the [router](crate::router::Router) reads for every inbound message, and [`Routes`], which
//! keeps that list in the database and changes it for the web UI.
//!
//! # Storage and liveness
//!
//! The ordered list lives in the `routes` table. [`Routes`] also keeps the list as a [`RouteList`]
//! snapshot that the router shares, and replaces it after every successful change, so the next
//! inbound message is routed by the new list and nothing restarts. A snapshot, rather than a
//! database read per message, keeps [`Router::classify`](crate::router::Router::classify)
//! synchronous and off the database for every message, including the ones that are dropped.
//! Changes are serialized by one lock that spans reading the stored list, validating the change,
//! writing it, and replacing the snapshot, so the snapshot always equals what is stored. Only this
//! process changes the table.
//!
//! # Validation
//!
//! A route is validated like a hand-written `[[routes]]` rule was: the channel instance and the
//! agent exist ([`Config`], the running one, so an agent created in the web UI can be routed to),
//! the kind is `direct` or `group`, the conversation is a plain, bounded id, and the agent is
//! required. Beyond that, a change is refused if the resulting list has a route that can never
//! match because an earlier route matches everything it does ([`RouteError::Unreachable`], with a
//! special message for a duplicate) or if the changed route would hide a later route
//! ([`RouteError::Hides`]). Hiding is judged against the list before the change: a route that was
//! already unreachable stays so without blocking unrelated changes (the page flags it, and so does
//! the startup log), so a list imported from `giles.toml`, which never checked this, can still be
//! edited into shape.
//!
//! # Import
//!
//! `giles.toml`'s `[[routes]]` are imported once, by [`Routes::open`], when the import has never
//! run (the database records that it did). They're validated first, exactly like a route made in
//! the web UI except that a shadowed route is kept as written. After that they're never read
//! again; `open` warns while `giles.toml` still has them.

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use giles_store::{NewRoute, RouteId, Store, StoredRoute};
use giles_types::{AgentId, ConversationKind, ConversationRef};
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::{Config, InstanceName, LiveConfig};

/// The longest conversation id a route accepts, in bytes.
pub const MAX_CONVERSATION_BYTES: usize = 512;

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

impl Route {
    /// What the route matches, for messages: `every conversation`, or the keys it sets, e.g.
    /// `channel libera, conversation #giles`.
    pub fn criteria(&self) -> String {
        let mut parts = Vec::new();
        if let Some(channel) = &self.channel {
            parts.push(format!("channel {channel}"));
        }
        if let Some(kind) = self.kind {
            parts.push(format!("{} conversations", kind_name(kind)));
        }
        if let Some(conversation) = &self.conversation {
            parts.push(format!("conversation {conversation}"));
        }
        if parts.is_empty() {
            "every conversation".to_owned()
        } else {
            parts.join(", ")
        }
    }

    /// Whether every conversation `other` matches is matched by `self` too: each key `self` sets
    /// is set to the same value by `other`.
    fn covers(&self, other: &Route) -> bool {
        (self.channel.is_none() || self.channel == other.channel)
            && (self.kind.is_none() || self.kind == other.kind)
            && (self.conversation.is_none() || self.conversation == other.conversation)
    }

    /// Whether `self` and `other` match exactly the same conversations.
    fn same_match(&self, other: &Route) -> bool {
        self.channel == other.channel
            && self.kind == other.kind
            && self.conversation == other.conversation
    }
}

/// The text of a kind, as the store and the forms write it.
pub fn kind_name(kind: ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Group => "group",
        _ => "direct",
    }
}

/// A route as typed in a form (or as read from `giles.toml`): text, where empty means any
/// (except for the agent).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RouteSpec {
    /// A channel instance name.
    pub channel: String,
    /// `direct` or `group`.
    pub kind: String,
    /// A conversation id, exactly as the channel reports it.
    pub conversation: String,
    /// An agent id.
    pub agent: String,
}

impl RouteSpec {
    /// The spec of an existing route, to fill the edit form.
    pub fn of(route: &Route) -> Self {
        Self {
            channel: route
                .channel
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            kind: route.kind.map(kind_name).unwrap_or_default().to_owned(),
            conversation: route.conversation.clone().unwrap_or_default(),
            agent: route.agent.to_string(),
        }
    }
}

/// The part of a route a [`RouteError::Invalid`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteField {
    /// The channel instance.
    Channel,
    /// The conversation kind.
    Kind,
    /// The conversation id.
    Conversation,
    /// The agent.
    Agent,
}

impl fmt::Display for RouteField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Channel => "channel",
            Self::Kind => "kind",
            Self::Conversation => "conversation",
            Self::Agent => "agent",
        })
    }
}

/// Why a route wasn't added, changed, removed, or moved. Nothing changed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RouteError {
    /// A value is wrong.
    #[error("{field}: {reason}")]
    Invalid {
        /// Which one.
        field: RouteField,
        /// What is wrong with it.
        reason: String,
    },
    /// Earlier routes match every conversation this one does, so it could never be used.
    #[error(
        "this route could never be used: {by} would always be tried first and match every \
         conversation it does"
    )]
    Unreachable {
        /// The earlier route or routes with their positions, e.g. `route 2 (channel libera)`. A
        /// route with no kind can be covered by two: one for direct and one for group
        /// conversations.
        by: String,
    },
    /// An earlier route matches exactly the same conversations.
    #[error("route {by} already matches {by_criteria}")]
    Duplicate {
        /// The 1-based position of the earlier route.
        by: usize,
        /// What both routes match.
        by_criteria: String,
    },
    /// The change would make a later route unusable.
    #[error(
        "this would leave route {position} ({criteria}) unusable: this route{} would always be \
         tried first and match every conversation it does",
        .with.as_ref().map(|other| format!(" and {other}")).unwrap_or_default()
    )]
    Hides {
        /// The 1-based position of the hidden route, in the list as it would be.
        position: usize,
        /// What the hidden route matches.
        criteria: String,
        /// The other earlier route that hides it together with this one, e.g.
        /// `route 1 (channel libera, direct conversations)`, when it takes two: one for direct
        /// and one for group conversations.
        with: Option<String>,
    },
    /// The route (or the one to insert before) doesn't exist, e.g. it was removed in another tab.
    #[error("that route no longer exists")]
    NotFound,
    /// The database failed.
    #[error("cannot store the routes: {0}")]
    Store(#[from] giles_store::Error),
    /// A stored route is malformed.
    #[error("the stored routes are damaged: {0}")]
    Corrupt(String),
}

impl RouteError {
    /// Whether the owner can fix it in the form (else it's the server's problem).
    pub fn is_invalid_input(&self) -> bool {
        !matches!(self, Self::Store(_) | Self::Corrupt(_) | Self::NotFound)
    }
}

/// Startup failed to import or load the routes.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RoutesError {
    /// A `[[routes]]` rule in `giles.toml` is invalid, so it can't be imported. Fix it or remove it.
    #[error("{file}: routes[{index}]{}: {reason}", key_suffix(.key))]
    Import {
        /// The file.
        file: String,
        /// The rule's index in `[[routes]]`.
        index: usize,
        /// The key, if the problem is in one.
        key: Option<&'static str>,
        /// What is wrong.
        reason: String,
    },
    /// The database failed.
    #[error("cannot load the routes: {0}")]
    Store(#[from] giles_store::Error),
    /// A stored route is malformed.
    #[error("the stored routes are damaged: {0}")]
    Corrupt(String),
}

fn key_suffix(key: &Option<&'static str>) -> String {
    key.map(|k| format!(".{k}")).unwrap_or_default()
}

/// A route as shown on the routes page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteView {
    /// The route's stable id.
    pub id: RouteId,
    /// The 1-based position.
    pub position: usize,
    /// The rule.
    pub route: Route,
    /// What is wrong with it now, in words: an earlier route hides it, its channel instance or
    /// agent isn't configured. Empty for a route that works.
    pub problems: Vec<String>,
}

/// An entry of a list under evaluation: the stored id, or `None` for a route being added.
type Entry = (Option<RouteId>, Route);

/// For each route, the earlier routes that between them match every conversation it does: the
/// first one that covers it alone, or for a route with no kind, the first covering its direct
/// conversations and the first covering its group conversations (in order of position).
fn shadowed_by(routes: &[Entry]) -> Vec<Option<Vec<usize>>> {
    (0..routes.len())
        .map(|i| {
            let route = &routes[i].1;
            let first = |wanted: &Route| (0..i).find(|&j| routes[j].1.covers(wanted));
            if let Some(j) = first(route) {
                return Some(vec![j]);
            }
            if route.kind.is_some() {
                return None;
            }
            let of_kind = |kind| Route {
                kind: Some(kind),
                ..route.clone()
            };
            let direct = first(&of_kind(ConversationKind::Direct))?;
            let group = first(&of_kind(ConversationKind::Group))?;
            let mut by = vec![direct, group];
            by.sort_unstable();
            Some(by)
        })
        .collect()
}

/// `route 2 (channel libera)`, or `routes 1 (...) and 2 (...)`.
fn name_routes(routes: &[Entry], by: &[usize]) -> String {
    let one = |j: usize| format!("{} ({})", j + 1, routes[j].1.criteria());
    match by {
        [only] => format!("route {}", one(*only)),
        _ => format!(
            "routes {}",
            by.iter().map(|&j| one(j)).collect::<Vec<_>>().join(" and ")
        ),
    }
}

/// Refuses `new` if it has an unreachable route that `old` didn't have, or if the route `changed`
/// (`None` for the one being added, which has no id yet) is unreachable at all.
fn check_reachable(
    old: &[Entry],
    new: &[Entry],
    changed: Option<RouteId>,
) -> Result<(), RouteError> {
    let already: HashSet<RouteId> = shadowed_by(old)
        .iter()
        .zip(old)
        .filter(|(by, _)| by.is_some())
        .filter_map(|(_, (id, _))| *id)
        .collect();
    for (i, by) in shadowed_by(new).into_iter().enumerate() {
        let Some(by) = by else { continue };
        let (id, route) = &new[i];
        if *id == changed {
            return Err(match by[..] {
                [only] if new[only].1.same_match(route) => RouteError::Duplicate {
                    by: only + 1,
                    by_criteria: new[only].1.criteria(),
                },
                _ => RouteError::Unreachable {
                    by: name_routes(new, &by),
                },
            });
        }
        if id.is_some_and(|id| already.contains(&id)) {
            continue;
        }
        let others: Vec<usize> = by.into_iter().filter(|&j| new[j].0 != changed).collect();
        return Err(RouteError::Hides {
            position: i + 1,
            criteria: route.criteria(),
            with: (!others.is_empty()).then(|| name_routes(new, &others)),
        });
    }
    Ok(())
}

fn invalid(field: RouteField, reason: impl Into<String>) -> RouteError {
    RouteError::Invalid {
        field,
        reason: reason.into(),
    }
}

/// Checks `spec` like a hand-written route: every value, and that the channel instance and the
/// agent exist in `config`.
fn validate(spec: &RouteSpec, config: &Config) -> Result<Route, RouteError> {
    let channel = match spec.channel.trim() {
        "" => None,
        name => {
            let name =
                InstanceName::new(name).map_err(|reason| invalid(RouteField::Channel, reason))?;
            if !config.channels.contains_key(&name) {
                return Err(invalid(
                    RouteField::Channel,
                    format!("no channel instance `{name}` (`[channel.{name}]`)"),
                ));
            }
            Some(name)
        }
    };
    let kind = match spec.kind.trim() {
        "" => None,
        "direct" => Some(ConversationKind::Direct),
        "group" => Some(ConversationKind::Group),
        other => {
            return Err(invalid(
                RouteField::Kind,
                format!("unknown kind {other:?}: use `direct` or `group`, or leave it empty"),
            ));
        }
    };
    let conversation = match spec.conversation.trim() {
        "" => None,
        id if id.len() > MAX_CONVERSATION_BYTES => {
            return Err(invalid(
                RouteField::Conversation,
                format!("too long: at most {MAX_CONVERSATION_BYTES} bytes"),
            ));
        }
        id if id.chars().any(char::is_control) => {
            return Err(invalid(
                RouteField::Conversation,
                "must not contain control characters",
            ));
        }
        id => Some(id.to_owned()),
    };
    let agent = spec.agent.trim();
    if agent.is_empty() {
        return Err(invalid(RouteField::Agent, "choose an agent"));
    }
    if !crate::config::is_valid_name(agent) {
        return Err(invalid(
            RouteField::Agent,
            format!("invalid agent name {agent:?}: use only letters, digits, and `-`"),
        ));
    }
    let agent = AgentId::new(agent).map_err(|e| invalid(RouteField::Agent, e.to_string()))?;
    if !config.agents.contains_key(&agent) {
        return Err(invalid(
            RouteField::Agent,
            format!("no agent `{agent}` (agents/{agent}/agent.toml)"),
        ));
    }
    Ok(Route {
        channel,
        kind,
        conversation,
        agent,
    })
}

fn to_new(route: &Route) -> NewRoute {
    NewRoute {
        channel: route.channel.as_ref().map(ToString::to_string),
        kind: route.kind,
        conversation: route.conversation.clone(),
        agent: route.agent.clone(),
    }
}

fn from_stored(stored: &StoredRoute) -> Result<Route, String> {
    let route = &stored.route;
    Ok(Route {
        channel: route
            .channel
            .as_deref()
            .map(InstanceName::new)
            .transpose()
            .map_err(|e| format!("route {}: {e}", stored.id))?,
        kind: route.kind,
        conversation: route.conversation.clone(),
        agent: route.agent.clone(),
    })
}

/// What `[[routes]]` in `giles.toml` can hold.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyRoute {
    channel: Option<String>,
    kind: Option<String>,
    conversation: Option<String>,
    agent: String,
}

/// The `[[routes]]` rules of `giles.toml`, validated like routes made in the web UI, but with
/// shadowed routes kept as written.
fn import_rules(legacy: &[toml::Value], config: &Config) -> Result<Vec<Route>, RoutesError> {
    let at = |index, key, reason: String| RoutesError::Import {
        file: "giles.toml".to_owned(),
        index,
        key,
        reason,
    };
    legacy
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let raw: LegacyRoute = value
                .clone()
                .try_into()
                .map_err(|e: toml::de::Error| at(index, None, e.message().to_owned()))?;
            for (key, value) in [
                ("channel", &raw.channel),
                ("kind", &raw.kind),
                ("conversation", &raw.conversation),
            ] {
                if value.as_deref().is_some_and(|v| v.trim().is_empty()) {
                    return Err(at(index, Some(key), "must not be empty".into()));
                }
            }
            let spec = RouteSpec {
                channel: raw.channel.unwrap_or_default(),
                kind: raw.kind.unwrap_or_default(),
                conversation: raw.conversation.unwrap_or_default(),
                agent: raw.agent,
            };
            validate(&spec, config).map_err(|e| match e {
                RouteError::Invalid { field, reason } => {
                    let key = match field {
                        RouteField::Channel => "channel",
                        RouteField::Kind => "kind",
                        RouteField::Conversation => "conversation",
                        RouteField::Agent => "agent",
                    };
                    at(index, Some(key), reason)
                }
                other => at(index, None, other.to_string()),
            })
        })
        .collect()
}

/// The stored `entries` with what is wrong with each given `config`.
fn describe(entries: &[Entry], config: &Config) -> Vec<RouteView> {
    let shadows = shadowed_by(entries);
    entries
        .iter()
        .zip(shadows)
        .enumerate()
        .map(|(i, ((id, route), by))| {
            let mut problems = Vec::new();
            if let Some(by) = by {
                problems.push(match by[..] {
                    [only] if entries[only].1.same_match(route) => {
                        format!(
                            "Never used: route {} matches the same conversations.",
                            only + 1
                        )
                    }
                    _ => format!(
                        "Never used: {} would always be tried first and match everything it does.",
                        name_routes(entries, &by)
                    ),
                });
            }
            if let Some(channel) = &route.channel
                && !config.channels.contains_key(channel)
            {
                problems.push(format!("There is no channel instance {channel}."));
            }
            if !config.agents.contains_key(&route.agent) {
                problems.push(format!("There is no agent {}.", route.agent));
            }
            RouteView {
                id: id.expect("stored routes have ids"),
                position: i + 1,
                route: route.clone(),
                problems,
            }
        })
        .collect()
}

/// Why [`Routes::hold_unrouted`] didn't hold the routes.
#[derive(Debug)]
pub enum HoldError {
    /// These routes name the agent.
    Routed(Vec<RouteView>),
    /// The stored routes couldn't be read.
    Routes(RouteError),
}

/// Route changes held off by [`Routes::hold_unrouted`] until this is dropped.
#[derive(Debug)]
pub struct RoutesHold<'a> {
    _changing: tokio::sync::MutexGuard<'a, ()>,
}

/// The ordered routes, in the database, and the [`RouteList`] the router reads. See the module
/// documentation. Share it in an `Arc`.
#[derive(Debug)]
pub struct Routes {
    store: Store,
    config: LiveConfig,
    list: RouteList,
    /// Held across read, validate, write, and replacing the snapshot.
    changing: Mutex<()>,
}

impl Routes {
    /// Opens the routes of `store`: imports `giles.toml`'s `[[routes]]` if the import has never
    /// run, loads the stored list, and logs what's wrong with it. Validation uses `config`'s
    /// current configuration.
    ///
    /// # Errors
    ///
    /// [`RoutesError::Import`] if the import is due and a `[[routes]]` rule is invalid (nothing is
    /// imported), or a database error.
    pub async fn open(store: Store, config: LiveConfig) -> Result<Self, RoutesError> {
        let now = config.get();
        let legacy = now.legacy_routes.len();
        let mut imported = false;
        if !store.routes_imported().await? {
            let rules = import_rules(&now.legacy_routes, &now)?;
            let count = store
                .routes_import(rules.iter().map(to_new).collect(), jiff::Timestamp::now())
                .await?;
            imported = count.is_some();
            if let Some(count @ 1..) = count {
                info!(
                    count,
                    "imported the [[routes]] of giles.toml; manage routes in the web UI"
                );
            }
        }
        if legacy > 0 {
            if imported {
                info!(
                    "giles.toml still has [[routes]]: they were imported once and are not read again; \
                     remove them from giles.toml"
                );
            } else {
                warn!(
                    count = legacy,
                    "giles.toml has [[routes]], which are ignored: routes are managed in the web UI \
                     (/admin/routes) and were imported when they were first stored; remove them \
                     from giles.toml"
                );
            }
        }
        let routes = Self {
            store,
            config,
            list: RouteList::default(),
            changing: Mutex::new(()),
        };
        let entries = routes.load().await.map_err(|e| match e {
            RouteError::Store(e) => RoutesError::Store(e),
            other => RoutesError::Corrupt(other.to_string()),
        })?;
        for view in describe(&entries, &now) {
            for problem in view.problems {
                warn!(route = view.position, criteria = %view.route.criteria(), agent = %view.route.agent, "{problem}");
            }
        }
        Ok(routes)
    }

    /// The running configuration the routes are validated against.
    pub fn config(&self) -> &LiveConfig {
        &self.config
    }

    /// The list the router reads. It follows every change made through this object.
    pub fn list(&self) -> RouteList {
        self.list.clone()
    }

    /// Reads the stored routes and makes them the list the router uses.
    async fn load(&self) -> Result<Vec<Entry>, RouteError> {
        let stored = self.store.routes().await?;
        let entries = stored
            .iter()
            .map(|s| Ok((Some(s.id), from_stored(s)?)))
            .collect::<Result<Vec<Entry>, String>>()
            .map_err(RouteError::Corrupt)?;
        self.list
            .set(entries.iter().map(|(_, r)| r.clone()).collect());
        Ok(entries)
    }

    /// Makes `entries`, just written to the store, the list the router uses.
    fn publish(&self, entries: &[Entry]) {
        self.list
            .set(entries.iter().map(|(_, r)| r.clone()).collect());
    }

    /// Every route in order, with what is wrong with each now.
    ///
    /// # Errors
    ///
    /// A database error, or a stored route that isn't valid.
    pub async fn view(&self) -> Result<Vec<RouteView>, RouteError> {
        let _changing = self.changing.lock().await;
        let entries = self.load().await?;
        Ok(describe(&entries, &self.config.get()))
    }

    /// Adds the route `spec` before the route `before` (at the end if `None`). The new list is the
    /// router's from the next message on.
    ///
    /// # Errors
    ///
    /// [`RouteError`]: an invalid value, a route that couldn't be used or would hide one, `before`
    /// not existing, or a database error. Nothing changes.
    pub async fn add(
        &self,
        spec: &RouteSpec,
        before: Option<RouteId>,
    ) -> Result<RouteId, RouteError> {
        let _changing = self.changing.lock().await;
        let route = validate(spec, &self.config.get())?;
        let old = self.load().await?;
        let at = match before {
            Some(id) => old
                .iter()
                .position(|(r, _)| *r == Some(id))
                .ok_or(RouteError::NotFound)?,
            None => old.len(),
        };
        let mut new = old.clone();
        new.insert(at, (None, route.clone()));
        check_reachable(&old, &new, None)?;
        let id = self.store.route_insert(at, to_new(&route)).await?;
        new[at].0 = Some(id);
        self.publish(&new);
        Ok(id)
    }

    /// Replaces the rule of route `id`, keeping its place.
    ///
    /// # Errors
    ///
    /// As [`add`](Self::add); [`RouteError::NotFound`] if there is no route `id`.
    pub async fn update(&self, id: RouteId, spec: &RouteSpec) -> Result<(), RouteError> {
        let _changing = self.changing.lock().await;
        let route = validate(spec, &self.config.get())?;
        let old = self.load().await?;
        let at = old
            .iter()
            .position(|(r, _)| *r == Some(id))
            .ok_or(RouteError::NotFound)?;
        let mut new = old.clone();
        new[at].1 = route.clone();
        check_reachable(&old, &new, Some(id))?;
        if !self.store.route_update(id, to_new(&route)).await? {
            return Err(RouteError::NotFound);
        }
        self.publish(&new);
        Ok(())
    }

    /// Holds route changes off while the caller takes `agent` out of the running configuration:
    /// returns the guard if no route names `agent`, else those routes, in order. A route can't be
    /// added or changed to name an agent while the guard is held, and the check of an agent's
    /// existence in [`add`](Self::add) and [`update`](Self::update) runs under the same lock, so
    /// no route can end up naming an agent that is gone. Drop the guard once the agent is out.
    ///
    /// # Errors
    ///
    /// [`HoldError::Routed`] with the routes that name the agent (with their position and rule),
    /// or [`HoldError::Routes`] if the stored routes can't be read.
    pub async fn hold_unrouted(&self, agent: &AgentId) -> Result<RoutesHold<'_>, HoldError> {
        let changing = self.changing.lock().await;
        let entries = self.load().await.map_err(HoldError::Routes)?;
        let naming: Vec<RouteView> = describe(&entries, &self.config.get())
            .into_iter()
            .filter(|view| view.route.agent == *agent)
            .collect();
        if naming.is_empty() {
            Ok(RoutesHold {
                _changing: changing,
            })
        } else {
            Err(HoldError::Routed(naming))
        }
    }

    /// Removes route `id`.
    ///
    /// # Errors
    ///
    /// [`RouteError::NotFound`] if there is no route `id`, or a database error.
    pub async fn remove(&self, id: RouteId) -> Result<(), RouteError> {
        let _changing = self.changing.lock().await;
        let mut entries = self.load().await?;
        if !self.store.route_remove(id).await? {
            return Err(RouteError::NotFound);
        }
        entries.retain(|(r, _)| *r != Some(id));
        self.publish(&entries);
        Ok(())
    }

    /// Moves route `id` one place earlier (`up`) or later. At the end of the list it stays.
    ///
    /// # Errors
    ///
    /// As [`add`](Self::add): a move that would hide a route is refused.
    pub async fn move_route(&self, id: RouteId, up: bool) -> Result<(), RouteError> {
        let _changing = self.changing.lock().await;
        let old = self.load().await?;
        let from = old
            .iter()
            .position(|(r, _)| *r == Some(id))
            .ok_or(RouteError::NotFound)?;
        let to = if up {
            from.saturating_sub(1)
        } else {
            (from + 1).min(old.len() - 1)
        };
        if to == from {
            return Ok(());
        }
        let mut new = old.clone();
        let entry = new.remove(from);
        new.insert(to, entry);
        check_reachable(&old, &new, Some(id))?;
        if !self.store.route_move(id, to).await? {
            return Err(RouteError::NotFound);
        }
        self.publish(&new);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use giles_types::ConversationId;

    use super::*;
    use crate::config::LoadOptions;
    use crate::paths::DataRoot;
    use crate::router::Router;
    use crate::sessions::AgentPolicy;

    const WEB: &str = "web";
    const LIBERA: &str = "libera";

    /// A configuration with channels `web` and `libera` and agents `jeeves` and `bunter`, and
    /// `extra` appended to giles.toml.
    fn config(dir: &Path, extra: &str) -> Config {
        let root = DataRoot::new(dir, &dir.join("tmp")).unwrap();
        std::fs::write(
            root.config_file(),
            format!(
                "[owner]\nidentities = [\"web:owner\"]\n\
                 [provider.fake]\ncomponent = \"openai\"\n\
                 [channel.web]\n[channel.libera]\ncomponent = \"irc\"\n{extra}"
            ),
        )
        .unwrap();
        for agent in ["jeeves", "bunter"] {
            let agent_dir = root.agents_dir().join(agent);
            std::fs::create_dir_all(&agent_dir).unwrap();
            std::fs::write(agent_dir.join("agent.toml"), "models = [\"fake/m\"]\n").unwrap();
        }
        Config::load_with(
            &root,
            &LoadOptions {
                env: &|_| None,
                giles_exe: None,
                ..LoadOptions::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    struct Harness {
        dir: tempfile::TempDir,
        store: Store,
        routes: Routes,
    }

    impl Harness {
        async fn new() -> Self {
            Self::with("").await
        }

        async fn with(extra: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path().join("giles.db")).await.unwrap();
            let config = LiveConfig::new(Arc::new(config(dir.path(), extra)));
            let routes = Routes::open(store.clone(), config).await.unwrap();
            Self { dir, store, routes }
        }

        /// Opens the routes again over the same store, with a new giles.toml.
        async fn reopen(&self, extra: &str) -> Result<Routes, RoutesError> {
            let config = LiveConfig::new(Arc::new(config(self.dir.path(), extra)));
            Routes::open(self.store.clone(), config).await
        }

        async fn add(&self, channel: &str, conversation: &str, agent: &str) -> RouteId {
            self.routes
                .add(&spec(channel, "", conversation, agent), None)
                .await
                .unwrap()
        }

        /// The agents of the stored routes, in order.
        async fn agents(&self) -> Vec<String> {
            self.store
                .routes()
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.route.agent.to_string())
                .collect()
        }

        /// The agent the router would choose for a direct conversation.
        fn pick(&self, channel: &str, conversation: &str) -> Option<String> {
            let router = Router::new(
                &crate::config::OwnerConfig {
                    identities: vec![crate::config::Identity::new("web:owner").unwrap()],
                    default: crate::config::Identity::new("web:owner").unwrap(),
                    timezone: None,
                },
                self.routes.list(),
                Arc::new(std::collections::HashMap::<AgentId, AgentPolicy>::new()),
            );
            router
                .route(
                    &InstanceName::new(channel).unwrap(),
                    &ConversationRef {
                        id: ConversationId::new(conversation).unwrap(),
                        kind: ConversationKind::Direct,
                        title: None,
                    },
                )
                .map(|a| a.to_string())
        }
    }

    fn spec(channel: &str, kind: &str, conversation: &str, agent: &str) -> RouteSpec {
        RouteSpec {
            channel: channel.into(),
            kind: kind.into(),
            conversation: conversation.into(),
            agent: agent.into(),
        }
    }

    fn message(err: RouteError) -> String {
        err.to_string()
    }

    #[tokio::test]
    async fn an_agent_can_be_held_unrouted_only_while_no_route_names_it() {
        let h = Harness::new().await;
        let bunter = AgentId::new("bunter").unwrap();
        let catch_all = h.add("", "", "jeeves").await;
        let specific = h
            .routes
            .add(&spec(LIBERA, "", "#giles", "bunter"), Some(catch_all))
            .await
            .unwrap();

        // Named by routes: they come back, in order, with where they are.
        let Err(HoldError::Routed(named)) = h.routes.hold_unrouted(&bunter).await else {
            panic!("bunter is routed");
        };
        assert_eq!(
            named.iter().map(|v| (v.id, v.position)).collect::<Vec<_>>(),
            [(specific, 1)]
        );
        assert!(named[0].route.criteria().contains("#giles"), "{named:?}");

        // Once they're gone, the hold is granted, and while it is held no route can be added for
        // the agent, however the add got there first (the check is under the same lock).
        h.routes.remove(specific).await.unwrap();
        let hold = h.routes.hold_unrouted(&bunter).await.unwrap();
        let late = spec(LIBERA, "", "#other", "bunter");
        let racing = h.routes.add(&late, None);
        tokio::pin!(racing);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut racing)
                .await
                .is_err(),
            "an add waits for the hold"
        );
        // The agent goes while the add waits: the add is then refused.
        h.routes.config().remove_agent(&bunter).unwrap();
        drop(hold);
        let refused = racing.await.unwrap_err();
        assert!(matches!(refused, RouteError::Invalid { .. }), "{refused}");
        assert_eq!(h.agents().await, ["jeeves"]);
    }

    #[tokio::test]
    async fn the_router_follows_every_change_without_a_restart() {
        let h = Harness::new().await;
        assert_eq!(h.pick(LIBERA, "#giles"), None);

        let catch_all = h.add("", "", "jeeves").await;
        assert_eq!(h.pick(LIBERA, "#giles").as_deref(), Some("jeeves"));

        // A more specific route goes before the catch-all.
        let first = h
            .routes
            .add(&spec(LIBERA, "", "#giles", "bunter"), Some(catch_all))
            .await
            .unwrap();
        assert_eq!(h.pick(LIBERA, "#giles").as_deref(), Some("bunter"));
        assert_eq!(h.pick(LIBERA, "#other").as_deref(), Some("jeeves"));
        assert_eq!(h.pick(WEB, "#giles").as_deref(), Some("jeeves"));

        h.routes
            .update(first, &spec(LIBERA, "", "#other", "bunter"))
            .await
            .unwrap();
        assert_eq!(h.pick(LIBERA, "#giles").as_deref(), Some("jeeves"));
        assert_eq!(h.pick(LIBERA, "#other").as_deref(), Some("bunter"));

        h.routes.remove(first).await.unwrap();
        assert_eq!(h.pick(LIBERA, "#other").as_deref(), Some("jeeves"));
        h.routes.remove(catch_all).await.unwrap();
        assert_eq!(h.pick(LIBERA, "#other"), None);
    }

    #[tokio::test]
    async fn the_first_matching_route_wins_and_moving_changes_who_that_is() {
        let h = Harness::new().await;
        // Both match #giles on libera; neither covers the other.
        let by_channel = h.add(LIBERA, "", "jeeves").await;
        let by_conversation = h.add("", "#giles", "bunter").await;
        assert_eq!(h.pick(LIBERA, "#giles").as_deref(), Some("jeeves"));
        assert_eq!(h.pick(WEB, "#giles").as_deref(), Some("bunter"));

        h.routes.move_route(by_conversation, true).await.unwrap();
        assert_eq!(h.agents().await, ["bunter", "jeeves"]);
        assert_eq!(h.pick(LIBERA, "#giles").as_deref(), Some("bunter"));

        h.routes.move_route(by_conversation, false).await.unwrap();
        assert_eq!(h.agents().await, ["jeeves", "bunter"]);
        assert_eq!(h.pick(LIBERA, "#giles").as_deref(), Some("jeeves"));

        // Moving past either end changes nothing.
        h.routes.move_route(by_channel, true).await.unwrap();
        h.routes.move_route(by_conversation, false).await.unwrap();
        assert_eq!(h.agents().await, ["jeeves", "bunter"]);
    }

    #[tokio::test]
    async fn invalid_values_are_refused_with_the_field_and_nothing_is_stored() {
        let h = Harness::new().await;
        let cases = [
            (
                spec("irc", "", "", "jeeves"),
                RouteField::Channel,
                "no channel instance `irc`",
            ),
            (
                spec("a_b", "", "", "jeeves"),
                RouteField::Channel,
                "invalid instance name",
            ),
            (
                spec("", "dm", "", "jeeves"),
                RouteField::Kind,
                "unknown kind",
            ),
            (
                spec("", "", "a\nb", "jeeves"),
                RouteField::Conversation,
                "control characters",
            ),
            (
                spec("", "", &"x".repeat(MAX_CONVERSATION_BYTES + 1), "jeeves"),
                RouteField::Conversation,
                "too long",
            ),
            (spec("", "", "", ""), RouteField::Agent, "choose an agent"),
            (
                spec("", "", "", "wooster"),
                RouteField::Agent,
                "no agent `wooster`",
            ),
            (
                spec("", "", "", "a.b"),
                RouteField::Agent,
                "invalid agent name",
            ),
        ];
        for (bad, want_field, reason) in cases {
            let err = h.routes.add(&bad, None).await.unwrap_err();
            let shown = err.to_string();
            assert!(
                matches!(err, RouteError::Invalid { field, .. } if field == want_field),
                "{bad:?}: {shown}"
            );
            assert!(shown.contains(reason), "{shown}");
        }
        assert!(h.store.routes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn values_are_trimmed_and_blank_means_any() {
        let h = Harness::new().await;
        h.routes
            .add(
                &spec(" libera ", " direct ", "  #giles  ", " jeeves "),
                None,
            )
            .await
            .unwrap();
        let routes = h.routes.view().await.unwrap();
        assert_eq!(
            RouteSpec::of(&routes[0].route),
            spec("libera", "direct", "#giles", "jeeves")
        );
        h.routes
            .add(&spec("   ", "", "", "bunter"), None)
            .await
            .unwrap();
        assert_eq!(
            h.routes.view().await.unwrap()[1].route.criteria(),
            "every conversation"
        );
    }

    #[tokio::test]
    async fn a_duplicate_or_shadowed_route_is_refused() {
        let h = Harness::new().await;
        h.add(LIBERA, "#giles", "jeeves").await;

        let dup = h
            .routes
            .add(&spec(LIBERA, "", "#giles", "bunter"), None)
            .await
            .unwrap_err();
        assert!(matches!(dup, RouteError::Duplicate { by: 1, .. }), "{dup}");
        assert!(message(dup).contains("route 1 already matches channel libera"));

        h.add(LIBERA, "", "jeeves").await;
        // The channel-wide route 2 doesn't cover route 1, but now covers anything on libera.
        let shadowed = h
            .routes
            .add(&spec(LIBERA, "", "#other", "bunter"), None)
            .await
            .unwrap_err();
        assert!(
            matches!(&shadowed, RouteError::Unreachable { by } if by == "route 2 (channel libera)"),
            "{shadowed}"
        );
        let text = message(shadowed);
        assert!(
            text.contains("could never be used")
                && text.contains("route 2 (channel libera) would always be tried first"),
            "{text}"
        );

        h.add("", "", "bunter").await;
        let after_catch_all = h
            .routes
            .add(&spec(WEB, "", "", "jeeves"), None)
            .await
            .unwrap_err();
        assert!(
            matches!(&after_catch_all, RouteError::Unreachable { by } if by.starts_with("route 3 (")),
            "{after_catch_all}"
        );
        assert_eq!(h.agents().await, ["jeeves", "jeeves", "bunter"]);
    }

    #[tokio::test]
    async fn a_change_that_would_hide_a_later_route_is_refused() {
        let h = Harness::new().await;
        let specific = h.add(LIBERA, "#giles", "jeeves").await;
        let catch_all = h.add("", "", "bunter").await;

        // Inserting a catch-all, editing into one, and moving one up each hide route 1.
        let err = h
            .routes
            .add(&spec("", "", "", "jeeves"), Some(specific))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RouteError::Hides { position: 2, .. }),
            "{err}"
        );
        assert_eq!(
            message(err),
            "this would leave route 2 (channel libera, conversation #giles) unusable: this route \
             would always be tried first and match every conversation it does"
        );

        let err = h
            .routes
            .update(specific, &spec("", "", "", "jeeves"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RouteError::Hides { position: 2, .. }),
            "{err}"
        );

        let err = h.routes.move_route(catch_all, true).await.unwrap_err();
        assert!(
            matches!(err, RouteError::Hides { position: 2, .. }),
            "{err}"
        );
        assert_eq!(h.agents().await, ["jeeves", "bunter"]);
        assert_eq!(h.pick(LIBERA, "#giles").as_deref(), Some("jeeves"));
    }

    #[tokio::test]
    async fn a_route_may_keep_its_place_and_value_when_edited_to_the_same_rule() {
        let h = Harness::new().await;
        let id = h.add(LIBERA, "#giles", "jeeves").await;
        h.add("", "", "jeeves").await;
        // Only the agent changes: the route doesn't count as its own duplicate.
        h.routes
            .update(id, &spec(LIBERA, "", "#giles", "bunter"))
            .await
            .unwrap();
        assert_eq!(h.agents().await, ["bunter", "jeeves"]);
    }

    #[tokio::test]
    async fn routes_that_were_already_unreachable_are_flagged_and_do_not_block_other_changes() {
        // giles.toml never checked this, so an import can hold such a route.
        let h = Harness::with(
            "[[routes]]\nagent = \"jeeves\"\n[[routes]]\nchannel = \"libera\"\nagent = \"bunter\"\n\
             [[routes]]\nchannel = \"libera\"\nagent = \"bunter\"\n",
        )
        .await;
        let views = h.routes.view().await.unwrap();
        assert_eq!(views.len(), 3);
        assert!(views[0].problems.is_empty());
        for view in &views[1..] {
            assert!(
                view.problems[0].contains(
                    "Never used: route 1 (every conversation) would always be tried first"
                ),
                "{:?}",
                view.problems
            );
        }

        // Removing one is fine, and an edit that leaves another one unusable is still refused
        // for that route.
        h.routes.remove(views[2].id).await.unwrap();
        let err = h
            .routes
            .update(views[1].id, &spec(LIBERA, "", "", "jeeves"))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RouteError::Unreachable { by } if by.starts_with("route 1 (")),
            "{err}"
        );
        // Moving the catch-all down makes the other usable.
        h.routes.move_route(views[0].id, false).await.unwrap();
        assert!(
            h.routes
                .view()
                .await
                .unwrap()
                .iter()
                .all(|v| v.problems.is_empty())
        );
    }

    #[tokio::test]
    async fn a_route_with_no_kind_is_hidden_by_routes_that_cover_both_kinds() {
        let h = Harness::new().await;
        let direct = h
            .routes
            .add(&spec(LIBERA, "direct", "", "jeeves"), None)
            .await
            .unwrap();
        // Only direct conversations on libera are covered so far: the new route can still match
        // group ones.
        let all_libera = spec(LIBERA, "", "", "bunter");
        let group = h
            .routes
            .add(&spec(LIBERA, "group", "", "jeeves"), None)
            .await
            .unwrap();
        // Now the two earlier routes between them match all it does.
        let err = h.routes.add(&all_libera, None).await.unwrap_err();
        let text = message(err);
        assert!(
            text.contains(
                "routes 1 (channel libera, direct conversations) and 2 (channel libera, group \
                 conversations) would always be tried first"
            ),
            "{text}"
        );
        assert_eq!(h.agents().await, ["jeeves", "jeeves"]);

        // Adding the catch-all before them is fine, but then it hides both.
        let err = h.routes.add(&all_libera, Some(direct)).await.unwrap_err();
        assert!(
            matches!(err, RouteError::Hides { position: 2, .. }),
            "{err}"
        );
        h.routes.remove(group).await.unwrap();
        let all_libera_id = h.routes.add(&all_libera, None).await.unwrap();

        // A group route between the direct one and the catch-all hides it together with route 1.
        let err = h
            .routes
            .add(&spec(LIBERA, "group", "", "jeeves"), Some(all_libera_id))
            .await
            .unwrap_err();
        assert!(
            matches!(err, RouteError::Hides { position: 3, .. }),
            "{err}"
        );
        assert_eq!(
            message(err),
            "this would leave route 3 (channel libera) unusable: this route and route 1 (channel \
             libera, direct conversations) would always be tried first and match every \
             conversation it does"
        );

        // Moving such a route up names the other one and not the moved route itself.
        let any_group = h
            .routes
            .add(&spec("", "group", "", "jeeves"), None)
            .await
            .unwrap();
        let err = h.routes.move_route(any_group, true).await.unwrap_err();
        assert!(
            matches!(err, RouteError::Hides { position: 3, .. }),
            "{err}"
        );
        assert_eq!(
            message(err),
            "this would leave route 3 (channel libera) unusable: this route and route 1 (channel \
             libera, direct conversations) would always be tried first and match every \
             conversation it does"
        );
        assert_eq!(h.agents().await, ["jeeves", "bunter", "jeeves"]);

        // An imported list can already hold such a route: it is flagged, not refused.
        let imported = Harness::with(
            "[[routes]]\nchannel = \"libera\"\nkind = \"direct\"\nagent = \"jeeves\"\n\
             [[routes]]\nchannel = \"libera\"\nkind = \"group\"\nagent = \"jeeves\"\n\
             [[routes]]\nchannel = \"libera\"\nagent = \"bunter\"\n",
        )
        .await;
        let views = imported.routes.view().await.unwrap();
        assert!(views[1].problems.is_empty());
        assert!(
            views[2].problems[0].contains("routes 1 (")
                && views[2].problems[0].contains(" and 2 ("),
            "{:?}",
            views[2].problems
        );
    }

    #[tokio::test]
    async fn the_router_sees_exactly_what_was_stored_after_each_change() {
        let h = Harness::new().await;
        let a = h.add(LIBERA, "#a", "jeeves").await;
        let b = h.add(LIBERA, "#b", "bunter").await;
        let c = h
            .routes
            .add(&spec(WEB, "", "", "jeeves"), Some(b))
            .await
            .unwrap();
        let in_router = |h: &Harness| {
            h.routes
                .list()
                .get()
                .iter()
                .map(|r| r.criteria())
                .collect::<Vec<_>>()
        };
        h.routes.move_route(c, true).await.unwrap();
        h.routes
            .update(a, &spec(LIBERA, "", "#z", "bunter"))
            .await
            .unwrap();
        h.routes.remove(b).await.unwrap();
        // `view` reads the store; the router's list was set from the changes.
        let router_list = in_router(&h);
        let stored: Vec<String> = h
            .store
            .routes()
            .await
            .unwrap()
            .iter()
            .map(|s| from_stored(s).unwrap().criteria())
            .collect();
        assert_eq!(router_list, stored);
        assert_eq!(
            router_list,
            ["channel web", "channel libera, conversation #z"]
        );
    }

    #[tokio::test]
    async fn a_stored_route_to_something_that_is_gone_is_flagged() {
        let h = Harness::new().await;
        h.store
            .route_insert(
                0,
                NewRoute {
                    channel: Some("oftc".into()),
                    kind: None,
                    conversation: None,
                    agent: AgentId::new("wooster").unwrap(),
                },
            )
            .await
            .unwrap();
        let views = h.routes.view().await.unwrap();
        assert_eq!(
            views[0].problems,
            [
                "There is no channel instance oftc.",
                "There is no agent wooster."
            ]
        );
    }

    #[tokio::test]
    async fn a_route_that_no_longer_exists_is_not_found() {
        let h = Harness::new().await;
        let id = h.add("", "", "jeeves").await;
        h.routes.remove(id).await.unwrap();
        let ok = spec("", "", "", "jeeves");
        assert!(matches!(
            h.routes.remove(id).await,
            Err(RouteError::NotFound)
        ));
        assert!(matches!(
            h.routes.update(id, &ok).await,
            Err(RouteError::NotFound)
        ));
        assert!(matches!(
            h.routes.move_route(id, true).await,
            Err(RouteError::NotFound)
        ));
        assert!(matches!(
            h.routes.add(&ok, Some(id)).await,
            Err(RouteError::NotFound)
        ));
        assert!(h.store.routes().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn racing_changes_cannot_both_add_the_same_route() {
        let h = Arc::new(Harness::new().await);
        let attempts: Vec<_> = (0..8)
            .map(|_| {
                let h = h.clone();
                tokio::spawn(async move {
                    h.routes
                        .add(&spec(LIBERA, "", "#giles", "jeeves"), None)
                        .await
                        .is_ok()
                })
            })
            .collect();
        let mut wins = 0;
        for attempt in attempts {
            wins += usize::from(attempt.await.unwrap());
        }
        assert_eq!(wins, 1);
        assert_eq!(h.store.routes().await.unwrap().len(), 1);
        assert_eq!(h.pick(LIBERA, "#giles").as_deref(), Some("jeeves"));
    }

    const LEGACY: &str = "[[routes]]\nchannel = \"libera\"\nconversation = \"#giles\"\nagent = \"jeeves\"\n\
                          [[routes]]\nkind = \"direct\"\nagent = \"bunter\"\n";

    #[tokio::test]
    async fn giles_toml_routes_are_imported_in_order_when_none_are_stored() {
        let h = Harness::with(LEGACY).await;
        let views = h.routes.view().await.unwrap();
        assert_eq!(views.len(), 2);
        assert_eq!(
            RouteSpec::of(&views[0].route),
            spec("libera", "", "#giles", "jeeves")
        );
        assert_eq!(
            RouteSpec::of(&views[1].route),
            spec("", "direct", "", "bunter")
        );
        // The router has them from the start.
        assert_eq!(h.pick(LIBERA, "#giles").as_deref(), Some("jeeves"));
        assert_eq!(h.pick(WEB, "anything").as_deref(), Some("bunter"));
    }

    #[tokio::test]
    async fn the_import_happens_once_and_the_file_is_not_read_again() {
        let h = Harness::with(LEGACY).await;
        let first = h.routes.view().await.unwrap();

        // Restart with different rules in the file: the stored routes stand.
        let again = h.reopen("[[routes]]\nagent = \"bunter\"\n").await.unwrap();
        assert_eq!(again.view().await.unwrap(), first);

        // Even when the owner has deleted every route, a restart doesn't bring the file's back.
        for view in &first {
            h.routes.remove(view.id).await.unwrap();
        }
        let after = h.reopen(LEGACY).await.unwrap();
        assert!(after.view().await.unwrap().is_empty());
        assert_eq!(h.pick(LIBERA, "#giles"), None);
    }

    #[tokio::test]
    async fn the_file_is_not_read_even_when_it_is_invalid_after_the_import() {
        let h = Harness::with(LEGACY).await;
        // A rule for an agent that no longer exists can't stop a start that ignores the file.
        let again = h
            .reopen("[[routes]]\nagent = \"wooster\"\n[[routes]]\nnonsense = 1\n")
            .await
            .unwrap();
        assert_eq!(again.view().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_empty_config_imports_nothing_and_later_rules_in_the_file_are_not_imported() {
        let h = Harness::new().await;
        assert!(h.routes.view().await.unwrap().is_empty());
        // The import already ran (and found nothing), so a rule added to the file later is ignored.
        let again = h.reopen(LEGACY).await.unwrap();
        assert!(again.view().await.unwrap().is_empty());
        assert_eq!(h.pick(LIBERA, "#giles"), None);
    }

    #[tokio::test]
    async fn an_invalid_rule_stops_the_import_with_its_key_and_stores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("giles.db")).await.unwrap();
        let cases = [
            (
                "channel = \"irc\"\nagent = \"jeeves\"",
                "routes[1].channel: no channel instance `irc`",
            ),
            (
                "channel = \"a_b\"\nagent = \"jeeves\"",
                "routes[1].channel: invalid instance name",
            ),
            ("agent = \"wooster\"", "routes[1].agent: no agent `wooster`"),
            ("agent = \"a.b\"", "routes[1].agent: invalid agent name"),
            (
                "kind = \"dm\"\nagent = \"jeeves\"",
                "routes[1].kind: unknown kind",
            ),
            (
                "conversation = \"\"\nagent = \"jeeves\"",
                "routes[1].conversation: must not be empty",
            ),
            (
                "channel = \"\"\nagent = \"jeeves\"",
                "routes[1].channel: must not be empty",
            ),
            (
                "channel = \"  \"\nagent = \"jeeves\"",
                "routes[1].channel: must not be empty",
            ),
            (
                "kind = \"\"\nagent = \"jeeves\"",
                "routes[1].kind: must not be empty",
            ),
            (
                "kind = \" \"\nagent = \"jeeves\"",
                "routes[1].kind: must not be empty",
            ),
            ("channel = \"web\"", "routes[1]: missing field `agent`"),
            (
                "agent = \"jeeves\"\nuser = \"me\"",
                "routes[1]: unknown field `user`",
            ),
        ];
        for (rule, want) in cases {
            let config = LiveConfig::new(Arc::new(config(
                dir.path(),
                &format!("[[routes]]\nagent = \"jeeves\"\n[[routes]]\n{rule}\n"),
            )));
            let err = Routes::open(store.clone(), config).await.unwrap_err();
            let shown = err.to_string();
            assert!(
                shown.starts_with("giles.toml: ") && shown.contains(want),
                "{rule}: {shown}"
            );
            assert!(store.routes().await.unwrap().is_empty());
            assert!(!store.routes_imported().await.unwrap(), "{rule}");
        }
    }
}
