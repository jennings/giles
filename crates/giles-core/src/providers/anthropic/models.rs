//! What the provider knows about Claude models without asking the API, and the `/v1/models`
//! response.

use giles_types::ModelInfo;
use serde::Deserialize;

/// Media types every current Claude model accepts as input blocks: images and PDFs.
pub const MEDIA_TYPES: [&str; 5] = [
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "application/pdf",
];

/// How a model family is asked to reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingMode {
    /// `thinking: {type: "enabled", budget_tokens}`: Claude 4.5 and earlier.
    Budget,
    /// `thinking: {type: "adaptive"}`: Claude 4.6 and later, which reject (4.7+) or deprecate
    /// (4.6) budgets.
    Adaptive,
}

/// Limits and request rules of a model family, from Anthropic's model overview and thinking
/// pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Family {
    /// Model id prefix: the id itself or the id followed by `-<suffix>` (a date snapshot or a
    /// minor version).
    pub prefix: &'static str,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub thinking: ThinkingMode,
    /// Accepts `thinking: {type: "disabled"}`, so a request without a reasoning budget can turn
    /// thinking off. Opus 5.5, Sonnet 5.5, Fable, and Mythos reject it: thinking is always on.
    pub can_disable_thinking: bool,
    /// Accepts a non-default `temperature`. Claude 4.7 and later reject one on every API
    /// version.
    pub sampling_params: bool,
}

/// The 1M-context, adaptive-thinking generation (Claude 4.6 and later); rows override fields.
const CURRENT: Family = Family {
    prefix: "",
    context_window: 1_000_000,
    max_output_tokens: 128_000,
    thinking: ThinkingMode::Adaptive,
    can_disable_thinking: false,
    sampling_params: false,
};

/// The Claude 4.5 generation: 200K context, thinking budgets, sampling parameters.
const CLAUDE_4_5: Family = Family {
    prefix: "",
    context_window: 200_000,
    max_output_tokens: 64_000,
    thinking: ThinkingMode::Budget,
    can_disable_thinking: true,
    sampling_params: true,
};

/// Known families, for models the Models API reports without limits and for choosing request
/// fields. More specific prefixes come first (`claude-opus-5-5` before `claude-opus-5`). Models
/// not listed are treated as newer than the table: [`FALLBACK_MAX_TOKENS`], adaptive thinking
/// that can't be turned off, and no sampling parameters.
const FAMILIES: &[Family] = &[
    Family {
        prefix: "claude-fable-5",
        ..CURRENT
    },
    Family {
        prefix: "claude-mythos-5",
        ..CURRENT
    },
    Family {
        prefix: "claude-opus-5-5",
        ..CURRENT
    },
    Family {
        prefix: "claude-opus-5",
        can_disable_thinking: true,
        ..CURRENT
    },
    Family {
        prefix: "claude-sonnet-5-5",
        ..CURRENT
    },
    Family {
        prefix: "claude-sonnet-5",
        can_disable_thinking: true,
        ..CURRENT
    },
    Family {
        prefix: "claude-haiku-5-5",
        can_disable_thinking: true,
        ..CURRENT
    },
    Family {
        prefix: "claude-opus-4-8",
        can_disable_thinking: true,
        ..CURRENT
    },
    Family {
        prefix: "claude-opus-4-7",
        can_disable_thinking: true,
        ..CURRENT
    },
    Family {
        prefix: "claude-opus-4-6",
        can_disable_thinking: true,
        sampling_params: true,
        ..CURRENT
    },
    Family {
        prefix: "claude-sonnet-4-6",
        can_disable_thinking: true,
        sampling_params: true,
        ..CURRENT
    },
    Family {
        prefix: "claude-opus-4-5",
        ..CLAUDE_4_5
    },
    Family {
        prefix: "claude-sonnet-4-5",
        ..CLAUDE_4_5
    },
    Family {
        prefix: "claude-haiku-4-5",
        ..CLAUDE_4_5
    },
];

/// The `max_tokens` sent for an unknown model when neither the request nor the config sets one.
/// Every current model allows at least this much.
pub(crate) const FALLBACK_MAX_TOKENS: u32 = 16_384;

/// The family `model` belongs to, if it's a known one.
pub(crate) fn family_of(model: &str) -> Option<&'static Family> {
    FAMILIES.iter().find(|family| {
        model
            .strip_prefix(family.prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
    })
}

/// One page of `GET /v1/models`.
#[derive(Debug, Deserialize)]
pub(crate) struct ModelsPage {
    pub data: Vec<WireModel>,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub last_id: Option<String>,
}

/// A model in the list. Fields other than `id` may be missing or null (older API versions,
/// gateways), so each falls back to the family table.
#[derive(Debug, Deserialize)]
pub(crate) struct WireModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    capabilities: Option<Capabilities>,
}

#[derive(Debug, Deserialize)]
struct Capabilities {
    #[serde(default)]
    image_input: Option<Support>,
    #[serde(default)]
    pdf_input: Option<Support>,
    #[serde(default)]
    thinking: Option<Support>,
}

#[derive(Debug, Deserialize)]
struct Support {
    supported: bool,
}

