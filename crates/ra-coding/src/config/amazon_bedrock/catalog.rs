// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Bundled Bedrock model catalogs derived from the bundled `OpenAI` models.

use super::BedrockEndpoint;
use crate::config::{
    ModelInfo, ModelsResponse,
    catalog::{ModelServiceTier, ModelVisibility, WebSearchToolType},
};

pub(crate) const AMAZON_BEDROCK_GPT_5_5_MODEL_ID: &str = "openai.gpt-5.5";
pub(crate) const AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID: &str = "openai.gpt-5.6-sol";
pub(crate) const AMAZON_BEDROCK_GPT_6_1_SOL_MODEL_ID: &str = "openai.gpt-6.1-sol";
pub(crate) const AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID: &str = "openai.gpt-6-sol";
pub(crate) const AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID: &str = "openai.gpt-6-luna";
pub(crate) const AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID: &str = "openai.gpt-6-astra";
pub(crate) const AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID: &str = "openai.gpt-5.6-terra";
pub(crate) const AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID: &str = "openai.gpt-5.6-luna";

const GPT_5_BEDROCK_CONTEXT_WINDOW: i64 = 272_000;
const ROUTING_VARIANTS: [(&str, &str, i32); 2] =
    [("global.", "Global", 0), ("us.", "US cross-region", 1)];

/// Mantle models in priority order: `(OpenAI slug, Bedrock slug, display name)`.
const MANTLE_MODELS: [(&str, &str, &str); 7] = [
    (
        "gpt-6.1-sol",
        AMAZON_BEDROCK_GPT_6_1_SOL_MODEL_ID,
        "GPT-6.1 Sol",
    ),
    (
        "gpt-6-astra",
        AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID,
        "GPT-6-Astra",
    ),
    ("gpt-6-sol", AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID, "GPT-6 Sol"),
    (
        "gpt-6-luna",
        AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID,
        "GPT-6 Luna",
    ),
    (
        "gpt-5.6-sol",
        AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID,
        "GPT-5.6 Sol",
    ),
    (
        "gpt-5.6-terra",
        AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID,
        "GPT-5.6 Terra",
    ),
    (
        "gpt-5.6-luna",
        AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID,
        "GPT-5.6 Luna",
    ),
];

pub(crate) fn static_model_catalog(bundled: &ModelsResponse) -> ModelsResponse {
    let mut models = Vec::with_capacity(MANTLE_MODELS.len() + 1);
    for (priority, (openai_slug, bedrock_slug, display_name)) in
        (0..).zip(MANTLE_MODELS.iter().copied())
    {
        if let Some(model) = bundled_openai_model(bundled, openai_slug) {
            models.push(bedrock_model(model, bedrock_slug, display_name, priority));
        }
    }
    if let Some(model) = bundled_openai_model(bundled, "gpt-5.5") {
        models.push(gpt_5_bedrock_model(
            model,
            AMAZON_BEDROCK_GPT_5_5_MODEL_ID,
            "GPT-5.5",
            /*priority*/ 7,
        ));
    }
    normalize_bundled_bedrock_catalog(ModelsResponse { models }, BedrockEndpoint::Mantle)
}

pub(crate) fn static_gov_model_catalog(bundled: &ModelsResponse) -> ModelsResponse {
    let mut catalog = static_model_catalog(bundled);
    catalog.models.retain(|model| {
        matches!(
            model.slug.as_str(),
            AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID | AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID
        )
    });
    catalog
}

