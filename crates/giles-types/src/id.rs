//! Identifier newtypes.
//!
//! Every id is a validated string: the string ids reject the empty string, and [`MediaId`] accepts
//! only a lowercase hex SHA-256 digest. Validation runs in every constructor and on
//! deserialization, so a value of one of these types always holds a valid id. Ids serialize as
//! plain JSON strings.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A string was rejected as an id.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}: {reason}")]
pub struct IdError {
    kind: &'static str,
    reason: &'static str,
}

impl IdError {
    fn new(kind: &'static str, reason: &'static str) -> Self {
        Self { kind, reason }
    }

    /// The kind of id that was rejected, e.g. `"event id"`.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// Why it was rejected.
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

/// Defines a non-empty string id with its constructors and conversions.
macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            /// Creates the id. Fails if `id` is empty.
            pub fn new(id: impl Into<String>) -> Result<Self, IdError> {
                let id = id.into();
                if id.is_empty() {
                    return Err(IdError::new($kind, "must not be empty"));
                }
                Ok(Self(id))
            }

            /// The id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consumes the id and returns its string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl_string_conversions!($name);
    };
}

/// Implements `Display`, `AsRef<str>`, `FromStr`, and the `String` conversions for an id whose
/// inherent `new(impl Into<String>) -> Result<Self, IdError>` validates.
macro_rules! impl_string_conversions {
    ($name:ident) => {
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, IdError> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(s: String) -> Result<Self, IdError> {
                Self::new(s)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdError;

            fn try_from(s: &str) -> Result<Self, IdError> {
                Self::new(s)
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }
    };
}

string_id!(
    /// A channel event id, unique within a channel instance (PLAN section 8.5). Ids should survive
    /// a restart of the source, e.g. an email `Message-ID` or a WhatsApp message id. The core
    /// deduplicates on (instance, event id) and acks with it.
    EventId,
    "event id"
);

string_id!(
    /// A message id native to a channel: returned by `Channel::send`, and used for `reply_to`.
    MessageId,
    "message id"
);

string_id!(
    /// A native identifier of a sender or recipient on a channel, verified by the channel (PLAN
    /// section 3.3), e.g. `+15551234567`, `me@example.com`, or `account:stephen`. Together with the
    /// channel's identity namespace it forms an identity (section 3.2).
    NativeId,
    "native id"
);

string_id!(
    /// A conversation id native to a channel (PLAN section 10): a web chat id, an IRC channel or
    /// nick, an email thread root, or a WhatsApp chat JID.
    ConversationId,
    "conversation id"
);

string_id!(
    /// A user name: the `<user>` of `users/<user>/user.toml` (PLAN section 3.1).
    UserId,
    "user id"
);

string_id!(
    /// An agent name: the `<agent>` of `agents/<agent>/` (PLAN section 11.3).
    AgentId,
    "agent id"
);

string_id!(
    /// Identifies one approval request within a tool call (PLAN section 6.4).
    ApprovalId,
    "approval id"
);

/// The id of a blob in the core's content-addressed blob store (PLAN section 11.5): the SHA-256 of
/// its bytes as 64 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct MediaId(String);

impl MediaId {
    const KIND: &'static str = "media id";

    /// Creates the id from its hex form. Fails unless `id` is exactly 64 lowercase hex digits.
    pub fn new(id: impl Into<String>) -> Result<Self, IdError> {
        let id = id.into();
        if id.len() != 64 {
            return Err(IdError::new(Self::KIND, "must be 64 hex digits"));
        }
        if !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(IdError::new(Self::KIND, "must be lowercase hex"));
        }
        Ok(Self(id))
    }

    /// Creates the id from a SHA-256 digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut id = String::with_capacity(64);
        for byte in digest {
            id.push(char::from(HEX[usize::from(byte >> 4)]));
            id.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(id)
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the id and returns its string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl_string_conversions!(MediaId);

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST_HEX: &str = "a3f9000000000000000000000000000000000000000000000000000000000001";

    #[test]
    fn string_ids_reject_empty() {
        let err = EventId::new("").unwrap_err();
        assert_eq!(err.kind(), "event id");
        assert_eq!(err.to_string(), "invalid event id: must not be empty");
        assert!(UserId::try_from(String::new()).is_err());
        assert!("".parse::<ConversationId>().is_err());
    }

    #[test]
    fn string_ids_keep_their_value_verbatim() {
        let id = NativeId::new(" account:stephen ").unwrap();
        assert_eq!(id.as_str(), " account:stephen ");
        assert_eq!(id.to_string(), " account:stephen ");
        assert_eq!(String::from(id), " account:stephen ");
    }

    #[test]
    fn string_ids_serialize_as_plain_strings() {
        let id = ConversationId::new("#giles").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), r##""#giles""##);
        let back: ConversationId = serde_json::from_str(r##""#giles""##).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn deserializing_an_empty_id_fails() {
        let err = serde_json::from_str::<MessageId>(r#""""#).unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err}");
    }

    #[test]
    fn media_id_accepts_only_lowercase_sha256_hex() {
        assert_eq!(MediaId::new(DIGEST_HEX).unwrap().as_str(), DIGEST_HEX);
        assert_eq!(
            MediaId::new(&DIGEST_HEX[..63]).unwrap_err().reason(),
            "must be 64 hex digits"
        );
        assert_eq!(
            MediaId::new(format!("{DIGEST_HEX}0")).unwrap_err().reason(),
            "must be 64 hex digits"
        );
        assert_eq!(
            MediaId::new(DIGEST_HEX.to_uppercase())
                .unwrap_err()
                .reason(),
            "must be lowercase hex"
        );
        let non_hex = DIGEST_HEX.replace('a', "g");
        assert!(MediaId::new(non_hex).is_err());
        // 64 bytes, but not 64 ASCII characters.
        assert!(MediaId::new("é".repeat(32)).is_err());
    }

    #[test]
    fn media_id_from_digest_is_lowercase_hex() {
        let mut digest = [0u8; 32];
        digest[0] = 0xa3;
        digest[1] = 0xf9;
        digest[31] = 0x01;
        assert_eq!(MediaId::from_digest(digest).as_str(), DIGEST_HEX);
        let all = MediaId::from_digest([0xff; 32]);
        assert_eq!(all.as_str(), "f".repeat(64));
        assert_eq!(MediaId::new(all.as_str()).unwrap(), all);
    }

    #[test]
    fn media_id_deserialization_validates() {
        assert!(serde_json::from_str::<MediaId>(r#""a3f9""#).is_err());
        let id: MediaId = serde_json::from_str(&format!("\"{DIGEST_HEX}\"")).unwrap();
        assert_eq!(id.as_str(), DIGEST_HEX);
    }
}
