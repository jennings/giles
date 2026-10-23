//! Identities: the channel identity-namespace rule and matching senders against `[owner]` (PLAN
//! sections 1.1, 3.2, 3.3). Design: `Docs/technical/identity-and-routing.md`.

use giles_types::{InboundMessage, NativeId, UserId};
use thiserror::Error;

use crate::config::{Identity, OwnerConfig};

/// The user id of the owner, the one user of v1 (PLAN section 1.1). Sessions, inbox rows, usage
/// rows, and per-user memory (`agents/<agent>/users/owner/`, `users/owner/USER.md`) use it.
pub const OWNER: &str = "owner";

/// [`OWNER`] as a [`UserId`].
pub fn owner() -> UserId {
    UserId::new(OWNER).expect("OWNER is a valid user id")
}

/// Why a channel's reported identity namespace was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum NamespaceError {
    /// The namespace is not `<component>` or `<component>:<more>`, so the channel would claim
    /// another channel's identities.
    #[error(
        "identity namespace {namespace:?} must be {component:?} or start with \"{component}:\""
    )]
    WrongComponent {
        /// The channel's component name.
        component: String,
        /// The reported namespace.
        namespace: String,
    },
    /// The namespace has an empty `:`-separated segment, whitespace, or a control character.
    #[error(
        "identity namespace {namespace:?} is malformed: use non-empty `:`-separated segments \
         without whitespace or control characters"
    )]
    Malformed {
        /// The reported namespace.
        namespace: String,
    },
}

/// Checks the identity namespace a channel reported (`ChannelInfo::identity_namespace`) against
/// its component name (`irc` for `giles-channel-irc`, `web` for the built-in web channel): it must
/// be the component name itself or start with `<component>:`, e.g. `irc:libera.chat`, so a plugin
/// can't claim another channel's identities. Every `:`-separated segment must be non-empty and
/// free of whitespace and control characters. The comparison is exact (case-sensitive).
///
/// Channel registration calls this and refuses the channel on error; the router repeats the check
/// for every event.
///
/// # Errors
///
/// [`NamespaceError`] naming the problem.
pub fn validate_namespace(component: &str, namespace: &str) -> Result<(), NamespaceError> {
    let malformed = namespace
        .split(':')
        .any(|segment| segment.is_empty() || segment.chars().any(char::is_whitespace))
        || namespace.chars().any(char::is_control);
    if malformed {
        return Err(NamespaceError::Malformed {
            namespace: namespace.to_owned(),
        });
    }
    let owned = namespace
        .strip_prefix(component)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'));
    if !owned || component.is_empty() {
        return Err(NamespaceError::WrongComponent {
            component: component.to_owned(),
            namespace: namespace.to_owned(),
        });
    }
    Ok(())
}

/// The owner's identities, for matching inbound senders (PLAN section 3.2).
///
/// An inbound id matches an owner identity when `<namespace>:<native id>` equals it. The
/// comparison is exact (case-sensitive) except in the `email` namespace (first segment `email`),
/// where the domain, after the last `@`, is compared ASCII case-insensitively; the local part
/// stays case-sensitive, as RFC 5321 allows mail servers to treat it so.
#[derive(Debug, Clone)]
pub struct OwnerIdentities {
    identities: Vec<Identity>,
}

impl OwnerIdentities {
    /// The identities of `[owner]`.
    pub fn new(owner: &OwnerConfig) -> Self {
        Self {
            identities: owner.identities.clone(),
        }
    }

    /// Whether `<namespace>:<id>` is one of the owner's identities.
    pub fn contains(&self, namespace: &str, id: &NativeId) -> bool {
        let email = namespace.split(':').next() == Some("email");
        self.identities.iter().any(|identity| {
            identity
                .as_str()
                .strip_prefix(namespace)
                .and_then(|rest| rest.strip_prefix(':'))
                .is_some_and(|configured| native_eq(email, configured, id.as_str()))
        })
    }

    /// Whether the message's sender or any of its aliases is one of the owner's identities.
    pub fn is_owner(&self, namespace: &str, message: &InboundMessage) -> bool {
        message.sender_ids().any(|id| self.contains(namespace, id))
    }
}

