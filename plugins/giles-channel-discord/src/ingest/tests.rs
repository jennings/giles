//! The catch-up algorithm, move by move: marks, buffers, paging, the age limit, retries, and the
//! empty-message alarm. No I/O: pages and live messages are fed in by hand.

use std::time::Duration;

use giles_plugin_sdk::giles_types::{ChannelEvent, ChannelStatus};
use tokio::time::Instant;

use super::{BUFFER_CAP, Clock, EMPTY_RUN, Fetch, Ingest, MAX_PAGES, Next, Out, PAGE, Params};
use crate::client::{DiscordMessage, Page, RestError};
use crate::fake::direct_message;
use crate::snowflake::at_unix_ms;
use crate::state::{DmChannel, StateFile};

/// 2026-10-24T12:00:00Z: the clock of every test.
const NOW: u64 = 1_792_843_200_000;
const HOUR_MS: u64 = 3_600_000;
const DAY: Duration = Duration::from_secs(24 * 3600);
const BOT: u64 = 900;
const USER: u64 = 11;
const OTHER_USER: u64 = 12;
const CHANNEL: u64 = 5000;
const OTHER_CHANNEL: u64 = 6000;

/// A message id created `ms_ago` before [`NOW`]; `n` orders ids of one millisecond.
fn id(ms_ago: u64, n: u64) -> u64 {
    at_unix_ms(NOW - ms_ago) + n
}

/// The first id the age limit allows after `age`.
fn floor(age: Duration) -> u64 {
    at_unix_ms(NOW - u64::try_from(age.as_millis()).unwrap())
}

fn dm(ms_ago: u64, text: &str) -> DiscordMessage {
    direct_message(id(ms_ago, 1), CHANNEL, USER, text)
}

fn dm_in(channel: u64, user: u64, ms_ago: u64, text: &str) -> DiscordMessage {
    direct_message(id(ms_ago, 1), channel, user, text)
}

struct Rig {
    ingest: Ingest,
    state: StateFile,
    dir: tempfile::TempDir,
    start: Instant,
}

impl Rig {
    fn new() -> Self {
        Self::with(DAY, |_| {})
    }

    /// Starts with the state `prepare` leaves in an empty state file.
    fn with(max_age: Duration, prepare: impl FnOnce(&StateFile)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = StateFile::open(dir.path());
        prepare(&state);
        Self::over(dir, state, max_age)
    }

    fn over(dir: tempfile::TempDir, state: StateFile, max_age: Duration) -> Self {
        let ingest = Ingest::new(Params {
            state: state.clone(),
            catch_up_max_age: max_age,
            clock: Clock::new(|| NOW),
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(8),
        });
        Self {
            ingest,
            state,
            dir,
            start: Instant::now(),
        }
    }

    /// The same data directory, in a new process.
    fn restarted(self, max_age: Duration) -> Self {
        let state = StateFile::open(self.dir.path());
        Self::over(self.dir, state, max_age)
    }

    fn ready(&mut self) {
        self.ingest.on_ready(BOT);
    }

    fn next(&self) -> Next {
        self.ingest.next(self.start)
    }

    /// The request to make now, which there must be one of.
    fn fetch(&self) -> Fetch {
        match self.next() {
            Next::Now(fetch) => fetch,
            other => panic!("no request to make: {other:?}"),
        }
    }

    fn page(&mut self, fetch: Fetch, page: Vec<DiscordMessage>) -> Vec<Out> {
        self.ingest.on_page(fetch, Ok(page.into()), self.start)
    }

    /// Answers every request with the messages of `history` after it, as Discord does (a page of
    /// up to 100, oldest first), until there is nothing to ask. Returns everything handed over.
    fn serve(&mut self, history: &[DiscordMessage]) -> Vec<Out> {
        let mut outs = Vec::new();
        while let Next::Now(fetch) = self.next() {
            let page = history
                .iter()
                .filter(|m| m.channel_id == fetch.channel && m.id > fetch.after)
                .take(usize::from(fetch.limit))
                .cloned()
                .collect();
            outs.extend(self.page(fetch, page));
        }
        outs
    }
}

