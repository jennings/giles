//! A child process's stderr as log lines: split into bounded lines, with secrets scrubbed. Used for
//! stdio MCP servers ([`crate::mcp`]) and launched plugins ([`crate::plugin_host`]).

use tokio::io::{AsyncRead, AsyncReadExt};
use tracing::Level;

use crate::config::{MIN_SCRUB_LEN, REDACTED, Secret, scrub};

/// Longest line passed on; the rest of a longer line is cut off.
pub(crate) const MAX_LINE: usize = 4096;

/// Splits a line that looks like a log message, a timestamp followed by a level
/// (`2026-10-29T16:04:17.522027Z  WARN giles_core::x: text`), into its level and the text after
/// it. Any other line is `info` and kept whole. The timestamp is dropped: the core stamps the line
/// itself.
pub(crate) fn parse_log_line(line: &str) -> (Level, &str) {
    split_log_line(line).unwrap_or((Level::INFO, line))
}

fn split_log_line(line: &str) -> Option<(Level, &str)> {
    let (timestamp, rest) = line.split_once(' ')?;
    if !is_timestamp(timestamp) {
        return None;
    }
    let rest = rest.trim_start_matches(' ');
    let (level, text) = rest.split_once(' ').unwrap_or((rest, ""));
    // Not `Level::from_str`, which also takes lower case and digits.
    let level = match level {
        "TRACE" => Level::TRACE,
        "DEBUG" => Level::DEBUG,
        "INFO" => Level::INFO,
        "WARN" => Level::WARN,
        "ERROR" => Level::ERROR,
        _ => return None,
    };
    Some((level, text))
}

/// An RFC 3339 timestamp, like `2026-10-29T16:04:17.522027Z`, which is what `tracing-subscriber`
/// writes.
fn is_timestamp(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 19
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[10] == b'T'
        && bytes
            .iter()
            .all(|b| b.is_ascii_digit() || b"-:.TZ+".contains(b))
}

/// Logs `$line`, a stderr line from [`forward`], at the level it carries (see [`parse_log_line`])
/// with target `$target`. Tracing needs the level and the target to be constants, so the level
/// picks one of five calls.
macro_rules! log_line {
    ($target:expr, $line:expr) => {{
        let (level, text) = $crate::stderr_lines::parse_log_line($line);
        match level {
            tracing::Level::TRACE => tracing::trace!(target: $target, "{text}"),
            tracing::Level::DEBUG => tracing::debug!(target: $target, "{text}"),
            tracing::Level::INFO => tracing::info!(target: $target, "{text}"),
            tracing::Level::WARN => tracing::warn!(target: $target, "{text}"),
            _ => tracing::error!(target: $target, "{text}"),
        }
    }};
}
pub(crate) use log_line;

/// Reads `stderr` to its end and hands each line, with `secrets` scrubbed, to `emit`. A read error
/// ends it like end-of-file (logged at debug level).
pub(crate) async fn forward(
    mut stderr: impl AsyncRead + Unpin,
    secrets: &[&Secret],
    mut emit: impl FnMut(String),
) {
    let mut lines = LineSplitter::default();
    let mut buffer = [0; 4096];
    let mut ready = Vec::new();
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => lines.push(&buffer[..n], &mut ready),
            Err(error) => {
                tracing::debug!(%error, "reading the child's stderr failed");
                break;
            }
        }
        for line in ready.drain(..) {
            emit(scrub_line(line, secrets));
        }
    }
    if let Some(line) = lines.finish() {
        emit(scrub_line(line, secrets));
    }
}

/// A line as passed on: `secrets` replaced by `<redacted>`, and a cut line marked with `…`. A cut
/// can split a secret, so a cut line that ends in the start of a secret (at least
/// [`MIN_SCRUB_LEN`] bytes of it, like `scrub`) has that end redacted too.
fn scrub_line(line: Line, secrets: &[&Secret]) -> String {
    let mut text = scrub(&line.text, secrets);
    if line.truncated {
        let partial = secrets
            .iter()
            .map(|secret| secret.expose())
            .filter(|value| value.len() >= MIN_SCRUB_LEN)
            .flat_map(|value| {
                (MIN_SCRUB_LEN..value.len())
                    .filter(|&end| value.is_char_boundary(end))
                    .map(move |end| &value[..end])
            })
            .filter(|prefix| text.ends_with(prefix))
            .map(str::len)
            .max();
        if let Some(len) = partial {
            text.truncate(text.len() - len);
            text.push_str(REDACTED);
        }
        text.push('…');
    }
    text
}

/// One line, before scrubbing.
#[derive(Debug, PartialEq, Eq)]
struct Line {
    text: String,
    /// The line was longer than [`MAX_LINE`] and was cut.
    truncated: bool,
}

/// Splits a byte stream into lines: `\n`-terminated, a trailing `\r` dropped, invalid UTF-8
/// replaced, and each line cut to [`MAX_LINE`] bytes so a child can't make the core buffer without
/// bound.
#[derive(Default)]
struct LineSplitter {
    line: Vec<u8>,
    /// The current line was cut; skip the rest of it.
    truncated: bool,
}