/// Compares two native ids of one namespace.
fn native_eq(email: bool, a: &str, b: &str) -> bool {
    if email
        && let (Some((a_local, a_domain)), Some((b_local, b_domain))) =
            (a.rsplit_once('@'), b.rsplit_once('@'))
    {
        return a_local == b_local && a_domain.eq_ignore_ascii_case(b_domain);
    }
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_user_id() {
        assert_eq!(owner().as_str(), "owner");
    }

    #[test]
    fn namespaces() {
        for (component, namespace) in [
            ("irc", "irc"),
            ("irc", "irc:libera.chat"),
            ("irc", "irc:libera.chat:extra"),
            ("web", "web"),
            ("whatsapp", "whatsapp"),
            ("email", "email"),
            ("my-chan", "my-chan:x"),
        ] {
            assert_eq!(
                validate_namespace(component, namespace),
                Ok(()),
                "{namespace}"
            );
        }
        for (component, namespace) in [
            ("irc", "ircx"),
            ("irc", "ircx:libera.chat"),
            ("irc", "whatsapp"),
            ("irc", "IRC"),
            ("irc", "IRC:libera.chat"),
            ("irc", "x:irc"),
            ("web", "we"),
            ("", "irc"),
        ] {
            assert!(
                matches!(
                    validate_namespace(component, namespace),
                    Err(NamespaceError::WrongComponent { .. })
                ),
                "{component} {namespace}"
            );
        }
        for namespace in [
            "",
            "irc:",
            ":irc",
            "irc::x",
            "irc: x",
            "irc:a b",
            "irc:\u{7}",
            "irc:\n",
        ] {
            assert!(
                matches!(
                    validate_namespace("irc", namespace),
                    Err(NamespaceError::Malformed { .. })
                ),
                "{namespace:?}"
            );
        }
    }

    #[test]
    fn namespace_error_messages() {
        let err = validate_namespace("irc", "whatsapp").unwrap_err();
        assert_eq!(
            err.to_string(),
            "identity namespace \"whatsapp\" must be \"irc\" or start with \"irc:\""
        );
    }

    fn owner_ids(ids: &[&str]) -> OwnerIdentities {
        OwnerIdentities {
            identities: ids.iter().map(|id| Identity::new(*id).unwrap()).collect(),
        }
    }

    fn native(id: &str) -> NativeId {
        NativeId::new(id).unwrap()
    }

    #[test]
    fn matching() {
        let owner = owner_ids(&[
            "whatsapp:+15551234567",
            "irc:libera.chat:account:stephen",
            "email:Me@Example.com",
            "web:owner",
        ]);
        let yes = [
            ("whatsapp", "+15551234567"),
            ("irc:libera.chat", "account:stephen"),
            ("web", "owner"),
            ("email", "Me@Example.com"),
            ("email", "Me@example.COM"),
        ];
        for (namespace, id) in yes {
            assert!(owner.contains(namespace, &native(id)), "{namespace}:{id}");
        }
        let no = [
            // Another namespace or a near miss on the namespace boundary.
            ("whatsapp:x", "+15551234567"),
            ("whats", "app:+15551234567"),
            ("irc:oftc", "account:stephen"),
            ("irc:libera.chat", "account:Stephen"),
            ("irc:libera.chat", "account:stephe"),
            ("irc:libera.chat", "account:stephenx"),
            ("WEB", "owner"),
            ("web", "Owner"),
            // Email local parts stay case-sensitive.
            ("email", "me@example.com"),
            ("email", "Me@Example.org"),
            ("email", "Me"),
            // The domain rule is only for the email namespace.
            ("whatsapp", "+15551234567@S"),
        ];
        for (namespace, id) in no {
            assert!(!owner.contains(namespace, &native(id)), "{namespace}:{id}");
        }
    }

    #[test]
    fn email_domain_rule_is_scoped_to_the_email_namespace() {
        let owner = owner_ids(&["xmpp:me@Example.com", "email:corp:me@Example.com"]);
        assert!(!owner.contains("xmpp", &native("me@example.com")));
        assert!(owner.contains("xmpp", &native("me@Example.com")));
        assert!(owner.contains("email:corp", &native("me@example.COM")));
    }

    #[test]
    fn email_without_at_sign_is_exact() {
        let owner = owner_ids(&["email:postmaster"]);
        assert!(owner.contains("email", &native("postmaster")));
        assert!(!owner.contains("email", &native("Postmaster")));
    }
}
