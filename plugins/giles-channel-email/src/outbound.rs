//! Building outgoing mail: `multipart/alternative` (plain text and HTML) with threading headers,
//! `Auto-Submitted`, and attachments.

use giles_plugin_sdk::render;
use giles_plugin_sdk::tls::rustls::crypto::aws_lc_rs;
use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::message_id::MessageId;
use mail_builder::headers::raw::Raw;

use crate::address::one_line;
use crate::inbound::OWN_ID_PREFIX;

/// The longest subject made up from a proactive message's first line, in characters.
const MAX_NEW_SUBJECT_CHARS: usize = 60;

/// RFC 3834 `Auto-Submitted` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AutoSubmitted {
    /// A reply to a message (`auto-replied`).
    Replied,
    /// A message nobody wrote to us first: jobs, reminders (`auto-generated`).
    Generated,
}

impl AutoSubmitted {
    fn as_str(self) -> &'static str {
        match self {
            Self::Replied => "auto-replied",
            Self::Generated => "auto-generated",
        }
    }
}

/// A file to attach.
#[derive(Debug, Clone)]
pub(crate) struct OutAttachment {
    pub media_type: String,
    pub name: Option<String>,
    pub bytes: Vec<u8>,
}

/// Everything in an outgoing message.
#[derive(Debug, Clone)]
pub(crate) struct Outgoing<'a> {
    pub from: &'a str,
    pub from_name: Option<&'a str>,
    pub to: &'a str,
    pub to_name: Option<&'a str>,
    pub subject: &'a str,
    pub message_id: &'a str,
    pub in_reply_to: Option<&'a str>,
    pub references: &'a [String],
    pub markdown: &'a str,
    pub attachments: &'a [OutAttachment],
    pub auto_submitted: AutoSubmitted,
    /// Unix seconds.
    pub date: i64,
}

/// A new message id: `giles.<128 random bits>@<domain>`.
pub(crate) fn new_message_id(domain: &str) -> String {
    let mut random = [0u8; 16];
    aws_lc_rs::default_provider()
        .secure_random
        .fill(&mut random)
        .expect("the system random number generator works");
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    format!("{OWN_ID_PREFIX}{hex}@{domain}")
}

/// `Re: <subject>`, unless it already starts with `Re:`.
pub(crate) fn reply_subject(subject: &str) -> String {
    let subject = subject.trim();
    let has_re = subject
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("re:"));
    if has_re {
        subject.to_owned()
    } else if subject.is_empty() {
        "Re: (no subject)".into()
    } else {
        format!("Re: {subject}")
    }
}

/// A subject for a message that starts a thread: its first line of text, shortened.
pub(crate) fn new_subject(markdown: &str) -> String {
    let text = render::plain_text(markdown);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut subject: String = line.chars().take(MAX_NEW_SUBJECT_CHARS).collect();
    if line.chars().count() > MAX_NEW_SUBJECT_CHARS {
        subject.push('\u{2026}');
    }
    if subject.is_empty() {
        "(no subject)".into()
    } else {
        subject
    }
}

/// A media type safe to write into `Content-Type`; anything else is sent as
/// `application/octet-stream`.
fn safe_media_type(media_type: &str) -> &str {
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
    };
    match media_type.split_once('/') {
        Some((kind, sub)) if token(kind) && token(sub) => media_type,
        _ => "application/octet-stream",
    }
}

/// The message as RFC 5322 bytes.
pub(crate) fn build(out: &Outgoing<'_>) -> Vec<u8> {
    // Header text from mail or the model stays on one line, so it can't add headers.
    let address = |name: Option<&str>, addr: &str| match name.map(one_line) {
        Some(name) => Address::new_address(Some(name), addr.to_owned()),
        None => Address::new_address(None::<String>, addr.to_owned()),
    };
    let html = format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"></head><body>\n{}</body></html>\n",
        // Email never shows images: remote ones would leak data, and there are no local ones.
        render::html(out.markdown, |_| false)
    );
    let mut builder = MessageBuilder::new()
        .from(address(out.from_name, out.from))
        .to(address(out.to_name, out.to))
        .subject(one_line(out.subject))
        .message_id(out.message_id.to_owned())
        .date(out.date)
        .header("Auto-Submitted", Raw::new(out.auto_submitted.as_str()))
        .text_body(render::plain_text(out.markdown))
        .html_body(html);
    if let Some(parent) = out.in_reply_to {
        builder = builder.in_reply_to(parent.to_owned());
    }
    if !out.references.is_empty() {
        builder = builder.references(MessageId::new_list(out.references.iter().cloned()));
    }
    for (i, attachment) in out.attachments.iter().enumerate() {
        let name = attachment
            .name
            .as_deref()
            .map(|n| n.chars().filter(|c| !c.is_control()).collect::<String>())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("attachment-{}", i + 1));
        builder = builder.attachment(
            safe_media_type(&attachment.media_type).to_owned(),
            name,
            attachment.bytes.clone(),
        );
    }
    builder
        .write_to_vec()
        .expect("writing to a Vec never fails")
}

#[cfg(test)]
mod tests {
    use mail_parser::{HeaderValue, MessageParser, MimeHeaders as _};

    use super::*;