pub(crate) fn static_runtime_model_catalog(bundled: &ModelsResponse) -> ModelsResponse {
    let base_models = static_model_catalog(bundled)
        .models
        .into_iter()
        .filter(|model| {
            matches!(
                model.slug.as_str(),
                AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID
                    | AMAZON_BEDROCK_GPT_5_6_TERRA_MODEL_ID
                    | AMAZON_BEDROCK_GPT_5_6_LUNA_MODEL_ID
                    | AMAZON_BEDROCK_GPT_6_1_SOL_MODEL_ID
                    | AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID
                    | AMAZON_BEDROCK_GPT_6_SOL_MODEL_ID
                    | AMAZON_BEDROCK_GPT_6_LUNA_MODEL_ID
            )
        })
        .collect::<Vec<_>>();
    let base_count = i32::try_from(base_models.len()).unwrap_or(i32::MAX);
    let mut models = Vec::with_capacity(ROUTING_VARIANTS.len() * base_models.len());
    for (prefix, routing_label, routing_priority) in ROUTING_VARIANTS {
        for model in &base_models {
            let mut variant = model.clone();
            variant.slug = format!("{prefix}{}", model.slug);
            variant.display_name = format!("{} ({routing_label})", model.display_name);
            variant.priority = routing_priority * base_count + model.priority;
            variant.supports_search_tool = false;
            models.push(variant);
        }
    }
    normalize_bundled_bedrock_catalog(ModelsResponse { models }, BedrockEndpoint::Runtime)
}

pub(super) fn normalize_bundled_bedrock_catalog(
    mut catalog: ModelsResponse,
    endpoint: BedrockEndpoint,
) -> ModelsResponse {
    for model in &mut catalog.models {
        // Bundled defaults must not inherit OpenAI-only tiers or opt into a premium tier.
        model.additional_speed_tiers.clear();
        model.service_tiers.clear();
        if matches!(
            (endpoint, model.slug.as_str()),
            (
                BedrockEndpoint::Mantle,
                AMAZON_BEDROCK_GPT_6_ASTRA_MODEL_ID | AMAZON_BEDROCK_GPT_6_1_SOL_MODEL_ID
            ) | (
                BedrockEndpoint::Runtime,
                "global.openai.gpt-6-astra"
                    | "us.openai.gpt-6-astra"
                    | "global.openai.gpt-6.1-sol"
                    | "us.openai.gpt-6.1-sol"
            )
        ) {
            model.service_tiers.push(ModelServiceTier {
                id: "ultrafast".to_string(),
                name: "Ultrafast".to_string(),
                description: "The fastest available responses for latency-sensitive work."
                    .to_string(),
            });
        }
        model.default_service_tier = None;
    }
    normalize_bedrock_catalog(catalog)
}

pub(crate) fn normalize_bedrock_catalog(mut catalog: ModelsResponse) -> ModelsResponse {
    // Custom catalogs own their tier definitions; only normalize wire compatibility here.
    for model in &mut catalog.models {
        // Bedrock rejects the `search_content_types` field used by multimodal search.
        model.web_search_tool_type = WebSearchToolType::Text;
    }
    catalog
}

fn gpt_5_bedrock_model(
    mut model: ModelInfo,
    bedrock_slug: &str,
    display_name: &str,
    priority: i32,
) -> ModelInfo {
    model.slug = bedrock_slug.to_string();
    model.display_name = display_name.to_string();
    model.priority = priority;
    model.context_window = Some(GPT_5_BEDROCK_CONTEXT_WINDOW);
    model.max_context_window = Some(GPT_5_BEDROCK_CONTEXT_WINDOW);
    model.visibility = ModelVisibility::List;
    model.availability_nux = None;
    model.upgrade = None;
    model
}

fn bedrock_model(
    mut model: ModelInfo,
    bedrock_slug: &str,
    display_name: &str,
    priority: i32,
) -> ModelInfo {
    model.slug = bedrock_slug.to_string();
    model.display_name = display_name.to_string();
    model.priority = priority;
    model.visibility = ModelVisibility::List;
    model.availability_nux = None;
    model.upgrade = None;
    model.use_responses_lite = false;
    model.tool_mode = None;
    model
}

fn bundled_openai_model(bundled: &ModelsResponse, slug: &str) -> Option<ModelInfo> {
    bundled
        .models
        .iter()
        .find(|model| model.slug == slug)
        .cloned()
}
