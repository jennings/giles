//! What model text needs so Discord shows it literally: no accidental formatting, no pings, and
//! no embeds fetched from the URLs it mentions.

use std::fmt::Write as _;

use super::super::html::starts_with_ignore_case;
use super::super::strip_controls;

/// Zero-width space: invisible, and breaks a mention or a run of backticks without changing how the
/// text reads.
pub(in super::super) const ZWSP: char = '\u{200b}';

/// Escapes prose so Discord shows it as written.
///
/// - Markup characters get a backslash, which Discord removes when rendering: backslash, `*`, `~`,
///   backtick, `[`, `]`, `_` (except inside a word), `|` (next to another `|`), and `<` (before a
///   non-space).
/// - Block markers at the start of a line (`#`, `>`, `-`, `+`, `-#`, and `1.`) are escaped when
///   `line_start` is set.
/// - `@everyone`, `@here`, and the `<@id>` family can't ping: `@` and `<` are followed by a
///   zero-width space where they would form one.
/// - With `wrap_urls`, a bare `http(s)://` URL becomes `<url>`: still a link, but without an
///   embed (and without its `_` and `*` read as markup).
pub(in super::super) fn prose(text: &str, wrap_urls: bool, line_start: bool) -> String {
    let text = strip_controls(text);
    let mut out = String::with_capacity(text.len() + 8);
    let numbered = if line_start {
        numbered_dot(&text)
    } else {
        None
    };
    if line_start && block_marker(&text) {
        out.push('\\');
    }
    let mut prev: Option<char> = None;
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        if wrap_urls
            && prev.is_none_or(|p| p.is_whitespace() || p == '(')
            && let Some(len) = bare_url(rest)
        {
            out.push('<');
            out.push_str(&url(&rest[..len], false));
            out.push('>');
            prev = rest[..len].chars().next_back();
            i += len;
            continue;
        }
        let Some(c) = rest.chars().next() else { break };
        let after = &rest[c.len_utf8()..];
        let next = after.chars().next();
        match c {
            '\\' | '*' | '~' | '`' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            '_' => {
                let inside_word = prev.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric);
                if !inside_word {
                    out.push('\\');
                }
                out.push('_');
            }
            '|' => {
                if prev == Some('|') || next == Some('|') {
                    out.push('\\');
                }
                out.push('|');
            }
            '<' => {
                if next.is_none_or(|n| !n.is_whitespace()) {
                    out.push('\\');
                }
                out.push('<');
                if mention_like(after) {
                    out.push(ZWSP);
                }
            }
            '@' => {
                out.push('@');
                if pings_soon(after) {
                    out.push(ZWSP);
                }
            }
            '.' if numbered == Some(i) => {
                out.push('\\');
                out.push('.');
            }
            _ => out.push(c),
        }
        prev = Some(c);
        i += c.len_utf8();
    }
    out
}

/// Makes text safe inside code, where backslashes don't escape: only mentions are defused, and
/// runs of backticks (which would end a span or fence) are broken up with zero-width spaces.
pub(in super::super) fn code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, c) in text.char_indices() {
        let after = &text[i + c.len_utf8()..];
        out.push(c);
        let defuse = match c {
            '`' => after.starts_with('`'),
            '@' => pings_soon(after),
            '<' => mention_like(after),
            _ => false,
        };
        if defuse {
            out.push(ZWSP);
        }
    }
    out
}

/// Writes `code` as an inline code span, with a delimiter that its content can't end.
pub(in super::super) fn code_span(content: &str) -> String {
    let content = code(content);
    // `code` leaves runs of one backtick at most, so two backticks always delimit.
    let delimiter = if content.contains('`') { "``" } else { "`" };
    let pad = if content.starts_with('`') || content.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{delimiter}{pad}{content}{pad}{delimiter}")
}

