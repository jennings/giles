//! The WhatsApp channel: [`WhatsApp`] implements the SDK's `Channel` on a session task.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Channel, ChannelEvent, ChannelInfo, Error, EventId, MessageId, OutboundMessage, OutputMode,
    Result, SendTarget,
};
use giles_plugin_sdk::{Configure, async_trait};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::backend::{Backend, Connector};
use crate::config::Settings;
use crate::datadir::DataDir;
use crate::media::MediaStore;
use crate::outbound;
use crate::real::RealConnector;
use crate::session::{self, Outgoing, Session, Timing};

/// Events buffered for the SDK, which persists each before taking the next. When it's full, the
/// session waits, and so does the acknowledgement of inbound messages.
const EVENT_BUFFER: usize = 64;

/// The identity namespace of every WhatsApp instance: identities are phone numbers, shared by all
/// instances (PLAN section 3.2).
pub(crate) const NAMESPACE: &str = "whatsapp";

/// The plugin's `configure`: validates the config, locks the data directory, and starts the
/// WhatsApp client in the background (WhatsApp being unreachable doesn't fail `Configure`; it
/// shows as an error status until the connection is back).
pub(crate) async fn configure(ctx: Configure, slot: &SessionSlot) -> Result<WhatsApp> {
    let settings = Arc::new(Settings::new(ctx.parse_config()?)?);
    let data = DataDir::open(ctx.data_dir())?;
    if data.clear_logged_out_session().map_err(Error::other)? {
        tracing::info!("the last session was logged out; linking a new device");
    }
    tracing::info!(
        instance = ctx.instance(),
        link = settings.link.mode(),
        "WhatsApp channel configured"
    );
    let connector = RealConnector::new(Arc::clone(&settings), data.store());
    let channel = WhatsApp::start(
        settings,
        Arc::new(connector),
        Arc::new(ctx.blobs().clone()),
        data,
        Timing::default(),
    );
    slot.set(channel.take_session());
    Ok(channel)
}

/// How long `main` waits for the session to shut the client down after the SDK has returned: the
/// session's own bound for that, and a second for it to get there.
pub(crate) fn shutdown_wait() -> Duration {
    Timing::default().shutdown_wait + Duration::from_secs(1)
}

/// The session task of a channel, to stop and wait for after the SDK has returned.
///
/// The SDK owns the channel and gives no hook for its end, and a process that exits when `serve`
/// returns would cut the session off before it disconnects the client (flushing the session
/// store). `main` keeps a [`SessionSlot`], which `configure` fills, and awaits it.
pub(crate) struct SessionHandle {
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl SessionHandle {
    /// Stops the session and waits up to `wait` for it to shut the client down.
    pub(crate) async fn shutdown(mut self, wait: Duration) {
        self.stop.cancel();
        let Some(task) = self.task.take() else {
            return;
        };
        if tokio::time::timeout(wait, task).await.is_err() {
            tracing::warn!("the WhatsApp session didn't stop within {wait:?}");
        }
    }
}

/// Where `configure` leaves the [`SessionHandle`] for `main`. Cheap to clone.
#[derive(Clone, Default)]
pub(crate) struct SessionSlot(Arc<std::sync::Mutex<Option<SessionHandle>>>);

impl SessionSlot {
    fn set(&self, handle: SessionHandle) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    /// Stops the session `configure` started, if any, and waits for it up to `wait`.
    pub(crate) async fn shutdown(&self, wait: Duration) {
        let handle = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            handle.shutdown(wait).await;
        }
    }
}

/// A linked WhatsApp device as a [`Channel`]. Dropping it stops the session task.
pub(crate) struct WhatsApp {
    info: ChannelInfo,
    settings: Arc<Settings>,
    store: Arc<dyn MediaStore>,
    send_wait: Duration,
    backend: watch::Receiver<Option<Arc<dyn Backend>>>,
    events: Arc<Mutex<mpsc::Receiver<Outgoing>>>,
    stop: CancellationToken,
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
    _guard: DropGuard,
}

impl WhatsApp {
    /// Starts the session task: it connects with `connector` and keeps events flowing. Must be
    /// called inside a Tokio runtime. `data` stays locked as long as the channel lives.
    pub(crate) fn start(
        settings: Arc<Settings>,
        connector: Arc<dyn Connector>,
        store: Arc<dyn MediaStore>,
        data: DataDir,
        timing: Timing,
    ) -> Self {
        let (out, events) = mpsc::channel(EVENT_BUFFER);
        let (backend_tx, backend) = watch::channel(None);
        let stop = CancellationToken::new();
        let send_wait = timing.send_wait;
        let task = tokio::spawn(session::run(Session {
            connector,
            store: Arc::clone(&store),
            settings: Arc::clone(&settings),
            data: Arc::new(data),
            timing,
            out,
            backend: backend_tx,
            stop: stop.clone(),
        }));
        Self {
            info: ChannelInfo {
                identity_namespace: NAMESPACE.to_owned(),
                output: OutputMode::Blocks,
                can_initiate: true,
            },
            settings,
            store,
            send_wait,
            backend,
            events: Arc::new(Mutex::new(events)),
            stop: stop.clone(),
            task: std::sync::Mutex::new(Some(task)),
            _guard: stop.drop_guard(),
        }
    }

    /// The session task, to be stopped and awaited by whoever outlives the channel. Only the
    /// first call gets the task.
    pub(crate) fn take_session(&self) -> SessionHandle {
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        SessionHandle {
            stop: self.stop.clone(),
            task,
        }
    }

    /// The connected backend, waiting up to `wait` for a dropped connection to come back.
    async fn connected(&self, wait: Duration) -> Result<Arc<dyn Backend>> {
        let mut backend = self.backend.clone();
        let ready = tokio::time::timeout(wait, backend.wait_for(Option::is_some)).await;
        match ready {
            Ok(Ok(backend)) => backend
                .clone()
                .ok_or_else(|| Error::unavailable("not connected to WhatsApp")),
            _ => Err(Error::unavailable("not connected to WhatsApp")),
        }
    }
}

#[async_trait]
impl Channel for WhatsApp {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        // The SDK persists each event before it polls for the next, so the poll after an event
        // is the news that it's on disk: that fires the event's `persisted` signal. A stream
        // dropped before then drops the signal, which counts as not persisted.
        let events = Arc::clone(&self.events);
        let pending: Option<oneshot::Sender<()>> = None;
        Ok(
            stream::unfold((events, pending), |(events, pending)| async move {
                if let Some(persisted) = pending {
                    // The receiver is gone when the session stopped waiting.
                    let _ = persisted.send(());
                }
                let Outgoing { event, persisted } = events.lock().await.recv().await?;
                Some((Ok(event), (events, persisted)))
            })
            .boxed(),
        )
    }

    async fn ack(&self, _event: &EventId) -> Result<()> {
        // The SDK keeps unacked events; WhatsApp was acknowledged when the event was persisted.
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        // A reply into a conversation waits out a dropped connection; a proactive send returns
        // at once, since the outbox retries it.
        let wait = match msg.target {
            SendTarget::Conversation(_) => self.send_wait,
            _ => Duration::ZERO,
        };
        let backend = self.connected(wait).await?;
        outbound::send(msg, &*backend, &*self.store, self.settings.max_blob).await
    }
}

#[cfg(test)]
mod tests;