/// The text of the messages handed over, in order.
fn texts(outs: &[Out]) -> Vec<String> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Deliver { event, .. } => match &**event {
                ChannelEvent::Message { message, .. } => Some(message.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The ids the cursor will pass, in order: delivered messages and ones done with.
fn cursors(outs: &[Out]) -> Vec<u64> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Deliver { cursor, .. } | Out::Advance(cursor) => Some(cursor.message),
            Out::Status(_) => None,
        })
        .collect()
}

fn statuses(outs: &[Out]) -> Vec<ChannelStatus> {
    outs.iter()
        .filter_map(|out| match out {
            Out::Status(status) => Some(status.clone()),
            _ => None,
        })
        .collect()
}

// --- live messages ---

#[test]
fn a_live_message_of_a_known_channel_is_delivered_at_once_and_a_replay_is_not() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    let first = dm(3000, "one");
    let second = dm(2000, "two");
    let outs = rig.ingest.on_message(first.clone());
    assert_eq!(texts(&outs), ["one"]);
    assert_eq!(cursors(&outs), [first.id]);
    assert_eq!(texts(&rig.ingest.on_message(second.clone())), ["two"]);
    // A resume replays what Discord already sent, and a message is never handled twice.
    assert!(rig.ingest.on_message(first).is_empty());
    assert!(rig.ingest.on_message(second).is_empty());
    // A known channel with no pass pending asks for nothing.
    assert_eq!(rig.next(), Next::Idle);
}

#[test]
fn messages_are_handled_in_the_order_they_arrive() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    let mut handed = Vec::new();
    for (ago, text) in [(5000, "a"), (4000, "b"), (3000, "c"), (2000, "d")] {
        handed.extend(rig.ingest.on_message(dm(ago, text)));
    }
    assert_eq!(texts(&handed), ["a", "b", "c", "d"]);
    let ids = cursors(&handed);
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]), "{ids:?}");
}

#[test]
fn a_cursor_in_the_state_file_is_the_mark_after_a_restart() {
    let mut rig = Rig::with(DAY, |state| {
        state.record_dm(CHANNEL, USER);
        state.advance(CHANNEL, id(2000, 1));
    });
    // The message at the cursor, and one before it, are not delivered again.
    assert!(rig.ingest.on_message(dm(2000, "seen")).is_empty());
    assert!(rig.ingest.on_message(dm(3000, "older")).is_empty());
    assert_eq!(texts(&rig.ingest.on_message(dm(1000, "new"))), ["new"]);
}

#[test]
fn a_bot_message_of_a_known_channel_is_done_with_but_not_delivered() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    let mut own = dm(1000, "my reply");
    own.author.id = BOT;
    own.author.bot = true;
    rig.ready();
    // Ready queued a pass for the known channel; answer it empty so the channel is live.
    let fetch = rig.fetch();
    rig.page(fetch, vec![]);
    let outs = rig.ingest.on_message(own.clone());
    assert!(texts(&outs).is_empty());
    assert_eq!(cursors(&outs), [own.id]);
}

#[test]
fn a_message_of_an_unknown_channel_from_a_bot_or_a_server_teaches_nothing() {
    let mut rig = Rig::new();
    rig.ready();
    let mut bot = dm(1000, "beep");
    bot.author.bot = true;
    let mut guild = dm(1000, "server");
    guild.guild = true;
    let mut webhook = dm(1000, "hook");
    webhook.webhook = true;
    let mut own = dm(1000, "mine");
    own.author.id = BOT;
    for message in [bot, guild, webhook, own] {
        assert!(rig.ingest.on_message(message).is_empty());
    }
    assert_eq!(rig.state.channels(), Vec::<u64>::new());
    assert_eq!(rig.next(), Next::Idle);
}

