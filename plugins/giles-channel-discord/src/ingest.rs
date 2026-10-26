//! Messages in: live from the Gateway and fetched over REST after downtime, merged into one
//! ordered, deduplicated stream of things for the SDK (`Docs/technical/discord.md`, "Catch-Up").
//!
//! [`Ingest`] is a state machine with no I/O and no clock of its own: the session feeds it Gateway
//! events and the answers to the history requests it asks for, and gets back what to hand to the
//! SDK ([`Out`]). That keeps the algorithm testable move by move.
//!
//! Per DM channel there is a high-water mark, the highest message id handled, and a mode:
//!
//! - **Live**: a message above the mark is handled and moves it; one at or below it is a duplicate
//!   (a replay after a resume, or a message the catch-up already fetched) and is ignored.
//! - **Catching up**: live messages wait in a buffer (at most [`BUFFER_CAP`]) while a pass pages
//!   through the channel's history after the mark, or after the age limit if that is later. When
//!   the pass is over the buffer is merged: what is above the mark is handled, the rest was
//!   fetched already.
//!
//! A pass runs for every known channel after each `Ready` (a new session, not a resumed one), and
//! for a channel seen for the first time before its first message is delivered. Passes are one
//! page request at a time, one channel at a time; a failed request is tried again later with a
//! growing delay, and the channel's other passes don't wait for it.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use giles_plugin_sdk::giles_types::{ChannelEvent, ChannelStatus};
use tokio::time::Instant;

use crate::client::{DiscordMessage, Page, RestError};
use crate::inbound::{self, Mapped};
use crate::snowflake;
use crate::state::StateFile;
use crate::status;

/// Messages per history request: Discord's maximum.
pub const PAGE: u8 = 100;
/// Pages per channel and pass. A pass that fills all of them stops and loses what is beyond.
pub const MAX_PAGES: u32 = 100;
/// Live messages kept for a channel while it catches up. Beyond this they are discarded, and the
/// channel gets another pass when this one is over, which fetches them.
pub const BUFFER_CAP: usize = 1000;
/// How many messages with nothing in them, in a row, make the status an error.
pub const EMPTY_RUN: u32 = 3;

/// The time, as milliseconds since the Unix epoch. Injected so tests can set it.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl Clock {
    /// The system clock.
    pub fn system() -> Self {
        Self::new(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| {
                    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
                })
        })
    }

    /// A clock that asks `now`.
    pub fn new(now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    fn now_ms(&self) -> u64 {
        (self.0)()
    }
}

/// What [`Ingest`] works with.
#[derive(Clone)]
pub struct Params {
    /// The plugin's state file: the channels, and where each one's cursor is.
    pub state: StateFile,
    /// How far back catch-up reaches. Zero turns it off.
    pub catch_up_max_age: Duration,
    /// The time.
    pub clock: Clock,
    /// The first delay before a failed history request is tried again.
    pub retry_min: Duration,
    /// The longest delay between tries.
    pub retry_max: Duration,
}

/// A message id in a channel: where the SDK's persisted position will be once it has taken the
/// message that carries this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// The DM channel.
    pub channel: u64,
    /// The message id.
    pub message: u64,
}

/// What to do with the result of handling a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Hand this event to the SDK. Once the SDK has taken the event after it, which it does after
    /// writing this one to disk, the cursor moves.
    Deliver {
        /// The event.
        event: Box<ChannelEvent>,
        /// Where the cursor goes when the SDK has persisted the event.
        cursor: Cursor,
    },
    /// A message that is not delivered but is done with (a bot's, an empty one): the cursor
    /// moves once everything before it has been persisted.
    Advance(Cursor),
    /// Report this status.
    Status(ChannelStatus),
}

/// One history request to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fetch {
    /// The channel.
    pub channel: u64,
    /// The message id the page must follow.
    pub after: u64,
    /// The page size.
    pub limit: u8,
}

/// What the session should do about history next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Nothing to fetch.
    Idle,
    /// A request is waiting for its retry delay: look again at this time.
    Wait(Instant),
    /// Make this request.
    Now(Fetch),
}

/// One channel's catch-up.
#[derive(Debug)]
struct Job {
    channel: u64,
    /// Fetch the page after this message.
    after: u64,
    pages: u32,
    /// Requests that failed in a row.
    failures: u32,
    /// Not before this time, after a failure.
    not_before: Option<Instant>,
    handled: usize,
    too_old: usize,
}

