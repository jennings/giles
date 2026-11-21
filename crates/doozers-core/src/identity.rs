//! Identities: the channel identity-namespace rule and the owner's user id (PLAN sections 1.1, 3.2,
//! 3.3). Matching senders against users is in [`users`](crate::users). Design:
//! `Docs/technical/identity-and-routing.md`.

use doozers_types::UserId;
use thiserror::Error;

/// The user id of the owner, the user `[owner]` of `doozers.toml` described before there were user
/// records (PLAN section 1.1). Sessions, inbox rows, usage rows, per-user memories
/// (`MemoryOwner::User`), and `users/owner/USER.md` use it, so the imported owner keeps it.
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
/// its component name (`irc` for `doozers-channel-irc`, `web` for the built-in web channel): it
/// must be the component name itself or start with `<component>:`, e.g. `irc:libera.chat`, so a
/// plugin can't claim another channel's identities. Every `:`-separated segment must be non-empty
/// and free of whitespace and control characters. The comparison is exact (case-sensitive).
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
}
