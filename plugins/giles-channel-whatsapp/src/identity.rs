//! Who a WhatsApp account is, in Giles terms (PLAN section 3.2).
//!
//! The identity is the phone number, `+15551234567` in the `whatsapp` namespace, with the LID
//! (`100000012345678@lid`) as an alias. WhatsApp often shows a sender only by LID, so the phone
//! number comes from the message's alternate address or from the client's learned LID-to-phone
//! map. When it's unknown, the LID itself is the identity: a stable id the owner can link.

use std::fmt;

use crate::backend::{Address, AddressKind};

/// The longest phone number E.164 allows, in digits.
const MAX_DIGITS: usize = 15;
/// The shortest number the plugin accepts. Real numbers are at least 7 digits with a country code.
const MIN_DIGITS: usize = 7;

/// Parses a phone number with country code into its digits (`"+1 (555) 123-4567"` becomes
/// `"15551234567"`), the form JIDs and pair-code requests use.
///
/// Accepts a leading `+` and the separators space, `-`, `.`, `(`, and `)`. Requires 7 to 15 digits
/// that don't start with `0`: a country code never does, and a leading `0` is a national trunk
/// prefix, which WhatsApp rejects.
pub(crate) fn parse_phone(input: &str) -> Result<String, PhoneError> {
    let trimmed = input.trim();
    let body = trimmed.strip_prefix('+').unwrap_or(trimmed);
    let mut digits = String::with_capacity(body.len());
    for c in body.chars() {
        match c {
            '0'..='9' => digits.push(c),
            ' ' | '-' | '.' | '(' | ')' => {}
            _ => return Err(PhoneError::BadCharacter(c)),
        }
    }
    if digits.is_empty() {
        return Err(PhoneError::Empty);
    }
    if digits.starts_with('0') {
        return Err(PhoneError::LeadingZero);
    }
    if !(MIN_DIGITS..=MAX_DIGITS).contains(&digits.len()) {
        return Err(PhoneError::Length(digits.len()));
    }
    Ok(digits)
}

/// Why [`parse_phone`] rejected its input.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PhoneError {
    /// No digits at all.
    Empty,
    /// A character that isn't a digit or a known separator.
    BadCharacter(char),
    /// Starts with `0`: a trunk prefix, not a country code.
    LeadingZero,
    /// Not 7 to 15 digits (the count is given).
    Length(usize),
}

impl fmt::Display for PhoneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("no digits"),
            Self::BadCharacter(c) => write!(f, "unexpected character {c:?}"),
            Self::LeadingZero => f.write_str(
                "starts with 0: write the number with its country code, like +15551234567",
            ),
            Self::Length(n) => write!(f, "{n} digits, expected {MIN_DIGITS} to {MAX_DIGITS}"),
        }
    }
}

impl std::error::Error for PhoneError {}

/// The identities of a message sender.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SenderIds {
    /// `+` and the digits.
    pub phone: Option<String>,
    /// `<user>@lid`.
    pub lid: Option<String>,
}

impl SenderIds {
    /// Collects the phone number and LID from the sender's address, the alternate address the
    /// server sent with it (the phone for a LID-addressed message, and the other way round), and
    /// `learned_phone`: the digits the client's LID map holds for the sender's LID, in that order
    /// of preference.
    pub(crate) fn new(
        sender: Option<&Address>,
        alt: Option<&Address>,
        learned_phone: Option<&str>,
    ) -> Self {
        let mut ids = Self::default();
        for address in sender.into_iter().chain(alt) {
            match address.kind {
                AddressKind::Phone if ids.phone.is_none() => {
                    ids.phone = Some(format!("+{}", address.user));
                }
                AddressKind::Lid if ids.lid.is_none() => {
                    ids.lid = Some(format!("{}@lid", address.user));
                }
                _ => {}
            }
        }
        if ids.phone.is_none()
            && let Some(phone) = learned_phone
        {
            ids.phone = Some(format!("+{phone}"));
        }
        ids
    }

    /// Whether the phone number is still unknown and the sender's LID could be looked up.
    pub(crate) fn needs_lookup(sender: Option<&Address>, alt: Option<&Address>) -> Option<String> {
        let has_phone = sender
            .into_iter()
            .chain(alt)
            .any(|a| a.kind == AddressKind::Phone);
        if has_phone {
            return None;
        }
        sender
            .into_iter()
            .chain(alt)
            .find(|a| a.kind == AddressKind::Lid)
            .map(|a| a.user.clone())
    }

    /// The canonical native identifier: the phone number, or the LID when that's all there is.
    pub(crate) fn canonical(&self) -> Option<&str> {
        self.phone.as_deref().or(self.lid.as_deref())
    }

    /// The other ids of the same person: the LID next to a phone number.
    pub(crate) fn aliases(&self) -> Vec<&str> {
        match (&self.phone, &self.lid) {
            (Some(_), Some(lid)) => vec![lid.as_str()],
            _ => Vec::new(),
        }
    }

    /// The JID of the person's direct chat: the phone's when known, so one person is one
    /// conversation however WhatsApp addressed a message.
    pub(crate) fn direct_chat(&self) -> Option<String> {
        if let Some(phone) = &self.phone {
            return Some(format!("{}@s.whatsapp.net", &phone[1..]));
        }
        self.lid.clone()
    }
}

