//! CommonMark to IRC messages, with formatting codes and line-limit splitting.

use super::text::{self, Flavor};

/// Toggles bold.
pub(super) const IRC_BOLD: &str = "\x02";
/// Toggles italics.
pub(super) const IRC_ITALIC: &str = "\x1d";

/// Renders CommonMark as IRC message lines, each at most `max_line_bytes` bytes long.
///
/// - Strong text and headings are bold (`\x02`), emphasis italic (`\x1d`); strikethrough is
///   `~~text~~`, inline code `` `code` ``.
/// - Code blocks are plain lines with their indentation, without formatting.
/// - Lists use `-` and `1.` markers, quotes a `> ` prefix, tables `|` between cells.
/// - Links show their target as `text <url>`; images show only `[image: alt]`.
/// - Control characters in the input (IRC formatting and color codes, CTCP's `\x01`) are dropped,
///   so the model can't inject them.
/// - Empty lines are dropped: IRC can't send an empty message.
///
/// `max_line_bytes` is the room left for the text of one message:
/// `512 - 2 - len("PRIVMSG <target> :") - len(":<nick>!<user>@<host> ")`, that is, the line limit
/// minus the CRLF, the command and target, and the source prefix the server adds when relaying.
/// The relayed host is often unknown or cloaked; when it is, assume a long one (63 bytes, the
/// usual server `HOSTLEN`).
/// Long lines are split at UTF-8 character boundaries, at the last space that fits if there is
/// one, and formatting still open at a split is reopened at the start of the next line (IRC
/// clients reset formatting at the end of every message). A line exceeds the limit only if a
/// single character doesn't fit, when `max_line_bytes` is below 4.
pub fn irc(markdown: &str, max_line_bytes: usize) -> Vec<String> {
    split_lines(&text::render(markdown, Flavor::Irc), max_line_bytes)
}

/// The formatting open at a point in the text.
#[derive(Clone, Copy, Default)]
struct Format {
    bold: bool,
    italic: bool,
}

impl Format {
    /// Updates the state with the codes in `text`.
    fn apply(&mut self, text: &str) {
        for c in text.chars() {
            match c {
                '\x02' => self.bold = !self.bold,
                '\x1d' => self.italic = !self.italic,
                _ => {}
            }
        }
    }

    /// The codes that reopen this formatting at the start of a message.
    fn codes(self) -> &'static str {
        match (self.bold, self.italic) {
            (false, false) => "",
            (true, false) => IRC_BOLD,
            (false, true) => IRC_ITALIC,
            (true, true) => "\x02\x1d",
        }
    }
}

/// Splits rendered text into messages of at most `max` bytes, reopening formatting on each.
fn split_lines(text: &str, max: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut format = Format::default();
    for line in text.split('\n') {
        let mut rest = line;
        loop {
            if !has_visible(rest) {
                format.apply(rest);
                break;
            }
            let first_char = rest.chars().next().map_or(0, char::len_utf8);
            let reopen = match format.codes() {
                codes if codes.len() + first_char <= max => codes,
                _ => "",
            };
            let budget = max - reopen.len().min(max);
            let end = if rest.len() <= budget {
                rest.len()
            } else {
                split_point(rest, budget)
            };
            let (chunk, tail) = rest.split_at(end);
            // A chunk of formatting codes alone (only when the limit is tiny) isn't worth a
            // message; its codes still count.
            if has_visible(chunk) {
                lines.push(format!("{reopen}{}", chunk.trim_end_matches(' ')));
            }
            format.apply(chunk);
            // Continue after the break: codes there only change the formatting to reopen, and
            // the spaces around them are the break itself.
            let next = tail.trim_start_matches([' ', '\x02', '\x1d']);
            format.apply(&tail[..tail.len() - next.len()]);
            rest = next;
        }
    }
    lines
}

/// Where to end a message taken from the start of `text`, which is longer than `budget` bytes.
fn split_point(text: &str, budget: usize) -> usize {
    let space = text.as_bytes()[..=budget]
        .iter()
        .rposition(|&b| b == b' ')
        .filter(|&i| has_visible(&text[..i]));
    match space {
        Some(i) => i,
        None => match text.floor_char_boundary(budget) {
            // Always make progress, even if the first character alone exceeds the budget.
            0 => text.chars().next().map_or(0, char::len_utf8),
            end => end,
        },
    }
}

