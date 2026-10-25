//! Splitting rendered WhatsApp text into messages of at most a given number of characters.
//!
//! WhatsApp accepts very long messages (about 65,000 characters), but a reply that long is a wall
//! of text in one bubble. The plugin splits at 4,096 characters, the size of WhatsApp's own
//! business API messages, preferring paragraph breaks, then line breaks, then spaces. A code block
//! cut in the middle is closed at the end of one message and reopened at the start of the next,
//! so each part renders as code.

/// The characters kept free in every part for closing and reopening a code fence
/// (`\n` and three backticks on each side).
const FENCE_RESERVE: usize = 8;
/// The fence marker.
const FENCE: &str = "```";

/// Splits `text` into parts of at most `limit` characters (at least [`MIN_LIMIT`]). Returns no
/// parts for text that is empty or only whitespace. Text that fits is returned whole, with its
/// surrounding blank lines trimmed.
pub(crate) fn split(text: &str, limit: usize) -> Vec<String> {
    assert!(limit >= MIN_LIMIT, "limit {limit} is below {MIN_LIMIT}");
    let budget = limit - FENCE_RESERVE;
    let lines: Vec<String> = text
        .trim_matches(['\n', '\r'])
        .split('\n')
        .flat_map(|line| pieces(line.trim_end_matches('\r'), budget))
        .collect();
    // Whether each line starts inside a code fence.
    let mut fence_before = Vec::with_capacity(lines.len() + 1);
    let mut open = false;
    for line in &lines {
        fence_before.push(open);
        if is_fence(line) {
            open = !open;
        }
    }
    fence_before.push(open);

    let mut parts = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        // The fence reopened at the top of this part costs a line.
        let reopened = fence_before[start];
        let mut size = if reopened { FENCE.len() } else { 0 };
        let mut end = start;
        while end < lines.len() {
            let add = lines[end].chars().count() + usize::from(end > start || reopened);
            if size + add > budget && end > start {
                break;
            }
            size += add;
            end += 1;
        }
        // Prefer to end at a paragraph break outside a code fence.
        let mut next = end;
        if end < lines.len()
            && let Some(blank) = (start + 1..end)
                .rev()
                .find(|&k| lines[k].trim().is_empty() && !fence_before[k])
        {
            end = blank;
            next = blank + 1;
        }
        let mut part = String::new();
        if reopened {
            part.push_str(FENCE);
            part.push('\n');
        }
        part.push_str(&lines[start..end].join("\n"));
        if fence_before[end] {
            part.push('\n');
            part.push_str(FENCE);
        }
        let part = part.trim_matches('\n');
        if !part.trim().is_empty() {
            parts.push(part.to_owned());
        }
        start = next;
    }
    parts
}

/// The smallest `limit` [`split`] accepts.
pub(crate) const MIN_LIMIT: usize = 64;

/// Whether the line opens or closes a code fence.
fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with(FENCE)
}

