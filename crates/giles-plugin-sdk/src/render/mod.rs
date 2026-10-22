//! Renderers for the CommonMark the model writes, one per channel output format.
//!
//! Every renderer parses with the same CommonMark extensions: tables, strikethrough, and task
//! lists. Raw HTML in the input is never passed through: each renderer shows it as literal text.
//! Control characters other than tab and newline never reach the output.

mod html;
mod irc;
mod text;

pub use html::{html, is_local_image};
pub use irc::irc;
pub use text::plain_text;

use std::borrow::Cow;

use pulldown_cmark::{Options, Parser};

/// Parses `markdown` with the extensions every renderer supports.
fn parser(markdown: &str) -> Parser<'_> {
    Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS,
    )
}

/// Removes control characters (C0, DEL, and C1) except tab and newline.
fn strip_controls(text: &str) -> Cow<'_, str> {
    let unwanted = |c: char| c.is_control() && c != '\t' && c != '\n';
    if text.contains(unwanted) {
        Cow::Owned(text.chars().filter(|c| !unwanted(*c)).collect())
    } else {
        Cow::Borrowed(text)
    }
}
