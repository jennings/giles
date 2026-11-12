//! HTML fragments of the chat pages: messages, tool calls, attachments, approval prompts, the
//! compose form, and the transcript history. Model output always goes through the sanitizing
//! CommonMark renderer with only local images allowed (PLAN section 9); tool input and output are
//! shown as escaped plain text.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use askama::Template;
use giles_plugin_sdk::render::{html, is_local_image};
use giles_store::{ConversationMessage, Entry, InboxEntry, SessionId, StoredMessageId};
use giles_types::{Block, ChannelEvent, EventId, InboundMessage, MediaRef, Role, Timestamp};
use serde_json::Value;
use tracing::error;

use super::hub::Live;
use crate::agent_loop::history;
use crate::core_tools::SEND_ATTACHMENT;
use crate::media::{self, SessionMedia};

/// How many of a tool's last output lines a tool call shows unfolded.
pub(super) const TOOL_OUTPUT_LINES: usize = 6;

/// Media types `/blobs/<id>` shows inline (and the page shows as `<img>`): raster images only, the
/// same set the renderer accepts as `data:` images. Everything else is a download.
pub(super) const INLINE_IMAGE_TYPES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
];

/// Whether `media_type` is an image the page shows inline.
pub(super) fn is_inline_image(media_type: &str) -> bool {
    let essence = media_type.split(';').next().unwrap_or_default().trim();
    INLINE_IMAGE_TYPES
        .iter()
        .any(|t| t.eq_ignore_ascii_case(essence))
}

/// Renders a fragment template; a failure (which askama templates without fallible filters
/// don't have) is logged and renders as nothing.
fn fragment<T: Template>(template: &T) -> String {
    match template.render() {
        Ok(html) => html.trim().to_owned(),
        Err(err) => {
            error!(error = %err, "cannot render a chat fragment");
            String::new()
        }
    }
}

/// `markdown` as sanitized HTML, with only blob-store and raster `data:` images kept.
pub(super) fn markdown(markdown: &str) -> String {
    html(markdown, is_local_image)
}

/// Whose message it is, as a CSS class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Who {
    /// The user.
    User,
    /// The agent (replies and notices).
    Assistant,
}

impl Who {
    fn class(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Template)]
#[template(path = "web/message.html")]
struct MessageFragment<'a> {
    id: &'a str,
    role: &'static str,
    body: &'a str,
    live: bool,
    cut_off: bool,
}

/// How a message is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum State {
    /// Complete.
    Done,
    /// Still being written.
    Live,
    /// Ended before the model finished.
    CutOff,
}

/// A message element with DOM id `id` and the already-rendered `body`.
pub(super) fn message(id: &str, who: Who, body: &str, state: State) -> String {
    fragment(&MessageFragment {
        id,
        role: who.class(),
        body,
        live: state == State::Live,
        cut_off: state == State::CutOff,
    })
}

struct AttachmentItem<'a> {
    id: &'a str,
    name: &'a str,
    image: bool,
}

#[derive(Template)]
#[template(path = "web/attachments.html")]
struct AttachmentsFragment<'a> {
    items: Vec<AttachmentItem<'a>>,
}

/// Images inline, anything else as a link, all served by `/blobs/<id>`.
pub(super) fn attachments(media: &[&MediaRef]) -> String {
    let items = media
        .iter()
        .map(|m| AttachmentItem {
            id: m.id.as_str(),
            name: m.name.as_deref().unwrap_or(m.media_type.as_str()),
            image: is_inline_image(&m.media_type),
        })
        .collect();
    fragment(&AttachmentsFragment { items })
}

/// A message body: the rendered text, then the attachments.
pub(super) fn body(text: &str, media: &[&MediaRef]) -> String {
    let mut out = markdown(text);
    if !media.is_empty() {
        out.push_str(&attachments(media));
    }
    out
}

#[derive(Template)]
#[template(path = "web/approval.html")]
struct ApprovalFragment<'a> {
    chat: &'a str,
    code: &'a str,
    prompt: &'a str,
    csrf: Option<&'a str>,
    answer: Option<&'a str>,
}

/// An approval prompt with its buttons (`prompt` already rendered). `csrf` adds the form field
/// for browsers without JavaScript; fragments sent over the event stream have none and rely on
/// the page's `hx-headers`.
pub(super) fn approval(chat: &str, code: &str, prompt: &str, csrf: Option<&str>) -> String {
    fragment(&ApprovalFragment {
        chat,
        code,
        prompt,
        csrf,
        answer: None,
    })
}

