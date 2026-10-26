//! A fake Discord client for tests: the Gateway is a queue the test pushes events into, and the
//! calls the plugin makes are recorded. For this crate's tests and integration tests only.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use giles_plugin_sdk::async_trait;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::client::{Discord, GatewayEvent};

/// A fake Gateway connection. Share it as an `Arc`; every method takes `&self`.
pub struct FakeDiscord {
    events: Mutex<Option<mpsc::UnboundedSender<GatewayEvent>>>,
    queue: tokio::sync::Mutex<mpsc::UnboundedReceiver<GatewayEvent>>,
    polls: Mutex<Vec<Instant>>,
    polled: watch::Sender<usize>,
    shutdowns: AtomicUsize,
    hang_on_shutdown: AtomicBool,
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

    async fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        if self.hang_on_shutdown.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
}
