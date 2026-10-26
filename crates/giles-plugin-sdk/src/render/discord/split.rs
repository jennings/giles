//! Splitting rendered Discord text into messages.
//!
//! The input is the renderer's own output, so the scan below only has to know what the renderer
//! writes: fence lines, backslash escapes, code spans, `**`, `*`, and `~~` markers, `[text](<url>)`
//! links, and `<url>` autolinks. A message ends at a paragraph break if one fits, else a line
//! break, else a space, else anywhere; whatever is open there (a code fence, styles, a code span)
//! is closed at the end of the message and reopened at the start of the next one.

use super::escape;

const FENCE: &str = "```";

/// A byte range of the text.
type Span = (usize, usize);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Strong,
    Emphasis,
    Strike,
}

impl Mark {
    fn text(self) -> &'static str {
        match self {
            Mark::Strong => "**",
            Mark::Emphasis => "*",
            Mark::Strike => "~~",
        }
    }
}

/// The open styles, outermost first.
#[derive(Clone, Copy, Default)]
struct Marks([Option<Mark>; 3]);

impl Marks {
    fn toggle(&mut self, mark: Mark) {
        if let Some(i) = self.0.iter().position(|m| *m == Some(mark)) {
            self.0[i..].rotate_left(1);
            self.0[2] = None;
        } else if let Some(slot) = self.0.iter_mut().find(|m| m.is_none()) {
            *slot = Some(mark);
        }
    }

    fn open(&self) -> impl DoubleEndedIterator<Item = Mark> + '_ {
        self.0.iter().flatten().copied()
    }
}

/// An open code fence.
#[derive(Clone, Copy)]
struct Fence {
    /// What precedes the backticks on the line that opened the fence: quote and list markers.
    prefix: Span,
    /// The language tag.
    info: Span,
}

/// What is open at a point in the text.
#[derive(Clone, Copy, Default)]
struct State {
    fence: Option<Fence>,
    marks: Marks,
    /// The length of the backtick run that opened the code span we are in, or 0.
    code: usize,
    /// Whether we are in the text of a `[text](<url>)` link.
    link: bool,
}

impl State {
    /// The length in characters of [`State::close`].
    fn close_len(&self) -> usize {
        match self.fence {
            Some(fence) => 1 + (fence.prefix.1 - fence.prefix.0) + FENCE.len(),
            None => self.code + self.marks.open().map(|m| m.text().len()).sum::<usize>(),
        }
    }

    /// What ends a message that stops in this state.
    fn close(&self, text: &str) -> String {
        match self.fence {
            Some(fence) => format!("\n{}{FENCE}", continuation(span(text, fence.prefix))),
            None => {
                let mut close = "`".repeat(self.code);
                for mark in self.marks.open().rev() {
                    close.push_str(mark.text());
                }
                close
            }
        }
    }
}

/// Where the next message starts, and what it reopens.
#[derive(Clone, Copy, Default)]
struct Resume {
    state: State,
    /// Whether the message starts inside a line, so it needs the line's quote prefix.
    mid_line: bool,
    /// The prefix of the line the message starts in.
    line_prefix: Span,
}

impl Resume {
    /// What starts a message in this state.
    fn reopen(&self, text: &str) -> String {
        if let Some(fence) = self.state.fence {
            let prefix = continuation(span(text, fence.prefix));
            let line = if self.mid_line { prefix.as_str() } else { "" };
            return format!("{prefix}{FENCE}{}\n{line}", span(text, fence.info));
        }
        let mut reopen = String::new();
        if self.mid_line {
            let prefix = span(text, self.line_prefix);
            if prefix.contains('>') {
                reopen.push_str(&continuation(prefix));
            }
        }
        for mark in self.state.marks.open() {
            reopen.push_str(mark.text());
        }
        reopen.push_str(&"`".repeat(self.state.code));
        reopen
    }
}

/// A place to end a message.
#[derive(Clone, Copy)]
struct Candidate {
    /// Where the message ends.
    end: usize,
    /// What is open there.
    state: State,
    resume: usize,
    mid_line: bool,
    line_prefix: Span,
}

enum Next {
    /// The rest of the text fits in one message, which ends in this state.
    Last(State),
    Cut(Candidate),
    /// Nothing fits the limit: its room is smaller than what a split needs to close and reopen.
    Stuck,
}

/// How good a place to end a message is, best last.
#[derive(Clone, Copy)]
enum Rank {
    Anywhere,
    Space,
    Line,
    Paragraph,
}