/// An answered approval prompt, replacing its form.
pub(super) fn approval_answered(code: &str, prompt: &str, answer: &str) -> String {
    fragment(&ApprovalFragment {
        chat: "",
        code,
        prompt,
        csrf: None,
        answer: Some(answer),
    })
}

#[derive(Template)]
#[template(path = "web/compose.html")]
struct ComposeFragment<'a> {
    chat: &'a str,
    csrf: &'a str,
    text: &'a str,
    error: Option<&'a str>,
}

/// The message form of chat `chat`, with `text` kept and an error shown after a failed post.
pub(super) fn compose(chat: &str, csrf: &str, text: &str, error: Option<&str>) -> String {
    fragment(&ComposeFragment {
        chat,
        csrf,
        text,
        error,
    })
}

/// The page notice that events were missed.
pub(super) fn reload_notice() -> String {
    r#"<p id="chat-notice" class="notice" hx-swap-oob="true">Some messages may be missing. <a href="">Reload the page</a> to see them.</p>"#
        .to_owned()
}

/// `fragment` (one element) with `hx-swap-oob="true"` added to its start tag, so the page
/// replaces the element with the same id instead of appending it.
pub(super) fn out_of_band(fragment: &str) -> String {
    let tag_end = fragment.find([' ', '>']).unwrap_or(fragment.len());
    let (tag, rest) = fragment.split_at(tag_end);
    format!(r#"{tag} hx-swap-oob="true"{rest}"#)
}

/// The DOM id of the tool call whose result is transcript row `slot`, the same live and in the
/// history.
pub(super) fn tool_element(slot: impl std::fmt::Display) -> String {
    format!("t-{slot}")
}

/// A tool call's input as text: an object's fields one per line (`key: value`, a string value
/// as is, anything else as JSON), any other value as JSON. Empty for no arguments.
pub(super) fn tool_input(input: &Value) -> String {
    let mut out = String::new();
    match input {
        Value::Object(fields) => {
            for (key, value) in fields {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(key);
                out.push_str(": ");
                match value {
                    Value::String(text) => out.push_str(text),
                    other => {
                        let _ = write!(out, "{other}");
                    }
                }
            }
        }
        Value::Null => {}
        Value::String(text) => out.push_str(text),
        other => {
            let _ = write!(out, "{other}");
        }
    }
    out
}

/// Where a tool call is.
#[derive(Debug, Clone, Copy)]
pub(super) enum ToolState<'a> {
    /// Running: no result yet.
    Running,
    /// The turn stopped before the result.
    Stopped,
    /// Its result: the `ToolResult` block's content.
    Done {
        content: &'a [Block],
        is_error: bool,
    },
}

/// Text shown in part: `shown`, and `folded` behind a disclosure labeled `label`.
struct Folded<'a> {
    shown: &'a str,
    folded: Option<&'a str>,
    label: String,
}

impl<'a> Folded<'a> {
    fn whole(text: &'a str) -> Self {
        Self {
            shown: text,
            folded: None,
            label: String::new(),
        }
    }

    /// The first line shown, the rest folded.
    fn first_line(text: &'a str) -> Self {
        let Some((first, rest)) = text.split_once('\n') else {
            return Self::whole(text);
        };
        Self {
            shown: first.strip_suffix('\r').unwrap_or(first),
            folded: Some(rest),
            label: lines_label(rest.lines().count(), "more"),
        }
    }

    /// The last `n` lines shown (`n` > 0), the earlier ones folded.
    fn last_lines(text: &'a str, n: usize) -> Self {
        let Some((at, _)) = text.rmatch_indices('\n').nth(n - 1) else {
            return Self::whole(text);
        };
        let earlier = text[..at].strip_suffix('\r').unwrap_or(&text[..at]);
        Self {
            shown: &text[at + 1..],
            folded: Some(earlier),
            label: lines_label(earlier.lines().count(), "earlier"),
        }
    }
}

/// "Show 1 more line", "Show 3 earlier lines".
fn lines_label(count: usize, which: &str) -> String {
    let s = if count == 1 { "" } else { "s" };
    format!("Show {count} {which} line{s}")
}

#[derive(Template)]
#[template(path = "web/tool_call.html")]
struct ToolCallFragment<'a> {
    id: &'a str,
    name: &'a str,
    input: Option<Folded<'a>>,
    output: Option<Folded<'a>>,
    attachments: String,
    running: bool,
    error: bool,
    status: Option<&'static str>,
}

