//! Per-chat live state: the event log every open page of a chat follows over server-sent events.
//!
//! Every change to what a chat page shows (a posted message, a sent message, a live message
//! growing, an approval prompt and its answer) is one [`Event`]: an HTML fragment for the page's
//! message list. Fragments that change an element already on the page carry `hx-swap-oob`. A page
//! is rendered with a [cursor](Hubs::cursor) (the last event it reflects) and asks the event
//! stream for everything after it, so nothing falls between the page load and the stream opening.
//! Each chat keeps its last [`RING`] events for that; a page further behind (or from before a
//! restart) is told to reload.
//!
//! Event numbers come from one counter for every chat, so they only grow per chat even when an
//! idle chat's hub is dropped ([`HUB_IDLE`]) and made again: a page from before that is behind
//! the new hub's floor and is told to reload.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use giles_types::{ConversationId, EventId, Timestamp};
use tokio::sync::broadcast;
use tokio::time::Instant;

use super::{lock, random_hex, render};

/// How many recent events each chat keeps for pages catching up.
pub(super) const RING: usize = 128;

/// How long a chat's hub is kept after its last use once nothing needs it (no open page, no live
/// message, no unanswered prompt). Dropped hubs are made again on the next use.
pub(super) const HUB_IDLE: Duration = Duration::from_secs(10 * 60);

/// One change to a chat page.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Event {
    /// Its number: larger than every earlier event of the chat.
    pub seq: u64,
    /// The fragment appended to the page's message list (out-of-band swaps included).
    pub html: String,
}

/// An approval prompt shown in a chat, until it's resolved or the hub is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Approval {
    /// The short code.
    pub code: String,
    /// The prompt, rendered.
    pub prompt: String,
    /// What a button sent (`Sent: yes`), shown instead of the buttons; `None` while unanswered.
    pub sent: Option<String>,
}

/// A live message as of the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Live {
    /// The element id.
    pub id: String,
    /// Its current (non-swapping) HTML.
    pub html: String,
    /// The text it shows.
    pub text: String,
    /// When it started.
    pub started_at: Timestamp,
}

/// What a page shows besides the stored transcript, as of one event.
#[derive(Debug)]
pub(super) struct Snapshot {
    /// The last event it reflects.
    pub seq: u64,
    /// Live messages still being written, oldest first.
    pub live: Vec<Live>,
    /// Prompts not resolved yet, oldest first.
    pub approvals: Vec<Approval>,
}

/// Where an event stream starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum After {
    /// No cursor: only new events.
    Now,
    /// After this event.
    Seq(u64),
    /// A cursor from another run of the daemon (or garbage): the page is stale.
    Stale,
}

/// An open event stream: the events to replay, whether some were lost, and the live feed.
#[derive(Debug)]
pub(super) struct Subscription {
    /// Events after the cursor still in the ring, oldest first.
    pub replay: Vec<Arc<Event>>,
    /// Events after the cursor were dropped: the page should reload.
    pub gap: bool,
    /// The last event number at subscription time; the feed only has later ones.
    pub seq: u64,
    /// Every later event.
    pub feed: broadcast::Receiver<Arc<Event>>,
}

#[derive(Debug)]
struct State {
    /// The last event's number (the floor while there is none).
    seq: u64,
    /// Events up to this number are gone: dropped from the ring, or from before this hub.
    floor: u64,
    ring: VecDeque<Arc<Event>>,
    live: Vec<Live>,
    approvals: Vec<Approval>,
    /// Posted messages' events still in the ring: number and event id.
    posted: Vec<(u64, EventId)>,
    /// The last publish, snapshot, subscription, or lookup.
    used: Instant,
}

/// One chat's live state.
#[derive(Debug)]
pub(super) struct Hub {
    state: Mutex<State>,
    feed: broadcast::Sender<Arc<Event>>,
    /// Event numbers, shared by every chat's hub.
    counter: Arc<AtomicU64>,
}

