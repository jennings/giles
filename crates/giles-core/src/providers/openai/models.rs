//! The `/models` response.

use giles_types::ModelInfo;
use serde::Deserialize;

use super::config::OpenAiConfig;

/// `GET /models`: `{"object": "list", "data": [...]}`. One page; the API isn't paginated.
#[derive(Debug, Deserialize)]
pub(crate) struct ModelList {
    pub data: Vec<WireModel>,
}

/// A listed model. OpenAI and Ollama report only `id` (plus `created` and `owned_by`); OpenRouter
/// adds a name, limits, input modalities, and supported parameters; vLLM adds `max_model_len`.
#[derive(Debug, Deserialize)]
pub(crate) struct WireModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_model_len: Option<u64>,
    #[serde(default)]
    architecture: Option<Architecture>,
    #[serde(default)]
    top_provider: Option<TopProvider>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Architecture {
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TopProvider {
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

/// A listed model as [`ModelInfo`], with the config's settings winning over what the server
/// reports:
///
/// - `context_window`: configured, else `context_length` (OpenRouter), `max_model_len` (vLLM),
///   or `top_provider.context_length`.
/// - `max_output_tokens`: `top_provider.max_completion_tokens`, if reported.
/// - `media_types`: the model's own configured list as is; otherwise the instance's list (or the
///   dialect's default), without the types whose modality the server reports the model lacks
///   (`image`, `audio`, and `file` for PDFs).
/// - `reasoning`: configured, else whether the server lists a reasoning parameter, else false.
pub(crate) fn model_info(model: WireModel, config: &OpenAiConfig) -> ModelInfo {
    let settings = config.settings(&model.id);
    let limit = |value: Option<u64>| value.and_then(|n| u32::try_from(n).ok());
    let top_provider = model.top_provider.as_ref();
    let own_media = config
        .models
        .get(&model.id)
        .and_then(|own| own.media_types.clone());
    let media_types = own_media.unwrap_or_else(|| {
        let modalities = model
            .architecture
            .as_ref()
            .and_then(|a| a.input_modalities.as_deref());
        let mut types = config.media_types(&model.id);
        if let Some(modalities) = modalities {
            let has = |modality: &str| modalities.iter().any(|m| m == modality);
            types.retain(|media_type| match media_type.split_once('/') {
                Some(("image", _)) => has("image"),
                Some(("audio", _)) => has("audio"),
                _ => has("file"),
            });
        }
        types
    });
    let reported_reasoning = model.supported_parameters.as_ref().map(|parameters| {
        parameters
            .iter()
            .any(|p| matches!(p.as_str(), "reasoning" | "reasoning_effort"))
    });
    ModelInfo {
        display_name: model.name,
        context_window: settings.context_window.or_else(|| {
            limit(model.context_length)
                .or(limit(model.max_model_len))
                .or(limit(top_provider.and_then(|t| t.context_length)))
        }),
        max_output_tokens: limit(top_provider.and_then(|t| t.max_completion_tokens)),
        media_types,
        reasoning: settings.reasoning.or(reported_reasoning).unwrap_or(false),
        id: model.id,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn info(model: Value, config: Value) -> ModelInfo {
        let config = OpenAiConfig::from_json(&config).unwrap();
        model_info(serde_json::from_value(model).unwrap(), &config)
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn openai_reports_only_ids() {
        let model = info(
            json!({"id": "gpt-5.1", "object": "model", "created": 1_762_000_000,
                   "owned_by": "system"}),
            json!({}),
        );
        assert_eq!(
            model,
            ModelInfo {
                id: "gpt-5.1".into(),
                display_name: None,
                context_window: None,
                max_output_tokens: None,
                media_types: strings(&["image/jpeg", "image/png", "image/gif", "image/webp"]),
                reasoning: false,
            }
        );
    }

    #[test]
    fn config_wins_over_defaults() {
        let config = json!({
            "base_url": "http://127.0.0.1:11434/v1",
            "context_window": 32_768,
            "models": {"qwen3:8b": {"reasoning": true, "context_window": 40_960}},
        });
        let qwen = info(
            json!({"id": "qwen3:8b", "owned_by": "library"}),
            config.clone(),
        );
        assert_eq!(qwen.context_window, Some(40_960));
        assert!(qwen.reasoning);
        assert_eq!(qwen.media_types, Vec::<String>::new());
        let llama = info(json!({"id": "llama3.2"}), config);
        assert_eq!(llama.context_window, Some(32_768));
        assert!(!llama.reasoning);
    }

    #[test]
    fn openrouter_details_fill_in_and_narrow_defaults() {
        let config = json!({
            "base_url": "https://openrouter.ai/api/v1",
            "media_types": ["image/png", "audio/wav", "application/pdf"],
        });
        let model = info(
            json!({
                "id": "anthropic/claude-sonnet-4.5", "name": "Anthropic: Claude Sonnet 4.5",
                "context_length": 1_000_000,
                "architecture": {"modality": "text+image->text",
                                 "input_modalities": ["text", "image", "file"],
                                 "output_modalities": ["text"]},
                "top_provider": {"context_length": 1_000_000, "max_completion_tokens": 64_000,
                                 "is_moderated": false},
                "supported_parameters": ["include_reasoning", "max_tokens", "reasoning",
                                         "temperature", "tools"],
            }),
            config.clone(),
        );
        assert_eq!(
            model,
            ModelInfo {
                id: "anthropic/claude-sonnet-4.5".into(),
                display_name: Some("Anthropic: Claude Sonnet 4.5".into()),
                context_window: Some(1_000_000),
                max_output_tokens: Some(64_000),
                media_types: strings(&["image/png", "application/pdf"]),
                reasoning: true,
            }
        );

        let text_only = info(
            json!({"id": "deepseek/deepseek-chat", "top_provider": {"context_length": 163_840},
                   "architecture": {"input_modalities": ["text"]},
                   "supported_parameters": ["max_tokens", "tools"]}),
            config,
        );
        assert_eq!(text_only.context_window, Some(163_840));
        assert_eq!(text_only.media_types, Vec::<String>::new());
        assert!(!text_only.reasoning);
    }

    #[test]
    fn a_models_own_media_types_are_taken_as_is() {
        let model = info(
            json!({"id": "m", "architecture": {"input_modalities": ["text"]},
                   "max_model_len": 131_072}),
            json!({"dialect": "generic", "models": {"m": {"media_types": ["image/png"]}}}),
        );
        assert_eq!(model.media_types, ["image/png"]);
        assert_eq!(model.context_window, Some(131_072));
    }
}
