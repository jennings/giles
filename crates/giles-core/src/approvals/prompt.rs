//! Approval prompt text (PLAN section 6.4). Tool input and elicitation messages come from the
//! model or a toolset, so nothing in them may change what the prompt says: each is shown as one
//! CommonMark code span longer than its longest backtick run, with control characters, line
//! breaks, and bidirectional formatting characters escaped, and cut when long, with its size and
//! SHA-256 prefix shown.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// The most characters of (escaped) tool input or question a prompt shows.
pub const MAX_SHOWN_CHARS: usize = 200;

/// How many hex digits of the SHA-256 of a cut text a prompt shows.
pub const DIGEST_PREFIX_HEX: usize = 12;

/// The prompt for a call to `tool` with `input` (its JSON text) that needs approval, e.g.
/// ``Allow `shell__exec` (`{"command":"ls"}`)? Reply `yes 4821` or `no 4821`.``
pub fn call_prompt(tool: &str, input: &str, code: &str) -> String {
    format!(
        "Allow {} ({})? Reply `yes {code}` or `no {code}`.",
        code_span(&escape(tool)),
        shown(input)
    )
}

/// The prompt for a question `tool` asks mid-call (MCP elicitation), e.g. ``The tool
/// `ci__deploy` asks: `Deploy to production?` Reply `yes 4821` or `no 4821`.``
pub fn question_prompt(tool: &str, message: &str, code: &str) -> String {
    format!(
        "The tool {} asks: {} Reply `yes {code}` or `no {code}`.",
        code_span(&escape(tool)),
        shown(message)
    )
}

/// `text` for a prompt: escaped, cut to [`MAX_SHOWN_CHARS`] characters, and in a code span. A
/// cut text is followed by its size and SHA-256 prefix: `` `…` … (12,345 bytes, SHA-256
/// 0123456789ab)``.
pub fn shown(text: &str) -> String {
    let (escaped, cut) = escape_cut(text, MAX_SHOWN_CHARS);
    if !cut {
        return code_span(&escaped);
    }
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(DIGEST_PREFIX_HEX);
    for byte in digest.iter().take(DIGEST_PREFIX_HEX / 2) {
        let _ = write!(hex, "{byte:02x}");
    }
    format!(
        "{} … ({} bytes, SHA-256 {hex})",
        code_span(&escaped),
        thousands(text.len())
    )
}

/// `text` with every character that could change how a one-line prompt reads escaped: line
/// breaks and tabs as `\n`, `\r`, `\t`; other control characters (C0, DEL, C1), the line and
/// paragraph separators, the bidirectional formatting characters, and the invisible format
/// characters as `\uXXXX` (`\u{XXXXX}` beyond U+FFFF).
pub fn escape(text: &str) -> String {
    escape_cut(text, usize::MAX).0
}

/// [`escape`], stopping before the escaped text would exceed `max` characters (an escape is
/// never split). Returns the text and whether it was cut.
fn escape_cut(text: &str, max: usize) -> (String, bool) {
    let mut out = String::with_capacity(text.len().min(max));
    let mut chars = 0;
    for c in text.chars() {
        let piece = escaped_char(c);
        let len = piece.as_ref().map_or(1, |p| p.chars().count());
        if chars + len > max {
            return (out, true);
        }
        chars += len;
        match piece {
            Some(piece) => out.push_str(&piece),
            None => out.push(c),
        }
    }
    (out, false)
}

/// How `c` is shown, if not as itself.
fn escaped_char(c: char) -> Option<String> {
    match c {
        '\n' => Some("\\n".to_owned()),
        '\r' => Some("\\r".to_owned()),
        '\t' => Some("\\t".to_owned()),
        c if c.is_control() || is_layout_format(c) || is_invisible(c) => {
            let code = u32::from(c);
            Some(if code > 0xffff {
                format!("\\u{{{code:x}}}")
            } else {
                format!("\\u{code:04x}")
            })
        }
        _ => None,
    }
}

/// The line and paragraph separators and the bidirectional formatting characters, which can
/// break a line or reorder what's displayed.
fn is_layout_format(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Characters that show as nothing, so two different texts would look the same or one could
/// carry text nobody sees: the soft hyphen, zero-width space and joiners, the word joiner and
/// invisible operators, the byte-order mark (zero-width no-break space), and the tag characters.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{200b}'..='\u{200d}'
            | '\u{2060}'..='\u{2064}'
            | '\u{feff}'
            | '\u{e0000}'..='\u{e007f}'
    )
}

