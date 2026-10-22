// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
// Also adapted from openai/openai-agents-python (MIT) @ f53c62451c7f34c15f0c87fa745e05404025420f, modified.
//! Native Responses web search declarations, ported from the Agents SDK and Codex.

use std::collections::BTreeMap;

use ra_core::{
    error::{Error, Result},
    model::{ApiProtocol, ModelToolDefinition},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Guidance for how much search context the model receives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum WebSearchContextSize {
    /// A small search context.
    Low,
    /// The framework and API default.
    #[default]
    Medium,
    /// A large search context.
    High,
}

/// Kinds of results the search may return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum WebSearchContentType {
    /// Text results.
    Text,
    /// Image results, included in the returned search call.
    Image,
}

/// Provider search filters, including forward-compatible provider fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WebSearchFilters {
    /// Domains whose subdomains may also appear in results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_domains: Option<Vec<String>>,
    /// Fields understood by newer provider versions.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// The provider's fixed location approximation kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum WebSearchUserLocationType {
    /// An approximate location, never a precise geolocation.
    #[default]
    Approximate,
}

/// Approximate user location for search localization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchUserLocation {
    /// The provider's location approximation kind.
    #[serde(rename = "type", default)]
    pub r#type: WebSearchUserLocationType,
    /// Two-letter country code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// Region name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// City name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    /// IANA timezone name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Forward-compatible location fields retained by the framework's dictionary contract.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Options for image results, matching the framework's optional dictionary fields.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSearchToolImageSettings {
    /// Number of image results. The provider owns range validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<i64>,
    /// Whether available image captions are included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<bool>,
    /// Forward-compatible image settings retained by the framework's dictionary contract.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// The framework's hosted `WebSearchTool` contract. Execution belongs to Responses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WebSearchTool {
    /// Optional approximate user location.
    pub user_location: Option<WebSearchUserLocation>,
    /// Optional provider filters.
    pub filters: Option<WebSearchFilters>,
    /// Search context size, defaulting to medium.
    pub search_context_size: WebSearchContextSize,
    /// Whether live fetches are allowed; absence uses the API default.
    pub external_web_access: Option<bool>,
    /// Result kinds; absence uses the API's text-only default.
    pub search_content_types: Option<Vec<WebSearchContentType>>,
    /// Optional image result settings.
    pub image_settings: Option<WebSearchToolImageSettings>,
}

impl WebSearchTool {
    /// Framework tool name, also used by named tool selection.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        "web_search"
    }

    /// Projects this declaration into the common agent and model tool table.
    pub fn to_definition(&self) -> Result<ModelToolDefinition> {
        let options = ResponsesWebSearchTool {
            external_web_access: self.external_web_access,
            indexed_web_access: None,
            filters: self.filters.clone(),
            user_location: self.user_location.clone(),
            search_context_size: Some(self.search_context_size),
            search_content_types: self.search_content_types.clone(),
            image_settings: self.image_settings.clone(),
        };
        options.definition(true)
    }
}

/// Adapter options for Codex-style product assembly.
///
/// Unlike the framework constructor, Codex omits unspecified context size, filters and location.
/// Indexed access is an adapter extension and is not a provider-neutral framework field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponsesWebSearchTool {
    /// Whether live external content is allowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_web_access: Option<bool>,
    /// Whether live fetches are restricted to indexed URLs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_web_access: Option<bool>,
    /// Domain and forward-compatible filters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<WebSearchFilters>,
    /// Approximate user location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_location: Option<WebSearchUserLocation>,
    /// Context size, omitted when the product has not configured it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_context_size: Option<WebSearchContextSize>,
    /// Requested result kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_content_types: Option<Vec<WebSearchContentType>>,
    /// Optional image settings from the framework contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_settings: Option<WebSearchToolImageSettings>,
}

impl ResponsesWebSearchTool {
    /// Creates an owned native tool declaration without a local invocation handler.
    pub fn to_definition(&self) -> Result<ModelToolDefinition> {
        self.definition(false)
    }

    fn definition(&self, framework_nulls: bool) -> Result<ModelToolDefinition> {
        let mut payload = serde_json::to_value(self).map_err(|error| {
            Error::caller("invalid Responses web search configuration").with_source(error)
        })?;
        payload["type"] = json!("web_search");
        if let Some(location) = payload.get_mut("user_location") {
            location["type"] = json!("approximate");
        }
        if framework_nulls {
            if self
                .filters
                .as_ref()
                .is_some_and(|filters| filters.allowed_domains.is_none())
            {
                payload["filters"]["allowed_domains"] = Value::Null;
            }
            if self.filters.is_none() {
                payload["filters"] = Value::Null;
            }
            if self.user_location.is_none() {
                payload["user_location"] = Value::Null;
            }
        }
        let includes = if self
            .search_content_types
            .as_ref()
            .is_some_and(|types| types.contains(&WebSearchContentType::Image))
        {
            vec!["web_search_call.results".to_owned()]
        } else {
            Vec::new()
        };
        Ok(ModelToolDefinition::hosted(
            "web_search",
            ApiProtocol::OpenAiResponses,
            payload,
            includes,
        ))
    }
}
