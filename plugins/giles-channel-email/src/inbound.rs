//! Turning a fetched message into what the channel reports, or a reason to drop it.

use giles_plugin_sdk::giles_types::Timestamp;
use mail_parser::{HeaderValue, Message, MessageParser, MimeHeaders as _};

use crate::address;
use crate::auth_results::{self, Verdict};

/// The local part prefix of the message ids this plugin generates, so its own sent copies can be
/// recognized when they land in the watched mailbox.
pub(crate) const OWN_ID_PREFIX: &str = "giles.";

/// What the channel needs to judge a message.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Context<'a> {
    /// The trusted `authserv-id`.
    pub authserv_id: &'a str,
    /// Our own address (normalized).
    pub own_address: &'a str,
}

/// Why a message never reaches the core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// Not a message (`mail-parser` found no headers).
    Unparseable,
    /// Sent by a machine (RFC 3834): the header that says so.
    Automated(String),
    /// No single, valid `From:` address.
    NoSender,
    /// A copy of a message this plugin sent.
    OwnCopy,
}

impl std::fmt::Display for Dropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparseable => f.write_str("not a parseable message"),
            Self::Automated(header) => write!(f, "automated mail ({header})"),
            Self::NoSender => f.write_str("no single From: address"),
            Self::OwnCopy => f.write_str("a copy of a message this channel sent"),
        }
    }
}

/// An attachment of an inbound message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attachment {
    pub bytes: Vec<u8>,
    pub media_type: String,
    pub name: Option<String>,
}

/// An inbound message, ready to be reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mail {
    /// The `From:` address, normalized.
    pub sender: String,
    pub display_name: Option<String>,
    pub verdict: Verdict,
    /// The message's own id (without angle brackets); synthesized if it has none.
    pub message_id: String,
    /// The first id of `References`, else of `In-Reply-To`, else the message's own id.
    pub root: String,
    /// The first `In-Reply-To` id.
    pub in_reply_to: Option<String>,
    /// The thread so far, ending with this message: its `References` (or `In-Reply-To`) and its
    /// own id.
    pub references: Vec<String>,
    /// The subject, trimmed (may be empty).
    pub subject: String,
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub sent_at: Option<Timestamp>,
}

impl Mail {
    /// Whether this message starts its thread.
    pub(crate) fn is_root(&self) -> bool {
        self.root == self.message_id
    }
}

/// The conversation id of a thread with one sender: the sender (lowercased) and the thread root.
/// Each sender in a thread gets their own direct conversation, so replies only ever go to the
/// person who wrote, and nobody can steer replies meant for another sender by writing into the
/// same thread.
pub(crate) fn conversation_id(sender: &str, root: &str) -> String {
    format!("{}/{root}", sender.to_ascii_lowercase())
}

/// Parses a raw message (RFC 5322 bytes, or only its header section). `synthetic_id` is used as
/// the message id when the message has no usable `Message-ID`.
pub(crate) fn parse(raw: &[u8], synthetic_id: &str, context: Context<'_>) -> Result<Mail, Dropped> {
    let message = MessageParser::default()
        .parse(raw)
        .filter(|m| !m.headers().is_empty())
        .ok_or(Dropped::Unparseable)?;
    if let Some(header) = automated(&message) {
        return Err(Dropped::Automated(header));
    }
    let (sender, display_name) = sender(&message).ok_or(Dropped::NoSender)?;
    let message_id = message
        .message_id()
        .and_then(address::clean_id)
        .unwrap_or(synthetic_id)
        .to_owned();
    if sender.eq_ignore_ascii_case(context.own_address) && message_id.starts_with(OWN_ID_PREFIX) {
        return Err(Dropped::OwnCopy);
    }
    let topmost = raw_values(&message, "Authentication-Results").next();
    let verdict = auth_results::verify(
        topmost.as_deref(),
        context.authserv_id,
        address::domain(&sender),
    );
    let references = ids(message.references());
    let in_reply_to = ids(message.in_reply_to()).into_iter().next();
    let root = references
        .first()
        .or(in_reply_to.as_ref())
        .unwrap_or(&message_id)
        .clone();
    let mut thread = if references.is_empty() {
        in_reply_to.iter().cloned().collect()
    } else {
        references
    };
    thread.push(message_id.clone());
    let subject = address::one_line(message.subject().unwrap_or_default());
    let sent_at = message
        .date()
        .filter(|date| date.is_valid())
        .and_then(|date| Timestamp::from_second(date.to_timestamp()).ok());
    let mut mail = Mail {
        sender,
        display_name,
        verdict,
        root,
        in_reply_to,
        references: thread,
        subject,
        text: String::new(),
        attachments: attachments(&message),
        sent_at,
        message_id,
    };
    mail.text = text(&message, &mail);
    Ok(mail)
}