#[test]
fn a_system_message_from_a_person_teaches_the_channel() {
    let mut rig = Rig::new();
    rig.ready();
    let mut call = dm(1000, "");
    call.kind = 3;
    rig.ingest.on_message(call);
    assert_eq!(rig.state.channels(), [CHANNEL]);
    assert_eq!(rig.fetch().channel, CHANNEL);
}

// --- first sight ---

#[test]
fn the_first_message_of_a_new_channel_waits_for_the_history_before_it() {
    let mut rig = Rig::new();
    rig.ready();
    let third = dm(1000, "third");
    assert!(rig.ingest.on_message(third.clone()).is_empty());
    // Learned at once, so a restart before anything is delivered still knows the channel.
    assert_eq!(
        rig.state.get(CHANNEL),
        Some(DmChannel {
            cursor: None,
            user: Some(USER)
        })
    );
    // No cursor: the history of the age limit.
    let fetch = rig.fetch();
    assert_eq!(
        fetch,
        Fetch {
            channel: CHANNEL,
            after: floor(DAY),
            limit: PAGE
        }
    );
    // A second message of the channel meanwhile waits behind the first.
    let fourth = dm(500, "fourth");
    assert!(rig.ingest.on_message(fourth).is_empty());

    let first = dm(3000, "first");
    let second = dm(2000, "second");
    // The history includes the first live message as well, as it would: it was sent already.
    let outs = rig.page(fetch, vec![first, second, third]);
    assert_eq!(texts(&outs), ["first", "second", "third", "fourth"]);
    assert_eq!(rig.next(), Next::Idle);
    // Live now.
    assert_eq!(texts(&rig.ingest.on_message(dm(100, "fifth"))), ["fifth"]);
}

#[test]
fn a_new_channels_first_message_is_delivered_once_even_if_the_page_has_it_and_the_buffer_too() {
    let mut rig = Rig::new();
    rig.ready();
    let only = dm(1000, "only");
    rig.ingest.on_message(only.clone());
    rig.ingest.on_message(only.clone());
    let fetch = rig.fetch();
    let outs = rig.page(fetch, vec![only]);
    assert_eq!(texts(&outs), ["only"]);
}

#[test]
fn a_channel_added_to_the_state_by_a_send_is_caught_up_on_its_first_message() {
    let mut rig = Rig::new();
    rig.ready();
    // What a send does after Create DM, while the plugin runs.
    rig.state.record_dm(OTHER_CHANNEL, OTHER_USER);
    assert_eq!(rig.next(), Next::Idle);
    let reply = dm_in(OTHER_CHANNEL, OTHER_USER, 1000, "hello");
    assert!(rig.ingest.on_message(reply).is_empty());
    assert_eq!(rig.fetch().channel, OTHER_CHANNEL);
}

#[test]
fn nothing_is_fetched_before_ready_for_a_channel_in_the_state_file() {
    let rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    assert_eq!(rig.next(), Next::Idle);
}

// --- catch-up after Ready ---

#[test]
fn ready_catches_up_every_known_channel_one_at_a_time_from_its_cursor() {
    let mut rig = Rig::with(DAY, |state| {
        state.record_dm(CHANNEL, USER);
        state.advance(CHANNEL, id(2 * HOUR_MS, 1));
        state.record_dm(OTHER_CHANNEL, OTHER_USER);
        state.advance(OTHER_CHANNEL, id(HOUR_MS, 5));
    });
    rig.ready();
    // The lower channel id first; the cursor, not the age limit, is where it starts.
    let first = rig.fetch();
    assert_eq!(
        first,
        Fetch {
            channel: CHANNEL,
            after: id(2 * HOUR_MS, 1),
            limit: PAGE
        }
    );
    let missed = dm(HOUR_MS, "missed while down");
    let outs = rig.page(first, vec![missed.clone()]);
    assert_eq!(texts(&outs), ["missed while down"]);
    assert_eq!(cursors(&outs), [missed.id]);
    // Then the other, which is asked for only now.
    let second = rig.fetch();
    assert_eq!(second.channel, OTHER_CHANNEL);
    assert_eq!(second.after, id(HOUR_MS, 5));
    assert!(rig.page(second, vec![]).is_empty());
    assert_eq!(rig.next(), Next::Idle);
}

