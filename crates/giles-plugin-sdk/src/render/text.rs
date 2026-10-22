//! CommonMark to line-oriented text formats. They share one block layout (paragraphs, list
//! markers, quote prefixes, tables) and differ in how they mark inline styles and code.

use std::borrow::Cow;
use std::mem;

use pulldown_cmark::{Alignment, Event, HeadingLevel, LinkType, Tag, TagEnd};

use super::irc::{IRC_BOLD, IRC_ITALIC};
use super::strip_controls;

/// Renders CommonMark as readable plain text, for the text part of email.
///
/// - Paragraphs and blocks are separated by a blank line; soft line breaks are kept.
/// - Level 1 headings are underlined with `=`, other levels with `-`.
/// - `*strong*`, `_emphasis_`, `~~strikethrough~~`, and `` `code` `` keep a light markup.
/// - Links show their target after the text, as `text <url>`; autolinks show the URL once.
/// - Images show only their alt text, as `[image: alt]`, never the URL.
/// - Code blocks keep their text verbatim, indented by four spaces.
/// - Lists use `-` and `1.` markers, quotes a `> ` prefix, tables aligned `|` columns.
/// - Raw HTML is shown as text; control characters other than tab and newline are dropped.
pub fn plain_text(markdown: &str) -> String {
    render(markdown, Flavor::Plain)
}

/// Which text format to write.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavor {
    Plain,
    /// IRC formatting codes; split into messages by `super::irc`.
    Irc,
}

/// Renders `markdown` in `flavor`, with lines separated by `\n` and no trailing newline.
pub(super) fn render(markdown: &str, flavor: Flavor) -> String {
    let mut writer = Writer::new(flavor);
    for event in super::parser(markdown) {
        writer.event(event);
    }
    writer.out
}

/// The separation owed before the next line of output.
#[derive(PartialEq, Eq)]
enum Sep {
    None,
    /// Start a new line.
    Line,
    /// Leave a blank line, written as the given container prefix (such as `>`).
    Blank(String),
}

#[derive(Clone, Copy)]
enum Style {
    Strong,
    Emphasis,
    Strikethrough,
}

/// A block that prefixes each of its lines: a quote or a list item.
struct Container {
    /// Written instead of `rest` on the container's first line (a list item's marker).
    first: Option<String>,
    rest: String,
    item: bool,
}

struct List {
    /// The next item number, for ordered lists.
    next: Option<u64>,
    /// Whether the items are paragraphs, separated by blank lines.
    loose: bool,
}

/// Inline content consumed for its text alone.
struct Raw {
    kind: RawKind,
    /// Open tags, including the one that started the capture.
    depth: usize,
    text: String,
}

enum RawKind {
    /// An image's alt text.
    Image,
    /// An autolink's text, which is its URL or email address.
    Autolink,
}

struct Writer {
    flavor: Flavor,
    out: String,
    /// Whether any line has been written.
    started: bool,
    /// Whether the current line has its prefixes written and can take more text.
    line_open: bool,
    pending: Sep,
    containers: Vec<Container>,
    lists: Vec<List>,
    /// Inline text being collected instead of written: headings, table cells.
    captures: Vec<String>,
    /// Nesting depth of each `Style`, so nested equal styles write one marker pair.
    styles: [usize; 3],
    /// Targets of the open links.
    links: Vec<String>,
    raw: Option<Raw>,
    /// Rows of the open table, header first.
    table: Option<(Vec<Alignment>, Vec<Vec<String>>)>,
    in_code_block: bool,
}