/// The header that marks a message as automated, if any: `Auto-Submitted` other than `no`,
/// `Precedence: bulk`, `list`, or `junk`, or any `List-Id`.
fn automated(message: &Message<'_>) -> Option<String> {
    for header in message.headers() {
        let name = header.name.as_str();
        let value = || raw_value(message, header).trim().to_ascii_lowercase();
        if name.eq_ignore_ascii_case("Auto-Submitted") {
            let value = value();
            let keyword = value.split([';', '(']).next().unwrap_or_default().trim();
            if keyword != "no" {
                return Some(format!("Auto-Submitted: {keyword}"));
            }
        } else if name.eq_ignore_ascii_case("Precedence") {
            let value = value();
            if matches!(value.as_str(), "bulk" | "list" | "junk") {
                return Some(format!("Precedence: {value}"));
            }
        } else if name.eq_ignore_ascii_case("List-Id") {
            return Some("List-Id".into());
        }
    }
    None
}

/// The sender: exactly one `From:` header with exactly one valid address.
fn sender(message: &Message<'_>) -> Option<(String, Option<String>)> {
    let from_headers = message
        .headers()
        .iter()
        .filter(|h| h.name.as_str().eq_ignore_ascii_case("From"))
        .count();
    if from_headers != 1 {
        return None;
    }
    let from = message.from()?;
    let mut addresses = from.iter();
    let first = addresses.next()?;
    if addresses.next().is_some() {
        return None;
    }
    let address = address::normalize(first.address()?)?;
    let name = first
        .name()
        .map(address::one_line)
        .filter(|n| !n.is_empty());
    Some((address, name))
}

/// Every value of the named top-level header, raw and unfolded, in order.
fn raw_values<'m>(message: &'m Message<'_>, name: &'m str) -> impl Iterator<Item = String> + 'm {
    message
        .headers()
        .iter()
        .filter(move |h| h.name.as_str().eq_ignore_ascii_case(name))
        .map(|h| raw_value(message, h))
}

/// A header's raw value, unfolded (line breaks removed; the whitespace after them kept).
fn raw_value(message: &Message<'_>, header: &mail_parser::Header<'_>) -> String {
    let bytes = message
        .raw_message
        .get(header.offset_start as usize..header.offset_end as usize)
        .unwrap_or_default();
    String::from_utf8_lossy(bytes).replace(['\r', '\n'], "")
}

/// The usable ids in a `References` or `In-Reply-To` value.
fn ids(value: &HeaderValue<'_>) -> Vec<String> {
    let list: Vec<&str> = match value {
        HeaderValue::Text(id) => vec![id.as_ref()],
        HeaderValue::TextList(ids) => ids.iter().map(AsRef::as_ref).collect(),
        _ => Vec::new(),
    };
    list.into_iter()
        .filter_map(address::clean_id)
        .map(str::to_owned)
        .collect()
}

/// The text the agent sees: the text bodies (HTML converted to text when there's no plain text),
/// with the subject first when the message starts a thread.
fn text(message: &Message<'_>, mail: &Mail) -> String {
    let bodies: Vec<String> = (0..message.text_body_count())
        .filter_map(|i| message.body_text(i))
        .map(|body| body.trim_end().to_owned())
        .filter(|body| !body.is_empty())
        .collect();
    let body = bodies.join("\n\n");
    if mail.is_root() && !mail.subject.is_empty() {
        if body.is_empty() {
            format!("Subject: {}", mail.subject)
        } else {
            format!("Subject: {}\n\n{body}", mail.subject)
        }
    } else {
        body
    }
}