#[test]
fn a_cursor_older_than_the_age_limit_starts_at_the_limit() {
    let mut rig = Rig::with(DAY, |state| {
        state.record_dm(CHANNEL, USER);
        state.advance(CHANNEL, id(10 * 24 * HOUR_MS, 1));
    });
    rig.ready();
    assert_eq!(rig.fetch().after, floor(DAY));
}

#[test]
fn a_message_older_than_the_age_limit_is_passed_not_delivered() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let fetch = rig.fetch();
    // Discord isn't supposed to return it for this `after`; if it did, it is not delivered, and
    // the cursor passes it so it isn't asked for again.
    let stale = dm(25 * HOUR_MS, "from last week");
    let fresh = dm(HOUR_MS, "from this morning");
    let outs = rig.page(fetch, vec![stale.clone(), fresh.clone()]);
    assert_eq!(texts(&outs), ["from this morning"]);
    assert_eq!(cursors(&outs), [stale.id, fresh.id]);
}

#[test]
fn paging_goes_on_after_the_largest_id_until_a_short_page() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let history: Vec<DiscordMessage> = (1..=237)
        .map(|n| direct_message(id(HOUR_MS, n), CHANNEL, USER, &format!("m{n}")))
        .collect();
    let mut requests = Vec::new();
    let mut handed = Vec::new();
    while let Next::Now(fetch) = rig.next() {
        requests.push(fetch.after);
        let page = history
            .iter()
            .filter(|m| m.id > fetch.after)
            .take(usize::from(fetch.limit))
            .cloned()
            .collect();
        handed.extend(rig.page(fetch, page));
    }
    // 100, 100, 37: three requests, each after the last message of the one before.
    assert_eq!(requests, [floor(DAY), history[99].id, history[199].id]);
    let expected: Vec<String> = (1..=237).map(|n| format!("m{n}")).collect();
    assert_eq!(texts(&handed), expected);
}

#[test]
fn a_full_page_with_a_message_that_could_not_be_read_is_still_full() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let history: Vec<DiscordMessage> = (1..=150)
        .map(|n| direct_message(id(HOUR_MS, n), CHANNEL, USER, &format!("m{n}")))
        .collect();
    let first = rig.fetch();
    // Discord sent 100; the models rejected the 50th, so only 99 could be read, and the page's
    // last id is the highest of the hundred sent.
    let mut readable = history[..100].to_vec();
    let skipped = readable.remove(49);
    let page = Page {
        messages: readable,
        received: 100,
        last: Some(history[99].id),
    };
    let mut handed = rig.ingest.on_page(first, Ok(page), rig.start);
    assert_eq!(texts(&handed).len(), 99);
    // The pass goes on: the next request is after the page's last id, and nothing is lost after it.
    let second = rig.fetch();
    assert_eq!(second.after, history[99].id);
    handed.extend(rig.page(second, history[100..].to_vec()));
    let expected: Vec<String> = (1..=150)
        .filter(|n| history[n - 1].id != skipped.id)
        .map(|n| format!("m{n}"))
        .collect();
    assert_eq!(texts(&handed), expected);
    assert_eq!(rig.next(), Next::Idle);
}

#[test]
fn a_page_of_exactly_one_hundred_is_followed_by_an_empty_one() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let history: Vec<DiscordMessage> = (1..=100)
        .map(|n| direct_message(id(HOUR_MS, n), CHANNEL, USER, "x"))
        .collect();
    let outs = rig.serve(&history);
    assert_eq!(texts(&outs).len(), 100);
    assert_eq!(rig.next(), Next::Idle);
}

