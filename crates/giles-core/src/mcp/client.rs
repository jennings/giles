//! The `rmcp` client handler: what the core does with requests and notifications from a server.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use rmcp::ClientHandler;
use rmcp::model::{
    ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult, ElicitationCapability,
    ErrorData, FormElicitationCapability, Implementation, ProgressNotificationParam, ProgressToken,
    ProtocolVersion,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient};
use tokio::sync::{mpsc, watch};

use super::elicitation::CallRouter;

/// The protocol version the core asks for: the newest one that still has the `initialize`
/// handshake (PLAN section 8.6 requires 2025-06-18 or later). Servers may answer with an older
/// version they support.
pub(crate) const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::LATEST_WITH_INITIALIZE;

/// How many progress notifications for one call are buffered before newer ones are dropped.
const PROGRESS_BUFFER: usize = 64;

/// How many progress notifications for not-yet-subscribed tokens are kept (see [`ProgressRouter`]).
const EARLY_PROGRESS: usize = 32;

/// The client side of one MCP connection.
pub(crate) struct ClientState {
    pub(crate) instance: String,
    pub(crate) progress: ProgressRouter,
    /// The calls running, for the questions servers ask.
    pub(crate) calls: CallRouter,
    /// Bumped on every `notifications/tools/list_changed`.
    pub(crate) tools_changed: watch::Sender<u64>,
}

impl ClientState {
    pub(crate) fn new(instance: String) -> Self {
        Self {
            instance,
            progress: ProgressRouter::default(),
            calls: CallRouter::default(),
            tools_changed: watch::Sender::new(0),
        }
    }
}

