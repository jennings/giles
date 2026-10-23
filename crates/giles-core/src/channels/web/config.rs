//! The web channel's `config` table (`[channel.<instance>] config = { ... }`).

use serde::Deserialize;
use serde_json::Value;

/// The web channel's settings. Unknown keys are an error.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {}

impl WebConfig {
    /// Parses the instance's `config` table (`PluginConfig::to_json`).
    ///
    /// # Errors
    ///
    /// The reason, naming the key at fault.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        serde_path_to_error::deserialize(value).map_err(|e| {
            let path = e.path().to_string();
            if path == "." {
                e.into_inner().to_string()
            } else {
                format!("{path}: {}", e.into_inner())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn empty_table_is_the_default_and_unknown_keys_are_rejected() {
        assert_eq!(WebConfig::from_json(&json!({})), Ok(WebConfig::default()));
        let err = WebConfig::from_json(&json!({"bogus": 1})).unwrap_err();
        assert!(err.contains("unknown field `bogus`"), "{err}");
    }
}