impl Hub {
    fn new(counter: Arc<AtomicU64>) -> Self {
        let floor = counter.load(Ordering::SeqCst);
        Self {
            state: Mutex::new(State {
                seq: floor,
                floor,
                ring: VecDeque::new(),
                live: Vec::new(),
                approvals: Vec::new(),
                posted: Vec::new(),
                used: Instant::now(),
            }),
            feed: broadcast::Sender::new(RING),
            counter,
        }
    }

    /// Records and broadcasts one event, after applying `change` to the state (which sees the
    /// event's number in `state.seq`). Under one lock, so [`subscribe`](Self::subscribe) sees
    /// every event exactly once, in the ring or the feed.
    fn publish(&self, html: String, change: impl FnOnce(&mut State)) {
        let mut state = lock(&self.state);
        state.seq = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        change(&mut state);
        state.used = Instant::now();
        let event = Arc::new(Event {
            seq: state.seq,
            html,
        });
        if state.ring.len() == RING
            && let Some(dropped) = state.ring.pop_front()
        {
            state.floor = dropped.seq;
            let floor = state.floor;
            state.posted.retain(|(seq, _)| *seq > floor);
        }
        state.ring.push_back(event.clone());
        // No receivers is fine: nobody has the chat open.
        let _ = self.feed.send(event);
    }

    /// Appends a finished message (`html` from [`render::message`]).
    pub(super) fn append(&self, html: String) {
        self.publish(html, |_| {});
    }

    /// Appends the message the user posted as event `id` (`html` from [`render::posted`]).
    pub(super) fn append_posted(&self, id: &EventId, html: String) {
        let id = id.clone();
        self.publish(html, |state| {
            let seq = state.seq;
            state.posted.push((seq, id));
        });
    }

    /// The posted events (their ids) after event `seq` still in the ring: an event stream from a
    /// page at `seq` replays them, so the page mustn't show them itself.
    pub(super) fn posted_after(&self, seq: u64) -> Vec<EventId> {
        lock(&self.state)
            .posted
            .iter()
            .filter(|(posted, _)| *posted > seq)
            .map(|(_, id)| id.clone())
            .collect()
    }

    /// Appends a live message with element id `id`.
    pub(super) fn live_start(&self, id: &str, html: String) {
        let live = Live {
            id: id.to_owned(),
            html: html.clone(),
            text: String::new(),
            started_at: Timestamp::now(),
        };
        self.publish(html, |state| state.live.push(live));
    }

    /// Replaces live message `id` with `html` showing `text`, still live.
    pub(super) fn live_update(&self, id: &str, html: String, text: &str) {
        let swap = render::out_of_band(&html);
        self.publish(swap, |state| {
            if let Some(live) = state.live.iter_mut().find(|live| live.id == id) {
                live.html = html;
                text.clone_into(&mut live.text);
            }
        });
    }

    /// Replaces live message `id` with its final `html`; it's no longer live.
    pub(super) fn live_end(&self, id: &str, html: &str) {
        self.publish(render::out_of_band(html), |state| {
            state.live.retain(|live| live.id != id);
        });
    }

    /// Appends an approval prompt (`html` is its form, without a CSRF field).
    pub(super) fn approval_add(&self, approval: Approval, html: String) {
        self.publish(html, |state| {
            state.approvals.retain(|a| a.code != approval.code);
            state.approvals.push(approval);
        });
    }

    /// Replaces the buttons of prompt `code` with `label` (what a button sent), rendering the
    /// prompt with `render(prompt, label)`. `false`, publishing nothing, if no such prompt is shown.
    pub(super) fn approval_sent(
        &self,
        code: &str,
        label: String,
        render: impl FnOnce(&str, &str) -> String,
    ) -> bool {
        let Some(prompt) = self.approval_prompt(code) else {
            return false;
        };
        self.publish(render::out_of_band(&render(&prompt, &label)), |state| {
            if let Some(approval) = state.approvals.iter_mut().find(|a| a.code == code) {
                approval.sent = Some(label);
            }
        });
        true
    }

