//! What the provider knows about Claude models without asking the API.

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

#[cfg(test)]
mod tests {
    use super::*;

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