/// A send target given as an identity.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Recipient {
    /// A phone number's digits, to be resolved to a chat.
    Phone(String),
    /// A LID's JID, used as it is.
    Lid(String),
}

/// Parses the identity of a `recipient` send: `+15551234567` (a phone number) or
/// `100000012345678@lid` (a LID).
pub(crate) fn parse_recipient(id: &str) -> Result<Recipient, String> {
    if let Some(user) = id.strip_suffix("@lid") {
        if !user.is_empty() && user.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(Recipient::Lid(id.to_owned()));
        }
        return Err(format!("{id} is not a LID (digits and @lid)"));
    }
    match parse_phone(id) {
        Ok(digits) => Ok(Recipient::Phone(digits)),
        Err(e) => Err(format!(
            "{id} is not a WhatsApp identity (a phone number like +15551234567, or <digits>@lid): {e}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_numbers_normalize_to_digits() {
        for (input, digits) in [
            ("+1 (555) 123-4567", "15551234567"),
            ("15551234567", "15551234567"),
            (" +44.20.7946.0958 ", "442079460958"),
            ("1234567", "1234567"),
            ("+123456789012345", "123456789012345"),
        ] {
            assert_eq!(parse_phone(input).as_deref(), Ok(digits), "{input}");
        }
    }

    #[test]
    fn phone_numbers_are_checked() {
        assert_eq!(parse_phone(""), Err(PhoneError::Empty));
        assert_eq!(parse_phone("+ - "), Err(PhoneError::Empty));
        assert_eq!(parse_phone("+0155512345"), Err(PhoneError::LeadingZero));
        assert_eq!(parse_phone("123456"), Err(PhoneError::Length(6)));
        assert_eq!(parse_phone("1234567890123456"), Err(PhoneError::Length(16)));
        assert_eq!(
            parse_phone("1555abc4567"),
            Err(PhoneError::BadCharacter('a'))
        );
        assert_eq!(parse_phone("+1+555"), Err(PhoneError::BadCharacter('+')));
        assert_eq!(
            parse_phone("100000012345678@lid"),
            Err(PhoneError::BadCharacter('@'))
        );
    }

    #[test]
    fn a_phone_sender_with_a_lid_alias() {
        let ids = SenderIds::new(
            Some(&Address::phone("15557654321")),
            Some(&Address::lid("100000012345678")),
            None,
        );
        assert_eq!(ids.canonical(), Some("+15557654321"));
        assert_eq!(ids.aliases(), ["100000012345678@lid"]);
        assert_eq!(
            ids.direct_chat().as_deref(),
            Some("15557654321@s.whatsapp.net")
        );
    }

    #[test]
    fn a_lid_sender_takes_its_phone_from_the_alternate_address() {
        let ids = SenderIds::new(
            Some(&Address::lid("100000012345678")),
            Some(&Address::phone("15557654321")),
            None,
        );
        assert_eq!(ids.canonical(), Some("+15557654321"));
        assert_eq!(ids.aliases(), ["100000012345678@lid"]);
    }

    #[test]
    fn a_lid_sender_takes_its_phone_from_the_learned_map() {
        let ids = SenderIds::new(
            Some(&Address::lid("100000012345678")),
            None,
            Some("15557654321"),
        );
        assert_eq!(ids.canonical(), Some("+15557654321"));
        assert_eq!(ids.aliases(), ["100000012345678@lid"]);
        // The alternate address wins over the map.
        let ids = SenderIds::new(
            Some(&Address::lid("100000012345678")),
            Some(&Address::phone("15550000000")),
            Some("15557654321"),
        );
        assert_eq!(ids.canonical(), Some("+15550000000"));
    }

    #[test]
    fn a_lid_alone_is_the_identity_when_the_phone_is_unknown() {
        let ids = SenderIds::new(Some(&Address::lid("100000012345678")), None, None);
        assert_eq!(ids.canonical(), Some("100000012345678@lid"));
        assert!(ids.aliases().is_empty());
        assert_eq!(ids.direct_chat().as_deref(), Some("100000012345678@lid"));
    }

    #[test]
    fn a_phone_sender_alone_has_no_aliases() {
        let ids = SenderIds::new(Some(&Address::phone("15557654321")), None, None);
        assert_eq!(ids.canonical(), Some("+15557654321"));
        assert!(ids.aliases().is_empty());
    }

    #[test]
    fn the_lookup_is_needed_only_for_a_lid_without_a_phone() {
        let lid = Address::lid("1000");
        let phone = Address::phone("15557654321");
        assert_eq!(
            SenderIds::needs_lookup(Some(&lid), None).as_deref(),
            Some("1000")
        );
        assert_eq!(SenderIds::needs_lookup(Some(&lid), Some(&phone)), None);
        assert_eq!(SenderIds::needs_lookup(Some(&phone), None), None);
        assert_eq!(SenderIds::needs_lookup(None, None), None);
    }

    #[test]
    fn recipients() {
        assert_eq!(
            parse_recipient("+15551234567"),
            Ok(Recipient::Phone("15551234567".into()))
        );
        assert_eq!(
            parse_recipient("100000012345678@lid"),
            Ok(Recipient::Lid("100000012345678@lid".into()))
        );
        for bad in [
            "",
            "stephen",
            "account:stephen",
            "@lid",
            "abc@lid",
            "+0123",
            "x@s.whatsapp.net",
        ] {
            assert!(parse_recipient(bad).is_err(), "{bad}");
        }
    }
}