fn attachments(message: &Message<'_>) -> Vec<Attachment> {
    message
        .attachments()
        .filter(|part| !part.is_multipart())
        .map(|part| {
            let media_type = part
                .content_type()
                .map(|ct| match ct.subtype() {
                    Some(sub) => format!("{}/{sub}", ct.ctype()),
                    None => ct.ctype().to_owned(),
                })
                .filter(|t| t.contains('/'))
                .unwrap_or_else(|| {
                    if part.is_message() {
                        "message/rfc822".into()
                    } else {
                        "application/octet-stream".into()
                    }
                })
                .to_ascii_lowercase();
            Attachment {
                bytes: part.contents().to_vec(),
                media_type,
                name: part.attachment_name().map(str::to_owned),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTEXT: Context<'static> = Context {
        authserv_id: "mx.example.net",
        own_address: "giles@example.net",
    };

    fn build(headers: &str, body: &str) -> Vec<u8> {
        format!("{headers}\r\n\r\n{body}")
            .replace("\r\n", "\n")
            .replace('\n', "\r\n")
            .into_bytes()
    }

    fn parse_ok(raw: &[u8]) -> Mail {
        parse(raw, "1.2@giles.invalid", CONTEXT).unwrap()
    }

    const OWNER_HEADERS: &str = "Authentication-Results: mx.example.net; dmarc=pass header.from=example.com\n\
         From: Stephen <me@Example.com>\n\
         To: giles@example.net\n\
         Subject: Groceries\n\
         Date: Thu, 22 Oct 2026 14:03:00 -0700\n\
         Message-ID: <root@example.com>";

    #[test]
    fn a_new_thread_from_an_authenticated_sender() {
        let mail = parse_ok(&build(OWNER_HEADERS, "Buy milk.\n"));
        assert_eq!(mail.sender, "me@example.com");
        assert_eq!(mail.display_name.as_deref(), Some("Stephen"));
        assert_eq!(mail.verdict, Verdict::Dmarc);
        assert_eq!(mail.message_id, "root@example.com");
        assert_eq!(mail.root, "root@example.com");
        assert!(mail.is_root());
        assert_eq!(mail.references, vec!["root@example.com"]);
        assert_eq!(mail.text, "Subject: Groceries\n\nBuy milk.");
        assert_eq!(
            mail.sent_at,
            Some("2026-10-22T21:03:00Z".parse::<Timestamp>().unwrap())
        );
    }

    #[test]
    fn replies_join_the_thread_root() {
        let reply = |id: &str, extra: &str| {
            format!(
                "{}\n{extra}",
                OWNER_HEADERS.replace("<root@example.com>", &format!("<{id}>"))
            )
        };
        let headers = reply(
            "c@example.com",
            "In-Reply-To: <b@example.net>\n\
             References: <root@example.com>\n <a@example.net> <b@example.net>",
        );
        let mail = parse_ok(&build(&headers, "And eggs."));
        assert_eq!(mail.message_id, "c@example.com");
        assert_eq!(mail.root, "root@example.com");
        assert!(!mail.is_root());
        assert_eq!(mail.in_reply_to.as_deref(), Some("b@example.net"));
        assert_eq!(
            mail.references,
            [
                "root@example.com",
                "a@example.net",
                "b@example.net",
                "c@example.com"
            ]
        );
        assert_eq!(mail.text, "And eggs.", "no subject line in replies");

        // In-Reply-To alone.
        let headers = reply("d@example.com", "In-Reply-To: <b@example.net>");
        let only_parent = parse_ok(&build(&headers, "x"));
        assert_eq!(only_parent.root, "b@example.net");
        assert_eq!(only_parent.references, ["b@example.net", "d@example.com"]);
    }

    #[test]
    fn conversations_are_per_sender_and_thread() {
        assert_eq!(
            conversation_id("Me@example.com", "root@example.com"),
            "me@example.com/root@example.com"
        );
    }

    #[test]
    fn a_missing_message_id_is_synthesized() {
        let headers = OWNER_HEADERS.replace("Message-ID: <root@example.com>", "X-Other: 1");
        let mail = parse_ok(&build(&headers, "hi"));
        assert_eq!(mail.message_id, "1.2@giles.invalid");
        assert_eq!(mail.root, "1.2@giles.invalid");
    }

    #[test]
    fn only_the_topmost_authentication_results_counts() {
        // A forged pass below the receiving server's header.
        let headers = format!(
            "Authentication-Results: mx.example.net; dmarc=fail header.from=example.com\n\
             Authentication-Results: mx.example.net; dmarc=pass header.from=example.com\n{}",
            OWNER_HEADERS.lines().skip(1).collect::<Vec<_>>().join("\n")
        );
        assert!(!parse_ok(&build(&headers, "x")).verdict.is_authenticated());

        // Another server's header on top: not trusted, even with ours below.
        let headers = format!("Authentication-Results: mx.evil.com; dmarc=pass\n{OWNER_HEADERS}");
        assert!(!parse_ok(&build(&headers, "x")).verdict.is_authenticated());
    }

    #[test]
    fn senders_need_exactly_one_from_address() {
        let two = OWNER_HEADERS.replace("<me@Example.com>", "<me@example.com>, <you@example.com>");
        assert_eq!(
            parse(&build(&two, "x"), "s", CONTEXT),
            Err(Dropped::NoSender)
        );
        let twice = format!("{OWNER_HEADERS}\nFrom: other@example.com");
        assert_eq!(
            parse(&build(&twice, "x"), "s", CONTEXT),
            Err(Dropped::NoSender)
        );
        let none = OWNER_HEADERS.replace("From: Stephen <me@Example.com>\n", "");
        assert_eq!(
            parse(&build(&none, "x"), "s", CONTEXT),
            Err(Dropped::NoSender)
        );
    }

    #[test]
    fn automated_mail_is_dropped() {
        let cases = [
            (
                "Auto-Submitted: auto-replied",
                Some("Auto-Submitted: auto-replied"),
            ),
            (
                "Auto-Submitted: Auto-Generated (vacation)",
                Some("Auto-Submitted: auto-generated"),
            ),
            ("Auto-Submitted: no", None),
            ("Auto-Submitted: No ; comment", None),
            ("Precedence: bulk", Some("Precedence: bulk")),
            ("Precedence: List", Some("Precedence: list")),
            ("Precedence: junk", Some("Precedence: junk")),
            ("Precedence: first-class", None),
            (
                "List-Id: Friends <friends.lists.example.com>",
                Some("List-Id"),
            ),
            ("List-ID: <x>", Some("List-Id")),
        ];
        for (header, expected) in cases {
            let raw = build(&format!("{OWNER_HEADERS}\n{header}"), "x");
            match (parse(&raw, "s", CONTEXT), expected) {
                (Err(Dropped::Automated(why)), Some(expected)) => assert_eq!(why, expected),
                (Ok(_), None) => {}
                (other, _) => panic!("{header}: {other:?}"),
            }
        }
    }

    #[test]
    fn our_own_copies_are_dropped() {
        let ours = "From: Giles <giles@example.net>\nMessage-ID: <giles.abc@example.net>";
        assert_eq!(
            parse(&build(ours, "x"), "s", CONTEXT),
            Err(Dropped::OwnCopy)
        );
        // Our address with a foreign id (the owner writing from the same mailbox) is kept.
        let theirs = "From: giles@example.net\nMessage-ID: <other@example.net>";
        assert!(parse(&build(theirs, "x"), "s", CONTEXT).is_ok());
    }

    #[test]
    fn mime_bodies_and_attachments() {
        let raw = build(
            &format!(
                "{OWNER_HEADERS}\nMIME-Version: 1.0\n\
             Content-Type: multipart/mixed; boundary=\"outer\""
            ),
            "--outer\n\
         Content-Type: multipart/alternative; boundary=\"inner\"\n\
         \n\
         --inner\n\
         Content-Type: text/plain; charset=utf-8\n\
         \n\
         Plain body \u{e9}.\n\
         --inner\n\
         Content-Type: text/html; charset=utf-8\n\
         \n\
         <p>HTML body</p>\n\
         --inner--\n\
         --outer\n\
         Content-Type: image/PNG\n\
         Content-Disposition: attachment; filename=\"dot.png\"\n\
         Content-Transfer-Encoding: base64\n\
         \n\
         iVBORw0KGgo=\n\
         --outer\n\
         Content-Type: application/pdf; name=\"r.pdf\"\n\
         Content-Transfer-Encoding: base64\n\
         \n\
         JVBERi0=\n\
         --outer--\n",
        );
        let mail = parse_ok(&raw);
        assert_eq!(mail.text, "Subject: Groceries\n\nPlain body \u{e9}.");
        assert_eq!(
            mail.attachments,
            vec![
                Attachment {
                    bytes: b"\x89PNG\r\n\x1a\n".to_vec(),
                    media_type: "image/png".into(),
                    name: Some("dot.png".into()),
                },
                Attachment {
                    bytes: b"%PDF-".to_vec(),
                    media_type: "application/pdf".into(),
                    name: Some("r.pdf".into()),
                },
            ]
        );
    }

    #[test]
    fn html_only_mail_becomes_text() {
        let raw = build(
            &format!("{OWNER_HEADERS}\nContent-Type: text/html; charset=utf-8"),
            "<html><body><p>Hello <b>there</b></p><script>x()</script></body></html>",
        );
        let text = parse_ok(&raw).text;
        assert!(
            text.starts_with("Subject: Groceries\n\nHello there"),
            "{text:?}"
        );
        assert!(!text.contains('<'), "{text:?}");
    }

    #[test]
    fn header_only_messages_parse() {
        let raw = build(OWNER_HEADERS, "");
        let mail = parse_ok(&raw[..raw.len() - 2]);
        assert_eq!(mail.text, "Subject: Groceries");
        assert!(parse(b"", "s", CONTEXT).is_err());
    }
}
