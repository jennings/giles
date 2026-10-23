//! Inbound `PRIVMSG`s as channel events: identities, conversations, text, and event ids.

use giles_plugin_sdk::giles_types::{
    ChannelEvent, ConversationId, ConversationKind, ConversationRef, EventId, InboundMessage,
    MessageId, NativeId, Result, Timestamp,
};
use irc_proto::message::Tag;
use sha2::{Digest as _, Sha256};

use crate::casemap::CaseMapping;

/// What the connection knows that decides how a message maps.
#[derive(Debug, Clone, Copy)]
pub(crate) struct View<'a> {
    /// Our current nick.
    pub nick: &'a str,
    /// The server's case mapping.
    pub casemap: CaseMapping,
    /// Channel prefixes (`CHANTYPES`).
    pub chantypes: &'a str,
    /// Prefixes for messages to some members of a channel (`STATUSMSG`), e.g. `@#chan`.
    pub statusmsg: &'a str,
    /// Whether the `account-tag` capability is on, so an `account` tag comes from the server.
    pub account_tag: bool,
    /// Whether mask identities count as verified.
    pub mask_identities: bool,
}

/// A `PRIVMSG` as received.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Privmsg<'a> {
    /// Message tags (values unescaped).
    pub tags: &'a [Tag],
    /// The sender's nick.
    pub nick: &'a str,
    /// The sender's user name (may be empty if the server left it out).
    pub user: &'a str,
    /// The sender's host (may be empty if the server left it out).
    pub host: &'a str,
    /// The channel or nick the message was sent to.
    pub target: &'a str,
    /// The text.
    pub text: &'a str,
}

/// A sender's identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identity {
    /// The canonical id.
    pub sender: NativeId,
    /// Other ids for the same sender.
    pub aliases: Vec<NativeId>,
    /// Whether the server vouches for `sender`.
    pub authenticated: bool,
}

/// The sender's identities: `account:<account>` (verified) when the server tagged the message
/// with a services account; otherwise `mask:<nick>!<user>@<host>`, verified only if mask
/// identities are on. With both, the mask is an alias. Nicks and accounts are case-folded, hosts
/// lowercased.
pub(crate) fn identify(
    view: &View<'_>,
    account: Option<&str>,
    nick: &str,
    user: &str,
    host: &str,
) -> Result<Identity> {
    let mask = NativeId::new(format!(
        "mask:{}!{user}@{}",
        view.casemap.fold(nick),
        host.to_ascii_lowercase()
    ))?;
    let complete = !user.is_empty() && !host.is_empty();
    let account = account.filter(|a| view.account_tag && !a.is_empty() && *a != "*");
    Ok(match account {
        Some(account) => Identity {
            sender: NativeId::new(format!("account:{}", view.casemap.fold(account)))?,
            aliases: if view.mask_identities && complete {
                vec![mask]
            } else {
                vec![]
            },
            authenticated: true,
        },
        None => Identity {
            sender: mask,
            aliases: vec![],
            authenticated: view.mask_identities && complete,
        },
    })
}

/// The value of tag `key`, if present with a value.
pub(crate) fn tag<'a>(tags: &'a [Tag], key: &str) -> Option<&'a str> {
    tags.iter()
        .find(|Tag(k, _)| k == key)
        .and_then(|Tag(_, v)| v.as_deref())
}

