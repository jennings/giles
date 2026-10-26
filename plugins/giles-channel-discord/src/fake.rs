//! A fake Discord client for tests: the Gateway is a queue the test pushes events into, and the
//! calls the plugin makes are recorded. For this crate's tests and integration tests only.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use giles_plugin_sdk::async_trait;
use giles_plugin_sdk::giles_types::{Error, MediaId, MediaRef, Result};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::client::{
    Author, Body, Discord, DiscordMessage, GatewayEvent, NewMessage, Page, RestError,
};
use crate::media::MediaStore;
use crate::snowflake;

/// A plain direct message from a person (not a bot) with `content`, as Discord delivers one: the
/// user `author` writing in the DM channel `channel`. Its time is the one its id encodes. Change
/// the fields for other kinds of message.
pub fn direct_message(id: u64, channel: u64, author: u64, content: &str) -> DiscordMessage {
    DiscordMessage {
        id,
        channel_id: channel,
        author: Author {
            id: author,
            bot: false,
            username: format!("user{author}"),
            global_name: None,
        },
        guild: false,
        webhook: false,
        kind: 0,
        content: content.to_owned(),
        attachments: Vec::new(),
        stickers: Vec::new(),
        timestamp_ms: i64::try_from(snowflake::unix_ms(id)).unwrap_or(i64::MAX),
        reply_to: None,
    }
}