impl LineSplitter {
    fn push(&mut self, mut bytes: &[u8], out: &mut Vec<Line>) {
        while !bytes.is_empty() {
            let (chunk, rest, complete) = match bytes.iter().position(|&b| b == b'\n') {
                Some(end) => (&bytes[..end], &bytes[end + 1..], true),
                None => (bytes, &[][..], false),
            };
            bytes = rest;
            if !self.truncated {
                let room = MAX_LINE - self.line.len();
                if chunk.len() > room {
                    self.line.extend_from_slice(&chunk[..room]);
                    self.truncated = true;
                } else {
                    self.line.extend_from_slice(chunk);
                }
            }
            if complete {
                out.push(self.take());
            }
        }
    }

    /// The last line, if the stream ended without a newline.
    fn finish(mut self) -> Option<Line> {
        (!self.line.is_empty() || self.truncated).then(|| self.take())
    }

    fn take(&mut self) -> Line {
        if self.line.last() == Some(&b'\r') {
            self.line.pop();
        }
        let line = Line {
            text: String::from_utf8_lossy(&self.line).into_owned(),
            truncated: self.truncated,
        };
        self.line.clear();
        self.truncated = false;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> (Vec<String>, Option<String>) {
        let mut splitter = LineSplitter::default();
        let mut lines = Vec::new();
        for chunk in chunks {
            splitter.push(chunk, &mut lines);
        }
        let shown = |line| scrub_line(line, &[]);
        (
            lines.into_iter().map(shown).collect(),
            splitter.finish().map(shown),
        )
    }

    #[test]
    fn splits_lines_across_chunks() {
        let (lines, rest) = split(&[b"one\ntw", b"o\r\n\nthr", b"ee"]);
        assert_eq!(lines, ["one", "two", ""]);
        assert_eq!(rest.as_deref(), Some("three"));
    }

    #[test]
    fn nothing_pending_after_a_final_newline() {
        assert_eq!(split(&[b"a\n"]), (vec!["a".to_owned()], None));
    }

    #[test]
    fn long_lines_are_cut_and_the_rest_skipped() {
        let long = vec![b'x'; MAX_LINE + 10];
        let (lines, rest) = split(&[&long, b"yyy\nnext\n"]);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], format!("{}…", "x".repeat(MAX_LINE)));
        assert_eq!(lines[1], "next");
        assert_eq!(rest, None);
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let (lines, _) = split(&[b"caf\xc3\n\xff\n"]);
        assert_eq!(lines, ["caf\u{fffd}", "\u{fffd}"]);
    }

    #[test]
    fn secrets_are_scrubbed_including_one_split_by_the_cut() {
        let token = Secret::new("tok-0123456789");
        let secrets = [&token];
        let line = |text: &str, truncated| Line {
            text: text.to_owned(),
            truncated,
        };
        assert_eq!(
            scrub_line(line("auth tok-0123456789 ok", false), &secrets),
            "auth <redacted> ok"
        );
        // The cut left the first 8 bytes of the token at the end of the line.
        assert_eq!(
            scrub_line(line("auth tok-0123", true), &secrets),
            "auth <redacted>…"
        );
        // Fewer than MIN_SCRUB_LEN bytes are too little to redact.
        assert_eq!(scrub_line(line("auth to", true), &secrets), "auth to…");
        // An uncut line that merely ends like the token is left alone.
        assert_eq!(
            scrub_line(line("auth tok-0", false), &secrets),
            "auth tok-0"
        );
    }

    #[tokio::test]
    async fn forward_emits_every_line_scrubbed() {
        let secret = Secret::new("hunter22");
        let mut lines = Vec::new();
        forward(&b"one hunter22\ntwo"[..], &[&secret], |line| {
            lines.push(line)
        })
        .await;
        assert_eq!(lines, ["one <redacted>", "two"]);
    }

    #[test]
    fn a_log_line_gives_its_level_and_text_without_the_timestamp() {
        let line = "2026-10-29T16:04:17.522027Z  WARN giles_core::sandbox::linux: lacking=signal";
        assert_eq!(
            parse_log_line(line),
            (Level::WARN, "giles_core::sandbox::linux: lacking=signal")
        );
        for (word, level) in [
            ("TRACE", Level::TRACE),
            ("DEBUG", Level::DEBUG),
            ("INFO", Level::INFO),
            ("ERROR", Level::ERROR),
        ] {
            let line = format!("2026-10-29T16:04:17Z {word} x: y");
            assert_eq!(parse_log_line(&line), (level, "x: y"), "{word}");
        }
        assert_eq!(
            parse_log_line("2026-10-29T16:04:17.5Z  INFO"),
            (Level::INFO, "")
        );
    }

    #[test]
    fn any_other_line_is_info_and_kept_whole() {
        for line in [
            "connected",
            "",
            "WARN something",
            "2026-10-29 WARN not a timestamp",
            "2026-10-29T16:04:17Z NOTICE unknown level",
            "2026-10-29T16:04:17Z warn lower case",
            "2026-10-29T16:04:17Z",
            "Error: cannot start: 2026-10-29T16:04:17Z WARN later",
        ] {
            assert_eq!(parse_log_line(line), (Level::INFO, line), "{line:?}");
        }
    }
}
