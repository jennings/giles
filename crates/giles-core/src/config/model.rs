//! Model references: `<provider instance>/<model id>` (PLAN section 6.10).

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;

use super::names::InstanceName;

/// A model on a provider instance, written `<provider instance>/<model id>`. The string is split
/// at the first `/`, so the model id may itself contain `/`
/// (`openrouter/anthropic/claude-sonnet-4.5`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct ModelRef {
    /// The provider instance, a key of `[provider.*]`.
    pub provider: InstanceName,
    /// The model id, passed to the provider unchanged.
    pub model: String,
}

impl FromStr for ModelRef {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = |why: &str| {
            format!("invalid model {text:?}: {why}; write <provider instance>/<model id>")
        };
        let (provider, model) = text.split_once('/').ok_or_else(|| invalid("no `/`"))?;
        let provider = InstanceName::new(provider)
            .map_err(|_| invalid("the provider instance name is not valid"))?;
        if model.is_empty() {
            return Err(invalid("the model id is empty"));
        }
        Ok(Self {
            provider,
            model: model.to_owned(),
        })
    }
}

impl TryFrom<String> for ModelRef {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_model_refs_split_at_the_first_slash() {
        let cases = [
            (
                "anthropic/claude-sonnet-4-5",
                "anthropic",
                "claude-sonnet-4-5",
            ),
            (
                "openrouter/anthropic/claude-sonnet-4.5",
                "openrouter",
                "anthropic/claude-sonnet-4.5",
            ),
            ("ollama/qwen3:8b", "ollama", "qwen3:8b"),
            ("a/ ", "a", " "),
        ];
        for (text, provider, model) in cases {
            let parsed: ModelRef = text.parse().unwrap();
            assert_eq!(parsed.provider.as_str(), provider, "{text}");
            assert_eq!(parsed.model, model, "{text}");
            assert_eq!(parsed.to_string(), text);
        }
    }

    #[test]
    fn invalid_model_refs() {
        let cases = [
            ("claude", "no `/`"),
            ("", "no `/`"),
            ("/claude", "provider instance name"),
            ("open_router/x", "provider instance name"),
            ("anthropic/", "model id is empty"),
        ];
        for (text, reason) in cases {
            let err = text.parse::<ModelRef>().unwrap_err();
            assert!(err.contains(reason), "{text:?}: {err}");
            assert!(err.contains("<provider instance>/<model id>"), "{err}");
        }
    }
}