/// One call of [`Discord::messages_after`] the fake received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCall {
    /// The channel asked about.
    pub channel: u64,
    /// The message id the page had to follow.
    pub after: u64,
    /// The page size asked for.
    pub limit: u8,
    /// When it was made (Tokio time, which a test can pause).
    pub at: Instant,
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
    downloads_open: watch::Sender<bool>,
    rest: Mutex<Rest>,
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
            downloads_open: watch::channel(true).0,
            rest: Mutex::default(),
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

    /// Holds every [`download`](Discord::download) call (after it has been recorded) until
    /// [`release_downloads`](Self::release_downloads), like a slow CDN.
    pub fn hold_downloads(&self) {
        self.downloads_open.send_replace(false);
    }

    /// Lets the held downloads, and later ones, through.
    pub fn release_downloads(&self) {
        self.downloads_open.send_replace(true);
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

/// What the fake's send calls were asked and what they will answer.
#[derive(Default)]
struct Rest {
    /// Answers for the next `create_message` calls, oldest first. When empty, a call succeeds
    /// with the next message id.
    message_script: VecDeque<Result<u64, RestError>>,
    /// Answers for the next `create_dm` calls. When empty, a call succeeds with a channel id
    /// derived from the user's.
    dm_script: VecDeque<Result<u64, RestError>>,
    /// Every `create_message` call, failed or not: the channel and the message.
    messages: Vec<(u64, NewMessage)>,
    /// Every `create_dm` call: the user.
    dms: Vec<u64>,
    /// The id the next unscripted message gets.
    next_message: u64,
    /// How long each `create_message` takes to answer, like a slow network.
    delay: Duration,
    /// What each URL of the fake CDN answers.
    served: HashMap<String, Served>,
    /// Every `download` call: the URL and the limit.
    downloads: Vec<(String, u64)>,
}

/// What a URL of the fake CDN answers to [`Discord::download`].
#[derive(Clone)]
pub enum Served {
    /// A file, as the chunks of its body. `declared` is the length the fake CDN announces up
    /// front, when it announces one.
    File {
        /// The body, in the chunks it arrives in.
        chunks: Vec<Bytes>,
        /// The `Content-Length`.
        declared: Option<u64>,
    },
    /// A body that never ends: chunks of `chunk` for as long as they are read. `pulled` counts the
    /// chunks the plugin has read, so a test can tell how much of it was ever taken.
    Endless {
        /// One chunk of the body.
        chunk: Bytes,
        /// How many chunks have been read.
        pulled: Arc<AtomicUsize>,
        /// The `Content-Length`, when the CDN announces one.
        declared: Option<u64>,
    },
    /// A body that breaks off after `chunks`: the connection drops.
    BreaksOff {
        /// What arrives before it does.
        chunks: Vec<Bytes>,
    },
    /// The download call itself fails: the CDN's status (an expired link is 403, a deleted file
    /// 404), or a transport error.
    Refused(RestError),
}

impl Served {
    /// A whole file in one chunk, with its length announced.
    pub fn file(bytes: &[u8]) -> Self {
        Self::File {
            chunks: vec![Bytes::copy_from_slice(bytes)],
            declared: Some(bytes.len() as u64),
        }
    }

    /// A refusal with the CDN's `status`.
    pub fn status(status: u16) -> Self {
        Self::Refused(RestError::Api { status, code: None })
    }
}

/// The DM channel the fake gives `user` when [`FakeDiscord::script_dms`] has no answer queued.
pub const fn dm_channel_of(user: u64) -> u64 {
    user + 1_000
}

impl FakeDiscord {
    /// Queues the answers of the next `create_message` calls, in order. Calls after the queue is
    /// empty succeed with a fresh message id.
    pub fn script_messages(&self, answers: impl IntoIterator<Item = Result<u64, RestError>>) {
        self.rest.lock().unwrap().message_script.extend(answers);
    }

    /// Queues the answers of the next `create_dm` calls, in order. Calls after the queue is empty
    /// succeed with [`dm_channel_of`] the user.
    pub fn script_dms(&self, answers: impl IntoIterator<Item = Result<u64, RestError>>) {
        self.rest.lock().unwrap().dm_script.extend(answers);
    }

    /// Makes every later `create_message` take `delay` (of tokio's time) to answer. The call is
    /// recorded when it starts.
    pub fn delay_rest(&self, delay: Duration) {
        self.rest.lock().unwrap().delay = delay;
    }

    /// Every `create_message` call so far, in order, failed ones included: the channel id and the
    /// text.
    pub fn messages(&self) -> Vec<(u64, String)> {
        let rest = self.rest.lock().unwrap();
        rest.messages
            .iter()
            .map(|(channel, message)| (*channel, message.content.clone()))
            .collect()
    }

    /// Every `create_dm` call so far: the user ids.
    pub fn dms(&self) -> Vec<u64> {
        self.rest.lock().unwrap().dms.clone()
    }

    /// Makes `url` answer `served` when it is downloaded. Any other URL is a 404.
    pub fn serve(&self, url: &str, served: Served) {
        self.rest
            .lock()
            .unwrap()
            .served
            .insert(url.to_owned(), served);
    }

    /// Every `download` call so far, in order: the URL and the limit it was given.
    pub fn downloads(&self) -> Vec<(String, u64)> {
        self.rest.lock().unwrap().downloads.clone()
    }
}

/// A blob store in memory: what [`MediaStore`] is in the SDK, for tests.
#[derive(Default)]
pub struct FakeMedia {
    blobs: Mutex<HashMap<MediaId, (Vec<u8>, MediaRef)>>,
    next: AtomicUsize,
    puts: AtomicUsize,
    largest_read: AtomicUsize,
    /// Makes every put fail, like a full disk.
    pub fail_puts: AtomicBool,
}

impl FakeMedia {
    /// The bytes of the blob `id`.
    pub fn get(&self, id: &MediaId) -> Option<Vec<u8>> {
        self.blobs
            .lock()
            .unwrap()
            .get(id)
            .map(|(bytes, _)| bytes.clone())
    }

    /// How many blobs are stored.
    pub fn len(&self) -> usize {
        self.blobs.lock().unwrap().len()
    }

    /// Whether no blob is stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many times a reader was put, whether it worked or not.
    pub fn puts(&self) -> usize {
        self.puts.load(Ordering::SeqCst)
    }

    /// The most bytes one read of a put reader returned: what the store held at a time.
    pub fn largest_read(&self) -> usize {
        self.largest_read.load(Ordering::SeqCst)
    }

    fn store(&self, bytes: Vec<u8>, media_type: String, name: Option<String>) -> MediaRef {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let media = MediaRef {
            id: MediaId::new(format!("{n:064x}")).expect("64 hex digits"),
            media_type,
            size: bytes.len() as u64,
            name,
        };
        self.blobs
            .lock()
            .unwrap()
            .insert(media.id.clone(), (bytes, media.clone()));
        media
    }
}

#[async_trait]
impl MediaStore for FakeMedia {
    async fn put_reader(
        &self,
        mut reader: Box<dyn AsyncRead + Send + Unpin>,
        media_type: String,
        name: Option<String>,
    ) -> Result<MediaRef> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.fail_puts.load(Ordering::SeqCst) {
            return Err(Error::other("no space left on device"));
        }
        let mut bytes = Vec::new();
        let mut buf = vec![0; 8 * 1024];
        loop {
            let len = reader.read(&mut buf).await.map_err(Error::other)?;
            if len == 0 {
                break;
            }
            self.largest_read.fetch_max(len, Ordering::SeqCst);
            bytes.extend_from_slice(&buf[..len]);
        }
        Ok(self.store(bytes, media_type, name))
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
                at: Instant::now(),
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

    async fn create_message(&self, channel: u64, message: NewMessage) -> Result<u64, RestError> {
        let (answer, delay) = {
            let mut rest = self.rest.lock().unwrap();
            rest.messages.push((channel, message));
            let answer = rest.message_script.pop_front().unwrap_or_else(|| {
                rest.next_message += 1;
                Ok(5_000 + rest.next_message)
            });
            (answer, rest.delay)
        };
        tokio::time::sleep(delay).await;
        answer
    }

    async fn create_dm(&self, user: u64) -> Result<u64, RestError> {
        let mut rest = self.rest.lock().unwrap();
        rest.dms.push(user);
        rest.dm_script
            .pop_front()
            .unwrap_or_else(|| Ok(dm_channel_of(user)))
    }

    async fn download(&self, url: &str, max: u64) -> Result<Body, RestError> {
        let served = {
            let mut rest = self.rest.lock().unwrap();
            rest.downloads.push((url.to_owned(), max));
            rest.served.get(url).cloned()
        };
        // A held download waits here, after it has been recorded.
        self.downloads_open
            .subscribe()
            .wait_for(|open| *open)
            .await
            .expect("the sender is held");
        let declared_too_large = |declared: &Option<u64>| declared.is_some_and(|d| d > max);
        match served {
            None => Err(RestError::Api {
                status: 404,
                code: None,
            }),
            Some(Served::Refused(error)) => Err(error),
            Some(Served::File { declared, .. } | Served::Endless { declared, .. })
                if declared_too_large(&declared) =>
            {
                Err(RestError::TooLarge)
            }
            Some(Served::File { chunks, .. }) => {
                Ok(stream::iter(chunks.into_iter().map(Ok)).boxed())
            }
            Some(Served::Endless { chunk, pulled, .. }) => Ok(stream::repeat_with(move || {
                pulled.fetch_add(1, Ordering::SeqCst);
                Ok(chunk.clone())
            })
            .boxed()),
            Some(Served::BreaksOff { chunks }) => Ok(stream::iter(
                chunks
                    .into_iter()
                    .map(Ok)
                    .chain(std::iter::once(Err(RestError::Transport))),
            )
            .boxed()),
        }
    }

    async fn shutdown(&self) {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        if self.hang_on_shutdown.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
}