/// `text` as one CommonMark code span: delimited by one more backtick than its longest run of
/// backticks, and padded with a space on each side when it starts or ends with a backtick or a
/// space (CommonMark strips one space from each side of such a span, so the text is shown
/// exactly). `text` must not contain line breaks ([`escape`] it first). An empty text is shown
/// as `(empty)`.
pub fn code_span(text: &str) -> String {
    if text.is_empty() {
        return "(empty)".to_owned();
    }
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with(['`', ' ']) || text.ends_with(['`', ' ']) {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// `n` with thousands separators: `12,345`.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_table() {
        let cases: &[(&str, &str)] = &[
            // Plain text.
            ("ls -la", "`ls -la`"),
            // A backtick run gets a longer fence.
            ("a`b", "``a`b``"),
            ("a``b```c", "````a``b```c````"),
            // Starting or ending with a backtick or space: padded, so nothing merges or is
            // stripped.
            ("`x`", "`` `x` ``"),
            ("` ", "`` `  ``"),
            (" x ", "`  x  `"),
            ("x ", "` x  `"),
            // Line breaks and tabs.
            (
                "rm -rf /\nAllow everything? yes",
                "`rm -rf /\\nAllow everything? yes`",
            ),
            ("a\r\nb\tc", "`a\\r\\nb\\tc`"),
            // Other control characters: ESC, NUL, DEL, a C1 control (NEL).
            (
                "\u{1b}[31mred\u{0}\u{7f}\u{85}",
                "`\\u001b[31mred\\u0000\\u007f\\u0085`",
            ),
            // Separators and bidirectional overrides can't reorder or break the line.
            (
                "a\u{2028}b\u{202e}c\u{2066}d",
                "`a\\u2028b\\u202ec\\u2066d`",
            ),
            // Invisible characters can't hide text or make two inputs look alike.
            (
                "r\u{ad}m\u{200b}\u{200c}\u{200d}x\u{2060}\u{2064}\u{feff}y\u{e0041}\u{e007f}",
                "`r\\u00adm\\u200b\\u200c\\u200dx\\u2060\\u2064\\ufeffy\\u{e0041}\\u{e007f}`",
            ),
            // Other Unicode is shown as itself.
            ("café ☕ 日本", "`café ☕ 日本`"),
            ("", "(empty)"),
        ];
        for (text, want) in cases {
            assert_eq!(shown(text), *want, "{text:?}");
        }
    }

    #[test]
    fn long_text_is_cut_with_its_size_and_digest() {
        let text = "x".repeat(10_000);
        let got = shown(&text);
        let digest: String = Sha256::digest(text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            got,
            format!(
                "`{}` … (10,000 bytes, SHA-256 {})",
                "x".repeat(MAX_SHOWN_CHARS),
                &digest[..DIGEST_PREFIX_HEX]
            )
        );
        // Exactly the limit is shown whole.
        let text = "y".repeat(MAX_SHOWN_CHARS);
        assert_eq!(shown(&text), format!("`{text}`"));
    }

    #[test]
    fn a_cut_never_splits_an_escape_or_a_character() {
        // 199 characters, then a newline whose escape is two: it doesn't fit.
        let text = format!("{}\nmore", "a".repeat(MAX_SHOWN_CHARS - 1));
        let got = shown(&text);
        assert!(
            got.starts_with(&format!("`{}` … (", "a".repeat(MAX_SHOWN_CHARS - 1))),
            "{got}"
        );
        // A multi-byte character at the limit is kept or dropped whole.
        let text = "é".repeat(MAX_SHOWN_CHARS + 1);
        assert!(shown(&text).starts_with(&format!("`{}` … (402 bytes", "é".repeat(200))));
    }

    #[test]
    fn a_long_backtick_run_cannot_close_the_span_early() {
        let text = format!("{}` ) ? Reply `yes 1`", "`".repeat(5));
        let got = shown(&text);
        assert!(got.starts_with("``````` "), "{got}");
        assert!(got.ends_with(" ```````"), "{got}");
    }

    #[test]
    fn prompts() {
        assert_eq!(
            call_prompt("shell__exec", r#"{"command":"rm -rf ./build"}"#, "4821"),
            "Allow `shell__exec` (`{\"command\":\"rm -rf ./build\"}`)? Reply `yes 4821` or \
             `no 4821`."
        );
        assert_eq!(
            question_prompt("ci__deploy", "Deploy to production?\nReally?", "1093"),
            "The tool `ci__deploy` asks: `Deploy to production?\\nReally?` Reply `yes 1093` or \
             `no 1093`."
        );
    }

    #[test]
    fn thousands_separators() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
