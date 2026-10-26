//! CommonMark to line-oriented text formats. They share one block layout (paragraphs, list
//! markers, quote prefixes, tables) and differ in how they mark inline styles and code.

use std::borrow::Cow;
use std::mem;

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, Tag, TagEnd, TextMergeStream,
};

use super::discord::escape as discord;
use super::html::code_language;
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

/// Renders CommonMark as WhatsApp markup.
///
/// - Strong text and headings are `*bold*`, emphasis `_italic_`, strikethrough `~struck~`, inline
///   code `` `code` ``, code blocks fenced with ```` ``` ````.
/// - Styles are closed before each line break and reopened after it.
/// - Lists use `-` and `1.` markers, quotes a `> ` prefix, tables `|` between cells.
/// - Links show their target as `text <url>`; images show only `[image: alt]`.
/// - WhatsApp has no escape character, so literal `*`, `_`, `~`, and `` ` `` in text are replaced
///   with look-alikes (`∗`, `ˍ`, `∼`, `ˋ`), except `*_~` inside a word and anything in a bare URL.
/// - Raw HTML is shown as text; control characters other than tab and newline are dropped.
pub fn whatsapp(markdown: &str) -> String {
    render(markdown, Flavor::WhatsApp)
}

/// Which text format to write.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavor {
    Plain,
    /// IRC formatting codes; split into messages by `super::irc`.
    Irc,
    WhatsApp,
    /// Discord's Markdown; split into messages by `super::discord`.
    Discord,
}

