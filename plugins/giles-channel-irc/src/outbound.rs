//! Outbound pieces: line limits, rendering, rate limiting, recipients, SASL payloads.

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use giles_plugin_sdk::render;

/// The host length assumed in the source prefix the server adds when it relays our messages: the
/// usual server `HOSTLEN`, since our visible host (often a cloak) isn't known for sure.
pub const HOST_LEN: usize = 63;
/// The IRC line limit, CRLF included.
const LINE_LIMIT: usize = 512;
/// How much of a base64 SASL payload one `AUTHENTICATE` line carries.
const SASL_CHUNK: usize = 400;

/// The bytes of text one `PRIVMSG` to `target` can carry so that the line the server relays,
/// `:<nick>!~<username>@<host> PRIVMSG <target> :<text>\r\n`, fits in 512 bytes: `512 - 2 -
/// len("PRIVMSG <target> :") - len(":<nick>!~<username>@<host> ")`, with a [`HOST_LEN`]-byte host
/// and room for the `~` servers add to an unverified user name. `cap` (the `max_line_bytes`
/// setting) lowers it further.
pub(crate) fn line_budget(target: &str, nick: &str, username: &str, cap: Option<usize>) -> usize {
    let command = "PRIVMSG ".len() + target.len() + " :".len();
    let source = ":".len() + nick.len() + "!~".len() + username.len() + "@".len() + HOST_LEN + 1;
    let budget = LINE_LIMIT
        .saturating_sub(2 + command + source)
        .min(cap.unwrap_or(usize::MAX));
    budget.max(1)
}

/// `markdown` as IRC lines of at most `budget` bytes, at most `max_lines` of them: past that, the
/// last line says how many more weren't sent.
pub(crate) fn lines(markdown: &str, budget: usize, max_lines: usize) -> Vec<String> {
    let mut lines = render::irc(markdown, budget);
    if lines.len() > max_lines {
        let kept = max_lines.saturating_sub(1);
        let cut = lines.len() - kept;
        lines.truncate(kept);
        lines.push(format!("[{cut} more lines not sent]"));
    }
    lines
}

/// Flood protection: `burst` lines at once, then one per `interval`.
#[derive(Debug)]
pub(crate) struct Throttle {
    burst: u32,
    interval: Duration,
    /// When the lines sent so far are paid off at one per `interval`.
    credit: Instant,
}

impl Throttle {
    pub(crate) fn new(burst: u32, interval: Duration, now: Instant) -> Self {
        Self {
            burst: burst.max(1),
            interval,
            credit: now,
        }
    }

    /// How long to wait before the next line may go, or `None` if it may go now.
    pub(crate) fn wait(&self, now: Instant) -> Option<Duration> {
        let allowance = now + self.interval * (self.burst - 1);
        (self.credit > allowance).then(|| self.credit - allowance)
    }

    /// Records a line sent at `now`.
    pub(crate) fn take(&mut self, now: Instant) {
        self.credit = self.credit.max(now) + self.interval;
    }
}

/// Who a [`SendTarget::Recipient`](giles_plugin_sdk::giles_types::SendTarget) names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recipient {
    /// `account:<name>`: whoever is logged in to the services account.
    Account(String),
    /// `mask:<nick>!<user>@<host>`: that nick, if its user and host still match.
    Mask {
        /// The nick.
        nick: String,
        /// The user name.
        user: String,
        /// The host.
        host: String,
    },
}

/// Parses a recipient native id (the identity without the `irc:<network>:` namespace).
pub(crate) fn recipient(id: &str) -> Option<Recipient> {
    if let Some(account) = id.strip_prefix("account:") {
        return is_target(account).then(|| Recipient::Account(account.to_owned()));
    }
    let mask = id.strip_prefix("mask:")?;
    let (nick, rest) = mask.split_once('!')?;
    let (user, host) = rest.rsplit_once('@')?;
    (crate::config::is_nick(nick) && is_target(user) && is_target(host)).then(|| Recipient::Mask {
        nick: nick.to_owned(),
        user: user.to_owned(),
        host: host.to_owned(),
    })
}

/// Whether `target` can be a message target or a `WHOIS` argument without breaking the line: no
/// spaces, commas, control characters, or a leading `:`.
pub(crate) fn is_target(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with(':')
        && !target
            .chars()
            .any(|c| c == ' ' || c == ',' || c.is_control())
}

