//! The instance config (`[channel.<name>] config`), validated into [`Settings`].

use std::fmt;

use giles_plugin_sdk::config::ByteSize;
use giles_plugin_sdk::giles_types::{Error, Result};
use serde::Deserialize;

use crate::identity::parse_phone;

/// The default for `max_blob`: the core's default limit for an attachment from a plugin.
pub(crate) const DEFAULT_MAX_BLOB: u64 = 50 * 1024 * 1024;
/// The default for `device_name`.
pub(crate) const DEFAULT_DEVICE_NAME: &str = "Giles";
/// The longest `device_name` accepted, in characters.
const MAX_DEVICE_NAME: usize = 64;

/// The config as written. Unknown keys are refused. [`Settings::new`] validates it.
// No `Debug`: `phone_number` must not end up in a log by accident.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// How to link a new device: `qr` (default) or `pair_code`.
    #[serde(default)]
    link: LinkMode,
    /// The phone number of the account to link, with country code. Required for `pair_code`.
    #[serde(default)]
    phone_number: Option<String>,
    /// The name WhatsApp shows for this device under Linked devices. Default `Giles`.
    #[serde(default)]
    device_name: Option<String>,
    /// The largest attachment the plugin downloads or sends. Default `50 MiB`.
    #[serde(default)]
    max_blob: Option<ByteSize>,
}

/// The `link` setting.
#[derive(Debug, Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LinkMode {
    /// Scan a QR code in WhatsApp.
    #[default]
    Qr,
    /// Enter a code in WhatsApp (needs `phone_number`).
    PairCode,
}

/// How a device that isn't linked yet gets linked.
///
/// `Debug` hides the phone number, which identifies the account and has no business in a log.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Link {
    /// Show a QR code.
    Qr,
    /// Request a pair code for this account's number (digits).
    PairCode {
        /// The account's phone number, digits only.
        phone: String,
    },
}

impl Link {
    /// The mode as the `link` setting spells it, for logs.
    pub(crate) fn mode(&self) -> &'static str {
        match self {
            Self::Qr => "qr",
            Self::PairCode { .. } => "pair_code",
        }
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Qr => f.write_str("Qr"),
            Self::PairCode { .. } => f.write_str("PairCode { phone: <redacted> }"),
        }
    }
}

/// The validated config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    /// How to link while no session exists.
    pub link: Link,
    /// The device name shown in WhatsApp.
    pub device_name: String,
    /// The most bytes of one attachment, in either direction.
    pub max_blob: u64,
}

/// An invalid-config error naming `key`.
fn invalid(key: &str, reason: &str) -> Error {
    Error::InvalidRequest(format!("invalid config: {key}: {reason}"))
}