    /// Retires prompt `code`, replacing it with `render(prompt)` (its outcome). `false`,
    /// publishing nothing, if no such prompt is shown.
    pub(super) fn approval_resolve(&self, code: &str, render: impl FnOnce(&str) -> String) -> bool {
        let Some(prompt) = self.approval_prompt(code) else {
            return false;
        };
        self.publish(render::out_of_band(&render(&prompt)), |state| {
            state.approvals.retain(|a| a.code != code);
        });
        true
    }

    fn approval_prompt(&self, code: &str) -> Option<String> {
        lock(&self.state)
            .approvals
            .iter()
            .find(|a| a.code == code)
            .map(|a| a.prompt.clone())
    }

    /// The live messages and prompts as of the latest event.
    pub(super) fn snapshot(&self) -> Snapshot {
        let mut state = lock(&self.state);
        state.used = Instant::now();
        Snapshot {
            seq: state.seq,
            live: state.live.clone(),
            approvals: state.approvals.clone(),
        }
    }

    /// Opens an event stream starting after `after`.
    pub(super) fn subscribe(&self, after: After) -> Subscription {
        let mut state = lock(&self.state);
        state.used = Instant::now();
        let feed = self.feed.subscribe();
        let (replay, gap) = match after {
            After::Now => (Vec::new(), false),
            After::Stale => (Vec::new(), true),
            After::Seq(seen) if seen > state.seq => (Vec::new(), true),
            After::Seq(seen) => {
                let replay: Vec<_> = state
                    .ring
                    .iter()
                    .filter(|event| event.seq > seen)
                    .cloned()
                    .collect();
                (replay, seen < state.floor)
            }
        };
        Subscription {
            replay,
            gap,
            seq: state.seq,
            feed,
        }
    }

    /// Whether nothing needs this hub any more as of `now`: no open stream, no live message, no
    /// unanswered prompt, and unused for [`HUB_IDLE`].
    fn is_idle(&self, now: Instant) -> bool {
        let state = lock(&self.state);
        self.feed.receiver_count() == 0
            && state.live.is_empty()
            && state.approvals.iter().all(|a| a.sent.is_some())
            && now.saturating_duration_since(state.used) >= HUB_IDLE
    }

    fn touch(&self) {
        lock(&self.state).used = Instant::now();
    }
}

/// Every chat's hub, made on first use and dropped when idle, and the cursor format.
#[derive(Debug)]
pub(super) struct Hubs {
    /// Random per run of the daemon, so a cursor from before a restart is recognized as stale.
    epoch: String,
    counter: Arc<AtomicU64>,
    hubs: Mutex<HashMap<ConversationId, Arc<Hub>>>,
}

impl Hubs {
    pub(super) fn new() -> Self {
        Self {
            epoch: random_hex(8),
            counter: Arc::default(),
            hubs: Mutex::default(),
        }
    }

    /// The hub of `chat`. Making a new one first drops every idle hub ([`HUB_IDLE`]); one in use
    /// elsewhere (an `Arc` held, e.g. by a live message) never is.
    pub(super) fn get(&self, chat: &ConversationId) -> Arc<Hub> {
        let mut hubs = lock(&self.hubs);
        if let Some(hub) = hubs.get(chat) {
            hub.touch();
            return hub.clone();
        }
        let now = Instant::now();
        hubs.retain(|_, hub| Arc::strong_count(hub) > 1 || !hub.is_idle(now));
        let hub = Arc::new(Hub::new(self.counter.clone()));
        hubs.insert(chat.clone(), hub.clone());
        hub
    }

    /// The hub of `chat` if it has one, without making one.
    pub(super) fn find(&self, chat: &ConversationId) -> Option<Arc<Hub>> {
        let hubs = lock(&self.hubs);
        let hub = hubs.get(chat)?;
        hub.touch();
        Some(hub.clone())
    }

    /// The cursor naming event `seq` of this run: `<epoch>-<seq>`. Also the SSE event id, so a
    /// reconnecting `EventSource` resumes with `Last-Event-ID`.
    pub(super) fn cursor(&self, seq: u64) -> String {
        format!("{}-{seq}", self.epoch)
    }