impl Writer {
    fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            out: String::new(),
            started: false,
            line_open: false,
            pending: Sep::None,
            containers: Vec::new(),
            lists: Vec::new(),
            captures: Vec::new(),
            styles: [0; 3],
            links: Vec::new(),
            raw: None,
            table: None,
            in_code_block: false,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some(raw) = &mut self.raw {
            match event {
                Event::Start(_) => raw.depth += 1,
                Event::End(_) => {
                    raw.depth -= 1;
                    if raw.depth == 0 {
                        self.finish_raw();
                    }
                }
                Event::Text(text)
                | Event::Code(text)
                | Event::Html(text)
                | Event::InlineHtml(text) => {
                    raw.text.push_str(&strip_controls(&text));
                }
                Event::SoftBreak | Event::HardBreak => raw.text.push(' '),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) if self.in_code_block => self.code_lines(&strip_controls(&text)),
            Event::Text(text) => self.prose(&text),
            // An inline HTML tag can span lines; its line breaks are soft breaks.
            Event::InlineHtml(html) => {
                for (i, piece) in html.split('\n').enumerate() {
                    if i > 0 {
                        self.line_break();
                    }
                    self.prose(piece);
                }
            }
            Event::Html(html) => {
                let html = self.escape(&html).into_owned();
                self.code_lines(&html);
            }
            Event::Code(code) => self.inline_code(&strip_controls(&code)),
            Event::SoftBreak | Event::HardBreak => self.line_break(),
            Event::Rule => {
                self.block();
                self.inline("---");
                self.end_line();
            }
            Event::TaskListMarker(checked) => self.inline(if checked { "[x] " } else { "[ ] " }),
            // Footnotes and math are not enabled by `super::parser`.
            Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                if self.marker_pending()
                    && let Some(list) = self.lists.last_mut()
                {
                    list.loose = true;
                }
                self.block();
            }
            Tag::Heading { .. } => {
                self.block();
                match self.flavor {
                    Flavor::Plain => self.captures.push(String::new()),
                    Flavor::Irc => self.style(Style::Strong, true),
                }
            }
            Tag::BlockQuote(_) => {
                self.block();
                self.containers.push(Container {
                    first: None,
                    rest: "> ".to_owned(),
                    item: false,
                });
            }
            Tag::CodeBlock(_) => {
                self.block();
                self.in_code_block = true;
                let indent = match self.flavor {
                    Flavor::Plain => "    ",
                    Flavor::Irc => "",
                };
                self.containers.push(Container {
                    first: None,
                    rest: indent.to_owned(),
                    item: false,
                });
            }
            Tag::HtmlBlock => self.block(),
            Tag::List(start) => {
                self.end_line();
                if !self.marker_pending() {
                    let in_tight_item = self.containers.last().is_some_and(|c| c.item)
                        && self.lists.last().is_some_and(|list| !list.loose);
                    self.request(if in_tight_item {
                        Sep::Line
                    } else {
                        self.blank()
                    });
                }
                self.lists.push(List {
                    next: start,
                    loose: false,
                });
            }
            Tag::Item => {
                self.end_line();
                let list = self.lists.last_mut();
                let (marker, loose) = match list {
                    Some(List {
                        next: Some(n),
                        loose,
                    }) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        (marker, *loose)
                    }
                    Some(List { next: None, loose }) => ("- ".to_owned(), *loose),
                    None => ("- ".to_owned(), false),
                };
                self.request(if loose { self.blank() } else { Sep::Line });
                self.containers.push(Container {
                    rest: " ".repeat(marker.chars().count()),
                    first: Some(marker),
                    item: true,
                });
            }
            Tag::Table(alignments) => {
                self.block();
                self.table = Some((alignments, Vec::new()));
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some((_, rows)) = &mut self.table {
                    rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.captures.push(String::new()),
            Tag::Emphasis => self.style(Style::Emphasis, true),
            Tag::Strong => self.style(Style::Strong, true),
            Tag::Strikethrough => self.style(Style::Strikethrough, true),
            Tag::Link {
                link_type: LinkType::Autolink | LinkType::Email,
                ..
            } => self.start_raw(RawKind::Autolink),
            Tag::Link { dest_url, .. } => self.links.push(dest_url.into_string()),
            Tag::Image { .. } => self.start_raw(RawKind::Image),
            // Not enabled by `super::parser`; their content renders as ordinary text.
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.end_line(),
            TagEnd::Heading(level) => match self.flavor {
                Flavor::Plain => {
                    let text = self.captures.pop().unwrap_or_default();
                    self.heading(level, &text);
                }
                Flavor::Irc => {
                    self.style(Style::Strong, false);
                    self.end_line();
                }
            },
            TagEnd::BlockQuote(_) => {
                self.end_line();
                self.containers.pop();
            }
            TagEnd::CodeBlock => {
                self.end_line();
                self.containers.pop();
                self.in_code_block = false;
            }
            TagEnd::List(_) => {
                self.end_line();
                self.lists.pop();
            }
            TagEnd::Item => {
                if self.marker_pending() {
                    // An empty item: write its marker alone, without the trailing space.
                    if let Some(marker) = self.containers.last_mut().and_then(|c| c.first.as_mut())
                    {
                        marker.truncate(marker.trim_end().len());
                    }
                    self.ensure_line();
                }
                self.end_line();
                self.containers.pop();
            }
            TagEnd::TableCell => {
                let cell = self.captures.pop().unwrap_or_default();
                if let Some(row) = self.table.as_mut().and_then(|(_, rows)| rows.last_mut()) {
                    row.push(cell.trim().to_owned());
                }
            }
            TagEnd::Table => {
                if let Some((alignments, rows)) = self.table.take() {
                    self.write_table(&alignments, &rows);
                }
            }
            TagEnd::Emphasis => self.style(Style::Emphasis, false),
            TagEnd::Strong => self.style(Style::Strong, false),
            TagEnd::Strikethrough => self.style(Style::Strikethrough, false),
            TagEnd::Link => {
                let dest = self.links.pop().unwrap_or_default();
                if !dest.is_empty() {
                    self.inline(&format!(" <{}>", strip_controls(&dest)));
                }
            }
            _ => {}
        }
    }

    fn heading(&mut self, level: HeadingLevel, text: &str) {
        if text.is_empty() {
            return;
        }
        self.inline(text);
        self.end_line();
        let underline = if level == HeadingLevel::H1 { "=" } else { "-" };
        self.inline(&underline.repeat(text.chars().count()));
        self.end_line();
    }

    fn write_table(&mut self, alignments: &[Alignment], rows: &[Vec<String>]) {
        if self.flavor != Flavor::Plain {
            // Chat clients use proportional fonts, so padding wouldn't align anything.
            for row in rows {
                self.inline(&row.join(" | "));
                self.end_line();
            }
            return;
        }
        let columns = rows.iter().map(Vec::len).max().unwrap_or_default();
        let mut widths = vec![1; columns];
        for row in rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.chars().count());
            }
        }
        for (i, row) in rows.iter().enumerate() {
            let cells = (0..columns).map(|column| {
                let cell = row.get(column).map_or("", String::as_str);
                let alignment = alignments.get(column).copied().unwrap_or(Alignment::None);
                pad(cell, widths[column], alignment)
            });
            let line = cells.collect::<Vec<_>>().join(" | ");
            self.inline(line.trim_end());
            self.end_line();
            if i == 0 {
                let rule = widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>();
                self.inline(&rule.join("-|-"));
                self.end_line();
            }
        }
    }

    fn style(&mut self, style: Style, open: bool) {
        let depth = &mut self.styles[style as usize];
        let marker_due = if open {
            *depth += 1;
            *depth == 1
        } else {
            *depth -= 1;
            *depth == 0
        };
        if marker_due {
            let marker = match (self.flavor, style) {
                (Flavor::Plain, Style::Strong) => "*",
                (Flavor::Plain, Style::Emphasis) => "_",
                (Flavor::Plain | Flavor::Irc, Style::Strikethrough) => "~~",
                (Flavor::Irc, Style::Strong) => IRC_BOLD,
                (Flavor::Irc, Style::Emphasis) => IRC_ITALIC,
            };
            self.inline(marker);
        }
    }

    fn inline_code(&mut self, code: &str) {
        self.inline(&format!("`{code}`"));
    }

    fn start_raw(&mut self, kind: RawKind) {
        self.raw = Some(Raw {
            kind,
            depth: 1,
            text: String::new(),
        });
    }

    fn finish_raw(&mut self) {
        let Some(raw) = self.raw.take() else {
            return;
        };
        match raw.kind {
            RawKind::Autolink => self.inline(&raw.text),
            RawKind::Image if raw.text.is_empty() => self.inline("[image]"),
            RawKind::Image => {
                let alt = self.escape(&raw.text).into_owned();
                self.inline(&format!("[image: {alt}]"));
            }
        }
    }

    /// Escapes prose for the flavor: drops control characters.
    fn escape<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self.flavor {
            Flavor::Plain | Flavor::Irc => strip_controls(text),
        }
    }

    fn prose(&mut self, text: &str) {
        let text = self.escape(text).into_owned();
        self.inline(&text);
    }

    /// Writes text on the current line, opening it if needed.
    fn inline(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(capture) = self.captures.last_mut() {
            capture.push_str(text);
        } else {
            self.ensure_line();
            self.out.push_str(text);
        }
    }

    /// Writes text whose newlines separate lines, such as a code block, keeping empty lines.
    fn code_lines(&mut self, text: &str) {
        for (i, line) in text.split('\n').enumerate() {
            if i > 0 {
                if self.line_open {
                    self.end_line();
                } else {
                    self.empty_line();
                }
            }
            self.inline(line);
        }
    }

    fn line_break(&mut self) {
        if let Some(capture) = self.captures.last_mut() {
            capture.push(' ');
        } else {
            self.end_line();
        }
    }

    /// Starts a block: it gets a line of its own, after a blank line unless it's the first block
    /// in a list item.
    fn block(&mut self) {
        self.end_line();
        if !self.marker_pending() {
            self.request(self.blank());
        }
    }

    /// Whether the innermost list item hasn't written its marker yet.
    fn marker_pending(&self) -> bool {
        self.containers
            .last()
            .is_some_and(|c| c.item && c.first.is_some())
    }

    /// The separation between blocks: a blank line, except on IRC, where it would only be
    /// dropped (an IRC message can't be empty) or show as a bare quote prefix.
    fn blank(&self) -> Sep {
        match self.flavor {
            Flavor::Irc => Sep::Line,
            Flavor::Plain => Sep::Blank(self.blank_prefix()),
        }
    }

    /// What an empty line inside the current containers holds: the quote prefixes.
    fn blank_prefix(&self) -> String {
        let prefix: String = self.containers.iter().map(|c| c.rest.as_str()).collect();
        prefix.trim_end().to_owned()
    }

    /// Raises the pending separation to at least `sep`.
    fn request(&mut self, sep: Sep) {
        match (&self.pending, &sep) {
            (Sep::Blank(_), _) | (Sep::Line, Sep::Line) | (_, Sep::None) => {}
            _ => self.pending = sep,
        }
    }

    fn flush_pending(&mut self) {
        let pending = mem::replace(&mut self.pending, Sep::None);
        if !self.started {
            return;
        }
        match pending {
            Sep::None => {}
            Sep::Line => self.out.push('\n'),
            Sep::Blank(prefix) => {
                self.out.push('\n');
                self.out.push_str(&prefix);
                self.out.push('\n');
            }
        }
    }

    fn ensure_line(&mut self) {
        if self.line_open {
            return;
        }
        self.flush_pending();
        for container in &mut self.containers {
            match container.first.take() {
                Some(marker) => self.out.push_str(&marker),
                None => self.out.push_str(&container.rest),
            }
        }
        self.line_open = true;
        self.started = true;
    }

    /// Ends the current line, if one is open.
    fn end_line(&mut self) {
        if self.line_open {
            self.line_open = false;
            self.request(Sep::Line);
        }
    }

    /// Writes an empty line inside a block, carrying only the quote prefixes.
    fn empty_line(&mut self) {
        self.flush_pending();
        let prefix = self.blank_prefix();
        self.out.push_str(&prefix);
        self.started = true;
        self.pending = Sep::Line;
    }
}