/// Splits `text` into messages of at most `max` characters (Unicode scalar values: the unit
/// Discord counts).
pub(super) fn split(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let text = text.trim_end();
    let mut chunks = Vec::new();
    let mut start = text
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(text.len());
    let mut resume = Resume::default();
    while start < text.len() {
        let reopen = resume.reopen(text);
        match next_chunk(text, start, &resume, count(&reopen), max) {
            Next::Last(state) => {
                chunks.push(format!("{reopen}{}{}", &text[start..], state.close(text)));
                break;
            }
            Next::Cut(cut) => {
                let body = text[start..cut.end].trim_end();
                chunks.push(format!("{reopen}{body}{}", cut.state.close(text)));
                start = cut.resume;
                resume = Resume {
                    state: State {
                        link: false,
                        ..cut.state
                    },
                    mid_line: cut.mid_line,
                    line_prefix: cut.line_prefix,
                };
            }
            Next::Stuck => {
                // Cut the text as it is: nothing can be reopened in so little room.
                let end = text[start..]
                    .char_indices()
                    .nth(max)
                    .map_or(text.len(), |(i, _)| start + i);
                let body = text[start..end].trim();
                if !body.is_empty() {
                    chunks.push(body.to_owned());
                }
                start = end;
                resume = Resume {
                    mid_line: true,
                    ..Resume::default()
                };
            }
        }
    }
    chunks
}

/// Finds where the message that starts at `start` should end. `reopened` is how much room the
/// text that reopens the state at its start already takes.
fn next_chunk(text: &str, start: usize, resume: &Resume, reopened: usize, max: usize) -> Next {
    let mut scan = Scan {
        text,
        state: resume.state,
        line_prefix: resume.line_prefix,
        hold_until: 0,
        content: false,
        fence_content: false,
        line_start: !resume.mid_line,
    };
    // The latest place to end a message of each rank.
    let mut best: [Option<Candidate>; 4] = [None; 4];
    let mut used = reopened;
    let mut at = start;
    loop {
        if at == text.len() {
            if used + scan.state.close_len() <= max {
                return Next::Last(scan.state);
            }
            break;
        }
        let c = text[at..].chars().next().unwrap_or('\n');
        if at > start && used + scan.state.close_len() <= max && scan.can_end_at(at) {
            let line = c == '\n';
            // A message that starts with `- `, `> `, `# ` or `1. ` would show as a list, quote,
            // or heading that the text never had.
            let marker = scan.state.fence.is_none()
                && escape::starts_block(text[at..].trim_start_matches(' '));
            let space = c == ' '
                && !marker
                && !scan.state.link
                && scan.state.code == 0
                && at > scan.line_prefix.1
                && !text[..at].ends_with(char::is_whitespace);
            // An empty quote line (`> `) between quoted paragraphs is a paragraph break too.
            let paragraph = line
                && scan.state.fence.is_none()
                && (text[at + 1..].starts_with('\n') || quote_only(text, at + 1));
            for (rank, applies) in [
                (Rank::Anywhere, !marker),
                (Rank::Space, space),
                (Rank::Line, line),
                (Rank::Paragraph, paragraph),
            ] {
                if applies {
                    best[rank as usize] = Some(scan.candidate(at, rank));
                }
            }
        }
        if scan.line_start {
            scan.begin_line(at);
        }
        scan.step(at, c);
        used += 1;
        at += c.len_utf8();
        if used > max {
            break;
        }
    }
    best.into_iter()
        .rev()
        .flatten()
        .next()
        .map_or(Next::Stuck, Next::Cut)
}

/// A reader of the text, one character at a time, that keeps track of what is open.
struct Scan<'a> {
    text: &'a str,
    state: State,
    /// Where the current line's prefix is: quote and list markers.
    line_prefix: Span,
    /// No message may end, and nothing is read, before this offset: it is inside a token (an
    /// escape, a marker, a URL) or on a fence line.
    hold_until: usize,
    /// Whether anything but whitespace and quote markers has been read.
    content: bool,
    /// Whether anything but whitespace has been read inside the open fence.
    fence_content: bool,
    /// Whether the next character starts a line.
    line_start: bool,
}