/// One line as pieces of at most `budget` characters, split at the last space that fits (the
/// space is dropped) or, with none, at a character boundary.
fn pieces(line: &str, budget: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = line;
    while rest.chars().count() > budget {
        // The byte offset after `budget` characters.
        let cut = rest
            .char_indices()
            .nth(budget)
            .map_or(rest.len(), |(offset, _)| offset);
        match rest[..cut].rfind(' ').filter(|&space| space > 0) {
            Some(space) => {
                pieces.push(rest[..space].to_owned());
                rest = rest[space + 1..].trim_start_matches(' ');
            }
            None => {
                pieces.push(rest[..cut].to_owned());
                rest = &rest[cut..];
            }
        }
    }
    pieces.push(rest.to_owned());
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lengths(parts: &[String]) -> Vec<usize> {
        parts.iter().map(|p| p.chars().count()).collect()
    }

    #[test]
    fn short_text_is_one_part() {
        assert_eq!(split("hello", 100), ["hello"]);
        assert_eq!(split("\n\nhello\nworld\n\n", 100), ["hello\nworld"]);
    }

    #[test]
    fn empty_text_has_no_parts() {
        assert!(split("", 100).is_empty());
        assert!(split(" \n\n ", 100).is_empty());
    }

    #[test]
    fn paragraphs_are_kept_whole_when_they_fit() {
        let a = "a".repeat(50);
        let b = "b".repeat(50);
        let c = "c".repeat(50);
        let parts = split(&format!("{a}\n\n{b}\n\n{c}"), 120);
        // Two paragraphs would fit in 120 characters with the blank line, but a third wouldn't.
        assert_eq!(parts, [format!("{a}\n\n{b}"), c]);
    }

    #[test]
    fn a_part_ends_at_the_last_paragraph_break_that_fits() {
        let text = format!(
            "{}\n\n{}\n{}",
            "a".repeat(30),
            "b".repeat(30),
            "c".repeat(30)
        );
        let parts = split(&text, 80);
        assert_eq!(
            parts,
            [
                "a".repeat(30),
                format!("{}\n{}", "b".repeat(30), "c".repeat(30))
            ]
        );
    }

    #[test]
    fn with_no_paragraph_break_it_splits_at_a_line() {
        let lines: Vec<String> = (0..10).map(|i| format!("line number {i:02}")).collect();
        let parts = split(&lines.join("\n"), 70);
        assert!(parts.len() > 1);
        assert!(lengths(&parts).iter().all(|&n| n <= 70));
        // Every line survives whole, in order.
        assert_eq!(parts.join("\n"), lines.join("\n"));
    }

    #[test]
    fn a_long_line_splits_at_spaces_and_then_anywhere() {
        let words = "word ".repeat(40);
        let parts = split(words.trim_end(), 64);
        assert!(
            lengths(&parts).iter().all(|&n| n <= 64),
            "{:?}",
            lengths(&parts)
        );
        assert_eq!(parts.join(" "), words.trim_end());

        let solid = "x".repeat(200);
        let parts = split(&solid, 64);
        assert!(lengths(&parts).iter().all(|&n| n <= 64));
        assert_eq!(parts.concat(), solid);
    }

    #[test]
    fn it_counts_characters_not_bytes() {
        let text = "é".repeat(200);
        let parts = split(&text, 64);
        assert!(lengths(&parts).iter().all(|&n| n <= 64));
        assert_eq!(parts.concat(), text);
        let text = "😀".repeat(100);
        let parts = split(&text, 64);
        assert!(lengths(&parts).iter().all(|&n| n <= 64));
        assert_eq!(parts.concat(), text);
    }

    #[test]
    fn a_code_block_cut_in_the_middle_is_closed_and_reopened() {
        let code: Vec<String> = (0..12).map(|i| format!("let value_{i} = {i};")).collect();
        let text = format!("intro\n```\n{}\n```\noutro", code.join("\n"));
        let parts = split(&text, 100);
        assert!(parts.len() >= 2, "{parts:?}");
        for part in &parts {
            assert!(part.chars().count() <= 100, "{part}");
            // Every part has balanced fences.
            let fences = part.lines().filter(|l| l.starts_with("```")).count();
            assert_eq!(fences % 2, 0, "{part}");
        }
        // The code survives in order.
        let joined = parts.join("\n");
        for line in &code {
            assert!(joined.contains(line.as_str()), "{line} lost");
        }
        assert!(parts[0].starts_with("intro"));
        assert!(parts.last().unwrap().ends_with("outro"));
    }

    #[test]
    fn a_blank_line_inside_a_code_block_is_not_a_split_point() {
        let text = format!("```\n{}\n\n{}\n```", "a".repeat(40), "b".repeat(40));
        let parts = split(&text, 80);
        // Cut inside the block, which is reopened: never at the blank line alone.
        assert!(parts.len() >= 2);
        for part in &parts {
            assert!(part.starts_with("```") && part.ends_with("```"), "{part}");
        }
    }

    #[test]
    fn every_part_fits() {
        let mut text = String::new();
        for i in 0..400 {
            text.push_str(&format!("paragraph {i} with a few words in it\n\n"));
            if i % 7 == 0 {
                text.push_str("```\ncode();\nmore();\n```\n\n");
            }
        }
        for limit in [64, 100, 500, 4096] {
            let parts = split(&text, limit);
            assert!(lengths(&parts).iter().all(|&n| n <= limit), "limit {limit}");
            assert!(parts.iter().all(|p| !p.trim().is_empty()));
        }
    }
}