enum Mode {
    Live,
    /// A pass is queued or running: live messages wait here.
    CatchingUp {
        buffer: VecDeque<DiscordMessage>,
        /// Live messages were discarded because the buffer was full.
        overflowed: bool,
    },
}

struct Channel {
    mode: Mode,
    /// The highest message id handled.
    mark: u64,
}

/// See the [module](self).
pub struct Ingest {
    params: Params,
    /// The plugin's own user id, once the Gateway has said it.
    bot: Option<u64>,
    channels: HashMap<u64, Channel>,
    queue: VecDeque<Job>,
    /// Messages in a row from a person with nothing in them. At [`EMPTY_RUN`] the status says
    /// Discord isn't sending message content, for as long as the run lasts.
    empty_run: u32,
    /// Whether Discord rejected the token: nothing more is requested.
    rejected: bool,
}

impl Ingest {
    /// An ingest of the channels in `params.state`, all live: no pass runs until [`on_ready`].
    ///
    /// [`on_ready`]: Self::on_ready
    pub fn new(params: Params) -> Self {
        let channels = params
            .state
            .channels()
            .into_iter()
            .map(|channel| (channel, Self::known(&params.state, channel)))
            .collect();
        Self {
            params,
            bot: None,
            channels,
            queue: VecDeque::new(),
            empty_run: 0,
            rejected: false,
        }
    }

    fn known(state: &StateFile, channel: u64) -> Channel {
        Channel {
            mode: Mode::Live,
            mark: state.get(channel).and_then(|dm| dm.cursor).unwrap_or(0),
        }
    }

    /// Whether passes run: catch-up is on, and Discord hasn't rejected the token.
    fn catch_up_enabled(&self) -> bool {
        !self.params.catch_up_max_age.is_zero() && !self.rejected
    }

    /// The first message id a pass may fetch after: the mark, or the first id of the age limit,
    /// whichever is later.
    fn start_after(&self, mark: u64) -> u64 {
        let max_age = u64::try_from(self.params.catch_up_max_age.as_millis()).unwrap_or(u64::MAX);
        let oldest = self.params.clock.now_ms().saturating_sub(max_age);
        mark.max(snowflake::at_unix_ms(oldest))
    }

    fn job(&self, channel: u64, mark: u64) -> Job {
        Job {
            channel,
            after: self.start_after(mark),
            pages: 0,
            failures: 0,
            not_before: None,
            handled: 0,
            too_old: 0,
        }
    }

    /// A new Gateway session is up (not a resumed one: that replays what was missed): every known
    /// channel is caught up again, since messages sent while there was no session were not
    /// replayed. `bot` is the plugin's own user id.
    pub fn on_ready(&mut self, bot: u64) {
        self.bot = Some(bot);
        // Passes of the earlier session start over, from where the marks are now.
        self.queue.clear();
        if !self.catch_up_enabled() {
            return;
        }
        // Channels the sends added to the state since the last look are as known as the rest.
        let known: BTreeSet<u64> = self
            .channels
            .keys()
            .copied()
            .chain(self.params.state.channels())
            .collect();
        for channel in known {
            let state = &self.params.state;
            let entry = self
                .channels
                .entry(channel)
                .or_insert_with(|| Self::known(state, channel));
            entry.mode = match std::mem::replace(&mut entry.mode, Mode::Live) {
                mode @ Mode::CatchingUp { .. } => mode,
                Mode::Live => Mode::CatchingUp {
                    buffer: VecDeque::new(),
                    overflowed: false,
                },
            };
            let mark = entry.mark;
            let job = self.job(channel, mark);
            self.queue.push_back(job);
        }
    }

    /// A message arrived live.
    pub fn on_message(&mut self, message: DiscordMessage) -> Vec<Out> {
        let mut outs = Vec::new();
        let channel = message.channel_id;
        if !self.channels.contains_key(&channel) {
            // A channel is learned from a person in it: a bot's or a server's message says
            // nothing about whose DM this is.
            if let Mapped::Dropped(reason) = inbound::map(&message, self.bot)
                && !reason.is_from_a_person()
            {
                tracing::debug!("ignored a message of an unknown channel: {reason:?}");
                return outs;
            }
            self.learn(channel, message.author.id);
        }
        let Some(entry) = self.channels.get_mut(&channel) else {
            return outs;
        };
        match &mut entry.mode {
            Mode::CatchingUp { buffer, overflowed } => {
                if buffer.len() < BUFFER_CAP {
                    buffer.push_back(message);
                } else {
                    *overflowed = true;
                }
            }
            Mode::Live => self.handle(message, &mut outs),
        }
        outs
    }