impl Scan<'_> {
    /// Whether a message may end before the character at `at` without leaving nothing to show
    /// or a fence with nothing in it.
    fn can_end_at(&self, at: usize) -> bool {
        if at < self.hold_until || !self.content {
            return false;
        }
        // Not between a quote's `>` and its text.
        if self.line_prefix.0 < at
            && at <= self.line_prefix.1
            && is_quote_prefix(&self.text[self.line_prefix.0..at])
        {
            return false;
        }
        match self.state.fence {
            None => true,
            Some(_) => self.fence_content && !self.closes_fence(at),
        }
    }

    /// Whether the next line starts with the closing fence: ending a message there would leave
    /// the fence that the next one reopens empty.
    fn closes_fence(&self, at: usize) -> bool {
        let rest = self.text[at..].trim_start_matches('\n');
        rest[prefix_len(rest)..].starts_with(FENCE)
    }

    fn candidate(&self, at: usize, rank: Rank) -> Candidate {
        let skip = |from: usize, chars: &[char]| {
            let rest = &self.text[from..];
            from + rest.len() - rest.trim_start_matches(chars).len()
        };
        let (resume, mid_line) = match rank {
            Rank::Paragraph | Rank::Line => {
                // Past the break, and past the empty quote lines in it.
                let mut resume = skip(at, &['\n']);
                while quote_only(self.text, resume) {
                    resume = skip(resume + line_len(self.text, resume), &['\n']);
                }
                (resume, false)
            }
            Rank::Space => (skip(at, &[' ']), true),
            Rank::Anywhere => (at, true),
        };
        Candidate {
            end: at,
            state: self.state,
            resume,
            mid_line,
            line_prefix: self.line_prefix,
        }
    }

    /// Reads the start of the line at `at`: its prefix, and whether it opens or closes a fence.
    fn begin_line(&mut self, at: usize) {
        self.line_start = false;
        let end = self.text[at..]
            .find('\n')
            .map_or(self.text.len(), |i| at + i);
        let line = &self.text[at..end];
        let prefix = prefix_len(line);
        self.line_prefix = (at, at + prefix);
        let Some(after) = line[prefix..].strip_prefix(FENCE) else {
            return;
        };
        match self.state.fence {
            None => {
                let lead = after.len() - after.trim_start().len();
                let info_start = at + prefix + FENCE.len() + lead;
                self.state = State {
                    fence: Some(Fence {
                        prefix: self.line_prefix,
                        info: (info_start, info_start + after.trim().len()),
                    }),
                    ..State::default()
                };
                self.fence_content = false;
            }
            Some(_) => self.state.fence = None,
        }
        self.hold_until = end;
    }

    /// Reads the character `c` at `at`.
    fn step(&mut self, at: usize, c: char) {
        if !c.is_whitespace() && c != '>' {
            self.content = true;
        }
        if at < self.hold_until {
            return;
        }
        if c == '\n' {
            self.line_start = true;
            if self.state.fence.is_none() {
                // No span crosses a line break.
                self.state = State::default();
            }
            return;
        }
        if self.state.fence.is_some() {
            self.fence_content |= !c.is_whitespace() && at >= self.line_prefix.1;
            return;
        }
        let rest = &self.text[at..];
        if self.state.code > 0 {
            if c == '`' {
                let run = backticks(rest);
                if run == self.state.code {
                    self.state.code = 0;
                }
                self.hold_until = at + run;
            }
            return;
        }
        match c {
            '\\' => {
                let escaped = rest[1..].chars().next().map_or(0, char::len_utf8);
                self.hold_until = at + 1 + escaped;
            }
            '`' => {
                self.state.code = backticks(rest);
                self.hold_until = at + self.state.code;
            }
            '*' if rest.starts_with("**") => {
                self.state.marks.toggle(Mark::Strong);
                self.hold_until = at + 2;
            }
            '*' => self.state.marks.toggle(Mark::Emphasis),
            '~' if rest.starts_with("~~") => {
                self.state.marks.toggle(Mark::Strike);
                self.hold_until = at + 2;
            }
            '[' => self.state.link = true,
            ']' => {
                self.state.link = false;
                if rest[1..].starts_with("(<") {
                    self.hold_until = at + url_end(rest);
                }
            }
            '<' if is_autolink(&rest[1..]) => self.hold_until = at + url_end(rest),
            _ => {}
        }
    }
}

/// The length of the run of backticks at the start of `rest`.
fn backticks(rest: &str) -> usize {
    rest.bytes().take_while(|b| *b == b'`').count()
}

/// The length of what ends a URL written as `<url>` or `](<url>)` at the start of `rest`: up to
/// and including the `>`, and the `)` after it.
fn url_end(rest: &str) -> usize {
    match rest.find('>') {
        Some(gt) if rest[gt + 1..].starts_with(')') && rest.starts_with("](") => gt + 2,
        Some(gt) => gt + 1,
        None => rest.len(),
    }
}

fn is_autolink(after_angle: &str) -> bool {
    super::escape::is_web_url(after_angle)
}

/// The length of the line that starts at `from`, without its line break.
fn line_len(text: &str, from: usize) -> usize {
    text[from..].find('\n').unwrap_or(text.len() - from)
}

/// Whether `prefix` is nothing but quote markers and the spaces after them.
fn is_quote_prefix(prefix: &str) -> bool {
    prefix.contains('>') && prefix.chars().all(|c| c == ' ' || c == '>')
}

/// Whether the line that starts at `from` is a quote line with nothing in it.
fn quote_only(text: &str, from: usize) -> bool {
    is_quote_prefix(&text[from..from + line_len(text, from)])
}

/// The length of the quote and list markers, and indentation, at the start of `line`.
fn prefix_len(line: &str) -> usize {
    let mut len = 0;
    loop {
        let rest = &line[len..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if rest.starts_with([' ', '>']) {
            len += 1;
        } else if rest.starts_with("- ") {
            len += 2;
        } else if digits > 0 && rest[digits..].starts_with(". ") {
            len += digits + 2;
        } else {
            return len;
        }
    }
}

/// The prefix a line continued in a new message needs: the quote markers, with the list markers
/// replaced by indentation.
fn continuation(prefix: &str) -> String {
    prefix
        .chars()
        .map(|c| if c == '>' { '>' } else { ' ' })
        .collect()
}

fn span(text: &str, (start, end): Span) -> &str {
    &text[start..end]
}

/// The length of `text` in characters.
fn count(text: &str) -> usize {
    text.chars().count()
}