    #[test]
    fn subjects() {
        assert_eq!(reply_subject("Groceries"), "Re: Groceries");
        assert_eq!(reply_subject("RE: Groceries"), "RE: Groceries");
        assert_eq!(reply_subject(""), "Re: (no subject)");
        assert_eq!(
            new_subject("\n# Daily *summary*\n\nMore."),
            "Daily _summary_"
        );
        let long = "word ".repeat(30);
        let subject = new_subject(&long);
        assert_eq!(subject.chars().count(), MAX_NEW_SUBJECT_CHARS + 1);
        assert!(subject.ends_with('\u{2026}'));
        assert_eq!(new_subject(""), "(no subject)");
    }

    #[test]
    fn message_ids_are_ours_and_unique() {
        let a = new_message_id("example.net");
        let b = new_message_id("example.net");
        assert_ne!(a, b);
        assert!(
            a.starts_with("giles.") && a.ends_with("@example.net"),
            "{a}"
        );
        assert_eq!(a.len(), "giles.".len() + 32 + "@example.net".len());
    }

    #[test]
    fn media_types_are_checked() {
        assert_eq!(safe_media_type("image/png"), "image/png");
        assert_eq!(
            safe_media_type("application/vnd.ms-excel"),
            "application/vnd.ms-excel"
        );
        for bad in [
            "png",
            "image/",
            "text/html\r\nX: y",
            "a/b c",
            "image/png; charset=x",
        ] {
            assert_eq!(safe_media_type(bad), "application/octet-stream", "{bad:?}");
        }
    }

    #[test]
    fn a_reply_is_multipart_alternative_with_threading_headers() {
        let references = vec!["root@example.com".to_owned(), "b@example.com".to_owned()];
        let attachments = vec![OutAttachment {
            media_type: "text/csv".into(),
            name: Some("list\r\n.csv".into()),
            bytes: b"a,b\n1,2\n".to_vec(),
        }];
        let raw = build(&Outgoing {
            from: "giles@example.net",
            from_name: Some("Giles"),
            to: "me@example.com",
            to_name: Some("Stephen"),
            subject: "Re: Groceries",
            message_id: "giles.1@example.net",
            in_reply_to: Some("b@example.com"),
            references: &references,
            markdown: "**Done.** ![x](https://evil.example/?d=1) [link](https://example.com)",
            attachments: &attachments,
            auto_submitted: AutoSubmitted::Replied,
            date: 1_792_706_580,
        });
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("Auto-Submitted: auto-replied\r\n"), "{text}");
        let message = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(message.message_id(), Some("giles.1@example.net"));
        assert_eq!(message.subject(), Some("Re: Groceries"));
        let from = message.from().unwrap().first().unwrap();
        assert_eq!(
            (from.name(), from.address()),
            (Some("Giles"), Some("giles@example.net"))
        );
        assert_eq!(
            message.to().unwrap().first().unwrap().address(),
            Some("me@example.com")
        );
        assert_eq!(
            message.in_reply_to(),
            &HeaderValue::Text("b@example.com".into())
        );
        assert_eq!(
            message.references(),
            &HeaderValue::TextList(vec!["root@example.com".into(), "b@example.com".into()])
        );
        assert_eq!(message.date().unwrap().to_timestamp(), 1_792_706_580);

        // multipart/mixed: multipart/alternative (text/plain, text/html), then the attachment.
        let root = message.root_part();
        assert!(root.is_content_type("multipart", "mixed"));
        let alternative = message.part(root.sub_parts().unwrap()[0]).unwrap();
        assert!(alternative.is_content_type("multipart", "alternative"));
        let [plain, html] = alternative.sub_parts().unwrap() else {
            panic!("two alternatives");
        };
        let plain = message.part(*plain).unwrap();
        let html = message.part(*html).unwrap();
        assert!(plain.is_content_type("text", "plain"));
        assert!(html.is_content_type("text", "html"));
        let plain = plain.text_contents().unwrap();
        assert_eq!(
            plain.trim(),
            "*Done.* [image: x] link <https://example.com>"
        );
        let html = html.text_contents().unwrap();
        assert!(html.contains("<strong>Done.</strong>"), "{html}");
        assert!(html.contains("href=\"https://example.com\""), "{html}");
        assert!(!html.contains("evil.example"), "no remote image: {html}");
        assert!(!html.contains("<img"), "{html}");

        let attachment = message.attachment(0).unwrap();
        assert_eq!(attachment.attachment_name(), Some("list.csv"));
        assert!(attachment.is_content_type("text", "csv"));
        assert_eq!(attachment.contents(), b"a,b\n1,2\n");
    }

    #[test]
    fn a_new_message_has_no_threading_headers() {
        let raw = build(&Outgoing {
            from: "giles@example.net",
            from_name: None,
            to: "me@example.com",
            to_name: None,
            subject: "Reminder\r\nBcc: evil@evil.example",
            message_id: "giles.2@example.net",
            in_reply_to: None,
            references: &[],
            markdown: "Water the plants.",
            attachments: &[],
            auto_submitted: AutoSubmitted::Generated,
            date: 1_792_706_580,
        });
        let text = String::from_utf8_lossy(&raw);
        assert!(
            text.contains("Auto-Submitted: auto-generated\r\n"),
            "{text}"
        );
        assert!(!text.contains("In-Reply-To"), "{text}");
        assert!(!text.contains("References"), "{text}");
        assert!(!text.contains("\r\nBcc:"), "{text}");
        let message = MessageParser::default().parse(&raw).unwrap();
        assert!(
            message
                .root_part()
                .is_content_type("multipart", "alternative")
        );
    }
}