/// Whether `text` has anything to show besides whitespace and formatting codes.
fn has_visible(text: &str) -> bool {
    text.chars()
        .any(|c| !c.is_whitespace() && c != '\x02' && c != '\x1d')
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: usize = 400;

    #[test]
    fn renders_irc() {
        let cases: &[(&str, &str, &[&str])] = &[
            ("empty", "", &[]),
            ("blank", "\n\n  \n", &[]),
            (
                "styles",
                "**bold**, *it*, ***both***, ~~gone~~, `code`",
                &["\x02bold\x02, \x1dit\x1d, \x1d\x02both\x02\x1d, ~~gone~~, `code`"],
            ),
            ("heading", "# Title **bold**", &["\x02Title bold\x02"]),
            ("paragraphs", "one\ntwo\n\nthree", &["one", "two", "three"]),
            (
                "lists",
                "- a\n  - b\n1. c\n2. d",
                &["- a", "  - b", "1. c", "2. d"],
            ),
            ("quote", "> a\n>\n> b", &["> a", "> b"]),
            (
                "code block",
                "```rust\nfn main() {\n\n    **x**\n}\n```",
                &["fn main() {", "    **x**", "}"],
            ),
            (
                "links and images",
                "[docs](https://example.com) <https://example.com> \
                 ![x](https://attacker.example/?d=1)",
                &["docs <https://example.com> https://example.com [image: x]"],
            ),
            (
                "table",
                "| a | b |\n|---|---|\n| 1 | 2 |",
                &["a | b", "1 | 2"],
            ),
            ("raw html", "<b>hi</b>", &["<b>hi</b>"]),
            (
                "input control codes",
                "a\x02b\x03 4,5red\x0f\x16\x1d\x1e\x11 \x01ACTION waves\x01",
                &["ab 4,5red ACTION waves"],
            ),
            (
                "control codes in code, links, and alt text",
                "`a\x02b` [c\x03d](https://e/f) ![g\x1dh](x)\n\n```\ni\x02j\n```",
                &["`ab` cd <https://e/f> [image: gh]", "ij"],
            ),
            (
                "bold across a line break",
                "**a\nb** c",
                &["\x02a", "\x02b\x02 c"],
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(irc(input, LIMIT), *expected, "case {name}");
        }
    }

    #[test]
    fn splits_long_lines() {
        let cases: &[(&str, &str, usize, &[&str])] = &[
            ("fits", "aaaa bbbb", 9, &["aaaa bbbb"]),
            ("at a space", "aaaa bbbb cccc", 10, &["aaaa bbbb", "cccc"]),
            ("collapses the break", "aaaa    bbbb", 6, &["aaaa", "bbbb"]),
            ("no space", "abcdefghij", 4, &["abcd", "efgh", "ij"]),
            ("two-byte characters", "ééééé", 5, &["éé", "éé", "é"]),
            ("four-byte characters", "👋👋👋", 7, &["👋", "👋", "👋"]),
            ("mixed widths", "a€b€c", 4, &["a€", "b€", "c"]),
            (
                "keeps code indentation",
                "```\n    indented line\n```",
                12,
                &["    indented", "line"],
            ),
            (
                "reopens bold",
                "**aaaa bbbb**",
                7,
                &["\x02aaaa", "\x02bbbb\x02"],
            ),
            (
                "reopens bold and italic",
                "***aaaa bbbb cccc***",
                8,
                &["\x1d\x02aaaa", "\x02\x1dbbbb", "\x02\x1dcccc\x02\x1d"],
            ),
            ("character larger than limit", "éa", 1, &["é", "a"]),
            ("no room to reopen", "**ab**", 1, &["a", "b"]),
            (
                "codes after the break",
                "*it **both** it* tail words here",
                6,
                &[
                    "\x1dit",
                    "\x02\x1dboth",
                    "\x1dit\x1d",
                    "tail",
                    "words",
                    "here",
                ],
            ),
        ];
        for (name, input, limit, expected) in cases {
            assert_eq!(irc(input, *limit), *expected, "case {name}");
        }
    }

    #[test]
    fn lines_never_exceed_the_limit() {
        let input = "# Überschrift mit **Fettdruck** und *Kursiv*\n\n\
                     Ein langer Absatz mit **fett *und kursiv* gemischt** sowie Emoji 👋🏽 und \
                     CJK 漢字かな交じり文, gefolgt von einer URL \
                     <https://example.com/ein/sehr/langer/pfad/ohne/leerzeichen>.\n\n\
                     - Liste mit `Code` und ~~Durchgestrichen~~\n\n\
                     ```\nfn lange_zeile() { println!(\"{}\", \"x\".repeat(100)); }\n```";
        let words: String = text::render(input, Flavor::Irc)
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '\x02' && *c != '\x1d')
            .collect();
        for limit in 4..80 {
            let lines = irc(input, limit);
            for line in &lines {
                assert!(line.len() <= limit, "limit {limit}: {line:?}");
                assert!(!line.is_empty(), "limit {limit}: empty line");
            }
            let rejoined: String = lines
                .concat()
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '\x02' && *c != '\x1d')
                .collect();
            assert_eq!(rejoined, words, "limit {limit} lost text");
        }
    }
}
