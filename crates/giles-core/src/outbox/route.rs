//! Which channel instance sends to an identity, and when to try again. Pure: the sender gathers
//! the configured instances and their registered channels first.

use std::time::Duration;

use giles_types::ChannelInfo;

use crate::config::InstanceName;

/// The delay after a row's first failed attempt; it doubles with each further failure.
pub const BACKOFF_INITIAL: Duration = Duration::from_secs(1);

/// The longest delay between attempts without a `retry_after` hint. Short, because a channel
/// that isn't running yet (a plugin still starting) gives no signal when it comes up.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// The longest `retry_after` a channel's [`Unavailable`](giles_types::Error::Unavailable) can
/// ask for; longer hints are cut to this.
pub const RETRY_AFTER_MAX: Duration = Duration::from_secs(60 * 60);

/// A configured channel instance, with its channel's capabilities if one is registered.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Slot<'a> {
    pub instance: &'a InstanceName,
    pub component: &'a str,
    pub info: Option<&'a ChannelInfo>,
}

/// Where an identity's sends go now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route<'a> {
    /// Through `instance`'s channel, to `recipient` (the identity without its namespace).
    Send {
        instance: &'a InstanceName,
        recipient: &'a str,
    },
    /// The identity belongs to `instance`, whose channel can't start conversations.
    CannotInitiate { instance: &'a InstanceName },
    /// The identity can only belong to one of these instances (never empty), none of which has
    /// a channel registered yet; wait for it.
    Wait { instances: Vec<&'a InstanceName> },
    /// No configured channel instance can own the identity.
    Unowned,
}

/// Routes `identity` (`<namespace>:<native id>`) over `slots`, which come sorted by instance
/// name:
///
/// - Registered channels whose namespace is a prefix of the identity, followed by `:` and a
///   non-empty native id, own it; the longest namespace wins, then a channel that can initiate,
///   then the first by name.
/// - Otherwise the instances with no channel registered whose component is the identity's first
///   segment may own it (a channel's namespace is its component or starts with `<component>:`),
///   so the send waits for them.
pub(crate) fn route<'a>(identity: &'a str, slots: &[Slot<'a>]) -> Route<'a> {
    let mut best: Option<(&Slot<'a>, &ChannelInfo, &'a str)> = None;
    for slot in slots {
        let Some(info) = slot.info else { continue };
        let Some(recipient) = native_id(identity, &info.identity_namespace) else {
            continue;
        };
        let better = best.is_none_or(|(_, best, _)| {
            let (len, best_len) = (info.identity_namespace.len(), best.identity_namespace.len());
            len > best_len || (len == best_len && info.can_initiate && !best.can_initiate)
        });
        if better {
            best = Some((slot, info, recipient));
        }
    }
    if let Some((slot, info, recipient)) = best {
        return if info.can_initiate {
            Route::Send {
                instance: slot.instance,
                recipient,
            }
        } else {
            Route::CannotInitiate {
                instance: slot.instance,
            }
        };
    }
    let component = identity.split(':').next().unwrap_or(identity);
    let instances: Vec<_> = slots
        .iter()
        .filter(|slot| slot.info.is_none() && slot.component == component)
        .map(|slot| slot.instance)
        .collect();
    if instances.is_empty() {
        Route::Unowned
    } else {
        Route::Wait { instances }
    }
}

/// The native id in `identity` if it's in `namespace`.
fn native_id<'a>(identity: &'a str, namespace: &str) -> Option<&'a str> {
    identity
        .strip_prefix(namespace)?
        .strip_prefix(':')
        .filter(|id| !id.is_empty())
}

/// How long to wait after a failed attempt, given the row's earlier failures: the channel's
/// `retry_after` if it gave one (between [`BACKOFF_INITIAL`] and [`RETRY_AFTER_MAX`]), else
/// [`BACKOFF_INITIAL`] doubled per earlier failure, at most [`BACKOFF_MAX`].
pub(crate) fn retry_delay(earlier_failures: u32, retry_after: Option<Duration>) -> Duration {
    match retry_after {
        Some(hint) => hint.clamp(BACKOFF_INITIAL, RETRY_AFTER_MAX),
        None => BACKOFF_INITIAL
            .saturating_mul(2u32.saturating_pow(earlier_failures))
            .min(BACKOFF_MAX),
    }
}

#[cfg(test)]
mod tests {
    use giles_types::OutputMode;

    use super::*;

    fn info(namespace: &str, can_initiate: bool) -> ChannelInfo {
        ChannelInfo {
            identity_namespace: namespace.to_owned(),
            output: OutputMode::Blocks,
            can_initiate,
        }
    }

    fn name(name: &str) -> InstanceName {
        InstanceName::new(name).unwrap()
    }