/// Renders `markdown` in `flavor`, with lines separated by `\n` and no trailing newline.
pub(super) fn render(markdown: &str, flavor: Flavor) -> String {
    let mut writer = Writer::new(flavor);
    // Merged text runs let WhatsApp escaping see a character's neighbors.
    for event in TextMergeStream::new(super::parser(markdown)) {
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

#[derive(Clone, Copy, PartialEq, Eq)]
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
    /// An image's target.
    dest: String,
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
    /// Where the current line's content starts, after its container prefixes.
    content_start: usize,
    pending: Sep,
    containers: Vec<Container>,
    lists: Vec<List>,
    /// Inline text being collected instead of written: headings, table cells.
    captures: Vec<String>,
    /// Nesting depth of each `Style`, so nested equal styles write one marker pair.
    styles: [usize; 3],
    /// The styles with a marker written, in the order they were opened.
    open_styles: Vec<Style>,
    /// Targets of the open links.
    links: Vec<String>,
    raw: Option<Raw>,
    /// Rows of the open table, header first.
    table: Option<(Vec<Alignment>, Vec<Vec<String>>)>,
    in_code_block: bool,
    /// Whether the open code block has had no text yet.
    block_empty: bool,
}

impl Writer {
    fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            out: String::new(),
            started: false,
            line_open: false,
            content_start: 0,
            pending: Sep::None,
            containers: Vec::new(),
            lists: Vec::new(),
            captures: Vec::new(),
            styles: [0; 3],
            open_styles: Vec::new(),
            links: Vec::new(),
            raw: None,
            table: None,
            in_code_block: false,
            block_empty: false,
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
            Event::Text(text) if self.in_code_block => {
                let code = self.code_text(&text);
                self.block_empty &= code.is_empty();
                self.code_lines(&code);
            }
            Event::Text(text) => self.prose(&text),
            // An inline HTML tag can span lines; its line breaks are soft breaks.
            Event::InlineHtml(html) => self.html_lines(&html),
            // Discord reads each line of an HTML block as Markdown, so it is escaped as prose.
            Event::Html(html) if self.flavor == Flavor::Discord => self.html_lines(&html),
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
                    Flavor::Plain | Flavor::Discord => self.captures.push(String::new()),
                    Flavor::Irc | Flavor::WhatsApp => self.style(Style::Strong, true),
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
            Tag::CodeBlock(kind) => {
                self.block();
                self.in_code_block = true;
                let indent = match self.flavor {
                    Flavor::Plain => "    ",
                    Flavor::Irc | Flavor::WhatsApp | Flavor::Discord => "",
                };
                self.containers.push(Container {
                    first: None,
                    rest: indent.to_owned(),
                    item: false,
                });
                match self.flavor {
                    Flavor::WhatsApp => {
                        self.inline("```");
                        self.end_line();
                    }
                    Flavor::Discord => {
                        let language = match kind {
                            CodeBlockKind::Fenced(info) => code_language(&info),
                            CodeBlockKind::Indented => String::new(),
                        };
                        self.inline(&format!("```{language}"));
                        self.end_line();
                        self.block_empty = true;
                    }
                    Flavor::Plain | Flavor::Irc => {}
                }
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
            } => self.start_raw(RawKind::Autolink, String::new()),
            Tag::Link { dest_url, .. } => {
                self.links.push(dest_url.into_string());
                if self.flavor == Flavor::Discord {
                    self.captures.push(String::new());
                }
            }
            Tag::Image { dest_url, .. } => self.start_raw(RawKind::Image, dest_url.into_string()),
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
                Flavor::Discord => {
                    let text = self.captures.pop().unwrap_or_default();
                    self.discord_heading(level, &text);
                }
                Flavor::Irc | Flavor::WhatsApp => {
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
                match self.flavor {
                    Flavor::WhatsApp => {
                        self.inline("```");
                        self.end_line();
                    }
                    Flavor::Discord => {
                        if self.block_empty {
                            // Discord shows a fence with nothing in it as text.
                            self.inline(&discord::ZWSP.to_string());
                            self.end_line();
                        }
                        self.inline("```");
                        self.end_line();
                    }
                    Flavor::Plain | Flavor::Irc => {}
                }
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
                if self.flavor == Flavor::Discord {
                    let text = self.captures.pop().unwrap_or_default();
                    self.discord_link(&text, &dest);
                } else if !dest.is_empty() {
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

    /// Writes a Discord heading. Discord has three levels, so the deeper ones share the last.
    fn discord_heading(&mut self, level: HeadingLevel, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let hashes = match level {
            HeadingLevel::H1 => "#",
            HeadingLevel::H2 => "##",
            _ => "###",
        };
        self.inline(&format!("{hashes} {text}"));
        self.end_line();
    }

    /// Writes a link's captured `text` for Discord: a masked link without an embed for a web
    /// target, else the text with its target as code (Discord only links `http` and `https`).
    fn discord_link(&mut self, text: &str, dest: &str) {
        let dest = strip_controls(dest);
        if self.table.is_some() {
            self.inline(text);
            if !dest.is_empty() {
                self.inline(&format!(" <{}>", discord::code(&dest)));
            }
        } else if discord::is_web_url(&dest) {
            if text.trim().is_empty() {
                self.inline(&format!("<{}>", discord::url(&dest, false)));
            } else {
                self.inline(&format!("[{text}](<{}>)", discord::url(&dest, true)));
            }
        } else {
            // The text isn't in brackets, so at the start of a line it must not read as a marker.
            if self.at_line_start() {
                self.inline(&discord::at_line_start(text));
            } else {
                self.inline(text);
            }
            if !dest.is_empty() {
                let space = if text.is_empty() { "" } else { " " };
                self.inline(&format!("{space}({})", discord::code_span(&dest)));
            }
        }
    }

    /// Writes the lines of raw HTML as prose; its line breaks are soft breaks.
    fn html_lines(&mut self, html: &str) {
        for (i, piece) in html.split('\n').enumerate() {
            if i > 0 {
                self.line_break();
            }
            self.prose(piece);
        }
    }

    fn write_table(&mut self, alignments: &[Alignment], rows: &[Vec<String>]) {
        if !matches!(self.flavor, Flavor::Plain | Flavor::Discord) {
            // Chat clients use proportional fonts, so padding wouldn't align anything.
            for row in rows {
                self.inline(&row.join(" | "));
                self.end_line();
            }
            return;
        }
        // Discord has no tables, but monospaced code blocks align columns.
        let fenced = self.flavor == Flavor::Discord;
        if fenced {
            self.inline("```");
            self.end_line();
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
        if fenced {
            self.inline("```");
            self.end_line();
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
            if open {
                self.open_styles.push(style);
            } else if let Some(i) = self.open_styles.iter().rposition(|s| *s == style) {
                self.open_styles.remove(i);
            }
            if !self.plain_inline() {
                self.inline(self.marker(style));
            }
        }
    }

    /// Whether inline text is for a Discord table cell, which is shown in a code block, where
    /// markup would show as it is written.
    fn plain_inline(&self) -> bool {
        self.flavor == Flavor::Discord && self.table.is_some()
    }

    fn marker(&self, style: Style) -> &'static str {
        match (self.flavor, style) {
            (Flavor::Plain | Flavor::WhatsApp, Style::Strong) => "*",
            (Flavor::Plain | Flavor::WhatsApp, Style::Emphasis) => "_",
            (Flavor::Plain | Flavor::Irc | Flavor::Discord, Style::Strikethrough) => "~~",
            (Flavor::Discord, Style::Strong) => "**",
            (Flavor::Discord, Style::Emphasis) => "*",
            (Flavor::WhatsApp, Style::Strikethrough) => "~",
            (Flavor::Irc, Style::Strong) => IRC_BOLD,
            (Flavor::Irc, Style::Emphasis) => IRC_ITALIC,
        }
    }

    fn inline_code(&mut self, code: &str) {
        match self.flavor {
            Flavor::Discord if self.table.is_some() => self.inline(&discord::code(code)),
            Flavor::Discord => self.inline(&discord::code_span(code)),
            // WhatsApp's inline code can't contain a backtick: show the code as text.
            Flavor::WhatsApp if code.contains('`') => self.prose(code),
            Flavor::Plain | Flavor::Irc | Flavor::WhatsApp => self.inline(&format!("`{code}`")),
        }
    }

    /// The text of a code block, which WhatsApp ends at the first triple backtick.
    fn code_text(&self, text: &str) -> String {
        let text = strip_controls(text);
        match self.flavor {
            Flavor::WhatsApp => text.replace("```", "\u{2cb}\u{2cb}\u{2cb}"),
            Flavor::Discord => discord::code(&text),
            Flavor::Plain | Flavor::Irc => text.into_owned(),
        }
    }

    fn start_raw(&mut self, kind: RawKind, dest: String) {
        self.raw = Some(Raw {
            kind,
            dest,
            depth: 1,
            text: String::new(),
        });
    }

    fn finish_raw(&mut self) {
        let Some(raw) = self.raw.take() else {
            return;
        };
        let discord_image = self.flavor == Flavor::Discord && self.table.is_none();
        match raw.kind {
            RawKind::Autolink if self.flavor == Flavor::Discord => {
                if self.table.is_none() && discord::is_web_url(&raw.text) {
                    self.inline(&format!("<{}>", discord::url(&raw.text, false)));
                } else {
                    let text = self.escape(&raw.text).into_owned();
                    self.inline(&text);
                }
            }
            RawKind::Autolink => self.inline(&raw.text),
            // An image is a link on Discord, with its embed suppressed: nothing is fetched until
            // someone clicks. Inside a link, whose text can't hold another link, only the alt
            // text is shown.
            RawKind::Image
                if discord_image && (!self.links.is_empty() || discord::is_web_url(&raw.dest)) =>
            {
                let label = if raw.text.trim().is_empty() {
                    "image".to_owned()
                } else {
                    self.escape(&raw.text).into_owned()
                };
                if self.links.is_empty() {
                    self.inline(&format!("[{label}](<{}>)", discord::url(&raw.dest, true)));
                } else {
                    self.inline(&label);
                }
            }
            RawKind::Image if raw.text.is_empty() => self.inline("[image]"),
            RawKind::Image => {
                let alt = self.escape(&raw.text).into_owned();
                self.inline(&format!("[image: {alt}]"));
            }
        }
    }

    /// Escapes prose for the flavor: drops control characters, and on WhatsApp neutralizes
    /// markup characters.
    fn escape<'a>(&self, text: &'a str) -> Cow<'a, str> {
        match self.flavor {
            Flavor::Plain | Flavor::Irc => strip_controls(text),
            Flavor::WhatsApp => Cow::Owned(whatsapp_literal(&strip_controls(text))),
            Flavor::Discord => Cow::Owned(self.discord_text(text, false, false)),
        }
    }

    /// Escapes text for Discord. A table cell is shown in a code block, where escapes would show.
    fn discord_text(&self, text: &str, wrap_urls: bool, line_start: bool) -> String {
        if self.table.is_some() {
            discord::code(&strip_controls(text))
        } else {
            discord::prose(text, wrap_urls, line_start)
        }
    }

    /// Whether text written now starts a line.
    fn at_line_start(&self) -> bool {
        self.captures.is_empty()
            && !self.in_code_block
            && (!self.line_open || self.out.len() == self.content_start)
    }

    fn prose(&mut self, text: &str) {
        let text = if self.flavor == Flavor::Discord {
            self.discord_text(text, self.links.is_empty(), self.at_line_start())
        } else {
            self.escape(text).into_owned()
        };
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
            let line_start = self.out.len() == self.content_start;
            if self.flavor == Flavor::WhatsApp && line_start && !self.in_code_block {
                self.out.push_str(&whatsapp_line_start(text));
            } else {
                self.out.push_str(text);
            }
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
        } else if matches!(self.flavor, Flavor::WhatsApp | Flavor::Discord)
            && !self.open_styles.is_empty()
        {
            // WhatsApp styles don't span lines: close them before the break, reopen after. So
            // do Discord's, which keeps a message that splits at a line break well formed.
            let styles = self.open_styles.clone();
            for style in styles.iter().rev() {
                self.inline(self.marker(*style));
            }
            self.end_line();
            for style in &styles {
                self.inline(self.marker(*style));
            }
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
            Flavor::Plain | Flavor::WhatsApp | Flavor::Discord => Sep::Blank(self.blank_prefix()),
        }
    }

    /// What an empty line inside the current containers holds: the quote prefixes.
    fn blank_prefix(&self) -> String {
        let prefix: String = self.containers.iter().map(|c| c.rest.as_str()).collect();
        if self.flavor == Flavor::Discord && prefix.contains('>') {
            // Discord reads a quote line only with a space after the `>`.
            return prefix;
        }
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
        self.content_start = self.out.len();
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

/// Replaces a WhatsApp block marker at the start of a line's content (`>`, `- `, or `1. `) with a
/// look-alike, so text that only resembles a quote or list doesn't become one.
fn whatsapp_line_start(text: &str) -> Cow<'_, str> {
    if let Some(rest) = text.strip_prefix('>') {
        return Cow::Owned(format!("\u{2c3}{rest}"));
    }
    if let Some(rest) = text.strip_prefix("- ") {
        return Cow::Owned(format!("\u{2010} {rest}"));
    }
    let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    match text[digits..].strip_prefix(". ") {
        Some(rest) if digits > 0 => Cow::Owned(format!("{}\u{2024} {rest}", &text[..digits])),
        _ => Cow::Borrowed(text),
    }
}

/// Replaces the WhatsApp markup characters in `text` that could open or close a style with
/// look-alikes, since WhatsApp has no escape character. `*`, `_`, and `~` between two
/// alphanumerics stay (WhatsApp only styles at word boundaries), and words that look like URLs are
/// left alone so they still work as links.
fn whatsapp_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_inclusive(char::is_whitespace) {
        if word.contains("://") || word.starts_with("www.") {
            out.push_str(word);
            continue;
        }
        let chars: Vec<char> = word.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            let lookalike = match c {
                '*' => '\u{2217}',
                '_' => '\u{2cd}',
                '~' => '\u{223c}',
                '`' => '\u{2cb}',
                _ => {
                    out.push(c);
                    continue;
                }
            };
            let inside_word = c != '`'
                && i > 0
                && chars[i - 1].is_alphanumeric()
                && chars.get(i + 1).is_some_and(|next| next.is_alphanumeric());
            out.push(if inside_word { c } else { lookalike });
        }
    }
    out
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

    #[test]
    fn renders_whatsapp() {
        let cases: &[(&str, &str, &str)] = &[
            ("empty", "", ""),
            (
                "inline styles",
                "**bold**, *it*, ~~gone~~, `x < y`, ***both***",
                "*bold*, _it_, ~gone~, `x < y`, _*both*_",
            ),
            ("nested strong writes one pair", "**a **b** c**", "*a b c*"),
            ("heading", "# Title **x**\n\nText", "*Title x*\n\nText"),
            (
                "styles close at line breaks",
                "**a\n*b*  \nc** d",
                "*a*\n*_b_*\n*c* d",
            ),
            (
                "lists",
                "- a\n  - b\n- [x] c\n\n1. d",
                "- a\n  - b\n- [x] c\n\n1. d",
            ),
            ("quote", "> a\n>\n> b", "> a\n>\n> b"),
            (
                "code block",
                "Run:\n\n```py\nx = a*b_c ~d `e`\n\nprint(x)\n```",
                "Run:\n\n```\nx = a*b_c ~d `e`\n\nprint(x)\n```",
            ),
            (
                "code block containing a fence",
                "````\n```\n````",
                "```\nˋˋˋ\n```",
            ),
            ("inline code with a backtick", "``a`b``", "aˋb"),
            (
                "literal markup",
                "2 \\* 3 \\*4\\* \\_x\\_ ~5 \\`y\\`",
                "2 ∗ 3 ∗4∗ ˍxˍ ∼5 ˋyˋ",
            ),
            (
                "inline html across lines",
                "**x <b\nc> y**",
                "*x <b*\n*c> y*",
            ),
            (
                "literal block markers",
                "1\\. not a list\n\n\\> not a quote\n\n\\- not an item\n\na\n\\- b\n\n\
                 | - x | y |\n|---|---|\n| 1 | 2 |\n\n- \\> z\n\n```\n- code\n> kept\n```",
                "1․ not a list\n\n˃ not a quote\n\n‐ not an item\n\na\n‐ b\n\n‐ x | y\n1 | 2\n\n\
                 - ˃ z\n\n```\n- code\n> kept\n```",
            ),
            (
                "markup inside words",
                "snake_case a\\*b\\*c x~y",
                "snake_case a*b*c x~y",
            ),
            (
                "bare urls",
                "see https://example.com/a_b/~c?d=e* and www.example.com/_x",
                "see https://example.com/a_b/~c?d=e* and www.example.com/_x",
            ),
            (
                "links",
                "[*x* \\_y](https://example.com/a_b) <https://example.com/~c_>",
                "_x_ ˍy <https://example.com/a_b> https://example.com/~c_",
            ),
            (
                "images",
                "![_alt_](https://attacker.example/?d=1)",
                "[image: alt]",
            ),
            (
                "table",
                "| a | *b* |\n|---|---|\n| 1 | 2 |",
                "a | _b_\n1 | 2",
            ),
            ("raw html", "<b>x</b>", "<b>x</b>"),
            (
                "control characters",
                "a\u{1b}[1mb\u{7}c\n\n```\nx\u{0}y\n```",
                "a[1mbc\n\n```\nxy\n```",
            ),
            ("multibyte", "*Grüße* 👋", "_Grüße_ 👋"),
        ];
        for (name, input, expected) in cases {
            assert_eq!(whatsapp(input), *expected, "case {name}");
        }
    }
}