/// Tool call element `id` ([`tool_element`]): the tool's name, its `input` ([`tool_input`]) with
/// only the first line unfolded, and by `state` its output with only the last
/// [`TOOL_OUTPUT_LINES`] lines unfolded, then the media it returned.
pub(super) fn tool_call(id: &str, name: &str, input: &str, state: ToolState<'_>) -> String {
    let input = input.trim_end();
    let mut output = String::new();
    let mut media = Vec::new();
    let (running, error, status) = match state {
        ToolState::Running => (true, false, Some("Running…")),
        ToolState::Stopped => (false, false, Some("Stopped before the end.")),
        ToolState::Done { content, is_error } => {
            for (index, block) in content.iter().enumerate() {
                match block {
                    Block::Text { text } if is_reference_of(text, content.get(index + 1)) => {}
                    Block::Text { text } => {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(text);
                    }
                    Block::Image { media: m }
                    | Block::Audio { media: m }
                    | Block::Document { media: m, .. } => media.push(m),
                    _ => {}
                }
            }
            let empty = output.trim_end().is_empty() && media.is_empty();
            let status = if is_error {
                Some("Failed.")
            } else if empty {
                Some("No output.")
            } else {
                None
            };
            (false, is_error, status)
        }
    };
    let output = output.trim_end();
    fragment(&ToolCallFragment {
        id,
        name,
        input: (!input.is_empty()).then(|| Folded::first_line(input)),
        output: (!output.is_empty()).then(|| Folded::last_lines(output, TOOL_OUTPUT_LINES)),
        attachments: if media.is_empty() {
            String::new()
        } else {
            attachments(&media)
        },
        running,
        error,
        status,
    })
}

/// The text and media of a stored message as the chat shows it: text blocks joined by blank lines,
/// and images and documents. A user message's time prefix (`[Thu 2026-10-22 14:03 PDT] `, added
/// for the model by the agent loop) is left out, and so is each media reference stored before its
/// media (`[media:<id> <name> <type>]`, [`media::attachment`]): the page shows the media itself.
/// A `send_attachment` call that succeeded shows what it sent: its caption and its media
/// ([`Sent`]). Tool calls and their results are shown apart ([`history`]); reasoning and provider
/// data never reach the page (PLAN section 6.9). `None` if nothing is left to show.
fn visible<'a>(
    session: SessionId,
    role: Role,
    content: &'a [Block],
    sent: &'a Sent<'a>,
) -> Option<(String, Vec<&'a MediaRef>)> {
    if content
        .iter()
        .any(|b| matches!(b, Block::ToolResult { .. }))
    {
        return None;
    }
    let mut texts = Vec::new();
    let mut media = Vec::new();
    for (index, block) in content.iter().enumerate() {
        match block {
            Block::Text { text } if is_reference_of(text, content.get(index + 1)) => {}
            Block::Text { text } if role == Role::User && texts.is_empty() => {
                texts.push(history::strip_time_prefix(text));
            }
            Block::Text { text } => texts.push(text.as_str()),
            Block::Image { media: m } | Block::Audio { media: m } => media.push(m),
            Block::Document { media: m, .. } => media.push(m),
            Block::ToolUse { .. } => {
                if let Some((caption, m)) = sent.attachment(session, block) {
                    texts.extend(caption);
                    media.push(m);
                }
            }
            _ => {}
        }
    }
    texts.retain(|text| !text.is_empty());
    if texts.is_empty() && media.is_empty() {
        return None;
    }
    Some((texts.join("\n\n"), media))
}

/// What `send_attachment` calls sent, for showing them in the transcript: the media the
/// conversation's messages hold, by id, and the calls whose result isn't an error, by session
/// and `ToolUse` id (ids are unique only within a session's provider responses).
#[derive(Default)]
struct Sent<'a> {
    library: SessionMedia,
    succeeded: HashSet<(SessionId, &'a str)>,
}

impl<'a> Sent<'a> {
    fn new(rows: &'a [ConversationMessage]) -> Self {
        let mut sent = Self::default();
        for row in rows {
            let Entry::Message(stored) = &row.message.entry else {
                continue;
            };
            sent.library.add(stored);
            for block in &stored.content {
                if let Block::ToolResult {
                    tool_use_id,
                    is_error: false,
                    ..
                } = block
                {
                    sent.succeeded.insert((row.session, tool_use_id.as_str()));
                }
            }
        }
        sent
    }

    /// The caption and media of `block` if it's a `send_attachment` call that succeeded.
    fn attachment(
        &'a self,
        session: SessionId,
        block: &'a Block,
    ) -> Option<(Option<&'a str>, &'a MediaRef)> {
        let Block::ToolUse { id, name, input } = block else {
            return None;
        };
        if name != SEND_ATTACHMENT || !self.succeeded.contains(&(session, id.as_str())) {
            return None;
        }
        let media = self.library.get(input.get("media")?.as_str()?)?;
        Some((input.get("caption").and_then(|c| c.as_str()), media))
    }
}