#[test]
fn a_pass_stops_after_the_page_cap_and_the_rest_is_lost() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let history: Vec<DiscordMessage> = (1..=u64::from(MAX_PAGES) * 100 + 50)
        .map(|n| direct_message(id(HOUR_MS, n), CHANNEL, USER, "x"))
        .collect();
    let outs = rig.serve(&history);
    assert_eq!(
        texts(&outs).len(),
        usize::try_from(MAX_PAGES).unwrap() * 100
    );
    assert_eq!(rig.next(), Next::Idle, "no 101st request");
    // The channel is live, and what came after the cap stays lost: the mark is past the pages.
    let later = direct_message(id(1000, 1), CHANNEL, USER, "live");
    assert_eq!(texts(&rig.ingest.on_message(later)), ["live"]);
}

#[test]
fn live_messages_during_a_pass_are_merged_without_a_gap_or_a_repeat() {
    let mut rig = Rig::with(DAY, |state| {
        state.record_dm(CHANNEL, USER);
        state.advance(CHANNEL, id(10 * HOUR_MS, 1));
    });
    rig.ready();
    let history: Vec<DiscordMessage> = (1..=150)
        .map(|n| direct_message(id(5 * HOUR_MS, n), CHANNEL, USER, &format!("h{n}")))
        .collect();
    let fetch = rig.fetch();
    // Live: one the history will have (sent before the page was made), one it won't.
    let in_history = history[149].clone();
    let after_history = direct_message(id(HOUR_MS, 1), CHANNEL, USER, "live-new");
    assert!(rig.ingest.on_message(in_history).is_empty());
    assert!(rig.ingest.on_message(after_history).is_empty());

    let page_one: Vec<_> = history[..100].to_vec();
    let mut handed = rig.page(fetch, page_one);
    let fetch = rig.fetch();
    handed.extend(rig.page(fetch, history[100..].to_vec()));
    let mut expected: Vec<String> = (1..=150).map(|n| format!("h{n}")).collect();
    expected.push("live-new".into());
    assert_eq!(texts(&handed), expected);
    assert_eq!(rig.next(), Next::Idle);
}

#[test]
fn a_channel_whose_pass_is_over_is_live_while_another_still_catches_up() {
    let mut rig = Rig::with(DAY, |state| {
        state.record_dm(CHANNEL, USER);
        state.record_dm(OTHER_CHANNEL, OTHER_USER);
    });
    rig.ready();
    // Passes run one channel at a time, lower id first.
    let fetch = rig.fetch();
    assert_eq!(fetch.channel, CHANNEL);
    rig.page(fetch, vec![]);
    assert_eq!(rig.fetch().channel, OTHER_CHANNEL);
    // CHANNEL is live now, and not held up by OTHER_CHANNEL's pass...
    assert_eq!(texts(&rig.ingest.on_message(dm(100, "live"))), ["live"]);
    // ...whose own live messages wait for it.
    let waiting = dm_in(OTHER_CHANNEL, OTHER_USER, 90, "waits");
    assert!(rig.ingest.on_message(waiting).is_empty());
    let fetch = rig.fetch();
    assert_eq!(texts(&rig.page(fetch, vec![])), ["waits"]);
}

#[test]
fn bots_and_own_messages_in_a_page_move_the_cursor_without_being_delivered() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let fetch = rig.fetch();
    let mut own = dm(3000, "my reply");
    own.author.id = BOT;
    own.author.bot = true;
    let mut bot = dm(2000, "beep");
    bot.author.id = 777;
    bot.author.bot = true;
    let person = dm(1000, "question");
    let outs = rig.page(fetch, vec![own.clone(), bot.clone(), person.clone()]);
    assert_eq!(texts(&outs), ["question"]);
    assert_eq!(cursors(&outs), [own.id, bot.id, person.id]);
}

#[test]
fn a_second_ready_starts_the_passes_over_from_the_marks() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let fetch = rig.fetch();
    let first = dm(3000, "a");
    rig.page(fetch, vec![first.clone()]);
    // The pass wasn't finished (say the page was full); the session reconnects with a new session.
    rig.ready();
    let again = rig.fetch();
    assert_eq!(again.after, first.id.max(floor(DAY)));
    // The answer to the request of the old session, if it arrives, is ignored.
    assert!(rig.page(fetch, vec![dm(2000, "stale")]).is_empty());
}