impl Settings {
    /// Validates `config`. Errors are [`Error::InvalidRequest`] naming the key, like
    /// `invalid config: phone_number: ...`.
    pub(crate) fn new(config: Config) -> Result<Self> {
        let link = match (config.link, config.phone_number) {
            (LinkMode::Qr, None) => Link::Qr,
            (LinkMode::Qr, Some(_)) => {
                return Err(invalid(
                    "phone_number",
                    "only used with link = \"pair_code\"; remove it to link with a QR code",
                ));
            }
            (LinkMode::PairCode, None) => {
                return Err(invalid(
                    "phone_number",
                    "required with link = \"pair_code\": the number of the account to link",
                ));
            }
            (LinkMode::PairCode, Some(number)) => Link::PairCode {
                phone: parse_phone(&number).map_err(|e| invalid("phone_number", &e.to_string()))?,
            },
        };
        let device_name = config
            .device_name
            .unwrap_or_else(|| DEFAULT_DEVICE_NAME.to_owned());
        if device_name.trim().is_empty()
            || device_name.chars().any(char::is_control)
            || device_name.chars().count() > MAX_DEVICE_NAME
        {
            return Err(invalid(
                "device_name",
                &format!("must be 1 to {MAX_DEVICE_NAME} characters, without control characters"),
            ));
        }
        let max_blob = config.max_blob.map_or(DEFAULT_MAX_BLOB, |size| size.0);
        if max_blob == 0 {
            return Err(invalid("max_blob", "must be more than 0"));
        }
        Ok(Self {
            link,
            device_name,
            max_blob,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn settings(config: Value) -> Result<Settings> {
        let config: Config = serde_json::from_value(config).map_err(Error::other)?;
        Settings::new(config)
    }

    /// The error message for `config`, which must be invalid.
    fn error(config: Value) -> String {
        match settings(config) {
            Err(Error::InvalidRequest(message)) => message,
            Err(other) => panic!("not InvalidRequest: {other}"),
            Ok(settings) => panic!("accepted: {settings:?}"),
        }
    }

    #[test]
    fn an_empty_config_links_with_a_qr_code() {
        assert_eq!(
            settings(json!({})).unwrap(),
            Settings {
                link: Link::Qr,
                device_name: "Giles".into(),
                max_blob: 50 * 1024 * 1024,
            }
        );
    }

    #[test]
    fn every_key() {
        assert_eq!(
            settings(json!({
                "link": "pair_code",
                "phone_number": "+1 (555) 123-4567",
                "device_name": "Giles at home",
                "max_blob": "8 MiB",
            }))
            .unwrap(),
            Settings {
                link: Link::PairCode {
                    phone: "15551234567".into()
                },
                device_name: "Giles at home".into(),
                max_blob: 8 * 1024 * 1024,
            }
        );
        assert_eq!(settings(json!({ "link": "qr" })).unwrap().link, Link::Qr);
        assert_eq!(
            settings(json!({ "max_blob": 4096 })).unwrap().max_blob,
            4096
        );
    }

    #[test]
    fn invalid_configs_name_the_key() {
        let cases = [
            (
                json!({ "link": "pair_code" }),
                "invalid config: phone_number: required with link = \"pair_code\"",
            ),
            (
                json!({ "phone_number": "+15551234567" }),
                "invalid config: phone_number: only used with link = \"pair_code\"",
            ),
            (
                json!({ "link": "pair_code", "phone_number": "0155512345" }),
                "invalid config: phone_number: starts with 0",
            ),
            (
                json!({ "link": "pair_code", "phone_number": "12345" }),
                "invalid config: phone_number: 5 digits",
            ),
            (
                json!({ "device_name": "" }),
                "invalid config: device_name: must be 1 to 64 characters",
            ),
            (
                json!({ "device_name": "  " }),
                "invalid config: device_name: must be 1 to 64 characters",
            ),
            (
                json!({ "device_name": "a\nb" }),
                "invalid config: device_name: must be 1 to 64 characters",
            ),
            (
                json!({ "device_name": "x".repeat(65) }),
                "invalid config: device_name: must be 1 to 64 characters",
            ),
            (
                json!({ "max_blob": 0 }),
                "invalid config: max_blob: must be more than 0",
            ),
        ];
        for (config, expected) in cases {
            let message = error(config.clone());
            assert!(message.starts_with(expected), "{config}: {message}");
        }
    }

    #[test]
    fn the_phone_number_never_shows_in_debug_output() {
        let settings = settings(json!({
            "link": "pair_code",
            "phone_number": "+1 555 123 4567",
        }))
        .unwrap();
        assert_eq!(
            settings.link,
            Link::PairCode {
                phone: "15551234567".into()
            }
        );
        for shown in [format!("{settings:?}"), format!("{:?}", settings.link)] {
            assert!(!shown.contains("5551234567"), "{shown}");
            assert!(shown.contains("PairCode"), "{shown}");
        }
        assert_eq!(settings.link.mode(), "pair_code");
        assert_eq!(Link::Qr.mode(), "qr");
        assert_eq!(format!("{:?}", Link::Qr), "Qr");
    }

    #[test]
    fn misspelled_and_mistyped_keys_are_refused() {
        for config in [
            json!({ "phone": "+15551234567" }),
            json!({ "link": "sms" }),
            json!({ "link": true }),
            json!({ "max_blob": "lots" }),
            json!({ "max_blob": -1 }),
            json!({ "device_name": 5 }),
        ] {
            assert!(
                serde_json::from_value::<Config>(config.clone()).is_err(),
                "{config}"
            );
        }
    }
}
