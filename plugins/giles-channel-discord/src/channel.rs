//! The Discord channel: [`DiscordChannel`] implements the SDK's `Channel` on a session task.

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt as _};
use giles_plugin_sdk::giles_types::{
    Channel, ChannelEvent, ChannelInfo, EventId, MessageId, OutboundMessage, OutputMode, Result,
};
use giles_plugin_sdk::{Configure, async_trait};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::client::Discord;
use crate::config::Settings;
use crate::ingest::{Clock, Cursor, Ingest, Params};
use crate::outbound::Outbound;
use crate::real::RealDiscord;
use crate::session::{self, Outgoing, Session, Timing};
use crate::state::StateFile;
use crate::token::TokenState;

/// Events buffered for the SDK, which persists each before taking the next. When it's full, the
/// session waits.
const EVENT_BUFFER: usize = 64;

/// The identity namespace of every Discord instance: identities are user ids, which are global
/// across Discord, so every instance reports the same identity for the same person (PLAN
/// section 3.2).
pub const NAMESPACE: &str = "discord";

/// The plugin's `configure`: [`configure_with`] and the Gateway client on `twilight`.
///
/// # Errors
///
/// An invalid config, naming the key.
pub async fn configure(ctx: Configure, slot: &SessionSlot) -> Result<DiscordChannel> {
    configure_with(ctx, slot, Timing::default(), |settings| {
        Arc::new(RealDiscord::new(settings))
    })
    .await
}

/// The plugin's `configure` with the client supplied by `connect`: validates the config, builds the
/// client from the settings, and starts the session task in the background. Discord being
/// unreachable doesn't fail `Configure`; it shows as an error status until the connection is up.
///
/// The real plugin passes the twilight client, the tests a fake. `slot` is where `main` finds the
/// session again to stop it after the SDK returns.
///
/// # Errors
///
/// An invalid config, naming the key.
pub async fn configure_with(
    ctx: Configure,
    slot: &SessionSlot,
    timing: Timing,
    connect: impl FnOnce(&Settings) -> Arc<dyn Discord>,
) -> Result<DiscordChannel> {
    let settings = Settings::new(ctx.parse_config()?)?;
    tracing::info!(
        instance = ctx.instance(),
        max_blob = settings.max_blob,
        catch_up_max_age = ?settings.catch_up_max_age,
        "Discord channel configured"
    );
    let channel = DiscordChannel::start(
        connect(&settings),
        timing,
        Inbound {
            state: StateFile::open(ctx.data_dir()),
            catch_up_max_age: settings.catch_up_max_age,
            clock: Clock::system(),
        },
    );
    slot.set(channel.take_session());
    Ok(channel)
}

/// How long `main` waits for the session to close the connection after the SDK has returned: the
/// session's own bound for that, and a second for it to get there.
pub fn shutdown_wait() -> Duration {
    Timing::default().shutdown_wait + Duration::from_secs(1)
}