/// Whether `text` is the media reference of `next`, the block after it.
fn is_reference_of(text: &str, next: Option<&Block>) -> bool {
    match next {
        Some(
            Block::Image { media: m }
            | Block::Audio { media: m }
            | Block::Document { media: m, .. },
        ) => text == media::reference(m),
        _ => false,
    }
}

/// The tool-result rows of a transcript, by session and `ToolUse` id: the row, and its result
/// unless the slot is still empty.
type ToolResults<'a> =
    HashMap<(SessionId, &'a str), (StoredMessageId, Option<(&'a [Block], bool)>)>;

fn tool_results(rows: &[ConversationMessage]) -> ToolResults<'_> {
    let mut results = HashMap::new();
    for row in rows {
        match &row.message.entry {
            Entry::EmptySlot { tool_use_id } => {
                results.insert((row.session, tool_use_id.as_str()), (row.message.id, None));
            }
            Entry::Message(stored) => {
                for block in &stored.content {
                    if let Block::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = block
                    {
                        results.insert(
                            (row.session, tool_use_id.as_str()),
                            (row.message.id, Some((content.as_slice(), *is_error))),
                        );
                    }
                }
            }
        }
    }
    results
}

/// The stored transcript as message and tool call elements, with a marker where a new session
/// (`/new`, idle reset) begins. Each tool call follows its message's text, with its result (or
/// as running, while its slot is empty); calls whose result row isn't loaded are left out, and
/// so are the elements in `replayed`, which the page's event stream adds.
pub(super) fn history(rows: &[ConversationMessage], replayed: &[String]) -> Vec<String> {
    let sent = Sent::new(rows);
    let results = tool_results(rows);
    let mut items = Vec::new();
    let mut session = None;
    for row in rows {
        let Entry::Message(stored) = &row.message.entry else {
            continue;
        };
        let shown = visible(row.session, stored.role, &stored.content, &sent);
        let calls: Vec<String> = stored
            .content
            .iter()
            .filter_map(|block| {
                let Block::ToolUse { id, name, input } = block else {
                    return None;
                };
                let (slot, result) = results.get(&(row.session, id.as_str()))?;
                let element = tool_element(slot);
                if replayed.contains(&element) {
                    return None;
                }
                let state = match result {
                    Some((content, is_error)) => ToolState::Done {
                        content,
                        is_error: *is_error,
                    },
                    None => ToolState::Running,
                };
                Some(tool_call(&element, name, &tool_input(input), state))
            })
            .collect();
        if shown.is_none() && calls.is_empty() {
            continue;
        }
        if session.is_some_and(|s| s != row.session) {
            items.push(r#"<p class="session-break">New session</p>"#.to_owned());
        }
        session = Some(row.session);
        if let Some((text, media)) = shown {
            let who = match stored.role {
                Role::User => Who::User,
                _ => Who::Assistant,
            };
            let id = format!("m-h{}", row.message.id);
            items.push(message(&id, who, &body(&text, &media), State::Done));
        }
        items.extend(calls);
    }
    items
}

/// A message the user posted, as every page shows it until it's in the transcript.
pub(super) fn posted(id: &EventId, text: &str) -> String {
    message(&format!("m-{id}"), Who::User, &markdown(text), State::Done)
}

/// The user's messages a chat page shows besides the transcript, as [`posted`] elements, oldest
/// first: inbox rows `waiting` for a turn (or in one, until it stores them), then posts the core
/// hasn't taken (`unacked`). Left out: the ones the page's event stream will replay
/// (`replayed`), and the ones `rows` already has (same text without the time prefix, stored at
/// the row's receive time, or for a post, at or after it was sent).
pub(super) fn waiting(
    rows: &[ConversationMessage],
    waiting: &[InboxEntry],
    unacked: Vec<(EventId, InboundMessage)>,
    replayed: &[EventId],
) -> Vec<String> {
    let stored = |text: &str, at: &dyn Fn(Timestamp) -> bool| {
        rows.iter().any(|row| match &row.message.entry {
            Entry::Message(m) => {
                m.role == Role::User
                    && at(m.meta.at)
                    && m.content.iter().find_map(|b| match b {
                        Block::Text { text } => Some(history::strip_time_prefix(text)),
                        _ => None,
                    }) == Some(text)
            }
            Entry::EmptySlot { .. } => false,
        })
    };
    let mut seen: Vec<EventId> = replayed.to_vec();
    let mut items = Vec::new();
    for entry in waiting {
        let Ok(ChannelEvent::Message { id, message }) =
            serde_json::from_value::<ChannelEvent>(entry.payload.clone())
        else {
            continue;
        };
        if !seen.contains(&id) && !stored(&message.text, &|at| at == entry.received_at) {
            items.push(posted(&id, &message.text));
        }
        seen.push(id);
    }
    for (id, message) in unacked {
        // The store keeps milliseconds.
        let sent = message.sent_at.map(|sent| sent.as_millisecond());
        let is_stored = stored(&message.text, &|at| {
            sent.is_some_and(|sent| at.as_millisecond() >= sent)
        });
        if !seen.contains(&id) && !is_stored {
            items.push(posted(&id, &message.text));
        }
    }
    items
}

/// Whether the transcript already has live message `live`: an assistant message stored after it
/// started holds its text so far. Then the page shows the stored one only (the live element's
/// later updates find no element and are ignored).
pub(super) fn is_stored(live: &Live, rows: &[ConversationMessage]) -> bool {
    rows.iter().any(|row| match &row.message.entry {
        Entry::Message(m) => {
            m.role == Role::Assistant
                && m.meta.at.as_millisecond() >= live.started_at.as_millisecond()
                && visible(row.session, m.role, &m.content, &Sent::default())
                    .is_some_and(|(text, _)| text.contains(&live.text))
        }
        Entry::EmptySlot { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use giles_store::{SessionId, StoredMessage, StoredMessageId};
    use giles_types::{MediaId, Message, MessageMeta, Timestamp};

    use super::*;

    fn media(media_type: &str, name: Option<&str>) -> MediaRef {
        MediaRef {
            id: MediaId::from_digest([0xab; 32]),
            media_type: media_type.into(),
            size: 3,
            name: name.map(str::to_owned),
        }
    }

    #[test]
    fn model_output_is_sanitized_and_remote_images_dropped() {
        let body = markdown("<script>x</script>");
        assert!(!body.contains("<script"), "{body}");
        let body = markdown("look ![x](https://evil/?d=1) ![y](/blobs/zz)");
        assert!(!body.contains("<img"), "{body}");
        assert!(!body.contains("https://evil"), "{body}");
        let id = "a".repeat(64);
        let body = markdown(&format!("![ok](/blobs/{id})"));
        assert!(body.contains(&format!(r#"src="/blobs/{id}""#)), "{body}");
    }

    #[test]
    fn attachments_show_raster_images_inline_and_link_the_rest() {
        let png = media("image/png", Some("a.png"));
        let svg = media("image/svg+xml", Some("<b>.svg"));
        let pdf = media("application/pdf", None);
        let html = attachments(&[&png, &svg, &pdf]);
        let id = png.id.as_str();
        assert!(
            html.contains(&format!(r#"<img src="/blobs/{id}" alt="a.png">"#)),
            "{html}"
        );
        assert!(
            html.contains(&format!(r#"<a href="/blobs/{id}">&#60;b&#62;.svg</a>"#)),
            "{html}"
        );
        assert!(html.contains(">application/pdf</a>"), "{html}");
        assert!(is_inline_image("IMAGE/PNG; charset=x"));
        assert!(!is_inline_image("image/svg+xml"));
    }

    #[test]
    fn out_of_band_marks_the_start_tag_only() {
        assert_eq!(
            out_of_band(r#"<article id="m-1" class="msg">x <b>y</b></article>"#),
            r#"<article hx-swap-oob="true" id="m-1" class="msg">x <b>y</b></article>"#
        );
        assert_eq!(out_of_band("<p>x</p>"), r#"<p hx-swap-oob="true">x</p>"#);
    }

    #[test]
    fn messages_and_prompts_render_their_state() {
        let live = message("m-1", Who::Assistant, "<p>hi</p>", State::Live);
        assert!(
            live.starts_with(r#"<article id="m-1" class="msg assistant live">"#),
            "{live}"
        );
        assert!(live.contains("Writing"), "{live}");
        let cut = message("m-1", Who::Assistant, "", State::CutOff);
        assert!(cut.contains("cut-off") && cut.contains("Stopped"), "{cut}");

        let form = approval("c1", "4821", "<p>Allow?</p>", Some("tok"));
        assert!(form.starts_with(r#"<form id="a-4821""#), "{form}");
        assert!(form.contains(r#"hx-post="/chats/c1/approvals""#), "{form}");
        assert!(form.contains(r#"name="csrf" value="tok""#), "{form}");
        for value in ["yes", "yes!", "no"] {
            assert!(
                form.contains(&format!(r#"name="answer" value="{value}""#)),
                "{form}"
            );
        }
        assert!(!approval("c1", "4821", "", None).contains("csrf"));
        let done = approval_answered("4821", "<p>Allow?</p>", "Denied");
        assert!(done.starts_with(r#"<div id="a-4821" class="approval answered">"#));
        assert!(
            done.contains("Denied") && !done.contains("<button"),
            "{done}"
        );
    }

    fn row(session: i64, id: i64, role: Role, content: Vec<Block>) -> ConversationMessage {
        ConversationMessage {
            session: SessionId::new(session),
            message: StoredMessage {
                id: StoredMessageId::new(id),
                seq: id,
                entry: Entry::Message(Message {
                    role,
                    content,
                    meta: MessageMeta::new(Timestamp::UNIX_EPOCH),
                }),
            },
        }
    }

    fn text(t: &str) -> Block {
        Block::Text { text: t.into() }
    }

    /// The id attribute of each element (the class of a session break).
    fn ids(items: &[String]) -> Vec<&str> {
        items
            .iter()
            .map(|item| item.split('"').nth(1).unwrap())
            .collect()
    }

    #[test]
    fn history_shows_text_media_and_tool_calls_but_not_reasoning() {
        let rows = vec![
            row(1, 1, Role::User, vec![text("hello **there**")]),
            row(
                1,
                2,
                Role::Assistant,
                vec![
                    Block::Reasoning {
                        text: Some("secret thoughts".into()),
                        opaque: None,
                    },
                    Block::ToolUse {
                        id: "t1".into(),
                        name: "x".into(),
                        input: serde_json::json!({"cmd": "tool input"}),
                    },
                ],
            ),
            row(
                1,
                3,
                Role::User,
                vec![Block::ToolResult {
                    tool_use_id: "t1".into(),
                    content: vec![text("tool output")],
                    is_error: false,
                }],
            ),
            row(
                1,
                4,
                Role::Assistant,
                vec![
                    text("checking"),
                    Block::ToolUse {
                        id: "t2".into(),
                        name: "y".into(),
                        input: serde_json::json!({}),
                    },
                ],
            ),
            ConversationMessage {
                session: SessionId::new(1),
                message: StoredMessage {
                    id: StoredMessageId::new(5),
                    seq: 5,
                    entry: Entry::EmptySlot {
                        tool_use_id: "t2".into(),
                    },
                },
            },
            row(
                2,
                6,
                Role::User,
                vec![Block::Image {
                    media: media("image/png", None),
                }],
            ),
        ];
        let items = history(&rows, &[]);
        assert_eq!(
            ids(&items),
            ["m-h1", "t-3", "m-h4", "t-5", "session-break", "m-h6"]
        );
        assert!(items[0].contains("<strong>there</strong>"));
        assert!(
            items[1].contains("<pre>cmd: tool input</pre>") && items[1].contains("tool output"),
            "{}",
            items[1]
        );
        assert!(items[3].contains("Running"), "{}", items[3]);
        assert!(items[5].contains("<img src=\"/blobs/"), "{}", items[5]);
        assert!(!items.concat().contains("secret thoughts"));

        // A call the event stream will add is left out.
        let items = history(&rows, &["t-3".into()]);
        assert_eq!(
            ids(&items),
            ["m-h1", "m-h4", "t-5", "session-break", "m-h6"]
        );
    }

    #[test]
    fn a_tool_call_shows_the_first_input_line_and_the_last_output_lines() {
        let input = tool_input(&serde_json::json!({
            "timeout": 30,
            "command": "cargo test \\\n  --all\r\n<b>",
        }));
        assert_eq!(input, "command: cargo test \\\n  --all\r\n<b>\ntimeout: 30");
        let output: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        let html = tool_call(
            "t-1",
            "shell__exec",
            &input,
            ToolState::Done {
                content: &[text(&output)],
                is_error: false,
            },
        );
        assert!(
            html.starts_with(r#"<article id="t-1" class="tool">"#),
            "{html}"
        );
        assert!(html.contains("shell__exec"), "{html}");
        assert!(html.contains(r"<pre>command: cargo test \</pre>"), "{html}");
        assert!(
            html.contains(
                "<summary>Show 3 more lines</summary><pre>  --all\r\n&#60;b&#62;\ntimeout: 30</pre>"
            ),
            "{html}"
        );
        assert!(
            html.contains(
                "<summary>Show 4 earlier lines</summary><pre>line 1\nline 2\nline 3\nline 4</pre>"
            ),
            "{html}"
        );
        assert!(
            html.contains("<pre>line 5\nline 6\nline 7\nline 8\nline 9\nline 10</pre>"),
            "{html}"
        );
        assert!(!html.contains("msg-status"), "{html}");

        // Short enough: nothing folded.
        let html = tool_call(
            "t-1",
            "x",
            "ls",
            ToolState::Done {
                content: &[text("a\nb\nc\nd\ne\nf\n")],
                is_error: false,
            },
        );
        assert!(!html.contains("<details>"), "{html}");
        assert!(html.contains("<pre>a\nb\nc\nd\ne\nf</pre>"), "{html}");
    }

    #[test]
    fn a_tool_call_shows_its_state() {
        let running = tool_call("t-1", "x", "", ToolState::Running);
        assert!(
            running.starts_with(r#"<article id="t-1" class="tool live">"#),
            "{running}"
        );
        assert!(running.contains("Running"), "{running}");
        assert!(!running.contains("tool-input"), "no input: {running}");
        let stopped = tool_call("t-1", "x", "", ToolState::Stopped);
        assert!(stopped.contains("Stopped"), "{stopped}");
        let failed = tool_call(
            "t-1",
            "x",
            "",
            ToolState::Done {
                content: &[text("no such file")],
                is_error: true,
            },
        );
        assert!(failed.contains(r#"class="tool error""#), "{failed}");
        assert!(
            failed.contains("no such file") && failed.contains("Failed"),
            "{failed}"
        );
        let empty = tool_call(
            "t-1",
            "x",
            "",
            ToolState::Done {
                content: &[text("\n")],
                is_error: false,
            },
        );
        assert!(empty.contains("No output"), "{empty}");

        // Media it returned is shown, without its reference.
        let photo = media("image/png", Some("plot.png"));
        let mut content = vec![text("saved")];
        content.extend(crate::media::attachment(photo));
        let html = tool_call(
            "t-1",
            "x",
            "",
            ToolState::Done {
                content: &content,
                is_error: false,
            },
        );
        assert!(html.contains("<pre>saved</pre>"), "{html}");
        assert!(html.contains("<img"), "{html}");
        assert!(!html.contains("[media:"), "{html}");
    }

    #[test]
    fn history_leaves_out_the_time_prefix_of_user_messages_only() {
        let prefix = "[Thu 2026-10-22 14:03 PDT]";
        let rows = vec![
            row(1, 1, Role::User, vec![text(&format!("{prefix} hello"))]),
            // An attachment without text: the stored text is the prefix alone.
            row(
                1,
                2,
                Role::User,
                vec![
                    text(prefix),
                    Block::Image {
                        media: media("image/png", None),
                    },
                ],
            ),
            row(1, 3, Role::User, vec![text(prefix)]),
            row(
                1,
                4,
                Role::Assistant,
                vec![text(&format!("{prefix} quoted"))],
            ),
        ];
        let items = history(&rows, &[]);
        assert_eq!(items.len(), 3, "{items:#?}");
        assert!(items[0].contains("<p>hello</p>"), "{}", items[0]);
        assert!(!items[0].contains("PDT"), "{}", items[0]);
        assert!(
            !items[1].contains("PDT") && items[1].contains("<img"),
            "{}",
            items[1]
        );
        assert!(
            items[2].contains("PDT"),
            "assistant text is kept: {}",
            items[2]
        );
    }

    #[test]
    fn history_shows_attachments_without_their_references() {
        let photo = media("image/png", Some("cat.png"));
        let reference = crate::media::reference(&photo);
        let mut content = vec![text("[Thu 2026-10-22 14:03 PDT] look")];
        content.extend(crate::media::attachment(photo));
        let rows = vec![
            row(1, 1, Role::User, content),
            // A reference not followed by its media is text the model wrote.
            row(1, 2, Role::Assistant, vec![text(&reference)]),
        ];
        let items = history(&rows, &[]);
        assert_eq!(items.len(), 2, "{items:#?}");
        assert!(items[0].contains("<p>look</p>"), "{}", items[0]);
        assert!(items[0].contains("<img"), "{}", items[0]);
        assert!(!items[0].contains("[media:"), "{}", items[0]);
        assert!(items[1].contains("[media:"), "{}", items[1]);
    }

    #[test]
    fn history_shows_what_send_attachment_sent() {
        let photo = media("image/png", Some("cat.png"));
        let send = |id: &str| Block::ToolUse {
            id: id.into(),
            name: "send_attachment".into(),
            input: serde_json::json!({"media": photo.id.as_str(), "caption": "Your **cat**."}),
        };
        let result = |id: &str, is_error: bool| Block::ToolResult {
            tool_use_id: id.into(),
            content: vec![text("result")],
            is_error,
        };
        let mut upload = vec![text("[Thu 2026-10-22 14:03 PDT] keep this")];
        upload.extend(crate::media::attachment(photo.clone()));
        let rows = vec![
            row(1, 1, Role::User, upload),
            row(1, 2, Role::Assistant, vec![send("t1")]),
            row(1, 3, Role::User, vec![result("t1", false)]),
            // A failed send shows only the call.
            row(1, 4, Role::Assistant, vec![send("t2")]),
            row(1, 5, Role::User, vec![result("t2", true)]),
            // The same id in another session, failed there: the first session's success
            // doesn't count for it.
            row(2, 6, Role::Assistant, vec![send("t1")]),
            row(2, 7, Role::User, vec![result("t1", true)]),
        ];
        let items = history(&rows, &[]);
        assert_eq!(
            ids(&items),
            ["m-h1", "m-h2", "t-3", "t-5", "session-break", "t-7"]
        );
        assert!(items[1].contains("<strong>cat</strong>"), "{}", items[1]);
        assert!(items[1].contains("<img"), "{}", items[1]);
    }

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + secs).unwrap()
    }

    fn row_at(id: i64, role: Role, t: &str, secs: i64) -> ConversationMessage {
        let mut row = row(1, id, role, vec![text(t)]);
        if let Entry::Message(m) = &mut row.message.entry {
            m.meta.at = at(secs);
        }
        row
    }

    fn inbound(t: &str, sent: i64) -> InboundMessage {
        InboundMessage {
            conversation: giles_types::ConversationRef {
                id: giles_types::ConversationId::new("c").unwrap(),
                kind: giles_types::ConversationKind::Direct,
                title: None,
            },
            sender: giles_types::NativeId::new("owner").unwrap(),
            sender_aliases: Vec::new(),
            sender_authenticated: true,
            sender_display_name: None,
            text: t.into(),
            attachments: Vec::new(),
            message_id: None,
            reply_to: None,
            sent_at: Some(at(sent)),
        }
    }

    fn event_id(id: &str) -> EventId {
        EventId::new(id).unwrap()
    }

    fn inbox(id: &str, t: &str, received: i64) -> InboxEntry {
        let event = ChannelEvent::Message {
            id: event_id(id),
            message: inbound(t, received),
        };
        InboxEntry {
            id: giles_store::InboxId::new(1),
            instance: "web".into(),
            event_id: event_id(id),
            session: Some(SessionId::new(1)),
            user: None,
            payload: serde_json::to_value(&event).unwrap(),
            state: giles_store::InboxState::Queued,
            received_at: at(received),
        }
    }

    #[test]
    fn waiting_posts_show_once_until_stored() {
        let rows = vec![row_at(1, Role::User, "[Thu 2026-10-22 14:03 PDT] a", 10)];
        let waiting_rows = [
            // Started and already stored by its turn (same received time and text).
            inbox("e1", "a", 10),
            // Same text, received later: another message.
            inbox("e2", "a", 11),
            inbox("e3", "b", 12),
        ];
        let unacked = vec![
            (event_id("e3"), inbound("b", 12)),
            (event_id("e4"), inbound("c", 13)),
            // Stored at or after it was sent (acked and stored since).
            (event_id("e5"), inbound("a", 9)),
            (event_id("e6"), inbound("d", 14)),
        ];
        let items = waiting(&rows, &waiting_rows, unacked, &[event_id("e6")]);
        let ids: Vec<_> = items
            .iter()
            .map(|item| item.split('"').nth(1).unwrap())
            .collect();
        assert_eq!(ids, ["m-e2", "m-e3", "m-e4"]);
    }

    #[test]
    fn a_live_message_is_stored_once_a_later_assistant_message_has_its_text() {
        let live = Live {
            id: "m-x".into(),
            html: String::new(),
            text: "Hel".into(),
            started_at: at(5),
        };
        assert!(!is_stored(&live, &[row_at(1, Role::Assistant, "Hello", 4)]));
        assert!(!is_stored(&live, &[row_at(1, Role::User, "Hello", 6)]));
        assert!(!is_stored(&live, &[row_at(1, Role::Assistant, "Bye", 6)]));
        assert!(is_stored(
            &live,
            &[row_at(1, Role::Assistant, "Hello there", 6)]
        ));
    }
}