/// The `AUTHENTICATE` arguments carrying a SASL PLAIN login: base64 of
/// `<username>\0<username>\0<password>` in chunks of 400 bytes, and `+` after a final full chunk
/// (or for an empty payload).
pub(crate) fn sasl_plain(username: &str, password: &str) -> Vec<String> {
    authenticate_chunks(&BASE64.encode(format!("{username}\0{username}\0{password}")))
}

fn authenticate_chunks(payload: &str) -> Vec<String> {
    let mut chunks: Vec<String> = payload
        .as_bytes()
        .chunks(SASL_CHUNK)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect();
    if payload.len().is_multiple_of(SASL_CHUNK) {
        chunks.push("+".to_owned());
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_leaves_room_for_the_relayed_source() {
        let budget = line_budget("stephen", "giles", "giles", None);
        let relayed = format!(
            ":giles!~giles@{} PRIVMSG stephen :{}\r\n",
            "h".repeat(HOST_LEN),
            "x".repeat(budget)
        );
        assert_eq!(relayed.len(), 512);
        assert_eq!(line_budget("stephen", "giles", "giles", Some(100)), 100);
        assert_eq!(
            line_budget("stephen", "giles", "giles", Some(10_000)),
            budget
        );
        assert_eq!(line_budget(&"#".repeat(600), "giles", "giles", None), 1);
    }

    #[test]
    fn long_messages_are_split_and_capped() {
        let words = "word ".repeat(100);
        let lines = lines(&words, 50, 100);
        assert!(lines.len() > 5);
        assert!(lines.iter().all(|line| line.len() <= 50));
        assert_eq!(lines.join(" ").split_whitespace().count(), 100);

        let many: String = (1..=10).map(|n| format!("line {n}\n\n")).collect();
        let capped = super::lines(&many, 100, 4);
        assert_eq!(
            capped,
            ["line 1", "line 2", "line 3", "[7 more lines not sent]"]
        );
        assert_eq!(super::lines(&many, 100, 10).len(), 10);
    }

    #[test]
    fn the_throttle_allows_a_burst_then_one_per_interval() {
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let mut throttle = Throttle::new(3, second, start);
        for _ in 0..3 {
            assert_eq!(throttle.wait(start), None);
            throttle.take(start);
        }
        assert_eq!(throttle.wait(start), Some(second));
        assert_eq!(throttle.wait(start + second), None);
        throttle.take(start + second);
        assert_eq!(throttle.wait(start + second), Some(second));
        // Idle time refills the burst, but no more than it.
        let later = start + second * 60;
        for _ in 0..3 {
            assert_eq!(throttle.wait(later), None);
            throttle.take(later);
        }
        assert!(throttle.wait(later).is_some());
    }

    #[test]
    fn no_interval_means_no_limit() {
        let now = Instant::now();
        let mut throttle = Throttle::new(1, Duration::ZERO, now);
        for _ in 0..100 {
            assert_eq!(throttle.wait(now), None);
            throttle.take(now);
        }
    }

    #[test]
    fn recipients() {
        assert_eq!(
            recipient("account:Stephen"),
            Some(Recipient::Account("Stephen".into()))
        );
        assert_eq!(
            recipient("mask:sj!~sj@user/sj"),
            Some(Recipient::Mask {
                nick: "sj".into(),
                user: "~sj".into(),
                host: "user/sj".into(),
            })
        );
        for bad in [
            "account:",
            "account:a b",
            "nick:sj",
            "mask:sj",
            "mask:sj!user",
            "mask:9x!u@h",
            "mask:sj!u@",
            "sj",
        ] {
            assert_eq!(recipient(bad), None, "{bad}");
        }
    }

    #[test]
    fn targets_cant_break_the_line() {
        assert!(is_target("#giles"));
        assert!(is_target("stephen"));
        for bad in ["", ":x", "a b", "a,b", "a\rb", "a\nb"] {
            assert!(!is_target(bad), "{bad:?}");
        }
    }

    #[test]
    fn sasl_plain_payloads_are_chunked() {
        assert_eq!(
            sasl_plain("giles", "pw"),
            [BASE64.encode("giles\0giles\0pw")]
        );
        // 300 raw bytes are exactly 400 base64 bytes: a full chunk, then `+`.
        let user = "u".repeat(148);
        let chunks = sasl_plain(&user, "pw");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 400);
        assert_eq!(chunks[1], "+");
        let chunks = sasl_plain(&"u".repeat(200), "pw");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 400);
        assert_eq!(
            BASE64.decode(chunks.concat()).unwrap(),
            format!("{0}\0{0}\0pw", "u".repeat(200)).into_bytes()
        );
        assert_eq!(authenticate_chunks(""), ["+"]);
    }
}
