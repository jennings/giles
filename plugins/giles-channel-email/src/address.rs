//! Email addresses and message ids, checked before they're used as identities or written into
//! headers and SMTP commands.

/// `address` as an identity: trimmed, with its domain lowercased (domains are case-insensitive;
/// local parts in principle aren't, so they're kept). `None` unless it's `local@domain` with no
/// whitespace, control characters, or angle brackets, which could break out of a header or an
/// SMTP command.
pub(crate) fn normalize(address: &str) -> Option<String> {
    let address = address.trim();
    let (local, domain) = address.rsplit_once('@')?;
    let bad = |c: char| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '@');
    if local.is_empty()
        || domain.is_empty()
        || local
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>'))
        || domain.chars().any(bad)
    {
        return None;
    }
    Some(format!("{local}@{}", domain.to_ascii_lowercase()))
}

/// The domain of a normalized address.
pub(crate) fn domain(address: &str) -> &str {
    address.rsplit_once('@').map_or("", |(_, domain)| domain)
}

/// A message id (without its angle brackets) that is safe to write into `In-Reply-To` and
/// `References`: 1 to 250 printable ASCII characters, no spaces or angle brackets.
pub(crate) fn clean_id(id: &str) -> Option<&str> {
    let id = id.trim().trim_start_matches('<').trim_end_matches('>');
    (!id.is_empty()
        && id.len() <= 250
        && id
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'<' | b'>')))
    .then_some(id)
}

/// Header text (a subject, a display name) on one line: control characters, line breaks
/// included, become spaces, and runs of whitespace one space.
pub(crate) fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `host` is this machine: `localhost`, `127.0.0.0/8`, or `::1`.
pub(crate) fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_keep_the_local_part_and_lowercase_the_domain() {
        assert_eq!(
            normalize(" Me.Name@Example.COM ").as_deref(),
            Some("Me.Name@example.com")
        );
        assert_eq!(
            normalize("\"a@b\"@example.com").as_deref(),
            Some("\"a@b\"@example.com")
        );
        for bad in [
            "",
            "me",
            "@example.com",
            "me@",
            "me@ex ample.com",
            "me@a>b",
            "m\re@x.com",
        ] {
            assert_eq!(normalize(bad), None, "{bad:?}");
        }
        assert_eq!(domain("me@example.com"), "example.com");
    }

    #[test]
    fn ids_that_could_break_a_header_are_refused() {
        assert_eq!(clean_id("<abc@example.com>"), Some("abc@example.com"));
        assert_eq!(clean_id("abc@example.com"), Some("abc@example.com"));
        for bad in [
            "",
            "<>",
            "a b@x",
            "a\r\nX-Evil: 1",
            "a>b@x",
            &"x".repeat(251),
        ] {
            assert_eq!(clean_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn loopback_hosts() {
        for host in ["localhost", "127.0.0.1", "127.8.9.10", "::1", "[::1]"] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in ["mail.example.com", "10.0.0.1", "localhost.example.com"] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn header_text_stays_on_one_line() {
        assert_eq!(one_line(" Re:\r\nBcc: x@y\t\u{0}z  "), "Re: Bcc: x@y z");
        assert_eq!(one_line("\r\n"), "");
    }
}