#[test]
fn a_live_message_during_a_restarted_pass_is_kept_for_it() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    rig.ingest.on_message(dm(1000, "live"));
    rig.ready();
    let fetch = rig.fetch();
    let outs = rig.page(fetch, vec![]);
    assert_eq!(texts(&outs), ["live"]);
}

// --- catch-up off ---

#[test]
fn with_catch_up_off_nothing_is_fetched_and_a_new_channel_is_live_at_once() {
    let mut rig = Rig::with(Duration::ZERO, |state| {
        state.record_dm(CHANNEL, USER);
        state.advance(CHANNEL, id(HOUR_MS, 1));
    });
    rig.ready();
    assert_eq!(rig.next(), Next::Idle);
    assert_eq!(texts(&rig.ingest.on_message(dm(500, "a"))), ["a"]);
    let outs = rig
        .ingest
        .on_message(dm_in(OTHER_CHANNEL, OTHER_USER, 400, "b"));
    assert_eq!(texts(&outs), ["b"]);
    assert_eq!(rig.next(), Next::Idle);
    // The cursor still stops a replay.
    assert!(rig.ingest.on_message(dm(HOUR_MS, "old")).is_empty());
}

// --- failures ---

#[test]
fn a_failed_request_is_tried_again_after_a_delay_that_doubles_to_a_cap() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let fetch = rig.fetch();
    let mut now = rig.start;
    let mut delays = Vec::new();
    for error in [
        RestError::Transport,
        RestError::TimedOut,
        RestError::Api {
            status: 503,
            code: None,
        },
        RestError::Transport,
        RestError::Transport,
        RestError::Transport,
    ] {
        assert!(rig.ingest.on_page(fetch, Err(error), now).is_empty());
        // Not due yet, and the same request when it is.
        let Next::Wait(at) = rig.ingest.next(now) else {
            panic!("not waiting");
        };
        delays.push(at - now);
        assert_eq!(
            rig.ingest.next(at - Duration::from_millis(1)),
            Next::Wait(at)
        );
        assert_eq!(rig.ingest.next(at), Next::Now(fetch));
        now = at;
    }
    assert_eq!(
        delays,
        [1, 2, 4, 8, 8, 8].map(Duration::from_secs),
        "doubling to the cap"
    );
    // A success ends the pass, and the next one starts with a short delay again.
    rig.ingest.on_page(fetch, Ok(Vec::new().into()), now);
    assert_eq!(rig.ingest.next(now), Next::Idle);
}

#[test]
fn a_failing_channel_does_not_hold_up_the_others() {
    let mut rig = Rig::with(DAY, |state| {
        state.record_dm(CHANNEL, USER);
        state.record_dm(OTHER_CHANNEL, OTHER_USER);
    });
    rig.ready();
    let first = rig.fetch();
    assert_eq!(first.channel, CHANNEL);
    rig.ingest
        .on_page(first, Err(RestError::TimedOut), rig.start);
    // The other channel is asked for while the first waits.
    let second = rig.fetch();
    assert_eq!(second.channel, OTHER_CHANNEL);
    // A live message of the waiting channel is buffered, and delivered when it is done.
    assert!(rig.ingest.on_message(dm(100, "waits")).is_empty());
    rig.page(second, vec![]);
    let Next::Wait(at) = rig.next() else {
        panic!("expected to wait");
    };
    assert_eq!(at - rig.start, Duration::from_secs(1));
    let retry = rig.ingest.next(at);
    let Next::Now(retry) = retry else {
        panic!("{retry:?}")
    };
    assert_eq!(retry, first);
    let outs = rig.ingest.on_page(retry, Ok(Vec::new().into()), at);
    assert_eq!(texts(&outs), ["waits"]);
}

