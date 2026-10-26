//! CommonMark to Discord messages: Discord's flavor of Markdown, split at its length limit.

pub(super) mod escape;
mod split;

use super::text::{self, Flavor};

/// The most characters Discord accepts in the content of a message.
///
/// The API counts Unicode scalar values (what `str::chars` yields), not UTF-16 units or bytes, so
/// an emoji outside the Basic Multilingual Plane counts once. The documentation says only "2000
/// characters".
pub const DISCORD_MAX_CHARS: usize = 2000;

/// Renders CommonMark as Discord messages of at most `max_chars` characters each; pass
/// [`DISCORD_MAX_CHARS`] for Discord's limit.
///
/// - Strong text is `**bold**`, emphasis `*italic*`, strikethrough `~~struck~~`, inline code
///   `` `code` ``, and code blocks keep their language tag. Headings use `#`, `##`, and `###`
///   (Discord has no deeper levels, so levels 4 to 6 are `###`); lists use `-` and `1.`, quotes
///   `> `.
/// - Discord has no tables: a table is a code block of aligned columns.
/// - Links are masked links, `[text](<url>)`; the angle brackets keep Discord from adding an
///   embed. A bare URL is wrapped in `<>` too. Links to anything but `http` and `https` show
///   their target as code after the text, since Discord doesn't make them links.
/// - Images are links too (`[alt](<url>)`), never embeds; an image with a target other than
///   `http` or `https` shows only `[image: alt]`.
/// - Text that Discord would otherwise format (`*`, `_`, `~`, `` ` ``, `|`, `[`, `<`, a leading
///   `#`, `>`, `-`, or `1.`) gets a backslash, so it shows as written. `@everyone`, `@here`, and
///   the `<@id>`, `<@&id>`, and `<#id>` forms get a zero-width space inside them, so they can't
///   ping anyone, in code too. Raw HTML is shown as text. Control characters other than tab and
///   newline are dropped.
/// - Quote and list markers, and styles, are closed and reopened where a message splits.
///
/// Each message ends at a paragraph break if one fits, else a line break, else a space, else
/// anywhere; the break is dropped. A code fence open at a split is closed at the end of the
/// message and reopened, with its language tag, at the start of the next, and open styles and
/// code spans are closed and reopened the same way. Empty or whitespace-only input gives no
/// messages, and no message is empty. No message exceeds `max_chars` (at least 1). Where the
/// limit leaves no room for the closing and reopening text, or a link or URL is longer than the
/// limit, the text is cut at the limit and its markup is not repaired.
pub fn discord(markdown: &str, max_chars: usize) -> Vec<String> {
    split::split(&text::render(markdown, Flavor::Discord), max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    const Z: &str = "\u{200b}";

    fn render(markdown: &str) -> Vec<String> {
        discord(markdown, DISCORD_MAX_CHARS)
    }

    #[test]
    fn renders_discord() {
        let cases: &[(&str, &str, &str)] = &[
            ("empty", "", ""),
            ("whitespace only", "  \n\n \t\n", ""),
            (
                "inline styles",
                "**bold**, *it*, ***both***, ~~gone~~, `code`",
                "**bold**, *it*, ***both***, ~~gone~~, `code`",
            ),
            (
                "underscore styles use asterisks",
                "__strong__ and _emphasis_",
                "**strong** and *emphasis*",
            ),
            (
                "nested styles",
                "**a *b ~~c~~* d** and *x **y** z*",
                "**a *b ~~c~~* d** and *x **y** z*",
            ),
            (
                "the same style nested writes one pair",
                "**a **b** c**",
                "**a b c**",
            ),
            (
                "styles close at line breaks",
                "**a\n*b*  \nc** d",
                "**a**\n***b***\n**c** d",
            ),
            (
                "inline code keeps markdown characters",
                "`*a* _b_ ~c~ [d](e) # f | g \\h`",
                "`*a* _b_ ~c~ [d](e) # f | g \\h`",
            ),
            (
                "inline code with a backtick",
                "``a`b`` and `` `c `` and ``d`` ``e```f``",
                "``a`b`` and `` `c `` and `d` ``e`\u{200b}`\u{200b}`f``",
            ),
            (
                "headings",
                "# One\n\n## Two\n\n### Three *x*\n\n#### Four\n\n###### Six",
                "# One\n\n## Two\n\n### Three *x*\n\n### Four\n\n### Six",
            ),
            (
                "setext heading",
                "Title\n=====\n\nSub\n---",
                "# Title\n\n## Sub",
            ),
            (
                "heading across lines is one line",
                "a <b\nc>\n===",
                "# a \\<\u{200b}b c>",
            ),
            (
                "unordered and ordered lists",
                "- a\n  - b\n    - c\n- d\n\n3. e\n4. f\n   1. g",
                "- a\n  - b\n    - c\n- d\n\n3. e\n4. f\n   1. g",
            ),
            (
                "star and plus bullets become dashes",
                "* a\n+ b",
                "- a\n\n- b",
            ),
            ("loose list", "- a\n\n  more\n- b", "- a\n\n  more\n\n- b"),
            (
                "task list",
                "- [ ] todo\n- [x] **done**",
                "- [ ] todo\n- [x] **done**",
            ),
            (
                "block quote, its empty lines keep the space Discord needs",
                "> a\n>\n> b\n> > c",
                "> a\n> \n> b\n> \n> > c",
            ),
            (
                "list and heading in a quote",
                "> - a\n> - b\n>\n> # h",
                "> - a\n> - b\n> \n> # h",
            ),
            (
                "code block keeps its language",
                "Run:\n\n```rust\nfn main() {\n\n    **x**\n}\n```\n\nDone.",
                "Run:\n\n```rust\nfn main() {\n\n    **x**\n}\n```\n\nDone.",
            ),
            (
                "code block language is cleaned",
                "```rust,ignore {x}\na\n```\n\n```ja va\nb\n```",
                "```rustignore\na\n```\n\n```ja\nb\n```",
            ),
            (
                "code block without a language, and indented",
                "```\na\n```\n\n    b\n    c",
                "```\na\n```\n\n```\nb\nc\n```",
            ),
            (
                "code block holding a fence",
                "````\n```\nx ``` y\n````",
                "```\n`\u{200b}`\u{200b}`\nx `\u{200b}`\u{200b}` y\n```",
            ),
            ("empty code block", "```py\n```", "```py\n\u{200b}\n```"),
            (
                "code block in a list and a quote",
                "1. step:\n\n   ```sh\n   make\n   ```\n\n> ```\n> x\n> ```",
                "1. step:\n\n   ```sh\n   make\n   ```\n\n> ```\n> x\n> ```",
            ),
            (
                "table",
                "| Name | Qty |\n|:--|--:|\n| apple | 3 |\n| fig | 12 |",
                "```\nName  | Qty\n------|----\napple |   3\nfig   |  12\n```",
            ),
            (
                "table cells are shown as written",
                "| *a* | `b` |\n|---|---|\n| [c](https://x.y/z) | <@1> \\| `` ` `` |",
                "```\na                 | b\n------------------|----------\nc <https://x.y/z> | <\u{200b}@1> | `\n```",
            ),
            (
                "table between paragraphs",
                "a\n\n| x |\n|---|\n| 1 |\n\nb",
                "a\n\n```\nx\n-\n1\n```\n\nb",
            ),
            (
                "links are masked without an embed",
                "[docs](https://example.com/a_b?x=1&y=2) and [*a* `b`](http://e.x/)",
                "[docs](<https://example.com/a_b?x=1&y=2>) and [*a* `b`](<http://e.x/>)",
            ),
            (
                "link targets are encoded",
                "[a](https://e.x/p(a)/b\\c`d)",
                "[a](<https://e.x/p%28a%29/b%5Cc%60d>)",
            ),
            (
                "link with no text is its url",
                "[](https://e.x/a_b) [ ](https://e.x/c)",
                "<https://e.x/a_b> <https://e.x/c>",
            ),
            (
                "link to something Discord doesn't link",
                "[x](mailto:a@b.c) [y](javascript:alert(1)) [z](./rel) [w]()",
                "x (`mailto:a@b.c`) y (`javascript:alert(1)`) z (`./rel`) w",
            ),
            (
                "autolinks",
                "<https://example.com/a_b*c> <me@example.com> <ftp://x/y_z>",
                "<https://example.com/a_b*c> me@example.com ftp://x/y_z",
            ),
            (
                "bare urls are wrapped",
                "see https://e.x/a_b/~c?d=e). Or (http://e.x/(z)), or HTTPS://E.X/a now",
                "see <https://e.x/a_b/~c?d=e>). Or (<http://e.x/(z)>), or <HTTPS://E.X/a> now",
            ),
            (
                "a url inside a word or link text is not wrapped",
                "xhttps://e.x [see https://e.x/a_b](https://e.x/c)",
                "xhttps://e.x [see https://e.x/a_b](<https://e.x/c>)",
            ),
            (
                "images become links without embeds",
                "![a *cat*](https://attacker.example/?d=1) ![](https://a.b/c.png)",
                "[a cat](<https://attacker.example/?d=1>) [image](<https://a.b/c.png>)",
            ),
            (
                "images that are not remote show their alt text",
                "![a](data:image/png;base64,AAAA) ![](/blobs/x) ![b](./b.png)",
                "[image: a] [image] [image: b]",
            ),
            (
                "image in a link shows its alt text",
                "[![badge](https://i.x/a.png)](https://l.x)",
                "[badge](<https://l.x>)",
            ),
            (
                "raw html is text",
                "<b>hi</b> a <br/> <!-- c -->\n\n<div>\n**x**\n</div>",
                "\\<b>hi\\</b> a \\<br/> \\<!-- c -->\n\n\\<div>\n\\*\\*x\\*\\*\n\\</div>",
            ),
            (
                "raw html cannot start a block",
                "<div>\n# h\n- b\n</div>\n\n<span\n> q>",
                "\\<div>\n\\# h\n\\- b\n\\</div>\n\n\\<\u{200b}span\n\n> q>",
            ),
            (
                "mentions",
                "@everyone @here @Everyone <@123> <@!123> <@&5> <#9> </cmd sub:1> <:e:1> <a:e:1> \
                 <t:1:R> <id:customize> <@$7>",
                "@\u{200b}everyone @\u{200b}here @\u{200b}Everyone \\<\u{200b}@123> \\<\u{200b}@!123> \
                 \\<\u{200b}@&5> \\<\u{200b}#9> \\<\u{200b}/cmd sub:1> \\<\u{200b}:e:1> \
                 \\<\u{200b}a:e:1> \\<\u{200b}t:1:R> id:customize \\<\u{200b}@$7>",
            ),
            (
                "mentions in code",
                "`@everyone <@1>`\n\n```\n@here <#2> </c:3>\n```",
                "`@\u{200b}everyone <\u{200b}@1>`\n\n```\n@\u{200b}here <\u{200b}#2> <\u{200b}/c:3>\n```",
            ),
            (
                "mentions in link targets and image text",
                "[x](https://e.x/@everyone) ![@here](https://e.x/i.png) user@here.com me@example.com",
                "[x](<https://e.x/%40everyone>) [@\u{200b}here](<https://e.x/i.png>) \
                 user@\u{200b}here.com me@example.com",
            ),
            (
                "markup characters are escaped",
                "2 \\* 3 \\_x\\_ \\~y\\~ \\`z\\` \\[a\\] a|b \\\\ \\<b 2 < 3",
                "2 \\* 3 \\_x\\_ \\~y\\~ \\`z\\` \\[a\\] a|b \\\\ \\<b 2 < 3",
            ),
            (
                "spoilers and underline markers are literal",
                "\\|\\|spoiler\\|\\| a||b x|y \\_\\_u\\_\\_",
                "\\|\\|spoiler\\|\\| a\\|\\|b x|y \\_\\_u\\_\\_",
            ),
            (
                "intraword underscores stay",
                "snake_case_name and \\_lead and trail\\_",
                "snake_case_name and \\_lead and trail\\_",
            ),
            (
                "line start markers are escaped",
                "\\# h\n\\## h\n\\> q\n\\>>> q\n\\- b\n\\+ b\n\\-# s\n1\\. n\n12\\. n",
                "\\# h\n\\## h\n\\> q\n\\>>> q\n\\- b\n\\+ b\n\\-# s\n1\\. n\n12\\. n",
            ),
            (
                "line start markers that Discord ignores stay",
                "#hash ##x +1 -5 1.5 2024. -x >x a # b > c - d",
                "#hash ##x +1 -5 1.5 2024. -x >x a # b > c - d",
            ),
            (
                "escaped markers after a soft break",
                "a\n\\# b\n\\- c",
                "a\n\\# b\n\\- c",
            ),
            (
                "escaped markers in a list item and a quote",
                "- \\# a\n- \\- b\n\n> \\> c\n> 1\\. d",
                "- \\# a\n- \\- b\n\n> \\> c\n> 1\\. d",
            ),
            (
                "link text that isn't a link can't start a block",
                "[# Big](foo)\n\n[1. Intro](#intro)\n\n[# Big]()\n\n[> q](x) and [# not at start](y)",
                "\\# Big (`foo`)\n\n1\\. Intro (`#intro`)\n\n\\# Big\n\n\\> q (`x`) and # not at start (`y`)",
            ),
            ("rule", "a\n\n---\n\nb", "a\n\n---\n\nb"),
            ("hard break", "a  \nb\\\nc", "a\nb\nc"),
            (
                "control characters",
                "a\u{1b}[31mb\u{7}c\u{85}\r\n\n`d\u{0}e`\n\n```\nx\u{8}y\n```",
                "a\\[31mbc\n\n`de`\n\n```\nxy\n```",
            ),
            (
                "multibyte",
                "# Grüße 👋🏽\n\n*Ünïcödé* 漢字",
                "# Grüße 👋🏽\n\n*Ünïcödé* 漢字",
            ),
        ];
        for (name, input, expected) in cases {
            let rendered = render(input);
            let want: &[&str] = if expected.is_empty() {
                &[]
            } else {
                &[expected]
            };
            assert_eq!(rendered, want, "case {name}");
        }
    }

    /// A form of mention in `text`, if there is one: what Discord would read from the content.
    fn mention_in(text: &str) -> Option<String> {
        let lower = text.to_lowercase();
        for form in ["@everyone", "@here", "<@", "<#", "<:", "<a:", "<t:", "<id:"] {
            if lower.contains(form) {
                return Some(form.to_owned());
            }
        }
        let mut rest = lower.as_str();
        while let Some(i) = rest.find("</") {
            rest = &rest[i + 2..];
            let end = rest.find(['>', '<', '\n']);
            if end.is_some_and(|e| rest[e..].starts_with('>') && rest[..e].contains(':')) {
                return Some("</name:id>".to_owned());
            }
        }
        None
    }

    #[test]
    fn nothing_that_pings_survives() {
        let forms = [
            "@everyone",
            "@here",
            "<@123>",
            "<@!123>",
            "<@&123>",
            "<#123>",
            "</cmd:123>",
            "<:name:123>",
            "<a:name:123>",
            "<t:123>",
            "<id:customize>",
        ];
        for form in forms {
            let contexts = [
                form.to_string(),
                format!("x {form} y"),
                format!("**{form}**"),
                format!("`{form}`"),
                format!("```\n{form}\n```"),
                format!("> {form}"),
                format!("- {form}"),
                format!("# {form}"),
                format!("[{form}](https://e.x/)"),
                format!("![{form}](https://e.x/i.png)"),
                format!("[x](https://e.x/{form})"),
                format!("| {form} |\n|---|\n| {form} |"),
                format!("<div>{form}</div>"),
                format!("<{form}>"),
            ];
            for markdown in contexts {
                let out = render(&markdown).concat();
                assert!(
                    !out.contains(form),
                    "{form} survives in {markdown:?}: {out:?}"
                );
            }
        }
        // A mention in pieces: inline markup between them doesn't survive to the output (an empty
        // link, a style in a table cell, an entity), so the pieces join into the form.
        let pieces = [
            "@[]()everyone",
            "@[]()here ping",
            "@[]()EVERYONE",
            "@ever[]()yone",
            "&#64;[]()here",
            "&#64;everyone &#60;@1>",
            "<[]()@1>",
            "<[]()#1>",
            "<a[]():e:1>",
            "<t[]():1>",
            "<[]()/cmd:1>",
            "</cmd[]():1>",
            "<[]()id:customize>",
            "| @*everyone* |\n|---|\n| x |",
            "| @*ev*eryone |\n|---|\n| x |",
            "| <`#1>` |\n|---|\n| x |",
            "| <[]()@1> |\n|---|\n| @**here** |",
            "| <*a*:e:1> |\n|---|\n| x |",
            "| @[x]()here |\n|---|\n| x |",
        ];
        for markdown in pieces {
            for limit in [DISCORD_MAX_CHARS, 7] {
                for message in discord(markdown, limit) {
                    assert_eq!(
                        mention_in(&message),
                        None,
                        "{markdown:?} @{limit}: {message:?}"
                    );
                }
            }
        }
        // The mention scan sees through a backslash, so the escape alone isn't enough.
        let out = render("<@123>").concat();
        assert!(!out.contains("<@"), "{out:?}");
        assert!(out.contains(&format!("<{Z}@123>")), "{out:?}");
    }

    #[test]
    fn splits_messages() {
        let cases: &[(&str, &str, usize, &[&str])] = &[
            ("empty", "", 10, &[]),
            ("blank", "\n\n  \n", 10, &[]),
            ("fits", "aaaa bbbb", 9, &["aaaa bbbb"]),
            (
                "splits at a space",
                "aaaa bbbb cccc",
                10,
                &["aaaa bbbb", "cccc"],
            ),
            ("collapses the break", "aaaa bbbb", 4, &["aaaa", "bbbb"]),
            (
                "paragraph break before line break",
                "aaa bbb\n\nccc ddd\neee",
                12,
                &["aaa bbb", "ccc ddd\neee"],
            ),
            (
                "line break before space",
                "aaa bbb\nccc ddd eee",
                14,
                &["aaa bbb", "ccc ddd eee"],
            ),
            ("line break", "aaa\nbbb\nccc", 8, &["aaa\nbbb", "ccc"]),
            ("hard split", "abcdefghij", 4, &["abcd", "efgh", "ij"]),
            (
                "hard split of a long word between words",
                "ab cdefghij kl",
                5,
                &["ab", "cdefg", "hij", "kl"],
            ),
            ("two-byte characters", "ééééé", 2, &["éé", "éé", "é"]),
            ("astral characters count once", "👋👋👋", 2, &["👋👋", "👋"]),
            (
                "a joined emoji can be cut between its code points",
                "👨\u{200d}👩\u{200d}👧",
                3,
                &["👨\u{200d}👩", "\u{200d}👧"],
            ),
            (
                "an escape is not cut",
                "aaa\\*bbb",
                4,
                &["aaa", "\\*bb", "b"],
            ),
            (
                "a url longer than the limit is cut",
                "<https://example.com/aaaaaaaaaaaa>",
                10,
                &["<https://e", "xample.com", "/aaaaaaaaa", "aaa>"],
            ),
            (
                "code fence reopened with its language",
                "```rust\naaaa\nbbbb\ncccc\n```",
                20,
                &[
                    "```rust\naaaa\n```",
                    "```rust\nbbbb\n```",
                    "```rust\ncccc\n```",
                ],
            ),
            (
                "code fence without a language",
                "```\naaaa bbbb cccc dddd\n```",
                14,
                &[
                    "```\naaaa\n```",
                    "```\nbbbb\n```",
                    "```\ncccc\n```",
                    "```\ndddd\n```",
                ],
            ),
            (
                "a fence that fits stays whole",
                "intro\n\n```\nx\ny\n```\n\noutro",
                12,
                &["intro", "```\nx\ny\n```", "outro"],
            ),
            (
                "code fence in a list",
                "- ```\n  aaaa bbbb\n  ```",
                20,
                &["- ```\n  aaaa\n  ```", "  ```\n  bbbb\n  ```"],
            ),
            (
                "code fence in a quote",
                "> ```py\n> aaaa bbbb\n> ```",
                22,
                &["> ```py\n> aaaa\n> ```", "> ```py\n> bbbb\n> ```"],
            ),
            (
                "table",
                "| a | b |\n|--|--|\n| 1 | 2 |\n| 3 | 4 |",
                24,
                &["```\na | b\n--|--\n```", "```\n1 | 2\n3 | 4\n```"],
            ),
            (
                "bold is reopened",
                "**aaaa bbbb cccc**",
                12,
                &["**aaaa**", "**bbbb**", "**cccc**"],
            ),
            (
                "bold and italic are reopened in order",
                "***aaaa bbbb***",
                12,
                &["***aaaa***", "***bbbb***"],
            ),
            (
                "only what is open is reopened",
                "~~aaaa bbbb~~ cc",
                11,
                &["~~aaaa~~", "~~bbbb~~ cc"],
            ),
            (
                "styles closed at a line break are not reopened twice",
                "**aaaa\nbbbb**",
                8,
                &["**aaaa**", "**bbbb**"],
            ),
            (
                "code span is reopened",
                "`aaaa bbbb`",
                8,
                &["`aaaa b`", "`bbb`"],
            ),
            (
                "a code span is not split at a space",
                "x `aaaa bbbb` y",
                12,
                &["x", "`aaaa bbbb`", "y"],
            ),
            (
                "a link is not split at a space",
                "x [aaaa bbbb](https://e.x/) y",
                30,
                &["x [aaaa bbbb](<https://e.x/>)", "y"],
            ),
            (
                "a link is moved whole to the next message",
                "xxxx yyyy [aa bb](https://e.x/) y",
                28,
                &["xxxx yyyy", "[aa bb](<https://e.x/>) y"],
            ),
            (
                "quote prefix is kept inside a quoted line",
                "> aaaa bbbb cccc",
                9,
                &["> aaaa", "> bbbb", "> cccc"],
            ),
            (
                "list marker stays with its text",
                "- aaaa bbbb cccc\n- dd",
                10,
                &["- aaaa", "bbbb cccc", "- dd"],
            ),
            (
                "an empty quote line is a paragraph break",
                "> aaaa\n>\n> bbbb",
                8,
                &["> aaaa", "> bbbb"],
            ),
            (
                "an empty quote line is dropped at a split, not left dangling",
                "> aa bb\n>\n> cc dd ee",
                9,
                &["> aa bb", "> cc dd", "> ee"],
            ),
            (
                "a space before a bullet is not a cut",
                "costs 5 - 3 = 2",
                8,
                &["costs", "5 - 3 =", "2"],
            ),
            (
                "a space before a block marker is not a cut",
                "aa > bb\naa # bb\naa -# bb\naa 1. bb\naa + bb",
                8,
                &["aa > bb", "aa # bb", "aa -# bb", "aa 1. bb", "aa + bb"],
            ),
            (
                "the text before a marker ends the message",
                "aaaa bbbb - cccc",
                10,
                &["aaaa", "bbbb -", "cccc"],
            ),
            (
                "ordered list",
                "1. aaaa bbbb cccc",
                11,
                &["1. aaaa", "bbbb cccc"],
            ),
            (
                "no room to close and reopen",
                "**ab**",
                2,
                &["**", "ab", "**"],
            ),
            ("limit of one", "ab", 1, &["a", "b"]),
            ("limit of zero is one", "ab", 0, &["a", "b"]),
        ];
        for (name, input, limit, expected) in cases {
            assert_eq!(discord(input, *limit), *expected, "case {name}");
        }
    }

    /// Discord counts Unicode scalar values: the limit is reached at 2000 of them whatever they
    /// weigh in bytes or UTF-16 units.
    #[test]
    fn limit_is_counted_in_code_points() {
        for unit in ["a", "é", "漢", "👋", "😀\u{fe0f}"] {
            let per = unit.chars().count();
            let fits = unit.repeat(DISCORD_MAX_CHARS / per);
            assert_eq!(fits.chars().count(), DISCORD_MAX_CHARS);
            assert_eq!(
                render(&fits),
                std::slice::from_ref(&fits),
                "exact limit of {unit:?}"
            );
            let over = format!("{fits}{unit}");
            let messages = render(&over);
            assert_eq!(messages.len(), 2, "one over the limit of {unit:?}");
            assert_eq!(messages[0].chars().count(), DISCORD_MAX_CHARS);
            assert_eq!(messages.concat(), over);
        }
    }

    #[test]
    fn splits_at_discords_limit_on_the_best_break() {
        let word = "word ".repeat(399); // 1995 characters, then the space is trimmed.
        let first = word.trim_end();
        // A paragraph break at the limit.
        let input = format!("{first}\n\nsecond paragraph");
        assert_eq!(render(&input), [first, "second paragraph"]);
        // A paragraph that ends exactly at the limit, and one that is a word over.
        let exact = format!("{}{}", "a".repeat(1999), "b");
        assert_eq!(render(&exact), std::slice::from_ref(&exact));
        let over = format!("{} {}", "a".repeat(1999), "b");
        assert_eq!(render(&over), ["a".repeat(1999), "b".to_owned()]);
        let over_by_break = format!("{}\n{}", "a".repeat(1999), "b");
        assert_eq!(render(&over_by_break), ["a".repeat(1999), "b".to_owned()]);
        // One token longer than the limit is cut at the limit.
        let token = "x".repeat(4500);
        let messages = render(&token);
        let lengths: Vec<_> = messages.iter().map(|m| m.chars().count()).collect();
        assert_eq!(lengths, [2000, 2000, 500]);
    }

    #[test]
    fn long_code_blocks_split_into_closed_fences() {
        let code: String = (0..400).map(|i| format!("let x{i} = {i};\n")).collect();
        for fence in ["```rust", "```"] {
            let messages = render(&format!("Here:\n\n{fence}\n{code}```\n\nDone."));
            assert!(messages.len() >= 3, "{}", messages.len());
            let opener = if fence == "```" { "```" } else { "```rust" };
            for (i, message) in messages.iter().enumerate() {
                assert!(message.chars().count() <= DISCORD_MAX_CHARS);
                let fences = message.lines().filter(|l| l.starts_with("```")).count();
                assert_eq!(
                    fences % 2,
                    0,
                    "message {i} leaves a fence open: {message:?}"
                );
                if i > 0 && i + 1 < messages.len() {
                    assert!(message.starts_with(&format!("{opener}\n")), "{message:?}");
                    assert!(message.ends_with("\n```"), "{message:?}");
                }
            }
            assert!(messages[0].starts_with("Here:"));
            assert!(
                messages
                    .last()
                    .is_some_and(|m| m.ends_with("\n```\n\nDone."))
            );
            let rebuilt: String = messages
                .concat()
                .lines()
                .filter(|line| !line.starts_with("```") && line.starts_with("let "))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(rebuilt, code.trim_end());
        }
    }

    /// Every message fits, none is empty, none opens a fence or style it doesn't close, and the
    /// text survives, whatever the limit.
    #[test]
    fn messages_are_well_formed_at_every_limit() {
        let input = "# Überschrift mit **Fettdruck** und *Kursiv*\n\n\
                     Ein langer Absatz mit **fett *und kursiv* gemischt** sowie Emoji 👋🏽 und \
                     CJK 漢字かな交じり文, ~~durchgestrichen und lang genug~~, `Code mit Leerzeichen`, \
                     gefolgt von https://example.com/ein/sehr/langer/pfad/ohne/leerzeichen.\n\n\
                     - Liste mit `Code` und ~~Durchgestrichen~~\n  - **Verschachtelt und ziemlich lang \
                     und noch länger**\n\n> Zitat mit *Kursiv* und einem langen Satz, der umbrochen werden \
                     muss, weil er so lang ist\n\n\
                     ```rust\nfn lange_zeile() { println!(\"{}\", \"x\".repeat(100)); }\n\nfn b() {}\n```\n\n\
                     | a | b |\n|---|---|\n| 1 | 2 |\n\n\
                     ```\n> kein Zitat\n- keine Liste\n```\n";
        let squash = |text: &str| -> String {
            text.chars()
                .filter(|c| !c.is_whitespace() && !"*~`>\\\u{200b}".contains(*c))
                .collect()
        };
        let whole = squash(&render(input).concat());
        // Rust-tagged fences repeat their tag, so compare without it.
        let strip_tag = |text: String| text.replace("rust", "");
        let whole = strip_tag(whole);
        for limit in 24..400 {
            let messages = discord(input, limit);
            assert!(!messages.is_empty());
            for message in &messages {
                let chars = message.chars().count();
                assert!(
                    chars <= limit,
                    "limit {limit}: {chars} chars in {message:?}"
                );
                assert!(!message.trim().is_empty(), "limit {limit}: empty message");
                let fences = message
                    .lines()
                    .filter(|l| l.trim_start_matches([' ', '>']).starts_with("```"))
                    .count();
                assert_eq!(fences % 2, 0, "limit {limit}: open fence in {message:?}");
            }
            let rejoined = strip_tag(squash(&messages.concat()));
            assert_eq!(rejoined, whole, "limit {limit} lost or changed text");
        }
    }

    /// Styles and code spans are closed and reopened, so every message has balanced markers.
    #[test]
    fn styles_are_balanced_in_every_message() {
        let input = "Ein langer Absatz mit **fett *und kursiv* gemischt**, ~~durchgestrichen und \
                     lang~~, `Code mit Leerzeichen` und ***alles zusammen in einem Satz*** bis \
                     zum Ende, und noch **ein fetter Schluss, der weit über das Limit reicht**.";
        for limit in 24..200 {
            for message in discord(input, limit) {
                for marker in ["*", "~~", "`"] {
                    let count = message.matches(marker).count();
                    assert_eq!(count % 2, 0, "limit {limit}: {marker} in {message:?}");
                }
            }
        }
    }
    /// Mentions built from arbitrary pieces never survive, in any message.
    #[test]
    fn random_pieces_never_make_a_mention() {
        const PIECES: &[&str] = &[
            "@",
            "everyone",
            "here",
            "ever",
            "yone",
            "<",
            "@1>",
            "#1>",
            ":e:1>",
            "a",
            "t",
            "id",
            "/cmd",
            ":1>",
            ":",
            "[]()",
            "`",
            "*",
            "**",
            "&#64;",
            "&#60;",
            "|",
            " ",
            "\n",
            "\n\n",
            ">",
            "> ",
            "| a |\n|---|\n",
            "```\n",
            "<b>",
            "x",
            "\\",
            "_",
            "~~",
            "[l](https://e.x/",
            ")",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        for _ in 0..3000 {
            let markdown: String = (0..next(24)).map(|_| PIECES[next(PIECES.len())]).collect();
            for limit in [DISCORD_MAX_CHARS, 40] {
                for message in discord(&markdown, limit) {
                    assert_eq!(
                        mention_in(&message),
                        None,
                        "{markdown:?} @{limit}: {message:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn pathological_urls_are_fast() {
        let started = std::time::Instant::now();
        let out = render(&format!("see http://x.y/{}", ")".repeat(50_000)));
        assert!(!out.is_empty());
        let out = render(&"</".repeat(50_000));
        assert!(!out.is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// Arbitrary mixes of markup never panic, and always fit and have something to show, even in
    /// limits too small to repair anything.
    #[test]
    fn arbitrary_markup_always_fits() {
        const PIECES: &[&str] = &[
            "a",
            "bc",
            " ",
            "  ",
            "\n",
            "\n\n",
            "**",
            "*",
            "_",
            "~~",
            "`",
            "``",
            "```",
            "```rs\n",
            "[",
            "](https://e.x/y)",
            "](",
            ">",
            "> ",
            "- ",
            "1. ",
            "# ",
            "|",
            "| a | b |\n|--|--|\n",
            "<",
            "<https://e.x/z>",
            "@everyone",
            "<@1>",
            "\\",
            "é",
            "👋",
            "\u{200b}",
            "http://e.x/a_b",
            "![i](https://e.x/i.png)",
            "<b>",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        for _ in 0..400 {
            let markdown: String = (0..next(40)).map(|_| PIECES[next(PIECES.len())]).collect();
            for limit in [1, 2, 3, 5, 8, 13, 21, 34, 55] {
                for message in discord(&markdown, limit) {
                    assert!(
                        message.chars().count() <= limit,
                        "{limit}: {message:?} from {markdown:?}"
                    );
                    assert!(
                        !message.trim().is_empty(),
                        "{limit}: empty from {markdown:?}"
                    );
                }
            }
        }
    }
}