/// Pads `cell` to `width` characters according to its column's alignment.
fn pad(cell: &str, width: usize, alignment: Alignment) -> String {
    let fill = width.saturating_sub(cell.chars().count());
    let (left, right) = match alignment {
        Alignment::Right => (fill, 0),
        Alignment::Center => (fill / 2, fill - fill / 2),
        Alignment::Left | Alignment::None => (0, fill),
    };
    format!("{}{cell}{}", " ".repeat(left), " ".repeat(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_plain_text() {
        let cases: &[(&str, &str, &str)] = &[
            ("empty", "", ""),
            ("whitespace only", "  \n\n ", ""),
            ("paragraphs", "One\ntwo\n\nThree", "One\ntwo\n\nThree"),
            (
                "headings",
                "# Title\n\nText\n\n### Sub *part*",
                "Title\n=====\n\nText\n\nSub _part_\n----------",
            ),
            (
                "inline styles",
                "**bold**, *it*, ~~gone~~, `x < y`, ***both***",
                "*bold*, _it_, ~~gone~~, `x < y`, _*both*_",
            ),
            (
                "nested emphasis writes one pair",
                "**a **b** c**",
                "*a b c*",
            ),
            (
                "tight nested lists",
                "- a\n  - b\n    1. c\n- d\n\n3. x\n4. y",
                "- a\n  - b\n    1. c\n- d\n\n3. x\n4. y",
            ),
            (
                "loose list",
                "- a\n\n  more a\n- b",
                "- a\n\n  more a\n\n- b",
            ),
            (
                "task list",
                "- [ ] todo\n- [x] done",
                "- [ ] todo\n- [x] done",
            ),
            ("empty item", "-\n- b", "-\n- b"),
            ("blockquote", "> a\n>\n> b\n> > c", "> a\n>\n> b\n>\n> > c"),
            ("list in quote", "> - a\n> - b", "> - a\n> - b"),
            (
                "links",
                "[docs](https://example.com/d) and <https://example.com> and <me@example.com>",
                "docs <https://example.com/d> and https://example.com and me@example.com",
            ),
            (
                "unsafe link shown as text",
                "[x](javascript:alert(1))",
                "x <javascript:alert(1)>",
            ),
            (
                "images hide the url",
                "![a *cat*](https://attacker.example/?d=1) ![](https://attacker.example/)",
                "[image: a cat] [image]",
            ),
            (
                "code block",
                "Run:\n\n```sh\ncd /tmp\n\n  ls -l\n```\n\nDone.",
                "Run:\n\n    cd /tmp\n\n      ls -l\n\nDone.",
            ),
            (
                "code block in list",
                "1. step:\n\n   ```\n   make\n   ```",
                "1. step:\n\n       make",
            ),
            (
                "raw html",
                "<div>\n<b>hi</b>\n</div>\n\nand <i>this</i>",
                "<div>\n<b>hi</b>\n</div>\n\nand <i>this</i>",
            ),
            (
                "table",
                "| Name | Qty |\n|:--|--:|\n| apple | 3 |\n| fig | 12 |",
                "Name  | Qty\n------|----\napple |   3\nfig   |  12",
            ),
            ("rule", "a\n\n---\n\nb", "a\n\n---\n\nb"),
            ("hard break", "a  \nb", "a\nb"),
            (
                "inline html across lines",
                "> <span\n> title=x>a</span>\n\n- b <i\n  c>d",
                "> <span\n> title=x>a</span>\n\n- b <i\n  c>d",
            ),
            (
                "inline html across lines in a setext heading",
                "a <b\nc>\n===",
                "a <b c>\n=======",
            ),
            (
                "control characters",
                "a\u{1b}[31mb\u{7}c\u{85}\r\n\n```\nx\u{0}y\u{8}\n```",
                "a[31mbc\n\n    xy",
            ),
            ("multibyte", "# Grüße 👋", "Grüße 👋\n======="),
        ];
        for (name, input, expected) in cases {
            assert_eq!(plain_text(input), *expected, "case {name}");
        }
    }
}