#[test]
fn a_request_that_can_never_work_skips_the_channel_and_releases_its_messages() {
    for status in [403, 404] {
        let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
        rig.ready();
        rig.ingest.on_message(dm(100, "live"));
        let fetch = rig.fetch();
        let outs = rig.ingest.on_page(
            fetch,
            Err(RestError::Api {
                status,
                code: Some(10003),
            }),
            rig.start,
        );
        assert_eq!(texts(&outs), ["live"], "{status}");
        assert_eq!(rig.next(), Next::Idle);
    }
}

#[test]
fn a_rejected_token_reports_it_stops_asking_and_releases_every_channel() {
    let mut rig = Rig::with(DAY, |state| {
        state.record_dm(CHANNEL, USER);
        state.record_dm(OTHER_CHANNEL, OTHER_USER);
    });
    rig.ready();
    rig.ingest.on_message(dm(200, "one"));
    rig.ingest
        .on_message(dm_in(OTHER_CHANNEL, OTHER_USER, 100, "two"));
    let fetch = rig.fetch();
    let outs = rig
        .ingest
        .on_page(fetch, Err(RestError::Unauthorized), rig.start);
    assert_eq!(statuses(&outs), [crate::status::rejected_token()]);
    assert_eq!(texts(&outs), ["one", "two"]);
    assert_eq!(rig.next(), Next::Idle);
    // Later Readys and new channels don't ask again either.
    rig.ready();
    assert_eq!(rig.next(), Next::Idle);
    let outs = rig
        .ingest
        .on_message(dm_in(7000, 13, 50, "new channel is live at once"));
    assert_eq!(texts(&outs), ["new channel is live at once"]);
    assert_eq!(rig.next(), Next::Idle);
}

#[test]
fn a_token_a_send_found_rejected_stops_the_asking_and_releases_every_channel_once() {
    let mut rig = Rig::with(DAY, |state| {
        state.record_dm(CHANNEL, USER);
        state.record_dm(OTHER_CHANNEL, OTHER_USER);
    });
    rig.ready();
    rig.ingest.on_message(dm(200, "one"));
    rig.ingest
        .on_message(dm_in(OTHER_CHANNEL, OTHER_USER, 100, "two"));

    let outs = rig.ingest.on_token_rejected();
    // The session reports the status; the ingest only lets go of what it held back.
    assert!(statuses(&outs).is_empty());
    assert_eq!(texts(&outs), ["one", "two"]);
    assert_eq!(rig.next(), Next::Idle);
    assert!(rig.ingest.on_token_rejected().is_empty());
    rig.ready();
    assert_eq!(rig.next(), Next::Idle);
}

#[test]
fn an_answer_nobody_asked_for_is_ignored() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let wrong = Fetch {
        channel: CHANNEL,
        after: 42,
        limit: PAGE,
    };
    assert!(rig.page(wrong, vec![dm(100, "x")]).is_empty());
    assert!(
        rig.ingest
            .on_page(wrong, Err(RestError::Unauthorized), rig.start)
            .is_empty()
    );
    assert!(
        matches!(rig.next(), Next::Now(_)),
        "the pass is still there"
    );
}

#[test]
fn live_messages_beyond_the_buffer_are_discarded_and_fetched_by_another_pass() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    rig.ready();
    let total = u64::try_from(BUFFER_CAP).unwrap() + 20;
    let live: Vec<DiscordMessage> = (1..=total)
        .map(|n| direct_message(id(HOUR_MS, n), CHANNEL, USER, &format!("l{n}")))
        .collect();
    for message in &live {
        assert!(rig.ingest.on_message(message.clone()).is_empty());
    }
    // The first pass finds nothing; the buffered messages are handed over, in order.
    let fetch = rig.fetch();
    let mut handed = rig.page(fetch, vec![]);
    assert_eq!(texts(&handed).len(), BUFFER_CAP);
    // Another pass starts, after the last of them, and finds what was discarded.
    let again = rig.fetch();
    assert_eq!(again.after, live[BUFFER_CAP - 1].id);
    handed.extend(rig.page(again, live[BUFFER_CAP..].to_vec()));
    let expected: Vec<String> = (1..=total).map(|n| format!("l{n}")).collect();
    assert_eq!(texts(&handed), expected);
    assert_eq!(rig.next(), Next::Idle);
}

