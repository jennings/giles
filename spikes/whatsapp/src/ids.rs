//! Pure helpers: phone number parsing, sender identities (phone number plus LID alias), chat kinds,
//! and echo classification. Kept free of I/O so they can be unit tested.

use std::collections::HashSet;
use std::fmt;

use whatsapp_rust::Server;
use whatsapp_rust::wacore_binary::jid::{Jid, JidExt};

/// Parses a phone number with country code into the digits-only form WhatsApp uses in JIDs and
/// pair-code requests (`"+1 (555) 123-4567"` becomes `"15551234567"`).
///
/// Accepts an optional leading `+` and the separators space, `-`, `.`, `(`, `)`. Requires 7 to 15
/// digits (the E.164 maximum) that don't start with `0` (a country code never does; a leading `0`
/// is a national trunk prefix, which WhatsApp rejects).
pub fn parse_phone(input: &str) -> Result<String, PhoneError> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('+').unwrap_or(trimmed);
    let mut digits = String::with_capacity(rest.len());
    for c in rest.chars() {
        match c {
            '0'..='9' => digits.push(c),
            ' ' | '-' | '.' | '(' | ')' => {}
            other => return Err(PhoneError::InvalidChar(other)),
        }
    }
    if !(7..=15).contains(&digits.len()) {
        return Err(PhoneError::Length(digits.len()));
    }
    if digits.starts_with('0') {
        return Err(PhoneError::LeadingZero);
    }
    Ok(digits)
}

/// Why [`parse_phone`] rejected its input.
#[derive(Debug, PartialEq, Eq)]
pub enum PhoneError {
    /// A character other than a digit, a leading `+`, or a separator.
    InvalidChar(char),
    /// The number of digits is outside 7..=15.
    Length(usize),
    /// The number starts with `0` instead of a country code.
    LeadingZero,
}

impl fmt::Display for PhoneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidChar(c) => write!(f, "unexpected character {c:?} in phone number"),
            Self::Length(n) => write!(f, "phone number has {n} digits, expected 7 to 15"),
            Self::LeadingZero => {
                f.write_str("phone number must start with the country code, not 0")
            }
        }
    }
}

impl std::error::Error for PhoneError {}

/// The identities of a message sender: the phone number Giles would use as the canonical id
/// (`whatsapp:+15551234567`) and the LID it would report as an alias (PLAN section 3.2).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SenderIds {
    /// `+` and the digits, e.g. `+15551234567`.
    pub phone: Option<String>,
    /// The LID user with its server, e.g. `100000012345678@lid`.
    pub lid: Option<String>,
}

/// Collects the sender's phone number and LID from the sender JID, the alternate JID the server
/// sent alongside it (`sender_alt`: the PN for a LID-addressed message and vice versa), and a
/// LID-to-phone mapping from the client's store (`(lid_user, phone_user)`), in that order of
/// preference. Device suffixes are dropped: every device of an account is the same person.
pub fn sender_ids(sender: &Jid, alt: Option<&Jid>, mapping: Option<(&str, &str)>) -> SenderIds {
    let mut ids = SenderIds::default();
    for jid in std::iter::once(sender).chain(alt) {
        match jid.server {
            Server::Pn | Server::Legacy | Server::Hosted if ids.phone.is_none() => {
                ids.phone = Some(format!("+{}", jid.user_base()));
            }
            Server::Lid | Server::HostedLid if ids.lid.is_none() => {
                ids.lid = Some(format!("{}@lid", jid.user_base()));
            }
            _ => {}
        }
    }
    if let Some((lid, phone)) = mapping {
        ids.phone.get_or_insert_with(|| format!("+{phone}"));
        ids.lid.get_or_insert_with(|| format!("{lid}@lid"));
    }
    ids
}

/// What kind of conversation a chat JID names, as Giles would report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    /// A one-to-one chat (`ConversationKind::Direct`).
    Direct,
    /// A group chat (`ConversationKind::Group`).
    Group,
    /// A status update (`status@broadcast`); the plugin should ignore these.
    Status,
    /// A channel/newsletter or broadcast list; the plugin should ignore these.
    Other,
}

impl ChatKind {
    /// Classifies a chat. `is_group` is the message source's own flag, which the library also sets
    /// for groups.
    pub fn of(chat: &Jid, is_group: bool) -> Self {
        if chat.is_status_broadcast() {
            Self::Status
        } else if is_group || chat.is_group() {
            Self::Group
        } else if chat.is_newsletter() || chat.is_broadcast_list() {
            Self::Other
        } else {
            Self::Direct
        }
    }
}

impl fmt::Display for ChatKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Direct => "direct",
            Self::Group => "group",
            Self::Status => "status",
            Self::Other => "other",
        })
    }
}

/// Whether a received message is a copy of one sent from the linked account (PLAN section 3.3:
/// the plugin must drop these before they reach the core).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Echo {
    /// Sent by someone else.
    No,
    /// Sent by this spike (its id is in the sent log).
    OwnSend,
    /// Sent from another device of the linked account, e.g. the phone.
    OtherDevice,
}

impl Echo {
    /// Classifies a message by the protocol's `is_from_me` flag and the ids this spike sent.
    pub fn classify(is_from_me: bool, message_id: &str, sent: &SentLog) -> Self {
        if sent.contains(message_id) {
            Self::OwnSend
        } else if is_from_me {
            Self::OtherDevice
        } else {
            Self::No
        }
    }
}

impl fmt::Display for Echo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::No => "no",
            Self::OwnSend => "own-send",
            Self::OtherDevice => "other-device",
        })
    }
}

/// The ids of messages this spike sent, one per line in `<data>/sent-ids`.
#[derive(Debug, Default)]
pub struct SentLog {
    ids: HashSet<String>,
}