/// The session task of a channel, to stop and wait for after the SDK has returned.
///
/// The SDK owns the channel and gives no hook for its end, and a process that exits when `serve`
/// returns would cut the session off before it closes the Gateway (so the bot would stay shown as
/// online until Discord times it out). `main` keeps a [`SessionSlot`], which `configure` fills,
/// and awaits it.
pub struct SessionHandle {
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl SessionHandle {
    /// Stops the session and waits up to `wait` for it to close the connection.
    pub async fn shutdown(mut self, wait: Duration) {
        self.stop.cancel();
        let Some(task) = self.task.take() else {
            return;
        };
        if tokio::time::timeout(wait, task).await.is_err() {
            tracing::warn!("the Discord session didn't stop within {wait:?}");
        }
    }
}

/// Where `configure` leaves the [`SessionHandle`] for `main`. Cheap to clone.
#[derive(Clone, Default)]
pub struct SessionSlot(Arc<std::sync::Mutex<Option<SessionHandle>>>);

impl SessionSlot {
    fn set(&self, handle: SessionHandle) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    /// Stops the session `configure` started, if any, and waits for it up to `wait`.
    pub async fn shutdown(&self, wait: Duration) {
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

/// What the channel needs to receive messages besides the connection.
#[derive(Clone)]
pub struct Inbound {
    /// The plugin's state file: the DM channels and their cursors.
    pub state: StateFile,
    /// How far back catch-up reaches; zero turns it off.
    pub catch_up_max_age: Duration,
    /// The time, for the age limit.
    pub clock: Clock,
}

/// A Discord bot as a [`Channel`]. Dropping it stops the session task.
pub struct DiscordChannel {
    info: ChannelInfo,
    events: Arc<Mutex<mpsc::Receiver<Outgoing>>>,
    state: StateFile,
    stop: CancellationToken,
    token: Arc<TokenState>,
    outbound: Outbound,
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
    _guard: DropGuard,
}

impl DiscordChannel {
    /// Starts the session task: it polls `client`, delivers its messages, catches up after
    /// downtime, and reports its status. Must be called inside a Tokio runtime.
    pub fn start(client: Arc<dyn Discord>, timing: Timing, inbound: Inbound) -> Self {
        let (out, events) = mpsc::channel(EVENT_BUFFER);
        let stop = CancellationToken::new();
        let ingest = Ingest::new(Params {
            state: inbound.state.clone(),
            catch_up_max_age: inbound.catch_up_max_age,
            clock: inbound.clock,
            retry_min: timing.catch_up_retry_min,
            retry_max: timing.catch_up_retry_max,
        });
        let token = Arc::new(TokenState::default());
        let outbound = Outbound::new(
            Arc::clone(&client),
            Arc::clone(&token),
            inbound.state.clone(),
        );
        let task = tokio::spawn(session::run(Session {
            client,
            timing,
            out,
            stop: stop.clone(),
            ingest,
            token: Arc::clone(&token),
        }));
        Self {
            info: ChannelInfo {
                identity_namespace: NAMESPACE.to_owned(),
                output: OutputMode::Blocks,
                can_initiate: true,
            },
            events: Arc::new(Mutex::new(events)),
            state: inbound.state,
            stop: stop.clone(),
            token,
            outbound,
            task: std::sync::Mutex::new(Some(task)),
            _guard: stop.drop_guard(),
        }
    }

    /// Whether Discord has rejected the bot token, and the way to say it has.
    pub fn token(&self) -> &Arc<TokenState> {
        &self.token
    }

    /// The session task, to be stopped and awaited by whoever outlives the channel. Only the first
    /// call gets the task.
    pub fn take_session(&self) -> SessionHandle {
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
}

#[async_trait]
impl Channel for DiscordChannel {
    fn info(&self) -> &ChannelInfo {
        &self.info
    }

    async fn events(&self) -> Result<BoxStream<'static, Result<ChannelEvent>>> {
        // The cursor of a channel moves when the SDK asks for the event after a message, never
        // earlier: the SDK persists each event before polling the stream again, so a poll is the
        // proof that the event handed out last is on disk (PLAN section 8.5). An event the SDK
        // took but didn't persist (it failed, or the process ended) leaves the cursor behind it,
        // and catch-up fetches it again after a restart.
        let events = Arc::clone(&self.events);
        let state = self.state.clone();
        Ok(stream::unfold(
            (events, state, None::<Cursor>),
            |(events, state, persisted)| async move {
                if let Some(cursor) = persisted {
                    state.advance(cursor.channel, cursor.message);
                }
                loop {
                    let item = events.lock().await.recv().await?;
                    match item.event {
                        Some(event) => return Some((Ok(event), (events, state, item.cursor))),
                        // Done with, not delivered: everything before it has been persisted,
                        // since the stream was polled to get here.
                        None => {
                            if let Some(cursor) = item.cursor {
                                state.advance(cursor.channel, cursor.message);
                            }
                        }
                    }
                }
            },
        )
        .boxed())
    }

    async fn ack(&self, _event: &EventId) -> Result<()> {
        // The SDK keeps unacked events and sends them again; nothing here depends on an ack.
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<MessageId> {
        self.outbound.send(msg).await
    }
}
