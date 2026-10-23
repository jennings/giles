//! A child process's stderr as log lines: split into bounded lines, with secrets scrubbed. Used for
//! stdio MCP servers ([`crate::mcp`]) and launched plugins ([`crate::plugin_host`]).

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::config::{MIN_SCRUB_LEN, REDACTED, Secret, scrub};

/// Longest line passed on; the rest of a longer line is cut off.
pub(crate) const MAX_LINE: usize = 4096;

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
}
