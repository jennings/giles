//! What the plugin tells the operator about the connection (`ChannelStatus`, PLAN section 8.5).
//!
//! A rejected token is `NeedsAuth` with the Developer Portal's URL; a working session is
//! `Connected`; everything else is an `Error` whose message says what is wrong. Messages never
//! include the token, a user id, or a message's text.

use giles_plugin_sdk::giles_types::{AuthChallenge, ChannelStatus};

use crate::client::GatewayEvent;

/// The Developer Portal page that lists the operator's applications. Its Bot page has "Reset
/// Token". `NeedsAuth` carries only the URL; what to do there is in the user guide.
pub const DEVELOPER_PORTAL: &str = "https://discord.com/developers/applications";

/// Close code 4004: Discord rejected the token (also what a token reset after too many Identifies
/// looks like).
const AUTHENTICATION_FAILED: u16 = 4004;

/// What a Gateway event means for the connection.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// A session is up: `Connected`.
    Up,
    /// The connection dropped and the client will try again. Nothing is reported unless it stays
    /// down past the grace period.
    Down,
    /// The connection is over for good until the plugin is restarted: report this status and stop
    /// reconnecting.
    Final(ChannelStatus),
}

/// What `event` means for the connection.
pub fn classify(event: &GatewayEvent) -> Outcome {
    match event {
        GatewayEvent::Ready { .. } | GatewayEvent::Resumed => Outcome::Up,
        GatewayEvent::Closed { code } => match code {
            Some(code) => close(*code),
            None => Outcome::Down,
        },
    }
}

/// The meaning of a close code. Discord's list of the codes a client must not reconnect after
/// (4004, 4010 to 4014) is the list `twilight-gateway` ends its stream on, so a `Final` here is
/// also the client's last event.
fn close(code: u16) -> Outcome {
    let error = |message: String| Outcome::Final(ChannelStatus::Error { message });
    match code {
        AUTHENTICATION_FAILED => Outcome::Final(rejected_token()),
        // With only the DIRECT_MESSAGES intent, which needs no portal toggle, neither can happen
        // unless Discord changes its rules, so this is not something the operator can fix.
        4013 | 4014 => error(format!(
            "Discord refused the intents the plugin requests (close {code}); this is a bug or a \
             change at Discord, please report it"
        )),
        4010 => error(
            "Discord refused the shard the plugin asked for (close 4010); this is a bug, please \
             report it"
                .to_owned(),
        ),
        4011 => error(
            "Discord requires sharding (close 4011): the bot is in too many servers to run \
             unsharded, which this plugin doesn't support"
                .to_owned(),
        ),
        4012 => error(
            "Discord doesn't support the API version the plugin uses (close 4012); this is a bug \
             or a change at Discord, please report it"
                .to_owned(),
        ),
        // 4000 to 4003, 4005, 4007 to 4009, 1000, 1001, 1006, and anything else: the client
        // resumes or identifies again.
        _ => Outcome::Down,
    }
}

/// The status of a rejected bot token: close 4004 here, a 401 from the REST API elsewhere.
pub fn rejected_token() -> ChannelStatus {
    ChannelStatus::NeedsAuth {
        challenge: AuthChallenge::Url {
            url: DEVELOPER_PORTAL.to_owned(),
        },
        expires_at: None,
    }
}

/// The status of a connection that dropped and didn't come back in time. The client is trying.
pub fn disconnected() -> ChannelStatus {
    ChannelStatus::Error {
        message: "disconnected from Discord, reconnecting".to_owned(),
    }
}

/// The status of a client whose event stream ended without a close the plugin classes as final.
/// Nothing in the plugin starts another one, so it stays until the plugin is restarted.
pub fn stopped() -> ChannelStatus {
    ChannelStatus::Error {
        message: "the connection to Discord ended and won't be retried; restart this plugin to \
                  connect again"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(code: Option<u16>) -> GatewayEvent {
        GatewayEvent::Closed { code }
    }

    fn error_message(outcome: Outcome) -> String {
        match outcome {
            Outcome::Final(ChannelStatus::Error { message }) => message,
            other => panic!("not a final error: {other:?}"),
        }
    }

    #[test]
    fn a_session_is_connected_whether_it_is_new_or_resumed() {
        assert_eq!(classify(&GatewayEvent::Ready { user_id: 1 }), Outcome::Up);
        assert_eq!(classify(&GatewayEvent::Resumed), Outcome::Up);
    }

    #[test]
    fn a_rejected_token_needs_auth_with_the_developer_portal() {
        assert_eq!(
            classify(&closed(Some(4004))),
            Outcome::Final(ChannelStatus::NeedsAuth {
                challenge: AuthChallenge::Url {
                    url: "https://discord.com/developers/applications".into()
                },
                expires_at: None,
            })
        );
        assert_eq!(
            rejected_token(),
            match classify(&closed(Some(4004))) {
                Outcome::Final(status) => status,
                other => panic!("{other:?}"),
            }
        );
    }

    #[test]
    fn closes_the_client_never_retries_are_final_errors() {
        for code in [4010, 4011, 4012, 4013, 4014] {
            let message = error_message(classify(&closed(Some(code))));
            assert!(message.contains(&code.to_string()), "{code}: {message}");
        }
        assert!(
            error_message(classify(&closed(Some(4014)))).contains("please report it"),
            "an intents refusal isn't something the operator can fix"
        );
        assert!(error_message(classify(&closed(Some(4011)))).contains("too many servers"));
    }

    #[test]
    fn every_other_close_is_a_drop_the_client_recovers_from() {
        for code in [
            Some(1000),
            Some(1001),
            Some(1006),
            Some(4000),
            Some(4001),
            Some(4002),
            Some(4003),
            Some(4005),
            Some(4007),
            Some(4008),
            Some(4009),
            Some(4999),
            None,
        ] {
            assert_eq!(classify(&closed(code)), Outcome::Down, "{code:?}");
        }
    }

    #[test]
    fn error_statuses_say_what_happened() {
        match disconnected() {
            ChannelStatus::Error { message } => {
                assert_eq!(message, "disconnected from Discord, reconnecting");
            }
            other => panic!("{other:?}"),
        }
        match stopped() {
            ChannelStatus::Error { message } => assert!(message.contains("restart"), "{message}"),
            other => panic!("{other:?}"),
        }
    }
}