    /// Adds a channel seen for the first time, with `user` in it. Its history is fetched before
    /// its first message is delivered.
    fn learn(&mut self, channel: u64, user: u64) {
        self.params.state.record_dm(channel, user);
        let mut entry = Self::known(&self.params.state, channel);
        if self.catch_up_enabled() {
            entry.mode = Mode::CatchingUp {
                buffer: VecDeque::new(),
                overflowed: false,
            };
            let job = self.job(channel, entry.mark);
            self.queue.push_back(job);
        }
        self.channels.insert(channel, entry);
    }

    /// Handles a message of a channel that is live (or being merged): drops it as a duplicate, or
    /// maps it, moves the mark, and says what to hand over.
    fn handle(&mut self, message: DiscordMessage, outs: &mut Vec<Out>) {
        let Some(entry) = self.channels.get_mut(&message.channel_id) else {
            return;
        };
        if message.id <= entry.mark {
            tracing::debug!("ignored a message already handled");
            return;
        }
        entry.mark = message.id;
        let cursor = Cursor {
            channel: message.channel_id,
            message: message.id,
        };
        match inbound::map(&message, self.bot) {
            Mapped::Message(inbound) => {
                let alarmed = std::mem::take(&mut self.empty_run) >= EMPTY_RUN;
                outs.push(Out::Deliver {
                    event: Box::new(ChannelEvent::Message {
                        id: inbound::event_id(message.id),
                        message: *inbound,
                    }),
                    cursor,
                });
                if alarmed {
                    outs.push(Out::Status(ChannelStatus::Connected));
                }
            }
            Mapped::Empty => {
                tracing::debug!("a direct message with no text, file, or sticker");
                self.empty_run += 1;
                outs.push(Out::Advance(cursor));
                // The status is repeated while the run lasts: the session sends a status only
                // when it changes, so a `Connected` from a reconnect in between is put right.
                if self.empty_run >= EMPTY_RUN {
                    outs.push(Out::Status(status::missing_content()));
                }
            }
            Mapped::Dropped(reason) => {
                tracing::debug!("dropped a message: {reason:?}");
                outs.push(Out::Advance(cursor));
            }
        }
    }

    /// What to do about history now: the first queued request that isn't waiting out a retry
    /// delay, else when the first of them stops waiting. Call it again after every result and
    /// every change.
    pub fn next(&self, now: Instant) -> Next {
        if self.rejected {
            return Next::Idle;
        }
        let mut wait: Option<Instant> = None;
        for job in &self.queue {
            match job.not_before {
                Some(at) if at > now => wait = Some(wait.map_or(at, |earliest| earliest.min(at))),
                _ => {
                    return Next::Now(Fetch {
                        channel: job.channel,
                        after: job.after,
                        limit: PAGE,
                    });
                }
            }
        }
        wait.map_or(Next::Idle, Next::Wait)
    }

    /// The answer to `fetch`. One that doesn't belong to a queued request (the session started
    /// over since) is ignored.
    pub fn on_page(
        &mut self,
        fetch: Fetch,
        result: Result<Page, RestError>,
        now: Instant,
    ) -> Vec<Out> {
        let mut outs = Vec::new();
        let Some(at) = self
            .queue
            .iter()
            .position(|job| job.channel == fetch.channel && job.after == fetch.after)
        else {
            return outs;
        };
        match result {
            Ok(page) => self.page(at, fetch, page, &mut outs),
            Err(RestError::Unauthorized) => {
                tracing::warn!("Discord rejected the bot token during catch-up");
                self.rejected = true;
                outs.push(Out::Status(status::rejected_token()));
                self.stop_fetching(&mut outs);
            }
            Err(RestError::Api { status, code })
                if (400..500).contains(&status) && status != 429 =>
            {
                // Asking again wouldn't help (a channel that is gone, one the bot can't see).
                tracing::warn!(
                    "catch-up of a channel failed for good ({status}, code {code:?}); skipping it"
                );
                self.finish(at, &mut outs);
            }
            Err(error) => {
                let job = &mut self.queue[at];
                job.failures += 1;
                let delay = self
                    .params
                    .retry_min
                    .saturating_mul(1_u32 << (job.failures - 1).min(20))
                    .min(self.params.retry_max);
                job.not_before = Some(now + delay);
                if job.failures == 1 {
                    tracing::warn!(
                        "catch-up of a channel failed ({error}); trying again in {delay:?}"
                    );
                } else {
                    tracing::debug!("catch-up failed again ({error}); trying again in {delay:?}");
                }
            }
        }
        outs
    }