    /// `(instance, component, registered channel)` triples, sorted by instance name.
    fn routed<'a>(
        identity: &'a str,
        slots: &'a [(InstanceName, &'a str, Option<ChannelInfo>)],
    ) -> Route<'a> {
        let slots: Vec<_> = slots
            .iter()
            .map(|(instance, component, info)| Slot {
                instance,
                component,
                info: info.as_ref(),
            })
            .collect();
        route(identity, &slots)
    }

    #[test]
    fn a_registered_namespace_owns_its_identities() {
        let slots = [(name("phone"), "whatsapp", Some(info("whatsapp", true)))];
        assert_eq!(
            routed("whatsapp:+15551234567", &slots),
            Route::Send {
                instance: &slots[0].0,
                recipient: "+15551234567",
            }
        );
    }

    #[test]
    fn the_namespace_must_end_at_a_colon_and_leave_a_native_id() {
        let slots = [(name("phone"), "whatsapp", Some(info("whatsapp", true)))];
        assert_eq!(routed("whatsappx:+1555", &slots), Route::Unowned);
        assert_eq!(routed("whatsapp:", &slots), Route::Unowned);
        assert_eq!(routed("email:me@example.com", &slots), Route::Unowned);
    }

    #[test]
    fn namespaces_with_colons_route_to_their_instance() {
        let slots = [
            (name("libera"), "irc", Some(info("irc:libera.chat", true))),
            (name("oftc"), "irc", Some(info("irc:oftc", true))),
        ];
        assert_eq!(
            routed("irc:oftc:account:stephen", &slots),
            Route::Send {
                instance: &slots[1].0,
                recipient: "account:stephen",
            }
        );
    }

    #[test]
    fn the_longest_namespace_wins() {
        let slots = [
            (name("a"), "irc", Some(info("irc", true))),
            (name("b"), "irc", Some(info("irc:libera.chat", true))),
        ];
        assert_eq!(
            routed("irc:libera.chat:account:x", &slots),
            Route::Send {
                instance: &slots[1].0,
                recipient: "account:x",
            }
        );
    }

    #[test]
    fn ties_prefer_a_channel_that_can_initiate_then_the_first_name() {
        let slots = [
            (name("a"), "irc", Some(info("irc:x", false))),
            (name("b"), "irc", Some(info("irc:x", true))),
            (name("c"), "irc", Some(info("irc:x", true))),
        ];
        assert_eq!(
            routed("irc:x:account:y", &slots),
            Route::Send {
                instance: &slots[1].0,
                recipient: "account:y",
            }
        );
        let slots = [
            (name("a"), "irc", Some(info("irc:x", false))),
            (name("b"), "irc", Some(info("irc:x", false))),
        ];
        assert_eq!(
            routed("irc:x:account:y", &slots),
            Route::CannotInitiate {
                instance: &slots[0].0
            }
        );
    }

    #[test]
    fn an_owner_that_cannot_initiate_is_reported() {
        let slots = [(name("web"), "web", Some(info("web", false)))];
        assert_eq!(
            routed("web:owner", &slots),
            Route::CannotInitiate {
                instance: &slots[0].0
            }
        );
    }

    #[test]
    fn unregistered_instances_of_the_component_are_waited_for() {
        let slots = [
            (name("libera"), "irc", None),
            (name("oftc"), "irc", Some(info("irc:oftc", true))),
            (name("phone"), "whatsapp", None),
        ];
        // `oftc` is up with another namespace, so only `libera` can own it.
        assert_eq!(
            routed("irc:libera.chat:account:x", &slots),
            Route::Wait {
                instances: vec![&slots[0].0]
            }
        );
        assert_eq!(
            routed("whatsapp:+1555", &slots),
            Route::Wait {
                instances: vec![&slots[2].0]
            }
        );
        let slots = [(name("libera"), "irc", None), (name("oftc"), "irc", None)];
        assert_eq!(
            routed("irc:oftc:account:x", &slots),
            Route::Wait {
                instances: vec![&slots[0].0, &slots[1].0]
            }
        );
    }

    #[test]
    fn no_candidate_means_unowned() {
        let slots = [(name("libera"), "irc", Some(info("irc:libera.chat", true)))];
        assert_eq!(routed("irc:oftc:account:x", &slots), Route::Unowned);
        assert_eq!(routed("email:me@example.com", &[]), Route::Unowned);
    }

    #[test]
    fn backoff_doubles_from_one_second_up_to_a_minute() {
        let delays: Vec<u64> = (0..9).map(|n| retry_delay(n, None).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(retry_delay(u32::MAX, None), BACKOFF_MAX);
    }

    #[test]
    fn retry_after_replaces_the_backoff_within_bounds() {
        let secs = Duration::from_secs;
        assert_eq!(retry_delay(5, Some(secs(3))), secs(3));
        assert_eq!(retry_delay(0, Some(secs(300))), secs(300));
        assert_eq!(retry_delay(0, Some(Duration::ZERO)), BACKOFF_INITIAL);
        assert_eq!(retry_delay(0, Some(secs(86_400))), RETRY_AFTER_MAX);
    }
}