/// The event for `msg`, or `None` for messages that don't become events: our own (echoes), CTCP
/// requests other than `ACTION`, messages with nothing to show, and ones addressed to neither a
/// channel nor us. `received` makes the event id unique when the server gives neither a `msgid`
/// nor a `time` tag (e.g. the receive time and a counter).
pub(crate) fn event(
    view: &View<'_>,
    msg: &Privmsg<'_>,
    received: &str,
) -> Result<Option<ChannelEvent>> {
    let casemap = view.casemap;
    if casemap.eq(msg.nick, view.nick) {
        return Ok(None);
    }
    let Some(text) = text(msg.text) else {
        return Ok(None);
    };
    let conversation = if let Some(channel) = channel(view, msg.target) {
        ConversationRef {
            id: ConversationId::new(casemap.fold(channel))?,
            kind: ConversationKind::Group,
            title: Some(channel.to_owned()),
        }
    } else if casemap.eq(msg.target, view.nick) {
        ConversationRef {
            id: ConversationId::new(casemap.fold(msg.nick))?,
            kind: ConversationKind::Direct,
            title: Some(msg.nick.to_owned()),
        }
    } else {
        return Ok(None);
    };
    let identity = identify(view, tag(msg.tags, "account"), msg.nick, msg.user, msg.host)?;
    let msgid = tag(msg.tags, "msgid").filter(|id| !id.is_empty());
    let time = tag(msg.tags, "time");
    let id = match msgid {
        Some(msgid) => EventId::new(format!("msgid:{msgid}"))?,
        None => EventId::new(format!(
            "sha256:{}",
            hash(&[
                time.unwrap_or(received),
                msg.nick,
                msg.user,
                msg.host,
                msg.target,
                msg.text
            ])
        ))?,
    };
    let reply_to = tag(msg.tags, "+draft/reply")
        .or_else(|| tag(msg.tags, "+reply"))
        .and_then(|id| MessageId::new(id).ok());
    Ok(Some(ChannelEvent::Message {
        id,
        message: InboundMessage {
            conversation,
            sender: identity.sender,
            sender_aliases: identity.aliases,
            sender_authenticated: identity.authenticated,
            sender_display_name: Some(msg.nick.to_owned()),
            text,
            attachments: vec![],
            message_id: msgid.and_then(|id| MessageId::new(id).ok()),
            reply_to,
            sent_at: time.and_then(|time| time.parse::<Timestamp>().ok()),
        },
    }))
}

/// The channel `target` names, without a `STATUSMSG` prefix, if it is one.
fn channel<'a>(view: &View<'_>, target: &'a str) -> Option<&'a str> {
    let target = match target.strip_prefix(|c| view.statusmsg.contains(c)) {
        Some(rest) if rest.starts_with(|c| view.chantypes.contains(c)) => rest,
        _ => target,
    };
    target
        .starts_with(|c| view.chantypes.contains(c))
        .then_some(target)
}

/// Hex SHA-256 of `parts`, each followed by a NUL.
fn hash(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The text to report: CTCP `ACTION` as `/me ...`, other CTCP dropped, formatting codes and
/// control characters removed. `None` if nothing visible is left.
pub(crate) fn text(raw: &str) -> Option<String> {
    let text = match raw.strip_prefix('\x01') {
        Some(ctcp) => {
            let ctcp = ctcp.strip_suffix('\x01').unwrap_or(ctcp);
            let action = ctcp.strip_prefix("ACTION")?;
            if !action.is_empty() && !action.starts_with(' ') {
                return None;
            }
            format!("/me{}", strip_formatting(action))
        }
        None => strip_formatting(raw),
    };
    (!text.trim().is_empty()).then_some(text)
}

/// `text` without IRC formatting: bold, italics, underline, strikethrough, monospace, reverse,
/// reset, and colors (`\x03` with up to two digits for the foreground and the background, `\x04`
/// with six hex digits each). Tabs become spaces; other control characters are dropped.
pub(crate) fn strip_formatting(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let (digit, len): (fn(&char) -> bool, usize) = match c {
            '\x03' => (char::is_ascii_digit, 2),
            '\x04' => (char::is_ascii_hexdigit, 6),
            '\t' => {
                out.push(' ');
                continue;
            }
            c if c.is_control() => continue,
            c => {
                out.push(c);
                continue;
            }
        };
        let skip = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| {
            let mut taken = 0;
            while taken < len && chars.next_if(digit).is_some() {
                taken += 1;
            }
            taken
        };
        if skip(&mut chars) > 0 {
            let mut lookahead = chars.clone();
            if lookahead.next() == Some(',') && lookahead.peek().is_some_and(digit) {
                chars.next();
                skip(&mut chars);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
