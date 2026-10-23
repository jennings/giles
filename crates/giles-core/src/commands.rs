//! Core commands: `/new`, `/stop`, and approval replies, which the core handles itself and never
//! passes to an agent (PLAN sections 3.2, 6.2, 6.3, 6.4). This module only parses; the sessions,
//! turn-control, and approval code acts on the result. Design:
//! `Docs/technical/identity-and-routing.md`.

use crate::router::Routed;

/// A core command parsed from an inbound message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CoreCommand {
    /// `/new`: end the conversation's session; later messages start a fresh one.
    New,
    /// `/stop`: cancel the session's running turn.
    Stop,
    /// `yes`, `yes!`, or `no`, optionally followed by a short code, answering a pending approval.
    Approval(ApprovalReply),
}

/// An answer to a pending approval, e.g. `yes 4821`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalReply {
    /// What the user answered.
    pub answer: Answer,
    /// The short code, if the user gave one. `None`: the reply names no approval, and may answer
    /// any approval pending for the requester in this session.
    pub code: Option<String>,
}

/// The answer in an [`ApprovalReply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Answer {
    /// `yes`: allow this call.
    Yes,
    /// `yes!`: allow this call and the same tool for the rest of the session.
    YesForSession,
    /// `no`: deny this call.
    No,
}

/// The approvals waiting for an answer, as [`crate::approvals`] tracks them. A `yes` or
/// `no` is an approval reply only if this reports a matching pending approval; otherwise it's an
/// ordinary message.
pub trait PendingApprovals {
    /// Whether `to.user` has an approval pending in the session `to.session`; with `code`, one
    /// with that short code (as the user typed it).
    fn is_pending(&self, to: &Routed, code: Option<&str>) -> bool;
}

impl<F: Fn(&Routed, Option<&str>) -> bool> PendingApprovals for F {
    fn is_pending(&self, to: &Routed, code: Option<&str>) -> bool {
        self(to, code)
    }
}

/// [`PendingApprovals`] with nothing ever pending.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPendingApprovals;

impl PendingApprovals for NoPendingApprovals {
    fn is_pending(&self, _: &Routed, _: Option<&str>) -> bool {
        false
    }
}

/// Parses `text` as a core command.
///
/// Leading and trailing whitespace is ignored and words are ASCII case-insensitive (phones
/// capitalize the first letter). Recognized forms, each the whole message:
///
/// - `/new` and `/stop`, with nothing after them. `/new@bot`, `/new please`, `/link`, and any
///   other `/word` are ordinary text.
/// - `yes`, `yes!`, or `no`, optionally followed by whitespace and a short code of ASCII letters
///   and digits. It's an approval reply only if `is_pending(code)` says an approval matching it
///   is pending; otherwise (`no` answering a question from the agent) it's ordinary text.
pub fn parse(text: &str, is_pending: impl FnOnce(Option<&str>) -> bool) -> Option<CoreCommand> {
    let mut words = text.split_whitespace();
    let first = words.next()?;
    let second = words.next();
    if words.next().is_some() {
        return None;
    }
    if second.is_none() {
        if first.eq_ignore_ascii_case("/new") {
            return Some(CoreCommand::New);
        }
        if first.eq_ignore_ascii_case("/stop") {
            return Some(CoreCommand::Stop);
        }
    }
    let answer = if first.eq_ignore_ascii_case("yes") {
        Answer::Yes
    } else if first.eq_ignore_ascii_case("yes!") {
        Answer::YesForSession
    } else if first.eq_ignore_ascii_case("no") {
        Answer::No
    } else {
        return None;
    };
    if let Some(code) = second
        && !code.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    is_pending(second).then(|| {
        CoreCommand::Approval(ApprovalReply {
            answer,
            code: second.map(str::to_owned),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(answer: Answer, code: Option<&str>) -> Option<CoreCommand> {
        Some(CoreCommand::Approval(ApprovalReply {
            answer,
            code: code.map(str::to_owned),
        }))
    }

    /// Pending approvals: any bare reply matches, and coded replies match code `4821`.
    fn pending(code: Option<&str>) -> bool {
        code.is_none_or(|c| c == "4821")
    }

    #[test]
    fn slash_commands() {
        let cases: &[(&str, Option<CoreCommand>)] = &[
            ("/new", Some(CoreCommand::New)),
            ("/stop", Some(CoreCommand::Stop)),
            ("  /new\n", Some(CoreCommand::New)),
            ("\t/stop ", Some(CoreCommand::Stop)),
            ("/New", Some(CoreCommand::New)),
            ("/STOP", Some(CoreCommand::Stop)),
            ("/new please", None),
            ("/stop now", None),
            ("/new@giles", None),
            ("/new.", None),
            ("new", None),
            ("/ new", None),
            ("//new", None),
            ("/link", None),
            ("/link ABCD2345", None),
            ("/foo", None),
            ("/", None),
            ("", None),
            ("   ", None),
            ("please /new", None),
            // Non-ASCII case folding doesn't apply: `ſ` is not `s`.
            ("/ſtop", None),
        ];
        for (text, want) in cases {
            assert_eq!(&parse(text, |_| true), want, "{text:?}");
            assert_eq!(&parse(text, |_| false), want, "{text:?}");
        }
    }

    #[test]
    fn approval_replies() {
        let cases: &[(&str, Option<CoreCommand>)] = &[
            ("yes", reply(Answer::Yes, None)),
            ("no", reply(Answer::No, None)),
            ("yes!", reply(Answer::YesForSession, None)),
            ("Yes", reply(Answer::Yes, None)),
            ("NO", reply(Answer::No, None)),
            ("YES!", reply(Answer::YesForSession, None)),
            (" yes \n", reply(Answer::Yes, None)),
            ("yes 4821", reply(Answer::Yes, Some("4821"))),
            ("no 4821", reply(Answer::No, Some("4821"))),
            ("yes! 4821", reply(Answer::YesForSession, Some("4821"))),
            ("Yes!   4821 ", reply(Answer::YesForSession, Some("4821"))),
            ("yes\n4821", reply(Answer::Yes, Some("4821"))),
            // A code that isn't pending: ordinary text.
            ("yes 1234", None),
            ("no 5", None),
            // Not the reply syntax.
            ("yes please", None),
            ("yes 4821 thanks", None),
            ("yes, 4821", None),
            ("yes.", None),
            ("yes !", None),
            ("yes 48-21", None),
            ("yes 4821!", None),
            ("y", None),
            ("yes!!", None),
            ("nope", None),
            ("ok", None),
            ("!yes", None),
            ("4821", None),
        ];
        for (text, want) in cases {
            assert_eq!(&parse(text, pending), want, "{text:?}");
        }
    }

    #[test]
    fn replies_with_nothing_pending_are_ordinary_text() {
        for text in ["yes", "no", "yes!", "yes 4821", "no 4821", "yes! 4821"] {
            assert_eq!(parse(text, |_| false), None, "{text:?}");
        }
    }

    #[test]
    fn pending_lookup_gets_the_code_as_typed() {
        let mut asked = Vec::new();
        parse("yes AbC9", |code| {
            asked.push(code.map(str::to_owned));
            true
        });
        parse("no", |code| {
            asked.push(code.map(str::to_owned));
            true
        });
        parse("/new", |code| {
            asked.push(code.map(str::to_owned));
            true
        });
        parse("maybe 1", |code| {
            asked.push(code.map(str::to_owned));
            true
        });
        assert_eq!(asked, [Some("AbC9".to_owned()), None]);
    }
}
