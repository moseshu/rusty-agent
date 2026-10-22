// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Coding-product assembly of native Responses web search, following Codex's tool specification.

use std::collections::BTreeMap;

use ra_core::{
    context::RunContext,
    error::Result,
    model::{ApiProtocol, ModelToolDefinition},
    tool::HostedToolResolver,
};
use ra_model::openai::responses::web_search::{
    ResponsesWebSearchTool, WebSearchContentType, WebSearchContextSize, WebSearchFilters,
    WebSearchUserLocation,
};
use ra_protocol::server::protocol::SandboxPolicy;
use serde::{Deserialize, Serialize};

use crate::thread_settings::CodingThreadSettings;

/// Access mode selected by the coding product.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchMode {
    /// Do not advertise hosted web search.
    Disabled,
    /// Search cached content without live fetches.
    #[default]
    Cached,
    /// Permit live fetches only for indexed URLs.
    Indexed,
    /// Permit live external content.
    Live,
}

impl WebSearchMode {
    /// Restricts access to that permitted by both modes, as Codex does.
    #[must_use]
    pub fn restrict_to(self, requested: Self) -> Self {
        match (self, requested) {
            (Self::Disabled, _) | (_, Self::Disabled) => Self::Disabled,
            (Self::Cached, _) | (_, Self::Cached) => Self::Cached,
            (Self::Indexed, _) | (_, Self::Indexed) => Self::Indexed,
            (Self::Live, Self::Live) => Self::Live,
        }
    }
}

/// Model search result capability. Absence of a model catalog means text-only by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebSearchToolType {
    /// Leave the content-type field absent.
    #[default]
    Text,
    /// Request text and image results.
    TextAndImage,
}

/// Product options corresponding to Codex's `tools.web_search` configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebSearchToolConfig {
    /// Optional context guidance. An unset value is omitted on the product wire.
    pub context_size: Option<WebSearchContextSize>,
    /// Allowed domains; an override replaces this list rather than appending to it.
    pub allowed_domains: Option<Vec<String>>,
    /// Approximate location, merged field by field.
    pub location: Option<WebSearchUserLocation>,
}

impl WebSearchToolConfig {
    /// Applies a higher-priority configuration using Codex's merge order.
    #[must_use]
    pub fn merge(&self, other: &Self) -> Self {
        let location = match (&self.location, &other.location) {
            (Some(base), Some(over)) => Some(WebSearchUserLocation {
                country: over.country.clone().or_else(|| base.country.clone()),
                region: over.region.clone().or_else(|| base.region.clone()),
                city: over.city.clone().or_else(|| base.city.clone()),
                timezone: over.timezone.clone().or_else(|| base.timezone.clone()),
                extra: base
                    .extra
                    .iter()
                    .chain(&over.extra)
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect(),
                ..WebSearchUserLocation::default()
            }),
            (base, over) => over.clone().or_else(|| base.clone()),
        };
        Self {
            context_size: other.context_size.or(self.context_size),
            allowed_domains: other
                .allowed_domains
                .clone()
                .or_else(|| self.allowed_domains.clone()),
            location,
        }
    }
}

/// Explicit coding configuration for an endpoint's hosted search implementation.
///
/// Endpoint support and model result kinds are supplied by the host, not inferred from names or
/// account identity. Per-turn resolution omits hosted search outside the Responses protocol.
/// Access constraints affect search declarations; network authorization is a separate product policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodingWebSearch {
    /// Preferred mode, cached by default.
    pub mode: WebSearchMode,
    /// Options forwarded by the Responses adapter.
    pub tools: WebSearchToolConfig,
    /// Model result capability.
    pub tool_type: WebSearchToolType,
    /// Whether the selected endpoint supports hosted search.
    pub supported: bool,
    /// Whether the endpoint supports live or indexed external access.
    pub external_web_access: bool,
    /// Modes permitted by product configuration.
    pub allowed_modes: Vec<WebSearchMode>,
    /// Standalone permission profile when no thread settings are installed.
    pub sandbox_disabled: bool,
}

impl Default for CodingWebSearch {
    fn default() -> Self {
        Self {
            mode: WebSearchMode::Cached,
            tools: WebSearchToolConfig::default(),
            tool_type: WebSearchToolType::Text,
            supported: true,
            external_web_access: true,
            allowed_modes: vec![
                WebSearchMode::Disabled,
                WebSearchMode::Cached,
                WebSearchMode::Indexed,
                WebSearchMode::Live,
            ],
            sandbox_disabled: false,
        }
    }
}

impl CodingWebSearch {
    /// Resolves the effective mode for this turn using Codex's permission-profile fallback order.
    #[must_use]
    pub fn mode_for_turn(&self, sandbox_disabled: bool) -> WebSearchMode {
        use WebSearchMode::{Cached, Disabled, Indexed, Live};
        if !self.supported {
            return Disabled;
        }
        let allowed = |mode| {
            (self.external_web_access || !matches!(mode, Live | Indexed))
                && self.allowed_modes.contains(&mode)
        };
        let preferred = self.mode;
        let candidates = if sandbox_disabled && !matches!(preferred, Disabled | Indexed) {
            [Live, Cached, Disabled]
        } else {
            if allowed(preferred) {
                return preferred;
            }
            [Cached, Live, Disabled]
        };
        candidates
            .into_iter()
            .find(|mode| allowed(*mode))
            .unwrap_or(Disabled)
    }

    /// Creates a native declaration for an already resolved permission profile.
    pub fn definition_for_turn(
        &self,
        sandbox_disabled: bool,
    ) -> Result<Option<ModelToolDefinition>> {
        use WebSearchMode::{Cached, Disabled, Indexed, Live};
        let (external, indexed) = match self.mode_for_turn(sandbox_disabled) {
            Disabled => return Ok(None),
            Cached => (false, None),
            Indexed => (true, Some(true)),
            Live => (true, None),
        };
        ResponsesWebSearchTool {
            external_web_access: Some(external),
            indexed_web_access: indexed,
            filters: self
                .tools
                .allowed_domains
                .clone()
                .map(|allowed_domains| WebSearchFilters {
                    allowed_domains: Some(allowed_domains),
                    extra: BTreeMap::new(),
                }),
            user_location: self.tools.location.clone(),
            search_context_size: self.tools.context_size,
            search_content_types: match self.tool_type {
                WebSearchToolType::Text => None,
                WebSearchToolType::TextAndImage => Some(vec![
                    WebSearchContentType::Text,
                    WebSearchContentType::Image,
                ]),
            },
            image_settings: None,
        }
        .to_definition()
        .map(Some)
    }
}

impl HostedToolResolver for CodingWebSearch {
    fn resolve(
        &self,
        context: &RunContext,
        protocol: ApiProtocol,
    ) -> Result<Option<ModelToolDefinition>> {
        if protocol != ApiProtocol::OpenAiResponses {
            return Ok(None);
        }
        let sandbox_disabled = context
            .app_context::<CodingThreadSettings>()
            .map_or(self.sandbox_disabled, |settings| {
                matches!(settings.sandbox_policy(), SandboxPolicy::DangerFullAccess)
            });
        self.definition_for_turn(sandbox_disabled)
    }
}