impl ClientHandler for ClientState {
    /// The core declares form-mode elicitation (`elicitation: {form: {}}`): a server may ask the
    /// person whose turn it is a question mid-call. No roots, no sampling, no URL mode.
    fn get_info(&self) -> ClientConfig {
        let mut elicitation = ElicitationCapability::new();
        elicitation.form = Some(FormElicitationCapability::new());
        let mut capabilities = ClientCapabilities::default();
        capabilities.elicitation = Some(elicitation);
        ClientConfig::new(
            capabilities,
            Implementation::new("giles", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(PROTOCOL_VERSION)
    }

    /// Hands the question to the call that asked it, as an approval request
    /// ([`CallRouter::ask`]).
    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, ErrorData> {
        Ok(self.calls.ask(&self.instance, request, context).await)
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.progress.dispatch(params);
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        tracing::debug!(instance = %self.instance, "MCP tool list changed");
        self.tools_changed
            .send_modify(|generation| *generation += 1);
    }
}

/// Routes progress notifications to the call that owns the progress token.
///
/// A call learns its token only after `rmcp` has queued the request, so the first notifications can
/// arrive before the call subscribes. Notifications for unknown tokens are therefore kept in a
/// small ring ([`EARLY_PROGRESS`]) and handed over on subscription; stragglers for finished calls
/// age out of it.
#[derive(Clone, Default)]
pub(crate) struct ProgressRouter {
    routes: Arc<Mutex<Routes>>,
}

#[derive(Default)]
struct Routes {
    subscribers: HashMap<ProgressToken, mpsc::Sender<ProgressNotificationParam>>,
    early: VecDeque<ProgressNotificationParam>,
}

impl ProgressRouter {
    fn lock(&self) -> std::sync::MutexGuard<'_, Routes> {
        self.routes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Delivers a notification to its subscriber, or keeps it for a subscriber still to come.
    /// A subscriber that is behind by [`PROGRESS_BUFFER`] notifications misses this one.
    pub(crate) fn dispatch(&self, params: ProgressNotificationParam) {
        let mut routes = self.lock();
        match routes.subscribers.get(&params.progress_token) {
            Some(sender) => {
                let _ = sender.try_send(params);
            }
            None => {
                if routes.early.len() == EARLY_PROGRESS {
                    routes.early.pop_front();
                }
                routes.early.push_back(params);
            }
        }
    }

    /// Subscribes to one token's notifications, including any that arrived already. The
    /// subscription ends when the returned [`ProgressSubscription`] is dropped.
    pub(crate) fn subscribe(&self, token: ProgressToken) -> ProgressSubscription {
        let (sender, receiver) = mpsc::channel(PROGRESS_BUFFER);
        let mut routes = self.lock();
        let early = std::mem::take(&mut routes.early);
        for params in early {
            if params.progress_token == token {
                let _ = sender.try_send(params);
            } else {
                routes.early.push_back(params);
            }
        }
        routes.subscribers.insert(token.clone(), sender);
        ProgressSubscription {
            router: self.clone(),
            token,
            receiver,
        }
    }
}

/// One call's progress notifications; unsubscribes on drop.
pub(crate) struct ProgressSubscription {
    router: ProgressRouter,
    token: ProgressToken,
    pub(crate) receiver: mpsc::Receiver<ProgressNotificationParam>,
}

impl Drop for ProgressSubscription {
    fn drop(&mut self) {
        self.router.lock().subscribers.remove(&self.token);
    }
}

#[cfg(test)]
mod tests {
    use rmcp::model::NumberOrString;

    use super::*;

    fn token(n: i64) -> ProgressToken {
        ProgressToken(NumberOrString::Number(n))
    }

    fn progress(n: i64, value: f64) -> ProgressNotificationParam {
        ProgressNotificationParam::new(token(n), value)
    }

    #[test]
    fn routes_by_token() {
        let router = ProgressRouter::default();
        let mut one = router.subscribe(token(1));
        let mut two = router.subscribe(token(2));
        router.dispatch(progress(2, 0.5));
        router.dispatch(progress(1, 0.25));
        assert_eq!(one.receiver.try_recv().unwrap().progress, 0.25);
        assert_eq!(two.receiver.try_recv().unwrap().progress, 0.5);
        assert!(one.receiver.try_recv().is_err());
    }

    #[test]
    fn early_notifications_are_handed_to_the_late_subscriber() {
        let router = ProgressRouter::default();
        router.dispatch(progress(7, 1.0));
        router.dispatch(progress(8, 1.0));
        router.dispatch(progress(7, 2.0));
        let mut seven = router.subscribe(token(7));
        assert_eq!(seven.receiver.try_recv().unwrap().progress, 1.0);
        assert_eq!(seven.receiver.try_recv().unwrap().progress, 2.0);
        assert!(seven.receiver.try_recv().is_err());
        // Token 8's notification stays for its own subscriber.
        let mut eight = router.subscribe(token(8));
        assert_eq!(eight.receiver.try_recv().unwrap().progress, 1.0);
    }

    #[test]
    fn early_notifications_are_bounded() {
        let router = ProgressRouter::default();
        for n in 0..(EARLY_PROGRESS as i64 + 5) {
            router.dispatch(progress(n, 1.0));
        }
        assert_eq!(router.lock().early.len(), EARLY_PROGRESS);
        // The oldest were dropped.
        let mut first = router.subscribe(token(0));
        assert!(first.receiver.try_recv().is_err());
    }

    #[test]
    fn dropping_the_subscription_unsubscribes() {
        let router = ProgressRouter::default();
        let subscription = router.subscribe(token(1));
        drop(subscription);
        assert!(router.lock().subscribers.is_empty());
        // Later notifications for it are only kept as early ones, within the bound.
        router.dispatch(progress(1, 1.0));
        assert_eq!(router.lock().early.len(), 1);
    }

    #[test]
    fn a_full_subscriber_drops_newer_notifications() {
        let router = ProgressRouter::default();
        let mut subscription = router.subscribe(token(1));
        for step in 0..(PROGRESS_BUFFER + 3) {
            router.dispatch(progress(1, step as f64));
        }
        let mut received = 0;
        while subscription.receiver.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, PROGRESS_BUFFER);
    }
}
