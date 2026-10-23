//! YAML front-matter in Markdown files (`SKILL.md`, memory files): a block that starts on the first
//! line with `---` and ends at the next line that is exactly `---`.
//!
//! Only the block's boundaries are found here. Callers parse the YAML themselves (skills) or keep
//! the block verbatim (memory files).

/// A file split at the end of its front-matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Split<'a> {
    /// The file up to the closing `---` line: a byte-order mark if any, the opening `---` line, and
    /// the YAML. This is itself a YAML stream (`---` starts a document), so a parser's line
    /// numbers match the file's.
    pub document: &'a str,
    /// The YAML between the two `---` lines.
    pub yaml: &'a str,
    /// Everything after the closing `---` line and its line ending.
    pub body: &'a str,
}

/// Splits off the front-matter, or returns `None` when `text` has none: its first line (after an
/// optional byte-order mark) is not `---`, or no later line closes the block.
///
/// Delimiter lines may end in `\n` or `\r\n` and may carry trailing spaces or tabs.
pub(crate) fn split(text: &str) -> Option<Split<'_>> {
    let start = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let mut lines = text[start..].split_inclusive('\n');
    let first = lines.next()?;
    if !is_delimiter(first) {
        return None;
    }
    let yaml_start = start + first.len();
    let mut offset = yaml_start;
    for line in lines {
        if is_delimiter(line) {
            return Some(Split {
                document: &text[..offset],
                yaml: &text[yaml_start..offset],
                body: &text[offset + line.len()..],
            });
        }
        offset += line.len();
    }
    None
}

/// Whether `line` (with its line ending, if any) is a `---` delimiter.
fn is_delimiter(line: &str) -> bool {
    line.trim_end_matches(['\n', '\r', ' ', '\t']) == "---"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lf_and_crlf() {
        let s = split("---\nname: x\n---\nbody\n").unwrap();
        assert_eq!(
            (s.document, s.yaml, s.body),
            ("---\nname: x\n", "name: x\n", "body\n")
        );

        let s = split("---\r\nname: x\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(s.document, "---\r\nname: x\r\n");
        assert_eq!(s.yaml, "name: x\r\n");
        assert_eq!(s.body, "body\r\n");
    }

    #[test]
    fn keeps_bom_in_document() {
        let s = split("\u{feff}---\na: 1\n---\nrest").unwrap();
        assert_eq!(s.document, "\u{feff}---\na: 1\n");
        assert_eq!(s.yaml, "a: 1\n");
        assert_eq!(s.body, "rest");
    }

    #[test]
    fn empty_block_and_closing_line_at_end_of_file() {
        let s = split("---\n---").unwrap();
        assert_eq!((s.document, s.yaml, s.body), ("---\n", "", ""));
    }

    #[test]
    fn delimiters_tolerate_trailing_whitespace_only() {
        assert!(split("--- \na: 1\n---\t\n").is_some());
        assert_eq!(split(" ---\na: 1\n---\n"), None);
        assert_eq!(split("----\na: 1\n----\n"), None);
        // `---x` inside the block is YAML, not the closing line.
        assert_eq!(
            split("---\na: |\n  ---x\n---\n").unwrap().yaml,
            "a: |\n  ---x\n"
        );
    }

    #[test]
    fn none_without_opening_or_closing_line() {
        assert_eq!(split("no front-matter\n---\n"), None);
        assert_eq!(split("---\nname: x\nnever closed\n"), None);
        assert_eq!(split(""), None);
        assert_eq!(split("---"), None);
    }
}