    /// A page of channel history for the job at `at`.
    fn page(&mut self, at: usize, fetch: Fetch, page: Page, outs: &mut Vec<Out>) {
        // By what Discord sent, not by what could be read: a full page with a message the models
        // rejected is still full, and the next page starts after it.
        let full = page.received >= usize::from(fetch.limit);
        let oldest = self.start_after(0);
        let mut newest = fetch.after.max(page.last.unwrap_or(0));
        for message in page.messages {
            newest = newest.max(message.id);
            if message.channel_id != fetch.channel {
                continue;
            }
            let job = &mut self.queue[at];
            if message.id < oldest {
                // Older than the age limit (Discord returned a message the request shouldn't
                // have matched): the cursor passes it, nothing is delivered.
                job.too_old += 1;
                if let Some(entry) = self.channels.get_mut(&fetch.channel)
                    && message.id > entry.mark
                {
                    entry.mark = message.id;
                    outs.push(Out::Advance(Cursor {
                        channel: fetch.channel,
                        message: message.id,
                    }));
                }
                continue;
            }
            job.handled += 1;
            self.handle(message, outs);
        }
        let job = &mut self.queue[at];
        job.after = newest;
        job.pages += 1;
        job.failures = 0;
        job.not_before = None;
        if full && job.pages >= MAX_PAGES {
            tracing::warn!(
                "catch-up of a channel stopped after {MAX_PAGES} pages of {} messages: the \
                 messages after them, up to the live ones, are skipped",
                fetch.limit
            );
        }
        if !full || job.pages >= MAX_PAGES {
            self.finish(at, outs);
        }
    }

    /// Ends the pass at `at`: merges the buffered live messages, and makes the channel live, or
    /// queues another pass if live messages were discarded.
    fn finish(&mut self, at: usize, outs: &mut Vec<Out>) {
        let Some(job) = self.queue.remove(at) else {
            return;
        };
        tracing::info!(
            "caught up a channel: {} messages handled, {} older than the age limit skipped",
            job.handled,
            job.too_old
        );
        self.merge(job.channel, outs);
    }

    /// Hands over what was buffered for `channel`, and returns it to live (or to another pass).
    fn merge(&mut self, channel: u64, outs: &mut Vec<Out>) {
        let Some(entry) = self.channels.get_mut(&channel) else {
            return;
        };
        let Mode::CatchingUp { buffer, overflowed } =
            std::mem::replace(&mut entry.mode, Mode::Live)
        else {
            return;
        };
        for message in buffer {
            self.handle(message, outs);
        }
        if overflowed && !self.rejected {
            tracing::warn!(
                "live messages were discarded while a channel caught up; catching up again"
            );
            if let Some(entry) = self.channels.get_mut(&channel) {
                entry.mode = Mode::CatchingUp {
                    buffer: VecDeque::new(),
                    overflowed: false,
                };
                let mark = entry.mark;
                let job = self.job(channel, mark);
                self.queue.push_back(job);
            }
        }
    }

    /// No more requests: every channel that was waiting for one is live.
    fn stop_fetching(&mut self, outs: &mut Vec<Out>) {
        self.queue.clear();
        let mut waiting: Vec<u64> = self
            .channels
            .iter()
            .filter(|(_, entry)| matches!(entry.mode, Mode::CatchingUp { .. }))
            .map(|(channel, _)| *channel)
            .collect();
        waiting.sort_unstable();
        for channel in waiting {
            self.merge(channel, outs);
        }
    }
}

#[cfg(test)]
mod tests;