    /// Where a stream given `cursor` starts.
    pub(super) fn after(&self, cursor: Option<&str>) -> After {
        let Some(cursor) = cursor.filter(|c| !c.is_empty()) else {
            return After::Now;
        };
        cursor
            .split_once('-')
            .filter(|(epoch, _)| *epoch == self.epoch)
            .and_then(|(_, seq)| seq.parse().ok())
            .map_or(After::Stale, After::Seq)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        lock(&self.hubs).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn htmls(events: &[Arc<Event>]) -> Vec<&str> {
        events.iter().map(|e| e.html.as_str()).collect()
    }

    fn hub() -> Hub {
        Hub::new(Arc::default())
    }

    fn chat(n: u8) -> ConversationId {
        ConversationId::new(format!("{n:032x}")).unwrap()
    }

    fn approval(code: &str) -> Approval {
        Approval {
            code: code.into(),
            prompt: "<p>ok?</p>".into(),
            sent: None,
        }
    }

    #[test]
    fn cursors_resume_this_run_only() {
        let hubs = Hubs::new();
        assert_eq!(hubs.after(None), After::Now);
        assert_eq!(hubs.after(Some("")), After::Now);
        assert_eq!(hubs.after(Some(&hubs.cursor(7))), After::Seq(7));
        let other = Hubs::new();
        assert_eq!(hubs.after(Some(&other.cursor(7))), After::Stale);
        assert_eq!(hubs.after(Some("garbage")), After::Stale);
        assert_eq!(hubs.after(Some(&format!("{}-x", hubs.epoch))), After::Stale);
    }

    #[tokio::test]
    async fn a_subscriber_gets_every_event_after_its_cursor_once() {
        let hub = hub();
        hub.append("<p>1</p>".into());
        hub.append("<p>2</p>".into());
        let page = hub.snapshot();
        assert_eq!(page.seq, 2);
        // Published between the page render and the stream opening.
        hub.append("<p>3</p>".into());

        let mut sub = hub.subscribe(After::Seq(page.seq));
        assert!(!sub.gap);
        assert_eq!(htmls(&sub.replay), ["<p>3</p>"]);
        assert_eq!(sub.seq, 3);
        hub.append("<p>4</p>".into());
        let next = sub.feed.try_recv().unwrap();
        assert_eq!((next.seq, next.html.as_str()), (4, "<p>4</p>"));
        assert!(sub.feed.try_recv().is_err());

        let fresh = hub.subscribe(After::Now);
        assert!(fresh.replay.is_empty() && !fresh.gap);
    }

    #[tokio::test]
    async fn a_cursor_older_than_the_ring_or_from_elsewhere_is_a_gap() {
        let hub = hub();
        for i in 0..RING + 5 {
            hub.append(format!("<p>{i}</p>"));
        }
        let behind = hub.subscribe(After::Seq(2));
        assert!(behind.gap);
        assert_eq!(behind.replay.len(), RING);
        // Exactly at the edge: the oldest kept event is the next one.
        let edge = hub.subscribe(After::Seq(5));
        assert!(!edge.gap);
        assert_eq!(edge.replay.len(), RING);
        assert!(hub.subscribe(After::Seq(10_000)).gap);
        assert!(hub.subscribe(After::Stale).gap);
        // An empty chat: a cursor at its floor is current.
        assert!(!self::hub().subscribe(After::Seq(0)).gap);
    }

    #[tokio::test]
    async fn live_messages_and_prompts_are_in_the_snapshot_until_they_end() {
        let hub = hub();
        hub.live_start("m-1", r#"<article id="m-1" class="msg">a</article>"#.into());
        hub.live_update(
            "m-1",
            r#"<article id="m-1" class="msg">ab</article>"#.into(),
            "ab",
        );
        hub.approval_add(approval("4821"), "<form>".into());
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.live.len(), 1);
        assert_eq!(
            snapshot.live[0].html,
            r#"<article id="m-1" class="msg">ab</article>"#
        );
        assert_eq!(snapshot.live[0].text, "ab");
        assert_eq!(snapshot.approvals, [approval("4821")]);

        let mut sub = hub.subscribe(After::Seq(0));
        let replay = htmls(&sub.replay);
        assert_eq!(replay[0], r#"<article id="m-1" class="msg">a</article>"#);
        assert_eq!(
            replay[1],
            r#"<article hx-swap-oob="true" id="m-1" class="msg">ab</article>"#
        );

        hub.live_end("m-1", r#"<article id="m-1" class="msg">abc</article>"#);
        // A button answer keeps the prompt (without buttons) until it's resolved.
        assert!(
            hub.approval_sent("4821", "Sent: yes".into(), |prompt, label| {
                format!("<div id=\"a\">{prompt}{label}</div>")
            })
        );
        assert_eq!(
            hub.snapshot().approvals[0].sent.as_deref(),
            Some("Sent: yes")
        );
        assert!(hub.approval_resolve("4821", |prompt| format!(
            "<div id=\"a\">{prompt}Allowed.</div>"
        )));
        assert!(!hub.approval_resolve("4821", |_| unreachable!()));
        assert!(!hub.approval_sent("4821", String::new(), |_, _| unreachable!()));
        let snapshot = hub.snapshot();
        assert!(snapshot.live.is_empty() && snapshot.approvals.is_empty());
        assert_eq!(
            sub.feed.try_recv().unwrap().html,
            r#"<article hx-swap-oob="true" id="m-1" class="msg">abc</article>"#
        );
        assert_eq!(
            sub.feed.try_recv().unwrap().html,
            r#"<div hx-swap-oob="true" id="a"><p>ok?</p>Sent: yes</div>"#
        );
        assert_eq!(
            sub.feed.try_recv().unwrap().html,
            r#"<div hx-swap-oob="true" id="a"><p>ok?</p>Allowed.</div>"#
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_hubs_are_dropped_and_their_pages_told_to_reload() {
        let hubs = Hubs::new();
        let quiet = hubs.get(&chat(1));
        quiet.append("<p>1</p>".into());
        let cursor = quiet.snapshot().seq;
        drop(quiet);

        // Kept: an open stream, a live message, an unanswered prompt, or an `Arc` held.
        let open = hubs.get(&chat(2));
        let stream = open.subscribe(After::Now);
        drop(open);
        hubs.get(&chat(3)).live_start("m-1", "<p>".into());
        hubs.get(&chat(4))
            .approval_add(approval("1"), "<form>".into());
        let held = hubs.get(&chat(5));

        tokio::time::advance(HUB_IDLE - Duration::from_secs(1)).await;
        hubs.get(&chat(6));
        assert_eq!(hubs.len(), 6, "nothing is idle yet");

        tokio::time::advance(Duration::from_secs(1)).await;
        hubs.get(&chat(7));
        assert_eq!(hubs.len(), 6, "chat 1 dropped, 7 added");
        assert!(hubs.find(&chat(1)).is_none());
        assert!(hubs.find(&chat(2)).is_some() && hubs.find(&chat(5)).is_some());

        // Made again: the old page's cursor is behind the new hub's floor.
        let again = hubs.get(&chat(1));
        assert!(again.subscribe(After::Seq(cursor)).gap);
        let fresh = again.snapshot().seq;
        assert!(fresh > cursor);
        assert!(!again.subscribe(After::Seq(fresh)).gap);
        drop((stream, held));

        // A sent (but unresolved) prompt doesn't keep a hub.
        let answered = hubs.get(&chat(4));
        assert!(answered.approval_sent("1", "Sent: no".into(), |p, l| format!("{p}{l}")));
        drop(answered);
        tokio::time::advance(HUB_IDLE).await;
        hubs.get(&chat(8));
        assert!(hubs.find(&chat(4)).is_none());
        assert!(hubs.find(&chat(3)).is_some(), "still live");
    }
}
