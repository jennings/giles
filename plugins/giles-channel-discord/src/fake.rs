//! A fake Discord client for tests: the Gateway is a queue the test pushes events into, and the
//! calls the plugin makes are recorded. For this crate's tests and integration tests only.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use giles_plugin_sdk::async_trait;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::client::{Discord, DiscordMessage, GatewayEvent, Page, RestError};

/// One call of [`Discord::messages_after`] the fake received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCall {
    /// The channel asked about.
    pub channel: u64,
    /// The message id the page had to follow.
    pub after: u64,
    /// The page size asked for.
    pub limit: u8,
}

/// A fake Gateway connection and a fake REST API. Share it as an `Arc`; every method takes `&self`.
///
/// The Gateway is a queue of events the test pushes. The REST API keeps a message history per
/// channel and answers [`messages_after`](Discord::messages_after) from it as Discord does
/// (the messages after the id, oldest first, up to the limit), after any errors the test queued.
pub struct FakeDiscord {
    events: Mutex<Option<mpsc::UnboundedSender<GatewayEvent>>>,
    queue: tokio::sync::Mutex<mpsc::UnboundedReceiver<GatewayEvent>>,
    polls: Mutex<Vec<Instant>>,
    polled: watch::Sender<usize>,
    shutdowns: AtomicUsize,
    hang_on_shutdown: AtomicBool,
    history: Mutex<HashMap<u64, Vec<DiscordMessage>>>,
    history_errors: Mutex<VecDeque<RestError>>,
    history_calls: Mutex<Vec<HistoryCall>>,
    history_called: watch::Sender<usize>,
    history_open: watch::Sender<bool>,
}

impl Default for FakeDiscord {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            events: Mutex::new(Some(tx)),
            queue: tokio::sync::Mutex::new(rx),
            polls: Mutex::default(),
            polled: watch::channel(0).0,
            shutdowns: AtomicUsize::new(0),
            hang_on_shutdown: AtomicBool::new(false),
            history: Mutex::default(),
            history_errors: Mutex::default(),
            history_calls: Mutex::default(),
            history_called: watch::channel(0).0,
            history_open: watch::channel(true).0,
        }
    }
}

impl FakeDiscord {
    /// Has the Gateway report `event` as soon as it is polled for one.
    ///
    /// # Panics
    ///
    /// After [`end`](Self::end).
    pub fn push(&self, event: GatewayEvent) {
        self.events
            .lock()
            .unwrap()
            .as_ref()
            .expect("the stream hasn't ended")
            .send(event)
            .expect("the client is held");
    }

    /// Ends the event stream after the events already pushed, as a client does after a close it
    /// won't recover from.
    pub fn end(&self) {
        self.events.lock().unwrap().take();
    }

    /// Makes [`shutdown`](Discord::shutdown) never finish, like a client stuck closing its socket.
    pub fn hang_on_shutdown(&self) {
        self.hang_on_shutdown.store(true, Ordering::SeqCst);
    }

    /// When [`next_event`](Discord::next_event) was called, in order.
    pub fn polls(&self) -> Vec<Instant> {
        self.polls.lock().unwrap().clone()
    }

    /// Waits until [`next_event`](Discord::next_event) has been called `n` times in all: the
    /// session has handled the events before it and is asking for the next.
    pub async fn polled(&self, n: usize) {
        let mut polled = self.polled.subscribe();
        polled
            .wait_for(|count| *count >= n)
            .await
            .expect("the sender is held");
    }

    /// How many times [`shutdown`](Discord::shutdown) was called.
    pub fn shutdowns(&self) -> usize {
        self.shutdowns.load(Ordering::SeqCst)
    }

    /// Adds `message` to the history of its channel, where [`messages_after`] finds it. Messages
    /// may be added in any order.
    ///
    /// [`messages_after`]: Discord::messages_after
    pub fn add_history(&self, message: DiscordMessage) {
        let mut history = self.history.lock().unwrap();
        let channel = history.entry(message.channel_id).or_default();
        channel.push(message);
        channel.sort_by_key(|message| message.id);
    }

    /// Makes the next [`messages_after`](Discord::messages_after) call that has no error before
    /// it fail with `error`. Queued errors are used in order, one per call.
    pub fn fail_history(&self, error: RestError) {
        self.history_errors.lock().unwrap().push_back(error);
    }

    /// Holds every [`messages_after`](Discord::messages_after) call (after it has been recorded)
    /// until [`resume_history`](Self::resume_history), like a slow Discord.
    pub fn pause_history(&self) {
        self.history_open.send_replace(false);
    }

    /// Lets the held [`messages_after`](Discord::messages_after) calls, and later ones, through.
    pub fn resume_history(&self) {
        self.history_open.send_replace(true);
    }

    /// The [`messages_after`](Discord::messages_after) calls so far, in order.
    pub fn history_calls(&self) -> Vec<HistoryCall> {
        self.history_calls.lock().unwrap().clone()
    }

    /// Waits until [`messages_after`](Discord::messages_after) has been called `n` times in all.
    pub async fn history_called(&self, n: usize) {
        let mut called = self.history_called.subscribe();
        called
            .wait_for(|count| *count >= n)
            .await
            .expect("the sender is held");
    }
}

#[async_trait]
impl Discord for FakeDiscord {
    async fn next_event(&self) -> Option<GatewayEvent> {
        let count = {
            let mut polls = self.polls.lock().unwrap();
            polls.push(Instant::now());
            polls.len()
        };
        self.polled.send_replace(count);
        self.queue.lock().await.recv().await
    }

    async fn messages_after(&self, channel: u64, after: u64, limit: u8) -> Result<Page, RestError> {
        let count = {
            let mut calls = self.history_calls.lock().unwrap();
            calls.push(HistoryCall {
                channel,
                after,
                limit,
            });
            calls.len()
        };
        self.history_called.send_replace(count);
        self.history_open
            .subscribe()
            .wait_for(|open| *open)
            .await
            .expect("the sender is held");
        if let Some(error) = self.history_errors.lock().unwrap().pop_front() {
            return Err(error);
        }
        let history = self.history.lock().unwrap();
        Ok(history
            .get(&channel)
            .map_or_else(Vec::new, |messages| {
                messages
                    .iter()
                    .filter(|message| message.id > after)
                    .take(usize::from(limit))
                    .cloned()
                    .collect()
            })
            .into())
    }

    async fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        if self.hang_on_shutdown.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
}