/// Percent-encodes what can't appear in a link target written as `<url>`, so the target ends
/// where the renderer says it does. With `masked`, also what ends a `[text](<url>)` target
/// early. `@everyone` and `@here` in a URL are defused too: a URL is text to Discord's mention
/// scan.
pub(in super::super) fn url(url: &str, masked: bool) -> String {
    let mut out = String::with_capacity(url.len());
    for (i, c) in url.char_indices() {
        let encode = match c {
            '@' => pings(&url[i + 1..]),
            '<' | '>' => true,
            '(' | ')' | '\\' | '`' => masked,
            _ => c.is_control() || c.is_whitespace(),
        };
        if encode {
            let mut buf = [0; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                let _ = write!(out, "%{byte:02X}");
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether Discord makes a link of `target` (it ignores other schemes in masked links).
pub(in super::super) fn is_web_url(target: &str) -> bool {
    starts_with_ignore_case(target, "https://") || starts_with_ignore_case(target, "http://")
}

/// Whether text after an `@` is `everyone` or `here`.
fn pings(after: &str) -> bool {
    starts_with_ignore_case(after, "everyone") || starts_with_ignore_case(after, "here")
}

/// Whether text after an `@` is, or at its end may become, `everyone` or `here`: other inline
/// pieces (an empty link, a style marker dropped in a table) can follow the text and complete the
/// word, so a piece that ends inside it counts.
fn pings_soon(after: &str) -> bool {
    let prefix_of = |word: &str| {
        after.len() < word.len()
            && word.as_bytes()[..after.len()].eq_ignore_ascii_case(after.as_bytes())
    };
    pings(after) || prefix_of("everyone") || prefix_of("here")
}

/// Whether text after a `<` is one of Discord's `<...>` forms: user, role, and channel mentions,
/// slash commands, custom emoji (`<:name:id>`, `<a:name:id>`), timestamps (`<t:...>`), and guild
/// navigation (`<id:...>`). A piece of text that ends inside the form counts, since other inline
/// pieces can follow it and complete the form.
fn mention_like(after: &str) -> bool {
    match after.chars().next() {
        None | Some('@' | '#' | ':' | '$') => true,
        // A slash command has a name and an id, `</name:id>`; `</b>` is just a closing tag.
        Some('/') => {
            let tag = &after[1..];
            match tag.find(['>', '<', '\n']) {
                Some(end) => tag[end..].starts_with('>') && tag[..end].contains(':'),
                None => true,
            }
        }
        Some(c) if c.is_ascii_alphabetic() => {
            let rest = after.trim_start_matches(|c: char| c.is_ascii_alphabetic());
            rest.is_empty() || rest.starts_with(':')
        }
        _ => false,
    }
}

/// Whether `text` starts like a Discord quote, heading, subtext, or bullet.
fn block_marker(text: &str) -> bool {
    match text.chars().next() {
        Some('>') => true,
        Some('#') => text.trim_start_matches('#').starts_with(' '),
        Some('-') => text[1..].starts_with(' ') || text.starts_with("-# "),
        Some('+') => text[1..].starts_with(' '),
        _ => false,
    }
}

/// Whether `text` starts like a Discord quote, heading, subtext, bullet, or numbered item.
pub(super) fn starts_block(text: &str) -> bool {
    block_marker(text) || numbered_dot(text).is_some()
}

/// The offset of the `.` in a leading `1. ` (a digit can't be escaped, so the dot is).
fn numbered_dot(text: &str) -> Option<usize> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0 && text[digits..].starts_with(". ")).then_some(digits)
}

/// Escapes text that starts a line, and so would start a quote, heading, or list: the text of a
/// link that Discord doesn't make one, which is written at the start of the line as it is.
pub(in super::super) fn at_line_start(text: &str) -> String {
    if block_marker(text) {
        format!("\\{text}")
    } else if let Some(dot) = numbered_dot(text) {
        format!("{}\\{}", &text[..dot], &text[dot..])
    } else {
        text.to_owned()
    }
}

/// The length of the bare URL at the start of `rest`, without the punctuation a sentence puts
/// after one.
fn bare_url(rest: &str) -> Option<usize> {
    let scheme = if starts_with_ignore_case(rest, "https://") {
        8
    } else if starts_with_ignore_case(rest, "http://") {
        7
    } else {
        return None;
    };
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '<' || c == '>')
        .unwrap_or(rest.len());
    let mut url = &rest[..end];
    let count = |c: char| url.matches(c).count();
    // Closers a sentence doesn't own are those the URL has an opener for; count once, then
    // keep the tallies as closers are trimmed.
    let (mut parens, mut brackets) = ((count('('), count(')')), (count('['), count(']')));
    while let Some(last) = url.chars().next_back() {
        let trailing = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '*' | '_' | '~' => true,
            ')' if parens.0 < parens.1 => {
                parens.1 -= 1;
                true
            }
            ']' if brackets.0 < brackets.1 => {
                brackets.1 -= 1;
                true
            }
            _ => false,
        };
        if !trailing {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    (url.len() > scheme).then_some(url.len())
}
