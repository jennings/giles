//! A server-sent events parser for streaming model APIs.
//!
//! [`SseParser`] follows the event stream interpretation rules of the WHATWG HTML standard
//! (section 9.2.6): lines end with CRLF, LF, or CR; a line starting with `:` is a comment; a field
//! name is everything before the first `:`, and one space after the colon is dropped; `data`
//! lines accumulate, joined with `\n`; a blank line dispatches the event; an event with no `data`
//! is not dispatched; an unfinished event at the end of the stream is discarded. A leading UTF-8
//! byte order mark is skipped. Lines are decoded as UTF-8 with replacement characters, which is
//! safe per line because a line terminator never occurs inside a multi-byte sequence.
//!
//! [`events`] adapts a byte stream (an HTTP response body) into a stream of [`SseEvent`]s.

use std::collections::VecDeque;
use std::fmt;

use futures::{Stream, StreamExt};

/// The largest event, in bytes of buffered lines, that the parser accepts before failing. It bounds
/// memory if a server never ends a line or an event.
pub const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event` field, or `message` if the event had none.
    pub event: String,
    /// The `data` lines, joined with `\n`.
    pub data: String,
    /// The last `id` field seen so far in the stream, if any.
    pub id: Option<String>,
}

/// An event stream that can't be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseError {
    /// An event grew past [`MAX_EVENT_BYTES`].
    TooLarge,
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "server-sent event larger than {MAX_EVENT_BYTES} bytes"),
        }
    }
}

impl std::error::Error for SseError {}

impl From<SseError> for giles_types::Error {
    fn from(error: SseError) -> Self {
        giles_types::Error::other(error)
    }
}

/// An incremental event stream parser: feed it chunks as they arrive with [`SseParser::push`].
#[derive(Debug, Default)]
pub struct SseParser {
    /// Bytes of the current, unterminated line.
    line: Vec<u8>,
    /// The previous chunk ended with CR, so a LF starting the next chunk ends nothing.
    after_cr: bool,
    /// Whether the start of the stream (and a possible byte order mark) is behind us.
    started: bool,
    event: String,
    data: String,
    /// Whether any `data` field arrived for the current event.
    has_data: bool,
    last_id: Option<String>,
    /// Bytes buffered for the current event, checked against [`MAX_EVENT_BYTES`].
    pending: usize,
}

impl SseParser {
    /// A parser at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses `chunk` and returns the events it completed, in order.
    pub fn push(&mut self, mut chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        let mut events = Vec::new();
        if !self.started {
            // Wait for three bytes before deciding whether the stream starts with a BOM.
            const BOM: &[u8] = b"\xEF\xBB\xBF";
            self.line.extend_from_slice(chunk);
            if self.line.len() < BOM.len() && BOM.starts_with(&self.line) {
                return Ok(events);
            }
            self.started = true;
            let buffered = std::mem::take(&mut self.line);
            let rest = buffered.strip_prefix(BOM).unwrap_or(&buffered);
            self.feed(rest, &mut events)?;
            return Ok(events);
        }
        if self.after_cr && !chunk.is_empty() {
            self.after_cr = false;
            if let Some(rest) = chunk.strip_prefix(b"\n") {
                chunk = rest;
            }
        }
        self.feed(chunk, &mut events)?;
        Ok(events)
    }

    fn feed(&mut self, mut chunk: &[u8], events: &mut Vec<SseEvent>) -> Result<(), SseError> {
        while let Some(end) = chunk.iter().position(|&b| b == b'\n' || b == b'\r') {
            self.line.extend_from_slice(&chunk[..end]);
            let line = std::mem::take(&mut self.line);
            self.process_line(&line, events)?;
            let terminator_len = if chunk[end] == b'\r' {
                match chunk.get(end + 1) {
                    Some(b'\n') => 2,
                    Some(_) => 1,
                    None => {
                        self.after_cr = true;
                        1
                    }
                }
            } else {
                1
            };
            chunk = &chunk[end + terminator_len..];
        }
        self.line.extend_from_slice(chunk);
        if self.pending + self.line.len() > MAX_EVENT_BYTES {
            return Err(SseError::TooLarge);
        }
        Ok(())
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) -> Result<(), SseError> {
        if line.is_empty() {
            self.dispatch(events);
            return Ok(());
        }
        self.pending += line.len();
        if self.pending > MAX_EVENT_BYTES {
            return Err(SseError::TooLarge);
        }
        if line[0] == b':' {
            return Ok(());
        }
        let line = String::from_utf8_lossy(line);
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => value.clone_into(&mut self.event),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "id" if !value.contains('\0') => self.last_id = Some(value.to_owned()),
            // `retry` only matters to clients that reconnect; model APIs aren't resumed.
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        self.pending = 0;
        if !std::mem::take(&mut self.has_data) {
            return;
        }
        events.push(SseEvent {
            event: if event.is_empty() {
                "message".to_owned()
            } else {
                event
            },
            data,
            id: self.last_id.clone(),
        });
    }
}

/// Parses a byte stream (e.g. an HTTP response body) into events.
///
/// Errors from `body` pass through; a parse error ends the stream after yielding it, as does the
/// first error from `body`. An unfinished event when `body` ends is discarded.
pub fn events<S, B, E>(body: S) -> impl Stream<Item = Result<SseEvent, E>> + Send
where
    S: Stream<Item = Result<B, E>> + Send,
    B: AsRef<[u8]>,
    E: From<SseError> + Send,
{
    struct State<S, E> {
        body: S,
        parser: SseParser,
        ready: VecDeque<Result<SseEvent, E>>,
        done: bool,
    }
    let state = State {
        body: Box::pin(body),
        parser: SseParser::new(),
        ready: VecDeque::new(),
        done: false,
    };
    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(item) = state.ready.pop_front() {
                return Some((item, state));
            }
            if state.done {
                return None;
            }
            match state.body.next().await {
                Some(Ok(chunk)) => match state.parser.push(chunk.as_ref()) {
                    Ok(events) => state.ready.extend(events.into_iter().map(Ok)),
                    Err(error) => {
                        state.done = true;
                        state.ready.push_back(Err(error.into()));
                    }
                },
                Some(Err(error)) => {
                    state.done = true;
                    state.ready.push_back(Err(error));
                }
                None => state.done = true,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(event: &str, data: &str) -> SseEvent {
        SseEvent {
            event: event.into(),
            data: data.into(),
            id: None,
        }
    }

    fn parse_all(input: &[u8]) -> Vec<SseEvent> {
        SseParser::new().push(input).unwrap()
    }

    /// Feeds `input` split at every possible point, and one byte at a time, and checks that the
    /// events never depend on how the stream was chunked.
    fn assert_chunking_invariant(input: &[u8], expected: &[SseEvent]) {
        assert_eq!(parse_all(input), expected, "whole input");
        for split in 0..=input.len() {
            let mut parser = SseParser::new();
            let mut events = parser.push(&input[..split]).unwrap();
            // An empty chunk (an empty HTTP body frame) changes nothing, even right after a CR.
            events.extend(parser.push(&[]).unwrap());
            events.extend(parser.push(&input[split..]).unwrap());
            assert_eq!(events, expected, "split at {split}");
        }
        let mut parser = SseParser::new();
        let events: Vec<_> = input
            .iter()
            .flat_map(|byte| parser.push(std::slice::from_ref(byte)).unwrap())
            .collect();
        assert_eq!(events, expected, "byte by byte");
    }

    #[test]
    fn named_events_with_json_data() {
        let input = b"event: message_start\ndata: {\"a\":1}\n\nevent: ping\ndata: {}\n\n";
        assert_chunking_invariant(
            input,
            &[event("message_start", "{\"a\":1}"), event("ping", "{}")],
        );
    }

    #[test]
    fn line_endings_crlf_cr_and_lf_are_equivalent() {
        let expected = [event("a", "1"), event("b", "2")];
        assert_chunking_invariant(
            b"event: a\r\ndata: 1\r\n\r\nevent: b\r\ndata: 2\r\n\r\n",
            &expected,
        );
        assert_chunking_invariant(b"event: a\rdata: 1\r\revent: b\rdata: 2\r\r", &expected);
        assert_chunking_invariant(b"event: a\r\ndata: 1\n\revent: b\rdata: 2\r\n\n", &expected);
    }

    #[test]
    fn multi_line_data_is_joined_with_newlines() {
        assert_chunking_invariant(
            b"data: one\ndata:two\ndata:  three\ndata\n\n",
            &[event("message", "one\ntwo\n three\n")],
        );
    }

    #[test]
    fn comments_unknown_fields_and_retry_are_ignored() {
        assert_chunking_invariant(
            b": keep-alive\nretry: 1000\nfoo: bar\nevent: x\ndata: y\n\n",
            &[event("x", "y")],
        );
    }

    #[test]
    fn events_without_data_are_not_dispatched_and_reset_the_type() {
        assert_chunking_invariant(
            b"event: lonely\n\ndata: next\n\n: only a comment\n\n",
            &[event("message", "next")],
        );
    }

    #[test]
    fn empty_data_line_dispatches_an_empty_event() {
        assert_chunking_invariant(b"data:\n\n", &[event("message", "")]);
    }

    #[test]
    fn unterminated_event_at_end_is_not_dispatched() {
        assert_chunking_invariant(
            b"data: done\n\ndata: partial\n",
            &[event("message", "done")],
        );
        assert_chunking_invariant(b"data: done\n\ndata: partial", &[event("message", "done")]);
    }

    #[test]
    fn leading_byte_order_mark_is_skipped_once() {
        assert_chunking_invariant(b"\xEF\xBB\xBFdata: a\n\n", &[event("message", "a")]);
        // Only at the very start: elsewhere it is part of the field name, an unknown field.
        assert_chunking_invariant(
            b"data: a\n\n\xEF\xBB\xBFdata: b\n\n",
            &[event("message", "a")],
        );
        // A stream shorter than a BOM that isn't one still parses.
        assert_chunking_invariant(b"\n", &[]);
    }

    #[test]
    fn id_persists_across_events_and_rejects_nul() {
        let input = b"id: 7\ndata: a\n\ndata: b\n\nid: x\0y\ndata: c\n\nid\ndata: d\n\n";
        let with_id = |data: &str, id: Option<&str>| SseEvent {
            event: "message".into(),
            data: data.into(),
            id: id.map(Into::into),
        };
        assert_chunking_invariant(
            input,
            &[
                with_id("a", Some("7")),
                with_id("b", Some("7")),
                with_id("c", Some("7")),
                with_id("d", Some("")),
            ],
        );
    }

    #[test]
    fn multi_byte_utf8_split_across_chunks() {
        assert_chunking_invariant(
            "data: héllo ✓\n\n".as_bytes(),
            &[event("message", "héllo ✓")],
        );
        assert_eq!(parse_all(b"data: \xFF\n\n"), [event("message", "\u{FFFD}")]);
    }

    #[test]
    fn oversized_line_or_event_fails() {
        let mut parser = SseParser::new();
        let big = vec![b'a'; MAX_EVENT_BYTES + 1];
        assert_eq!(parser.push(&big), Err(SseError::TooLarge));

        let mut parser = SseParser::new();
        let half = vec![b'a'; MAX_EVENT_BYTES / 2];
        let mut line = b"data: ".to_vec();
        line.extend_from_slice(&half);
        line.push(b'\n');
        assert!(parser.push(&line).unwrap().is_empty());
        assert_eq!(parser.push(&line), Err(SseError::TooLarge));
    }

    #[test]
    fn event_size_limit_resets_per_event() {
        let mut parser = SseParser::new();
        let mut line = b"data: ".to_vec();
        line.extend_from_slice(&vec![b'a'; MAX_EVENT_BYTES / 2]);
        line.extend_from_slice(b"\n\n");
        for _ in 0..3 {
            assert_eq!(parser.push(&line).unwrap().len(), 1);
        }
    }

    #[derive(Debug, PartialEq)]
    enum TestError {
        Body(&'static str),
        Sse(SseError),
    }

    impl From<SseError> for TestError {
        fn from(error: SseError) -> Self {
            Self::Sse(error)
        }
    }

    #[tokio::test]
    async fn stream_adapter_yields_events_then_passes_body_errors_through() {
        let chunks: Vec<Result<&[u8], TestError>> = vec![
            Ok(b"event: a\ndata: 1\n\nevent: b\nda"),
            Ok(b"ta: 2\n\n"),
            Err(TestError::Body("reset")),
            Ok(b"event: c\ndata: 3\n\n"),
        ];
        let items: Vec<_> = events(futures::stream::iter(chunks)).collect().await;
        assert_eq!(
            items,
            [
                Ok(event("a", "1")),
                Ok(event("b", "2")),
                Err(TestError::Body("reset")),
            ]
        );
    }

    #[tokio::test]
    async fn stream_adapter_ends_after_a_parse_error() {
        let big = vec![b'a'; MAX_EVENT_BYTES + 1];
        let chunks: Vec<Result<Vec<u8>, TestError>> = vec![
            Ok(b"data: ok\n\n".to_vec()),
            Ok(big),
            Ok(b"data: no\n\n".to_vec()),
        ];
        let items: Vec<_> = events(futures::stream::iter(chunks)).collect().await;
        assert_eq!(
            items,
            [
                Ok(event("message", "ok")),
                Err(TestError::Sse(SseError::TooLarge))
            ]
        );
    }
}
