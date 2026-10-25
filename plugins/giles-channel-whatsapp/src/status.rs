//! What the plugin tells the operator about the connection (`ChannelStatus`, PLAN section 8.5).
//!
//! Linking is `NeedsAuth` with the QR code or pair code and when it stops working; a working
//! session is `Connected`; everything else is an `Error` whose message says what to do. Messages
//! never include a phone number or a message's text.

use std::time::Duration;

use giles_plugin_sdk::giles_types::{AuthChallenge, ChannelStatus, Timestamp};

use crate::backend::WaEvent;
use crate::config::Link;

/// The advice that ends most error messages: restarting the plugin (the admin page's **Restart**
/// button, or restarting Giles) starts the client again, which is how a new QR code is requested.
const RESTART: &str = "Restart this plugin to try again.";

/// The status a client event changes the connection to, or `None` if it changes nothing the
/// operator sees. `now` dates the expiry of a QR code or pair code. `Disconnected` is not
/// handled here: the session reports it only if the client doesn't reconnect quickly.
pub(crate) fn for_event(event: &WaEvent, link: &Link, now: Timestamp) -> Option<ChannelStatus> {
    match event {
        // A QR code only counts when linking by QR code: with a pair code configured, the client
        // also offers QR codes, and showing them would replace the pair code.
        WaEvent::PairingQr { code, timeout } if *link == Link::Qr => {
            Some(ChannelStatus::NeedsAuth {
                challenge: AuthChallenge::QrCode {
                    payload: code.clone(),
                },
                expires_at: expiry(now, *timeout),
            })
        }
        WaEvent::PairingCode { code, timeout } if *link != Link::Qr => {
            Some(ChannelStatus::NeedsAuth {
                challenge: AuthChallenge::PairCode { code: code.clone() },
                expires_at: expiry(now, *timeout),
            })
        }
        // Only when the client closed itself, which it does with no pair code outstanding.
        WaEvent::QrExhausted { closed: true } => {
            Some(error(format!("no QR code was scanned in time. {RESTART}")))
        }
        WaEvent::LinkFailed(why) => Some(error(format!("linking failed: {why}. {RESTART}"))),
        WaEvent::Connected => Some(ChannelStatus::Connected),
        WaEvent::LoggedOut(reason) => Some(error(format!(
            "WhatsApp logged this device out ({reason}), for example because it was removed \
             under Linked devices. {RESTART} It will link a new device."
        ))),
        WaEvent::Fatal(why) => Some(error(why.clone())),
        WaEvent::Stopped(reason) => Some(restarting(reason)),
        _ => None,
    }
}

/// The status of a connection that dropped and didn't come back by itself in time.
pub(crate) fn disconnected(reason: &str) -> ChannelStatus {
    error(format!(
        "disconnected from WhatsApp ({reason}); the client is reconnecting"
    ))
}

/// The status of a client that died by itself and will be started again.
pub(crate) fn restarting(reason: &str) -> ChannelStatus {
    error(format!(
        "the WhatsApp client stopped ({reason}); starting it again"
    ))
}

/// The status when the client couldn't be started.
pub(crate) fn failed_to_start(why: &str) -> ChannelStatus {
    error(format!("couldn't start the WhatsApp client: {why}"))
}

fn error(message: String) -> ChannelStatus {
    ChannelStatus::Error { message }
}

/// When something valid for `timeout` from `now` stops being valid.
fn expiry(now: Timestamp, timeout: Duration) -> Option<Timestamp> {
    now.checked_add(timeout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        "2026-10-25T12:00:00Z".parse().unwrap()
    }

    fn qr(code: &str, secs: u64) -> WaEvent {
        WaEvent::PairingQr {
            code: code.into(),
            timeout: Duration::from_secs(secs),
        }
    }

    fn pair_code(code: &str, secs: u64) -> WaEvent {
        WaEvent::PairingCode {
            code: code.into(),
            timeout: Duration::from_secs(secs),
        }
    }

    fn pair_link() -> Link {
        Link::PairCode {
            phone: "15551234567".into(),
        }
    }

    #[test]
    fn a_qr_code_is_a_challenge_that_expires_with_its_timeout() {
        assert_eq!(
            for_event(&qr("2@abc", 60), &Link::Qr, now()),
            Some(ChannelStatus::NeedsAuth {
                challenge: AuthChallenge::QrCode {
                    payload: "2@abc".into()
                },
                expires_at: Some("2026-10-25T12:01:00Z".parse().unwrap()),
            })
        );
    }

    #[test]
    fn a_pair_code_is_a_challenge_that_expires_with_its_timeout() {
        assert_eq!(
            for_event(&pair_code("ABCD1234", 180), &pair_link(), now()),
            Some(ChannelStatus::NeedsAuth {
                challenge: AuthChallenge::PairCode {
                    code: "ABCD1234".into()
                },
                expires_at: Some("2026-10-25T12:03:00Z".parse().unwrap()),
            })
        );
    }

    #[test]
    fn only_the_configured_way_of_linking_is_shown() {
        // The client offers both while a pair code is configured; the pair code is what the
        // operator asked for.
        assert_eq!(for_event(&qr("2@abc", 60), &pair_link(), now()), None);
        assert_eq!(
            for_event(&pair_code("ABCD1234", 180), &Link::Qr, now()),
            None
        );
    }

    #[test]
    fn connected_and_errors() {
        assert_eq!(
            for_event(&WaEvent::Connected, &Link::Qr, now()),
            Some(ChannelStatus::Connected)
        );
        let message = |event: WaEvent| match for_event(&event, &Link::Qr, now()) {
            Some(ChannelStatus::Error { message }) => message,
            other => panic!("not an error: {other:?}"),
        };
        assert!(message(WaEvent::QrExhausted { closed: true }).contains("Restart this plugin"));
        assert!(message(WaEvent::LinkFailed("phone said no".into())).contains("phone said no"));
        assert!(message(WaEvent::LoggedOut("LoggedOut".into())).contains("link a new device"));
        assert_eq!(message(WaEvent::Fatal("banned".into())), "banned");
        assert!(message(WaEvent::Stopped("Stopped".into())).contains("starting it again"));
    }

    #[test]
    fn events_that_change_nothing_report_nothing() {
        for event in [
            WaEvent::Paired,
            WaEvent::Disconnected("reset".into()),
            // A pair code outstanding keeps the socket up, so the QR codes running out is not news.
            WaEvent::QrExhausted { closed: false },
        ] {
            assert_eq!(for_event(&event, &Link::Qr, now()), None);
        }
    }

    #[test]
    fn disconnects_say_the_client_is_reconnecting() {
        match disconnected("connection reset") {
            ChannelStatus::Error { message } => {
                assert!(message.contains("connection reset"), "{message}");
                assert!(message.contains("reconnecting"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
}