// --- persistence across restarts ---

#[test]
fn what_the_cursor_passed_is_not_delivered_again_after_a_restart() {
    let mut rig = Rig::new();
    rig.ready();
    let one = dm(3000, "one");
    let two = dm(2000, "two");
    let three = dm(1000, "three");
    let history = [one.clone(), two.clone(), three.clone()];
    rig.ingest.on_message(one);
    let handed = rig.serve(&history);
    assert_eq!(texts(&handed), ["one", "two", "three"]);
    // The SDK has persisted the first two, but not the third, when the process ends: the channel
    // moves the cursor only for what the SDK took.
    rig.state.advance(CHANNEL, two.id);

    let mut rig = rig.restarted(DAY);
    rig.ready();
    let fetch = rig.fetch();
    assert_eq!(fetch.after, two.id, "the cursor, not the age limit");
    let outs = rig.serve(&history);
    assert_eq!(
        texts(&outs),
        ["three"],
        "only what the cursor hadn't passed"
    );
}

// --- empty messages ---

#[test]
fn three_empty_messages_in_a_row_are_an_error_and_a_message_with_text_clears_it() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    let mut ago = 10_000;
    let mut next = |text: &str| {
        ago -= 100;
        dm(ago, text)
    };
    let mut all = Vec::new();
    for round in 1..=EMPTY_RUN {
        let outs = rig.ingest.on_message(next(""));
        // An empty message is done with, not delivered.
        assert!(texts(&outs).is_empty());
        assert_eq!(cursors(&outs).len(), 1);
        if round < EMPTY_RUN {
            assert!(statuses(&outs).is_empty(), "round {round}");
        }
        all.extend(outs);
    }
    assert_eq!(statuses(&all), [crate::status::missing_content()]);
    // While the run lasts the status is said again, which the session doesn't repeat to the SDK.
    assert_eq!(
        statuses(&rig.ingest.on_message(next(""))),
        [crate::status::missing_content()]
    );
    // Text clears it: delivered, and Connected again.
    let outs = rig.ingest.on_message(next("hello"));
    assert_eq!(texts(&outs), ["hello"]);
    assert_eq!(statuses(&outs), [ChannelStatus::Connected]);
    // And counts from zero again.
    for _ in 0..EMPTY_RUN - 1 {
        assert!(statuses(&rig.ingest.on_message(next(""))).is_empty());
    }
    assert_eq!(
        statuses(&rig.ingest.on_message(next(""))),
        [crate::status::missing_content()]
    );
}

#[test]
fn two_empty_messages_and_a_message_with_text_never_raise_it() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    let mut ago = 10_000;
    for _ in 0..5 {
        for text in ["", "", "text"] {
            ago -= 100;
            let outs = rig.ingest.on_message(dm(ago, text));
            assert!(statuses(&outs).is_empty());
        }
    }
}

#[test]
fn bots_and_system_messages_do_not_count_as_empty() {
    let mut rig = Rig::with(DAY, |state| state.record_dm(CHANNEL, USER));
    let mut ago = 10_000;
    for round in 0..10 {
        ago -= 100;
        let mut message = dm(ago, "");
        if round % 2 == 0 {
            message.author.bot = true;
        } else {
            message.kind = 6;
        }
        assert!(statuses(&rig.ingest.on_message(message)).is_empty());
    }
}

// --- the clock ---

#[test]
fn the_system_clock_is_the_present() {
    // After the day this was written, and not absurdly far after it.
    let now = Clock::system().now_ms();
    assert!(now > NOW && now < NOW + 50 * 365 * 24 * HOUR_MS, "{now}");
}
