//! The instance config (`[channel.discord.<name>] config`), validated into [`Settings`].

use std::fmt;
use std::time::Duration;

use giles_plugin_sdk::config::{ByteSize, HumanDuration};
use giles_plugin_sdk::giles_types::{Error, Result};
use serde::Deserialize;

/// The default for `max_blob`: the core's default limit for an attachment from a plugin.
pub const DEFAULT_MAX_BLOB: u64 = 50 * 1024 * 1024;
/// The default for `catch_up_max_age`.
pub const DEFAULT_CATCH_UP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The largest `catch_up_max_age`: the window in which the core deduplicates inbound events by id,
/// so a message fetched again after a restart is dropped there.
pub const MAX_CATCH_UP_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A credential: never shown by `Debug`, and there is no `Display`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The value, for the one place that hands it to Discord.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The config as written. Unknown keys are refused. [`Settings::new`] validates it.
// Debug is `Secret`'s redaction, so the token can't be printed by accident.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The bot token (`{ secret = "..." }` in practice). Required.
    #[serde(default)]
    token: Option<Secret>,
    /// The largest attachment the plugin downloads or sends. Default `50 MiB`.
    #[serde(default)]
    max_blob: Option<ByteSize>,
    /// How far back catch-up after downtime fetches. Default `24h`; `0s` turns it off.
    #[serde(default)]
    catch_up_max_age: Option<HumanDuration>,
}

/// The validated config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The bot token.
    pub token: Secret,
    /// The most bytes of one attachment, in either direction.
    pub max_blob: u64,
    /// The oldest message catch-up fetches, as an age. Zero turns catch-up off.
    pub catch_up_max_age: Duration,
}

/// An invalid-config error naming `key`.
fn invalid(key: &str, reason: &str) -> Error {
    Error::InvalidRequest(format!("invalid config: {key}: {reason}"))
}

impl Settings {
    /// Validates `config`. Errors are [`Error::InvalidRequest`] naming the key, like
    /// `invalid config: token: ...`, and never contain the token.
    ///
    /// # Errors
    ///
    /// A missing or malformed token, a zero `max_blob`, or a `catch_up_max_age` over 30 days.
    pub fn new(config: Config) -> Result<Self> {
        let token = config.token.ok_or_else(|| {
            invalid(
                "token",
                "required: the bot token from the Bot page of the application in the Discord \
                 Developer Portal",
            )
        })?;
        validate_token(token.expose())?;
        let max_blob = config.max_blob.map_or(DEFAULT_MAX_BLOB, |size| size.0);
        if max_blob == 0 {
            return Err(invalid("max_blob", "must be more than 0"));
        }
        let catch_up_max_age = config
            .catch_up_max_age
            .map_or(DEFAULT_CATCH_UP_MAX_AGE, |age| age.0);
        if catch_up_max_age > MAX_CATCH_UP_MAX_AGE {
            return Err(invalid(
                "catch_up_max_age",
                "must be at most \"30d\", the time the core remembers an inbound message",
            ));
        }
        Ok(Self {
            token,
            max_blob,
            catch_up_max_age,
        })
    }
}