/// A listed model as [`ModelInfo`]. Limits come from the API, else the family table, else stay
/// unknown. Media types come from the `image_input` and `pdf_input` capabilities, else all of
/// [`MEDIA_TYPES`] (every Claude model since Claude 3 takes images and PDFs). `reasoning` comes
/// from the `thinking` capability, else whether the family is known (all of them think).
pub(crate) fn model_info(model: WireModel) -> ModelInfo {
    let family = family_of(&model.id);
    let limit = |value: Option<u64>| value.and_then(|n| u32::try_from(n).ok());
    let capabilities = model.capabilities.as_ref();
    let supports = |support: Option<&Support>| support.is_none_or(|s| s.supported);
    let images = supports(capabilities.and_then(|c| c.image_input.as_ref()));
    let pdfs = supports(capabilities.and_then(|c| c.pdf_input.as_ref()));
    let media_types = MEDIA_TYPES
        .iter()
        .filter(|&&media_type| {
            if media_type == "application/pdf" {
                pdfs
            } else {
                images
            }
        })
        .map(|&media_type| media_type.to_owned())
        .collect();
    let reasoning = match capabilities.and_then(|c| c.thinking.as_ref()) {
        Some(thinking) => thinking.supported,
        None => family.is_some(),
    };
    ModelInfo {
        context_window: limit(model.max_input_tokens).or(family.map(|f| f.context_window)),
        max_output_tokens: limit(model.max_tokens).or(family.map(|f| f.max_output_tokens)),
        id: model.id,
        display_name: model.display_name,
        media_types,
        reasoning,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn info(model: serde_json::Value) -> ModelInfo {
        model_info(serde_json::from_value(model).unwrap())
    }

    #[test]
    fn listed_limits_and_capabilities_win() {
        let model = info(json!({
            "type": "model", "id": "claude-haiku-4-5-20251001", "display_name": "Claude Haiku 4.5",
            "created_at": "2025-10-01T00:00:00Z", "max_input_tokens": 200_000,
            "max_tokens": 32_000,
            "capabilities": {"image_input": {"supported": true}, "pdf_input": {"supported": false},
                             "thinking": {"supported": true, "types": {}}},
        }));
        assert_eq!(
            model,
            ModelInfo {
                id: "claude-haiku-4-5-20251001".into(),
                display_name: Some("Claude Haiku 4.5".into()),
                context_window: Some(200_000),
                max_output_tokens: Some(32_000),
                media_types: ["image/jpeg", "image/png", "image/gif", "image/webp"]
                    .map(String::from)
                    .to_vec(),
                reasoning: true,
            }
        );
        assert!(model.accepts("image/png"));
        assert!(!model.accepts("application/pdf"));
    }

    #[test]
    fn missing_fields_fall_back_to_the_family_table() {
        let model = info(json!({
            "id": "claude-opus-5-5", "max_input_tokens": null, "capabilities": null,
        }));
        assert_eq!(model.context_window, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(128_000));
        assert_eq!(model.media_types, MEDIA_TYPES.map(String::from));
        assert!(model.reasoning);
        assert_eq!(model.display_name, None);

        let unknown = info(json!({"id": "claude-next-1"}));
        assert_eq!(unknown.context_window, None);
        assert_eq!(unknown.max_output_tokens, None);
        assert!(!unknown.reasoning);
        assert!(unknown.accepts("application/pdf"));
    }

    #[test]
    fn family_lookup_matches_whole_id_segments() {
        let opus = family_of("claude-opus-4-5-20251101").unwrap();
        assert_eq!(opus.prefix, "claude-opus-4-5");
        assert_eq!(opus.thinking, ThinkingMode::Budget);
        assert_eq!(
            family_of("claude-opus-4-5").unwrap().prefix,
            "claude-opus-4-5"
        );
        // The more specific row wins; other minor versions fall to the generation's row.
        let opus_5_5 = family_of("claude-opus-5-5").unwrap();
        assert_eq!(opus_5_5.prefix, "claude-opus-5-5");
        assert!(!opus_5_5.can_disable_thinking);
        let opus_5 = family_of("claude-opus-5").unwrap();
        assert_eq!(opus_5.prefix, "claude-opus-5");
        assert!(opus_5.can_disable_thinking);
        assert_eq!(
            family_of("claude-fable-5-1").unwrap().prefix,
            "claude-fable-5"
        );
        assert_eq!(
            family_of("claude-sonnet-4-6").unwrap().thinking,
            ThinkingMode::Adaptive
        );
        assert_eq!(family_of("claude-opus-4-50"), None);
        assert_eq!(family_of("claude-opus-4"), None);
        assert_eq!(family_of("claude-3-haiku-20240307"), None);
        assert_eq!(family_of("x-claude-opus-5"), None);
    }

    #[test]
    fn every_row_is_reachable_by_its_own_prefix() {
        for family in FAMILIES {
            assert_eq!(family_of(family.prefix), Some(family), "{family:?}");
        }
    }

    #[test]
    fn request_rules_by_family() {
        let rules = |model: &str| {
            let family = family_of(model).unwrap();
            (family.can_disable_thinking, family.sampling_params)
        };
        for model in [
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-fable-5-1",
            "claude-mythos-5",
        ] {
            assert_eq!(rules(model), (false, false), "{model}");
        }
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-haiku-5-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
        ] {
            assert_eq!(rules(model), (true, false), "{model}");
        }
        for model in [
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-opus-4-5-20251101",
            "claude-sonnet-4-5-20250929",
            "claude-haiku-4-5-20251001",
        ] {
            assert_eq!(rules(model), (true, true), "{model}");
        }
    }
}