impl SentLog {
    /// Parses the log file's contents. Blank lines and surrounding whitespace are ignored.
    pub fn parse(contents: &str) -> Self {
        let ids = contents
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        Self { ids }
    }

    /// Whether `id` was sent by this spike.
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// The line to append to the log for a newly sent message.
    pub fn line(id: &str) -> String {
        format!("{}\n", id.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_phone_accepts_formatted_numbers() {
        assert_eq!(parse_phone("+1 (555) 123-4567").unwrap(), "15551234567");
        assert_eq!(parse_phone(" 44.20.7946.0958 ").unwrap(), "442079460958");
        assert_eq!(parse_phone("15551234567").unwrap(), "15551234567");
    }

    #[test]
    fn parse_phone_enforces_length_bounds() {
        assert_eq!(parse_phone("1234567").unwrap(), "1234567");
        assert_eq!(parse_phone("123456"), Err(PhoneError::Length(6)));
        assert_eq!(parse_phone("123456789012345").unwrap(), "123456789012345");
        assert_eq!(parse_phone("1234567890123456"), Err(PhoneError::Length(16)));
        assert_eq!(parse_phone("+"), Err(PhoneError::Length(0)));
    }

    #[test]
    fn parse_phone_rejects_bad_input() {
        assert_eq!(parse_phone("05551234567"), Err(PhoneError::LeadingZero));
        assert_eq!(
            parse_phone("+1555123456x"),
            Err(PhoneError::InvalidChar('x'))
        );
        // Only one leading plus, and only at the start.
        assert_eq!(
            parse_phone("++15551234567"),
            Err(PhoneError::InvalidChar('+'))
        );
        assert_eq!(
            parse_phone("1555+1234567"),
            Err(PhoneError::InvalidChar('+'))
        );
        assert_eq!(
            parse_phone("1555@s.whatsapp.net"),
            Err(PhoneError::InvalidChar('@'))
        );
    }

    #[test]
    fn sender_ids_from_pn_sender_with_lid_alt() {
        let ids = sender_ids(
            &Jid::pn_device("15551234567", 3),
            Some(&Jid::lid("100000012345678")),
            None,
        );
        assert_eq!(ids.phone.as_deref(), Some("+15551234567"));
        assert_eq!(ids.lid.as_deref(), Some("100000012345678@lid"));
    }

    #[test]
    fn sender_ids_from_lid_sender_with_pn_alt() {
        let ids = sender_ids(
            &Jid::lid_device("100000012345678", 12),
            Some(&Jid::pn("15551234567")),
            None,
        );
        assert_eq!(ids.phone.as_deref(), Some("+15551234567"));
        assert_eq!(ids.lid.as_deref(), Some("100000012345678@lid"));
    }

    #[test]
    fn sender_ids_fall_back_to_the_store_mapping() {
        let lid_only = sender_ids(
            &Jid::lid("100000012345678"),
            None,
            Some(("100000012345678", "15551234567")),
        );
        assert_eq!(lid_only.phone.as_deref(), Some("+15551234567"));
        assert_eq!(lid_only.lid.as_deref(), Some("100000012345678@lid"));

        // The message's own JIDs win over the mapping.
        let both = sender_ids(
            &Jid::pn("15551234567"),
            Some(&Jid::lid("100000012345678")),
            Some(("999", "999")),
        );
        assert_eq!(both.phone.as_deref(), Some("+15551234567"));
        assert_eq!(both.lid.as_deref(), Some("100000012345678@lid"));
    }

    #[test]
    fn sender_ids_without_alias_leave_it_empty() {
        let ids = sender_ids(&Jid::lid("100000012345678"), None, None);
        assert_eq!(ids.phone, None);
        assert_eq!(ids.lid.as_deref(), Some("100000012345678@lid"));
    }

    #[test]
    fn chat_kind_classifies_chats() {
        let parse = |s: &str| s.parse::<Jid>().unwrap();
        assert_eq!(
            ChatKind::of(&Jid::pn("15551234567"), false),
            ChatKind::Direct
        );
        assert_eq!(
            ChatKind::of(&Jid::lid("100000012345678"), false),
            ChatKind::Direct
        );
        assert_eq!(
            ChatKind::of(&parse("120363025246125486@g.us"), false),
            ChatKind::Group
        );
        assert_eq!(ChatKind::of(&Jid::pn("15551234567"), true), ChatKind::Group);
        assert_eq!(
            ChatKind::of(&parse("status@broadcast"), false),
            ChatKind::Status
        );
        assert_eq!(
            ChatKind::of(&parse("120363144038483540@newsletter"), false),
            ChatKind::Other
        );
    }

    #[test]
    fn echo_prefers_own_sends_over_the_from_me_flag() {
        let sent = SentLog::parse("3EB0AAAA\n\n  3EB0BBBB  \n");
        assert_eq!(Echo::classify(true, "3EB0AAAA", &sent), Echo::OwnSend);
        assert_eq!(Echo::classify(false, "3EB0BBBB", &sent), Echo::OwnSend);
        assert_eq!(Echo::classify(true, "3EB0CCCC", &sent), Echo::OtherDevice);
        assert_eq!(Echo::classify(false, "3EB0CCCC", &sent), Echo::No);
        assert!(!sent.contains(""));
    }

    #[test]
    fn sent_log_lines_round_trip() {
        let contents = [SentLog::line("3EB0AAAA"), SentLog::line(" 3EB0BBBB\n")].concat();
        assert_eq!(contents, "3EB0AAAA\n3EB0BBBB\n");
        let sent = SentLog::parse(&contents);
        assert!(sent.contains("3EB0AAAA") && sent.contains("3EB0BBBB"));
    }
}