/// Checks the shape of a bot token without saying what it is.
fn validate_token(token: &str) -> Result<()> {
    if token.is_empty() {
        return Err(invalid("token", "must not be empty"));
    }
    // Discord's own examples write `Authorization: Bot <token>`, so the prefix gets pasted. It
    // has a space, so it would fail the next check, but this message says what to do.
    // `get`, not a slice: byte 4 may be inside a multibyte character.
    if token.len() > 4
        && token
            .get(..4)
            .is_some_and(|p| p.eq_ignore_ascii_case("bot "))
    {
        return Err(invalid(
            "token",
            "paste the token alone, without the `Bot` prefix",
        ));
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(invalid(
            "token",
            "must be the token alone: printable ASCII, with no spaces or line breaks",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// A token-shaped string for tests. Not a real credential.
    const TOKEN: &str = "MTIzNDU2.Gabcde.not-a-real-token-1234567890";

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
    fn a_token_alone_is_enough() {
        assert_eq!(
            settings(json!({ "token": TOKEN })).unwrap(),
            Settings {
                token: Secret(TOKEN.into()),
                max_blob: 50 * 1024 * 1024,
                catch_up_max_age: Duration::from_secs(24 * 3600),
            }
        );
    }

    #[test]
    fn every_key() {
        assert_eq!(
            settings(json!({
                "token": TOKEN,
                "max_blob": "8 MiB",
                "catch_up_max_age": "2h30m",
            }))
            .unwrap(),
            Settings {
                token: Secret(TOKEN.into()),
                max_blob: 8 * 1024 * 1024,
                catch_up_max_age: Duration::from_secs(9000),
            }
        );
        assert_eq!(
            settings(json!({ "token": TOKEN, "max_blob": 4096 }))
                .unwrap()
                .max_blob,
            4096
        );
    }

    #[test]
    fn catch_up_can_be_turned_off_and_reaches_thirty_days() {
        let age = |text: &str| {
            settings(json!({ "token": TOKEN, "catch_up_max_age": text }))
                .unwrap()
                .catch_up_max_age
        };
        assert_eq!(age("0s"), Duration::ZERO);
        assert_eq!(age("30d"), MAX_CATCH_UP_MAX_AGE);
        assert_eq!(age("720h"), MAX_CATCH_UP_MAX_AGE);
    }

    #[test]
    fn invalid_configs_name_the_key() {
        let cases = [
            (json!({}), "invalid config: token: required"),
            (
                json!({ "token": "" }),
                "invalid config: token: must not be empty",
            ),
            (
                json!({ "token": format!("Bot {TOKEN}") }),
                "invalid config: token: paste the token alone, without the `Bot` prefix",
            ),
            (
                json!({ "token": format!("bot {TOKEN}") }),
                "invalid config: token: paste the token alone",
            ),
            (
                json!({ "token": format!("{TOKEN}\n") }),
                "invalid config: token: must be the token alone",
            ),
            (
                json!({ "token": format!("{TOKEN} ") }),
                "invalid config: token: must be the token alone",
            ),
            (
                json!({ "token": "tökén" }),
                "invalid config: token: must be the token alone",
            ),
            // Byte 4 falls inside a multibyte character: the prefix check must not slice there.
            (
                json!({ "token": "abcédef" }),
                "invalid config: token: must be the token alone",
            ),
            (
                json!({ "token": "ab€def" }),
                "invalid config: token: must be the token alone",
            ),
            (
                json!({ "token": TOKEN, "max_blob": 0 }),
                "invalid config: max_blob: must be more than 0",
            ),
            (
                json!({ "token": TOKEN, "catch_up_max_age": "30d1s" }),
                "invalid config: catch_up_max_age: must be at most \"30d\"",
            ),
            (
                json!({ "token": TOKEN, "catch_up_max_age": "31d" }),
                "invalid config: catch_up_max_age: must be at most \"30d\"",
            ),
        ];
        for (config, expected) in cases {
            let message = error(config.clone());
            assert!(message.starts_with(expected), "{config}: {message}");
            // Whatever is wrong, the message never repeats the token.
            assert!(!message.contains("not-a-real-token"), "{message}");
        }
    }

    #[test]
    fn misspelled_and_mistyped_keys_are_refused() {
        for config in [
            json!({ "token": TOKEN, "tokn": "x" }),
            json!({ "token": TOKEN, "max_blobs": "1 MiB" }),
            json!({ "token": 5 }),
            json!({ "token": TOKEN, "max_blob": "lots" }),
            json!({ "token": TOKEN, "max_blob": -1 }),
            json!({ "token": TOKEN, "catch_up_max_age": 60 }),
            json!({ "token": TOKEN, "catch_up_max_age": "soon" }),
        ] {
            assert!(
                serde_json::from_value::<Config>(config.clone()).is_err(),
                "{config}"
            );
        }
    }

    #[test]
    fn the_token_never_shows_in_debug_output() {
        let config: Config = serde_json::from_value(json!({ "token": TOKEN })).unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");

        let settings = Settings::new(config).unwrap();
        for shown in [
            format!("{settings:?}"),
            format!("{settings:#?}"),
            format!("{:?}", settings.token),
            format!("{:?}", Some(settings.clone())),
        ] {
            assert!(!shown.contains("not-a-real-token"), "{shown}");
            assert!(shown.contains("<redacted>"), "{shown}");
        }
        assert_eq!(settings.token.expose(), TOKEN);
    }
}
